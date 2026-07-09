# Rust Rewrite — Phase 3 (spawner / runner: spawn claude, stdio stream-json, incremental tailing, worktree sync) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `server-rs/`, port the engine-independent spawn/run core to Rust at behavioral parity with the TS reference: `server/engine/runner.ts` (`Runner`/`RunHandle`/`RunSpec`/`localRunner`), `server/engine/tailer.ts` (`createTailer`/`Tailer`), `server/engine/spawner.ts` (`SpawnHandle`/`spawnClaude`/`composeUserText`/`encodeUserMessage`/`buildSpec`/`BASE_ARGS`/`OUTBOX_NOTE`), `server/engine/worktree.ts` (worktree create/remove/sync/diff/discard), and `server/engine/claudeConfig.ts` (`buildSessionConfigDir`). The result is everything the Phase 4 engine needs to launch a turn: build the argv + env, spawn `claude` via `tokio::process` with a live stdin pipe writing stdout/stderr to the session log, tail that log into `ClaudeEvent`s, and prepare/sync the per-session git worktree + CLAUDE config dir. No HTTP/engine/queue wiring (Phase 4 consumes this; Phase 5 serves it).

**Architecture:** Four new modules under `server-rs/src/`:
- `runner.rs` — `RunSpec`, `RunHandle` (trait), `Runner` (trait), `LocalRunner` (production, `tokio::process::Command`), plus a test-only spy runner. Mirror of `runner.ts`.
- `tailer.rs` — `EventTailer` (byte-offset + carry-buffer line splitter feeding `crate::stream::parse_line`). Mirror of `tailer.ts`. This is the *event* tailer (raw line → `ClaudeEvent`); it is a **different concern** from `transcript.rs`'s `RenderedProjection` (which produces *rendered display lines* with a byte cursor for the windowed read API). Do not merge them.
- `spawner.rs` — `SpawnOptions`, `SpawnHandle`, `spawn_claude`, `compose_user_text`, `encode_user_message`, `build_spec`, the `BASE_ARGS`/`OUTBOX_NOTE` constants. Mirror of `spawner.ts`. `SpawnHandle` exposes an async event/exit stream instead of an `EventEmitter` (Rust idiom: a `tokio::sync::mpsc` channel + a background poll task), preserving the same observable behavior (events drain from the tailer on a `POLL_MS` interval; a clean `result` line → exit 0, otherwise exit 1).
- `worktree.rs` — `SessionWorktree`, `create_session_worktrees`, `remove_session_worktrees`, `sync_worktree`, `diff_worktree`, `discard_worktree`, plus single-repo `create_worktree`/`remove_worktree`. Mirror of `worktree.ts`. Git is shelled out via `tokio::process::Command` (async; matches TS `execFile`/`execFileSync` semantics with a 30 s timeout on the network-bound calls).
- `claude_config.rs` — `build_session_config_dir`. Mirror of `claudeConfig.ts` (symlink the shared `~/.claude` items + a curated `skills/`).

**Tech Stack:** Rust (edition 2021). `tokio` (already a dep — `process`, `io`, `sync::mpsc`, `time`, `task`, `fs` all under the `full` feature). `serde_json` (already) for `encode_user_message`. Reuses `crate::stream::{parse_line, ClaudeEvent}` (Phase 2) and `crate::store::Store::log_path` (Phase 1). **No new crate dependencies.**

**Parity bar:** Behavioral parity with the TS reference is the bar. Every `tailer.test.ts`, `runner.test.ts`, and `spawner.test.ts` case has a mirrored Rust test asserting the same event sequence / argv / env / exit behavior, driven by the **same** `server/test/fixtures/fake-claude*.sh` scripts (referenced by absolute path from the repo, never the real `claude`).

## Global Constraints

Copy these exact parity values/field names verbatim into the implementation; do not paraphrase.

- **Crate:** `agentic-dev/server-rs/`. Run all `cargo` commands from there. **Commit only — NEVER push.** End every commit message with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Modules:** new files `server-rs/src/runner.rs`, `server-rs/src/tailer.rs`, `server-rs/src/spawner.rs`, `server-rs/src/worktree.rs`, `server-rs/src/claude_config.rs`; register each `mod …;` in `src/main.rs` (alphabetical with the existing `mod` lines: order becomes `api, auth, claude_config, config, runner, spawner, state, store, stream, tailer, throttle, transcript, worktree`). Do **not** touch `transcript.rs` (`RenderedProjection`/`is_rendered`/`filter_rendered`) or `stream.rs` (`parse_line`/`ClaudeEvent`) — reuse them.
- **`BASE_ARGS` (verbatim — `spawner.ts` lines 26-31):** `["--output-format", "stream-json", "--verbose", "--include-partial-messages", "--dangerously-skip-permissions"]`. Order is load-bearing (argv assertions in tests). As a Rust `const BASE_ARGS: [&str; 5]`.
- **`POLL_MS` (verbatim):** `120` (ms). The `SpawnHandle` poll interval.
- **`OUTBOX_NOTE` (verbatim — `spawner.ts` lines 36-41, byte-for-byte):**
  ```
  [Delivering files to the user: this session runs in an app with no terminal. To give the user a file — a build artifact (e.g. an APK), a generated document, a report, an exported image — copy or write it into ./outbox/ (relative to your working directory; create the dir if needed). Files in ./outbox/ are shown in the app to preview and download. Do this whenever the user asks you to send, give, share, or export a file. Do NOT put ordinary source-code edits in ./outbox/.]
  ```
  Build it from the exact same string-concatenation pieces as TS so the bytes match. The only test assertion is `compose_user_text("...")` contains `"Delivering files"` — but keep the whole literal verbatim (it is sent to the model).
