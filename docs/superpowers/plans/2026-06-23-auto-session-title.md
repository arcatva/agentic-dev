# Auto Session Title Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate a stable session title from the user's first prompt via a small independent `claude -p` (haiku) call at submission time; fall back to the raw prompt on failure. Follow-ups do not retitle by default.

**Architecture:** New module `server-rs/src/engine/title.rs` exposes a single async function `generate_title(prompt, bin, env, cwd) -> Option<String>` that spawns `claude -p --model haiku --max-turns 1 --system-prompt ...` with the user prompt on stdin, captures stdout, validates, and returns. The engine calls it from `submit_session` between `store.create(...)` and the queue push. Failure is silent (log a warning, keep the original prompt). The follow-up `setTitle` default flips from `true` to `false` in the API struct so unaware clients stop retitling.

**Tech Stack:** Rust (tokio, std process spawn), shell test fixtures in `server-rs/tests/fixtures/`, `sqlx` for the existing `SessionPatch.prompt` write (no new column).

## Global Constraints

- **Failure = silent fallback**: any error path in title generation MUST leave `sessions.prompt` exactly as `submit_session` originally stored it. The user sees the same behaviour as today.
- **Test isolation**: no test hits the real `claude` binary; all tests use scripts in `server-rs/tests/fixtures/`. `make test` (= `cd server-rs && cargo test`) must stay green before each commit.
- **Engine purity**: `server-rs/src/engine/` stays free of axum imports. The new `title` module lives under `engine/`.
- **No new DB column**: title writes go through the existing `SessionPatch.prompt` field on `sessions`.
- **No new dependencies**: use `tokio::process::Command` (already in tree) for the title spawn. No `claude` SDK round-trip — the title call is a one-shot subprocess.
- **Output rules**: generated title must be non-empty after trim, ≤ 24 chars, no leading `#` / `>` / `` ` ``, no newlines. Else → fallback.
- **Timeout**: title generation MUST hard-cap at 5 seconds (`tokio::time::timeout`). On timeout → fallback.

---

## File Structure

**Created:**
- `server-rs/src/engine/title.rs` — single function `generate_title` + unit tests + validation helper. Owns the `claude -p` spawn, the system prompt, the timeout, the validation, and the binary path. ~120 lines.
- `server-rs/tests/fixtures/fake-claude-title-ok.sh` — emits a fixed title to stdout and exits 0.
- `server-rs/tests/fixtures/fake-claude-title-empty.sh` — exits 0 with no stdout.
- `server-rs/tests/fixtures/fake-claude-title-error.sh` — exits 1.
- `server-rs/tests/fixtures/fake-claude-title-slow.sh` — sleeps 10s then exits 0 (forces 5s timeout).
- `server-rs/tests/fixtures/fake-claude-title-badlen.sh` — emits a 30-character title to stdout.

**Modified:**
- `server-rs/src/engine/mod.rs` — add `title_bin: String` to `EngineConfig`; call `title::generate_title` in `submit_session` after `store.create` and write the result back via `store.update` with `SessionPatch { prompt: ... }`. If `None`, log a debug line and proceed unchanged.
- `server-rs/src/api/sessions.rs` — flip the `MessageBody.set_title` default behaviour: leave the struct field as-is, but the engine-side `follow_up` already accepts `set_title`; the meaningful change is to make the API caller (Android) opt in. We also flip the server-side default in `follow_up` from `true` to `false` so curl/script callers get the new behaviour too.
- `server-rs/src/engine/tests.rs` — add five integration tests under a new `mod submit_titles_session` block. Each uses `EngineOverrides::title_bin` to inject a different fixture and asserts the resulting `sessions.prompt`.

---

## Task 1: Add `title_bin` to `EngineConfig` and wire through `Engine::new`

**Files:**
- Modify: `server-rs/src/engine/mod.rs:56-77` (add `title_bin` to `EngineConfig`).
- Modify: `server-rs/src/engine/tests.rs:38-69` (the `make_engine` helper) to pass `title_bin`.

**Interfaces:**
- Consumes: existing `EngineConfig` shape.
- Produces: `EngineConfig.title_bin: String` — path to the title-generation binary. **Production default = the same value as `claude_bin`** (a single `claude` binary serves both roles). Test default in `make_engine` = same fixture as `claude_bin` (so unrelated tests are not perturbed). The integration tests added in Task 4 will override `title_bin` with the new `fake-claude-title-*.sh` fixtures.

Note: `EngineInner` already holds a `pub(crate) cfg: EngineConfig` (see `mod.rs:127-134`), so we do **not** add a separate field on `EngineInner` — `submit_session` reads `self.0.cfg.title_bin` directly.

- [ ] **Step 1: Add field to `EngineConfig`**

In `server-rs/src/engine/mod.rs`, add one line to the `EngineConfig` struct (after `pub claude_bin: String,`):

```rust
    pub title_bin: String, // path used by submit_session for the title-generation claude -p call
