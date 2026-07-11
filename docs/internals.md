# agentic-dev — internals & ops notes

Reference notes for working on this repo (architecture facts, ops recipes, headless-stream quirks).
Read this before deep work; keep `CLAUDE.md` for the short always-on rules.

## Service & deploy
- `systemctl --user {restart,is-active} agentic-dev`. This repo is API-only (no served UI); any
  server/API change (routes, engine, parser) needs a restart. The client is the agentic-dev
  Android app (`~/src/agentic-dev-android`), versioned and built separately.
- **Streaming-only (since 2026-06-19): a restart terminates running turns.** Each session is one
  persistent claude streaming process with a live stdin pipe the engine uses to inject follow-up turns.
  That pipe dies with the platform, so a restart kills in-flight turns; on boot `recover()` finalizes
  them from the log (`done` if the last turn emitted a `result`, else `interrupted`) and the user resumes
  via `--resume`. The old classic one-shot backend + systemd transient-unit runner (which *did* survive
  restarts) was removed — see `docs/streaming-only-refactor.md`. (Consequence: the Feature #6 cgroup caps
  `MemoryMax`/`CPUQuota`/etc. were only enforced by the systemd runner, so they are no longer applied.)
- **Runner = the official Agent SDK via a Node bridge.** The Rust `SdkRunner` spawns
  `server-rs/sdk-bridge/sdk-bridge.mjs` per turn; the bridge drives claude through
  `@anthropic-ai/claude-agent-sdk`, not the raw `claude -p` CLI. The SDK is the *harness* side of
  claude's stream-json control protocol — it does the `initialize` handshake and answers
  `can_use_tool`. That is what makes **AskUserQuestion actually wait** for the user (see stream-quirks
  below). The bridge mirrors every SDK message to the session log in the same stream-json shape, so the
  tailer/stream-parser/client are unchanged; it maps `RunSpec` → SDK options (cwd, env incl.
  `CLAUDE_CONFIG_DIR`, model, effort/ultracode, `resume`), auto-allows every tool except
  AskUserQuestion, and supports interrupt (turn only) + stop (SIGTERM — hard kill). The SDK ships its
  own claude binary and uses the `~/.claude` login. There is no raw-`claude`-CLI runner: the old
  `LocalRunner` + `build_spec` argv + `fake-claude.sh` harness was removed. Tests inject an `SdkRunner`
  pointed at a fake bridge script (`tests/fixtures/fake-sdk-bridge-*.sh`, run via `bash` not `node`)
  that appends canned stream-json to `$SDK_BRIDGE_LOG` — same path the real bridge writes. The bridge's
  SDK dep is installed with `npm install` in `server-rs/` after pulling.

## Tests / build
- `make test` (= `cd server-rs && cargo test`) — the whole suite (engine + api, in-crate).
  `make build` runs the bridge's `npm install` then `cargo build --release` (the service binary), so the
  SDK dep is never forgotten; the server also preflights the bridge SDK at boot and logs a fix command if
  it's missing. Keep tests green before commit.

## API smoke check
No UI in this repo. To exercise the API by hand: POST `/api/login` with
`{password: <~/.agentic-dev/login-password>}` → use the returned token as `Authorization: Bearer`
on `/api/sessions`, `/api/repos`, `/api/skills`, etc.; stream a session over the WS at
`/api/sessions/:id/stream`. The Android app (`~/src/agentic-dev-android`) is the real client.

## Headless `claude -p` stream quirks (what a client can/can't show)
- **Subagents** (Task tool — appears as name `Agent` in the stream) are linked by `parent_tool_use_id`
  (null = main agent). A subagent's **internal turns do NOT stream**; only its **input** (a `user` text
  tagged with the parent id) and its **result** (the `tool_result` whose `tool_use_id` is the Agent
  call) are available. The client groups these into a left-list / right-detail agent tree.