- **`compose_user_text(text)` (verbatim — `spawner.ts` lines 110-112):** `format!("{OUTBOX_NOTE}\n\n---\n\n{text}")`. The preamble, then `\n\n---\n\n`, then the user's text. This separator (`"\n\n---\n\n"`, 7 bytes) is the one `sdkRunner` strips for ask-answers — keep it exact.
- **`encode_user_message(text)` (verbatim — `spawner.ts` lines 115-117):** `serde_json::json!({"type":"user","message":{"role":"user","content":text}}).to_string() + "\n"`. One stream-json NDJSON line ending in `\n`. (TS uses `JSON.stringify(...) + "\n"`.)
- **`build_spec` argv assembly (verbatim — `spawner.ts` lines 119-143):** `args = ["-p", "--input-format", "stream-json"] ++ (resume_session_id ? ["--resume", <id>] : []) ++ extra ++ BASE_ARGS`, where `extra` is built **in this order**: `if model {push "--model", model}`; `let ultracode = mode == Some("ultracode"); if effort.is_some() && !ultracode {push "--effort", effort}`; `if ultracode {push "--settings", "{\"ultracode\":true}"}`. The prompt is **NOT** an argv positional (it rides on stdin via `compose_user_text` + `encode_user_message`). `--input-format stream-json` is always present (one persistent session per process). The ultracode settings literal is exactly `{"ultracode":true}`.
- **`CLAUDE_CONFIG_DIR` env (verbatim — `spawner.ts` lines 127-129):** start from the merged env (process env overlaid with `opts.env`), then **always** set `env["CLAUDE_CONFIG_DIR"] = opts.claude_config_dir.unwrap_or("".to_string())` — set-or-clear, so the parent's value never leaks. When `claude_config_dir` is `None` the value is the **empty string** (`fake-claude-echoenv.sh` asserts `CLAUDE_CONFIG_DIR=` exactly).
- **cgroup cap fields (`memory_max`/`memory_high`/`cpu_quota`/`tasks_max`):** carried on `SpawnOptions` and threaded into `RunSpec` verbatim (`Option<String>`), but **NOT enforced** by `LocalRunner` (parity: TS comment "NOT enforced by the local runner"). They exist only so the spy-runner test can assert they were threaded through, and so Phase 4 can pass them. Omit (`None`) when unset.
- **`LocalRunner` spawn (verbatim — `runner.ts` lines 46-74):** open `spec.log_path` for **append** (create if missing); spawn `spec.bin` with `spec.args`, `cwd = spec.cwd`, `env = spec.env` (cleared base env + exactly the spec's map — see env note), `stdin = piped`, `stdout`/`stderr` → the log file, in its **own process group** (TS `detached: true`) so `stop()` can kill the whole tree. `is_active()` is true until the child exits; `exit_code()` is the child's status code (meaningful once inactive). `stop()` sends SIGTERM to the **process group** (`-pid`), falling back to the child alone. `write(line)` pushes one NDJSON line to the child's stdin (appending `\n` if absent); `end_input()` closes stdin (EOF).
- **`RunHandle` contract (verbatim — `runner.ts` lines 27-37):** `is_active() -> bool`; `exit_code() -> Option<i32>` (meaningful once `!is_active()`); `stop()`; optional `interrupt()`; optional `write(line)`; optional `end_input()`. In Rust this is a trait with default `interrupt/write/end_input` no-ops (TS's optional methods).
- **`EventTailer` (verbatim — `tailer.ts`):** constructed with `(path, start_offset = 0)`. `poll() -> Vec<ClaudeEvent>`: if the file is missing → `vec![]`; else read **only** bytes from `offset` to current EOF (`size <= offset → vec![]`), append to a UTF-8 carry buffer, then drain **complete** newline-terminated lines through `crate::stream::parse_line` (a line may yield 0..n events; flatten). `flush() -> Vec<ClaudeEvent>`: parse any trailing **partial** (newline-less) line once after the writer exits — `let rest = buffer.trim(); if rest.is_empty() {return vec![]}; buffer.clear(); parse_line(rest)`. `offset() -> u64`: bytes consumed so far. The carry buffer holds a partial last line across polls. Read bytes, not chars, and decode lossily so a multi-byte char split across two reads survives (use a `Vec<u8>` carry; split on `b'\n'`; `String::from_utf8_lossy` each complete line — but because lines are split on the byte `\n` and a complete line is valid UTF-8, lossy never actually corrupts).
- **`SpawnHandle` event/exit semantics (verbatim — `spawner.ts` lines 45-100):** drives a background task that every `POLL_MS` calls `tailer.poll()` and forwards each event; latches `saw_result = true` on a `Result { is_error: false, .. }` event (an **error** result is a failed turn → does NOT latch). When the run goes inactive (`!run.is_active()`), it **finishes once**: drains `tailer.poll()` **then** `tailer.flush()`, forwards those events, and emits exit `if saw_result {0} else {1}` — success is decided by claude's own clean `result` line, **never** the OS exit status (a killed/crashed turn emits no clean result → exit 1). `kill()` calls `run.stop()`. `write(line)` resets `saw_result = false` (the latch must reflect only the current turn) **then** forwards to `run.write`. `end_input()` → `run.end_input`. `interrupt()` → `run.interrupt()` if present, else write a stream-json interrupt control_request to stdin: `{"type":"control_request","request_id":"int-<now_ms>","request":{"subtype":"interrupt"}}`. `detach()` stops polling without killing the run.
- **Tail start offset (verbatim — `spawner.ts` lines 145-155):** `spawn_claude` constructs the tailer at the **current size** of `log_path` (0 if missing), so the `agentic_prompt` marker the engine wrote just before spawning reaches clients via backfill, not as a live event. Use a `file_size(path) -> u64` helper (`= metadata(path).map(|m| m.len()).unwrap_or(0)`).
- **`spawn_claude(opts, runner)` (verbatim — `spawner.ts` lines 151-156):** `let run = runner.start(build_spec(&opts)); let handle = SpawnHandle::new(run, EventTailer::new(&opts.log_path, file_size(&opts.log_path))); handle.start_loop(); handle`. The runner is injected (prod `LocalRunner`, tests pass a fake/spy). Default arg = `LocalRunner::new()`.
- **Worktree git commands (verbatim — `worktree.ts`):**
  - `create_session_worktrees(repo_specs: &[(repo, repo_path)], root, id) -> Vec<SessionWorktree>`: `session_dir = root/id`, `branch = format!("agentic/{id}")`, `mkdir -p session_dir`. For each `(repo, repo_path)`: `worktree_path = session_dir/repo`; `base_sha = git -C <repo_path> rev-parse HEAD` (trimmed); `git -C <repo_path> worktree add <worktree_path> -b <branch>`; push `SessionWorktree { repo, worktree_path, base_sha }`.
  - `remove_session_worktrees(repo_specs, session_dir)`: for each, `git -C <repo_path> worktree remove --force <session_dir/repo>` then `git -C <repo_path> worktree prune`, **both** wrapped so failure is ignored (idempotent).
  - `sync_worktree(worktree_path)` (async, best-effort): resolve the remote default branch — `base = git -C <wt> symbolic-ref --short refs/remotes/origin/HEAD` → if it starts with `origin/`, `base = &head["origin/".len()..]`, else fall back to `"master"` (also on error); `git -C <wt> fetch origin <base>`; `git -C <wt> rebase --autostash origin/<base>`. On **any** error, `git -C <wt> rebase --abort` (ignore its error) and return. Never propagate (a turn must not break because sync failed). Network-bound calls get a **30 s timeout** (`GIT_TIMEOUT_MS = 30_000`).
  - `diff_worktree(worktree_path, base_sha) -> String` (async): `git -C <wt> add -AN` (intent-to-add so untracked files show), then `git -C <wt> --no-pager diff <base_sha>`; return stdout. (No 64 MB maxBuffer cap needed — `tokio::process` `output()` collects all stdout; keep the 30 s timeout.)
  - `discard_worktree(repo_path, worktree_path, branch)`: if `worktree_path` exists → `git -C <repo_path> worktree remove --force <worktree_path>`, else `git -C <repo_path> worktree prune`; then `git -C <repo_path> branch -D <branch>` (ignore error if gone). Idempotent.
  - `create_worktree(repo_path, root, repo, id) -> WorktreeInfo` / `remove_worktree(repo_path, worktree_path)`: the single-repo variants (`worktree.ts` lines 15-26) — `worktree_path = root/repo/id`, `branch = agentic/<id>`, `mkdir -p root/repo`, `git -C <repo_path> worktree add <worktree_path> -b <branch>`. Keep for parity; the engine uses the multi-repo variant.
- **`build_session_config_dir(base_claude_dir, chosen_skills, dest_dir) -> PathBuf` (verbatim — `claudeConfig.ts`):** `LINKED = [".credentials.json", "CLAUDE.md", "settings.json", "settings.local.json", "plugins", "memory"]`. `mkdir -p dest_dir`. For each `item` in `LINKED`: `rm -rf dest_dir/item`; if `base_claude_dir/item` exists → `symlink(base_claude_dir/item, dest_dir/item)`. Then `skills_dir = dest_dir/skills`; `rm -rf skills_dir`; `mkdir skills_dir`. For each `name` in `chosen_skills`: **skip** if `name == "."` OR contains `".."` OR contains `"/"` OR does not fully match `^[A-Za-z0-9_.-]+$`; else if `base_claude_dir/skills/name` exists → `symlink(base_claude_dir/skills/name, skills_dir/name)`. Return `dest_dir`. Do **NOT** wipe `dest_dir` wholesale (claude writes its `projects/` transcript there; it must survive a rebuild for `--resume`). Idempotent — re-run replaces the `LINKED` symlinks and the whole `skills/` subtree only.
- **Fixtures (verbatim paths):** tests reference the existing shell fixtures by absolute path under the repo: `server/test/fixtures/fake-claude.sh` (init `fake-sess-123`, `model claude-opus-4-8`, two text deltas `Hello `+`world`, `result is_error=false total_cost_usd=0.0042`; honors `FAKE_CLAUDE_SLEEP`), `fake-claude-error.sh` (`result is_error=true result="…session limit…"`), `fake-claude-echoargs.sh` (writes argv to `FAKE_ARGS_OUT`), `fake-claude-echoenv.sh` (writes `CLAUDE_CONFIG_DIR=<val>` to `FAKE_ENV_OUT`). Resolve them via `env!("CARGO_MANIFEST_DIR")` (= `…/server-rs`) joined to `../server/test/fixtures/<name>`. **Never** invoke the real `claude` in tests.
- **Out of scope (later phases):** the SDK-based `sdkRunner.ts` (the official Agent SDK responder for in-turn AskUserQuestion) — Rust prod uses the raw-CLI `LocalRunner` over `tokio::process` with the interrupt control_request fallback already wired in `SpawnHandle`; the engine's queue/concurrency/pump/subscribe/watchdog/recover (Phase 4); the structured turn-lifecycle logging (Phase 4); wiring worktrees into session create + diff/discard HTTP endpoints (Phase 5/6); `repos.ts`/`templates.ts`/`groups.ts` (Phase 4+). All `cargo test` green before each commit.

## File Structure

- `server-rs/src/runner.rs` — NEW: `RunSpec`, `RunHandle` (trait), `Runner` (trait), `LocalRunner`, inline `#[cfg(test)] mod tests`.
- `server-rs/src/tailer.rs` — NEW: `EventTailer`, inline tests.
- `server-rs/src/spawner.rs` — NEW: `SpawnOptions`, `SpawnHandle`, `spawn_claude`, `compose_user_text`, `encode_user_message`, `build_spec`, `BASE_ARGS`, `OUTBOX_NOTE`, `POLL_MS`, inline tests.
- `server-rs/src/worktree.rs` — NEW: `WorktreeInfo`, `SessionWorktree`, the create/remove/sync/diff/discard fns, inline tests.
- `server-rs/src/claude_config.rs` — NEW: `build_session_config_dir`, `LINKED`, inline tests.
- `server-rs/src/main.rs` — add the five `mod …;` lines (alphabetical).
- `server-rs/Cargo.toml` — **unchanged** (no new deps).

---

### Task 1: `runner.rs` — `RunSpec`/`RunHandle`/`Runner`/`LocalRunner` (mirror `runner.ts`)

**Files:**
- Create: `server-rs/src/runner.rs`
- Modify: `server-rs/src/main.rs` (add `mod runner;`)
- Test: inline `#[cfg(test)]` in `src/runner.rs`

**Interfaces (exact Rust signatures):**
```rust
/// What to run for one turn. stdout+stderr are appended to log_path (the session log file).
/// Mirror of TS `RunSpec`.
#[derive(Clone, Debug, Default)]
pub struct RunSpec {
    pub bin: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: std::collections::HashMap<String, String>,
    pub log_path: std::path::PathBuf,
    pub unit: String, // diagnostic label
    // cgroup caps — carried for compatibility, NOT enforced by LocalRunner (parity with runner.ts).
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
    // Structured turn config — LocalRunner already bakes these into `args`; carried for the SDK seam.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub resume_session_id: Option<String>,
}

/// A started turn. Liveness + exit are polled. Mirror of TS `RunHandle` (optional methods → defaulted).
pub trait RunHandle: Send + Sync {
    fn is_active(&self) -> bool;
    fn exit_code(&self) -> Option<i32>; // meaningful once is_active() is false
    fn stop(&self);
    fn interrupt(&self) {}
    fn write(&self, _line: &str) {}
    fn end_input(&self) {}
    /// True when the runner has a native turn-interrupt (e.g. a future SDK runner's query.interrupt()).
    /// LocalRunner returns the default `false`, so SpawnHandle::interrupt falls back to a stdin
    /// control_request. Mirror of TS `RunHandle.interrupt?` being present vs absent.
    fn has_native_interrupt(&self) -> bool { false }
}

pub trait Runner: Send + Sync {
    fn start(&self, spec: RunSpec) -> Box<dyn RunHandle>;
}

/// Spawns claude as a persistent child writing stdout/stderr to the log file; liveness via the
/// child's exit. Holds a live stdin pipe so the engine can inject follow-up turns. Mirror of `localRunner`.
pub struct LocalRunner;
impl LocalRunner { pub fn new() -> Self { LocalRunner } }
impl Default for LocalRunner { fn default() -> Self { LocalRunner::new() } }
impl Runner for LocalRunner { fn start(&self, spec: RunSpec) -> Box<dyn RunHandle> { /* … */ } }
```

**Implementation notes (load-bearing):**
- Use `std::process::Command` (sync spawn is fine — the child runs detached and we poll liveness; this matches TS `spawn` which is non-blocking). Set `.stdin(Stdio::piped())`, redirect stdout+stderr to the **same** appended fd of `log_path` (open with `OpenOptions::new().create(true).append(true)`, `try_clone()` for the second). Put the child in its own process group: on Unix use `std::os::unix::process::CommandExt::process_group(0)` so `kill(-pid)` reaches the whole tree (parity with TS `detached: true`).
- Liveness: spawn a background `std::thread` that calls `child.wait()` and stores the exit code + flips `active=false` behind an `Arc<Mutex<…>>` (mirrors TS `child.on("exit")`). Keep the child's `stdin` handle in the `RunHandle` impl for `write`/`end_input`.
- `stop()`: `kill(Pid::from_raw(-pid), SIGTERM)` — but to avoid a new `nix` dependency, shell it via `libc::kill(-(pid as i32), libc::SIGTERM)` (libc is a transitive dep of tokio/sqlx; if it is not directly usable, fall back to `std::process::Command::new("kill").args(["-TERM", &format!("-{pid}")])` which needs no new crate). Prefer the `kill` subprocess fallback to keep `Cargo.toml` unchanged. On failure, `child.kill()` the child alone.
- `write(line)`: `let line = if line.ends_with('\n') { line.into() } else { format!("{line}\n") }; let _ = self.stdin.lock().unwrap().as_mut().map(|w| w.write_all(line.as_bytes()));` (ignore a closed pipe). `end_input()`: drop/`take()` the stdin handle (closes the pipe → EOF).

**Steps:**
- [ ] **Step 1: write the module + a failing test.** Create `server-rs/src/runner.rs` with the types above and an unimplemented `LocalRunner::start` (e.g. `todo!()`). Add `mod runner;` to `main.rs`. Inline test (mirrors `runner.test.ts` "runs to the log file … exit code 0", using a hand-rolled poll loop like the TS `drain`):
```rust
#[cfg(test)]
mod tests {
    // NOTE: Task 1 ships before tailer.rs exists, so these tests must NOT import EventTailer.
    // They assert liveness/exit + raw log-file contents directly via a sleep+read loop (mirrors the
    // TS `drain` helper in runner.test.ts without the event parsing).
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/test/fixtures").join(name)
            .to_string_lossy().into_owned()
    }
    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-run-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap(); d
    }
    fn base_spec(dir: &std::path::Path) -> RunSpec {
        RunSpec { bin: fixture("fake-claude.sh"), args: vec![], cwd: dir.to_string_lossy().into_owned(),
            env: std::env::vars().collect::<HashMap<_, _>>(),
            log_path: dir.join("log.jsonl"), unit: "test".into(), ..Default::default() }
    }

    #[test]
    fn runs_to_log_file_and_exits_zero() {
        let dir = tmp_dir();
        let spec = base_spec(&dir);
        let log = spec.log_path.clone();
        let h = LocalRunner::new().start(spec);
        for _ in 0..200 { if !h.is_active() { break } std::thread::sleep(Duration::from_millis(20)); }
        assert!(!h.is_active());
        assert_eq!(h.exit_code(), Some(0));
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("\"type\":\"system\"") && body.contains("text_delta") && body.contains("\"type\":\"result\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stop_terminates_a_slow_run() {
        let dir = tmp_dir();
        let mut spec = base_spec(&dir);
        spec.env.insert("FAKE_CLAUDE_SLEEP".into(), "5".into());
        let h = LocalRunner::new().start(spec);
        // let it emit its first lines
        for _ in 0..50 {
            if std::fs::read_to_string(dir.join("log.jsonl")).map(|s| !s.is_empty()).unwrap_or(false) { break }
            std::thread::sleep(Duration::from_millis(20));
        }
        h.stop();
        for _ in 0..100 { if !h.is_active() { break } std::thread::sleep(Duration::from_millis(20)); }
        assert!(!h.is_active());
        std::fs::remove_dir_all(&dir).ok();
    }
}
```
- [ ] **Step 2: run `cargo test --lib runner::`, expect FAIL** (`todo!()` panics / does not exit). Confirm the test compiles and the failure is the unimplemented body, not a type error.
- [ ] **Step 3: implement `LocalRunner::start`** per the notes above (spawn, fd redirect, process group, wait-thread, stdin handle, stop/write/end_input). Implementation:
```rust
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

struct LocalHandle {
    state: Arc<Mutex<(bool, Option<i32>)>>, // (active, exit_code)
    pid: u32,
    stdin: Mutex<Option<std::process::ChildStdin>>,
}
impl RunHandle for LocalHandle {
    fn is_active(&self) -> bool { self.state.lock().unwrap().0 }
    fn exit_code(&self) -> Option<i32> { self.state.lock().unwrap().1 }
    fn stop(&self) {
        // Kill the whole process group (matches TS process.kill(-pid, SIGTERM)).
        let killed = Command::new("kill").args(["-TERM", &format!("-{}", self.pid)]).status().map(|s| s.success()).unwrap_or(false);
        if !killed { let _ = Command::new("kill").args(["-TERM", &self.pid.to_string()]).status(); }
    }
    fn write(&self, line: &str) {
        let line = if line.ends_with('\n') { line.to_string() } else { format!("{line}\n") };
        if let Some(w) = self.stdin.lock().unwrap().as_mut() { let _ = w.write_all(line.as_bytes()); let _ = w.flush(); }
    }
    fn end_input(&self) { let _ = self.stdin.lock().unwrap().take(); }
}

impl Runner for LocalRunner {
    fn start(&self, spec: RunSpec) -> Box<dyn RunHandle> {
        use std::fs::OpenOptions;
        let log = OpenOptions::new().create(true).append(true).open(&spec.log_path).expect("open log");
        let log2 = log.try_clone().expect("clone log fd");
        let mut cmd = Command::new(&spec.bin);
        cmd.args(&spec.args).current_dir(&spec.cwd)
            .env_clear().envs(&spec.env)
            .stdin(Stdio::piped()).stdout(Stdio::from(log)).stderr(Stdio::from(log2));
        #[cfg(unix)]
        { use std::os::unix::process::CommandExt; cmd.process_group(0); }
        let mut child = cmd.spawn().expect("spawn claude");
        let pid = child.id();
        let stdin = child.stdin.take();
        let state = Arc::new(Mutex::new((true, None::<i32>)));
        let st = state.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let mut g = st.lock().unwrap(); g.0 = false; g.1 = code;
        });
        Box::new(LocalHandle { state, pid, stdin: Mutex::new(stdin) })
    }
}
```
> **Rust feature notes (for the C#/Go/JS reader):** `Box<dyn RunHandle>` is a trait object — like a `RunHandle*` interface pointer in C#/Go (`interface{}`-typed) — chosen so `Runner::start` can return either the real `LocalHandle` or a test fake. `Arc<Mutex<…>>` is a thread-safe shared, mutable cell (Go: a struct behind a `sync.Mutex` shared via pointer; C#: a `lock`-guarded field). The wait-thread is the analogue of TS `child.on("exit", …)` — a callback that flips `active=false` when the OS reports the child gone. **Gotcha:** `env_clear().envs(&spec.env)` exactly reproduces TS, where `spec.env` is the **full** merged env (spawner already merged `process.env`); do NOT additionally inherit the parent env or `CLAUDE_CONFIG_DIR` would leak. **Gotcha:** `try_clone()` gives a second fd to the same file so stdout and stderr both append in order (matches TS `stdio: ["pipe", fd, fd]`).
- [ ] **Step 4: run `cargo test --lib runner::`, expect PASS.** Then full `cargo test` from `server-rs/` — confirm no regressions.
- [ ] **Step 5: commit** (commit-only, NEVER push):
```
git -C agentic-dev add server-rs/src/runner.rs server-rs/src/main.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T1: runner.rs — RunSpec/RunHandle/Runner/LocalRunner (tokio process seam)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `tailer.rs` — `EventTailer` (mirror `tailer.ts` + `tailer.test.ts`)

**Files:**
- Create: `server-rs/src/tailer.rs`
- Modify: `server-rs/src/main.rs` (add `mod tailer;`)
- Test: inline `#[cfg(test)]`

**Interfaces (exact Rust signatures):**
```rust
use crate::stream::{parse_line, ClaudeEvent};
use std::path::{Path, PathBuf};

/// Reads complete newline-delimited lines appended to a file since a byte offset, parsing each into
/// `ClaudeEvent`s via `parse_line`. Stateful: tracks the byte offset consumed + a carry buffer for a
/// partial last line. Mirror of TS `Tailer`/`createTailer`.
pub struct EventTailer {
    path: PathBuf,
    offset: u64,
    buffer: Vec<u8>, // carry: bytes after the last newline
}

impl EventTailer {
    pub fn new(path: impl AsRef<Path>, start_offset: u64) -> Self { /* … */ }
    pub fn poll(&mut self) -> Vec<ClaudeEvent> { /* parse newly-appended complete lines */ }
    pub fn flush(&mut self) -> Vec<ClaudeEvent> { /* parse a trailing newline-less line, once */ }
    pub fn offset(&self) -> u64 { self.offset }
}
```

**Implementation:**
```rust
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

impl EventTailer {
    pub fn new(path: impl AsRef<Path>, start_offset: u64) -> Self {
        EventTailer { path: path.as_ref().to_path_buf(), offset: start_offset, buffer: Vec::new() }
    }

    fn drain_lines(&mut self) -> Vec<ClaudeEvent> {
        let mut events = Vec::new();
        while let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=nl).collect(); // includes the '\n'
            let s = String::from_utf8_lossy(&line[..line.len() - 1]); // strip the '\n'
            events.extend(parse_line(&s));
        }
        events
    }

    pub fn poll(&mut self) -> Vec<ClaudeEvent> {
        if !self.path.exists() { return Vec::new(); }
        let mut f = match File::open(&self.path) { Ok(f) => f, Err(_) => return Vec::new() };
        let size = match f.metadata() { Ok(m) => m.len(), Err(_) => return Vec::new() };
        if size <= self.offset { return Vec::new(); }
        let len = (size - self.offset) as usize;
        if f.seek(SeekFrom::Start(self.offset)).is_err() { return Vec::new(); }
        let mut buf = vec![0u8; len];
        let read = match f.read(&mut buf) { Ok(n) => n, Err(_) => return Vec::new() };
        self.offset += read as u64;
        self.buffer.extend_from_slice(&buf[..read]);
        self.drain_lines()
    }

    pub fn flush(&mut self) -> Vec<ClaudeEvent> {
        let rest = String::from_utf8_lossy(&self.buffer);
        let rest = rest.trim().to_string();
        if rest.is_empty() { return Vec::new(); }
        self.buffer.clear();
        parse_line(&rest)
    }

    pub fn offset(&self) -> u64 { self.offset }
}
```
> **Parity note vs `tailer.ts`:** TS keeps `buffer` as a JS string and `offset` advances by **bytes** read (`offset += read`). The Rust version keeps `buffer` as `Vec<u8>` and advances `offset` by bytes too — identical accounting. Splitting on the byte `b'\n'` then `from_utf8_lossy` per complete line is safe because (a) lines are newline-delimited and each complete line is valid UTF-8, and (b) a partial trailing line's bytes stay in `buffer` until its newline arrives, so a multi-byte char split across two `read`s is reassembled before decoding (mirrors the `tailer.test.ts` "carries a partial line across polls" case at the byte level).

**Steps:**
- [ ] **Step 1: write failing tests** mirroring `tailer.test.ts` (missing file → `[]`; appended complete lines + offset advance; partial line carried; `flush` of a newline-less line; non-zero start offset skips prior bytes). Implement `EventTailer` with `poll`/`flush` returning `Vec::new()` stubs first so tests fail.
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    const TEXT: &str = r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hi"}}}"#;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-tail-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap(); d.join("log.jsonl")
    }
    fn append(p: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn missing_file_yields_empty() {
        let p = std::env::temp_dir().join("nope-xyz-agentic").join("x.jsonl");
        assert!(EventTailer::new(&p, 0).poll().is_empty());
    }

    #[test]
    fn parses_appended_lines_and_advances_offset() {
        let p = tmp();
        append(&p, &format!("{INIT}\n"));
        let mut t = EventTailer::new(&p, 0);
        assert!(matches!(t.poll().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1"));
        assert!(t.poll().is_empty()); // nothing new
        append(&p, &format!("{TEXT}\n"));
        assert!(matches!(t.poll().as_slice(), [ClaudeEvent::Text { text, .. }] if text == "hi"));
    }

    #[test]
    fn carries_partial_line_across_polls() {
        let p = tmp();
        let mut t = EventTailer::new(&p, 0);
        append(&p, &INIT[..10]); // half a line, no newline
        assert!(t.poll().is_empty());
        append(&p, &format!("{}\n", &INIT[10..])); // complete it
        assert!(matches!(t.poll().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1"));
    }

    #[test]
    fn flush_parses_trailing_newlineless_line() {
        let p = tmp();
        append(&p, INIT); // no trailing newline
        let mut t = EventTailer::new(&p, 0);
        assert!(t.poll().is_empty()); // incomplete → buffered
        assert!(matches!(t.flush().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1"));
    }

    #[test]
    fn honours_non_zero_start_offset() {
        let p = tmp();
        append(&p, &format!("{INIT}\n"));
        let start = format!("{INIT}\n").len() as u64;
        let mut t = EventTailer::new(&p, start);
        assert!(t.poll().is_empty()); // skip what was already there
        append(&p, &format!("{TEXT}\n"));
        assert!(matches!(t.poll().as_slice(), [ClaudeEvent::Text { text, .. }] if text == "hi"));
    }
}
```
- [ ] **Step 2: run `cargo test --lib tailer::`, expect FAIL** (stubbed `poll`/`flush` return empty).
- [ ] **Step 3: implement** `EventTailer::{new, drain_lines, poll, flush, offset}` per the code above.
- [ ] **Step 4: run `cargo test --lib tailer::`, expect PASS.** Then full `cargo test` — no regressions.
- [ ] **Step 5: commit:**
```
git -C agentic-dev add server-rs/src/tailer.rs server-rs/src/main.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T2: tailer.rs — EventTailer (byte-offset line splitter → ClaudeEvent)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `spawner.rs` — args/env/preamble pure helpers (`build_spec`/`compose_user_text`/`encode_user_message`) + constants

**Files:**
- Create: `server-rs/src/spawner.rs` (the pure parts; `SpawnHandle`/`spawn_claude` land in Task 4)
- Modify: `server-rs/src/main.rs` (add `mod spawner;`)
- Test: inline `#[cfg(test)]`

**Interfaces (exact Rust signatures):**
```rust
use crate::runner::RunSpec;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    pub bin: String,                       // "claude" in prod, fake path in tests
    pub cwd: String,                       // the worktree
    pub prompt: String,
    pub env: HashMap<String, String>,      // overlay on top of the process env
    pub resume_session_id: Option<String>, // --resume <id> when set
    pub claude_config_dir: Option<String>, // per-session CLAUDE_CONFIG_DIR; None = clear it
    pub model: Option<String>,             // --model
    pub effort: Option<String>,            // --effort (omitted when ultracode)
    pub mode: Option<String>,              // "ultracode" => --settings {"ultracode":true}
    pub log_path: PathBuf,                 // claude stdout/stderr appended here; the tailer reads it
    pub unit: String,                      // transient-unit label (agentic-<id>)
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
}

pub const BASE_ARGS: [&str; 5] = [
    "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--dangerously-skip-permissions",
];
pub const POLL_MS: u64 = 120;
pub const OUTBOX_NOTE: &str = "[Delivering files to the user: …]"; // full literal, verbatim

pub fn compose_user_text(text: &str) -> String;          // OUTBOX_NOTE + "\n\n---\n\n" + text
pub fn encode_user_message(text: &str) -> String;        // {"type":"user","message":{"role":"user","content":text}}\n
pub fn build_spec(opts: &SpawnOptions) -> RunSpec;        // mirror of buildSpec
```

**Implementation:**
```rust
pub const OUTBOX_NOTE: &str =
    "[Delivering files to the user: this session runs in an app with no terminal. To give the user a \
file — a build artifact (e.g. an APK), a generated document, a report, an exported image — copy or \
write it into ./outbox/ (relative to your working directory; create the dir if needed). Files in \
./outbox/ are shown in the app to preview and download. Do this whenever the user asks you to send, \
give, share, or export a file. Do NOT put ordinary source-code edits in ./outbox/.]";

pub fn compose_user_text(text: &str) -> String { format!("{OUTBOX_NOTE}\n\n---\n\n{text}") }

pub fn encode_user_message(text: &str) -> String {
    serde_json::json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string() + "\n"
}

pub fn build_spec(opts: &SpawnOptions) -> RunSpec {
    let ultracode = opts.mode.as_deref() == Some("ultracode");
    let mut extra: Vec<String> = Vec::new();
    if let Some(m) = opts.model.as_deref().filter(|s| !s.is_empty()) { extra.push("--model".into()); extra.push(m.into()); }
    if let Some(e) = opts.effort.as_deref().filter(|s| !s.is_empty()) { if !ultracode { extra.push("--effort".into()); extra.push(e.into()); } }
    if ultracode { extra.push("--settings".into()); extra.push("{\"ultracode\":true}".into()); }

    // Merge process env, then opts.env, then set-or-clear CLAUDE_CONFIG_DIR.
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (k, v) in &opts.env { env.insert(k.clone(), v.clone()); }
    env.insert("CLAUDE_CONFIG_DIR".into(), opts.claude_config_dir.clone().unwrap_or_default());

    let mut args: Vec<String> = vec!["-p".into(), "--input-format".into(), "stream-json".into()];
    if let Some(id) = opts.resume_session_id.as_deref() { args.push("--resume".into()); args.push(id.into()); }
    args.extend(extra);
    args.extend(BASE_ARGS.iter().map(|s| s.to_string()));

    RunSpec {
        bin: opts.bin.clone(), args, cwd: opts.cwd.clone(), env,
        log_path: opts.log_path.clone(), unit: opts.unit.clone(),
        memory_max: opts.memory_max.clone(), memory_high: opts.memory_high.clone(),
        cpu_quota: opts.cpu_quota.clone(), tasks_max: opts.tasks_max.clone(),
        model: opts.model.clone(), effort: opts.effort.clone(), mode: opts.mode.clone(),
        resume_session_id: opts.resume_session_id.clone(),
    }
}
```
> **Parity note:** TS reads `opts.model` truthily (`if (opts.model)`), so an **empty string** model is treated as absent — the `.filter(|s| !s.is_empty())` reproduces that for `model` and `effort`. `mode` is matched literally against `"ultracode"`. The `extra` push order (model, effort, settings) and the `args` assembly order (`-p`, `--input-format stream-json`, resume, extra, BASE_ARGS) are exact.

**Steps:**
- [ ] **Step 1: write failing tests** for the pure helpers — mirror the argv/env assertions in `spawner.test.ts` by inspecting `build_spec(&opts).args` / `.env` directly (no process spawn needed for these), plus `compose_user_text`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SpawnOptions { SpawnOptions { bin: "claude".into(), cwd: "/tmp".into(), prompt: "x".into(),
        log_path: "/tmp/x.jsonl".into(), unit: "t".into(), ..Default::default() } }
    fn has_pair(args: &[String], a: &str, b: &str) -> bool {
        args.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    #[test]
    fn includes_resume_when_set_else_omits() {
        let mut o = opts(); o.resume_session_id = Some("sess-xyz".into());
        assert!(has_pair(&build_spec(&o).args, "--resume", "sess-xyz"));
        assert!(!build_spec(&opts()).args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn passes_model_and_effort_when_set_omits_otherwise() {
        let mut o = opts(); o.model = Some("opus".into()); o.effort = Some("high".into());
        let a = build_spec(&o).args;
        assert!(has_pair(&a, "--model", "opus"));
        assert!(has_pair(&a, "--effort", "high"));
        let a2 = build_spec(&opts()).args;
        assert!(!a2.iter().any(|x| x == "--model") && !a2.iter().any(|x| x == "--effort"));
    }

    #[test]
    fn ultracode_adds_settings_omits_effort_off_adds_neither() {
        let mut o = opts(); o.mode = Some("ultracode".into()); o.effort = Some("high".into());
        let a = build_spec(&o).args;
        assert!(has_pair(&a, "--settings", "{\"ultracode\":true}"));
        assert!(!a.iter().any(|x| x == "--effort")); // ultracode owns reasoning effort
        let a2 = build_spec(&opts()).args;
        assert!(!a2.iter().any(|x| x == "--settings") && !a2.iter().any(|x| x.contains("ultracode")));
    }

    #[test]
    fn always_has_input_format_stream_json_and_base_args() {
        let a = build_spec(&opts()).args;
        assert!(has_pair(&a, "--input-format", "stream-json"));
        assert_eq!(&a[0], "-p");
        assert!(has_pair(&a, "--output-format", "stream-json"));
        assert!(a.iter().any(|x| x == "--dangerously-skip-permissions"));
        assert!(!a.iter().any(|x| x == &opts().prompt)); // prompt is NOT an argv positional
    }

    #[test]
    fn sets_or_clears_claude_config_dir() {
        let mut o = opts(); o.claude_config_dir = Some("/tmp/my-cfg".into());
        assert_eq!(build_spec(&o).env.get("CLAUDE_CONFIG_DIR").unwrap(), "/tmp/my-cfg");
        assert_eq!(build_spec(&opts()).env.get("CLAUDE_CONFIG_DIR").unwrap(), ""); // cleared, not absent
    }

    #[test]
    fn compose_user_text_prepends_outbox_preamble() {
        let s = compose_user_text("do the thing");
        assert!(s.contains("Delivering files"));
        assert!(s.contains("do the thing"));
        assert!(s.contains("\n\n---\n\n"));
    }

    #[test]
    fn encode_user_message_is_one_ndjson_line() {
        let line = encode_user_message("hi");
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi");
    }
}
```
- [ ] **Step 2: run `cargo test --lib spawner::`, expect FAIL** (functions not yet implemented — stub them with `todo!()` / wrong bodies first so they fail to compile-or-assert; then write the real bodies).
- [ ] **Step 3: implement** `OUTBOX_NOTE`, `compose_user_text`, `encode_user_message`, `build_spec` per the code above. (`SpawnHandle`/`spawn_claude` come in Task 4.)
- [ ] **Step 4: run `cargo test --lib spawner::`, expect PASS.** Then full `cargo test` — no regressions.
- [ ] **Step 5: commit:**
```
git -C agentic-dev add server-rs/src/spawner.rs server-rs/src/main.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T3: spawner.rs — build_spec/compose_user_text/encode_user_message + BASE_ARGS/OUTBOX_NOTE

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `spawner.rs` — `SpawnHandle` + `spawn_claude` (poll loop, result-latch exit, kill/write/interrupt) (mirror `spawner.ts` + `spawner.test.ts`)

**Files:**
- Modify: `server-rs/src/spawner.rs` (add `SpawnHandle`, `spawn_claude`, `file_size`)
- Test: extend inline `#[cfg(test)]` with the `spawn_claude` integration cases

**Interfaces (exact Rust signatures):**
```rust
use crate::runner::{LocalRunner, RunHandle, Runner};
use crate::stream::ClaudeEvent;
use crate::tailer::EventTailer;
use tokio::sync::mpsc;

/// Drives a started turn: polls the tailer every POLL_MS, forwarding ClaudeEvents over `events`, and
/// emits the exit code over `exit` once the run goes inactive (0 iff a clean `result` was seen).
/// Mirror of TS `SpawnHandle` (EventEmitter → mpsc channels).
pub struct SpawnHandle {
    pub(crate) run: std::sync::Arc<dyn RunHandle>, // pub(crate) so the kill test can clone it
    pub events: mpsc::UnboundedReceiver<ClaudeEvent>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    poll_task: tokio::task::JoinHandle<()>,
    saw_result: std::sync::Arc<std::sync::atomic::AtomicBool>, // cleared by write(); set in the poll loop
}

impl SpawnHandle {
    pub fn kill(&self);                 // run.stop()
    pub fn write(&self, line: &str);    // reset saw_result latch (in the loop) + run.write
    pub fn end_input(&self);            // run.end_input
    pub fn interrupt(&self);            // run.interrupt OR stdin control_request
    pub fn detach(self);                // stop polling without killing the run
}

pub fn file_size(path: &std::path::Path) -> u64;
pub fn spawn_claude(opts: SpawnOptions, runner: &dyn Runner) -> SpawnHandle;
// convenience: pub fn spawn_claude_local(opts) -> SpawnHandle { spawn_claude(opts, &LocalRunner::new()) }
```

**Implementation notes (the loop, verbatim semantics):**
- The poll loop is a `tokio::spawn`ed task with a `tokio::time::interval(Duration::from_millis(POLL_MS))`. Each tick: `for ev in tailer.poll() { if let ClaudeEvent::Result { is_error: false, .. } = ev { saw_result = true } let _ = events_tx.send(ev); }` then `if !run.is_active() { finish }`. `finish` (run **once**): drain `tailer.poll()` **then** `tailer.flush()`, forward them (same `saw_result` update), send `if saw_result {0} else {1}` over the oneshot, break the loop.
- `write` must reset the latch. Because the latch lives in the poll task, share it as an `Arc<AtomicBool>` (`saw_result`) the loop reads/writes and `write()` clears **before** calling `run.write` — exactly TS `write(){ this.sawResult = false; this.run.write(line); }`. (TS keeps it as an instance field; the Rust equivalent is the shared atomic.)
- `run` is shared as `Arc<dyn RunHandle>` between the handle (for kill/write/interrupt) and the poll task (for `is_active`). `RunHandle: Send + Sync` makes that sound. `Runner::start` returns `Box<dyn RunHandle>`; convert with `Arc::from(box_handle)` (or change `start` to return `Arc<dyn RunHandle>` — pick one and keep it consistent; `Arc::from(Box<dyn T>)` works on stable).
- `interrupt`: `self.run.interrupt()` (the trait default is a no-op for `LocalRunner`, so it falls through to) — but to mirror TS's "if no native interrupt, write the control_request", expose `RunHandle::has_native_interrupt() -> bool { false }` (default) so `SpawnHandle::interrupt` can branch. `LocalRunner`'s handle returns `false` → `SpawnHandle::interrupt` writes `{"type":"control_request","request_id":format!("int-{}",now_ms()),"request":{"subtype":"interrupt"}}` (+`\n`) via `run.write`. (A future SDK runner would return `true` and implement `interrupt`.)
- `file_size(path) = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)`.
- `spawn_claude(opts, runner)`: `let run: Arc<dyn RunHandle> = Arc::from(runner.start(build_spec(&opts))); let tailer = EventTailer::new(&opts.log_path, file_size(&opts.log_path)); SpawnHandle::start(run, tailer)` where `start` wires the channels + spawns the poll task. **Tail start = current file size**, so a pre-written `agentic_prompt` marker arrives via backfill, not live (parity).

**Steps:**
- [ ] **Step 1: write failing tests** mirroring `spawner.test.ts` (`spawnClaude` describe block), driven by the real fixtures. These are `#[tokio::test]` async tests:
```rust
#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::runner::LocalRunner;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/test/fixtures").join(name).to_string_lossy().into_owned()
    }
    fn lp() -> PathBuf {
        std::env::temp_dir().join(format!("spawn-test-{}-{}.jsonl", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }
    async fn collect(mut h: SpawnHandle) -> (Vec<ClaudeEvent>, i32) {
        let mut events = Vec::new();
        let code = loop {
            tokio::select! {
                Some(ev) = h.events.recv() => events.push(ev),
                code = &mut h.exit => break code.unwrap_or(-1),
            }
        };
        // drain any events already queued alongside the exit
        while let Ok(ev) = h.events.try_recv() { events.push(ev); }
        (events, code)
    }

    #[tokio::test]
    async fn streams_parsed_events_and_exits_zero() {
        let o = SpawnOptions { bin: fixture("fake-claude.sh"), cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: "ignored".into(), log_path: lp(), unit: "t".into(), ..Default::default() };
        let (events, code) = collect(spawn_claude(o, &LocalRunner::new())).await;
        assert_eq!(code, 0);
        assert!(events.iter().any(|e| matches!(e, ClaudeEvent::Init { session_id, .. } if session_id == "fake-sess-123")));
        let text: String = events.iter().filter_map(|e| if let ClaudeEvent::Text { text, .. } = e { Some(text.as_str()) } else { None }).collect();
        assert_eq!(text, "Hello world");
        assert!(events.iter().any(|e| matches!(e, ClaudeEvent::Result { cost_usd: Some(c), .. } if (*c - 0.0042).abs() < 1e-9)));
    }

    #[tokio::test]
    async fn error_result_makes_turn_fail() {
        let o = SpawnOptions { bin: fixture("fake-claude-error.sh"), cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: "x".into(), log_path: lp(), unit: "t".into(), ..Default::default() };
        let (events, code) = collect(spawn_claude(o, &LocalRunner::new())).await;
        assert_ne!(code, 0); // error result → not counted as success
        assert!(events.iter().any(|e| matches!(e, ClaudeEvent::Result { is_error: true, text: Some(t), .. } if t.contains("session limit"))));
    }

    #[tokio::test]
    async fn kill_promptly_terminates_the_process_tree() {
        let mut env = std::collections::HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "5".into());
        let o = SpawnOptions { bin: fixture("fake-claude.sh"), cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: "x".into(), env, log_path: lp(), unit: "t".into(), ..Default::default() };
        let h = spawn_claude(o, &LocalRunner::new());
        let start = Instant::now();
        let killer = { let run = h.run.clone(); tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(100)).await; run.stop(); }) };
        let (_events, code) = collect(h).await;
        let _ = killer.await;
        assert_ne!(code, 0); // killed → no clean result → exit 1
        assert!(start.elapsed() < Duration::from_secs(2)); // not after the 5s sleep
    }

    #[tokio::test]
    async fn resume_appears_in_argv() {
        // mirror spawner.test.ts "--resume" using fake-claude-echoargs.sh
        let out = std::env::temp_dir().join(format!("argv-resume-{}.txt", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let mut env = std::collections::HashMap::new();
        env.insert("FAKE_ARGS_OUT".into(), out.to_string_lossy().into_owned());
        let o = SpawnOptions { bin: fixture("fake-claude-echoargs.sh"), cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: "follow up".into(), resume_session_id: Some("sess-xyz".into()), env, log_path: lp(), unit: "t".into(), ..Default::default() };
        let (_e, _c) = collect(spawn_claude(o, &LocalRunner::new())).await;
        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("--resume") && argv.contains("sess-xyz"));
    }

    #[tokio::test]
    async fn claude_config_dir_is_set_in_child_env() {
        let out = std::env::temp_dir().join(format!("env-{}.txt", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let mut env = std::collections::HashMap::new();
        env.insert("FAKE_ENV_OUT".into(), out.to_string_lossy().into_owned());
        let o = SpawnOptions { bin: fixture("fake-claude-echoenv.sh"), cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: "x".into(), claude_config_dir: Some("/tmp/my-cfg".into()), env, log_path: lp(), unit: "t".into(), ..Default::default() };
        let (_e, _c) = collect(spawn_claude(o, &LocalRunner::new())).await;
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "CLAUDE_CONFIG_DIR=/tmp/my-cfg");
    }
}
```
> Note: `collect()` reads `h.events`/`h.exit` (public fields); the kill test reads `h.run` (the `pub(crate) run` field on `SpawnHandle` above — both the test and the impl live in `spawner.rs`, so `pub(crate)` suffices). The struct fields are exactly `{ run, events, exit, poll_task, saw_result }` as defined in the Interfaces block.
- [ ] **Step 2: run `cargo test --lib spawner::handle_tests`, expect FAIL** (no `SpawnHandle`/`spawn_claude` yet → compile error, then once stubbed, assertion failures).
- [ ] **Step 3: implement** `file_size`, `SpawnHandle` (channels + poll task + `kill`/`write`/`end_input`/`interrupt`/`detach`), and `spawn_claude` per the notes. Sketch:
```rust
pub fn file_size(path: &std::path::Path) -> u64 { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) }

impl SpawnHandle {
    fn start(run: std::sync::Arc<dyn RunHandle>, mut tailer: EventTailer) -> SpawnHandle {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let saw_result = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loop_run = run.clone();
        let loop_saw = saw_result.clone();
        let poll_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(POLL_MS));
            let mut exit_tx = Some(exit_tx);
            loop {
                tick.tick().await;
                for ev in tailer.poll() {
                    if let ClaudeEvent::Result { is_error: false, .. } = ev { loop_saw.store(true, std::sync::atomic::Ordering::SeqCst); }
                    let _ = events_tx.send(ev);
                }
                if !loop_run.is_active() {
                    let mut tail: Vec<ClaudeEvent> = tailer.poll(); tail.extend(tailer.flush());
                    for ev in tail {
                        if let ClaudeEvent::Result { is_error: false, .. } = ev { loop_saw.store(true, std::sync::atomic::Ordering::SeqCst); }
                        let _ = events_tx.send(ev);
                    }
                    let code = if loop_saw.load(std::sync::atomic::Ordering::SeqCst) { 0 } else { 1 };
                    if let Some(tx) = exit_tx.take() { let _ = tx.send(code); }
                    break;
                }
            }
        });
        SpawnHandle { run, events: events_rx, exit: exit_rx, poll_task, saw_result }
    }
    pub fn kill(&self) { self.run.stop(); }
    pub fn write(&self, line: &str) { self.saw_result.store(false, std::sync::atomic::Ordering::SeqCst); self.run.write(line); }
    pub fn end_input(&self) { self.run.end_input(); }
    pub fn interrupt(&self) {
        if self.run.has_native_interrupt() { self.run.interrupt(); return; }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let req = serde_json::json!({ "type": "control_request", "request_id": format!("int-{now}"), "request": { "subtype": "interrupt" } }).to_string();
        self.run.write(&req);
    }
    pub fn detach(self) { self.poll_task.abort(); } // stop polling, don't kill the run
}

pub fn spawn_claude(opts: SpawnOptions, runner: &dyn Runner) -> SpawnHandle {
    let run: std::sync::Arc<dyn RunHandle> = std::sync::Arc::from(runner.start(build_spec(&opts)));
    let tailer = EventTailer::new(&opts.log_path, file_size(&opts.log_path));
    SpawnHandle::start(run, tailer)
}
```
> Add `saw_result: std::sync::Arc<std::sync::atomic::AtomicBool>` to the `SpawnHandle` struct and `fn has_native_interrupt(&self) -> bool { false }` to the `RunHandle` trait (default impl), so `LocalHandle` inherits `false`. **Rust feature note:** `Arc::from(Box<dyn RunHandle>)` reuses the existing allocation to make a shared, ref-counted handle — needed because both the handle and the poll task hold `run`. The poll task is a detached green thread (`tokio::spawn`); `detach()` aborts it (TS `clearInterval`), `kill()` stops the underlying child (TS `run.stop()`).
- [ ] **Step 4: run `cargo test --lib spawner::`, expect PASS** (pure + handle tests). Then full `cargo test` — no regressions. (If the `kill` test is flaky on a loaded machine, the 2 s bound has 50× margin over `POLL_MS`; do not loosen the result-latch logic to make it pass.)
- [ ] **Step 5: commit:**
```
git -C agentic-dev add server-rs/src/spawner.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T4: spawner.rs — SpawnHandle + spawn_claude (poll loop, result-latch exit, kill/write/interrupt)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `worktree.rs` — git worktree create/remove/sync/diff/discard (mirror `worktree.ts` + `worktree.test.ts`)

**Files:**
- Create: `server-rs/src/worktree.rs`
- Modify: `server-rs/src/main.rs` (add `mod worktree;`)
- Test: inline `#[cfg(test)]` (uses a real temp git repo, like `worktree.test.ts`)

**Interfaces (exact Rust signatures):**
```rust
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct WorktreeInfo { pub worktree_path: PathBuf, pub branch: String }

#[derive(Clone, Debug)]
pub struct SessionWorktree { pub repo: String, pub worktree_path: PathBuf, pub base_sha: String }

#[derive(thiserror::Error, Debug)]
pub enum WorktreeError {
    #[error("git failed: {0}")] Git(String),
    #[error("io error: {0}")] Io(#[from] std::io::Error),
}

pub fn create_worktree(repo_path: &Path, root: &Path, repo: &str, id: &str) -> Result<WorktreeInfo, WorktreeError>;
pub fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<(), WorktreeError>;
pub fn create_session_worktrees(repo_specs: &[(String, PathBuf)], root: &Path, id: &str) -> Result<Vec<SessionWorktree>, WorktreeError>;
pub fn remove_session_worktrees(repo_specs: &[(String, PathBuf)], session_dir: &Path);
pub async fn sync_worktree(worktree_path: &Path);                 // best-effort, never errors out
pub async fn diff_worktree(worktree_path: &Path, base_sha: &str) -> Result<String, WorktreeError>;
pub fn discard_worktree(repo_path: &Path, worktree_path: &Path, branch: &str) -> Result<(), WorktreeError>;
```

**Implementation notes:**
- `GIT_TIMEOUT_MS = 30_000`. Synchronous git (create/remove/discard) uses `std::process::Command::output()`; the network-bound async ones (`sync_worktree`, `diff_worktree`) use `tokio::process::Command` wrapped in `tokio::time::timeout(Duration::from_millis(GIT_TIMEOUT_MS), …)`.
- Helper `fn git_sync(args: &[&str]) -> Result<String, WorktreeError>`: run `git` with `args`, on non-zero status return `Err(Git(stderr))`, else `Ok(stdout_string)`. `repo_path`/`worktree_path` are passed via explicit `-C <path>` args (parity with TS — never via `current_dir`, so the assertions match argv exactly).
- `create_session_worktrees`: `mkdir -p root/id`; per repo: `base_sha = git_sync(["-C", repo_path, "rev-parse", "HEAD"])?.trim()`; `git_sync(["-C", repo_path, "worktree", "add", worktree_path, "-b", branch])?`.
- `remove_session_worktrees`: per repo, `let _ = git_sync(["-C", repo_path, "worktree", "remove", "--force", wt]); let _ = git_sync(["-C", repo_path, "worktree", "prune"]);` (errors swallowed — idempotent).
- `sync_worktree`: as the Global Constraint spells out; every git call is `git_async(worktree_path, args).await` and all errors are caught → fall to `rebase --abort` (also ignored). Returns `()` always.
- `diff_worktree`: `git_async(wt, &["add", "-AN"]).await?; let out = git_async(wt, &["--no-pager", "diff", base_sha]).await?; Ok(out)`.
- `discard_worktree`: `if worktree_path.exists() { git_sync(["-C", repo_path, "worktree", "remove", "--force", worktree_path])?; } else { git_sync(["-C", repo_path, "worktree", "prune"])?; } let _ = git_sync(["-C", repo_path, "branch", "-D", branch]);`.

**Steps:**
- [ ] **Step 1: write failing tests** mirroring `worktree.test.ts`. Build a throwaway git repo in a temp dir (init, config user, one commit), then exercise create → file edit → diff → discard. (Mirror only what `worktree.test.ts` asserts; if that file is large, cover: create makes the branch+path; `diff_worktree` shows an edit; `discard_worktree` removes path+branch idempotently; second `discard` does not error.)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {:?} failed", args);
    }
    fn temp_repo() -> (PathBuf, PathBuf) { // (root, repo_path)
        let root = std::env::temp_dir().join(format!("agentic-wt-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "master"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        (root, repo)
    }

    #[tokio::test]
    async fn create_diff_discard_roundtrip() {
        let (root, repo) = temp_repo();
        let wts = create_session_worktrees(&[("repo".into(), repo.clone())], &root, "sess1").unwrap();
        let wt = &wts[0];
        assert!(wt.worktree_path.exists());
        assert_eq!(wt.repo, "repo");
        // layout: <root>/<id>/<repo> (parity with createSessionWorktrees)
        assert_eq!(wt.worktree_path, root.join("sess1").join("repo"));
        // base_sha is a git hex sha (mirror worktree.test.ts /^[0-9a-f]{7,}$/)
        assert!(wt.base_sha.len() >= 7 && wt.base_sha.chars().all(|c| c.is_ascii_hexdigit()));
        // the session branch exists in the repo
        let branches = String::from_utf8(Command::new("git").arg("-C").arg(&repo)
            .args(["branch", "--list", "agentic/sess1"]).output().unwrap().stdout).unwrap();
        assert!(branches.contains("agentic/sess1"));
        // edit a tracked file + add an untracked one
        std::fs::write(wt.worktree_path.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.worktree_path.join("new.txt"), "hi\n").unwrap();
        let diff = diff_worktree(&wt.worktree_path, &wt.base_sha).await.unwrap();
        assert!(diff.contains("a.txt") && diff.contains("+two"));
        assert!(diff.contains("new.txt")); // -AN surfaces the untracked file
        discard_worktree(&repo, &wt.worktree_path, &format!("agentic/{}", "sess1")).unwrap();
        assert!(!wt.worktree_path.exists());
        // idempotent: a second discard must not error
        discard_worktree(&repo, &wt.worktree_path, &format!("agentic/{}", "sess1")).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sync_worktree_never_panics_without_remote() {
        let (root, repo) = temp_repo();
        let wts = create_session_worktrees(&[("repo".into(), repo.clone())], &root, "sess2").unwrap();
        // no origin remote → must fall through cleanly, not error
        sync_worktree(&wts[0].worktree_path).await;
        std::fs::remove_dir_all(&root).ok();
    }
}
```
- [ ] **Step 2: run `cargo test --lib worktree::`, expect FAIL** (functions unimplemented).
- [ ] **Step 3: implement** all worktree functions + the `git_sync`/`git_async` helpers per the notes.
- [ ] **Step 4: run `cargo test --lib worktree::`, expect PASS.** Then full `cargo test` — no regressions.
- [ ] **Step 5: commit:**
```
git -C agentic-dev add server-rs/src/worktree.rs server-rs/src/main.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T5: worktree.rs — session worktree create/remove/sync/diff/discard (git via process)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `claude_config.rs` — `build_session_config_dir` (mirror `claudeConfig.ts` + `claudeConfig.test.ts`)

**Files:**
- Create: `server-rs/src/claude_config.rs`
- Modify: `server-rs/src/main.rs` (add `mod claude_config;`)
- Test: inline `#[cfg(test)]`

**Interfaces (exact Rust signatures):**
```rust
use std::path::{Path, PathBuf};

/// Items symlinked verbatim from the real ~/.claude so the session keeps auth, global CLAUDE.md,
/// settings, plugins, and memory. Only skills/ is curated. Verbatim from claudeConfig.ts.
const LINKED: [&str; 6] = [".credentials.json", "CLAUDE.md", "settings.json", "settings.local.json", "plugins", "memory"];

/// Build a per-session CLAUDE config dir for CLAUDE_CONFIG_DIR. Symlinks the shared config + a curated
/// skills/. Returns dest_dir. Idempotent (rebuilds the symlinks + skills/, preserves projects/).
pub fn build_session_config_dir(base_claude_dir: &Path, chosen_skills: &[String], dest_dir: &Path) -> std::io::Result<PathBuf>;

/// A skill name must be a single concrete dir name (reject ".", "..", "/", and non-[A-Za-z0-9_.-]).
fn valid_skill_name(name: &str) -> bool;
```

**Implementation:**
```rust
use std::os::unix::fs::symlink;

fn valid_skill_name(name: &str) -> bool {
    name != "." && !name.contains("..") && !name.contains('/')
        && !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

pub fn build_session_config_dir(base_claude_dir: &Path, chosen_skills: &[String], dest_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    // Refresh the symlinked shared config (safe to recreate each turn).
    for item in LINKED {
        let dst = dest_dir.join(item);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dst); // in case it became a dir
        let src = base_claude_dir.join(item);
        if src.exists() { symlink(&src, &dst)?; }
    }
    // Refresh skills/ to exactly the chosen set. Do NOT wipe dest_dir wholesale — claude's projects/
    // transcript must survive for --resume.
    let skills_dir = dest_dir.join("skills");
    let _ = std::fs::remove_dir_all(&skills_dir);
    std::fs::create_dir_all(&skills_dir)?;
    for name in chosen_skills {
        if !valid_skill_name(name) { continue; }
        let src = base_claude_dir.join("skills").join(name);
        if src.exists() { symlink(&src, skills_dir.join(name))?; }
    }
    Ok(dest_dir.to_path_buf())
}
```
> **Parity note:** `existsSync` in TS follows symlinks; `Path::exists()` does too — matched. `rmSync(dst, {recursive,force})` removes a file OR dir OR symlink; the two `remove_file`/`remove_dir_all` (both ignoring errors) cover the same cases. The rejection set `name == "." || name.includes("..") || name.includes("/") || !/^[A-Za-z0-9_.-]+$/` maps to `valid_skill_name` exactly (note `..` is caught by the substring test before the charset test, same as TS). Unix-only `symlink` matches the platform the server runs on (the TS `symlinkSync` is likewise effectively Unix here).

**Steps:**
- [ ] **Step 1: write failing tests** mirroring `claudeConfig.test.ts` — set up a fake base `~/.claude` (a `CLAUDE.md`, a `skills/` with two skill dirs), call `build_session_config_dir`, assert: `LINKED` items that exist become symlinks; `skills/<chosen>` symlinks exist for valid chosen names; a crafted bad name (`"../evil"`, `"."`, `"a/b"`) is **skipped** (no symlink, no panic); a `projects/` file written into `dest_dir` survives a second build (resume safety).
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-cfg-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap(); d
    }

    #[test]
    fn links_shared_config_and_curated_skills_skipping_bad_names() {
        let root = tmp();
        let base = root.join("claude");
        std::fs::create_dir_all(base.join("skills").join("good")).unwrap();
        std::fs::create_dir_all(base.join("skills").join("alsogood")).unwrap();
        std::fs::write(base.join("CLAUDE.md"), "g").unwrap();
        let dest = root.join("session-cfg");
        let chosen = vec!["good".to_string(), "alsogood".to_string(), "../evil".to_string(), ".".to_string(), "a/b".to_string(), "missing".to_string()];
        build_session_config_dir(&base, &chosen, &dest).unwrap();

        assert!(std::fs::symlink_metadata(dest.join("CLAUDE.md")).unwrap().file_type().is_symlink());
        assert!(dest.join("skills").join("good").exists());
        assert!(dest.join("skills").join("alsogood").exists());
        assert!(!dest.join("skills").join("evil").exists());       // bad name skipped
        assert!(!dest.join("skills").join("missing").exists());    // no source → skipped
        // a LINKED item with no source must NOT create a symlink
        assert!(std::fs::symlink_metadata(dest.join("settings.json")).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn preserves_projects_across_rebuild() {
        let root = tmp();
        let base = root.join("claude");
        std::fs::create_dir_all(&base).unwrap();
        let dest = root.join("session-cfg");
        build_session_config_dir(&base, &[], &dest).unwrap();
        std::fs::create_dir_all(dest.join("projects")).unwrap();
        std::fs::write(dest.join("projects").join("transcript.jsonl"), "x").unwrap();
        // a follow-up turn rebuilds the config dir — the transcript must survive
        build_session_config_dir(&base, &["new".to_string()], &dest).unwrap();
        assert!(dest.join("projects").join("transcript.jsonl").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn valid_skill_name_rules() {
        assert!(valid_skill_name("good-skill_1.2"));
        assert!(!valid_skill_name("."));
        assert!(!valid_skill_name("../x"));
        assert!(!valid_skill_name("a/b"));
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("bad name")); // space not in charset
    }
}
```
- [ ] **Step 2: run `cargo test --lib claude_config::`, expect FAIL** (function unimplemented).
- [ ] **Step 3: implement** `valid_skill_name` + `build_session_config_dir` per the code above.
- [ ] **Step 4: run `cargo test --lib claude_config::`, expect PASS.** Then full `cargo test` from `server-rs/` — confirm the **whole** crate is green (api/auth/config/store/transcript/stream/runner/tailer/spawner/worktree all pass).
- [ ] **Step 5: commit:**
```
git -C agentic-dev add server-rs/src/claude_config.rs server-rs/src/main.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 3 T6: claude_config.rs — build_session_config_dir (symlinked shared config + curated skills)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Definition of done

- Five new modules exist and are registered in `main.rs` (alphabetical): `runner`, `tailer`, `spawner`, `worktree`, `claude_config`.
- `runner.rs`: `RunSpec`, `RunHandle` trait, `Runner` trait, `LocalRunner` (tokio/std process, process-group kill, stdin pipe). Mirrors `runner.ts`; `runner.test.ts` cases (`runs to the log file … exit 0`, `stop() terminates a slow run`) pass via the fake-claude fixture.
- `tailer.rs`: `EventTailer` with byte-offset incremental reads + carry buffer + flush. Every `tailer.test.ts` case mirrored and passing.
- `spawner.rs`: `BASE_ARGS`/`POLL_MS`/`OUTBOX_NOTE` verbatim; `build_spec`, `compose_user_text`, `encode_user_message`, `file_size`, `SpawnHandle`, `spawn_claude`. Every `spawner.test.ts` case (stream+exit0, error→fail, kill, --resume/--model/--effort/--settings, CLAUDE_CONFIG_DIR, composeUserText) mirrored and passing. Exit code is decided by the clean-`result` latch, never the OS status. The prompt rides on stdin, not argv. `CLAUDE_CONFIG_DIR` is always set-or-cleared.
- `worktree.rs`: `create_session_worktrees`/`remove_session_worktrees`/`sync_worktree`/`diff_worktree`/`discard_worktree` (+ single-repo `create_worktree`/`remove_worktree`). Git shelled via process with explicit `-C`, 30 s timeout on network-bound calls, `sync_worktree` best-effort (never errors), `diff_worktree` uses `add -AN`. Round-trip + no-remote-sync tests pass.
- `claude_config.rs`: `build_session_config_dir` symlinks `LINKED` items that exist + a curated `skills/`, skips invalid skill names, preserves `projects/` across rebuilds. Tests pass.
- `cargo test` (whole crate) is green. **No new crate dependencies** (`Cargo.toml` unchanged). Six commits (T1–T6); none pushed.
- Out of scope (deferred): `sdkRunner` (SDK Agent responder), engine queue/pump/subscribe/watchdog/recover/structured-logging (Phase 4), HTTP wiring of spawn/diff/discard + WS (Phase 5), push/usage/misc (Phase 6).
