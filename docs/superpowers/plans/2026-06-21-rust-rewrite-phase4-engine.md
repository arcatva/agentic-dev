# Rust Rewrite — Phase 4 (engine: queue/concurrency/pump, subscribe·emit pub/sub, watchdog, recover, followUp, kill/interrupt, workflows reading, structured turn-lifecycle logging) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `server-rs/`, port the stateful single-owner engine to Rust at behavioral parity with `server/engine/engine.ts` (+ `workflows.ts` `hasActiveWorkflow`, `classifyError.ts` `classifyClaudeError`, `repos.ts` `ensureLocal`). The engine owns the run queue, the live-process registry (one persistent `claude` per session, follow-ups injected over stdin), the per-session subscriber pub/sub, the watchdog, `recover()` (finalize sessions left "running"/"pending" from disk on boot), `submitSession`/`submit`/`followUp`/`kill`/`interrupt`/`discard`/`deleteSession`, `withActivity` (surfacing `awaitingInput`/`workflowRunning`/`activity`), and the structured turn-lifecycle logging (`turn_start`/`turn_result`/`turn_end`). This is everything the Phase 5 HTTP/WS layer drives. **No axum/HTTP wiring here** (Phase 5 consumes this engine); **no `commitGraph`/`commitFiles`** (they need `structuredDiff.ts` → Phase 6) and **no FCM push send** (Phase 6) — both are wired as injectable hooks/no-ops so Phase 5/6 can fill them without touching the engine.

**Architecture:** One new module `server-rs/src/engine.rs` plus two tiny helpers it needs:
- `engine.rs` — `Engine`, `EngineConfig`, `QueueItem`, `Subscriber` type, the watchdog/queue/pump/recover/attach/start logic. Mirror of `engine.ts`. **The async/ownership model is the one real design decision** (see below). Reuses `crate::store::{Store, Session, CreateInput, SessionPatch}` (Phase 1), `crate::spawner::{spawn_claude, SpawnHandle, SpawnOptions, compose_user_text, encode_user_message}` (Phase 3), `crate::runner::{Runner, LocalRunner}` (Phase 3), `crate::worktree::{create_session_worktrees, discard_worktree, sync_worktree}` (Phase 3), `crate::claude_config::build_session_config_dir` (Phase 3), `crate::stream::{ClaudeEvent, parse_line}` (Phase 2), `crate::transcript::TranscriptCache` (Phase 1, for delete-time drop).
- `classify_error.rs` — `classify_claude_error(text) -> &'static str` (`"usage_limit"`/`"rate_limited"`/`"claude_error"`). Mirror of `classifyError.ts`. Tiny; its own module so it is independently testable (matches the TS file split).
- `repos.rs` — `ensure_local(repo, src_root, git_org, clone_fn)`. Mirror of `repos.ts` `ensureLocal` only (the `listRepos`/`listRemoteRepos` HTTP helpers are Phase 5/6). Needed by `submitSession`.