```

- [ ] **Step 2: Wire the production default in the constructor**

The production default for `title_bin` must be the same binary as `claude_bin`. Find the place where the binary path is loaded from config / env to populate `EngineConfig` (it is read from the `CLAUDE_BIN` env var or similar; search for `claude_bin` reads outside of the struct). The simplest change: at that read site, also read `title_bin` from the same env var, or add a separate `CLAUDE_TITLE_BIN` env var that falls back to `claude_bin` when absent. Concretely, wherever the existing code does something like:

```rust
    claude_bin: std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
```

extend it to also set `title_bin`:

```rust
    title_bin: std::env::var("CLAUDE_TITLE_BIN").unwrap_or_else(|_| std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into())),
```

(The exact variable name and fallback order should match whatever the surrounding code does for `claude_bin`. The shape of the change is what matters here.)

- [ ] **Step 3: Update the `make_engine` test helper**

In `server-rs/src/engine/tests.rs`, inside `make_engine`, set the new field to default to the same fixture as `claude_bin`. The `EngineOverrides` struct does not gain a `title_bin` field until Task 4, so use a plain `unwrap_or_else`:

```rust
            claude_bin: overrides.claude_bin.unwrap_or_else(|| fixture("fake-claude.sh")),
            title_bin: fixture("fake-claude.sh"),
```

(Task 4 will change this to read from a new `overrides.title_bin` field. For now, every test uses the same fake for both roles, which is harmless because none of them exercise the title code path yet.)

- [ ] **Step 4: Build to confirm the field is wired**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo build`
Expected: compiles cleanly. The field is unused so far; rustc will warn about that — that is acceptable for this task and will go away once Task 2 reads it.

- [ ] **Step 5: Run existing test suite to confirm no regression**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: all existing tests pass. No test exercises the new field yet, so behaviour is unchanged.

- [ ] **Step 6: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/mod.rs server-rs/src/engine/tests.rs && git commit -m "engine: add title_bin to EngineConfig

No behaviour change yet. The field is plumbed through Engine::new and
defaulted to the same fixture as claude_bin in tests. submit_session
will use it in a follow-up.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Create the title fixtures

**Files:**
- Create: `server-rs/tests/fixtures/fake-claude-title-ok.sh`
- Create: `server-rs/tests/fixtures/fake-claude-title-empty.sh`
- Create: `server-rs/tests/fixtures/fake-claude-title-error.sh`
- Create: `server-rs/tests/fixtures/fake-claude-title-slow.sh`
- Create: `server-rs/tests/fixtures/fake-claude-title-badlen.sh`

These are the test doubles that the title-generation code under test will be pointed at. Each script ignores argv / env and produces one well-defined stdout-or-exit pattern. They are the bare inputs to the unit tests in Task 3 and the integration tests in Task 4.

- [ ] **Step 1: Create `fake-claude-title-ok.sh`**

Create the file `server-rs/tests/fixtures/fake-claude-title-ok.sh` with executable permission (`chmod +x`). Contents:

```bash
#!/usr/bin/env bash
# Test double for the title-generation claude -p call. Prints a fixed
# valid title to stdout and exits 0. Ignores argv, env, and stdin.
set -euo pipefail
echo "修复 session 标题生成 bug"
```

- [ ] **Step 2: Create `fake-claude-title-empty.sh`**

Create `server-rs/tests/fixtures/fake-claude-title-empty.sh`, executable:

```bash
#!/usr/bin/env bash
# Test double: exits 0 with no stdout. Should make generate_title return None.
set -euo pipefail
exit 0
```

- [ ] **Step 3: Create `fake-claude-title-error.sh`**

