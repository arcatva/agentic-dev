# Worktree build environment — inherit from the main checkout (via injected session CLAUDE.md)

**Date:** 2026-07-10
**Status:** Draft — awaiting user review
**Repo:** agentic-dev (backend only; no Android changes required)

## Problem

A session's repo worktrees are created with `git worktree add` and nothing else
(`engine::worktree::create_session_worktrees`). They contain the tracked source but
NOT the machine-local, **gitignored** build environment that lives in each repo's
main checkout at `$AGENTIC_SRC_ROOT/<repo>` (default `~/src/<repo>`). So a fresh
worktree cannot build until that environment is reconstructed by hand. Confirmed gaps
today:

- **agentic-dev-android**: no `local.properties` (so gradle can't find the SDK —
  `ANDROID_HOME`/`ANDROID_SDK_ROOT` are also unset), no `release.keystore` /
  `keystore.properties` (signed builds impossible in-worktree), no `.gradle/` cache.
  `gradle` is not on `PATH` (mitigated: the committed `./gradlew` wrapper pulls the
  pinned 8.10.2 dist into the shared `~/.gradle`).
- **agentic-dev**: no `server-rs/node_modules` (the Claude Agent SDK bridge's runtime
  deps), so the server won't run until `make build`/`bridge-deps` is run. (`cargo test`
  is unaffected — it uses a fake bash bridge.)

This is a recurring friction ("the worktree has no gradle / no SDK / no keystore") that
hits every new session which needs to build, not just these two repos.

## Goal

Make a fresh session worktree buildable without manual guesswork, in a way that
**generalizes to any repo** (Rust, Android, Python, Go, …) the platform is used to
develop — without the platform needing to know each repo's build system.

## Decision (chosen approach)

**Agent-driven, documented in the platform-injected session CLAUDE.md.** The platform
does NOT link, copy, install, or otherwise provision anything. Instead, the orientation
CLAUDE.md that the platform already injects into every session gains one short section
telling the agent: your worktree lacks the main checkout's local build environment;
when you need to build or run, symlink the pieces you need from `~/src/<repo>`, using
your judgment about which are needed.

### Why this over the alternatives (recorded for context)

- **Platform auto-provisioning (symlink a declared/default set at worktree-create).**
  Deterministic and zero-token, and it would help humans/CI too. Rejected because it
  requires the platform to encode per-repo build knowledge (or guess via a default
  list / heuristic), which does not generalize cleanly to arbitrary future repos — the
  user's explicit concern. It also adds real engine logic and failure modes.
- **Repo-level bootstrap only (`make setup` per repo).** No platform change, but relies
  on the agent remembering to run it, and every repo (including these two) must add the
  script. Superseded by putting the convention in one shared injected place.

The chosen approach trades determinism for generality and near-zero platform change:
the agent decides what a given build needs. It is language/repo-agnostic and applies to
all current and future sessions (single- and multi-repo) automatically.

## Design

Purely additive **text** in the injected session CLAUDE.md — no new provisioning
behavior. Mirrors the existing `ROUTING_GUIDE` pattern exactly (a static section string
pushed into the `sections` list that `write_session_claude_md` joins and writes).

### Components / changes (all in `server-rs/`)

| File | Change |
|---|---|
| `src/engine/session_guide.rs` | Add `pub const WORKTREE_SETUP_GUIDE: &str = "…"` — the section text below. Sibling of the existing `ROUTING_GUIDE` const; no logic. |
| `src/engine/mod.rs` | At both session-CLAUDE.md assembly sites (~L644 create path, ~L1173 fork/adopt path), push `WORKTREE_SETUP_GUIDE` into `sections` alongside `ROUTING_GUIDE`, so it is written on every session. |
| `src/engine/session_guide.rs` (tests) | Add a unit test asserting the const is non-empty and that `write_session_claude_md` includes its heading (following the existing `write_claude_md_*` test style). |

The platform still writes zero build files and runs zero commands. `create_session_worktrees`
is untouched.

### The injected section (verbatim)

```markdown
## Build environment — inherit it from the main checkout

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
a symlink to one. If a repo ships its own setup (`make setup` / a bootstrap script), prefer that.
```

## Scope

**In:**
- The new `WORKTREE_SETUP_GUIDE` const + its wiring at both assembly sites + a unit test.

**Out (deliberately):**
- No platform-side linking/copying/installing; `create_session_worktrees` unchanged.
- No new engine logic, no `.agentic/*` manifest, no default-set/heuristic linking.

**Optional follow-up (only if the user wants it):** add a one-line concrete hint to each
repo's own `CLAUDE.md` — android: typically link `local.properties`, `*.keystore`/
`keystore.properties`, `.gradle`; agentic-dev: link `server-rs/node_modules`. Keeps the
agent from re-discovering each time. Not part of the core change.

## Testing

- Unit test in `session_guide.rs`: `WORKTREE_SETUP_GUIDE` is non-empty and contains its
  heading; `write_session_claude_md(&[ROUTING_GUIDE, WORKTREE_SETUP_GUIDE, …])` produces
  a file containing the heading (reuse the existing temp-dir helper + assertion style).
- `make test` (cargo test) stays green.
- Manual: open a fresh session, confirm the session-dir `CLAUDE.md` contains the section.

## Success criteria

1. Every newly created session's injected `CLAUDE.md` contains the "Build environment —
   inherit it from the main checkout" section.
2. No change to platform provisioning behavior (no files linked/written by the server;
   `create_session_worktrees` untouched).
3. `cargo test` green.

## Landing

Per `agentic-dev/CLAUDE.md`: adversarially pre-verify with `delegate`, open a PR, adopt
Codex review, auto-merge. Redeploy (`systemctl --user restart agentic-dev`) picks up the
new injected text for sessions created after the restart; it does not rewrite existing
sessions' CLAUDE.md.
