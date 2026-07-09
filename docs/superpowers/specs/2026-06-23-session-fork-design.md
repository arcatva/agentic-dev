# Session fork — design

**Goal:** Let a user "fork" an existing agentic-dev session: create a new server-side session that
inherits the source's code state (worktree snapshot) and conversation history (concatenated into
the new session's first prompt). The source session is untouched. The fork is a real, independent
session — its own worktree, its own log, its own claude process.

## Background (verified by reading the codebase)

- An agentic-dev **session** is a per-session git worktree (`agentic/<session-id>` branch on each
  repo) plus a headless claude process spawned into that worktree, with a stream-json log on disk
  and a row in `sessions` sqlite. See `server-rs/src/engine/store.rs` and `worktree.rs`.
- The Android client (`agentic-dev-android`) is the only client; the only API surface to keep
  stable is the HTTP/WS API in `server-rs/src/api/`.
- Worktrees are created with `git worktree add -b agentic/<id> <path>` (see
  `worktree.rs:create_session_worktrees`). The base of a new branch is, by convention, the
  repo's remote default branch (resolved via `git symbolic-ref refs/remotes/origin/HEAD`).
- The session **log** is `stream-json` on disk; the SDK bridge (`server-rs/sdk-bridge.mjs`) reads
  it back via the Agent SDK. New sessions start with no log.
- Claude Code's own `/fork` (slash command) is **process-internal** — it does not create a new
  server-side session, worktree, or sqlite row. It is **not** what we want here.

## 1. Server: `POST /api/sessions/:id/fork`

A new route that takes an existing session id, validates it, and creates a **new session** that
inherits from it.

### Request
```
POST /api/sessions/:id/fork
Body: { }   // v1 takes no body
```

### Response
```
201 Created
{ "id": "<new-id>", "session": <Session> }
```

`Session` is the same shape returned by `POST /api/sessions` (see
`api/sessions.rs:create_session`).

### Errors
- `404` — source session id not found
- `409` — source session's per-repo worktree is unhealthy (e.g. its `.git` is missing — we
  cannot snapshot a HEAD that is unreadable)
- `500` — underlying git failure (any worktree creation step); body carries the git stderr
  truncated to 200 chars

### Implementation outline
1. Load the source session via `st.engine.get(src_id)`. 404 if missing.
2. For each `repo` in source.repos, resolve the source worktree path
   (`<worktrees_root>/<src_id>/<repo>`), read its HEAD with `git -C <wt> rev-parse HEAD`. If
   the dir is missing or the command fails, return 409.
3. Generate a new session id (reuse the id-generation used by `create_session`; the simplest
   path is to delegate to the same helper).
4. For each repo, create the new worktree **branched off the source HEAD**, not the remote
   default:
   ```
   git -C <repo_root> worktree add -b agentic/<new-id> \
       <worktrees_root>/<new-id>/<repo> <src-head-sha>
   ```
5. Read the source's log file (`<log_dir>/<src_id>.jsonl`), filter it to a plain-text transcript
   (see §2), and build the **seed prompt** that the new session will carry in its `prompt`
   column. Use the source's own `prompt` (truncated to 50 chars) as the visible label so the
   list row reads "Fork of <source prompt>…" — the same column is the list-row title:
   ```
   Fork of <src.prompt (first 50 chars)>:

   <filtered transcript>
   ```
   The seed prompt is **stored but not consumed automatically** — the new session is created in
   status `"pending"` and **not** enqueued. It only runs when the user opens the new session and
   sends a real follow-up prompt (which gets appended via the normal follow-up turn path; the
   seed prompt serves as the conversation-history context for that first turn, not as the
   turn's user message).
6. Insert the new sqlite row. The new session has:
   - `id` = new id
   - `repos`, `skills`, `hidden_skills`, `model`, `effort`, `mode`, `permission` — copied from source
   - `parent_session_id` = `src_id` (new column, see §3)
   - `prompt` = the synthesized fork seed prompt from step 5. The existing `sessions.prompt`
     column doubles as the session's display title in the list (no separate `name` column
     exists), so the seed prompt is what the list shows. Storing "Fork of <source prompt>:
     <transcript>" means the list row reads like "Fork of <source prompt>…" and the user
     keeps a clear visual link to the parent.
7. Return the new session.

If step 4 or 5 fails partway, **roll back**: remove any worktrees already created, delete the
new sqlite row, and surface the error.

### Why "filter log into first prompt" instead of SDK resume
Claude SDK's `resume` takes a Claude-internal session id (UUID), not an agentic-dev session id,
and resolves it via the user's `~/.claude` projects tree. That is **orthogonal** to agentic-dev's
local log model. Copying the log into the new session's first prompt is a few dozen lines, has
zero coupling to Claude internals, and is the "lightweight, zero-dep" choice confirmed during
brainstorming.

## 2. Log → readable transcript (pure function)

Lives in `server-rs/src/engine/transcript.rs` (or a new `fork.rs` — implementer's call, but keep
it as a pure fn in `engine/` because it is engine-level and unit-tested in-crate).