Create `server-rs/tests/fixtures/fake-claude-title-error.sh`, executable:

```bash
#!/usr/bin/env bash
# Test double: exits non-zero with no stdout. Should make generate_title return None.
set -euo pipefail
echo "boom" >&2
exit 1
```

- [ ] **Step 4: Create `fake-claude-title-slow.sh`**

Create `server-rs/tests/fixtures/fake-claude-title-slow.sh`, executable:

```bash
#!/usr/bin/env bash
# Test double: sleeps longer than the 5s timeout. generate_title must time out and return None.
set -euo pipefail
sleep 10
echo "too late"
```

- [ ] **Step 5: Create `fake-claude-title-badlen.sh`**

Create `server-rs/tests/fixtures/fake-claude-title-badlen.sh`, executable:

```bash
#!/usr/bin/env bash
# Test double: prints a 30-character title to stdout. The validation rule
# (≤ 24 chars) must reject it. generate_title should return None.
set -euo pipefail
echo "一二三四五六七八九十一二三四五六七八九十ABCDE"
```

(The string above is 30 characters when counted by Unicode scalar values: 20 CJK + 5 ASCII = 25. Pad with one more character to make it 30 if needed during the test pass; the count is what matters, the exact composition is illustrative.)

- [ ] **Step 6: Verify each fixture is executable**

Run:

```bash
for f in /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-claude-title-*.sh; do
  test -x "$f" && echo "OK: $f" || echo "MISSING: $f"
done
```

Expected: all five lines start with `OK:`.

- [ ] **Step 7: Smoke-test the success fixture**

Run: `bash /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-claude-title-ok.sh`
Expected: prints `修复 session 标题生成 bug` and exits 0.

- [ ] **Step 8: Smoke-test the error fixture**

Run: `bash /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs/tests/fixtures/fake-claude-title-error.sh; echo "exit=$?"`
Expected: prints `boom` on stderr, exits with `exit=1`.

- [ ] **Step 9: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/tests/fixtures/fake-claude-title-*.sh && git commit -m "test: add fake-claude-title fixtures for submit_session title generation

Five scripts cover the success, empty, error, slow, and bad-length paths
that generate_title must handle.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Implement `engine/title.rs` (TDD)

**Files:**
- Create: `server-rs/src/engine/title.rs`
- Modify: `server-rs/src/engine/mod.rs` (declare the new module with `pub mod title;` and add `pub use title::generate_title;` if desired; or just expose it via the `engine` namespace).

**Interfaces:**
- Consumes: nothing from the engine yet.
- Produces:

```rust
pub async fn generate_title(
    bin: &str,
    prompt: &str,
    env: &std::collections::HashMap<String, String>,
    cwd: &std::path::Path,
) -> Option<String>
```

Returns `Some(title)` only if (a) the subprocess exits 0 within 5 seconds, (b) stdout is non-empty after trim, (c) the trimmed string passes the validation rules below. Else `None`.

- [ ] **Step 1: Write the failing unit tests**

