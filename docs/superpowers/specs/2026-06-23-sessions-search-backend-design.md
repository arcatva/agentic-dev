# 2026-06-23 sessions search backend design

Status: design, ready for review.
Owner: arc.
Companion: `agentic-dev-android/docs/superpowers/specs/2026-06-23-session-search-and-login-ux-design.md`.

## Context

The Android client is adding content search across session list entries. It needs a backend route that searches session metadata plus the rendered transcript projection and returns per-session match snippets. Today's `/api/sessions` route only returns `Session` rows, not transcripts, so the client cannot search content without first loading each session's transcript (expensive and out of scope for the list screen). A new route solves that.

## Goals

- Single endpoint that returns matching `Session` rows plus short, ranked, per-session snippets.
- Strict cost caps so a query cannot wedge the engine.
- Same auth model as other session routes.
- A small, explicit request/response shape that the Android client can deserialize without special handling.

## Non-goals

- Persistent or in-memory full-text index. Linear scan with caps is sufficient for the first version.
- Searching thinking text.
- Fuzzy matching, tokenization, stemming. Case-insensitive substring only.
- A separate ranking server, ML-based relevance, or query parsing beyond trim + min-length.

## API

### `GET /api/sessions/search`

Query parameters:

- `q` (required): trimmed query string. Minimum 2 characters. Shorter → `{ "query": q, "results": [] }`.
- `limit` (optional): default 50, max 50.

Authentication: same as other `/api/sessions/...` routes. No anonymous search.

Response:

```json
{
  "query": "build failed",
  "results": [
    {
      "session": {
        "id": "ses_abc",
        "prompt": "fix the login bug",
        "status": "done",
        "repos": ["agentic-dev"],
        "createdAt": 1700000000000,
        "endedAt": 1700000050000
      },
      "score": 12.3,
      "matches": [
        { "field": "notes",       "snippet": "...the build failed in step 2 because...", "lineIndex": 412 },
        { "field": "toolSummary", "snippet": "Bash · npm run build",                        "lineIndex": 410 }
      ]
    }
  ]
}
```

`field` is a string enum:

- `title`, `repo`, `branch`, `sessionId`, `status`, `error`
- `prompt`
- `notes`, `answer`
- `toolName`, `toolSummary`, `toolDetail`
- `spawnDesc`, `spawnResult`
- `skill`, `workflow`, `ask`, `plan`, `perm`
- `attachment`

`snippet` is at most 200 characters. The handler trims on whitespace when possible and prepends `...` if it cut content. `lineIndex` is the rendered-projection line offset of the match (matches the same `since`/`start`/`total` semantics used by `GET /api/sessions/:id` and the WebSocket backfill).

`score` is a synthetic float the client does not need to interpret; it reflects ranking tier plus a small bonus for metadata matches. It exists for test assertions and future filters.

### Errors

- 400 if `q` is missing or only whitespace.
- 401/403 if authentication fails (same as existing session routes).
- 500 only on a hard error; the search service swallows per-session errors and skips that session.

## Ranking

Tiered ranking, evaluated in order:

1. Tier A — title/prompt hit (`title`, `prompt`).
2. Tier B — metadata hit (`repo`, `branch`, `sessionId`, `status`, `error`).
3. Tier C — transcript hit (`notes`, `answer`, `tool*`, `spawn*`, `skill`, `workflow`, `ask`, `plan`, `perm`, `attachment`).

Within a tier, sort by:

1. `score` desc.
2. `COALESCE(lastUserMessageAt, createdAt)` desc.
3. `seq` desc.

## Search input

- Trim whitespace.
- Case-insensitive.
- Substring match only.
- No regex, no escaping beyond literal substring.

## Search over the transcript

Use the same rendered projection that `GET /api/sessions/:id` and the WebSocket backfill use. That means:

- Only these raw `type` values are visible: `agentic_prompt`, `stream_event`, `assistant`, `result`, `agent_result`. Everything else is dropped (`system` init/retry, raw `user` tool_result, control frames, etc.).
- Parsing rules mirror `engine/stream.rs::parse_line` and `to_wire` so the snippet context the user sees matches what the Android client renders.
- Thinking text (`ClaudeEvent::Thinking`, wire `kind:"thinking"`) is excluded.
- The `engineExit` synthetic frame, `init`, and `backfill` are excluded.

Per-line classification into `field` values:

