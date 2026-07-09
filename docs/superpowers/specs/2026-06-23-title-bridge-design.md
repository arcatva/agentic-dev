# Title/Retitle via SDK bridge — design

Date: 2026-06-23
Status: draft (awaiting user review)
Repo: `agentic-dev` (server-rs)
Supersedes: the prior title_bin + `claude -p` CLI design (still on master; this spec
is a rewrite, not a small fix).

## Problem

The previous design for session-title generation spawned a `claude -p
--model haiku --max-turns 1` subprocess (`server-rs/src/engine/title.rs`,
gated by `EngineConfig.title_bin`). Manual measurement shows this is
~14s cold and a single CLI invocation regularly exceeds the 5s
timeout, so the existing retitle path is silently falling back to the
user's raw prompt. The right path is the same one the main turn
runner uses: the Node SDK bridge (`server-rs/sdk-bridge.mjs`), which
the engine already drives per turn via `engine::sdk_runner::SdkRunner`.

## Goal

`submit_session` should kick off (asynchronously) a haiku call through
the SDK bridge that summarises the user's first prompt into a 5–12
character Chinese title, and write the result to `sessions.prompt`
via `SessionPatch`. Every 5 user messages, the same bridge should be
asked whether the current title still fits and — if not — rewrite it.
Both calls share the bridge's `query()` warmup and auth path.

## Non-goals

- Periodic retitle at any cadence other than every 5 user messages.
- A separate "rename" UI in the Android client (the existing
  `setTitle=true` API call is the escape hatch; not used by humans today).
- Title changes for sessions that are already running on the previous
  binary; only new sessions (and existing sessions' next retitle
  trigger) pick up the new behaviour.
- Refactoring the SDK bridge into a separate package; the
  `if (mode === "title")` early-exit branch is small and stays in the
  same file.
- Removing `LocalRunner` (the `claude -p` test seam). It still has
  callers in `spawner.rs` and `engine/tests.rs` that are out of scope.

## Design

### Bridge mode

`sdk-bridge.mjs` reads `SDK_BRIDGE_MODE`. New values:

- `title` — read a single user message from stdin (a single JSON line
  in the same shape the main bridge accepts), run a one-shot haiku
  query with a fixed system prompt, write the model output to
  stdout, and `process.exit(0)`. No log file, no resume, no
  AskUserQuestion support, no `query()` loop.
- `retitle` — read two fields from a single JSON line on stdin:
  `currentTitle` and a `messages` array. Run a one-shot haiku query
  whose system prompt asks the model to either keep the title
  (`{"change": false}`) or propose a new one (`{"change": true,
  "title": "..."}`). Write the parsed JSON to stdout, exit 0.
- anything else (including absent) — current behaviour, unchanged.

The mode is selected by `engine/title.rs` via a new env var
`SDK_BRIDGE_MODE=title|retitle` (alongside the existing
`SDK_BRIDGE_LOG`/`SDK_BRIDGE_CWD`).

### Rust-side spawner

A new tiny module `server-rs/src/engine/title_bridge.rs` exposes:

```rust
pub struct TitleBridge {
    node_bin: String,
    bridge_path: String,
}

impl TitleBridge {
    pub async fn generate(
        &self,
        prompt: &str,
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError>;

    pub async fn maybe_retitle(
        &self,
        current_title: &str,
        recent_messages: &[(String, String)],
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError>;
}
```

Both functions spawn `node sdk-bridge.mjs` with `SDK_BRIDGE_MODE` set
accordingly. The child process inherits the process env (so
`ANTHROPIC_API_KEY` / OAuth credentials in the agentic-dev env reach
the SDK) plus the small overlay of `SDK_BRIDGE_*` vars. The child is
short-lived; the call awaits its exit. Timeout: 20s. On timeout,
non-zero exit, empty stdout, or stdout that fails the existing
`is_valid_title` / JSON parse rules, the function returns `Ok(None)`
(silent fallback per the prior design).

For test isolation, `TitleBridge::with_node(node, bridge)` lets tests
inject a fake `node` binary (a shell script) that mimics the bridge
contract on argv + stdin + stdout. This replaces all 8
`fake-claude-title-*.sh` fixtures.

### Wiring

- `EngineConfig` gains `title_bridge: Arc<TitleBridge>` (cloned into
  inner state, mirroring how the main runner is held). The old
  `title_bin: String` field is **deleted** along with
  `Config.title_bin` and `AGENTIC_TITLE_BIN`.
