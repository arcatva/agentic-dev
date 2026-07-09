# Rust + Tokio Backend Rewrite — Migration Strategy & Phase 0/1 Design

**Date:** 2026-06-20
**Repo:** `agentic-dev` (the backend; client `agentic-dev-android` is untouched)
**Status:** Strategy approved; Phase 0/1 ready for implementation planning

## Goal

Rewrite the agentic-dev backend (currently TypeScript/Node + Fastify) in **Rust + Tokio**, keeping the HTTP + WebSocket API byte-for-byte compatible so the Android client needs no changes. The rewrite folds in the previously-designed transcript-serving fix (in-memory incremental rendered projection + pagination) natively, so the head-of-line-blocking problem is solved by construction rather than patched.

**Why a rewrite (decision of record):** The acute driver was head-of-line blocking from re-reading + re-parsing whole multi-MB transcripts on the request path. That is fixable in TS alone (cheaper, lower risk), and that was the recommendation. The user chose the Rust rewrite as a strategic decision, accepting the cost; this spec executes that decision responsibly — phased, parity-validated, reversible.

## Non-goals

- No Android client changes in the backend phases (the windowed-transcript client work is a separate, already-scoped effort; the Rust API exposes the windowed endpoints, the client adopts them on its own track).
- No new product features. Behavioral parity with the current TS backend is the bar.
- No big-bang "stop everything for weeks." Each phase is independently built and tested against the TS reference.

## The migration seam: shared on-disk state, two interchangeable binaries