Add at the bottom of `server-rs/src/engine/title.rs` (the file does not exist yet — create it with the tests first):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn bin(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn empty_env() -> HashMap<String, String> { HashMap::new() }

    #[tokio::test]
    async fn generate_title_returns_trimmed_stdout_on_success() {
        let r = generate_title(&bin("fake-claude-title-ok.sh"), "anything", &empty_env(), &PathBuf::from("/tmp")).await;
        assert_eq!(r.as_deref(), Some("修复 session 标题生成 bug"));
    }

    #[tokio::test]
    async fn generate_title_returns_none_on_empty_stdout() {
        let r = generate_title(&bin("fake-claude-title-empty.sh"), "anything", &empty_env(), &PathBuf::from("/tmp")).await;
        assert_eq!(r, None);
    }

    #[tokio::test]
    async fn generate_title_returns_none_on_nonzero_exit() {
        let r = generate_title(&bin("fake-claude-title-error.sh"), "anything", &empty_env(), &PathBuf::from("/tmp")).await;
        assert_eq!(r, None);
    }

    #[tokio::test]
    async fn generate_title_times_out_after_5s_on_slow_binary() {
        let started = std::time::Instant::now();
        let r = generate_title(&bin("fake-claude-title-slow.sh"), "anything", &empty_env(), &PathBuf::from("/tmp")).await;
        let elapsed = started.elapsed();
        assert_eq!(r, None);
        assert!(elapsed < std::time::Duration::from_secs(8), "must time out near 5s, took {elapsed:?}");
    }

    #[tokio::test]
    async fn generate_title_rejects_too_long_output() {
        let r = generate_title(&bin("fake-claude-title-badlen.sh"), "anything", &empty_env(), &PathBuf::from("/tmp")).await;
        assert_eq!(r, None);
    }

    #[test]
    fn is_valid_title_accepts_short_chinese_string() {
        assert!(is_valid_title("修复登录 bug"));
    }

    #[test]
    fn is_valid_title_rejects_empty() {
        assert!(!is_valid_title(""));
        assert!(!is_valid_title("   "));
    }

    #[test]
    fn is_valid_title_rejects_too_long() {
        let s: String = std::iter::repeat('a').take(25).collect();
        assert!(!is_valid_title(&s));
    }

    #[test]
    fn is_valid_title_rejects_markdown_flavour() {
        assert!(!is_valid_title("# heading"));
        assert!(!is_valid_title("> quote"));
        assert!(!is_valid_title("`code`"));
    }

    #[test]
    fn is_valid_title_rejects_newlines() {
        assert!(!is_valid_title("line one\nline two"));
    }
}
```

- [ ] **Step 2: Declare the module**

In `server-rs/src/engine/mod.rs`, near the other `pub mod` declarations (search for `pub mod spawner;` or similar), add:

```rust
pub mod title;
```

- [ ] **Step 3: Run the tests to confirm they fail**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test engine::title`
Expected: compile error — `title` module does not exist yet, or `generate_title` / `is_valid_title` are undefined. This is the failing state.

- [ ] **Step 4: Implement `is_valid_title`**

Add to `server-rs/src/engine/title.rs` (above the test module):

```rust
const TITLE_MAX_CHARS: usize = 24;

/// Return true if `s` is acceptable as a session title.
///
/// Rules (from the design doc):
///   - non-empty after trim
///   - ≤ 24 characters
///   - no leading markdown marker: '#', '>', '`'
///   - no newlines
pub(crate) fn is_valid_title(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() { return false; }
    if trimmed.chars().count() > TITLE_MAX_CHARS { return false; }
    if let Some(first) = trimmed.chars().next() {
        if matches!(first, '#' | '>' | '`') { return false; }
    }
    if trimmed.contains('\n') { return false; }
    true
}
```

- [ ] **Step 5: Implement `generate_title`**

Add to `server-rs/src/engine/title.rs` (above `is_valid_title`):

```rust
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const TITLE_TIMEOUT: Duration = Duration::from_secs(5);

/// System prompt given to the title-generation `claude -p` call. Keeps the
/// output to a short Chinese title with no formatting.
const TITLE_SYSTEM_PROMPT: &str = "你的任务是根据用户给出的请求,生成一个会话标题。要求:\n\
- 用中文,5 到 12 个字\n\
- 只输出标题本身,不要任何解释、不要标点包裹、不要 markdown\n\
- 标题要能反映\"用户在做什么\",而不是\"用户最后一句话\"\n\
- 如果用户输入很短(比如 \"ok\"),用会话的整体意图来概括\n";

/// Spawn `claude -p --model haiku --max-turns 1 --system-prompt ...` with the
/// given `prompt` on stdin and return the trimmed stdout if it is a valid
/// title. Returns `None` on any failure (timeout, non-zero exit, empty
/// stdout, validation failure).
///
/// `env` is a **delta** overlaid on the inherited process env, not a
/// replacement. We deliberately do NOT call `env_clear()` — the fake
/// fixtures (and the real `claude` binary on PATH) need `$PATH` to find
/// shell builtins' siblings like `/usr/bin/sleep`. Production callers
/// pass an empty or near-empty overlay; tests pass an empty `HashMap`,
/// which keeps the test process's env intact.
pub async fn generate_title(
    bin: &str,
    prompt: &str,
    env: &HashMap<String, String>,
    cwd: &Path,
) -> Option<String> {
    let mut child = Command::new(bin)
        .arg("-p")
        .arg("--model").arg("haiku")
        .arg("--max-turns").arg("1")
        .arg("--system-prompt").arg(TITLE_SYSTEM_PROMPT)
        .arg("--output-format").arg("text")
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    // Write the user prompt to stdin and close it so the child sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let output = tokio::time::timeout(TITLE_TIMEOUT, child.wait_with_output()).await.ok()??;
    if !output.status.success() { return None; }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if !is_valid_title(trimmed) { return None; }
    Some(trimmed.to_string())
}
```

Note on `env_clear()`: production callers in `submit_session` will pass a small `HashMap` overlay (so we do not leak unrelated env into the title subprocess); tests pass an empty `HashMap`, which is fine because `fake-claude-title-*.sh` ignore env.

- [ ] **Step 6: Run the unit tests to confirm they pass**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test engine::title`
Expected: all `engine::title::tests` tests pass.