- `agentic_prompt` → `prompt`.
- `stream_event` with `event.delta.type == "text_delta"` → `notes`.
- `stream_event` with `event.delta.type == "thinking_delta"` → dropped.
- Final `assistant` `tool_use name == "Skill"` → `skill`.
- Final `assistant` `tool_use name == "Agent" | "Task"` → `spawnDesc`; spawned `agent_result` → `spawnResult`.
- Final `assistant` `tool_use name == "Workflow"` → `workflow`.
- Final `assistant` `tool_use name == "AskUserQuestion"` → `ask`.
- Final `assistant` `tool_use` of any other name → `toolName` (the tool name) plus `toolSummary` and `toolDetail` derived the same way Android derives them (so a search for `cat src/foo.kt` works whether the user remembers the file path or the `Read` summary).
- `result` non-blank text → `answer` (final result/error text).
- `agentic_perm` with `permKind != "plan"` → `perm`.
- `agentic_perm` with `permKind == "plan"` → `plan`.
- `agentic_file` → `attachment`.

Metadata:

- `id` → `sessionId`.
- `prompt` (top-level) → `title` for ranking, but snippet is the literal prompt text.
- `repos[]` → `repo`.
- `branch` → `branch`.
- `status` → `status`.
- `error` → `error`.

## Cost guards

- `q.length() < 2` → return empty immediately, do not iterate sessions.
- `limit` clamped to `[1, 50]`.
- Per session:
  - Stop after 3 matches.
  - Stop after 50,000 rendered lines scanned.
  - Stop after 50 ms wall clock (instrumented but not enforced in v1).
- Per query:
  - Stop after `limit` results.
- The search service holds an `Arc<Engine>` reference and reads through it; it does not block the writer or the WebSocket loop.

## Concurrency and state

- Search reads the same `TranscriptCache` / `RenderedProjection` that the WebSocket backfill uses, so `since`/`start`/`total` semantics line up.
- A snapshot of the session list is taken at request entry (`Store::list()`); the list itself is not locked. New sessions arriving mid-query simply do not appear in the response. Deletions are tolerated by skipping per-session errors.
- Per-session scanning is sequential for now. Parallelism is not needed at v1 caps; it can be added later without changing the API.

## Implementation outline

- `engine/search.rs` (new):
  - `pub struct SearchService { engine: Arc<Engine> }`.
  - `pub fn search(&self, query: &str, limit: usize) -> Result<SearchResponse>`.
  - Internal helpers: `search_session`, `scan_rendered_lines`, `derive_match_field`, `extract_snippet`, `rank_session`.
- `api/sessions.rs`:
  - `pub async fn search_sessions(State(state): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> Result<Json<SearchResponse>, ApiError>`.
  - Validates `q`, clamps `limit`, calls `SearchService::search`, returns JSON.
- `api/mod.rs`: register `GET /api/sessions/search` ahead of `/api/sessions/:id` so the static path wins.

## Testing

Unit tests in `engine/search.rs`:

- Title vs metadata vs transcript ranking order.
- Case-insensitive match, trimmed query, min-length 2, short query returns empty.
- Snippet length cap 200, ellipsis prefix when truncated, trim on whitespace.
- Per-session 3-match cap, query-level 50-result cap.
- Thinking excluded.
- `system` init/retry, raw `user` tool_result, control frames, `engineExit`, `init`, `backfill` excluded.
- `result` with text produces `answer` match.
- `Skill` / `Agent` / `Task` / `Workflow` / `AskUserQuestion` produce the correct `field`.
- Ordinary `tool_use` produces `toolName` + derived `toolSummary` + derived `toolDetail`.
- `agentic_perm` produces `perm` or `plan` based on `permKind`.
- `agentic_file` produces `attachment`.
- Deleted/missing session is skipped without panic.
- Stable ordering: ties broken by `lastUserMessageAt` desc, then `seq` desc.

Route tests in `tests/search.rs`:

- 200 on a basic match; response shape matches the spec.
- 400 on missing or empty `q`.
- Auth required when other session routes are.
- 50-result cap with synthetic 60-session fixture.

## Files touched

- `agentic-dev/server-rs/src/api/mod.rs`
- `agentic-dev/server-rs/src/api/sessions.rs`
- `agentic-dev/server-rs/src/engine/search.rs` (new)
- `agentic-dev/server-rs/src/engine/mod.rs` (export `SearchService` if needed)
- `agentic-dev/server-rs/tests/search.rs` (new)

## Out of scope / follow-up

- Persistent full-text index if linear scan proves too slow.
- A typed `SearchQuery` parser (e.g. `repo:foo status:running build`) — pure substring is enough for v1.
- Highlighting matches in the snippet (client-side concern).
- Streaming search results.