The engine is **stateful and single-owner** — it holds live `claude` child processes, an in-memory WebSocket subscriber registry, and the run queue. You cannot run half the live sessions in Node and half in Rust (a session's child process + its WS subscribers live in one OS process). So this is **not** a per-endpoint strangler migration.

Instead, the migration seam is the **on-disk state format**:

> Both implementations read/write the **same** durable state — the sqlite DB, the per-session `<id>.jsonl` logs, and the per-session `.claude-config` layout. The Rust binary matches that format exactly. **Cutover = a restart that happens to start a different binary.** In-flight turns are marked `interrupted` (identical to today's restart behavior), and the new process recovers sessions from disk via the same `recover()` logic.

Consequences:
- **Parity is testable.** Run the Rust server against a *copy* of the real `~/.agentic-dev` state on a test port; point a client at it; behavior must be identical.
- **Rollback is trivial.** The cutover is swapping the systemd unit's `ExecStart` back to the Node binary (the on-disk state is unchanged/compatible).
- **The contract to preserve:** the on-disk format (below) + the HTTP/WS API.

## API stability contract (must not change)

The Android client (`agentic-dev-android`) talks to a fixed surface; the Rust server must serve it identically:
- REST: `GET /api/sessions`, `GET /api/sessions/:id` (+ windowed `?limit`/`?before`), `POST /api/sessions`, `POST /api/sessions/:id/messages`, `DELETE /api/sessions/:id`, kill/diff/discard/file/upload, `/api/sessions/:id/workflows[...]`, `/api/usage`, config, auth.
- WS: `GET /api/sessions/:id/stream?since=N&token=...` — backfill from `since` (rendered-line coordinates) then live events.
- Auth: HMAC bearer token, **same secret + same scheme** (`AGENTIC_AUTH_SECRET`) so existing tokens validate.
- Wire types: the JSON shapes in `server/engine/types.ts` + `server/api/*` are the source of truth; Rust `serde` structs mirror them field-for-field (including the new `lastUserMessageAt`, `workflowRunning`, and the windowed `{ log, start, total }` response).

## On-disk state contract (Rust must match)

- **sqlite** at `~/.agentic-dev/db.sqlite` (WAL). The `sessions` table schema from `server/engine/store.ts` (`COLUMNS` + `ADDED_COLUMNS`), including `lastUserMessageAt`. Rust runs the same `CREATE TABLE IF NOT EXISTS` + idempotent column migration so it can open an existing DB.
- **Per-session logs** at `~/.agentic-dev/logs/<id>.jsonl` — append-only, one JSON event per line, written by the claude subprocess's stdout capture + `agentic_prompt` markers.
- **Per-session config** at `<worktreePath>/.claude-config/...` (projects/workflows/subagents layout) — Rust reads the same paths for workflow status.
- **Config/env**: `~/.agentic-dev/service.env` + the systemd unit env (`AGENTIC_PORT`, `AGENTIC_HOST`, `AGENTIC_MAX_CONCURRENT`, `AGENTIC_AUTH_SECRET`, `AGENTIC_STREAMING`, watchdog/idle envs, etc.) — same names + semantics.

## Tech stack

- Runtime: **tokio** (multi-threaded). CPU-heavy one-shot work (e.g. cold transcript hydration) uses `spawn_blocking` / a bounded pool, never the async executor.
- HTTP: **axum**. WebSocket: axum's `ws` (tungstenite).
- sqlite: **sqlx** (async, compile-time-checked) — or `rusqlite` behind `spawn_blocking` if sqlx friction appears; decided in Phase 1's plan.
- Serialization: **serde / serde_json**.
- Subprocess (spawn the claude SDK binary, stream stdout stream-json): **tokio::process**.
- Auth: **hmac + sha2**. Push (FCM v1): **reqwest** + JWT.
- Crate location: **`agentic-dev/server-rs/`** (a sibling to `server/`), so it shares the repo + docs during development; it can graduate to its own repo at cutover.

## Phase decomposition (each phase = its own spec → plan → TDD cycle)

| Phase | Scope |
|---|---|
| **0** | Scaffold: cargo workspace, axum skeleton, config loader (env parity), HMAC auth (token-compatible), health endpoint, CI/test harness, fake-claude fixture port. |
| **1** | **store**: sqlite schema + migration parity; per-session jsonl logs; **and the new in-memory incremental rendered-transcript model + windowed read API** (the HOL-blocking fix, built natively). |
| **2** | stream-json parser: port `streamParser` event model (the `ClaudeEvent` enum) + `renderedLog` filter rules. |
| **3** | spawner/runner: spawn the claude SDK binary, stdio stream-json, incremental tailing, worktree sync. |
| **4** | engine: queue/concurrency/pump, subscribe·emit pub/sub, watchdog, recover, followUp, kill, workflows reading, structured turn-lifecycle logging. |
| **5** | API + WS: all REST routes + the WS stream endpoint to full parity (incl. windowed transcript endpoints, client-disconnect log downgrade). |
| **6** | push (FCM), usage cache, diff/upload/file, misc endpoints. |
| **7** | conformance vs TS + cutover: cross-impl conformance suite, run against a real-state copy, systemd cutover + rollback runbook. |

Phases 2–7 get their own design specs as we reach them. This spec details Phase 0 and Phase 1.

## Phase 0 — scaffold (detail)

- `server-rs/Cargo.toml` (tokio, axum, serde, sqlx, hmac, sha2, anyhow/thiserror, tracing). Workspace layout: `src/main.rs`, `src/config.rs`, `src/auth.rs`, `src/api/mod.rs`, plus module stubs for `store`, `engine`, `stream` to be filled in later phases.
- **config**: load the same env vars (`AGENTIC_*`) with the same defaults (`maxConcurrent = AGENTIC_MAX_CONCURRENT or unlimited`, port 7420, etc.). Unit-tested against the documented defaults (mirror `config.test.ts`).
- **auth**: HMAC token verify/sign matching the TS scheme exactly (same `AGENTIC_AUTH_SECRET`, same token format) — a token minted by the TS server must verify in Rust and vice-versa. Cross-checked with a known token vector.
- **health**: `GET /healthz` (new, internal) + the existing `/api/config` shape, so we can smoke-test the server boots and authenticates before any engine exists.
- **fake-claude**: port `server/test/fixtures/fake-claude.sh` usage so integration tests never hit the real claude.
- Deliverable: `cargo test` green; the server boots, serves `/api/config` with auth, rejects bad tokens.

## Phase 1 — store + incremental transcript model (detail)

This phase carries the HOL-blocking fix. Two parts:

### 1a. sqlite store parity
- Open `~/.agentic-dev/db.sqlite` (WAL, busy_timeout) and run the same `CREATE TABLE IF NOT EXISTS sessions (...)` + idempotent `ALTER TABLE ... ADD COLUMN` migration as `store.ts`, including the `lastUserMessageAt` backfill. A Rust process must open a DB created by the TS server and round-trip a session unchanged.
- `Session` struct mirrors `types.ts` (serde rename to match JSON field names). `create`/`get`/`list`(ordered by `COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC`)/`update`.
- Per-session log file append (`appendLog`) + `logPath`.

### 1b. In-memory incremental rendered-transcript model (the fix)
The agreed design, native in Rust:

- **`RenderedProjection`** (per session): holds `rendered: Vec<String>` (the filtered lines), a file **byte cursor** `byte_offset`, `raw_count`, and an approximate `bytes` for memory accounting.
  - `sync()`: read from `byte_offset` to EOF (tailer-style, **only new bytes**), ingest each new raw line (`raw_count += 1`; push to `rendered` iff it passes the same `isRendered` filter as `renderedLog.ts`), advance `byte_offset`. O(new bytes). The **first** sync of a cold/large session reads the whole file once — done with `spawn_blocking` (or chunked with yields) so it never blocks the async executor.
  - `window_tail(limit)` → `{ start, lines, total }` (recent window); `range(before, limit)` (scroll-back); `slice_from(since)` (WS catch-up); `count()` (= rendered length, the cursor) — all O(window)/O(delta), in memory.
- **`TranscriptCache`**: `Map<id, RenderedProjection>` with a byte budget (`AGENTIC_TRANSCRIPT_CACHE_BYTES`, default 256 MB) and LRU eviction; `get(id)` returns a `sync()`-ed projection, hydrating (with in-flight-dedup) if absent; dropped on session delete.
- **Equivalence invariant (must test):** `projection.rendered` equals the current `filterRendered(readLog)` output for any fixture log — so existing clients' rendered-coordinate cursors stay correct.
- **Serving foundations** (wired fully in Phase 5, but the model + a thin read API land here): window-open, scroll-back range, WS catch-up slice, and the O(1) send-cursor (`count()`), all served from the projection — **no whole-file re-read, no `rawToRenderedOffset`, ever on the hot path.**

- Deliverable: `cargo test` green for store parity (open a TS-created DB) + the projection (incremental sync, cold hydration off-executor, window/range/slice/count, LRU eviction + re-hydrate + concurrent-get dedup, the equivalence invariant).

## Parity / conformance testing strategy

- **Per-module unit tests** (`cargo test`) at each phase, mirroring the TS tests' intent.
- **Cross-impl conformance** (Phase 7, but seeded earlier): a suite of API request/response + WS-transcript scenarios run against both the TS and Rust servers (same fake-claude, same fixture state) asserting identical responses.
- **Real-state shadow run:** before cutover, run the Rust server on a test port against a *copy* of `~/.agentic-dev`; drive it with the Android client; confirm identical behavior (list order, transcript content/cursors, streaming, follow-up, workflows).
- **The Android client is the ultimate parity oracle** — it must not be able to tell which backend it's talking to.

## Risk & rollback

- **Reference implementation stays live:** the TS backend keeps running and being maintained until the Rust version passes conformance + shadow-run. No development freeze on the TS side beyond avoiding gratuitous format changes.
- **Reversible cutover:** swap the systemd unit back to the Node `ExecStart`; on-disk state is compatible both ways.
- **Format drift is the main hazard:** any change to the sqlite schema or log/event JSON must be made in both implementations (or in the TS one and mirrored) until cutover. Phase 7 includes a format-conformance check.
- **CPU discipline:** the one real "don't block the executor" hazard (cold transcript hydration, large diffs) must use `spawn_blocking`; lint/review for accidental sync heavy work on async paths.

## Per-phase process

Each phase runs the full loop: brainstorm (design spec) → writing-plans (implementation plan) → subagent-driven TDD execution → review. This spec is the roadmap + Phase 0/1 design; Phases 2–7 get their own specs as we reach them.
