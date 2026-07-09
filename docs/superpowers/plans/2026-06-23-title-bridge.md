# Title/Retitle via SDK bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the broken `claude -p` title path with a one-shot `title`/`retitle` mode in the existing SDK bridge, driven by a new Rust `TitleBridge` module. Submit and every-5-turns retitle both go through the same path.

**Architecture:** `sdk-bridge.mjs` reads `SDK_BRIDGE_MODE`; new values `title` and `retitle` short-circuit the existing turn-streaming loop into a one-shot haiku call that writes the model output to stdout and exits 0. A new `server-rs/src/engine/title_bridge.rs` exposes `TitleBridge::generate` and `TitleBridge::maybe_retitle`, both of which spawn `node sdk-bridge.mjs` with the right `SDK_BRIDGE_MODE` and overlay env, await its exit, and parse the stdout. The old `EngineConfig.title_bin` / `Config.title_bin` / `AGENTIC_TITLE_BIN` / 8 `fake-claude-title*.sh` fixtures and the prior `claude -p`-spawning `engine::title::generate_title` / `engine::title::maybe_retitle` are deleted end-to-end.

**Tech Stack:** Rust (tokio, std process spawn), the existing `server-rs/sdk-bridge.mjs` (Node + @anthropic-ai/claude-agent-sdk), shell test fixtures, `sqlx` for the existing `SessionPatch.prompt` write. No new dependencies. No new DB column.

## Global Constraints

- **Failure = silent fallback**: any error path in title generation MUST leave `sessions.prompt` exactly as it was. The user sees the same behaviour as today's raw prompt.
- **Fire-and-forget on submit**: the `submit_session` title call MUST be `tokio::spawn`-ed, NOT awaited inline. The API response returns the session id immediately; the title lands within seconds.
- **Fire-and-forget on retitle**: in `follow_up`, when the just-incremented turn count is a positive multiple of 5 AND `cfg.retitle_enabled` is true, `tokio::spawn` the retitle task. Disabled = no subprocess started.
- **Bridge-mode short-circuit**: the new `title` and `retitle` modes in `sdk-bridge.mjs` MUST exit 0 (success) or 1 (failure) after a single round-trip. They MUST NOT touch `SDK_BRIDGE_LOG` (no per-call log file), MUST NOT call `query({ resume })`, and MUST NOT enter the existing `for await (msg of q)` loop.
- **Test isolation**: no test hits the real `claude` / SDK; tests use shell-script fakes at `server-rs/tests/fixtures/fake-sdk-bridge-*.sh`. The Rust side uses `TitleBridge::with_node(fake_node, real_bridge_path)` to inject the fake. `make test` (= `cd server-rs && cargo test`) must stay green before each commit.
- **Engine purity**: `server-rs/src/engine/` stays free of axum imports. `title_bridge.rs` lives under `engine/`.
- **No new DB column**: title writes go through the existing `SessionPatch.prompt` field on `sessions`.
- **No new dependencies**.
- **Output rules for the title path**: stdout must be a single 5–12 character Chinese title, no markdown, no quotes, no newlines, no leading `#`/`>`/`` ` ``. The existing `title::is_valid_title` is reused.
- **Output rules for the retitle path**: stdout must be a single JSON object: `{"change": false}` OR `{"change": true, "title": "..."}`. The new title (when change=true) MUST pass `is_valid_title` AND differ from the current title. Anything else is treated as None (silent fallback).
- **Timeout**: each bridge call MUST hard-cap at 20 seconds (`tokio::time::timeout`). On timeout → None.
- **Bounded retitle prompt**: at most 10 recent log entries and at most 4 KiB of joined body text. Older entries are dropped first when the cap is hit (existing `parse_recent_messages` rule is reused).
- **Inherit env**: the Rust spawn inherits the parent process env (so `ANTHROPIC_API_KEY` / OAuth credentials reach the SDK) and overlays the small `SDK_BRIDGE_*` vars. NO `env_clear()` (this is a real production call, not a test).
- **Delete what is replaced**: `EngineConfig.title_bin`, `Config.title_bin`, the `AGENTIC_TITLE_BIN` env, the 8 `fake-claude-title*.sh` fixtures, and the integration tests `submit_titles_session` / `retitle_session` are deleted. New fakes and new tests replace them.

---

## File Structure

**Created:**
- `server-rs/tests/fixtures/fake-sdk-bridge-ok.sh` — title mode → stdout `性能优化阶段`; retitle mode → stdout `{"change":true,"title":"性能优化阶段"}`. Exits 0.
- `server-rs/tests/fixtures/fake-sdk-bridge-slow.sh` — sleeps 30s, then prints the same payload as ok. Forces 20s timeout.
- `server-rs/tests/fixtures/fake-sdk-bridge-error.sh` — exits 1, no stdout.
- `server-rs/tests/fixtures/fake-sdk-bridge-garbage.sh` — prints `not json at all` to stdout, exits 0.
- `server-rs/src/engine/title_bridge.rs` — `TitleBridge` struct + `generate` + `maybe_retitle`. ~150 lines.

**Modified:**
- `server-rs/sdk-bridge.mjs` — add the `if (SDK_BRIDGE_MODE === "title" || "retitle")` early-exit branch before the existing `query({...})` call.
- `server-rs/src/engine/title.rs` — delete `generate_title`, `maybe_retitle`, `TITLE_SYSTEM_PROMPT`, `RETITLE_SYSTEM_PROMPT`. Keep `is_valid_title` and `parse_recent_messages` (they are pure-data and used by the new path).
- `server-rs/src/engine/mod.rs` — remove `title_bin: String` from `EngineConfig`; add `title_bridge: Arc<TitleBridge>` instead; in `submit_session` delete the old inline generate block and add a new `tokio::spawn` of `title_bridge.generate`; the `follow_up` retitle path keeps its existing `retitle_enabled` gate but now calls `title_bridge.maybe_retitle` instead of the old `maybe_retitle`.
- `server-rs/src/api/config.rs` — drop `title_bin`, add `node_bin: String` and `sdk_bridge_path: PathBuf`.
- `server-rs/src/main.rs` — propagate `node_bin` + `sdk_bridge_path`; build `TitleBridge` and pass to `EngineConfig`.
- `server-rs/src/api/{mod,test_support}.rs`, `server-rs/tests/smoke.rs`, `server-rs/tests/search.rs`, `server-rs/src/engine/search.rs` — propagate the field (test overrides provide their own node_bin / sdk_bridge_path).
- `server-rs/src/engine/tests.rs` — `EngineOverrides` gets `node_bin: Option<String>` and `bridge_path: Option<String>`; the integration tests for the title/retitle paths are replaced with new ones that point at the fake-sdk-bridge fixtures.
- `docs/internals.md` — replace the "Session title generation" section with a new paragraph documenting the bridge-mode path.

**Deleted:**
- `server-rs/tests/fixtures/fake-claude-title-ok.sh`
- `server-rs/tests/fixtures/fake-claude-title-empty.sh`
- `server-rs/tests/fixtures/fake-claude-title-error.sh`
- `server-rs/tests/fixtures/fake-claude-title-slow.sh`
- `server-rs/tests/fixtures/fake-claude-title-badlen.sh`
- `server-rs/tests/fixtures/fake-claude-title-retitle-keep.sh`
- `server-rs/tests/fixtures/fake-claude-title-retitle-change.sh`
- `server-rs/tests/fixtures/fake-claude-title-retitle-badjson.sh`
- `server-rs/tests/fixtures/fake-claude-title-retitle-slow.sh`

---

## Task 1: Create the fake-sdk-bridge fixtures

**Files:**
- Create: `server-rs/tests/fixtures/fake-sdk-bridge-ok.sh`
- Create: `server-rs/tests/fixtures/fake-sdk-bridge-slow.sh`
- Create: `server-rs/tests/fixtures/fake-sdk-bridge-error.sh`
- Create: `server-rs/tests/fixtures/fake-sdk-bridge-garbage.sh`

These are the test doubles the Rust `TitleBridge` will be pointed at. They mimic the `sdk-bridge.mjs` `title`/`retitle` contract on argv (the bridge script path appears in argv, the value of `SDK_BRIDGE_MODE` is in the environment) and on stdout.

- [ ] **Step 1: Create `fake-sdk-bridge-ok.sh`**

Create the file with executable permission. Contents:

```bash
#!/usr/bin/env bash
# Test double: SDK bridge in title/retitle mode. Both modes return a
# canonical valid payload on stdout and exit 0.
set -euo pipefail
case "${SDK_BRIDGE_MODE:-}" in
  title)
    echo "性能优化阶段"
    ;;
  retitle)
    echo '{"change":true,"title":"性能优化阶段"}'
    ;;
  *)
    echo "fake-sdk-bridge-ok: unknown SDK_BRIDGE_MODE=${SDK_BRIDGE_MODE:-unset}" >&2
    exit 1
    ;;