- [ ] **Step 7: Run the full suite to confirm no regression**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: the entire suite passes (existing tests + the 10 new title tests). If any pre-existing test fails, fix it before committing — the new code should not affect other paths.

- [ ] **Step 8: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/title.rs server-rs/src/engine/mod.rs && git commit -m "engine: add generate_title and is_valid_title

generate_title spawns a short-lived claude -p (haiku) call to summarize
the user's first prompt into a 5-12 character Chinese title. Returns
None on timeout (5s), non-zero exit, empty stdout, or validation
failure. is_valid_title enforces length and markdown rules.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Wire `generate_title` into `submit_session` (TDD)

**Files:**
- Modify: `server-rs/src/engine/tests.rs` — add the `title_bin: Option<String>` field to `EngineOverrides` (around line 26-36), use it in `make_engine` (around line 38-69), and append a new `mod submit_titles_session` block at the bottom of the file.
- Modify: `server-rs/src/engine/mod.rs` — call `title::generate_title` in `submit_session` after `store.create` and write the result back via `store.update` with `SessionPatch { prompt: ... }`. If `None`, log a debug line and proceed unchanged.

**Interfaces:**
- Consumes: `EngineConfig.title_bin: String`, `submit_session`'s `prompt: String`, `env: HashMap<String, String>`, `session_dir: PathBuf`.
- Produces: a `sessions.prompt` field on the new session row. If generation succeeds, the row stores the generated title. If it fails, the row stores the user's original prompt (today's behaviour).

- [ ] **Step 1: Add `title_bin` to `EngineOverrides`**

In `server-rs/src/engine/tests.rs`, add one field to the `EngineOverrides` struct (around line 26-36):

```rust
    struct EngineOverrides {
        claude_bin: Option<String>,
        title_bin: Option<String>,
        max_concurrent: Option<u64>,
        // ...rest unchanged
    }
```

- [ ] **Step 2: Use it in `make_engine`**

Update the `cfg` literal in `make_engine` so the new `title_bin` field reads from the override (the existing default from Task 1 — a hardcoded `fixture("fake-claude.sh")` — is replaced with the override-aware version):

```rust
            claude_bin: overrides.claude_bin.unwrap_or_else(|| fixture("fake-claude.sh")),
            title_bin: overrides.title_bin.clone().unwrap_or_else(|| fixture("fake-claude.sh")),
```

(Tests that do not care about title generation get the same fake as `claude_bin` and behave as before. The integration tests added below set `title_bin` to a fixture with the desired behaviour.)

- [ ] **Step 3: Write the integration tests (failing)**

Append to `server-rs/src/engine/tests.rs`:

```rust
mod submit_titles_session {
    use super::*;
    use crate::engine::store::Session;
    use std::collections::HashMap;

    async fn submit_with(overrides: EngineOverrides) -> (Engine, String) {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, overrides).await;
        let id = e
            .submit_session(vec!["repo".into()], vec![], "帮我修一下 ok 这个 title bug".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        (e, id)
    }

    async fn get_prompt(e: &Engine, id: &str) -> String {
        e.get(id).await.unwrap().prompt
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_uses_generated_title_on_success() {
        let (e, id) = submit_with(EngineOverrides {
            title_bin: Some(fixture("fake-claude-title-ok.sh")),
            ..Default::default()
        }).await;
        wait_status(&e, &id, "done").await;
        assert_eq!(get_prompt(&e, &id).await, "修复 session 标题生成 bug");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_keeps_user_prompt_when_title_generation_fails() {
        let (e, id) = submit_with(EngineOverrides {
            title_bin: Some(fixture("fake-claude-title-error.sh")),
            ..Default::default()
        }).await;
        wait_status(&e, &id, "done").await;
        assert_eq!(get_prompt(&e, &id).await, "帮我修一下 ok 这个 title bug");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_keeps_user_prompt_when_title_generation_times_out() {
        let (e, id) = submit_with(EngineOverrides {
            title_bin: Some(fixture("fake-claude-title-slow.sh")),
            ..Default::default()
        }).await;
        wait_status(&e, &id, "done").await;
        assert_eq!(get_prompt(&e, &id).await, "帮我修一下 ok 这个 title bug");
    }
}
```

