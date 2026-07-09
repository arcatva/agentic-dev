# Worktree build-env inherit — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every agentic-dev session's injected CLAUDE.md gains a section telling the agent to symlink the needed, gitignored build environment from each repo's main checkout, so a fresh worktree is buildable.

**Architecture:** Purely additive orientation text. Add one static `&str` const (`WORKTREE_SETUP_GUIDE`) in `session_guide.rs` — a sibling of the existing `ROUTING_GUIDE` — and push it into the `sections` list at both session-CLAUDE.md assembly sites in `engine/mod.rs`. No provisioning logic; the platform links nothing. The agent does the linking with its own judgment.

**Tech Stack:** Rust (server-rs), cargo test.

## Global Constraints

- Text-only change — NO new engine/provisioning behavior; `create_session_worktrees` stays untouched.
- Mirror the existing `ROUTING_GUIDE` pattern exactly (a static section string added to `sections`).
- The section applies to every session (single- and multi-repo): it is added next to `ROUTING_GUIDE`, which is always `sections[0]`.
- `make test` (= `cd server-rs && cargo test`) stays green; code must compile (`cargo build`).
- Injected section text is the exact English block in the spec (`docs/superpowers/specs/2026-07-10-worktree-build-env-inherit-design.md`).

---

### Task 1: Inject `WORKTREE_SETUP_GUIDE` into the session CLAUDE.md

**Files:**
- Modify: `server-rs/src/engine/session_guide.rs` (add the const + a unit test)
- Modify: `server-rs/src/engine/mod.rs:644` (first assembly site) and `:1173` (second assembly site)

**Interfaces:**
- Produces: `pub const WORKTREE_SETUP_GUIDE: &str` in `crate::engine::session_guide`.
- Consumes: existing `write_session_claude_md(&Path, &[String])`, `ROUTING_GUIDE`, and the test helper `tmp()` in the `session_guide::tests` module.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `server-rs/src/engine/session_guide.rs`:

```rust
#[test]
fn worktree_setup_guide_present_and_composed_into_claude_md() {
    // The const carries its heading and the actionable verbs.
    assert!(WORKTREE_SETUP_GUIDE.contains("## Build environment — inherit it from the main checkout"));
    assert!(WORKTREE_SETUP_GUIDE.contains("symlink the pieces the build needs"));
    // It composes into the written session CLAUDE.md alongside the routing guide.
    let sess = tmp();
    write_session_claude_md(&sess, &[ROUTING_GUIDE.to_string(), WORKTREE_SETUP_GUIDE.to_string()]);
    let content = std::fs::read_to_string(sess.join("CLAUDE.md")).unwrap();
    assert!(content.contains("Build environment — inherit it from the main checkout"));
    assert!(content.contains("Do NOT link build OUTPUT"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd server-rs && cargo test session_guide::tests::worktree_setup_guide_present_and_composed_into_claude_md`
Expected: FAIL — compile error `cannot find value WORKTREE_SETUP_GUIDE in this scope`.

- [ ] **Step 3: Add the const**

In `server-rs/src/engine/session_guide.rs`, immediately after the `ROUTING_GUIDE` const definition (after its closing `;`), add:

```rust
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
    ln -s ~/src/<repo>/local.properties .
    ln -s ~/src/<repo>/.gradle .gradle
Symlinks point at the main checkout's deps/caches/keys — reuse, not copies. This session
pushes its branch and opens PRs, so before you link secrets (`*.keystore`,
`keystore.properties`, `.env`) confirm they're gitignored in that repo and never `git add`
a symlink to one. If a repo ships its own setup (`make setup` / a bootstrap script), prefer that."#;
```

- [ ] **Step 4: Wire it into both assembly sites in `engine/mod.rs`**

Site 1 — replace the line at `server-rs/src/engine/mod.rs:644`:

```rust
            let mut sections: Vec<String> = vec![crate::engine::session_guide::ROUTING_GUIDE.to_string()];
```

with:

```rust
            let mut sections: Vec<String> = vec![
                crate::engine::session_guide::ROUTING_GUIDE.to_string(),
                crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string(),
            ];
```

Site 2 — replace the line at `server-rs/src/engine/mod.rs:1173`:

```rust
            let mut sections = vec![crate::engine::session_guide::ROUTING_GUIDE.to_string()];
```

with:

```rust
            let mut sections = vec![
                crate::engine::session_guide::ROUTING_GUIDE.to_string(),
                crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string(),
            ];
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cd server-rs && cargo test session_guide::tests::worktree_setup_guide_present_and_composed_into_claude_md`
Expected: PASS.

- [ ] **Step 6: Run the full suite + compile floor**

Run: `cd server-rs && cargo test && cargo build`
Expected: all tests pass; build succeeds (the hard merge floor).

- [ ] **Step 7: Commit**

```bash
git add server-rs/src/engine/session_guide.rs server-rs/src/engine/mod.rs
git commit -m "feat(session): inject worktree build-env inherit guidance into session CLAUDE.md

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Injected section on every session → Step 3 (const) + Step 4 (both sites, next to always-present `ROUTING_GUIDE`). ✓
- No platform provisioning behavior change → only a const + two `vec!` literals touched; `create_session_worktrees` untouched. ✓
- Unit test asserting const + composed output → Step 1. ✓
- `cargo test` green + compile → Step 6. ✓
- Exact English text → Step 3 matches the spec verbatim block. ✓

**Placeholder scan:** none — all steps carry real code/commands.

**Type consistency:** `WORKTREE_SETUP_GUIDE: &str` used consistently in const (Step 3), test (Step 1), and both `vec!`s (Step 4); `.to_string()` applied where `Vec<String>` is required. ✓