- **Workflow** runs in a background channel, NOT the main stream — the Workflow tool_use is the only
  trace there. But claude writes a journal + per-agent transcripts to disk under
  `<configBase>/projects/<cwd-slug>/<claudeSessionId>/`: `workflows/wf_*.json` (status, summary,
  phases, `workflowProgress[]` per-agent with state/model/promptPreview/resultPreview) and
  `subagents/workflows/<runId>/agent-<id>.jsonl` (each agent's full transcript). `server-rs/src/engine/workflows.rs`
  reads these; `GET /api/sessions/:id/workflows` + `…/:runId/agents/:agentId` expose them; the client's
  list shows each run + its agents, selecting an agent lazy-loads its transcript. configBase =
  `<worktreePath>/.claude-config` for skill sessions, else `~/.claude`.
- **Orchestration Mode** (Normal / Workflows / Ultra-code) has no CLI flag — it injects a prompt
  preamble ("ultracode is ON …" / "workflows enabled …"); the keyword `ultracode` + "Workflow tool"
  trigger the opt-in. Persisted on the session; reused on follow-ups.
- **AskUserQuestion** now *waits* for the user (since the SDK-bridge switch, 2026-06-20). The SDK's
  `canUseTool` parks the turn on the question instead of erroring, so it is answered **in the same turn**.
  agentic-dev surfaces it as `{kind:"ask"}`, renders the question + option chips, and the answer is
  delivered as the next user message — the bridge intercepts it while an ask is pending and resolves
  `canUseTool` with `updatedInput.answers`, so no separate follow-up/`--resume` is needed. A parked ask
  emits no log events, so the engine flags it (`pendingAsk`) to exempt it from the idle watchdog (the 2 h
  wall cap still applies). (Before 2026-06-20 the raw CLI had no responder, so AskUserQuestion returned
  `is_error:true "Answer questions?"` instantly and the answer came as a fresh follow-up turn.)

## Plan usage % (billing → subscription)
No per-token `$` billing on the subscription — the UI shows plan-usage %. Source:
`GET https://api.anthropic.com/api/oauth/usage` with the OAuth bearer token from
`~/.claude/.credentials.json` (`claudeAiOauth.accessToken`) + header `anthropic-beta: oauth-2025-04-20`.
Returns `{five_hour, seven_day, seven_day_sonnet, seven_day_opus, …}`, each `{utilization (0–100 %), resets_at}`.
Wrapped at `GET /api/usage` (server reads the token locally; only % reach the client; ~30s cache).
The token is a secret — never print it.

## Line-level diff (single file)
`GET /api/sessions/{id}/commits/{sha}/diff?repo=<repo>&path=<rel>` returns the parsed patch for
ONE file — `{ diff: { path, status, binary, truncated, hunks:[{ oldStart, oldLines, newStart,
newLines, header, lines:[{ kind: context|add|del, oldLine, newLine, content }] }] } }`. `sha` is a
commit hash or the literal `working` (working-tree vs HEAD). `path` is required. Untracked
working-tree files have no git patch, so the server synthesizes an all-additions diff from the file
contents. Binary files set `binary:true` with no hunks; diffs over `MAX_DIFF_BYTES` (1 MB of raw git
output) are cut at a line boundary with `truncated:true`. Engine: `Engine::commit_diff`
(`engine/diff.rs`) validates repo/sha/path then calls `structured_diff::commit_diff_for_repo` (the
unified-diff parser). This is the read backing the client's per-file diff view — it complements
`…/commits/{sha}/files` (which is only +N/-M counts). NB: this reintroduces a line-level diff after
the 2026-06-20 commit-graph slice deliberately dropped one as YAGNI.

## One-click code review
`POST /api/sessions/{id}/review` runs a single Claude pass over the session's **working-tree diff**
(vs HEAD, across all repos) and returns `{ findings:[{severity,file,line,title,detail}], reviewed,
model }`. `reviewed:false` (with empty findings) means there were no tracked changes, so no model
call was made — the route short-circuits. The call is a **stateless direct `/v1/messages` request**
(NOT a session turn / SDK bridge), so it works on running and idle sessions and never touches the
worktree. `engine::diff::working_diff` gathers the capped diff (`REVIEW_DIFF_MAX_BYTES` = 200 KB,
each repo banner-prefixed); `engine::review::AnthropicReviewer` makes the call, reusing the shared
transport `title_client::anthropic_messages` + `resolve_auth_from_env` (one place knows how to talk
to `/v1/messages`). Model = session model → `ANTHROPIC_DEFAULT_SONNET_MODEL` → `claude-sonnet-4-5`.
Errors: no server creds → 503; upstream failure → 502. Untracked files are NOT reviewed (git diff
omits them). Tested via wiremock (`tests/review_client.rs`) — no real network in CI.

## Rewind (code-only restore)
`POST /api/sessions/{id}/rewind { turnIndex }` restores the working tree to the snapshot taken just
before turn `turnIndex` ran. **Code only** — the branch/HEAD and chat history are untouched.
Semantics: tracked files are restored (and tracked deletions recreated), but **untracked files
created since the snapshot are kept** (no `git clean`).

Snapshots are taken best-effort in `follow_up` on the idle/resume path (NOT the live type-ahead
branch — the working tree is mid-write there), labeled with the about-to-start turn index
(`count_user_turns`). `worktree::snapshot_worktree` captures the full working tree (tracked +
non-ignored untracked) into a commit object via a throwaway `GIT_INDEX_FILE` + `write-tree` +
`commit-tree`, pointed at by a **session-scoped** ref `refs/agentic/snapshots/<id>/<turn>` (worktrees
of one repo share its ref store, so the id scope avoids cross-session collision). Restore is
`read-tree <snap^{tree}>` + `checkout-index -a -f` (`worktree::restore_worktree`). `turnIndex == 0`
restores the per-repo base SHA (state before the first prompt ≈ undo all session work). Requires an
idle session (`live_session` → Busy otherwise). Snapshot refs are cleaned up best-effort on discard.
A snapshot only exists for turns reached via a resume follow-up after this shipped; a missing
snapshot → 400. Self-restart-safe (refs persist in the repo, not in memory).

## Design docs
Specs/plans live under `docs/superpowers/{specs,plans}/`.

## Session title generation

Session titles live in `sessions.prompt` and are owned by the first
`submit_session` call. The engine spawns a fire-and-forget task that
calls `engine::title_client::AnthropicHttpTitleGenerator` — a
`reqwest` client that posts to `${ANTHROPIC_BASE_URL}/v1/messages` and
reads the response as plain text. The system prompt asks for a 5–12
character Chinese title that reflects "what the user is doing" rather
than the last sentence. The task has a 20-second hard timeout; on
any failure (timeout, non-2xx, invalid output, latin-only output,
empty content, 401) the original prompt is kept, so behaviour
degrades to pre-feature.

The same generator drives periodic refresh (every 5 user messages).
When `follow_up` increments the turn count to a multiple of 5 and
`AGENTIC_RETITLE` is not `off`, it spawns a task that calls
`maybe_retitle` with `currentTitle` and the last 10 user/assistant
entries. The generator posts a retitle prompt that asks the model to
return `{"change": false}` or `{"change": true, "title": "..."}`;
the engine updates `sessions.prompt` only when the new title passes
validation AND differs from the current one.

Auth resolves in this order: `ANTHROPIC_AUTH_TOKEN` env (preferred —
set by ccswitch), else `~/.claude/.credentials.json::claudeAiOauth.accessToken`.
If neither is set, the generator fails at startup. No refresh loop
in v1: a 401 from the upstream returns `Ok(None)` and the title is
silently kept as the original prompt.

The model is read from `ANTHROPIC_DEFAULT_HAIKU_MODEL` (ccswitch
sets this to whatever model the user has routed); the timeout from
`AGENTIC_TITLE_TIMEOUT` seconds. The endpoint base URL from
`ANTHROPIC_BASE_URL` (defaults to `https://api.anthropic.com`).

`follow_up` no longer retitles by default — the API
`POST /sessions/:id/message` flips `setTitle` from `true` to `false`
in the absence of an explicit value. Clients that want to rename a
session can still pass `setTitle=true`.

## Session fork

A user-initiated fork creates a new server-side session that inherits
the source session's code state (worktree snapshot) and conversation
history (filtered transcript).

`POST /api/sessions/{id}/fork` (no body) returns
`201 { id, session }`. The new session is independent: it gets its
own worktree (branched off the source HEAD per repo), its own log,
its own sqlite row in `status = "pending"`. It is NOT enqueued — it
runs only when the user opens it and sends a follow-up prompt.

The new session's `prompt` column is seeded with the source's
stream-json log filtered to plain text (`engine/transcript_filter.rs`),
prefixed with `Fork of <source prompt>:\n\n`. The new session's
`parentSessionId` points at the source.

Server implementation: `engine::Engine::fork_session` (in
`server-rs/src/engine/mod.rs`). Schema change: `sessions.parentSessionId
TEXT DEFAULT NULL` — added via the `ADDED_COLUMNS` migration in
`engine/store.rs`. Existing rows read `None`.

The fork's worktree branches (`create_fork_worktrees` in
`engine/worktree.rs`) use `git worktree add -b agentic/<new-id> <path>
<base-sha>` so the new branch points at the source HEAD. The
`git_sync` helper is now `pub(crate)` so the rollback helper in
`fork_session` can reuse it.

Failure during fork rolls back any worktrees it created and deletes
the new sqlite row before returning the error.