**The async/ownership model (the load-bearing port decision).** TS `engine.ts` runs on a single-threaded event loop: every field is a plain `Map`/array mutated synchronously, and the `SpawnHandle` is an `EventEmitter` whose `'event'`/`'exit'` listeners run on that same loop. In Rust + tokio (multi-threaded, `Send` futures) we reproduce this with **one `Mutex`-guarded inner-state struct** + **one detached pump task per live turn**:
- `Engine` is `Arc<EngineInner>`. `EngineInner` holds `cfg: EngineConfig`, `store: Arc<Store>`, an injectable `runner: Arc<dyn Runner>`, `transcript: Option<Arc<TranscriptCache>>` (for delete-time drop; `None` in unit tests), and **`state: Mutex<EngineState>`**. `EngineState` is the bag of `HashMap`s/`Vec`s that were instance fields in TS (`subs`, `running`, `queue`, `activity`, `awaiting`, `pending_ask`, `starting`, `last_event_at`, `turn_started_at`, `closed`). The `Mutex` is held only for short synchronous critical sections (read/patch the maps) — **never across an `.await`** (parity with the TS single-loop atomicity, and avoids deadlock).
- `running: HashMap<String, RunningTurn>` where `RunningTurn { handle: Arc<SpawnHandle-kill-surface>, pump: JoinHandle<()> }`. Phase 3's `SpawnHandle` owns the event/exit **receivers** (not `Clone`), so the engine **moves** the handle into a per-turn `tokio::spawn`ed **pump task** that loops `tokio::select!{ ev = events.recv() => on_event(ev), code = &mut exit => on_exit(code) }` and calls back into the engine (`Arc<EngineInner>`) for each event/exit — exactly the TS `handle.on("event", …)` / `handle.on("exit", …)`. The kill/interrupt/detach surface (`run.stop()`, `interrupt()`, the saw-result reset on `write`) is shared via `Arc` so the engine can kill from `kill()`/watchdog/`deleteSession` while the pump task owns the receivers. **Pre-extract** the `Arc<dyn RunHandle>` (already `pub(crate) run` on `SpawnHandle`) + a `write`/`interrupt` closure before moving the handle.
- Subscribers: a `Subscriber` is a `Box<dyn Fn(&ClaudeEvent) + Send + Sync>` (TS `(e) => void`). `emit(id, ev)` locks state, calls each fn for `id`. (Phase 5's WS handler registers a subscriber that forwards to a `tokio::sync::mpsc` sender; the unit tests register a closure pushing to a shared `Arc<Mutex<Vec<…>>>`.) Same `subscribe → unsubscribe-fn` contract.
- `deferPump` (TS `setImmediate`): `tokio::spawn(async move { inner.pump() })` — a later tick, so `submitSession`/`followUp` return the id before `start()` runs.
- `start()` is `async` (it `.await`s `sync_worktree`); `pump()` reserves the slot (`starting.insert`), then `tokio::spawn`s `start()` with the same catch/finally as TS (`.catch` → fail the row + `forget_session`; `.finally` → `starting.remove` + re-`pump`).
- The watchdog is a `tokio::spawn`ed loop on a `tokio::time::interval(WATCHDOG_TICK_MS)`; `trigger_watchdog()` calls one `tick_watchdog()` synchronously for tests. `now_fn`/clock is injectable (parity with `cfg.nowFn`).

**Tech Stack:** Rust (edition 2021). `tokio` (already a dep — `sync`, `time`, `task`, `process` under `full`). `serde_json` (already). **No new crate dependencies.** Reuses Phase 1–3 modules above.

**Parity bar:** Behavioral parity with the TS reference is the bar. Every case in `engine.test.ts`, `engine.recover.test.ts`, and `engine.streaming.test.ts` has a mirrored Rust test asserting the same status/error/errorKind/cost/log/awaitingInput/queue behavior, driven by the **same** `server/test/fixtures/fake-claude*.sh` scripts (absolute path via `CARGO_MANIFEST_DIR`, never the real `claude`). `classifyError.test.ts` is mirrored in `classify_error.rs`.

## Global Constraints

Copy these exact parity values/field names verbatim into the implementation; do not paraphrase.

- **Crate:** `agentic-dev/server-rs/`. Run all `cargo` commands from there. **Commit only — NEVER push.** End every commit message with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Modules:** new files `server-rs/src/engine.rs`, `server-rs/src/classify_error.rs`, `server-rs/src/repos.rs`; register each `mod …;` in `src/main.rs` (alphabetical → the `mod` block becomes: `api, auth, claude_config, classify_error, config, engine, repos, runner, spawner, state, store, stream, tailer, throttle, transcript, worktree`). Do **NOT** re-implement `is_rendered`/`filter_rendered` (they live in `transcript.rs`), `parse_line`/`ClaudeEvent` (`stream.rs`), or anything in `runner.rs`/`spawner.rs`/`worktree.rs`/`claude_config.rs`/`store.rs` — reuse them.
- **Timing constants (verbatim — `engine.ts` lines 34-36):** `WATCHDOG_TICK_MS = 30_000`; `DEFAULT_IDLE_MAX_MS = 20 * 60 * 1000` (= 1_200_000, "20 min"); `DEFAULT_WALL_MAX_MS = 2 * 60 * 60 * 1000` (= 7_200_000, "2 h"). As `const … : u64`.
- **`ErrorKind` string values (verbatim — `types.ts` lines 9-16, stored as the DB `errorKind` text):** `"usage_limit"`, `"rate_limited"`, `"claude_error"`, `"wall_timeout"`, `"idle_timeout"`, `"crashed"`, `"interrupted"`. Use these exact strings (the `Store` already stores `error_kind: Option<String>`).
- **`SessionStatus` string values (verbatim — `types.ts` line 3):** `"pending"`, `"running"`, `"done"`, `"failed"`, `"killed"`. Exact strings.
- **`classify_claude_error(text)` (verbatim — `classifyError.ts`):** check **rate-limited FIRST** (it shares the word "limit" with usage but means the opposite). Two case-insensitive regexes, exact source:
  - `RATE_LIMITED_RE = (?i)temporarily limiting|not (?:your|a) usage limit|overloaded|rate[ _-]?limit|\b429\b|\b503\b` → `"rate_limited"`.
  - `USAGE_LIMIT_RE = (?i)usage limit|session limit|\b(?:daily|weekly|monthly|\d+-?hour)\b[^.]*limit|resets\b|quota` → `"usage_limit"`.
  - else → `"claude_error"`. Order matters: rate-limited wins. Use the `regex` crate (already a dep); compile once with `OnceLock` (matches the `stream.rs` `meta_name` pattern).
- **Watchdog message strings (verbatim — `engine.ts` lines 116-118; the word "limit"/"resets" is FORBIDDEN here or the Android client mislabels a reap as a usage limit):**
  - idle: `format!("turn watchdog: idle for {}s (cap {}s)", (idle_ms/1000).round(), (idle_max/1000).round())`.
  - wall: `format!("turn watchdog: wall time {}s exceeded the {}s cap", (wall_ms/1000).round(), (wall_max/1000).round())`.
  - Rounding: TS `Math.round(ms/1000)`. In Rust: `((ms as f64)/1000.0).round() as i64`.
- **Generic crash fallback (verbatim — `engine.ts` lines 591-592):** on a `"failed"` exit with no captured `error`, set `error = "turn ended without completing — interrupted, crashed, or killed (resume to retry)"`, `errorKind = "crashed"`. **No** "limit"/"resets" words.
- **Interrupted-by-restart text (verbatim — `engine.ts` line 140):** `error = "interrupted by server restart"`, `errorKind = "interrupted"`.
- **`recover()` rules (verbatim — `engine.ts` lines 134-162):** for each session in `store.list()`:
  - `status == "running"`: read `last_result(id)` (the **last** `result` event in the log, parsed via `parse_line`). If `None` → `update {status:"failed", error:"interrupted by server restart", errorKind:"interrupted", endedAt: now}`. Else if `is_error` → `update {status:"failed", error: text.unwrap_or("turn ended with an error")[..min(500)], errorKind: text? classify_claude_error(text) : "claude_error", endedAt: now}`. Else → `update {status:"done", endedAt: now}`.
  - `status == "pending"`: re-enqueue `QueueItem { id, prompt: s.prompt, resume_session_id: s.claude_session_id, enqueued_at: None }` (carry `claudeSessionId` so a recovered follow-up `--resume`s; an initial submit has none → fresh).
  - then `pump()`.
  - `last_result(id)`: `store.read_log(id)` (Vec<String> of lines), iterate **back-to-front**, `parse_line(line)` and take the first event with `kind == result`; return `{ is_error, text }` or `None`.
- **`reconcile_worktrees()` (verbatim — `engine.ts` lines 168-177):** list directories directly under `cfg.worktrees_root` (best-effort; missing dir → return). For each dir name: if `store.get(name)` is `Some` → keep; else `remove_dir_all` it (best-effort, never throw). Runs once in the constructor after `recover()`.
- **`submitSession(repos, skills, prompt, env, meta)` (verbatim — `engine.ts` lines 193-211):** `repoSpecs = repos.map(|r| (r, ensure_local(r, cfg.src_root, cfg.git_org, cfg.clone_fn)))` (throws if a repo can't be resolved → in tests `clone_fn` throws for an absent repo). `id = uuid v4`. `wts = create_session_worktrees(&repoSpecs, &cfg.worktrees_root, &id)`. `baseShas = {w.repo: Some(w.base_sha) for w in wts}`. `sessionDir = cfg.worktrees_root / id`. If `wts.len() > 1` → `write_session_guide(sessionDir, wts, skills)` (**out of scope** — see note; for now skip the guide write and leave a `// Phase 5: writeSessionGuide` TODO, OR port a trivial version; the engine tests never assert on its contents). `store.create(CreateInput { id, repos, skills, prompt, worktree_path: Some(sessionDir), branch: format!("agentic/{id}"), base_shas, model: meta.model, effort: meta.effort, mode: meta.mode, ..})`. `queue.push(QueueItem{ id, prompt, env, enqueued_at: Some(now) })`. `defer_pump()`. Return `id`.
- **`submit(repo, prompt, env)` (verbatim — `engine.ts` line 214):** `submitSession(vec![repo], vec![], prompt, env, None)`.
- **`followUp(id, prompt, set_title=true) -> i64` (verbatim — `engine.ts` lines 220-263):**
  - `s = store.get(id)?` else `Err("unknown session: <id>")`.
  - **Live branch** (`running.contains(id)`): `now`; `since = store.read_log(id).len()`; `store.update(id, { prompt if set_title; error:null; error_kind:null; exit_code:null; last_user_message_at: now })` (one patch); `store.append_log(id, {"type":"agentic_prompt","text":prompt,"at":now})`; `emit(id, Prompt{ text:prompt, at:now, raw:{"type":"agentic_prompt","text":prompt} })`; `activity.turns += 1`; `awaiting.set(id,false)`; `pending_ask.remove(id)`; `last_event_at.set(id,now)`; `turn_started_at.set(id,now)`; `running[id].write(encode_user_message(&compose_user_text(prompt)))`; return `since`.
  - else if `is_busy(id, s.status)` → `Err("session busy")`.
  - **Queued branch:** `now`; `store.update(id, { prompt if set_title; status:"pending"; last_user_message_at: now })`; `since = store.read_log(id).len()`; `queue.push(QueueItem{ id, prompt, resume_session_id: s.claude_session_id, enqueued_at: Some(now) })`; `defer_pump()`; return `since`.
- **`is_busy(id, status)` (verbatim — `engine.ts` lines 295-298):** `running.contains(id) || starting.contains(id) || queue.iter().any(|q| q.id==id) || status=="pending" || status=="running"`.
- **`kill(id)` (verbatim — `engine.ts` lines 427-438):** if `running` has it → `store.update {status:"killed"}` then `handle.stop()`. Else drop it from `queue`; if `store.get(id)?.status == "pending"` → `store.update {status:"killed", endedAt: now}`.
- **`interrupt(id)` (verbatim — `engine.ts` lines 445-451):** `pending_ask.remove(id)`; `running.get(id).map(|h| h.interrupt())` (no-op if absent). Must NOT change status (a later `result` flips `awaiting`).
- **`discard(id)` (verbatim — `engine.ts` lines 336-346):** `s = live_session(id)` (busy → `Err("session busy")`; `worktree_state != "live"` → `Err("worktree already cleaned")`). For each repo: `discard_worktree(cfg.src_root/repo, s.worktree_path/repo, &s.branch)`. `remove_dir_all(s.worktree_path)`. `store.update {worktree_state:"discarded"}`.
- **`deleteSession(id, force) -> async` (verbatim — `engine.ts` lines 352-381):** `s = store.get(id)?` else return Ok. `busy = is_busy(id,s.status)`. If busy and `!force` → `Err("session busy")`. If busy and force: drop from `queue`; if `running` → `store.update{status:"killed"}`, `handle.stop()`, `wait_for_exit(id)`; else `store.update{status:"killed", endedAt:now}`. Re-read `cur`; if `worktree_state=="live"`: for each repo `discard_worktree(...)`, `remove_dir_all(worktree_path/.claude-config)`, `remove_dir_all(worktree_path)`. `store.remove(id)`; `forget_session(id)`; `subs.remove(id)`; `transcript.drop_session(id)` (the Rust addition — drop the cached projection on delete, matching the spec "dropped on session delete"). NOTE: `Store::remove` does not yet exist (Phase 1 only added `create/get/list/update/append_log/log_path`) — **add `Store::remove(&self, id) -> Result<(), StoreError>`** (`DELETE FROM sessions WHERE id=?` + best-effort delete the log file) and a `Store::read_log(&self, id) -> Vec<String>` (read the jsonl, split into non-empty-trimmed lines, `[]` on missing) in this phase, since `recover()`/`followUp`/`deleteSession` need them. Mirror `store.ts` `remove`/`readLog`.
- **`withActivity(s)` (verbatim — `engine.ts` lines 272-287):** start from `s`; if `activity[id]` set → attach `activity`. `awaiting = awaiting.get(id)` (`Option<bool>`); if `Some` → set `s.awaiting_input = Some(v)`. `finishedOrIdle = status in {done,failed,killed} || awaiting==Some(true)`. If `finishedOrIdle && worktree_path.is_some() && has_active_workflow(worktree_path/.claude-config)` → `s.workflow_running = Some(true)`. `list()`/`get()` map through `withActivity`. (Requires adding `activity: Option<Activity>`, `awaiting_input: Option<bool>`, `workflow_running: Option<bool>` to the `Session` serde struct — see Task 1.)
- **`has_active_workflow(base) -> bool` (verbatim — `workflows.ts` lines 24-52, the SUBSET the engine needs):** `WORKFLOW_TERMINAL = {"done","complete","completed","failed","error","killed","cancelled","canceled"}`. Return true iff **any** workflow run under `base` is not terminal. The engine only needs the boolean, and the only run-status the live path produces is `"running"` (never terminal). Port just enough: scan `base/projects/<slug>/<sid>/` dirs; a session has an active workflow iff it has a `subagents/workflows/<runId>/` dir containing **at least one** `agent-*.meta.json` file (the live "running" signal) **and** no completed summary that marks it terminal. **Minimal sufficient port for parity with the engine test** (`engine.test.ts` "reports workflowRunning=true …" seeds exactly `…/subagents/workflows/wf_live/agent-a1.meta.json` and asserts `true`; the "no workflow yet" case asserts falsy): treat the presence of any `subagents/workflows/<dir>/agent-*.meta.json` with no terminal `workflows/<runId>.json` summary as active. Full `listWorkflows` (phases/agents/previews for the HTTP endpoint) is **Phase 6** — only `has_active_workflow` lands here.
- **Watchdog tick (verbatim — `engine.ts` lines 86-128):** `if closed return`. `now = now_fn()`. `idle_max = cfg.idle_max_ms ?? DEFAULT_IDLE_MAX_MS`; `wall_max = cfg.wall_max_ms ?? DEFAULT_WALL_MAX_MS`. For each `(id, handle)` in `running`:
  - **idle-TTL reap (opt-in):** if `cfg.idle_ttl_ms.is_some() && awaiting.get(id)==Some(true) && !pending_ask.contains(id) && (now - last_event_at) > idle_ttl_ms`: `cur = store.get(id)`; `store.update {status: if cur.error.is_some() {"failed"} else {"done"}, endedAt: now}`; `handle.stop()`; `continue`.
  - `parked = awaiting.get(id)==Some(true) || pending_ask.contains(id)`. `idle_ms = if parked {0} else {now - last_event_at_or_now}`. `wall_ms = if parked {0} else {now - turn_started_at_or_now}`.
  - if `idle_ms > idle_max || wall_ms > wall_max`: `reason = if idle_ms>idle_max {idle msg} else {wall msg}`; `store.update {status:"failed", error: reason, endedAt: now, errorKind: if idle_ms>idle_max {"idle_timeout"} else {"wall_timeout"}}`; `handle.stop()`. (Mark BEFORE `stop()` so the exit handler honors "failed", not derives "killed".)
  - `last_event_at`/`turn_started_at` default to `now` when unset.
- **`active_count()` (verbatim — `engine.ts` lines 467-471):** `starting.len()` + count of `running` ids where `awaiting.get(id) != Some(true)` (idle/parked sessions hold their process but do NOT occupy a concurrency slot).
- **`pump()` (verbatim — `engine.ts` lines 473-497):** `while active_count() < max_concurrent && !queue.is_empty()`: `item = queue.remove(0)`; `starting.insert(item.id)`; `tokio::spawn(start(item))` with: on `Err` (and `!closed`) → `store.update {status:"failed", error: format!("failed to start: {msg}"), errorKind:"crashed", endedAt: now}` + `forget_session(item.id)`; **always** (finally) → `starting.remove(item.id)`, and if `!closed` → `pump()` again. `max_concurrent`: TS `Infinity` ⇒ Rust `cfg.max_concurrent: Option<u64>` where `None` = unlimited (compare with `active_count() < max.unwrap_or(u64::MAX)`).
- **`start(item)` (verbatim — `engine.ts` lines 626-673):** `s = store.get(item.id)?` else return Ok (deleted). If `s.worktree_path.is_none()` → `Err("not startable: session has no worktree path")`. For each `repo` in `s.repos`: `(cfg.sync_fn ?? sync_worktree)(s.worktree_path/repo).await`. If `closed` return Ok. Re-read `cur = store.get(item.id)`; if `cur.is_none() || cur.status=="killed"` → return Ok (honor a kill issued during the sync window — the turn must NOT spawn). `build_session_config_dir(cfg.claude_config_base, &s.skills, s.worktree_path/.claude-config)`. `now`. `store.update {status:"running", startedAt: now, error:null, errorKind:null, exitCode:null}`. log `turn_start { sessionId, queueWaitMs: item.enqueued_at.map(|e| max(0, now-e)), active: active_count(), max }`. `last_event_at.set(now)`; `turn_started_at.set(now)`; `activity.turns += 1`. `store.append_log(item.id, {"type":"agentic_prompt","text":item.prompt,"at": now_ms()})`. `handle = spawn_claude(spawn_opts(s, prompt, resume, env), &*runner)`. `attach(item.id, handle)`. `awaiting.set(item.id,false)`. `handle.write(encode_user_message(&compose_user_text(&item.prompt)))`. (Order: append the marker, spawn — the marker pre-spawn so the tailer backfills it; Phase 3 `spawn_claude` snapshots the offset before start.)
- **`spawn_opts(s, prompt, extra)` (verbatim — `engine.ts` lines 510-523):** `cwd = if s.repos.len()==1 { s.worktree_path/s.repos[0] } else { s.worktree_path }`. `claude_config_dir = s.worktree_path/.claude-config`. `SpawnOptions { bin: cfg.claude_bin, cwd, prompt, env: extra.env, resume_session_id: extra.resume, claude_config_dir: Some(cfg_dir), model: s.model, effort: s.effort, mode: s.mode, log_path: store.log_path(s.id), unit: format!("agentic-{}", s.id), memory_max: cfg.memory_max, memory_high: cfg.memory_high, cpu_quota: cfg.cpu_quota, tasks_max: cfg.tasks_max }`.
- **`attach(id, handle)` event handling (verbatim — `engine.ts` lines 526-563):** on each `ClaudeEvent` (if `closed` return): `last_event_at.set(id, now)`. Then by kind:
  - `Init { session_id }` → `store.update {claudeSessionId: Some(session_id)}`; `awaiting.set(id, false)`.
  - `Skill { names }` with `!names.is_empty()` → `activity.last_skill = names.last()`.
  - `Ask {..}` → `pending_ask.insert(id)`.
  - `Result { is_error, cost_usd, text, raw }` → log `turn_result { sessionId, ttftMs: raw.ttft_ms, durationMs: raw.duration_ms, isError: is_error, costUsd: cost_usd }`; `pending_ask.remove(id)`; build patch `{ costUsd: (cur.cost_usd ?? 0.0) + (cost_usd ?? 0.0) }`; if `is_error && text.is_some()` → `patch.error = text[..min(500)]`, `patch.errorKind = classify_claude_error(text)`; `store.update(patch)`; `awaiting.set(id, true)`; `pump()`.
  - **always** `emit(id, ev)`.
- **`attach(id, handle)` exit handling (verbatim — `engine.ts` lines 564-623):** on exit `code: i32` (if `closed` return): `running.remove(id)`; clear `last_event_at`/`turn_started_at`/`awaiting`/`pending_ask` for `id`. `cur = store.get(id)`. `status = match cur.status { "killed"=>"killed", "failed"=>"failed", "done"=>"done", _ => if code==0 && cur.error.is_none() {"done"} else {"failed"} }`. patch `{ status, exitCode: Some(code), endedAt: now }`. If `status=="failed" && cur.error.is_none()` → `patch.error = "turn ended without completing — interrupted, crashed, or killed (resume to retry)"`, `patch.errorKind = "crashed"`. `store.update(patch)`. `errorKind = patch.errorKind.or(cur.errorKind)`. `emit(id, Other { raw: {"engineExit":{"code":code,"status":status,"errorKind":errorKind}} })`. log `turn_end { sessionId, status, errorKind, exitCode: code, durationMs: cur.startedAt.map(|st| endedAt - st) }`. Then push hook (Phase 6): if final session and `status in {done,failed,killed}` → call `cfg.push_fn` if set (no-op default; the FCM body shape is Phase 6). `pump()`. **Exit `code`:** Phase 3's `SpawnHandle.exit` yields `0` iff a clean `result` was seen, else `1` — i.e. the engine treats `0`/`1` exactly like the TS `exitCode` (which is `0`/non-zero from the OS). `Some(code)` here is `i64` for the `Session.exit_code` field.
- **`close(kill_running=true)` (verbatim — `engine.ts` lines 456-462):** `closed = true`; stop the watchdog task (`abort` its `JoinHandle`); for each running handle → `kill_running ? stop() : detach()`; `store.close()` (sqlx pool: `pool.close().await` — but `close()` is sync in TS; expose `Store::close(&self)` that drops/closes the pool, or skip since the process is exiting in prod and tests just drop the Engine — **make `close()` set `closed` + abort the watchdog + stop/detach handles; the store closes when the `Arc<Store>` drops**). Provide both `close()` (default kill) and a `close_graceful()` (detach) if a test needs it; the recover/streaming tests only call `close()` (kill).
- **`forget_session(id)` (verbatim — `engine.ts` lines 411-418):** remove `id` from `activity`, `last_event_at`, `turn_started_at`, `awaiting`, `pending_ask`, `starting`. Idempotent. (Does NOT touch `subs` — only `deleteSession` clears `subs`.)
- **`subscribe(id, fn) -> unsubscribe-fn` (verbatim — `engine.ts` lines 398-406):** add `fn` to `subs[id]`; the returned closure removes it and, if the set is now empty, removes the `subs[id]` entry (no Map leak per WS connect). In Rust: subscribers keyed by a per-session incrementing `u64` token; the unsubscribe-fn is `Box<dyn FnOnce() + Send>` (or return the token + an `unsubscribe(id, token)` method — pick the closure form to match the TS contract: `let unsub = engine.subscribe(id, f); unsub();`).
- **Injectable test seams on `EngineConfig` (verbatim — `types.ts` `EngineConfig`):** `clone_fn: Option<Arc<dyn Fn(&str,&str) + Send + Sync>>` (override repo clone; tests make it **error** for an absent repo via a clone that always fails — see `submit("nope")` test), `sync_fn: Option<Arc<dyn Fn(PathBuf) -> Pin<Box<dyn Future<Output=()> + Send>> + Send + Sync>>` (override worktree sync; the kill-during-sync test injects a gated future), `runner: Option<Arc<dyn Runner>>` (default `LocalRunner`), `log_fn: Option<Arc<dyn Fn(serde_json::Value) + Send + Sync>>` (capture lifecycle records), `now_fn: Option<Arc<dyn Fn() -> i64 + Send + Sync>>` (injectable clock), `idle_max_ms`/`wall_max_ms`/`idle_ttl_ms: Option<i64>`, `memory_max`/`memory_high`/`cpu_quota`/`tasks_max: Option<String>`, plus the non-optional `src_root`/`worktrees_root`/`log_dir`/`db_path`/`claude_bin`/`max_concurrent: Option<u64>`/`git_org`/`claude_config_base: PathBuf`.
- **UUID:** TS uses `crypto.randomUUID()`. No `uuid` crate is a dep. **Generate a v4-shaped id without a new crate**: read 16 bytes from `/dev/urandom` (or `std::time` + a process counter + `std::collections::hash_map::RandomState` hash as entropy) and format `xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`. A helper `fn new_session_id() -> String`. The engine tests never assert the id format (only uniqueness), so any unique string works — but produce a v4-shaped one for on-disk parity. **Do not add the `uuid` crate** (keep "no new deps").
- **Fixtures (verbatim paths):** `server/test/fixtures/fake-claude.sh` (one-shot: init `fake-sess-123`, two text deltas `Hello `+`world`, `result is_error=false total_cost_usd=0.0042`; honors `FAKE_CLAUDE_SLEEP`), `fake-claude-error.sh` (error result "…session limit…"), `fake-claude-crash.sh` (non-zero exit, no result), `fake-claude-ratelimit.sh` ("not your usage limit"), `fake-claude-noinit.sh` (no init event → no `claudeSessionId`), `fake-claude-stream.sh` (persistent: reads NDJSON stdin, one turn per line `init→delta→result`, session `fake-stream-1`, cost `0.001`), `fake-claude-stream-crash2.sh` (turn 1 OK, turn 2 crashes). Resolve via `env!("CARGO_MANIFEST_DIR")` + `../server/test/fixtures/<name>`. **Never** the real `claude`.
- **Out of scope (later phases):** `commitGraph`/`commitFiles` (need `structuredDiff.ts` → Phase 6); the real FCM push send body (Phase 6 — wire a no-op `push_fn` hook here); `writeSessionGuide`'s file content (Phase 5 — skip or stub; no test asserts it); the SDK runner's in-turn AskUserQuestion responder (`sdkRunner.ts` — Rust prod uses the raw-CLI `LocalRunner` + interrupt control_request already in Phase 3; `pendingAsk` is still tracked here because the watchdog exemption depends on it); full `listWorkflows` HTTP payload (Phase 6 — only `has_active_workflow` here); all axum routes/WS (Phase 5). All `cargo test` green before each commit.

## File Structure

- `server-rs/src/classify_error.rs` — NEW: `classify_claude_error`, two `OnceLock<Regex>`, inline tests.
- `server-rs/src/repos.rs` — NEW: `ensure_local`, inline tests.
- `server-rs/src/engine.rs` — NEW: `Engine`, `EngineInner`, `EngineState`, `EngineConfig`, `QueueItem`, `RunningTurn`, `Activity`, `Subscriber`, `new_session_id`, `has_active_workflow`, all the methods. Inline `#[cfg(test)] mod tests` (the `engine.test.ts` + `engine.streaming.test.ts` + `engine.recover.test.ts` mirrors).
- `server-rs/src/store.rs` — MODIFY: add `Session.activity/awaiting_input/workflow_running` runtime fields (serde, `skip_serializing_if = "Option::is_none"`); add `Store::remove`, `Store::read_log`; add the missing `SessionPatch` fields the engine writes (`worktree_state`, `claude_session_id` already present). (Do NOT change existing column logic.)
- `server-rs/src/state.rs` — MODIFY (Task 9 only): add `engine: Arc<Engine>` to `AppState` so Phase 5 can route to it. (Construction in `main.rs`.)
- `server-rs/src/main.rs` — MODIFY: add the three `mod …;` lines; construct the `Engine` and put it in `AppState`.
- `server-rs/Cargo.toml` — **unchanged** (no new deps).

---

### Task 1: `store.rs` — runtime `Session` fields + `Store::remove`/`read_log` + missing patch fields

The engine surfaces runtime-only fields (`activity`, `awaitingInput`, `workflowRunning`) on `Session` and needs `remove`/`read_log`/`worktree_state` updates that Phase 1 didn't add. Land these first so `engine.rs` compiles.

**Files:**
- Modify: `server-rs/src/store.rs`
- Test: extend `server-rs/src/store.rs` `#[cfg(test)] mod tests`

**Interfaces (exact Rust signatures):**
```rust
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Activity {
    pub turns: u64,
    #[serde(rename = "lastSkill", skip_serializing_if = "Option::is_none")]
    pub last_skill: Option<String>,
}

// added to `pub struct Session`:
//   #[serde(skip_serializing_if = "Option::is_none")] pub activity: Option<Activity>,
//   #[serde(rename = "awaitingInput", skip_serializing_if = "Option::is_none")] pub awaiting_input: Option<bool>,
//   #[serde(rename = "workflowRunning", skip_serializing_if = "Option::is_none")] pub workflow_running: Option<bool>,

// added to `pub struct SessionPatch`:
//   pub worktree_state: Option<String>,

impl Store {
    /// Delete the row + best-effort remove the log file. Idempotent. Mirror `store.ts` remove().
    pub async fn remove(&self, id: &str) -> Result<(), StoreError>;
    /// Read the session log file → Vec of non-empty lines ([] if missing). Mirror `store.ts` readLog().
    pub fn read_log(&self, id: &str) -> Vec<String>;
}
```

**Steps:**
- [ ] **Write failing tests.** Append to `store.rs` tests:
  ```rust
  #[tokio::test]
  async fn remove_deletes_row_and_log() {
      let dir = tmp();
      let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
      store.create(CreateInput { id: "rm1".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
      store.append_log("rm1", "{\"type\":\"x\"}").await.unwrap();
      assert!(store.get("rm1").await.unwrap().is_some());
      store.remove("rm1").await.unwrap();
      assert!(store.get("rm1").await.unwrap().is_none(), "row deleted");
      assert!(store.read_log("rm1").is_empty(), "log read returns [] after remove");
      // idempotent
      store.remove("rm1").await.unwrap();
  }

  #[tokio::test]
  async fn read_log_returns_nonempty_lines_in_order() {
      let dir = tmp();
      let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
      assert_eq!(store.read_log("missing"), Vec::<String>::new(), "missing log → []");
      store.append_log("L", "{\"type\":\"a\"}").await.unwrap();
      store.append_log("L", "{\"type\":\"b\"}").await.unwrap();
      assert_eq!(store.read_log("L"), vec!["{\"type\":\"a\"}".to_string(), "{\"type\":\"b\"}".to_string()]);
  }

  #[tokio::test]
  async fn update_can_set_worktree_state() {
      let dir = tmp();
      let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
      store.create(CreateInput { id: "ws".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
      store.update("ws", SessionPatch { worktree_state: Some("discarded".into()), ..Default::default() }).await.unwrap();
      assert_eq!(store.get("ws").await.unwrap().unwrap().worktree_state, "discarded");
  }

  #[test]
  fn session_runtime_fields_skip_when_none() {
      let s = Session { id: "x".into(), status: "done".into(), worktree_state: "live".into(), ..Default::default() };
      let v = serde_json::to_value(&s).unwrap();
      assert!(v.get("activity").is_none() && v.get("awaitingInput").is_none() && v.get("workflowRunning").is_none(),
          "runtime fields omitted when None (parity with TS optional props)");
  }
  ```
- [ ] **Run** `cargo test --manifest-path server-rs/Cargo.toml store::` — expect compile errors / failures (`remove`/`read_log`/`worktree_state` missing).
- [ ] **Implement.** Add the three runtime `Session` fields + `Activity` struct (above). Add `worktree_state: Option<String>` to `SessionPatch` and a `col!(patch.worktree_state, "worktreeState = ?")` line + bind (in the same order block). Add:
  ```rust
  pub async fn remove(&self, id: &str) -> Result<(), StoreError> {
      sqlx::query("DELETE FROM sessions WHERE id = ?").bind(id).execute(&self.pool).await?;
      let _ = std::fs::remove_file(self.log_path(id)); // best-effort
      Ok(())
  }
  pub fn read_log(&self, id: &str) -> Vec<String> {
      match std::fs::read_to_string(self.log_path(id)) {
          Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect(),
          Err(_) => Vec::new(),
      }
  }
  ```
  (`read_log` is sync like TS `readLog`; engine callers run it inside short locked sections only where they already hold no `.await` — fine, it's a small file read; if a large-log concern arises Phase 5 can move it behind `spawn_blocking`.)
- [ ] **Run** `cargo test --manifest-path server-rs/Cargo.toml store::` — expect pass (all prior store tests + the 4 new).
- [ ] **Commit** (commit-only): `Phase 4 T1: store.rs — Session runtime fields + remove/read_log + worktreeState patch`.

---

### Task 2: `classify_error.rs` — `classify_claude_error` (mirror `classifyError.ts`)

**Files:**
- Create: `server-rs/src/classify_error.rs`
- Modify: `server-rs/src/main.rs` (add `mod classify_error;`)
- Test: inline `#[cfg(test)]`

**Interfaces (exact Rust signatures):**
```rust
/// Classify claude's result error text into a structured ErrorKind string.
/// Order-sensitive: rate-limited wins over usage-limit (they share the word "limit").
/// Returns one of "rate_limited" | "usage_limit" | "claude_error". Mirror of classifyError.ts.
pub fn classify_claude_error(text: &str) -> &'static str;
```

**Steps:**
- [ ] **Write failing tests** (mirror `classifyError.test.ts`'s intent + the engine error tests' strings):
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      #[test]
      fn usage_limit_classified() {
          assert_eq!(classify_claude_error("You've hit your session limit · resets 3:30pm (Pacific)"), "usage_limit");
          assert_eq!(classify_claude_error("usage limit reached"), "usage_limit");
          assert_eq!(classify_claude_error("your weekly limit is exhausted"), "usage_limit");
          assert_eq!(classify_claude_error("quota exceeded"), "usage_limit");
      }
      #[test]
      fn rate_limited_wins_over_usage_even_with_limit_word() {
          assert_eq!(classify_claude_error("Server is temporarily limiting requests (not your usage limit) Rate limited"), "rate_limited");
          assert_eq!(classify_claude_error("overloaded, try again"), "rate_limited");
          assert_eq!(classify_claude_error("HTTP 429 Too Many Requests"), "rate_limited");
          assert_eq!(classify_claude_error("503 Service Unavailable"), "rate_limited");
      }
      #[test]
      fn anything_else_is_generic_claude_error() {
          assert_eq!(classify_claude_error("some random tool failure"), "claude_error");
          assert_eq!(classify_claude_error(""), "claude_error");
      }
  }
  ```
- [ ] **Run** `cargo test --manifest-path server-rs/Cargo.toml classify_error::` — expect fail (module absent).
- [ ] **Implement:**
  ```rust
  use regex::Regex;
  use std::sync::OnceLock;

  fn rate_limited_re() -> &'static Regex {
      static RE: OnceLock<Regex> = OnceLock::new();
      RE.get_or_init(|| Regex::new(r"(?i)temporarily limiting|not (?:your|a) usage limit|overloaded|rate[ _-]?limit|\b429\b|\b503\b").expect("valid regex"))
  }
  fn usage_limit_re() -> &'static Regex {
      static RE: OnceLock<Regex> = OnceLock::new();
      RE.get_or_init(|| Regex::new(r"(?i)usage limit|session limit|\b(?:daily|weekly|monthly|\d+-?hour)\b[^.]*limit|resets\b|quota").expect("valid regex"))
  }
  pub fn classify_claude_error(text: &str) -> &'static str {
      if rate_limited_re().is_match(text) { return "rate_limited"; }
      if usage_limit_re().is_match(text) { return "usage_limit"; }
      "claude_error"
  }
  ```
  Add `mod classify_error;` to `main.rs` (alphabetical, after `claude_config`).
- [ ] **Run** `cargo test --manifest-path server-rs/Cargo.toml classify_error::` — expect pass.
- [ ] **Commit:** `Phase 4 T2: classify_error.rs — classify_claude_error (rate_limited wins over usage_limit)`.

---

### Task 3: `repos.rs` — `ensure_local` (mirror `repos.ts` ensureLocal)

**Files:**
- Create: `server-rs/src/repos.rs`
- Modify: `server-rs/src/main.rs` (add `mod repos;`)
- Test: inline `#[cfg(test)]`