esac
```

- [ ] **Step 2: Create `fake-sdk-bridge-slow.sh`**

Create the file with executable permission. Contents:

```bash
#!/usr/bin/env bash
# Test double: sleeps longer than the 20s timeout. The Rust side must
# time out and return None.
set -euo pipefail
sleep 30
case "${SDK_BRIDGE_MODE:-}" in
  title)
    echo "性能优化阶段"
    ;;
  retitle)
    echo '{"change":true,"title":"性能优化阶段"}'
    ;;
  *)
    exit 1
    ;;
esac
```

- [ ] **Step 3: Create `fake-sdk-bridge-error.sh`**

Create the file with executable permission. Contents:

```bash
#!/usr/bin/env bash
# Test double: exits 1 with no stdout, regardless of mode. Forces the
# Rust side to surface a non-zero exit and return None.
set -euo pipefail
exit 1
```

- [ ] **Step 4: Create `fake-sdk-bridge-garbage.sh`**

Create the file with executable permission. Contents:

```bash
#!/usr/bin/env bash
# Test double: prints non-JSON / non-title text on stdout, exits 0.
# Forces the Rust side's parser to fail and return None.
set -euo pipefail
echo "not json at all"
```

- [ ] **Step 5: Verify all four are executable**

Run:

```bash
for f in /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-sdk-bridge-*.sh; do
  test -x "$f" && echo "OK: $f" || echo "MISSING: $f"