Input: stream-json log contents (one JSON object per line). Output: a single string with
`<role>: <text>` lines, one block per turn, in order.

Rules:
- Walk top-level `user` and `assistant` message frames.
- For each, concatenate the `content` blocks that are plain text. **Drop** tool_use / tool_result
  blocks — they are noise to a follow-up prompt and would bloat the context.
- Skip the synthetic `agentic_prompt` / `system` frames (engine bookkeeping) entirely.
- Hard cap: 50,000 characters; truncate with a trailing `[... truncated, full log retained on
  source session ...]`. (50k is well under any model's input limit, leaves headroom for the
  user's real follow-up.)
- Strip control characters that would break the prompt.

Unit tests:
- empty log → empty string
- log with only tool_use → empty string
- log with mixed user/assistant/tool frames → only user/assistant text in order
- log > 50k chars → truncated with the marker

## 3. Schema: `parent_session_id`

`Session` (`server-rs/src/engine/store.rs`) gains a new field:

```rust
pub parent_session_id: Option<String>,
```

- `CreateInput` is **not** changed — fork bypasses the create input and writes the field
  directly via the store.
- `SessionPatch` is **not** changed — `parent_session_id` is immutable after creation.
- Existing rows get `None` on read (column added with `DEFAULT NULL`, sqlite migration is
  additive — no destructive schema change).
- `list_sessions` returns the field; client surfaces it.
- New store helper `list_children(parent_id) -> Vec<Session>` (already a `WHERE` filter; thin
  wrapper, no need to expose via API in v1 if not used).

## 4. Android client

### SessionScreen (per-session detail)
- Add a `Fork` `IconButton` (`Icons.Rounded.CallSplit`) to the top bar's `actions` slot, next to
  the existing history/workflows icons.
- On click: call a new `agenticApi.fork(id)`. On success: navigate to the new session's detail
  (or stay and let the user see the snackbar — match the team's existing post-action nav
  convention; for v1, **navigate to the new session** so the user can immediately send the
  follow-up prompt that starts the new session).
- On failure: snackbar with the server's error message.

### HomeScreen (session list row)
- Add a "Fork" action to the multi-select mode (currently home has long-press → multi-select
  → Delete). After fork, the list reloads (the existing list-refresh path picks up the new
  session) and shows a snackbar "Forked".
- v1 does not add a per-row overflow menu — the team hasn't shipped that UI surface for any
  other action, so a row-level Fork menu would be inconsistent. Future work can add one.

### Forked-from affordance
- SessionScreen shows a small chip / subtitle `Forked from <source prompt (truncated)>`
  (clickable, jumps to source) when `parent_session_id != null` and the source is fetchable.
- The new session's `prompt` (which doubles as the list-row title — there is no separate name
  column) is prepended with `"Fork of <source prompt>:\n\n"` so the list row reads
  "Fork of <source prompt>…" without any client-side rewriting.

### Tests
- `FakeAgenticApi` (`app/src/test/.../FakeAgenticApi.kt`) gains a `fork(id)` stub.
- `SessionViewModelTest`: tapping Fork with the stubbed API navigates with the new id; tapping
  Fork with a failing API shows the error message and does not navigate.
- `HomeViewModelTest` (or wherever the row-menu lives): the row menu exposes a Fork action.

## 5. Testing (server)

- `engine`: pure-fn tests for the log filter (see §2).
- `engine`: store round-trip with `parent_session_id` (read/write/null default).
- `api`: integration test using `test_support` — `POST /fork` on a known session:
  - 404 on unknown id
  - 201 on success; response `id` is new; new session's `parent_session_id == src_id`; new
    session's worktree exists with HEAD equal to source HEAD per repo
  - 409 when the source worktree is removed before the call
- `api`: regression that `POST /fork` does **not** spawn a claude process (no `RunningTurn` is
  created). The new session's `status` stays `"pending"` (the value written by `Store::create`;
  see `store.rs` line ~222) — verify by reading the returned `Session` from the response.

## 6. Risks / non-goals (v1)

- **Not implemented**: chained-fork tree view in the client (showing the full
  "forked from X, which was forked from Y"). Single-level `parent_session_id` is enough for v1;
  the chain can be computed lazily.
- **Not implemented**: "close source on fork" / "freeze source". The source stays live and
  writable, per the brainstorming decision.
- **Not implemented**: auto-start of the new session. The new session sits idle until the user
  opens it and sends a follow-up prompt (the normal follow-up path handles the spawn).
- **Not implemented**: integrating Claude Code's `/fork` slash command. The two are
  semantically different; conflating them would break the isolation model.
- **Log filter is lossy** (drops tool calls). Acceptable for v1 — the goal is to give the new
  claude process enough context to keep going, not to reproduce the exact log. Tool-call details
  are still on disk in the source session if the user wants to look them up.
- **Large worktrees**: `git worktree add` with an explicit base commit is O(1) on the git side
  (it just records the ref) — we are not copying the working tree contents at the FS level.
  Confirmed by reading `worktree.rs`; the helper uses `git worktree add -b`, not `cp -r`.