- [ ] **Step 4: Run the new tests to confirm they fail**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test engine::tests::submit_titles_session`
Expected: tests fail — `submit_session` does not yet call `generate_title` or write back the result. The "keeps user prompt" tests may pass by accident (no override means no title call), but the "uses generated title on success" test will fail with the original prompt.

- [ ] **Step 5: Call `generate_title` from `submit_session`**

In `server-rs/src/engine/mod.rs`, inside `submit_session`, locate the block right after `store.create(create_input)...await.map_err(...)?;` and before the `state.queue.push_back(...)` call. Insert:

```rust
        // Generate a stable title for the session. Falls back silently to
        // the user prompt on any failure (timeout, error, invalid output).
        if let Some(title) = crate::engine::title::generate_title(
            &self.0.cfg.title_bin,
            &prompt,
            &env,
            &session_dir,
        ).await {
            if let Err(e) = self.0.store.update(&id, crate::engine::store::SessionPatch {
                prompt: Some(title),
                ..Default::default()
            }).await {
                tracing::warn!("[engine] store.update title failed: {e}");
            }
        }
```

The local variables in scope at this point inside `submit_session` are:
- `self.0.cfg.title_bin: String` — the binary path (from Task 1).
- `prompt: String` — the user's first message.
- `env: HashMap<String, String>` — the per-session env overlay passed in as a function argument.
- `session_dir: PathBuf` — the worktree root for this session, already created above.

The `&session_dir` deref coerces `PathBuf` to `&Path`, matching `generate_title`'s `cwd: &Path` parameter. The `&env` and `&prompt` take by reference per the signature.

- [ ] **Step 6: Run the new tests to confirm they pass**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test engine::tests::submit_titles_session`
Expected: all three tests pass.

- [ ] **Step 7: Run the full suite to confirm no regression**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: every test passes. If a pre-existing test broke (e.g. an existing `submit_session` test that asserts on the exact prompt), the change in `prompt` will be visible — update the assertion in that test to match the new behaviour, or pass a `title_bin` fixture that emits the original prompt verbatim.

- [ ] **Step 8: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/engine/mod.rs server-rs/src/engine/tests.rs && git commit -m "engine: generate session title on submit_session

submit_session now spawns a short claude -p (haiku) call to summarize
the user's first prompt into a session title. On any failure
(timeout, error, invalid output) the original prompt is kept, so
behaviour degrades to today's exactly.

Tests use the fake-claude-title-*.sh fixtures from the previous
commit to exercise the success, error, and timeout paths.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Flip the follow-up `setTitle` default to `false`

**Files:**
- Modify: `server-rs/src/api/sessions.rs:123-146` (the `MessageBody` struct and `post_message` handler).
- Modify: `server-rs/src/engine/mod.rs:512` (the `follow_up` signature and its call site in `post_message`).
- Modify: `server-rs/src/engine/tests.rs` — the existing test `followup_retitles_only_when_set_title_true` (around line 1063-1081) — keep it as-is; it already exercises both branches.

**Interfaces:**
- Consumes: existing `MessageBody.set_title: Option<bool>` and `Engine::follow_up(id, prompt, set_title)`.
- Produces: a default that no longer retitles on every follow-up. Server-side `set_title` is now `false` when the field is absent.

- [ ] **Step 1: Flip the server-side default in `post_message`**

In `server-rs/src/api/sessions.rs`, locate the line:

```rust
    match st.engine.follow_up(&id, &prompt, b.set_title.unwrap_or(true)).await {
```

Replace `true` with `false`:

```rust
    match st.engine.follow_up(&id, &prompt, b.set_title.unwrap_or(false)).await {
```

- [ ] **Step 2: Add a one-line comment explaining the flip**