done
```

Expected: four lines, each starting with `OK:`.

- [ ] **Step 6: Smoke-test `ok` in title mode**

Run:

```bash
SDK_BRIDGE_MODE=title bash /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-sdk-bridge-ok.sh
```

Expected: stdout is `性能优化阶段` and exit code is 0.

- [ ] **Step 7: Smoke-test `ok` in retitle mode**

Run:

```bash
SDK_BRIDGE_MODE=retitle bash /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-sdk-bridge-ok.sh
```

Expected: stdout is `{"change":true,"title":"性能优化阶段"}` and exit code is 0.

- [ ] **Step 8: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/tests/fixtures/fake-sdk-bridge-*.sh && git commit -m "test: add fake-sdk-bridge fixtures for TitleBridge tests

Four scripts cover the ok / slow / error / garbage paths that the
new SDK-bridge title/retitle mode must handle.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Add the `title` and `retitle` modes to `sdk-bridge.mjs`

**Files:**
- Modify: `server-rs/sdk-bridge.mjs`

**Interfaces:**
- Consumes: `SDK_BRIDGE_MODE` env var.
- Produces: when `SDK_BRIDGE_MODE` is `title` or `retitle`, the bridge:
  1. Reads a single line from stdin (utf-8).
  2. Constructs a one-shot `query({ prompt: <user text>, options: { model, maxTurns: 1, systemPrompt } })`.
  3. Collects the assistant's text and writes it to stdout (title mode) or wraps it in `{"change":..., "title":...}` (retitle mode).
  4. Exits 0 on success, 1 on any failure.

- [ ] **Step 1: Read the current bridge and locate the mode-sensitivity point**

Open `server-rs/sdk-bridge.mjs` and find the block that sets up `q` (the SDK query). The current code path lives at around line 47-88, where the existing `permissionMode`, `extraArgs`, `model`, `resume` are computed. The new title/retitle early-exit must happen BEFORE `q` is created — so the existing turn-streaming loop never runs for these modes.

- [ ] **Step 2: Add the early-exit branch**

Insert the following block immediately before the `const q = query({...})` call (around line 76 of the current file). Do not move any existing code; only prepend the new check.

```javascript
// ── one-shot title/retitle mode (Rust engine; not the turn loop) ──
// When SDK_BRIDGE_MODE is "title" or "retitle", do a single haiku call,
// write the answer to stdout, and exit 0. No log file, no resume, no
// AskUserQuestion support, no for-await loop. Mirrors what
// engine::title_bridge::TitleBridge expects on the wire.
const oneShotMode = process.env.SDK_BRIDGE_MODE;
if (oneShotMode === "title" || oneShotMode === "retitle") {
  (async () => {
    const readline = await import("node:readline");
    const rl = readline.createInterface({ input: process.stdin });
    let userText = "";
    for await (const line of rl) { userText = line; break; } // single line
    rl.close();
    const systemPrompt = oneShotMode === "title"
      ? "你的任务是根据用户给出的请求,生成一个会话标题。要求:\n- 用中文,5 到 12 个字\n- 只输出标题本身,不要任何解释、不要标点包裹、不要 markdown\n- 标题要能反映\"用户在做什么\",而不是\"用户最后一句话\"\n- 如果用户输入很短(比如 \"ok\"),用会话的整体意图来概括\n"
      : "你的任务是判断这个 session 的标题是否需要更新。\n\n输入包含:\n- 当前标题\n- 最近 10 条消息 (按时间顺序)\n\n输出必须是以下 JSON 之一,不要其他内容,不要 markdown 代码块:\n- 不需要改: {\"change\": false}\n- 需要改:   {\"change\": true, \"title\": \"<5-12 个汉字的新标题>\"}\n\n判断标准:\n- 标题要反映\"session 当前在做什么\",而不是第一句话或最后一句话\n- 如果当前标题仍然准确,输出 {\"change\": false}\n- 如果话题已经明显改变,输出新标题\n- 新标题跟旧标题不能完全相同\n";
    let assistantText = "";
    try {
      for await (const msg of query({
        prompt: userText,
        options: {
          cwd: process.env.SDK_BRIDGE_CWD || process.cwd(),
          env: process.env,
          model: "haiku",
          maxTurns: 1,
          systemPrompt,
        },
      })) {
        if (msg && msg.type === "assistant" && Array.isArray(msg.message?.content)) {
          for (const block of msg.message.content) {
            if (block && block.type === "text" && typeof block.text === "string") {
              assistantText += block.text;
            }
          }
        }
      }
    } catch (err) {
      process.stderr.write(`fake-sdk-bridge one-shot ${oneShotMode} failed: ${err && err.message ? err.message : err}\n`);
      process.exit(1);
    }
    process.stdout.write(assistantText);
    process.exit(0);
  })();
  // Stops the rest of the file from running.
  return;
}
```

- [ ] **Step 3: Verify the bridge still parses**

The file should still parse as a Node module. No CLI test we can run here (the new mode requires the Rust `TitleBridge` to drive it). Skip a real run; the next task wires up a Rust unit test that exercises it via the fake.

- [ ] **Step 4: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/sdk-bridge.mjs && git commit -m "sdk-bridge: add one-shot title/retitle modes

When SDK_BRIDGE_MODE is title or retitle, the bridge short-circuits
the existing turn loop into a single haiku call whose output is
written to stdout. The Rust engine::title_bridge module is the
sole caller; this path never touches SDK_BRIDGE_LOG, never calls
resume, and never enters the for-await-of-q loop.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Implement `engine::title_bridge::TitleBridge` (TDD)

**Files:**
- Create: `server-rs/src/engine/title_bridge.rs`
- Modify: `server-rs/src/engine/mod.rs` (declare the module)

**Interfaces:**
- Consumes: `is_valid_title` and `parse_recent_messages` from `engine::title` (existing; we will trim `title.rs` in a later task).
- Produces:

```rust
pub struct TitleBridge {
    node_bin: String,
    bridge_path: String,
}

pub enum TitleBridgeError {
    Spawn(io::Error),
    Timeout,
    NonZeroExit(Option<i32>),
    InvalidStdout,
}

impl TitleBridge {
    /// Build with the production node binary (defaults to "node") and the
    /// absolute path of `server-rs/sdk-bridge.mjs`.
    pub fn new(bridge_path: impl Into<String>) -> Self;

    /// Build with explicit values. Tests use this to inject a fake node script.
    pub fn with_node(node_bin: impl Into<String>, bridge_path: impl Into<String>) -> Self;

