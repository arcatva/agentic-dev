# Structured stop reason (`errorKind`)

## Problem

When a turn ended in a non-clean state, the Android client decided the stop-reason banner by regex-
matching the free-text `error` string against `/limit|resets/i` and showing **"⚠ Usage limit reached"**
on a hit. Several unrelated failures carry the word "limit", so they were all mislabeled as usage limits:

- the **2h wall-clock watchdog** message (`…exceeded limit 7200s`),
- the **idle watchdog** message (`…(limit 1200s)`),
- the generic non-zero-exit fallback (`…usage limit reached…`).

So a turn that was reaped for running too long, idling, or crashing/OOM showed up as "Usage limit reached".

## Design

The server is the only place that knows *why* a turn ended (watchdog vs. exit code vs. claude's result
event), so it now tags each terminal state with a structured `errorKind`. The client maps the code to a
label and no longer guesses from text.

### Taxonomy (`ErrorKind` in `server-rs/src/engine/classify_error.rs`)

| errorKind     | set where (engine/mod.rs)                     | meaning                                  |
|---------------|-----------------------------------------------|------------------------------------------|
| `usage_limit` | result event isError + `classifyClaudeError`  | the user's usage/quota limit (wait for reset) |
| `rate_limited`| result event isError + `classifyClaudeError`  | transient server rate-limit/overload (retryable, NOT quota) |
| `claude_error`| result event isError + `classifyClaudeError`  | claude reported some other turn error     |
| `wall_timeout`| watchdog                                      | reaped for exceeding the wall-clock cap   |
| `idle_timeout`| watchdog                                      | reaped for being idle too long            |
| `crashed`     | exit handler fallback (non-zero, no error)    | crash / OOM / signal                      |
| `interrupted` | `recover()` + store reconcile (`recover.rs`)  | cut off by a server restart               |
| `null`        | —                                             | clean turn, or a pre-field session/server |

`classify_claude_error` (`server-rs/src/engine/classify_error.rs`) is the **one** place a limit is detected from
free text, and only ever runs on claude's own result text — never on our watchdog messages. It is
order-sensitive: transient rate-limit/overload markers ("temporarily limiting", "not your usage limit",
"overloaded", "rate limit", 429/503) are checked **first** and win, because that text usually also
contains "limit" and would otherwise be mistaken for the user's quota (`usage_limit`). The persisted
`error` string keeps claude's verbatim text, so a pre-errorKind client's `/limit|resets/` fallback still
flags both as "usage limit" — no regression; only new clients tell `usage_limit` and `rate_limited` apart.

### Invariants

- `errorKind` is only meaningful on a `failed`/`killed` session. A fresh turn **clears** `error`,
  `errorKind`, and `exitCode` at start (`start()` for a freshly spawned/resumed process, and the
  stdin-inject path for a live session), so a resumed-then-succeeded session ends `done` with no stale
  failure markers.
- The human-readable `error` string is kept (and was reworded to neutral "cap"/"killed" wording so it
  never trips the legacy client heuristic) — shown as detail under the banner, and as the fallback for a
  `null` errorKind.

### Client (`agentic-dev-android`)

- `Session.errorKind: String?` added (optional → backward-compatible deserialization;
  `Json { ignoreUnknownKeys = true }`).
- `domain/StopReason.kt` — pure `stopReason(errorKind, error, status): String` with `when(errorKind)`
  and a legacy regex fallback for `null`/unknown kinds. Unit-tested in `domain/StopReasonTest.kt`.
- `SessionScreen.kt` resume banner calls `stopReason(...)` instead of the inline regex.

## Backward compatibility

- new server + old app → app ignores `errorKind`, uses its regex; the error string is now honest, so it
  no longer mislabels watchdog/crash failures.
- old server + new app → `errorKind` is null, `stopReason` falls back to the regex.
- old DB → `store.migrate()` adds the `errorKind TEXT` column (idempotent) on boot; old rows read null.

## Deploy

- Server: merge to master, `systemctl --user restart agentic-dev`. `store.migrate()` runs on boot and
  backfills the column. Note (streaming-only): a restart kills running turns — `recover()` finalizes
  them (`interrupted`) and the user resumes. Prefer restarting when sessions are idle.
- Client: rebuild + reinstall the APK (the field/label change is client-side).

## Out of scope (follow-up)

In **streaming** mode, an `is_error` result sets `error`/`errorKind` but leaves status `running`
(awaiting) until the process exits, so a usage limit hit mid-session isn't shown in the resume banner
until the process ends. This is pre-existing behavior of the `error` field; changing streaming status
semantics is a separate decision and was intentionally not bundled here.

## Files

- Server: `types.ts`, `store.ts`, `classifyError.ts` (new), `engine.ts`; tests `classifyError.test.ts`
  (new), `engine.test.ts`, `engine.recover.test.ts`, `store.test.ts`; fixture `fake-claude-crash.sh` (new).
- Android: `data/net/Models.kt`, `domain/StopReason.kt` (new), `ui/session/SessionScreen.kt`; test
  `domain/StopReasonTest.kt` (new).
