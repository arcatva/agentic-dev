# agentic-dev — Claude guidance

Local agent-driven dev platform that spawns headless `claude -p` sessions in git worktrees
and streams them over an HTTP + WebSocket API. The client is the agentic-dev Android app
(`~/src/agentic-dev-android`); this repo is the backend only.

**Read `docs/internals.md`** for architecture/ops notes, the headless screenshot/verify recipe,
deploy rules, and `claude -p` stream quirks (subagents/workflow/Mode/AskUserQuestion). Append durable
project notes there (or `docs/`), not to global `~/.claude` memory.

## Layout
- `server-rs/src/engine/` — HTTP-independent core: stream parser, store (sqlite + log files),
  worktree, spawner, runner, repos, engine (pool/subscribe/kill). Unit-tested in-crate.
- `server-rs/src/api/` — axum routes + WS stream + HMAC token auth + config. This is the whole
  surface the Android client talks to; there is no server-rendered UI.
- `server-rs/sdk-bridge.mjs` — the only Node code: the per-turn claude transport (Agent SDK),
  spawned by the Rust runner so AskUserQuestion can pause in-turn.

## Rules
- Tests must stay green before commit: `make test` (= `cd server-rs && cargo test`). Tests never hit
  the real `claude`: they run through the production runner (`SdkRunner`) pointed at a fake bridge
  script in `server-rs/tests/fixtures/` (`fake-sdk-bridge-*.sh`, default `fake-sdk-bridge-ok.sh`),
  invoked via `bash` instead of `node sdk-bridge.mjs`. The fake bridge appends canned stream-json to
  `$SDK_BRIDGE_LOG` (SdkRunner sends the bridge's stdout to /dev/null) — no node, no API cost.
- Engine (`server-rs/src/engine/`) stays free of axum imports (keep it unit-testable in isolation).
- Driver invariant (SDK bridge ONLY): the per-turn transport is `server-rs/sdk-bridge.mjs`, spawned by
  `runner::SdkRunner`, in BOTH production and tests — there is no raw-`claude`-CLI runner anymore (the
  old `LocalRunner` / `build_spec` argv / `fake-claude.sh` harness was removed). Production injects an
  SdkRunner pointed at the real bridge (`main.rs`); tests inject one pointed at a `fake-sdk-bridge-*.sh`
  script. Per-session permission mode is passed to the SDK as `extraArgs["permission-mode"] = <mode>` —
  **kebab-case**; the CLI rejects camelCase `--permissionMode` ("unknown option" → exit 1).
  bypassPermissions goes through that same flag (NOT `--dangerously-skip-permissions`).
- Resume safety: `--resume` makes the CLI send `previous_message_id` from the transcript, which the API
  requires to be a real server id (`msg_...`). Some SDK-written transcripts contain only synthetic ids →
  resume 400s and can never succeed (the engine can't synthesize server ids; trimming the tail does not
  help). Before resuming, `engine::resume_gate::transcript_is_resumable` checks for any `msg_` id; with
  none, the engine drops `--resume` and runs the turn FRESH (chat context not carried in; worktree
  untouched). A fresh turn that earns real `msg_` ids makes future resumes work — self-healing.
- Build with `make build` — it runs the bridge's `npm install` (Claude SDK, in `server-rs/`) then
  `cargo build --release`. The server preflights the bridge SDK at boot and logs a fix command if missing.

## Client
The Android app lives in `~/src/agentic-dev-android` (Kotlin/Compose, M3 Expressive). It is the
only client — keep the API stable for it. The former in-repo React/Vite web UI was removed.

## PR workflow — auto-merge after Codex review
This repo lands changes hands-off. **This section OVERRIDES the generic session-workflow rule**
("don't merge it yourself / conflicts are the user's call") for THIS repo: here you DO merge, and
you DO resolve rebase conflicts yourself.

After a change is ready:
1. **Adversarially verify the change BEFORE you commit.** Fan out sub-agents with `delegate`
   (per the model-routing rule at the top of the session CLAUDE.md — leave `model` UNSET so each
   task is cost-routed; give them a `title` + `phase`). Give each worker the diff and a distinct
   *refutation* angle — logic/regression, edge cases & error paths, security, and test/coverage
   gaps — and tell it to actively try to prove the change wrong, not to praise it. Fix anything a
   worker surfaces as a real problem before committing. This is our own pre-flight gate; it runs
   in ADDITION to Codex review below, not instead of it.
2. Commit on the session branch, push, open a PR with `gh pr create` (targets default branch
   `master`). One PR per coherent change. Opening a non-draft PR triggers a Codex review
   automatically; if you opened it as a draft, mark it ready (or comment `@codex review`) to
   trigger one.
3. **Wait for the Codex review** — it posts a few minutes after the PR opens. Codex either leaves
   comment(s) with suggestions, or — if it has nothing to flag — just reacts with 👍 and stays
   silent. Poll its output (identify Codex by its bot author, login contains `codex`):
   `gh api repos/arcatva/agentic-dev/issues/<n>/comments` (top-level summary),
   `gh api repos/arcatva/agentic-dev/pulls/<n>/comments` (inline),
   `gh api repos/arcatva/agentic-dev/issues/<n>/reactions` (to catch a 👍 left with no comment), and
   `gh api repos/arcatva/agentic-dev/pulls/<n>/reviews`.
   No comments (just the 👍) means "looks good — nothing to change." (No response after a
   reasonable wait, e.g. ~10 min → proceed without it.)
4. **Triage every Codex comment.** Adopt the ones that are correct and worthwhile — commit the
   fix on the branch and push. Skip ones that are wrong, purely stylistic, or low-value; state
   which you skipped and why in one line. (You can also have Codex do the fix by commenting
   `@codex address that feedback`, but prefer fixing it yourself so you stay in control of the diff.)
5. **Auto-merge:** `gh pr merge <n> --rebase`. If the rebase conflicts, resolve them yourself
   (read both sides, preserve both intents) and finish the merge — do NOT stop to ask about
   conflicts.
6. Merge is **aggressive** (user's choice): it does NOT wait on the test suite. The one hard
   floor is that the code must still **compile** (`cd server-rs && cargo build` / `make build`); `make test` is
   optional and never blocks a merge.
7. Stop and ask the user ONLY if Codex reports a genuine correctness/security blocker you cannot
   confidently fix, or a conflict has two truly incompatible intents.

## Releases — on-demand only (user's choice)
Merging PRs never bumps versions or publishes releases (ci.yml only runs tests). Cut a release
ONLY when the user asks for one:
1. Bump `version` in `server-rs/Cargo.toml` inside the release PR and land it via the normal
   PR flow above.
2. Tag the merged master `v<version>` and push the tag — `.github/workflows/release.yml` builds
   the Linux/macOS tarballs (+ SHA256SUMS) and attaches them to the GitHub Release.
3. Never move or re-push an already-published tag (the v0.1.0 re-tags during the initial
   bootstrap were a one-off repair, not the pattern).
Cost note: on a private repo the two macOS matrix jobs bill at 10x minutes (~60–80 billed
minutes per release run); on a public repo standard-runner minutes are free. Either way,
release on demand — not per-merge.

## Delivering a build artifact to the user
The user is remote (no terminal): hand off any built binary/tarball via `./outbox/` (see the global
outbox note). **Naming rule:**
- **Default (ad-hoc build) → build timestamp** `<YYYYMMDD-HHMM>.<ext>` (e.g. `20260709-2248.tar.gz`)
  so ad-hoc builds are distinguishable.
- **When the user explicitly says "tag" or "release" → version number** `v<version>.<ext>`
  (e.g. `v0.4.2.tar.gz`), matching the `server-rs/Cargo.toml` version / git tag — NOT the timestamp.