- `Engine::new` constructs the bridge from `EngineConfig.title_bridge`
  (which is populated by `Config::load` from `AGENTIC_SDK_BRIDGE` env
  or defaults to `<repo>/server-rs/sdk-bridge.mjs`).
- `submit_session` `tokio::spawn`s a task that calls
  `title_bridge.generate(prompt, &session_dir)`, then on
  `Ok(Some(t))` writes `SessionPatch { prompt: Some(t) }`. Failures
  are silently dropped.
- `follow_up` keeps the existing `act.turns += 1` line; when the new
  count is a multiple of 5 and `cfg.retitle_enabled` is true, it
  `tokio::spawn`s a task that reads the last 10 messages from the
  session log (existing `Store::read_log` + a `parse_recent_messages`
  helper that stays in `title.rs` and is pure-data, no subprocess)
  and calls `title_bridge.maybe_retitle`.

### Deleted surface

The following, all introduced in the previous spec and now redundant:

- `server-rs/src/engine/title.rs::generate_title` (the
  `claude -p`-spawning function)
- `server-rs/src/engine/title.rs::TITLE_SYSTEM_PROMPT`
- `server-rs/src/engine/title.rs::maybe_retitle` (the
  `claude -p`-spawning retitle)
- `server-rs/src/engine/title.rs::RETITLE_SYSTEM_PROMPT`
- `EngineConfig.title_bin`
- `Config.title_bin`
- `AGENTIC_TITLE_BIN` env var
- All 8 fixtures in `server-rs/tests/fixtures/fake-claude-title*.sh`
- The integration tests in `engine/tests.rs::submit_titles_session`
  and `engine/tests.rs::retitle_session` (rewritten against the new
  fake-node approach below)

### What stays

- `is_valid_title` (pure-data, no subprocess) — kept in `title.rs` and
  reused by both `TitleBridge` paths.
- `parse_recent_messages` (pure-data) — kept in `title.rs`.
- `Engine::maybe_retitle_session` and the `retitle_enabled` config
  gate — kept; only the subprocess call inside the spawned task
  changes.
- The follow_up `setTitle` default flip from the prior spec.

### Testing

Tests use four small fake-node shell scripts at
`server-rs/tests/fixtures/fake-sdk-bridge-{ok,slow,error,garbage}.sh`.
Each inspects `SDK_BRIDGE_MODE` on argv and a single stdin line, and
prints one of the canonical stdout shapes or errors:

- `fake-sdk-bridge-ok.sh` — `mode=title` → prints `性能优化阶段` to
  stdout and exits 0. `mode=retitle` → prints
  `{"change":true,"title":"性能优化阶段"}` to stdout and exits 0.
- `fake-sdk-bridge-slow.sh` — sleeps 30s, then prints the same
  payload as the OK fake. Forces the 20s timeout in Rust.
- `fake-sdk-bridge-error.sh` — exits 1 with no stdout, regardless
  of mode.
- `fake-sdk-bridge-garbage.sh` — prints `not json at all` to stdout
  and exits 0, regardless of mode.

The Rust side of these tests uses `TitleBridge::with_node(fake,
real_bridge_path)` and asserts return values. The existing 282 lib
tests must continue to pass; the new tests are additive.

## Risks

- **SDK bridge one-shot overhead**: each title call forks a Node
  process. Cold start is comparable to the main turn runner
  (~hundreds of ms) and well under the 20s timeout. No
  pre-warming is needed.
- **Spawn path complexity**: `TitleBridge` introduces a new
  spawn-from-Rust pattern. The existing `SdkRunner` and `LocalRunner`
  remain; the new module is small (~120 lines) and isolated.
- **Bridge contract drift**: if the main bridge changes its
  message-input shape, the title branch could silently break. The
  spec includes a unit test on the bridge's input parser so the
  shape is captured in code.
- **Existing in-flight sessions**: sessions created before the new
  binary are not retroactively titled. They will get a title on
  the next periodic retitle trigger (5th user message) — at
  which point the bridge call is fire-and-forget and the user
  notices nothing.

## Out of scope

- Removing the `LocalRunner` (`claude -p` test seam) entirely.
- Refactoring `sdk-bridge.mjs` into a package.
- Any client-side change.
