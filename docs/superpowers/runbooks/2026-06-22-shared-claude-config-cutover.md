# Runbook — shared `~/.claude` config-dir cutover (fixes the 401 / stuck-resume bug)

**Date:** 2026-06-22
**Scope:** agentic-dev `server-rs` (the live Rust backend). Touches the session spawn path → a
restart bounces in-flight turns, so deploy when sessions are idle.

---

## Why

A session (`02e74d6e`) was stuck on `401 Invalid authentication credentials`, and re-login + resume
did **not** fix it. Root cause:

- Each session ran with `CLAUDE_CONFIG_DIR=<worktree>/.claude-config`, where `.credentials.json` was
  a **symlink** to the shared `~/.claude/.credentials.json`.
- Claude refreshes its OAuth token with an **atomic write** (`write tmp + rename`). `rename()`
  **replaces the symlink with an independent real file** — from then on the session's credential is
  decoupled from `~/.claude`.
- OAuth refresh tokens **rotate**: when one decoupled copy refreshes, every other copy's refresh
  token is revoked → cascading 401s. Re-login only updates `~/.claude`, never the decoupled copy, so
  resume kept reading the dead token.

Evidence: of 9 live sessions, 3 had already turned their symlink into a stale real file; one was
already expired. Confirmed in the CLI binary that the credential writer (`atomicWrite` → `Cg`) does
`writeFile(tmp); rename(tmp, path)`.

## The fix

All agentic sessions now use the **real `~/.claude`** as `CLAUDE_CONFIG_DIR` — the exact same file
the user's own `claude` processes use. One shared, self-refreshing credential: refreshes update the
one file everyone reads; rotation can never strand a copy or log out the user's personal claude.
(Any separate per-session copy/dir would re-introduce the rotation cascade — this is the only robust
shape.)

Per-session isolation is preserved:

| concern | how it stays isolated |
|---|---|
| app conversation view | agentic-dev's own `~/.agentic-dev/logs/<id>.jsonl` — independent of `CLAUDE_CONFIG_DIR` |
| `--resume` context | claude namespaces transcripts under `~/.claude/projects/<cwd-slug>/` (unique per worktree) |
| workflow viewer | now scoped to the session's claude transcript UUID (see code) |
| credentials | the one thing now shared — the fix |

## Behavior changes (accept before deploying)

1. **Personal skills become globally visible.** Sessions now see `~/.claude/skills/`
   (`rke2-ops`, `cloudstack-ops`, `tenants-dev`, …). The per-session `skills` API param no longer
   *restricts* (it was unused — every session was created with `[]`). Plugins were already shared.
2. **Agentic state merges into your personal `~/.claude`.** Session transcripts land in
   `~/.claude/projects/` (cwd-namespaced, so they don't collide); your personal `/resume` list will
   show worktree entries, and `.claude.json` sees more concurrent writers (already true for your
   several long-running `claude` processes — tolerated via atomic writes).

## Code changes (`server-rs`)

- `engine/mod.rs` — `spawn_opts`: `CLAUDE_CONFIG_DIR = claude_config_base` (`~/.claude`).
  `start()`: dropped the per-session `.claude-config` build. `with_activity` + `has_active_workflow`:
  scope workflow checks to the session's `claude_session_id`. `delete`: no `.claude-config` to remove.
- `api/sessions.rs` — workflow routes read from `claude_config_base`, scoped to the session UUID
  (no uuid → empty, never another session's runs).
- `engine/workflows.rs` — `project_dirs` / `list_workflows` / `read_workflow_agent` take an optional
  `session_uuid`; only descend into `projects/<slug>/` dirs that hold that uuid. Encoding-independent
  (we never reconstruct claude's cwd→slug transform). New test: `scopes_to_session_uuid_…`.
- `engine/claude_config.rs` — **deleted** (its only job was building the per-session dir).
- `engine/usage.rs` — unchanged (already read `~/.claude/.credentials.json`).

Tests: `cargo test --lib` → **253 passed**.

## Deploy steps

> Do this when sessions are idle — restart finalizes in-flight turns as `interrupted`.

```bash
# 1. Land the change on master in the LIVE checkout (the service builds from ~/src/agentic-dev).
#    (from this worktree branch: merge/cherry-pick, or re-apply the diff, then commit to master.)

# 2. Build the release binary.
cargo build --release --manifest-path ~/src/agentic-dev/server-rs/Cargo.toml

# 3. Migrate existing sessions' claude transcripts + workflow data into the shared ~/.claude.
#    (run from a NORMAL shell, NOT inside an agentic session — see note in the script.)
~/src/agentic-dev/scripts/migrate-shared-claude-config.sh --dry-run   # preview
~/src/agentic-dev/scripts/migrate-shared-claude-config.sh             # apply (idempotent, no-clobber)

# 4. Restart the service.
systemctl --user restart agentic-dev      # adjust to your actual unit/redeploy script
```

## Verify after deploy

```bash
# A new session's credential is the shared file, NOT a per-session copy:
ls -la ~/src/agentic-worktrees/<new-session>/.claude-config 2>/dev/null   # → should NOT exist
# Spawned claude points at the shared dir:
#   CLAUDE_CONFIG_DIR=/home/<user>/.claude   (check the session's env / process)
# Workflow viewer still shows only the session's own runs (open a session that ran a workflow).
# Token refresh no longer strands sessions: let a session sit past a token refresh, then resume.
```

## Rollback

Revert the `server-rs` commit and rebuild/restart. The migration is additive + no-clobber, so the
copied data in `~/.claude/projects/` is harmless to leave; the old per-session `.claude-config/`
dirs were never deleted, so reverting restores the prior behavior intact.

## Follow-up (not done here)

The legacy TS server (`server/engine/{engine,workflows}.ts`, `server/api/routes.ts`) still has the
per-session `.claude-config` logic. It is **not deployed** (Rust is live). Apply the same change there
only if the TS server is ever revived.