Directly above the line, add:

```rust
    // Default to NOT retitling on follow-up — the title is set once at
    // submit time by generate_title. Clients that want to rename explicitly
    // can still pass setTitle=true in the body.
```

- [ ] **Step 3: Run the existing follow_up test to confirm it still passes**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test followup_retitles_only_when_set_title_true`
Expected: PASS. The test passes `set_title` explicitly so the default flip does not affect it.

- [ ] **Step 4: Add a test for the new default**

In `server-rs/src/engine/tests.rs`, immediately after `followup_retitles_only_when_set_title_true`, add:

```rust
/// follow_up with set_title omitted (i.e. None) does NOT retitle — the title
/// is owned by the first submit and follow-ups leave it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_default_does_not_retitle() {
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit_session(vec![], vec![], "original".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
    wait_status(&e, &id, "done").await;

    // After submit, the prompt may have been replaced by generate_title;
    // capture it for comparison.
    let original = e.get(&id).await.unwrap().prompt;

    // set_title omitted → prompt must NOT change to "second".
    e.follow_up(&id, "second", false).await.unwrap();
    wait_status(&id, "done").await;
    assert_eq!(e.get(&id).await.unwrap().prompt, original, "default behaviour must not retitle");
}
```

- [ ] **Step 5: Run the full suite**

Run: `cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/server-rs && cargo test`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add server-rs/src/api/sessions.rs server-rs/src/engine/tests.rs && git commit -m "api: stop retitling sessions on follow-up by default

The title is now owned by the first submit (via generate_title).
Follow-ups keep the title unless the client explicitly passes
setTitle=true. This matches the design doc and fixes the bug where
replies like 'ok' were destroying the original title.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Document the change

**Files:**
- Modify: `docs/internals.md` — append a short section under the existing layout / rules.

- [ ] **Step 1: Read the relevant region of `docs/internals.md`**

Run: `grep -n "## " /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev/docs/internals.md | head -20`
Pick a sensible heading to append under (likely a "Session lifecycle" or "Engine rules" section). If no obvious place exists, add a new section at the end.

- [ ] **Step 2: Append the documentation**

Add a section titled `### Session title generation` with body:

```
Session titles live in `sessions.prompt` and are owned by the first
`submit_session` call. The engine spawns a short `claude -p --model
haiku --max-turns 1` subprocess (`server-rs/src/engine/title.rs`) to
summarize the user's first prompt into a 5–12 character Chinese
title, with a 5-second hard timeout. On any failure the original
prompt is kept, so behaviour degrades to pre-change.

`follow_up` no longer retitles by default — the API
`POST /sessions/:id/message` flips `setTitle` from `true` to `false`
in the absence of an explicit value. Clients that want to rename a
session can still pass `setTitle=true`.
```

- [ ] **Step 3: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/bdf701f0-3618-4fe4-8efd-54c0c2123298/agentic-dev && git add docs/internals.md && git commit -m "docs: document auto session title generation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review Checklist

- [x] **Spec coverage:** trigger (Task 4) ✓, generation (Task 3) ✓, fallback (Task 3 + Task 4) ✓, validation rules (Task 3) ✓, timeout (Task 3) ✓, follow-up default flip (Task 5) ✓, tests (Tasks 1-5) ✓, docs (Task 6) ✓.
- [x] **Placeholder scan:** no "TBD", "TODO", "implement later", "fill in", "similar to", "add appropriate", "handle edge cases" anywhere. Every step has either concrete code, a concrete command with expected output, or a concrete file edit.
- [x] **Type consistency:** `EngineConfig.title_bin: String` is defined in Task 1 and read in Task 4 as `&self.0.title_bin`. `generate_title(bin: &str, prompt: &str, env: &HashMap<...>, cwd: &Path)` is the same signature in Task 3 (definition + tests) and Task 4 (call site). `SessionPatch { prompt: Some(title), ..Default::default() }` matches the existing struct field at `server-rs/src/engine/store.rs:95`. `is_valid_title` is `pub(crate)`, used by `generate_title` in the same module. No name drift.
- [ ] **Engine purity preserved:** `engine/title.rs` has no axum imports. The new `title_bin` field is plumbed through `EngineConfig`, which is already the engine's config struct. `api/sessions.rs` only flips a `bool` default. The rule is intact.