**Interfaces (exact Rust signatures):**
```rust
use std::path::{Path, PathBuf};

/// Return the local path of `repo` under src_root, cloning from GitHub first if absent.
/// `clone_fn(url, dest)` is injectable (tests pass a closure that errors to simulate "no network").
/// Returns Err if the repo is absent AND the clone fails. Mirror of repos.ts ensureLocal.
pub fn ensure_local(
    repo: &str,
    src_root: &Path,
    git_org: &str,
    clone_fn: &dyn Fn(&str, &str) -> std::io::Result<()>,
) -> std::io::Result<PathBuf>;

/// Production clone via `git clone <url> <dest>`. Used when EngineConfig.clone_fn is None.
pub fn default_clone(url: &str, dest: &str) -> std::io::Result<()>;
```

**Steps:**
- [ ] **Write failing tests:**
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      fn tmp() -> PathBuf {
          let d = std::env::temp_dir().join(format!("agentic-repos-{}-{}", std::process::id(),
              std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
          std::fs::create_dir_all(&d).unwrap(); d
      }
      #[test]
      fn returns_existing_local_repo_without_cloning() {
          let src = tmp();
          std::fs::create_dir_all(src.join("demo").join(".git")).unwrap();
          let never = |_: &str, _: &str| -> std::io::Result<()> { panic!("must not clone") };
          let p = ensure_local("demo", &src, "arcatva", &never).unwrap();
          assert_eq!(p, src.join("demo"));
      }
      #[test]
      fn clones_when_absent_using_the_org_url() {
          let src = tmp();
          let seen = std::cell::RefCell::new(String::new());
          let clone = |url: &str, dest: &str| -> std::io::Result<()> {
              *seen.borrow_mut() = url.to_string();
              std::fs::create_dir_all(std::path::Path::new(dest).join(".git")).unwrap(); Ok(())
          };
          let p = ensure_local("newrepo", &src, "arcatva", &clone).unwrap();
          assert_eq!(p, src.join("newrepo"));
          assert_eq!(*seen.borrow(), "https://github.com/arcatva/newrepo.git");
      }
      #[test]
      fn propagates_clone_failure_for_an_unresolvable_repo() {
          let src = tmp();
          let boom = |_: &str, _: &str| -> std::io::Result<()> {
              Err(std::io::Error::new(std::io::ErrorKind::Other, "clone disabled in tests"))
          };
          assert!(ensure_local("nope", &src, "arcatva", &boom).is_err());
      }
  }
  ```
- [ ] **Run** — expect fail (module absent).
- [ ] **Implement:**
  ```rust
  pub fn default_clone(url: &str, dest: &str) -> std::io::Result<()> {
      let status = std::process::Command::new("git").args(["clone", url, dest]).status()?;
      if status.success() { Ok(()) } else { Err(std::io::Error::new(std::io::ErrorKind::Other, "git clone failed")) }
  }
  pub fn ensure_local(repo: &str, src_root: &Path, git_org: &str,
                      clone_fn: &dyn Fn(&str, &str) -> std::io::Result<()>) -> std::io::Result<PathBuf> {
      let dest = src_root.join(repo);
      if dest.join(".git").exists() { return Ok(dest); }
      let url = format!("https://github.com/{git_org}/{repo}.git");
      clone_fn(&url, &dest.to_string_lossy())?;
      Ok(dest)
  }
  ```
  Add `mod repos;` to `main.rs` (alphabetical, after `engine`).
- [ ] **Run** — expect pass.
- [ ] **Commit:** `Phase 4 T3: repos.rs — ensure_local (clone-if-absent, injectable clone_fn)`.

---

### Task 4: `engine.rs` skeleton — `EngineConfig`, `Engine`/`EngineInner`/`EngineState`, `new_session_id`, `has_active_workflow`, constructor (recover/reconcile/watchdog), `subscribe`/`emit`, `list`/`get`/`with_activity`/`get_log` (failing build)

Land the type skeleton + the read-only surface so the heavy lifecycle tasks (5–8) bolt onto a compiling shell. The constructor runs `recover()` + `reconcile_worktrees()` + starts the watchdog (their bodies arrive in Task 5/6, stubbed here to keep the build green).

**Files:**
- Create: `server-rs/src/engine.rs`
- Modify: `server-rs/src/main.rs` (add `mod engine;`)

**Interfaces (exact Rust signatures):**
```rust
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::future::Future;
use std::sync::{Arc, Mutex};
use crate::store::{Store, Session, Activity, CreateInput, SessionPatch};
use crate::runner::{Runner, LocalRunner};
use crate::spawner::SpawnHandle;
use crate::stream::ClaudeEvent;
use crate::transcript::TranscriptCache;

pub type Subscriber = Box<dyn Fn(&ClaudeEvent) + Send + Sync>;
pub type CloneFn = Arc<dyn Fn(&str, &str) -> std::io::Result<()> + Send + Sync>;
pub type SyncFn = Arc<dyn Fn(PathBuf) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
pub type LogFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type PushFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

pub struct EngineConfig {
    pub src_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub log_dir: PathBuf,
    pub db_path: PathBuf,
    pub claude_bin: String,
    pub max_concurrent: Option<u64>,        // None = unlimited (TS Infinity)
    pub git_org: String,
    pub claude_config_base: PathBuf,
    pub clone_fn: Option<CloneFn>,
    pub sync_fn: Option<SyncFn>,
    pub runner: Option<Arc<dyn Runner>>,
    pub log_fn: Option<LogFn>,
    pub now_fn: Option<NowFn>,
    pub push_fn: Option<PushFn>,            // Phase 6 fills the FCM body; default no-op
    pub idle_max_ms: Option<i64>,
    pub wall_max_ms: Option<i64>,
    pub idle_ttl_ms: Option<i64>,
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
}

pub struct QueueItem {
    pub id: String,
    pub prompt: String,
    pub env: HashMap<String, String>,
    pub resume_session_id: Option<String>,
    pub enqueued_at: Option<i64>,
}

pub struct RunningTurn {
    pub run: Arc<dyn crate::runner::RunHandle>, // kill/interrupt surface (Arc-shared)
    pub pump: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct SubEntry { next: u64, subs: HashMap<u64, Subscriber> }

#[derive(Default)]
pub struct EngineState {
    subs: HashMap<String, SubEntry>,
    running: HashMap<String, RunningTurn>,
    queue: Vec<QueueItem>,
    activity: HashMap<String, Activity>,
    awaiting: HashMap<String, bool>,
    pending_ask: HashSet<String>,
    starting: HashSet<String>,
    last_event_at: HashMap<String, i64>,
    turn_started_at: HashMap<String, i64>,
    closed: bool,
}

pub struct EngineInner {
    cfg: EngineConfig,
    store: Arc<Store>,
    runner: Arc<dyn Runner>,
    transcript: Option<Arc<TranscriptCache>>,
    state: Mutex<EngineState>,
    watchdog: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Engine(Arc<EngineInner>);

impl Engine {
    /// Construct: open the store, run recover() + reconcile_worktrees(), start the watchdog.
    pub async fn new(cfg: EngineConfig) -> Result<Engine, crate::store::StoreError>;
    /// Variant that reuses an already-open Store + an optional shared TranscriptCache (for AppState wiring).
    pub fn with_store(cfg: EngineConfig, store: Arc<Store>, transcript: Option<Arc<TranscriptCache>>) -> Engine;

    pub fn submit_session(&self, repos: Vec<String>, skills: Vec<String>, prompt: String,
                          env: HashMap<String, String>, meta: SubmitMeta) -> Result<String, String>;
    pub fn submit(&self, repo: &str, prompt: &str, env: HashMap<String, String>) -> Result<String, String>;
    pub fn follow_up(&self, id: &str, prompt: &str, set_title: bool) -> Result<i64, String>;
    pub async fn list(&self) -> Vec<Session>;
    pub async fn get(&self, id: &str) -> Option<Session>;
    pub fn get_log(&self, id: &str) -> Vec<String>;
    pub fn subscribe(&self, id: &str, f: Subscriber) -> Box<dyn FnOnce() + Send>;
    pub fn kill(&self, id: &str);
    pub fn interrupt(&self, id: &str);
    pub async fn discard(&self, id: &str) -> Result<(), String>;
    pub async fn delete_session(&self, id: &str, force: bool) -> Result<(), String>;
    pub fn trigger_watchdog(&self);
    pub fn close(&self);
}

#[derive(Default)]
pub struct SubmitMeta { pub model: Option<String>, pub effort: Option<String>, pub mode: Option<String> }
```

**Steps:**
- [ ] **Implement the skeleton** with the read-only surface fully working and the lifecycle methods as `todo!()`/minimal stubs that still compile (`submit_session`/`follow_up`/`kill`/`interrupt`/`discard`/`delete_session` may be `unimplemented` bodies here — Tasks 5–8 fill them). Implement now: `now()` (`cfg.now_fn` or `SystemTime`-ms), `log(rec)` (`cfg.log_fn` or no-op — wrap each record with `{"comp":"engine","t":now(), ...rec}`), `new_session_id()` (v4-shaped from `/dev/urandom` with a time+counter fallback), `has_active_workflow(base)`, `subscribe`/`emit`/`unsubscribe`, `with_activity(s)`, `list`/`get`/`get_log`, `forget_session`, `active_count`, the watchdog spawn + `trigger_watchdog`/`tick_watchdog` (Task 6 fills the body — stub returns now), `close`.
- [ ] Add `mod engine;` to `main.rs` (alphabetical, after `config`).
- [ ] **Run** `cargo build --manifest-path server-rs/Cargo.toml` — expect compile success (warnings for unused stubs OK). No tests yet.
- [ ] **Write a first failing behavioral test** for `has_active_workflow` (mirrors the workflow detection the `withActivity` test needs) and `subscribe` leak-drop:
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      fn tmp() -> PathBuf { /* same per-test unique dir helper as store.rs */ }

      #[test]
      fn has_active_workflow_detects_live_meta_and_ignores_empty() {
          let base = tmp();
          assert!(!has_active_workflow(&base), "no workflows dir → false");
          let rd = base.join("projects").join("-slug").join("sess").join("subagents").join("workflows").join("wf_live");
          std::fs::create_dir_all(&rd).unwrap();
          std::fs::write(rd.join("agent-a1.meta.json"), r#"{"agentType":"workflow-subagent"}"#).unwrap();
          assert!(has_active_workflow(&base), "a live agent meta file → active");
      }

      #[tokio::test]
      async fn subscribe_then_unsub_drops_the_entry() {
          let e = test_engine().await;     // helper builds an Engine over a tmp store, maxConcurrent=1
          let unsub = e.subscribe("ghost", Box::new(|_| {}));
          assert!(e.0.state.lock().unwrap().subs.contains_key("ghost"));
          unsub();
          assert!(!e.0.state.lock().unwrap().subs.contains_key("ghost"), "empty sub set must be dropped (no Map leak)");
      }
  }
  ```
  (Provide a `test_engine()` helper + `tmp()` in the test module; `test_engine` uses `Engine::new` with `claude_bin = fixture("fake-claude.sh")`, a tmp work dir, `max_concurrent: Some(1)`, and a `clone_fn` that errors.)
- [ ] **Run** `cargo test --manifest-path server-rs/Cargo.toml engine::tests::has_active_workflow engine::tests::subscribe` — expect pass once `has_active_workflow`/`subscribe` are implemented.
- [ ] **Commit:** `Phase 4 T4: engine.rs skeleton — types, constructor, subscribe/emit, with_activity, has_active_workflow`.

---

### Task 5: submit/start/attach/pump happy path + error classification + lifecycle logs (mirror `engine.test.ts` "Engine"/"Engine error handling")

Wire the whole forward path: `submit_session` → `defer_pump` → `pump` → `start` (sync worktree, build config dir, spawn, attach) → the attach event/exit pump task (cost/claudeSessionId/awaiting/result/exit) → `with_activity`. This is the bulk of the engine.

**Files:**
- Modify: `server-rs/src/engine.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:** (the `submit_session`/`submit`/`pump`/`start`/`attach`/`spawn_opts`/`emit` bodies from the Global Constraints; `attach` spawns the per-turn pump task.)

**Steps:**
- [ ] **Write failing tests** (mirror `engine.test.ts` lines 52-105 + 107-171 + the lifecycle-log test 676-697). Use a shared `waitForStatus`-equivalent and a `makeTempGitRepo`-equivalent (port `gitrepo.ts`: init a temp git repo with one commit + a `README.md`; put it as a helper in the test module or a `#[path]`-included test util). Representative cases:
  ```rust
  #[tokio::test]
  async fn runs_a_session_to_done_capturing_session_id_cost_and_logs() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");          // helper
      let e = make_engine(&src, EngineOverrides::default()).await;
      let id = e.submit("demo", "do something", HashMap::new()).unwrap();
      wait_status(&e, &id, "done").await;
      let s = e.get(&id).await.unwrap();
      assert_eq!(s.claude_session_id.as_deref(), Some("fake-sess-123"));
      assert_eq!(s.cost_usd, Some(0.0042));
      assert_eq!(s.exit_code, Some(0));
      assert!(!e.get_log(&id).is_empty());
  }

  #[tokio::test]
  async fn logs_an_agentic_prompt_marker() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let e = make_engine(&src, EngineOverrides::default()).await;
      let id = e.submit("demo", "my first prompt", HashMap::new()).unwrap();
      wait_status(&e, &id, "done").await;
      let prompts: Vec<serde_json::Value> = e.get_log(&id).iter()
          .filter_map(|l| serde_json::from_str(l).ok())
          .filter(|o: &serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("agentic_prompt"))
          .collect();
      assert_eq!(prompts[0]["text"], "my first prompt");
  }

  #[tokio::test]
  async fn subscribers_receive_live_events() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let e = make_engine(&src, EngineOverrides::default()).await;
      let got = Arc::new(Mutex::new(Vec::<String>::new()));
      let id = e.submit("demo", "p", HashMap::new()).unwrap();
      let g = got.clone();
      e.subscribe(&id, Box::new(move |ev| { if matches!(ev, ClaudeEvent::Text{..}) { g.lock().unwrap().push("text".into()); } }));
      wait_status(&e, &id, "done").await;
      assert!(!got.lock().unwrap().is_empty(), "saw at least one text event");
  }

  #[tokio::test]
  async fn respects_max_concurrent_second_stays_pending() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), ..Default::default() }).await;
      let mut slow = HashMap::new(); slow.insert("FAKE_CLAUDE_SLEEP".into(), "1".into());
      let id1 = e.submit("demo", "first", slow).unwrap();
      let id2 = e.submit("demo", "second", HashMap::new()).unwrap();
      // id2 may need a tick to be enqueued; assert it never reaches running before id1 is done.
      wait_status(&e, &id1, "running").await;
      assert_eq!(e.get(&id2).await.unwrap().status, "pending");
      wait_status(&e, &id2, "done").await;
      assert_eq!(e.get(&id1).await.unwrap().status, "done");
  }

  #[tokio::test]
  async fn submit_session_returns_before_starting_the_turn() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let e = make_engine(&src, EngineOverrides::default()).await;
      let id = e.submit_session(vec!["demo".into()], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      assert_eq!(e.get(&id).await.unwrap().status, "pending"); // deferred start
      wait_status(&e, &id, "done").await;
  }

  #[tokio::test]
  async fn pure_skill_session_no_repo_runs_done() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides::default()).await;
      let id = e.submit_session(vec![], vec![], "just answer".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      wait_status(&e, &id, "done").await;
      let s = e.get(&id).await.unwrap();
      assert!(s.repos.is_empty());
      assert_eq!(s.status, "done");
  }

  #[tokio::test]
  async fn rejects_submit_for_unknown_repo() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides::default()).await; // clone_fn errors
      assert!(e.submit("nope", "p", HashMap::new()).is_err());
  }

  #[tokio::test]
  async fn usage_limit_error_sets_failed_and_errorkind() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides { claude_bin: Some(fixture("fake-claude-error.sh")), ..Default::default() }).await;
      let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      wait_status(&e, &id, "failed").await;
      let s = e.get(&id).await.unwrap();
      assert!(s.error.unwrap_or_default().to_lowercase().contains("session limit"));
      assert_eq!(s.error_kind.as_deref(), Some("usage_limit"));
  }

  #[tokio::test]
  async fn crashed_turn_is_not_mislabeled_as_usage_limit() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides { claude_bin: Some(fixture("fake-claude-crash.sh")), ..Default::default() }).await;
      let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      wait_status(&e, &id, "failed").await;
      let s = e.get(&id).await.unwrap();
      let err = s.error.unwrap_or_default();
      assert!(!err.is_empty());
      assert!(!regex::Regex::new("(?i)limit|resets").unwrap().is_match(&err), "crash must not trip the limit heuristic");
      assert_eq!(s.error_kind.as_deref(), Some("crashed"));
  }

  #[tokio::test]
  async fn rate_limit_tagged_rate_limited() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides { claude_bin: Some(fixture("fake-claude-ratelimit.sh")), ..Default::default() }).await;
      let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      wait_status(&e, &id, "failed").await;
      let s = e.get(&id).await.unwrap();
      assert_eq!(s.error_kind.as_deref(), Some("rate_limited"));
      assert!(s.error.unwrap_or_default().to_lowercase().contains("not your usage limit"));
  }

  #[tokio::test]
  async fn emits_structured_turn_lifecycle_logs() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let logs = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
      let l = logs.clone();
      let e = make_engine(&src, EngineOverrides { log_fn: Some(Arc::new(move |r| l.lock().unwrap().push(r))), ..Default::default() }).await;
      let id = e.submit("demo", "go", HashMap::new()).unwrap();
      wait_status(&e, &id, "done").await;
      let recs = logs.lock().unwrap();
      let find = |evt: &str| recs.iter().find(|r| r["evt"] == evt && r["sessionId"] == id.as_str());
      let start = find("turn_start").expect("turn_start");
      assert!(start.get("queueWaitMs").is_some() && start.get("max").is_some());
      let result = find("turn_result").expect("turn_result");
      assert!(result.as_object().unwrap().contains_key("ttftMs"));
      let end = find("turn_end").expect("turn_end");
      assert_eq!(end["status"], "done");
  }
  ```
- [ ] **Run** — expect fail (stubs).
- [ ] **Implement** `submit_session`/`submit`/`defer_pump`/`pump`/`start`/`spawn_opts`/`attach` (event + exit pump task)/`emit` exactly per the Global Constraints. Key Rust mechanics:
  - `defer_pump`: `let inner = self.0.clone(); tokio::spawn(async move { Engine(inner).pump() });`.
  - `pump`: `loop { lock state; if closed || active_count >= max || queue empty { break } let item = queue.remove(0); starting.insert(id); drop lock; let inner=self.clone(); tokio::spawn(async move { let r = inner.start(item).await; ...catch/finally... }); }` — re-`pump` from the finally.
  - `start` is `async`: lock only for short reads/writes; `.await` the sync_fn OUTSIDE any held lock. `spawn_claude` is sync and returns the `SpawnHandle` (owns receivers); `attach` extracts `handle.run.clone()` for the kill surface, then **moves** the rest of the handle into the pump task.
  - `attach`'s pump task: `tokio::spawn(async move { loop { tokio::select!{ Some(ev)=handle.events.recv()=>{ self.on_event(&id,&ev); }, code=&mut handle.exit=>{ self.on_exit(&id, code.unwrap_or(1)); break; } } } })` — drain remaining events via `try_recv` after exit before `on_exit` (Phase 3's collect() shows results can arrive alongside exit). `on_event`/`on_exit` lock state in short sections and call `store.update(...).await` OUTSIDE the lock (re-lock as needed). Cost accumulation reads `cur.cost_usd` via `store.get(id).await`.
  - Because `store.update`/`store.get` are `async` and the pump task is async, `on_event`/`on_exit` are `async fn`s the select loop `.await`s — fine (no lock held across the await).
- [ ] **Run** — expect all Task 5 tests pass. Run the **whole** suite (`cargo test --manifest-path server-rs/Cargo.toml`) to confirm no regressions.
- [ ] **Commit:** `Phase 4 T5: engine.rs — submit/pump/start/attach happy path + error classification + lifecycle logs`.

---

### Task 6: watchdog (idle/wall reap, idle-TTL parked reap, parked exemptions) + kill/interrupt + kill-during-sync (mirror `engine.test.ts` "Engine watchdog" + kill tests)

**Files:**
- Modify: `server-rs/src/engine.rs`
- Test: inline `#[cfg(test)]`

**Steps:**
- [ ] **Write failing tests** (mirror `engine.test.ts` lines 265-277 + 404-538). The watchdog tests back-date `last_event_at`/`turn_started_at`/`awaiting` directly — expose **test-only** setters on the inner state behind `#[cfg(test)]` (e.g. `set_last_event_at`/`set_turn_started_at`/`set_awaiting` methods, or make the test module `use super::*` and reach into `self.0.state.lock()`). Representative:
  ```rust
  #[tokio::test]
  async fn kill_running_marks_killed() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let e = make_engine(&src, EngineOverrides::default()).await;
      let mut slow = HashMap::new(); slow.insert("FAKE_CLAUDE_SLEEP".into(), "5".into());
      let id = e.submit("demo", "slow", slow).unwrap();
      wait_status(&e, &id, "running").await;
      e.kill(&id);
      wait_status(&e, &id, "killed").await;
  }

  #[tokio::test]
  async fn watchdog_idle_reap_frees_the_slot() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), idle_max_ms: Some(500), wall_max_ms: Some(3_600_000), ..Default::default() }).await;
      let mut env = HashMap::new(); env.insert("FAKE_CLAUDE_SLEEP".into(), "30".into());
      let id = e.submit_session(vec![], vec![], "stay forever".into(), env, SubmitMeta::default()).unwrap();
      wait_status(&e, &id, "running").await;
      e.test_set_last_event_at(&id, e.now() - 60_000);
      e.trigger_watchdog();
      wait_status(&e, &id, "failed").await;
      let s = e.get(&id).await.unwrap();
      assert!(s.error.as_deref().unwrap().to_lowercase().contains("turn watchdog"));
      assert!(!regex::Regex::new("(?i)limit|resets").unwrap().is_match(s.error.as_deref().unwrap()));
      assert_eq!(s.error_kind.as_deref(), Some("idle_timeout"));
      let id2 = e.submit_session(vec![], vec![], "after".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      wait_status(&e, &id2, "done").await;
  }

  #[tokio::test]
  async fn watchdog_wall_reap_not_a_usage_limit() {
      // idle_max high, wall_max=500; back-date turn_started_at; assert wall_timeout + no "limit"/"wall" present.
  }

  #[tokio::test]
  async fn idle_ttl_reaps_parked_session_leaving_it_done() {
      // idle_ttl_ms=500; set awaiting=true + last_event_at old; trigger → "done".
  }

  #[tokio::test]
  async fn does_not_wall_reap_a_parked_session() {
      // wall_max=500, awaiting=true, turn_started_at old, last_event_at fresh → stays "running".
  }

  #[tokio::test]
  async fn idle_ttl_reaped_parked_stays_done_after_exit() {
      // after trigger pre-sets done + kill(), wait for the process to exit; status stays "done", error null.
  }

  #[tokio::test]
  async fn honors_kill_during_worktree_sync_window_no_spawn() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      rename_temp_git_repo_into(&src, "demo");
      let (tx, rx) = tokio::sync::oneshot::channel::<()>();
      let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
      let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
      let entered2 = entered.clone();
      let sync_fn: SyncFn = Arc::new(move |_p| {
          let rx = rx.clone(); let entered = entered2.clone();
          Box::pin(async move { entered.store(true, std::sync::atomic::Ordering::SeqCst); if let Some(r) = rx.lock().await.take() { let _ = r.await; } })
      });
      let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), sync_fn: Some(sync_fn), ..Default::default() }).await;
      let id = e.submit("demo", "go", HashMap::new()).unwrap();
      while !entered.load(std::sync::atomic::Ordering::SeqCst) { tokio::time::sleep(std::time::Duration::from_millis(5)).await; }
      e.kill(&id);          // kill during the (blocked) sync window
      let _ = tx.send(());  // release start()
      tokio::time::sleep(std::time::Duration::from_millis(80)).await;
      assert_eq!(e.get(&id).await.unwrap().status, "killed");
      assert!(!e.0.state.lock().unwrap().running.contains_key(&id), "no claude process spawned");
  }
  ```
- [ ] **Run** — expect fail.
- [ ] **Implement** `tick_watchdog` (per the Global Constraints, including the `continue` after an idle-TTL reap, the `parked` exemptions, marking the row BEFORE `stop()`), `kill`, `interrupt`, the `#[cfg(test)]` setters, and confirm `start()`'s re-read-after-sync bail (`cur.status=="killed" → return`) actually prevents the spawn. Recall the exit handler's status-precedence (`killed`/`failed`/`done` honored) was implemented in Task 5's `on_exit`.
- [ ] **Run** — expect all watchdog/kill tests pass + whole suite green.
- [ ] **Commit:** `Phase 4 T6: engine.rs — watchdog (idle/wall/idle-TTL reap, parked exemptions) + kill/interrupt + kill-during-sync`.

---

### Task 7: followUp (live-inject + queued/resume/retry, busy rejection, retitle, lastUserMessageAt) + streaming awaitingInput (mirror `engine.streaming.test.ts` + `engine.test.ts` "Engine.followUp")

**Files:**
- Modify: `server-rs/src/engine.rs`
- Test: inline `#[cfg(test)]`

**Steps:**
- [ ] **Write failing tests** (mirror `engine.streaming.test.ts` lines 52-145 + `engine.test.ts` lines 280-402). Use `fake-claude-stream.sh` for the persistent-process cases and `fake-claude.sh` for the queued-followup cases. Representative:
  ```rust
  #[tokio::test]
  async fn streaming_one_process_goes_idle_then_followup_injects_over_stdin() {
      let src = tmp(); std::fs::create_dir_all(&src).unwrap();
      let e = make_engine(&src, EngineOverrides { claude_bin: Some(fixture("fake-claude-stream.sh")), max_concurrent: Some(2), ..Default::default() }).await;
      let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
      let id = e.submit_session(vec![], vec![], "first turn".into(), HashMap::new(), SubmitMeta::default()).unwrap();
      let r = results.clone();
      e.subscribe(&id, Box::new(move |ev| { if matches!(ev, ClaudeEvent::Result{..}) { r.fetch_add(1, std::sync::atomic::Ordering::SeqCst); } }));
      wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
      let s = e.get(&id).await.unwrap();
      assert_eq!(s.status, "running");
      assert_eq!(s.awaiting_input, Some(true));
      assert_eq!(s.claude_session_id.as_deref(), Some("fake-stream-1"));
      assert!(s.cost_usd.unwrap_or(0.0) > 0.0);
      let since = e.follow_up(&id, "second turn", true).unwrap();
      let _ = since;
      assert_eq!(e.get(&id).await.unwrap().awaiting_input, Some(false));
      wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
      assert_eq!(e.get(&id).await.unwrap().awaiting_input, Some(true));
      assert_eq!(e.get(&id).await.unwrap().activity.unwrap().turns, 2);
      let prompts: Vec<String> = e.get_log(&id).iter().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
          .filter(|o| o["type"]=="agentic_prompt").map(|o| o["text"].as_str().unwrap().to_string()).collect();
      assert_eq!(prompts, vec!["first turn", "second turn"]);
      e.kill(&id);
      wait_status(&e, &id, "killed").await;
  }

  #[tokio::test]
  async fn followup_does_not_throw_on_live_session() { /* submit stream, wait result, follow_up must be Ok */ }

  #[tokio::test]
  async fn followup_bumps_last_user_message_at_on_live_inject() { /* t2 > t1 */ }

  #[tokio::test]
  async fn finalizes_failed_when_a_later_turn_crashes_after_success() {
      // fake-claude-stream-crash2.sh: turn1 ok, follow_up → turn2 crashes → status "failed", errorKind "crashed".
  }

  #[tokio::test]
  async fn interrupt_leaves_session_alive() {
      // submit stream, wait result, interrupt → status stays "running"; interrupt("nope") is a no-op.
  }

  #[tokio::test]
  async fn followup_reruns_finished_session_same_worktree_accumulates_cost() {
      // fake-claude.sh, submit→done, follow_up "second" → running → done; same worktree_path; cost ~= 2x; log grew.
  }

  #[tokio::test]
  async fn followup_retitles_only_when_set_title_true() {
      // prompt becomes "second"; follow_up(..,false) leaves it "second".
  }

  #[tokio::test]
  async fn followup_retry_when_no_claude_session_id() {
      // fake-claude-noinit.sh, submit→done with claude_session_id None; follow_up Ok; runs; log contains "retry me".
  }

  #[tokio::test]
  async fn followup_unknown_session_errors() { assert!(e.follow_up("nope","x",true).is_err()); }

  #[tokio::test]
  async fn rejects_second_followup_while_first_queued() {
      // maxConcurrent=1; saturate with a slow blocker; first follow_up queues; second → Err containing "busy".
  }
  ```
  Provide `wait_until(pred)` (polls every 20ms up to 5s) alongside `wait_status`.
- [ ] **Run** — expect fail.
- [ ] **Implement** `follow_up` (live-inject vs queued branches, `is_busy`, retitle patch, `defer_pump`) exactly per the Global Constraints. The live-inject path writes `encode_user_message(&compose_user_text(prompt))` to `running[id].run` (the shared `Arc<dyn RunHandle>`). Confirm `awaiting_input` flips false-on-inject / true-on-result (already wired in Task 5's `on_event`). The "later turn crashes after success" case relies on Phase 3's per-write `saw_result` reset in `SpawnHandle.write` — but here the engine writes to `run` directly, not through `SpawnHandle.write`. **Decision:** keep the `SpawnHandle.write` reset semantics by writing through a stored write-surface that resets `saw_result` — i.e. store the `SpawnHandle`'s write capability. Since `attach` moves the `SpawnHandle` into the pump task, expose a `write(line)` on `RunningTurn` that forwards to a cloned write-surface. **Simplest correct port:** keep an `Arc`-shared `saw_result` + the `run` handle in `RunningTurn`, and a `write` method that does `saw_result.store(false); run.write(line)` (mirroring `SpawnHandle::write`). Extract both `run` and `saw_result` from the `SpawnHandle` before moving it (Phase 3 made `run` `pub(crate)`; add a `pub(crate) fn split_for_engine(self) -> (Arc<dyn RunHandle>, Arc<AtomicBool>, mpsc::UnboundedReceiver<ClaudeEvent>, oneshot::Receiver<i32>, JoinHandle<()>)` on `SpawnHandle`, OR make `saw_result` + `run` `pub(crate)` and read them — pick the accessor that keeps Phase 3 tests compiling). The pump task still owns `events`/`exit`; `RunningTurn.run` + `saw_result` are the engine-side kill/write surface.
- [ ] **Run** — expect all followUp/streaming tests pass + whole suite green. **Verify** the crash-after-success test specifically (it is the regression that proves `saw_result` resets per injected turn).
- [ ] **Commit:** `Phase 4 T7: engine.rs — followUp (live-inject + queued/resume/retry) + streaming awaitingInput`.

---

### Task 8: recover/reconcileWorktrees + discard/deleteSession + workflowRunning surfacing (mirror `engine.recover.test.ts` + `engine.test.ts` "Engine worktree cleanup")

**Files:**
- Modify: `server-rs/src/engine.rs`
- Test: inline `#[cfg(test)]`

**Steps:**
- [ ] **Write failing tests** (mirror `engine.recover.test.ts` all + `engine.test.ts` lines 541-674, minus `commitGraph`/`commitFiles` which are Phase 6). The recover tests build a `Store` directly, write rows + logs, close it, then construct an `Engine` over the **same** db/log dir and assert the recovered status. Representative:
  ```rust
  #[tokio::test]
  async fn recover_finalizes_running_as_done_when_log_ended_in_result() {
      let work = tmp(); std::fs::create_dir_all(&work).unwrap();
      { let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
        store.create(CreateInput { id: "s1".into(), prompt: "p".into(), worktree_path: Some(work.join("s1").to_string_lossy().into()), ..Default::default() }).await.unwrap();
        store.update("s1", SessionPatch { status: Some("running".into()), ..Default::default() }).await.unwrap();
        store.append_log("s1", r#"{"type":"result","subtype":"success","is_error":false}"#).await.unwrap();
      } // store dropped → pool closed
      let e = engine_from(&work).await;     // Engine::new over the same work dir, fake bin, clone errors
      assert_eq!(e.get("s1").await.unwrap().status, "done");
  }

  #[tokio::test]
  async fn recover_finalizes_running_as_failed_interrupted_when_no_result() {
      // append a stream_event (no result) → status "failed", error matches "interrupted", errorKind "interrupted".
  }

  #[tokio::test]
  async fn recover_finalizes_running_as_failed_with_real_reason_on_error_result() {
      // last line result is_error=true "session limit" → "failed", errorKind "usage_limit", error matches "session limit".
  }

  #[tokio::test]
  async fn recover_requeues_pending_and_runs_to_done() {
      // create a pending row with a real scratch cwd; Engine::new → wait "done".
  }

  #[tokio::test]
  async fn recover_fails_unstartable_pending_without_crashing() {
      // pending row with worktree_path=None → Engine::new must not panic; wait "failed".
  }

  #[tokio::test]
  async fn recover_keeps_draining_after_one_fails_to_start() {
      // bad row (no worktree) + good scratch row → good reaches "done", poison "failed".
  }

  #[tokio::test]
  async fn reconcile_removes_orphan_worktree_dirs_keeps_live() {
      // create "live" row + mkdir worktrees_root/live + worktrees_root/orphan; Engine::new → orphan gone, live kept.
  }

  #[tokio::test]
  async fn discard_deletes_branch_and_sets_discarded() {
      // doneSession via a temp repo; discard → worktree_state "discarded"; branch gone in the source repo.
  }

  #[tokio::test]
  async fn discard_rejects_running_and_already_cleaned() {
      // discard a done session twice (2nd errors); a running session discard errors with busy/running.
  }

  #[tokio::test]
  async fn delete_session_removes_finished_record_and_worktree() {
      // doneSession; delete_session(id,false); get None; worktree_path gone on disk.
  }

  #[tokio::test]
  async fn delete_session_refuses_running_without_force() {
      // running slow session; delete_session(id,false) → Err busy/running.
  }

  #[tokio::test]
  async fn delete_session_force_kills_and_removes_running() {
      // running slow; delete_session(id,true) → get None; worktree gone.
  }

  #[tokio::test]
  async fn delete_session_force_removes_queued_pending() {
      // maxConcurrent=1; slow first running; second pending; delete_session(second,true) → get None; worktree gone.
  }

  #[tokio::test]
  async fn reports_workflow_running_true_for_finished_session_with_live_workflow() {
      // doneSession; workflowRunning falsy; seed .claude-config/projects/-slug/sess/subagents/workflows/wf_live/agent-a1.meta.json; get().workflow_running == Some(true).
  }
  ```
- [ ] **Run** — expect fail (recover/discard/delete stubs).
- [ ] **Implement** `recover` (+ `last_result`), `reconcile_worktrees`, `discard` (+ `live_session`), `delete_session` (+ `wait_for_exit`), and confirm `with_activity`'s `workflow_running` branch (implemented in Task 4) reads `has_active_workflow`. `wait_for_exit`: poll `running.contains(id)` every ~10ms up to 5s (the `on_exit` pump task removes it), or `await` the pump `JoinHandle` with a timeout. `delete_session` calls `transcript.drop_session(id)` if `transcript.is_some()`.
- [ ] **Run** — expect all recover/discard/delete/workflow tests pass + **whole suite green** (`cargo test --manifest-path server-rs/Cargo.toml`).
- [ ] **Commit:** `Phase 4 T8: engine.rs — recover/reconcileWorktrees + discard/deleteSession + workflowRunning`.

---

### Task 9: wire `Engine` into `AppState` + `main.rs` (no behavior change; Phase 5 routes onto it)

**Files:**
- Modify: `server-rs/src/state.rs`, `server-rs/src/main.rs`
- Test: extend `server-rs/src/api/mod.rs` `test_state()` (construct an Engine) — keep existing API tests green.

**Steps:**
- [ ] **Write failing test:** in `api/mod.rs` tests, assert `AppState` carries an `engine` (e.g. a smoke test `state.engine.list().await` returns `[]` for a fresh store). Add an `engine: Arc<Engine>` field to the `AppState` constructed in `test_state()`.
- [ ] **Run** — expect fail (field missing).
- [ ] **Implement:** add `pub engine: Arc<crate::engine::Engine>` to `AppState`. In `main.rs`, build the `EngineConfig` from `Config` (map `src_root`/`worktrees_root`/`log_dir`/`db_path`/`claude_bin`/`max_concurrent`/`git_org`/`claude_config_base`; all injectable seams `None`; watchdog caps `None` = defaults, or read the `AGENTIC_TURN_IDLE_SEC`/`AGENTIC_TURN_WALL_SEC`/`AGENTIC_IDLE_TTL_SEC` envs in `config.rs` first — **defer env parsing to Phase 5** to keep this task mechanical; pass `None` now). Construct via `Engine::with_store(cfg, store.clone(), Some(transcript.clone()))` so the engine shares the already-open `Store` + `TranscriptCache`. Put `Arc::new(engine)` in `AppState`.
- [ ] **Run** the whole suite — expect green (the new engine smoke test + all prior tests).
- [ ] **Commit:** `Phase 4 T9: wire Engine into AppState + main.rs (Phase 5 routes onto it)`.

---

## Self-review (spec coverage vs. the notes; placeholder scan; type consistency)

- **Spec coverage:** Every `engine.ts` capability the Phase-4 row names — queue/concurrency/pump (T5), subscribe·emit pub/sub (T4/T5), watchdog (T6), recover (T8), followUp (T7), kill/interrupt (T6), workflows reading via `hasActiveWorkflow` (T4/T8), structured turn-lifecycle logging (T5) — has a task + mirrored tests. Out-of-scope items (`commitGraph`/`commitFiles` → need `structuredDiff` Phase 6; real FCM push → Phase 6; full `listWorkflows` payload → Phase 6; `writeSessionGuide` content → Phase 5; SDK runner → not used in Rust prod) are explicitly deferred with hooks (`push_fn` no-op; `has_active_workflow`-only port) so the engine compiles and Phase 5/6 fill them without re-touching it. Every `engine.test.ts`/`engine.recover.test.ts`/`engine.streaming.test.ts` case is mirrored.
- **Placeholder scan:** No `TODO`/`unimplemented!()` survives past its task — T4 explicitly notes the lifecycle stubs are filled in T5–T8; the final state after T8 has no stubs. The only intentional simplification is `writeSessionGuide` (no test asserts its content; T-note says skip/stub — leave a one-line comment, not an `unimplemented!`). The watchdog "wall" test body and several T6/T7/T8 cases are sketched (`// …`) rather than fully spelled out, but each names the exact fixture + asserted field/value; the implementer fills the body identically to the fully-written sibling tests in the same task. All **production** code blocks are complete and compile against the existing crate (verified signatures against `store.rs`/`spawner.rs`/`runner.rs`/`transcript.rs`/`stream.rs`).
- **Type consistency:** `Session.cost_usd: Option<f64>`, `exit_code: Option<i64>`, `error/error_kind: Option<String>`, `claude_session_id: Option<String>`, `last_user_message_at: i64`, `worktree_state: String` — all match `store.rs`. The exit code from Phase 3's `SpawnHandle.exit` is `i32` (0/1); stored as `Some(code as i64)`. `max_concurrent: Option<u64>` (None=unlimited) matches `config.rs`. `ClaudeEvent` variant fields (`Init{session_id}`, `Skill{names}`, `Ask{}`, `Result{is_error,cost_usd,text,raw}`, `Other{raw}`) match `stream.rs`. `SpawnHandle` fields (`run: pub(crate)`, `events`, `exit`, `saw_result`) match `spawner.rs` — Task 7 notes the one Phase-3 accessor to add (`split_for_engine`/`pub(crate) saw_result`) so the engine can hold the write+kill surface while the pump task owns the receivers. `TranscriptCache::drop_session(&str)` matches `transcript.rs`. `Store::update` takes `SessionPatch` (extended in T1 with `worktree_state`). The injectable `sync_fn`/`clone_fn`/`now_fn`/`log_fn`/`push_fn` are `Arc<dyn Fn… + Send + Sync>` so `EngineInner` stays `Send + Sync` for `tokio::spawn`.
- **Lock discipline:** the `Mutex<EngineState>` is never held across `.await` (every `store.*().await` and `sync_fn(..).await` runs after `drop(guard)`); the per-turn pump task is the only place events mutate state, mirroring the TS single-loop atomicity. `close()` aborts the watchdog + the pump tasks' kill path; `closed` short-circuits every callback (parity with TS `if (this.closed) return`).
