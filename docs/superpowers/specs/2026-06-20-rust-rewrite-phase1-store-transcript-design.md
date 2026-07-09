# Rust Rewrite — Phase 1 (Store + Incremental Transcript Model) Design

**Date:** 2026-06-20
**Repo:** `agentic-dev` (crate `server-rs/`)
**Status:** Design (carries the approved transcript model from the strategy spec; resolves the open store decisions). Awaiting user review → writing-plans.

## Goal

In the Rust crate, implement (1) the sqlite **store** at parity with `server/engine/store.ts` (open an existing DB, same schema + migration, same `Session` shape and ordering), and (2) the **in-memory incremental rendered-transcript model** — the head-of-line-blocking fix — built natively: a per-session append-only rendered projection with O(1) cursor, O(delta) catch-up, O(window) reads, async cold hydration, and an LRU memory bound. No HTTP wiring yet (Phase 5 mounts the endpoints); this phase delivers the store + model + a thin read API, unit-tested.

This phase carries the design approved in `2026-06-20-rust-backend-rewrite-design.md` (§"Phase 1") and in the earlier transcript brainstorm; it adds the resolved tech decisions and the exact module shapes.

## Resolved decisions

- **sqlite via `sqlx`** (async; runtime `query`/`query_as`, NOT the compile-time `query!` macro — so no live-DB-at-build-time / offline-metadata friction). WAL + `busy_timeout` to match the TS store.
- **Cold transcript hydration via `tokio::task::spawn_blocking`** (the one-time full read+parse runs off the async executor); incremental syncs (small new-byte reads) run inline on the async path.
- **Older-history storage = full in-memory rendered projection per warm session + LRU byte cap** (approach A from the transcript brainstorm). Cold/evicted sessions re-hydrate from the file on next access.

## On-disk contract this phase must honor (from `store.ts`)

- DB at `Config::db_path` (default `~/.agentic-dev/db.sqlite`), WAL, `synchronous=NORMAL`, `busy_timeout=5000`.
- `sessions` table — the TS `COLUMNS` (id PK, repo, prompt, worktreePath, branch, claudeSessionId, status, costUsd, exitCode, error, errorKind, createdAt, startedAt, endedAt, baseSha, worktreeState, repos, skills, baseShas, model, effort, mode, seq) plus `ADDED_COLUMNS` (baseSha, worktreeState DEFAULT 'live', repos, skills, baseShas, model, effort, mode, errorKind, **lastUserMessageAt**). JSON columns (`repos`/`skills`/`baseShas`) are TEXT holding JSON. `create()` sets `createdAt = lastUserMessageAt = now`, `seq` auto-increments per-process. `list()` orders by `COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC`. The migration adds missing columns idempotently and backfills `lastUserMessageAt = createdAt WHERE NULL`.
- Per-session log file at `Config::log_dir/<id>.jsonl`, append-only, one JSON event per line.
- `isRendered` line filter (from `renderedLog.ts`): keep lines whose text starts with `{"type":"agentic_prompt"`, `{"type":"stream_event"`, `{"type":"assistant"`, or `{"type":"result"`; drop everything else. (`filterRendered` falls back to the raw lines if filtering yields nothing — replicate that fallback.)

## Components

### `src/store.rs` — `Store`
- Opens the sqlite pool (sqlx `SqlitePool`), applies pragmas, runs `ensure_schema()` (CREATE TABLE IF NOT EXISTS + idempotent ADD COLUMN loop + the `lastUserMessageAt` backfill).
- `Session` struct mirroring `types.ts` (serde field names match the JSON the API will emit; JSON columns deserialized from TEXT).
- `create(input) -> Session`, `get(id) -> Option<Session>`, `list() -> Vec<Session>` (the COALESCE ordering), `update(id, patch)`.
- `append_log(id, line)`, `log_path(id)`.
- All async (sqlx). `seq` is a per-`Store` atomic counter (matches the TS in-process `seq`).
- **Parity test:** open a DB created by the TS server (a fixture copied into a temp dir), `get`/`list` round-trip a known row unchanged; `create` then re-open and read back; the migration adds `lastUserMessageAt` to a legacy-schema fixture and backfills it.

