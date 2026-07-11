//! Orientation CLAUDE.md for a multi-repo session, written to the session dir so Claude Code loads it
//! as project context (the session dir itself has no project CLAUDE.md otherwise).

use std::path::{Path, PathBuf};

/// First meaningful line of a doc (heading text or first non-empty line), trimmed to ~100 chars.
pub fn first_meaningful_line(text: &str) -> Option<String> {
    for raw in text.split('\n') {
        // Strip a leading run of '#' characters then surrounding whitespace.
        let line = raw.trim_start_matches('#').trim();
        if !line.is_empty() {
            return Some(if line.chars().count() > 100 {
                let head: String = line.chars().take(100).collect();
                format!("{head}…")
            } else {
                line.to_string()
            });
        }
    }
    None
}

/// A one-line description of a repo, pulled from its CLAUDE.md (preferred) or README.md.
pub fn summarize_repo(worktree_path: &Path) -> Option<String> {
    for name in ["CLAUDE.md", "README.md"] {
        if let Ok(s) = std::fs::read_to_string(worktree_path.join(name)) {
            if let Some(line) = first_meaningful_line(&s) {
                return Some(line);
            }
        }
    }
    None
}

/// Orientation memory body for a multi-repo session. `repos` = (name, optional one-line summary).
pub fn build_session_guide(repos: &[(String, Option<String>)], skills: &[String]) -> String {
    let mut lines = vec![
        "# agentic-dev multi-repo session".to_string(),
        String::new(),
        "You are working in an agentic-dev session workspace. Each repository below is checked out as its".to_string(),
        "own git worktree in a subdirectory of the current directory, all on branch `agentic/<session>`.".to_string(),
        "`cd` into a repository to work on it — each has its own `CLAUDE.md` with project-specific guidance".to_string(),
        "that loads when you work there. Changes are isolated to this session's worktrees.".to_string(),
        String::new(),
        "## Repositories".to_string(),
    ];
    for (name, summary) in repos {
        match summary {
            Some(s) => lines.push(format!("- `{name}/` — {s}")),
            None => lines.push(format!("- `{name}/`")),
        }
    }
    if !skills.is_empty() {
        lines.push(String::new());
        lines.push("## Skills loaded for this session".to_string());
        lines.push(skills.join(", "));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Write the orientation CLAUDE.md into the session dir. Callers gate on multi-repo. Best-effort.
pub fn write_session_guide(
    session_dir: &Path,
    repo_worktrees: &[(String, PathBuf)],
    skills: &[String],
) {
    let repos: Vec<(String, Option<String>)> = repo_worktrees
        .iter()
        .map(|(repo, wt)| (repo.clone(), summarize_repo(wt)))
        .collect();
    let _ = std::fs::write(
        session_dir.join("CLAUDE.md"),
        build_session_guide(&repos, skills),
    );
}

/// Tier-1 "harness operating rules" — the model-routing rule. Injected into the MAIN session as an
/// APPENDED system prompt (never a replace), NOT written into the session CLAUDE.md. Orchestrator-only:
/// workers are denied `delegate`/`Workflow`/`Task`/`Agent` (they never fan out) and the router/title
/// one-shots never reach the main query, so none of them should carry these rules; and a repo's own
/// CLAUDE.md must not be able to override "use delegate", which is why this is a system prompt, not
/// project memory. Split from the old combined guide: this const is *which model* (routing);
/// [`FANOUT_GUIDE`] is *how to structure a fan-out*. [`harness_rules`] concatenates the two and the
/// caller sets the result on the main spawn's `SpawnOptions.append_system_prompt`.
pub const ROUTING_GUIDE: &str = r#"## Model routing — always route fan-out through `delegate`

You have a `delegate` tool that routes each subtask to the **cheapest capable model** across the registered providers **and** the native Claude tiers (opus/sonnet/haiku), running them in parallel as separate workers.

**Rule:** for ANY parallel work, subagent fan-out, or "spawn N agents" task, use `delegate` — it cost-routes per subtask. **Do NOT set the `model` field on any task** — leave it UNSET so `delegate` auto-routes each task to the cheapest capable model (your registered providers + the Claude tiers). Setting `model` (e.g. "sonnet") PINS that task and BYPASSES routing entirely — never hand-pick a model unless the user explicitly names one.

**Do NOT use the `Workflow` tool to fan out parallel agents.** A Workflow's agents run in-process on the MAIN model and CANNOT be cost-routed — nothing can intercept them. `delegate` is the routed equivalent: it runs each task as a SEPARATE worker process routed to the cheapest capable model. Even when the user says "create a Workflow" or "spawn N agents" for parallel work, use `delegate` for the actual fan-out. Reserve the `Workflow` tool ONLY for genuinely deterministic multi-stage scripting (loops / conditionals / pipelines across rounds) that a single `delegate` batch can't express — and those agents will run UNROUTED on the main model. Native `Agent`/`Task` subagents are likewise unrouted."#;

/// Tier-1 fan-out mechanics + discipline (see [`ROUTING_GUIDE`] for the tier contract — same
/// injection, same audience). Split out of the old routing guide so "which model" (routing) and "how
/// to structure a fan-out" (this) are separate single-responsibility sections.
pub const FANOUT_GUIDE: &str = r#"## Fan-out discipline — how to structure a `delegate` batch

When you fan out with `delegate`:

**Pass a `title` and per-task `phase`.** The `title` names the batch (it becomes the workflow card title); a `phase` label per task groups workers under phase headers — the same titled, phased card a native Workflow renders.

**Keep each task's `prompt` compact.** Say what to do and give the repo path — then let the worker read the files itself. Do NOT inline file contents or long absolute-path dumps, and prefer forward slashes. A large, escape-heavy payload makes the model more likely to emit the `delegate` call as malformed JSON, which is rejected and costs a wasted retry turn.

**Cut non-overlapping boundaries first.** The top failure mode of a large fan-out is workers duplicating each other's work. Before you fan out, split the work into disjoint slices (by file / subsystem / search-angle) and state what each worker owns.

**Parallelize only genuinely independent work.** Tasks that share state, are tightly coupled, or have a sequential dependency belong in ONE worker (or your own thread) — splitting them across workers produces conflicting patches and lost context. Independent = no shared state, no ordering requirement.

**Verify consequential fan-out output before acting on it.** Cheap routed workers are fast but can be wrong. For any finding you will commit, merge, or report, add a verify phase: a second `delegate` pass whose workers try to REFUTE each finding from distinct angles (logic, edge cases, security); drop what a refuter kills. Skip this only for throwaway exploratory reads."#;

/// The Tier-1 harness system prompt: routing rules + fan-out discipline, joined. Set on the MAIN
/// session spawn's `SpawnOptions.append_system_prompt` (appended to Claude Code's base prompt); NOT
/// written into any CLAUDE.md, so delegate workers and the router never load it.
pub fn harness_rules() -> String {
    format!("{ROUTING_GUIDE}\n\n{FANOUT_GUIDE}")
}

/// Build-environment guidance, written into every session's CLAUDE.md next to ROUTING_GUIDE.
/// A session worktree has the tracked source but none of the machine-local, gitignored build
/// environment (deps, SDK pointers, caches, signing keys) that lives in the repo's main checkout.
/// This tells the agent to symlink what a build needs from `~/src/<repo>` — the platform links
/// nothing itself, keeping this repo-agnostic.
pub const WORKTREE_SETUP_GUIDE: &str = r#"## Build environment — inherit it from the main checkout

Your repo worktrees have the source but NOT the machine-local, gitignored build
environment that lives in each repo's main checkout at `~/src/<repo>` (the repos
root `$AGENTIC_SRC_ROOT`, default `~/src`): dependency dirs, SDK pointers, build
caches, signing keys. A freshly-created worktree may fail to build or run until you
bring those over.

**When you need to BUILD or RUN** (not just edit), symlink the pieces the build needs
from the main checkout into the matching worktree — don't reinstall from scratch. Use
your judgment about what the build actually needs. Typical local env:
- dependency dirs: `node_modules` (incl. nested like `server-rs/node_modules`), `.venv`, `vendor/`
- local config / pointers: `local.properties` (Android SDK dir), `.env`
- caches: `.gradle/`
- signing material, only when you need a signed build: `*.keystore`, `keystore.properties`

**Do NOT link build OUTPUT** (`target/`, `build/`, `app/build/`, `dist/`) — each
worktree builds its own; sharing it causes stale or corrupt results.

How (run at the worktree's repo root, e.g. inside `agentic-dev-android/`):
    ln -s "${AGENTIC_SRC_ROOT:-$HOME/src}/<repo>/local.properties" .
    ln -s "${AGENTIC_SRC_ROOT:-$HOME/src}/<repo>/.gradle" .gradle
Symlinks point at the main checkout's deps/caches/keys — reuse, not copies. This session
pushes its branch and opens PRs, so before you link secrets (`*.keystore`,
`keystore.properties`, `.env`) confirm they're gitignored in that repo and never `git add`
a symlink to one. If a repo ships its own setup (`make setup` / a bootstrap script), prefer that."#;

/// Write the session-dir `CLAUDE.md` from an ordered list of `sections` (e.g. the multi-repo
/// orientation guide followed by the user's session-scoped custom guidance). Blank/whitespace-only
/// sections are dropped; the rest are trimmed and joined with a Markdown horizontal-rule separator.
/// When every section is empty NOTHING is written (so a single-repo session with no custom guidance
/// leaves the session dir clean, exactly as before this field existed). Best-effort — a write error
/// is swallowed so it can never abort session creation.
pub fn write_session_claude_md(session_dir: &Path, sections: &[String]) {
    let body = sections
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    if !body.is_empty() {
        let _ = std::fs::write(session_dir.join("CLAUDE.md"), format!("{body}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "sg-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn first_line_strips_heading_and_trims() {
        assert_eq!(
            first_meaningful_line("## My Repo\n\nbody"),
            Some("My Repo".into())
        );
        assert_eq!(
            first_meaningful_line("\n\n  hello  \nx"),
            Some("hello".into())
        );
        assert_eq!(first_meaningful_line("\n  \n"), None);
        let long = "#".to_string() + &"a".repeat(150);
        assert!(first_meaningful_line(&long).unwrap().ends_with('…'));
    }

    #[test]
    fn summarize_prefers_claude_md_over_readme() {
        let d = tmp();
        std::fs::write(d.join("README.md"), "# readme line").unwrap();
        std::fs::write(d.join("CLAUDE.md"), "# claude line").unwrap();
        assert_eq!(summarize_repo(&d), Some("claude line".into()));
        let d2 = tmp();
        std::fs::write(d2.join("README.md"), "# only readme").unwrap();
        assert_eq!(summarize_repo(&d2), Some("only readme".into()));
        assert_eq!(summarize_repo(&tmp()), None);
    }

    #[test]
    fn guide_lists_repos_with_summaries_and_skills() {
        let g = build_session_guide(
            &[
                ("api".into(), Some("the backend".into())),
                ("web".into(), None),
            ],
            &["rust".into(), "tdd".into()],
        );
        assert!(g.contains("# agentic-dev multi-repo session"));
        assert!(g.contains("- `api/` — the backend"));
        assert!(g.contains("- `web/`\n") || g.contains("- `web/`"));
        assert!(g.contains("## Skills loaded for this session"));
        assert!(g.contains("rust, tdd"));
    }

    #[test]
    fn write_guide_creates_claude_md_in_session_dir() {
        let sess = tmp();
        let r1 = tmp();
        std::fs::write(r1.join("CLAUDE.md"), "# repo one").unwrap();
        write_session_guide(&sess, &[("one".into(), r1)], &[]);
        let content = std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap();
        assert!(content.contains("- `one/` — repo one"));
    }

    #[test]
    fn write_claude_md_joins_sections_with_separator() {
        let sess = tmp();
        write_session_claude_md(&sess, &["# Guide\nbody".into(), "Run tests first.".into()]);
        let content = std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap();
        assert!(content.contains("# Guide\nbody"));
        assert!(content.contains("Run tests first."));
        assert!(
            content.contains("\n---\n"),
            "sections must be separated by a horizontal rule"
        );
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn write_claude_md_single_section_has_no_separator() {
        let sess = tmp();
        write_session_claude_md(&sess, &["Only custom guidance.".into()]);
        let content = std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap();
        assert_eq!(content, "Only custom guidance.\n");
        assert!(!content.contains("---"));
    }

    #[test]
    fn write_claude_md_skips_blank_sections_and_writes_nothing_when_all_empty() {
        // A blank section among real ones is dropped (no leading/trailing separator).
        let sess = tmp();
        write_session_claude_md(&sess, &["".into(), "  \n ".into(), "real".into()]);
        assert_eq!(
            std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap(),
            "real\n"
        );

        // All-empty (the single-repo, no-custom-guidance case) writes no file at all.
        let sess2 = tmp();
        write_session_claude_md(&sess2, &["".into(), "   ".into()]);
        assert!(
            !sess2.join("CLAUDE.md").exists(),
            "no CLAUDE.md should be written when every section is blank"
        );

        // Empty slice is a no-op too.
        let sess3 = tmp();
        write_session_claude_md(&sess3, &[]);
        assert!(!sess3.join("CLAUDE.md").exists());
    }

    #[test]
    fn worktree_setup_guide_present_and_composed_into_claude_md() {
        // The const carries its heading and the actionable verbs.
        assert!(WORKTREE_SETUP_GUIDE
            .contains("## Build environment — inherit it from the main checkout"));
        assert!(WORKTREE_SETUP_GUIDE.contains("symlink the pieces the build needs"));
        // Tier-2 CLAUDE.md carries the build-env guide, NOT the routing guide (which moved to the
        // Tier-1 append-system-prompt via `harness_rules`).
        let sess = tmp();
        write_session_claude_md(&sess, &[WORKTREE_SETUP_GUIDE.to_string()]);
        let content = std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Build environment — inherit it from the main checkout"));
        assert!(content.contains("Do NOT link build OUTPUT"));
        assert!(
            !content.contains("Model routing"),
            "routing guide must NOT be in the session CLAUDE.md"
        );
    }

    #[test]
    fn harness_rules_hold_routing_and_fan_out_but_not_build_env() {
        let rules = harness_rules();
        // Tier-1 = routing + fan-out discipline, concatenated.
        assert!(rules.contains("## Model routing — always route fan-out through `delegate`"));
        assert!(rules.contains("## Fan-out discipline"));
        // The split is clean: routing is "which model"; fan-out discipline is "how to structure it".
        assert!(ROUTING_GUIDE.contains("Do NOT set the `model` field"));
        assert!(!ROUTING_GUIDE.contains("Cut non-overlapping boundaries"));
        assert!(FANOUT_GUIDE.contains("Cut non-overlapping boundaries first"));
        assert!(FANOUT_GUIDE.contains("Verify consequential fan-out output"));
        // Tier-1 must NOT carry the build-env guide (that's Tier-2 CLAUDE.md).
        assert!(!rules.contains("Build environment — inherit it from the main checkout"));
    }
}