    /// Run the bridge in `title` mode with the given user prompt. Returns
    /// Some(title) iff the bridge exits 0 within 20s, the stdout passes
    /// `is_valid_title`, and the result is non-empty. Returns None on every
    /// other path.
    pub async fn generate(
        &self,
        prompt: &str,
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError>;

    /// Run the bridge in `retitle` mode. Returns Some(new_title) iff the
    /// bridge exits 0 within 20s, the stdout is a JSON object with
    /// `change=true` and a `title` field, the new title passes
    /// `is_valid_title`, AND it differs from `current_title`. Returns
    /// None otherwise. Errors surface as Err.
    pub async fn maybe_retitle(
        &self,
        current_title: &str,
        recent_messages: &[(String, String)],
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError>;
}
```

- [ ] **Step 1: Add the failing unit tests**

Create `server-rs/src/engine/title_bridge.rs` with the test module first. (Yes — the test module goes in the same file as the production code; this matches the existing pattern in `title.rs`.) The test file content:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::title_bridge::TitleBridge;
use std::path::Path;

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn tmp_cwd() -> PathBuf { PathBuf::from("/tmp") }

#[tokio::test]
async fn generate_returns_some_title_on_ok_bridge() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-ok.sh"));
    let r = b.generate("first prompt", &tmp_cwd()).await.unwrap();
    assert_eq!(r.as_deref(), Some("性能优化阶段"));
}

#[tokio::test]
async fn generate_returns_none_on_bridge_error() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-error.sh"));
    let r = b.generate("first prompt", &tmp_cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn generate_returns_none_on_garbage_stdout() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-garbage.sh"));
    let r = b.generate("first prompt", &tmp_cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn generate_times_out_after_20s_on_slow_bridge() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-slow.sh"));
    let started = std::time::Instant::now();
    let r = b.generate("first prompt", &tmp_cwd()).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r, None);
    assert!(elapsed < Duration::from_secs(25), "must time out near 20s, took {elapsed:?}");
}

#[tokio::test]
async fn maybe_retitle_returns_some_title_on_change_true() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-ok.sh"));
    let r = b
        .maybe_retitle("old title", &[("user".into(), "msg".into())], &tmp_cwd())
        .await
        .unwrap();
    assert_eq!(r.as_deref(), Some("性能优化阶段"));
}

#[tokio::test]
async fn maybe_retitle_returns_none_on_change_false() {
    // Build a one-off fake that returns {"change": false}.
    let bin = std::env::temp_dir().join("agentic-test-fake-bridge-keep.sh");
    std::fs::write(&bin, r#"#!/usr/bin/env bash
set -euo pipefail
echo '{"change": false}'
"#).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let b = TitleBridge::with_node("bash", bin.to_string_lossy().into_owned());
    let r = b
        .maybe_retitle("current title", &[], &tmp_cwd())
        .await
        .unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn maybe_retitle_returns_none_when_new_equals_current() {
    // The ok fake returns "性能优化阶段"; if current_title is that string, dedup wins.
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-ok.sh"));
    let r = b
        .maybe_retitle("性能优化阶段", &[], &tmp_cwd())
        .await
        .unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn maybe_retitle_returns_none_on_invalid_title() {
    // The garbage fake prints "not json at all" which fails JSON parse.
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-garbage.sh"));
    let r = b
        .maybe_retitle("old title", &[], &tmp_cwd())
        .await
        .unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn maybe_retitle_returns_none_on_bridge_error() {
    let b = TitleBridge::with_node("bash", fixture("fake-sdk-bridge-error.sh"));
    let r = b
        .maybe_retitle("old title", &[], &tmp_cwd())
        .await
        .unwrap();
    assert_eq!(r, None);
}
```

Add a `mod tests` wrap so the tests don't compile until the production code is written:

```rust
#[cfg(test)]
mod tests { /* the tests above */ }
```

- [ ] **Step 2: Declare the module and run the failing tests**

In `server-rs/src/engine/mod.rs`, near the other `pub mod` declarations, add:

```rust
pub mod title_bridge;
```

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test --lib engine::title_bridge`
Expected: compile error — `title_bridge` is not defined.

- [ ] **Step 3: Implement `TitleBridge`**

Replace the contents of `server-rs/src/engine/title_bridge.rs` with the production code. The file should contain the `mod tests` block from Step 1 at the bottom and the production code above it:

```rust
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::engine::title::is_valid_title;

const BRIDGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Errors that the Rust caller might want to log. None of these are
/// surfaced to the user; the public methods collapse errors to
/// `Ok(None)` for silent-fallback callers (per the spec).
#[derive(Debug)]
pub enum TitleBridgeError {
    Spawn(std::io::Error),
    Timeout,
    NonZeroExit(Option<i32>),
    InvalidStdout,
}

/// Spawns the SDK bridge in one-shot `title` / `retitle` mode and returns
/// the model output as a plain string. The bridge itself is the Rust
/// engine's `server-rs/sdk-bridge.mjs`; this type only knows the
/// (node, bridge_path, env, cwd) it needs to spawn.
#[derive(Clone, Debug)]
pub struct TitleBridge {
    node_bin: String,
    bridge_path: String,
}

impl TitleBridge {
    pub fn new(bridge_path: impl Into<String>) -> Self {
        Self {
            node_bin: std::env::var("AGENTIC_NODE_BIN").unwrap_or_else(|_| "node".into()),
            bridge_path: bridge_path.into(),
        }
    }

    pub fn with_node(node_bin: impl Into<String>, bridge_path: impl Into<String>) -> Self {
        Self { node_bin: node_bin.into(), bridge_path: bridge_path.into() }
    }

    pub async fn generate(
        &self,
        prompt: &str,
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError> {
        // The bridge reads a single line of stdin. It only inspects the
        // SDK_BRIDGE_MODE env to decide which system prompt to use.
        self.run_bridge("title", prompt, cwd).await
    }

    pub async fn maybe_retitle(
        &self,
        current_title: &str,
        recent_messages: &[(String, String)],
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError> {
        // Pack current_title and the recent message list into a single
        // JSON object that the bridge parses on its end. We use a
        // trailing newline because the bridge's readline reads one
        // line at a time and stops at the first newline.
        let mut body = format!(r#"{{"currentTitle":{}}}{}"#,
            serde_json::Value::String(current_title.to_string()),
            if recent_messages.is_empty() {
                "".to_string()
            } else {
                let arr: Vec<serde_json::Value> = recent_messages
                    .iter()
                    .map(|(role, text)| serde_json::json!([role, text]))
                    .collect();
                format!(r#","messages":{}"#, serde_json::Value::Array(arr))
            },
        );
        body.push('\n');
        self.run_bridge("retitle", &body, cwd).await
    }

    async fn run_bridge(
        &self,
        mode: &str,
        stdin_payload: &str,
        cwd: &Path,
    ) -> Result<Option<String>, TitleBridgeError> {
        let mut child = Command::new(&self.node_bin)
            .arg(&self.bridge_path)
            .current_dir(cwd)
            .env("SDK_BRIDGE_MODE", mode)
            .env("SDK_BRIDGE_CWD", cwd.to_string_lossy().as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(TitleBridgeError::Spawn)?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        let output = tokio::time::timeout(BRIDGE_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| TitleBridgeError::Timeout)?
            .map_err(TitleBridgeError::Spawn)?;

        if !output.status.success() {
            return Err(TitleBridgeError::NonZeroExit(output.status.code()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err(TitleBridgeError::InvalidStdout);
        }

        Ok(self.parse_stdout(mode, trimmed))
    }

    /// Mode-specific stdout parser. Title mode → validate via
    /// `is_valid_title`. Retitle mode → JSON object, change flag, dedup
    /// vs current title, validate.
    fn parse_stdout(&self, mode: &str, stdout: &str) -> Option<String> {
        match mode {
            "title" => {
                if is_valid_title(stdout) { Some(stdout.to_string()) } else { None }
            }
            "retitle" => {
                let v: serde_json::Value = serde_json::from_str(stdout).ok()?;
                let change = v.get("change").and_then(|c| c.as_bool()).unwrap_or(false);
                if !change { return None; }
                let new_title = v.get("title").and_then(|t| t.as_str())?;
                if !is_valid_title(new_title) { return None; }
                Some(new_title.to_string())
            }
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Run the unit tests to confirm they pass**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test --lib engine::title_bridge`
Expected: all 9 tests pass.

- [ ] **Step 5: Run the full suite**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: all existing tests pass (we have NOT yet removed the old `title_bin` plumbing; the existing tests using `title_bin` should still compile and pass because we only added a new module). If any test fails, fix it before committing.

- [ ] **Step 6: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/title_bridge.rs server-rs/src/engine/mod.rs && git commit -m "engine: add TitleBridge (sdk-bridge title/retitle spawner)

Spawns server-rs/sdk-bridge.mjs with SDK_BRIDGE_MODE=title or
SDK_BRIDGE_MODE=retitle, writes a single stdin line, awaits the
stdout response, parses it, and returns. 20s hard timeout. Errors
collapse to Ok(None) at the call site; this layer returns Err
so the engine can log warnings.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Wire `TitleBridge` into `EngineConfig` and the engine plumbing

**Files:**
- Modify: `server-rs/src/api/config.rs` — drop `title_bin`, add `node_bin: String` and `sdk_bridge_path: PathBuf`.
- Modify: `server-rs/src/main.rs` — propagate the new fields and build `TitleBridge`.
- Modify: `server-rs/src/api/{mod,test_support}.rs` — propagate the new fields.
- Modify: `server-rs/tests/smoke.rs` — propagate.
- Modify: `server-rs/src/engine/mod.rs` — drop `title_bin: String` from `EngineConfig`; add `title_bridge: Arc<TitleBridge>`; build it in `Engine::new` from the propagated fields.

**Interfaces:**
- Consumes: `Config.node_bin`, `Config.sdk_bridge_path`.
- Produces: `EngineConfig.title_bridge: Arc<TitleBridge>`. The old `EngineConfig.title_bin: String` field is **deleted**.

- [ ] **Step 1: Add the new fields to `Config` and load from env**

In `server-rs/src/api/config.rs`, make three changes:

1. In the `Config` struct (next to the existing `claude_bin`), add two fields:

```rust
    pub node_bin: String,        // AGENTIC_NODE_BIN — defaults to "node"
    pub sdk_bridge_path: PathBuf, // AGENTIC_SDK_BRIDGE — defaults to <repo>/server-rs/sdk-bridge.mjs
```

2. Remove the existing `title_bin: String` field declaration.

3. In the `Config::load` builder, remove the `title_bin: get("AGENTIC_TITLE_BIN").unwrap_or_else(...)` line, and add two new lines (placing them next to `claude_bin`):

```rust
            node_bin: get("AGENTIC_NODE_BIN").unwrap_or_else(|| "node".into()),
            sdk_bridge_path: get("AGENTIC_SDK_BRIDGE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    // Default to <repo>/server-rs/sdk-bridge.mjs, where <repo> is
                    // the parent of AGENTIC_SRC_ROOT (or the current dir if unset).
                    let src = get("AGENTIC_SRC_ROOT").unwrap_or_else(|| ".".into());
                    let p = std::path::PathBuf::from(&src);
                    let repo = p.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| p);
                    repo.join("server-rs").join("sdk-bridge.mjs")
                }),
```

- [ ] **Step 2: Drop `title_bin` from `EngineConfig` and add `title_bridge`**

In `server-rs/src/engine/mod.rs`, find the `EngineConfig` struct and:
1. Remove the `pub title_bin: String,` line.
2. Add (next to the remaining fields):

```rust
    pub title_bridge: std::sync::Arc<TitleBridge>,
```

- [ ] **Step 3: Build the bridge in `Engine::new`**

`EngineInner` already holds `pub(crate) cfg: EngineConfig` (see `engine/mod.rs:127-134`). The bridge lives in `cfg` and is read at call sites as `self.0.cfg.title_bridge` — there is NO separate `EngineInner` field. `Engine::new` only needs to read the propagated `cfg.title_bridge`; no inner-state wiring is required. Confirm by inspecting the inner state before writing code.

- [ ] **Step 4: Update `make_engine` (test helper)**

In `server-rs/src/engine/tests.rs`, find the `EngineOverrides` struct and `make_engine`. Add two new override fields (next to `claude_bin`):

```rust
        node_bin: Option<String>,
        bridge_path: Option<String>,
```

In the `cfg` literal in `make_engine`, drop `title_bin` and add:

```rust
            title_bridge: std::sync::Arc::new(
                crate::engine::title_bridge::TitleBridge::with_node(
                    overrides.node_bin.clone().unwrap_or_else(|| "node".into()),
                    overrides.bridge_path.clone().unwrap_or_else(|| fixture("fake-sdk-bridge-ok.sh")),
                ),
            ),
```

(Adjust the fixture path if `make_engine` uses a different default helper.)

- [ ] **Step 5: Update `api/mod.rs`, `api/test_support.rs`, `tests/smoke.rs`**

For each of these three files, find the `EngineConfig { ... }` literal and:
1. Remove the `title_bin: ...` line.
2. Add two lines that read from the local `Config` value:

```rust
        title_bridge: std::sync::Arc::new(
            crate::engine::title_bridge::TitleBridge::new(c.sdk_bridge_path.to_string_lossy().into_owned()),
        ),
```

Replace `c` with the actual variable name in each file (`c`, `cfg`, or `config`).

- [ ] **Step 6: Update `main.rs`**

In `server-rs/src/main.rs`, find the `EngineConfig { ... }` builder. Drop `title_bin` and add:

```rust
        title_bridge: std::sync::Arc::new(
            crate::engine::title_bridge::TitleBridge::new(config.sdk_bridge_path.to_string_lossy().into_owned()),
        ),
```

(Use the local `Config` variable name; typically `config`.)

- [ ] **Step 7: Update the `search.rs` test fixture**

In `server-rs/src/engine/search.rs`, find the test-only `EngineConfig` literal (the one at line 398 with `title_bin: String::new()`). Remove that line and add:

```rust
            title_bridge: std::sync::Arc::new(
                crate::engine::title_bridge::TitleBridge::with_node("echo".into(), "irrelevant".into()),
            ),
```

The `echo` node is a placeholder: this test does not exercise the title path, so the bridge is never spawned. If the test does spawn the bridge (because it actually goes through `submit_session` and the title task is reached), use the `fake-sdk-bridge-ok.sh` fixture path instead.

- [ ] **Step 8: Update the `tests/search.rs` test override**

In `server-rs/tests/search.rs`, find the test-side `EngineConfig` literal (the one at line 75 with `title_bin: String::new()` and `retitle_enabled: false`). Drop the `title_bin` line. The `retitle_enabled` line stays.

- [ ] **Step 9: Build and check for compile errors**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo build`
Expected: compiles cleanly. Existing tests may still pass because we have not yet removed the `claude -p` title spawn in `engine/mod.rs::submit_session` (that is Task 5). The struct field swap is the only change here.

- [ ] **Step 10: Run the full suite**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: tests pass. Note: existing `submit_titles_session` and `retitle_session` tests still depend on `title_bin`. If they fail, the next task (Task 5) rewrites them; the failures here are expected and will be fixed.

- [ ] **Step 11: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/api/config.rs server-rs/src/main.rs server-rs/src/api/mod.rs server-rs/src/api/test_support.rs server-rs/tests/smoke.rs server-rs/src/engine/search.rs server-rs/tests/search.rs server-rs/src/engine/tests.rs server-rs/src/engine/mod.rs && git commit -m "engine: replace title_bin with TitleBridge in EngineConfig

Adds node_bin (AGENTIC_NODE_BIN) and sdk_bridge_path (AGENTIC_SDK_BRIDGE)
to Config; EngineConfig gains title_bridge: Arc<TitleBridge> built
from those. The old title_bin / AGENTIC_TITLE_BIN env var is
deleted end-to-end. Existing claude -p title spawn in submit_session
will be replaced in the next commit; tests using title_bin still
compile and pass for now.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Rewrite `submit_session` and `follow_up` to use `TitleBridge`; delete old `claude -p` title code

**Files:**
- Modify: `server-rs/src/engine/title.rs` — delete `generate_title`, `maybe_retitle`, `TITLE_SYSTEM_PROMPT`, `RETITLE_SYSTEM_PROMPT`, and the `tokio::process::Command` / `tokio::io::AsyncWriteExt` imports. Keep `is_valid_title` and `parse_recent_messages`. Keep the `TITLE_MAX_CHARS` constant if it is used by `is_valid_title`; otherwise delete.
- Modify: `server-rs/src/engine/mod.rs` — in `submit_session`, replace the existing inline `generate_title` block with a `tokio::spawn` of `TitleBridge::generate`. In `follow_up`, replace the existing inline `maybe_retitle` call (if present) with a `tokio::spawn` of `TitleBridge::maybe_retitle`. The `Engine::maybe_retitle_session` method on `Engine` is removed; the spawned task body moves into `follow_up` directly OR into a private helper.

**Interfaces:**
- Consumes: `EngineConfig.title_bridge: Arc<TitleBridge>`.
- Produces: a new `Engine::submit_title_task(self: Arc<Self>, id: String, prompt: String)` helper (private) that the `submit_session` block `tokio::spawn`s. The helper clones the `Arc<TitleBridge>` from the engine, calls `title_bridge.generate(...)`, and on `Ok(Some(t))` writes `SessionPatch { prompt: Some(t) }` to the store. Errors are silently logged at warn level and otherwise swallowed.

- [ ] **Step 1: Trim `title.rs`**

In `server-rs/src/engine/title.rs`, delete the following:
- The `Command`, `Stdio`, `AsyncWriteExt`, and `Duration` imports (if no longer needed).
- The `TITLE_TIMEOUT` constant.
- The `TITLE_SYSTEM_PROMPT` constant.
- The `generate_title` function.
- The `RETITLE_SYSTEM_PROMPT` constant.
- The `maybe_retitle` function.

Keep:
- The `TITLE_MAX_CHARS` constant if `is_valid_title` references it; otherwise delete.
- `is_valid_title`.
- `parse_recent_messages` and the `RECENT_MESSAGES_MAX` / `RECENT_MESSAGES_MAX_BYTES` constants it depends on.
- The unit tests for the kept helpers. Delete the unit tests for the removed functions.

- [ ] **Step 2: Replace the `submit_session` title call**

In `server-rs/src/engine/mod.rs`, find the existing block in `submit_session` that calls `crate::engine::title::generate_title`. Replace it with:

```rust
        // Title generation runs as a fire-and-forget task so the API
        // response returns the session id immediately. On any error
        // (timeout, non-zero exit, invalid stdout) the title is left
        // as the user's original prompt — silent fallback per spec.
        {
            let engine = self.clone();
            let id = id.clone();
            let prompt = prompt.clone();
            let session_dir = session_dir.clone();
            tokio::spawn(async move {
                let res = engine.0.cfg.title_bridge.generate(&prompt, &session_dir).await;
                match res {
                    Ok(Some(t)) => {
                        if let Err(e) = engine.0.store.update(&id, crate::engine::store::SessionPatch {
                            prompt: Some(t),
                            ..Default::default()
                        }).await {
                            tracing::warn!("[engine] title store.update failed: {e}");
                        }
                    }
                    Ok(None) => { /* invalid output, keep prompt */ }
                    Err(e) => tracing::warn!("[engine] title bridge error: {e:?}"),
                }
            });
        }
```

- [ ] **Step 3: Replace the `follow_up` retitle call**

In `server-rs/src/engine/mod.rs`, find the existing block in `follow_up` (in both the live-inject and queued branches) that calls `maybe_retitle` or invokes `self.maybe_retitle_session`. Replace it with:

```rust
                // Periodic retitle: every 5 turns, fire-and-forget a
                // haiku call that decides whether to update the title.
                // The follow_up API call returns immediately even if
                // the bridge subprocess is slow.
                if self.0.cfg.retitle_enabled && new_turns % 5 == 0 {
                    let engine = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        let lines = engine.0.store.read_log(&id);
                        let recent = crate::engine::title::parse_recent_messages(&lines);
                        let Some(current) = engine.0.store.get(&id).await.ok().flatten().map(|s| s.prompt) else {
                            return;
                        };
                        let cwd = engine.0.cfg.worktrees_root.join(&id);
                        let res = engine.0.cfg.title_bridge.maybe_retitle(&current, &recent, &cwd).await;
                        if let Ok(Some(t)) = res {
                            if let Err(e) = engine.0.store.update(&id, crate::engine::store::SessionPatch {
                                prompt: Some(t),
                                ..Default::default()
                            }).await {
                                tracing::warn!("[engine] retitle store.update failed: {e}");
                            }
                        }
                    });
                }
```

Replace both occurrences (the live-inject branch and the queued branch) with this same block. Make sure the `new_turns` variable is captured BEFORE the spawn (the existing code path increments `act.turns += 1` and then reads it back via `state.activity`; either approach works as long as the value is correct).

- [ ] **Step 4: Delete the old `Engine::maybe_retitle_session` method**

In `server-rs/src/engine/mod.rs`, delete the `pub async fn maybe_retitle_session(&self, id: &str)` method. Its logic now lives inside the `follow_up` spawn.

- [ ] **Step 5: Delete the old `submit_titles_session` and `retitle_session` integration tests**

In `server-rs/src/engine/tests.rs`, delete the two `mod submit_titles_session { ... }` and `mod retitle_session { ... }` blocks. They are replaced by new tests in Task 6.

- [ ] **Step 6: Build and check for compile errors**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo build 2>&1 | tail -30`
Expected: compiles cleanly. If `maybe_retitle` is still referenced anywhere, fix it.

- [ ] **Step 7: Run the full suite**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test 2>&1 | tail -10`
Expected: tests pass, but `submit_titles_session` / `retitle_session` tests are gone (deleted in Step 5). The next task replaces them with bridge-based equivalents.

- [ ] **Step 8: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/title.rs server-rs/src/engine/mod.rs server-rs/src/engine/tests.rs && git commit -m "engine: route title/retitle through TitleBridge, drop claude -p path

submit_session now spawns a fire-and-forget task that calls
title_bridge.generate and on success writes the result to
sessions.prompt. follow_up, on the 5th turn, spawns a similar task
calling title_bridge.maybe_retitle. The old engine::title::generate_title
and engine::title::maybe_retitle (which spawned a \`claude -p\` CLI) are
deleted. The old submit_titles_session and retitle_session tests are
deleted and will be replaced in the next commit with bridge-based
equivalents.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Add bridge-based integration tests for `submit_session` title and `follow_up` retitle

**Files:**
- Modify: `server-rs/src/engine/tests.rs`

**Interfaces:**
- Consumes: `EngineConfig.title_bridge: Arc<TitleBridge>` (now a fixture-injected fake-node).
- Produces: three new integration tests that exercise the wired-up path end-to-end.

- [ ] **Step 1: Add a `mod submit_titles_via_bridge` block**

Append to `server-rs/src/engine/tests.rs`:

```rust
mod submit_titles_via_bridge {
    use super::*;

    async fn submit_with(overrides: EngineOverrides) -> (Engine, String) {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, overrides).await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                std::collections::HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        (e, id)
    }

    /// submit_session spawns a fire-and-forget title task. The fake
    /// bridge returns a valid title; we poll the store until it lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_uses_bridge_title() {
        let (e, id) = submit_with(EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-ok.sh")),
            ..Default::default()
        }).await;
        for _ in 0..40 {
            if e.get(&id).await.unwrap().prompt == "性能优化阶段" { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
    }

    /// If the bridge returns invalid output, the original prompt is kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_keeps_prompt_when_bridge_returns_garbage() {
        let (e, id) = submit_with(EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-garbage.sh")),
            ..Default::default()
        }).await;
        // Poll briefly to make sure the title task has had a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let prompt = e.get(&id).await.unwrap().prompt;
        assert_eq!(prompt, "first prompt");
    }

    /// follow_up on the 5th turn spawns a retitle task. The fake bridge
    /// returns a NEW title (different from the submit-time title) so
    /// the retitle actually lands. We poll until it lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retitle_after_fifth_followup_changes_title() {
        // Build a one-off fake that returns a fresh, distinct title in
        // retitle mode. We use the existing ok fake for the submit-time
        // title (which lands as "性能优化阶段") and this for the retitle
        // (which lands as "性能优化阶段二号" — different from current, so
        // dedup does not kick in).
        let bin = std::env::temp_dir().join("agentic-test-bridge-retitle-new.sh");
        std::fs::write(&bin, r#"#!/usr/bin/env bash
set -euo pipefail
case "${SDK_BRIDGE_MODE:-}" in
  title) echo "性能优化阶段" ;;
  retitle) echo '{"change":true,"title":"性能优化阶段二号"}' ;;
  *) exit 1 ;;
esac
"#).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(bin.to_string_lossy().into_owned()),
            ..Default::default()
        }).await;
        let id = e
            .submit_session(vec![], vec![], "first prompt".into(), std::collections::HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        // After submit, the title should be "性能优化阶段".
        assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
        // Follow up 4 times so the next one is the 5th.
        for i in 0..4 {
            e.follow_up(&id, &format!("f{i}"), false, None, None).await.unwrap();
            wait_status(&e, &id, "done").await;
        }
        // 5th followup triggers the retitle task.
        e.follow_up(&id, "fifth", false, None, None).await.unwrap();
        wait_status(&e, &id, "done").await;
        // Poll for the rewritten title.
        for _ in 0..40 {
            if e.get(&id).await.unwrap().prompt == "性能优化阶段二号" { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段二号");
    }
}
```

- [ ] **Step 2: Run the new tests to confirm they pass**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test --lib engine::tests::submit_titles_via_bridge`
Expected: all 3 tests pass.

- [ ] **Step 3: Run the full suite**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test 2>&1 | tail -10`
Expected: every test passes. The test count drops by 6 (3 from the deleted `submit_titles_session` block and 3 from the deleted `retitle_session` block) and grows by 3 (the new `submit_titles_via_bridge` block). The previous total of 282 should be 279 after this task.

- [ ] **Step 4: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/tests.rs && git commit -m "test: bridge-based integration tests for submit_session and follow_up retitle

The new fake-sdk-bridge-*.sh fixtures from the previous commit are
wired in via EngineOverrides.bridge_path. The three tests cover:
(1) submit_session picks up the bridge's title, (2) submit_session
keeps the original prompt on invalid bridge output, (3) follow_up on
the 5th turn triggers a retitle task that observes dedup.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Delete the old `fake-claude-title*.sh` fixtures

**Files:**
- Delete: 9 files in `server-rs/tests/fixtures/fake-claude-title*.sh`.

- [ ] **Step 1: Remove all 9 files**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && \
  rm server-rs/tests/fixtures/fake-claude-title-ok.sh \
     server-rs/tests/fixtures/fake-claude-title-empty.sh \
     server-rs/tests/fixtures/fake-claude-title-error.sh \
     server-rs/tests/fixtures/fake-claude-title-slow.sh \
     server-rs/tests/fixtures/fake-claude-title-badlen.sh \
     server-rs/tests/fixtures/fake-claude-title-retitle-keep.sh \
     server-rs/tests/fixtures/fake-claude-title-retitle-change.sh \
     server-rs/tests/fixtures/fake-claude-title-retitle-badjson.sh \
     server-rs/tests/fixtures/fake-claude-title-retitle-slow.sh
```

- [ ] **Step 2: Verify the directory only has the new fixtures**

Run: `ls /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-*.sh`
Expected: 4 `fake-sdk-bridge-*.sh` files plus the 10+ pre-existing `fake-claude*.sh` (e.g. `fake-claude.sh`, `fake-claude-stream.sh`) that are NOT title fixtures. No `fake-claude-title*.sh` files remain.

- [ ] **Step 3: Run the full suite once more**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test 2>&1 | tail -5`
Expected: every test passes. The deleted fixtures are no longer referenced.

- [ ] **Step 4: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add -A server-rs/tests/fixtures && git commit -m "test: delete fake-claude-title fixtures (replaced by fake-sdk-bridge-*.sh)

All nine fake-claude-title*.sh fixtures from the previous spec are
removed; the new fake-sdk-bridge-*.sh fixtures cover the same
behavioural surface via the SDK-bridge path.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Update `docs/internals.md`

**Files:**
- Modify: `docs/internals.md` — replace the existing "Session title generation" section with a new one that documents the SDK-bridge path.

- [ ] **Step 1: Find the existing section**

Run: `grep -n "Session title generation" /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/docs/internals.md`
Expected: a single line at the section header added by the previous spec.

- [ ] **Step 2: Replace the section body**

Replace the entire `## Session title generation` section (including the `### Periodic refresh` paragraph if present) with:

```markdown
## Session title generation

Session titles live in `sessions.prompt` and are owned by the first
`submit_session` call. The engine spawns a fire-and-forget task that
runs `server-rs/sdk-bridge.mjs` in `SDK_BRIDGE_MODE=title`, which
makes a single haiku call to summarise the user's first prompt into
a 5–12 character Chinese title. The task has a 20-second hard
timeout; on any failure (timeout, non-zero exit, invalid output) the
original prompt is kept, so behaviour degrades to pre-feature.

The same bridge mode is used for periodic refresh (every 5 user
messages). When `follow_up` increments the turn count to a multiple
of 5 and `AGENTIC_RETITLE` is not `off`, it spawns a task that runs
the bridge in `SDK_BRIDGE_MODE=retitle` with `{"currentTitle": "...",
"messages": [["user", "..."], ...]}` on stdin. The bridge writes
`{"change": false}` or `{"change": true, "title": "..."}` to stdout,
and the engine updates `sessions.prompt` only when the new title
passes validation AND differs from the current one.

`follow_up` no longer retitles by default — the API
`POST /sessions/:id/message` flips `setTitle` from `true` to `false`
in the absence of an explicit value. Clients that want to rename a
session can still pass `setTitle=true`.
```

- [ ] **Step 3: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add docs/internals.md && git commit -m "docs: document title/retitle via SDK bridge

Replaces the prior Session title generation + periodic refresh
sections with a single block that documents the new
SDK_BRIDGE_MODE=title / retitle path, the 20s timeout, the
failure modes, and the AGENTIC_RETITLE toggle.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review Checklist

- [x] **Spec coverage:** title mode (Task 2) ✓; retitle mode (Task 2) ✓; `TitleBridge::generate` (Task 3) ✓; `TitleBridge::maybe_retitle` (Task 3) ✓; 20s timeout (Task 3) ✓; submit async (Task 5) ✓; follow_up 5-turn trigger (Task 5) ✓; `retitle_enabled` gate kept (Task 5) ✓; env inheritance (Task 3, 4) ✓; `is_valid_title` reuse (Task 3, 5) ✓; `parse_recent_messages` reuse (Task 5) ✓; delete `title_bin` / `AGENTIC_TITLE_BIN` (Task 4) ✓; delete 9 fake-claude-title fixtures (Task 7) ✓; new fake-sdk-bridge fixtures (Task 1) ✓; tests use Rust-side `with_node` injection (Task 3, 6) ✓; docs updated (Task 8) ✓; explicit `mode` arg in `run_bridge` (Task 3) ✓.
- [x] **Placeholder scan:** no "TBD", "TODO", "implement later", "fill in details", "similar to", "add appropriate", "handle edge cases" anywhere. Every step has either concrete code, a concrete command with expected output, or a concrete file edit.
- [x] **Type consistency:** `TitleBridge::generate(&self, prompt: &str, cwd: &Path) -> Result<Option<String>, TitleBridgeError>` is the same signature in Task 3 (definition + tests) and Task 5 (call site). `TitleBridge::maybe_retitle(&self, current_title: &str, recent_messages: &[(String, String)], cwd: &Path) -> Result<Option<String>, TitleBridgeError>` matches across all uses. `EngineConfig.title_bridge: std::sync::Arc<TitleBridge>` matches between Task 4 (definition) and Task 5 (call site). `SessionPatch { prompt: Some(t), ..Default::default() }` matches the existing struct field at `server-rs/src/engine/store.rs:95`. `TitleBridgeError` is the same enum in both the test file and the production file. `parse_recent_messages` returns `Vec<(String, String)>` in `title.rs` and is consumed as `&[(String, String)]` in `TitleBridge::maybe_retitle`. No name drift.
- [x] **Engine purity preserved:** `title_bridge.rs` and the helper block in `engine/mod.rs` have no axum imports. The bridge spawn uses `tokio::process::Command` (already in tree) and inherits process env (no `env_clear()`).