### `src/transcript.rs` — `RenderedProjection`
- Fields: `rendered: Vec<String>`, `byte_offset: u64` (consumed up to in the file), `raw_count: u64`, `bytes: usize` (≈ memory = Σ line lengths).
- `is_rendered(line) -> bool` (the shared filter; single source of truth, reused by the equivalence test).
- `sync(path)`: read from `byte_offset` to EOF (only new bytes, tailer-style), split into complete lines (carry a partial-line remainder), for each: `raw_count += 1`, push to `rendered` if `is_rendered`, add to `bytes`; advance `byte_offset`. The **first** sync (cold, `byte_offset == 0`, whole file) runs inside `spawn_blocking`; incremental syncs (small) run inline.
- `window_tail(limit) -> Window { start, lines, total }` where `start = rendered.len().saturating_sub(limit)`, `lines = rendered[start..]`, `total = rendered.len()`.
- `range(before, limit) -> &[String]` = `rendered[before.saturating_sub(limit)..before]` (scroll-back).
- `slice_from(since) -> &[String]` = `rendered[min(since, len)..]` (WS catch-up).
- `count() -> u64` = `rendered.len()` (the rendered-coordinate cursor).
- **Equivalence invariant (test):** after `sync`, `projection.rendered` equals `filter_rendered(read_whole_file(path))` for fixture logs of mixed line types — guaranteeing the rendered-coordinate cursor matches what the TS server (and thus the existing Android client) expects. Includes the empty-after-filter fallback case.

### `src/transcript.rs` — `TranscriptCache`
- `HashMap<String, RenderedProjection>` + access-order tracking + a byte budget (`AGENTIC_TRANSCRIPT_CACHE_BYTES`, default 256 MB; added to `Config`).
- `get(id, log_path) -> &RenderedProjection` (async): hydrate if absent (first `sync`, off-executor), else incremental `sync`; touch access-order; evict least-recently-used while over budget. In-flight-hydration dedup so concurrent `get`s of a cold session don't double-read.
- `drop(id)` on session delete.
- Shared as `Arc<Mutex<TranscriptCache>>` (or a small actor) in app state; only `sync`'s big read is off-lock/off-executor.
- **Tests:** incremental sync after append yields only new rendered lines; cold hydrate builds full projection; window/range/slice/count correctness; LRU eviction over a tiny budget then re-hydrate; concurrent-get dedup; equivalence invariant.

## Data flow (how the fix removes the re-reads)

- **Open a session** (Phase 5 will call): `cache.get(id).window_tail(K)` → recent window from memory; no whole-file read on a warm session, one `spawn_blocking` read on a cold one.
- **Scroll back:** `cache.get(id).range(before, K)` → in-memory slice.
- **WS catch-up:** `cache.get(id).slice_from(since)` → O(delta) in-memory slice.
- **Send cursor (`POST /messages`):** `cache.get(id).count()` BEFORE the follow-up appends → O(1); eliminates the TS double full-read + `rawToRenderedOffset`.
- Live streaming (engine pub/sub) is unchanged and lands in Phase 4; the projection stays current via its own incremental `sync` (the append-only file is the seam).

## Error handling

- Missing log file → empty projection (parity with `readLog` returning `[]`).
- A torn/partial last line → held in the carry buffer until the newline arrives (tailer semantics), never parsed half.
- sqlx errors → surfaced as a `StoreError` (thiserror); the store API returns `Result`. Malformed JSON in a TEXT column degrades to the documented fallback (mirror `safeJson`), never panics.
- LRU eviction never drops a projection mid-`sync`; eviction picks idle entries only.

## Testing

- `cargo test` per module: store parity (TS-created DB fixture round-trip; legacy migration+backfill; list ordering), projection (sync/window/range/slice/count/equivalence/fallback), cache (LRU/dedup/hydrate).
- A small fixture set under `server-rs/tests/fixtures/`: a `db.sqlite` created by the TS store, and `.jsonl` logs with mixed rendered/non-rendered lines.
- No real claude, no network.

## Out of scope (later phases)

- HTTP endpoints for the windowed transcript (Phase 5).
- The engine (spawn/stream/queue/watchdog), workflows, push (Phases 3–6).
- Android pagination client work (separate track; the Rust API will expose `{log,start,total}` + the scroll-back endpoint when Phase 5 lands).

## Decomposition for the plan

Phase 1 splits into independently-testable tasks: (T1) `Store` schema+CRUD+migration parity; (T2) `RenderedProjection` (sync + window/range/slice/count + equivalence); (T3) `TranscriptCache` (LRU + hydrate + dedup); (T4) wire `Config::transcript_cache_bytes` + `AppState` carries `Store` + `TranscriptCache` (no HTTP yet). The plan details each.
