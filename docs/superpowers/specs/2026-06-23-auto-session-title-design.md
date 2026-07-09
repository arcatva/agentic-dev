# Auto session title — design

Date: 2026-06-23
Status: draft (awaiting user review)
Repo: `agentic-dev` (server-rs)

## Problem

`sessions.prompt` is currently displayed as the session title in the Android
client. It is overwritten on every follow-up with the user's latest raw
message (default `setTitle=true` in the `POST /sessions/:id/message` API).
A one-word reply such as "ok" therefore destroys the original title and the
session list becomes unreadable.

## Goal

Session titles should describe what the session is doing, not what the user
last typed. They should be set automatically when a session is created and
remain stable across follow-ups.

## Non-goals

- Periodic re-titling of long-running sessions.
- Per-message retitling.
- A separate `summary` field for the session list preview.
- Changing the existing `setTitle` API contract (we only flip the default).

## Design

### Trigger

Title generation runs **once per session**, on first submission
(`Engine::submit_session`), **before** the main run is enqueued. Follow-ups
do not retitle. The Android client never needs to know about the change.

### Generation

A separate lightweight `claude -p` call produces the title. The call:

- Uses model **`haiku`** (5–12 Chinese characters does not need Sonnet).
- Has `--max-turns 1` (no tool use).
- Has a fixed system prompt that constrains the output to 5–12 Chinese
  characters, no markdown, no quotes, no explanation.
- Receives only the user's original `prompt` as input (the simplest input
  that still solves the core problem; see brainstorm Q3/A).
- Has a hard timeout of **5 seconds**.

If the call times out, errors, returns an empty string, or returns a string
that fails the length / character-class check, the engine falls back to
the user's original `prompt` as the title (the current behaviour).

### Where it runs

A new module `server-rs/src/engine/title.rs` exposes:

```rust
pub async fn generate_title(
    prompt: &str,
    bin: &Path,        // path to the `claude` binary
    fake: Option<&Path>, // test-only override (env or arg)
) -> Option<String>
```

Internally it spawns `claude -p --model haiku --max-turns 1 --system-prompt ...`
with `prompt` on stdin, captures stdout, trims, validates. Returns `None`
on any failure path. The engine calls it from `submit_session` between
`store.create(...)` and `state.queue.push_back(...)`. The result, if any,
is written via a second `store.update(id, SessionPatch { prompt: Some(t) })`
before the run is enqueued. On `None`, the create-time `prompt` is left
untouched.

### Default behaviour change for follow-ups

`POST /sessions/:id/message` keeps the existing `setTitle` parameter.
**Its default flips from `true` to `false`** in the request struct on the
Android side. The server already accepts `setTitle=false` (covered by an
existing test in `engine/tests.rs`), so no server change is required for
this. The Rust default in the API struct also flips to `false` so any
unaware caller (curl, scripts) gets the new behaviour.

### Failure modes & fallback table

| Condition                                | Title used               |
| ---------------------------------------- | ------------------------ |
| Generation succeeds + passes validation  | generated string         |
| Generation times out (>5s)               | original user prompt     |
| Generation errors / non-zero exit        | original user prompt     |
| Output empty or fails length check       | original user prompt     |

In every fallback case, behaviour is identical to today.

### Validation rules

Generated title must:

- Be non-empty after trim.
- Be ≤ 24 characters (covers Chinese + emoji + English mix).
- Not start with `#` / `>` / backtick (avoids accidental markdown).
- Not contain newline.

If any rule fails → fallback to user prompt.

## Testing

Tests live in `server-rs/src/engine/title.rs` (unit) and
`server-rs/src/engine/tests.rs` (integration, using the existing fake
`claude` scripts in `server-rs/tests/fixtures/`).

Cases to cover:

1. `generate_title` with a fake `claude` that prints a valid title →
   returns the trimmed string.
2. Fake exits non-zero → returns `None`.
3. Fake outputs empty string → returns `None`.
4. Fake outputs a 30-character string → returns `None`.
5. Fake outputs a markdown-flavoured string → returns `None`.
6. Engine `submit_session` with a fake `claude` that succeeds → resulting
   session row has `prompt == generated_title`, not the original user
   prompt.
7. Engine `submit_session` with a fake `claude` that errors → resulting
   session row has `prompt == original_user_prompt`.
8. `follow_up` with `setTitle=true` and `=false` continues to behave as
   today (existing test, no regression).

## Risks

- **Latency on first message**: up to ~5 s added before the run starts.
  Mitigated by the timeout and by the fact that the run itself takes much
  longer; the perceived delay is bounded by the longer of the two. Worth
  measuring in dev once implemented; consider making the title generation
  non-blocking (fire-and-forget) if it proves noticeable — but only after
  measuring, YAGNI.
- **Cost**: one extra Haiku call per session. ~$0.0001 each. Negligible.
- **Existing clients**: any client that relied on `setTitle` defaulting to
  `true` will silently stop retitling. Document in `CHANGELOG` and
  `docs/internals.md`.

## Out of scope (future ideas)

- Per-session `summary` field for list previews.
- Title updates based on full conversation (would require parsing Claude
  output / additional call after run ends).
- Client-side "rename" UI (the server already supports `setTitle=true`).
