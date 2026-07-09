# Rust Rewrite — Phase 2 (stream-json parser: `parseLine` / `ClaudeEvent`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `server-rs/`, port `server/engine/streamParser.ts` to Rust at behavioral parity: a `parse_line(line: &str) -> Vec<ClaudeEvent>` function and a `ClaudeEvent` enum that mirror `server/engine/types.ts` field-for-field. One raw stream-json line maps to 0..n events (an assistant message can carry several tool_uses → several events). `parentToolUseId` routes UI parts to the right agent column. No HTTP/engine wiring yet — the engine consumes this in Phase 4.

**Architecture:** New module `server-rs/src/stream.rs` exposes `parse_line` + the `ClaudeEvent` enum + `SpawnedAgent` struct. Parsing is pure and synchronous (cheap per-line JSON parse; the engine already calls it inline per tailed line). Each event carries `raw: serde_json::Value` (the parsed line) so downstream code (Phase 4 engine) can read fields the typed shape doesn't surface — exactly as TS reads `ev.raw.ttft_ms` / `ev.raw.duration_ms` on a `result`. Helpers `result_text` (flatten tool_result content) and `meta_name` (regex-extract a dynamic workflow's real name from its inline script) mirror the two TS private functions.

**Tech Stack:** Rust (edition 2021), `serde` / `serde_json` 1 (already deps), `regex` 1 (NEW dep — for `meta_name`). No async, no new tokio usage.

**Parity bar:** Behavioral parity with the TS reference (`server/engine/streamParser.ts`, `server/engine/streamParser.test.ts`, `server/engine/types.ts`) is the bar. Every `streamParser.test.ts` case has a mirrored Rust test asserting the same event sequence and field values.

## Global Constraints

Copy these exact parity values/field names verbatim into the implementation; do not paraphrase.

- **Crate:** `agentic-dev/server-rs/`. Run all `cargo` commands from there. **Commit only — NEVER push.** End every commit message with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Module:** new file `server-rs/src/stream.rs`; register `mod stream;` in `src/main.rs` (alphabetical with the other `mod` lines). Do **not** touch `transcript.rs`'s `is_rendered`/`filter_rendered` — they are the rendered-line filter (a different concern) and already exist.
- **Entry point signature (verbatim):** `pub fn parse_line(line: &str) -> Vec<ClaudeEvent>`. Blank-after-trim or non-JSON line → `vec![]` (empty). A parsed object that matches no rule → exactly one `ClaudeEvent::Other`.
- **`raw` on every variant:** every event carries `raw: serde_json::Value` = the full parsed line object. This is load-bearing: the Phase 4 engine reads `raw.ttft_ms` / `raw.duration_ms` (on `result`) and treats `raw` as the forward-compat escape hatch. Never drop it.
- **`parent_tool_use_id` extraction (verbatim):** `let parent = obj.get("parent_tool_use_id").and_then(|v| v.as_str()).map(String::from)` → `Option<String>`. `None` = the main agent. This routes events to the right agent column. The TS field on events is `parentToolUseId` (serde-rename to that on the wire is **not** required in Phase 2 — these events are not yet serialized to JSON; Phase 5 decides wire serialization. Keep the Rust field name `parent_tool_use_id`).
- **Dispatch order (verbatim — first match wins, exactly as `streamParser.ts` lines 32-102):**
  1. `type=="system" && subtype=="init" && session_id is string` → `Init { session_id, raw }`.
  2. `type=="agentic_prompt"` → `Prompt { text = String(obj.text ?? ""), at = obj.at if number else 0, raw }`. `at` is epoch-ms; read it as `obj.get("at").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0)` so a JSON integer OR a JSON float (serde_json's `as_i64()` is `None` for float-encoded numbers) both work, matching TS `typeof obj.at === "number"`.
  3. `type=="system" && subtype=="api_retry"` → `Retry { attempt = Number(obj.attempt ?? 0), max_retries = Number(obj.max_retries ?? 0), category = String(obj.error ?? "unknown"), raw }`.
  4. `type=="stream_event" && event.delta.type=="text_delta"` → `Text { text = String(event.delta.text ?? ""), parent_tool_use_id = parent, raw }`.
  5. `type=="stream_event" && event.delta.type=="thinking_delta"` → `Thinking { text = String(event.delta.thinking ?? ""), parent_tool_use_id = parent, raw }`.
  6. `type=="result"` → `Result { is_error = Boolean(obj.is_error), cost_usd = obj.total_cost_usd if number else None, text = <see result-text rule>, raw }`.
  7. `type=="user" && message.content is array` → per content block: `text` block **with** a parent → `Text`; `tool_result` block with string `tool_use_id` → `AgentResult { tool_use_id, text = result_text(b.content), raw }`. (Other blocks ignored.) Returns the collected vec (may be empty).
  8. `type=="assistant" && message.content is array` → collect, **in this order**: `Skill` (if any), `Agent` (if any), each `Workflow` (in array order), `Ask` (first match only), then each non-special tool. Returns the collected vec (may be empty).
  9. otherwise → `vec![Other { raw }]`.
- **`result.text` field resolution (verbatim — `streamParser.ts` lines 59-63):** `text = obj.result if string else obj.error if string else (obj.errors if array → join the string elements with "\n", but if that join is empty → None) else None`. Carry as `Option<String>`. (The `errors` empty-join → `None` matters: an empty/non-string `errors[]` must yield `None`, not `Some("")`.)
- **`result.cost_usd` (verbatim):** `cost_usd = obj.total_cost_usd if it is a JSON number else None`. Type: `Option<f64>`.
- **Assistant `Skill` rule (verbatim — lines 82-84):** from blocks where `b.type=="tool_use"`, take those with `name=="Skill" && input.skill is string`; collect `names: Vec<String> = [input.skill...]`. Emit `Skill { names, parent_tool_use_id, raw }` **only if `names` is non-empty**.
- **Assistant `Agent` rule (verbatim — lines 85-88):** from tool_use blocks with `(name=="Agent" || name=="Task") && input is present`, build `SpawnedAgent { id = String(b.id ?? ""), agent_type = String(input.subagent_type ?? "agent"), description = String(input.description ?? "") }`. Emit `Agent { agents, parent_tool_use_id, raw }` **only if `agents` is non-empty**. NOTE: default `agent_type` is the literal `"agent"` (not `"Task"`).
- **Assistant `Workflow` rule (verbatim — lines 89-92):** for **each** tool_use block with `name=="Workflow"`, `name_field = input.name ?? input.title ?? meta_name(input.script) ?? "workflow"`; emit `Workflow { id = String(w.id ?? ""), name = String(name_field), parent_tool_use_id, raw }`. One event per Workflow block.
- **Assistant `Ask` rule (verbatim — lines 93-94):** `find` (first) tool_use block with `name=="AskUserQuestion" && input.questions is array`; emit `Ask { questions = input.questions (the array, as Vec<serde_json::Value>), parent_tool_use_id, raw }`. At most one.
- **Assistant ordinary `tool` rule (verbatim — lines 96-99):** `SPECIAL = {"Skill","Agent","Task","Workflow","AskUserQuestion"}`. For each tool_use block whose `name` is **not** in `SPECIAL`, emit `Tool { name = String(t.name ?? "tool"), input = t.input ?? {} (serde_json::Value, default Value::Object empty), parent_tool_use_id, raw }`.
- **`result_text(content)` helper (verbatim — lines 4-8):** `content` is string → return it; `content` is array → keep blocks with `b.type=="text"`, map to `b.text` (as string, `""` if missing), join with `"\n"`; else → `""`. Returns `String`.
- **`meta_name(script)` helper (verbatim — lines 13-17):** if `script` is not a string → `None`; else regex `meta\s*=\s*\{[\s\S]*?name\s*:\s*['"]([^'"]+)['"]` over the script, return capture group 1 if matched else `None`. The `[\s\S]*?` is a **lazy, dot-matches-newline** span (the script is multiline) — in Rust use `(?s)` inline flag so `.` matches newlines, written as `meta\s*=\s*\{(?s:.*?)name\s*:\s*['"]([^'"]+)['"]` OR build with `RegexBuilder::new(...).dot_matches_new_line(true)`. Compile the regex once (`OnceLock` / `LazyLock`), never per call.
- **String coercion parity (`String(x)`):** TS `String(obj.text ?? "")` coerces. In Rust, read the JSON value: if it is a JSON string use it; if absent/null use the default (`""` / `"agent"` / `"tool"` / `"workflow"` / `"unknown"` per rule). Do **not** stringify a non-string JSON number/bool here unless the TS rule does — the cases above only ever read string-or-absent fields, so `value.as_str().unwrap_or(<default>).to_string()` is the faithful coercion. The exception is `Number(...)`: `attempt`/`max_retries` use `obj.attempt.as_f64()` (or `as_i64()` → cast) with default `0`.
- **Number coercion parity (`Number(x ?? 0)`):** `attempt` / `max_retries` are `u32` (TS treats them as small counts). Read `obj.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0) as u32`. Mirror for `max_retries` via `max_retries`. A missing/non-number field → `0`.
- **`is_error` coercion (`Boolean(obj.is_error)`):** `obj.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)`. Missing/non-bool → `false`.
- **Out of scope (later phases):** wiring `parse_line` into a tailer/engine (Phase 3/4); serializing `ClaudeEvent` to the WS/HTTP wire (Phase 5 — note the TS events are objects with a `kind` discriminator + camelCase fields; Phase 5 decides the serde representation); `classifyClaudeError` (Phase 4 engine). All `cargo test` green before each commit.

## File Structure

- `server-rs/Cargo.toml` — add `regex = "1"` under `[dependencies]`.
- `server-rs/src/stream.rs` — NEW: `SpawnedAgent`, `ClaudeEvent`, `parse_line`, `result_text`, `meta_name`, inline `#[cfg(test)] mod tests`.
- `server-rs/src/main.rs` — add `mod stream;` to the module list.

---

### Task 1: add `regex` dep + create the `stream` module skeleton (failing tests)

**Files:**
- Modify: `server-rs/Cargo.toml` (add `regex`), `server-rs/src/main.rs` (add `mod stream;`)
- Create: `server-rs/src/stream.rs`
- Test: inline `#[cfg(test)]` in `src/stream.rs`

**Interfaces (exact Rust signatures):**
```rust
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnedAgent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEvent {
    Init { session_id: String, raw: serde_json::Value },
    Prompt { text: String, at: i64, raw: serde_json::Value },
    Text { text: String, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Skill { names: Vec<String>, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Ask { questions: Vec<serde_json::Value>, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Agent { agents: Vec<SpawnedAgent>, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Workflow { id: String, name: String, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Thinking { text: String, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    Tool { name: String, input: serde_json::Value, parent_tool_use_id: Option<String>, raw: serde_json::Value },
    AgentResult { tool_use_id: String, text: String, raw: serde_json::Value },
    Retry { attempt: u32, max_retries: u32, category: String, raw: serde_json::Value },
    Result { is_error: bool, cost_usd: Option<f64>, text: Option<String>, raw: serde_json::Value },
    Other { raw: serde_json::Value },
}

pub fn parse_line(line: &str) -> Vec<ClaudeEvent>;
fn result_text(content: &serde_json::Value) -> String;
fn meta_name(script: &serde_json::Value) -> Option<String>;
```

> **Why `raw: serde_json::Value` on every variant** (matches TS `raw: unknown`): the Phase 4 engine reads `ev.raw.ttft_ms` / `ev.raw.duration_ms` (a `result` event) and uses `raw` as the forward-compat field bag. `serde_json::Value` is the Rust analogue of TS `unknown` — a parsed-but-untyped JSON tree (like Python `dict`/`Any`, or Go `interface{}`/`map[string]any`). `#[derive(PartialEq)]` is needed so tests can `assert_eq!` whole events (`serde_json::Value: PartialEq`, so this derives cleanly).

- [ ] **Step 1: add `regex` to `Cargo.toml`.** Under `[dependencies]`, after `serde_json = "1"`:
```toml
regex = "1"
```
- [ ] **Step 2: register the module** in `src/main.rs` — add `mod stream;` in the `mod` block (after `mod store;`, before `mod throttle;` to keep it sorted):
```rust
mod state;
mod store;
mod stream;
mod throttle;
mod transcript;
```
- [ ] **Step 3: write `src/stream.rs` with the enum/struct/signatures and a `todo!()` body**, plus the FIRST failing test (blank/non-JSON → `[]`). This compiles and fails (the `todo!()` panics):
```rust
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct SpawnedAgent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEvent {
    Init { session_id: String, raw: Value },
    Prompt { text: String, at: i64, raw: Value },
    Text { text: String, parent_tool_use_id: Option<String>, raw: Value },
    Skill { names: Vec<String>, parent_tool_use_id: Option<String>, raw: Value },
    Ask { questions: Vec<Value>, parent_tool_use_id: Option<String>, raw: Value },
    Agent { agents: Vec<SpawnedAgent>, parent_tool_use_id: Option<String>, raw: Value },
    Workflow { id: String, name: String, parent_tool_use_id: Option<String>, raw: Value },
    Thinking { text: String, parent_tool_use_id: Option<String>, raw: Value },
    Tool { name: String, input: Value, parent_tool_use_id: Option<String>, raw: Value },
    AgentResult { tool_use_id: String, text: String, raw: Value },
    Retry { attempt: u32, max_retries: u32, category: String, raw: Value },
    Result { is_error: bool, cost_usd: Option<f64>, text: Option<String>, raw: Value },
    Other { raw: Value },
}

pub fn parse_line(_line: &str) -> Vec<ClaudeEvent> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_or_non_json_lines_yield_no_events() {
        assert_eq!(parse_line(""), vec![]);
        assert_eq!(parse_line("   "), vec![]);
        assert_eq!(parse_line("not json"), vec![]);
    }
}
```
- [ ] **Step 4: run `cargo test --lib stream::` from `server-rs/`, expect FAIL** (the `todo!()` panics in `blank_or_non_json_lines_yield_no_events`). Confirm it compiles first — a compile error here means the enum/signatures are wrong, fix before proceeding.
- [ ] **Step 5: commit** (commit-only, NEVER push):
```
git -C agentic-dev add server-rs/Cargo.toml server-rs/src/main.rs server-rs/src/stream.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 2 T1: stream module skeleton + ClaudeEvent enum (failing)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: implement `parse_line` — guard clauses, helpers, simple events

**Files:**
- Modify: `server-rs/src/stream.rs` (implement `parse_line`, `result_text`, `meta_name`)
- Test: inline `#[cfg(test)]` in `src/stream.rs`

**Interfaces (exact Rust signatures):** as Task 1 (now with real bodies).

This task implements everything EXCEPT the assistant-message branch (Task 3 adds that), plus all the simple-event tests. The assistant branch is stubbed to fall through to `Other` until Task 3, so the assistant tests are written in Task 3.

- [ ] **Step 1: write the failing tests for the simple events** (mirror `streamParser.test.ts` cases: init, text-delta with/without parent, api_retry, result-with-cost, result error via `errors[]`, legacy result error via `result`, thinking, agentic_prompt, user-text-with-parent, user-tool_result → agentResult). Add to `mod tests`:
```rust
    use serde_json::json;

    // Helper: assert the event vec ignoring `raw` is not practical (raw differs per call);
    // instead match on the typed fields via pattern matching.

    #[test]
    fn parses_init_and_extracts_session_id() {
        let line = json!({ "type": "system", "subtype": "init", "session_id": "abc123" }).to_string();
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ClaudeEvent::Init { session_id, .. } => assert_eq!(session_id, "abc123"),
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn parses_text_delta_with_parent_tool_use_id() {
        let line = json!({
            "type": "stream_event",
            "parent_tool_use_id": "toolu_1",
            "event": { "delta": { "type": "text_delta", "text": "hello" } }
        }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text { text, parent_tool_use_id, .. } => {
                assert_eq!(text, "hello");
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_from_main_agent_has_none_parent() {
        let line = json!({
            "type": "stream_event",
            "event": { "delta": { "type": "text_delta", "text": "hi" } }
        }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text { text, parent_tool_use_id, .. } => {
                assert_eq!(text, "hi");
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parses_api_retry() {
        let line = json!({ "type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 5, "error": "overloaded" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Retry { attempt, max_retries, category, .. } => {
                assert_eq!((*attempt, *max_retries), (2, 5));
                assert_eq!(category, "overloaded");
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn parses_result_with_cost() {
        let line = json!({ "type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.0123 }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result { is_error, cost_usd, .. } => {
                assert_eq!(*is_error, false);
                assert_eq!(*cost_usd, Some(0.0123));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn captures_error_result_from_errors_array() {
        let line = json!({ "type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["You've hit your session limit · resets 3:30pm"] }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result { is_error, text, .. } => {
                assert_eq!(*is_error, true);
                assert_eq!(text.as_deref(), Some("You've hit your session limit · resets 3:30pm"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn reads_legacy_error_result_text_field() {
        let line = json!({ "type": "result", "subtype": "error_during_execution", "is_error": true,
            "result": "You've hit your session limit · resets 3:30pm" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result { is_error, text, .. } => {
                assert_eq!(*is_error, true);
                assert_eq!(text.as_deref(), Some("You've hit your session limit · resets 3:30pm"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parses_agentic_prompt_marker() {
        let line = json!({ "type": "agentic_prompt", "text": "do x" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Prompt { text, at, .. } => { assert_eq!(text, "do x"); assert_eq!(*at, 0); }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn emits_thinking_for_thinking_delta() {
        let line = json!({ "type": "stream_event", "event": { "delta": { "type": "thinking_delta", "thinking": "hmm" } } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Thinking { text, .. } => assert_eq!(text, "hmm"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn user_text_block_with_parent_is_text_event() {
        let line = json!({ "type": "user", "parent_tool_use_id": "toolu_A",
            "message": { "content": [{ "type": "text", "text": "your job: reply pong" }] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text { text, parent_tool_use_id, .. } => {
                assert_eq!(text, "your job: reply pong");
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_A"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn user_tool_result_block_is_agent_result() {
        let line = json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "toolu_A", "content": [{ "type": "text", "text": "pong" }] }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::AgentResult { tool_use_id, text, .. } => {
                assert_eq!(tool_use_id, "toolu_A");
                assert_eq!(text, "pong");
            }
            other => panic!("expected AgentResult, got {other:?}"),
        }
    }
```
- [ ] **Step 2: run `cargo test --lib stream::`, expect FAIL** (all still hit `todo!()`).
- [ ] **Step 3: implement the helpers + `parse_line` guard clauses.** Replace the `todo!()` body and add the two helpers. The assistant branch is a temporary `// Task 3` placeholder that falls through to `Other`:
```rust
use regex::Regex;
use std::sync::OnceLock;

/// Flatten a tool_result's content (string, or array of text blocks) to plain text.
/// Mirror of TS `resultText`.
fn result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .map(|b| b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// A dynamic ("ultracode") workflow's real name lives only inside its inline script's
/// `export const meta = { name: '…' }`. Extract it so the UI chip shows the real name.
/// Mirror of TS `metaName`. The script is multiline, so `.` must match newlines (`(?s)`).
fn meta_name(script: &Value) -> Option<String> {
    let s = script.as_str()?;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"meta\s*=\s*\{(?s:.*?)name\s*:\s*['"]([^'"]+)['"]"#).expect("valid regex")
    });
    re.captures(s).map(|c| c[1].to_string())
}

pub fn parse_line(line: &str) -> Vec<ClaudeEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let obj: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let parent: Option<String> = obj
        .get("parent_tool_use_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let ty = obj.get("type").and_then(|v| v.as_str());
    let subtype = obj.get("subtype").and_then(|v| v.as_str());

    // 1. system/init
    if ty == Some("system") && subtype == Some("init") {
        if let Some(session_id) = obj.get("session_id").and_then(|v| v.as_str()) {
            return vec![ClaudeEvent::Init { session_id: session_id.to_string(), raw: obj }];
        }
    }
    // 2. agentic_prompt
    if ty == Some("agentic_prompt") {
        let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // `at` is epoch-ms; accept a JSON integer OR a float-encoded number (serde_json's
        // as_i64() is None for floats). Matches TS `typeof obj.at === "number" ? obj.at : 0`.
        let at = obj.get("at").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0);
        return vec![ClaudeEvent::Prompt { text, at, raw: obj }];
    }
    // 3. system/api_retry
    if ty == Some("system") && subtype == Some("api_retry") {
        let attempt = obj.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let max_retries = obj.get("max_retries").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let category = obj.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        return vec![ClaudeEvent::Retry { attempt, max_retries, category, raw: obj }];
    }
    // 4 & 5. stream_event deltas
    if ty == Some("stream_event") {
        let delta_type = obj.pointer("/event/delta/type").and_then(|v| v.as_str());
        if delta_type == Some("text_delta") {
            let text = obj.pointer("/event/delta/text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            return vec![ClaudeEvent::Text { text, parent_tool_use_id: parent, raw: obj }];
        }
        if delta_type == Some("thinking_delta") {
            let text = obj.pointer("/event/delta/thinking").and_then(|v| v.as_str()).unwrap_or("").to_string();
            return vec![ClaudeEvent::Thinking { text, parent_tool_use_id: parent, raw: obj }];
        }
    }
    // 6. result
    if ty == Some("result") {
        let cost_usd = obj.get("total_cost_usd").and_then(|v| v.as_f64());
        let text = result_text_field(&obj);
        let is_error = obj.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        return vec![ClaudeEvent::Result { is_error, cost_usd, text, raw: obj }];
    }
    // 7. user message — subagent input (text+parent) and results (tool_result)
    if ty == Some("user") {
        if let Some(content) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
            let mut out = Vec::new();
            for b in content {
                let bt = b.get("type").and_then(|v| v.as_str());
                if bt == Some("text") && parent.is_some() {
                    let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    out.push(ClaudeEvent::Text { text, parent_tool_use_id: parent.clone(), raw: obj.clone() });
                } else if bt == Some("tool_result") {
                    if let Some(tool_use_id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                        let text = result_text(b.get("content").unwrap_or(&Value::Null));
                        out.push(ClaudeEvent::AgentResult { tool_use_id: tool_use_id.to_string(), text, raw: obj.clone() });
                    }
                }
            }
            return out;
        }
    }
    // 8. assistant message — IMPLEMENTED IN TASK 3
    if ty == Some("assistant") {
        if let Some(_content) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
            // Task 3 fills this in. For now fall through to Other.
        }
    }

    vec![ClaudeEvent::Other { raw: obj }]
}

/// The `result` event's error/transcript text lives in a different field per result shape:
/// `result` (string), else `error` (string), else `errors` (string[] joined by "\n"; empty → None).
/// Mirror of TS `streamParser.ts` lines 59-63.
fn result_text_field(obj: &Value) -> Option<String> {
    if let Some(s) = obj.get("result").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = obj.get("error").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = obj.get("errors").and_then(|v| v.as_array()) {
        let joined = arr
            .iter()
            .filter_map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() { return None; }
        return Some(joined);
    }
    None
}
```
> **`obj.pointer("/event/delta/type")`** is serde_json's JSON-Pointer lookup — returns `Option<&Value>`, `None` if any segment is missing. It is the safe analogue of TS `obj.event?.delta?.type` (optional chaining). No panic on a missing intermediate.
> **`obj.clone()` in the `user` loop**: each emitted event owns a copy of `raw` (the TS code shares one object reference, but Rust ownership requires a clone per event). This matches the TS semantics — every event's `raw` is the same line object. Cloning a `serde_json::Value` is a deep clone; these lines are small, so this is fine.
- [ ] **Step 4: run `cargo test --lib stream::`, expect PASS** for all Task 1 + Task 2 tests (assistant tests not yet written). If `result_text_field` isn't referenced before its definition, that's fine — Rust resolves free functions regardless of order.
- [ ] **Step 5: commit** (commit-only, NEVER push):
```
git -C agentic-dev add server-rs/src/stream.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 2 T2: parse_line guards + simple events (init/text/retry/result/thinking/prompt/user)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: implement the assistant-message branch (skill / agent / workflow / ask / tool)

**Files:**
- Modify: `server-rs/src/stream.rs` (fill the assistant branch + add tests)
- Test: inline `#[cfg(test)]` in `src/stream.rs`

**Interfaces (exact Rust signatures):** unchanged.

- [ ] **Step 1: write the failing assistant tests** (mirror `streamParser.test.ts` cases: assistant-no-content → Other; Skill names + ordinary tool ordering; ordinary tool only; AskUserQuestion; Agent with type+description; subagent text tagged with parent — already covered by stream_event test; Workflow by name; multiple subagents → one Agent event with 2 agents; dynamic workflow name from script meta). Add to `mod tests`:
```rust
    #[test]
    fn assistant_with_no_content_is_other() {
        let line = json!({ "type": "assistant" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Other { .. } => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn extracts_skill_names_then_ordinary_tool_in_order() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "thinking" },
            { "type": "tool_use", "name": "Skill", "input": { "skill": "superpowers:writing-plans" } },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ] } }).to_string();
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 2);
        match &evs[0] {
            ClaudeEvent::Skill { names, parent_tool_use_id, .. } => {
                assert_eq!(names, &vec!["superpowers:writing-plans".to_string()]);
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Skill first, got {other:?}"),
        }
        match &evs[1] {
            ClaudeEvent::Tool { name, .. } => assert_eq!(name, "Bash"),
            other => panic!("expected Tool second, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_ordinary_tool_call_with_input() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Tool { name, input, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls"));
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn extracts_ask_user_question() {
        let questions = json!([{ "question": "A or B?", "header": "Pick",
            "options": [{ "label": "A", "description": "" }], "multiSelect": false }]);
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "AskUserQuestion", "input": { "questions": questions } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Ask { questions: q, .. } => {
                assert_eq!(q.len(), 1);
                assert_eq!(q[0].get("question").and_then(|v| v.as_str()), Some("A or B?"));
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn extracts_spawned_subagent_with_type_and_description() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_A", "name": "Agent",
              "input": { "subagent_type": "Explore", "description": "search repos" } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Agent { agents, parent_tool_use_id, .. } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0], SpawnedAgent {
                    id: "toolu_A".into(), agent_type: "Explore".into(), description: "search repos".into() });
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn extracts_workflow_tool_use_by_name() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_W", "name": "Workflow", "input": { "name": "review-changes" } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "toolu_W");
                assert_eq!(name, "review-changes");
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn yields_one_agent_event_with_multiple_subagents() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "a1", "name": "Agent", "input": { "subagent_type": "Explore", "description": "x" } },
            { "type": "tool_use", "id": "a2", "name": "Agent", "input": { "subagent_type": "Plan", "description": "y" } }
        ] } }).to_string();
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ClaudeEvent::Agent { agents, .. } => assert_eq!(agents.len(), 2),
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn extracts_dynamic_workflow_name_from_inline_script_meta() {
        let script = "export const meta = {\n  name: 'fix-bugs',\n  description: 'x',\n}\n// body";
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_w", "name": "Workflow", "input": { "script": script } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "toolu_w");
                assert_eq!(name, "fix-bugs");
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn agent_default_type_is_agent_when_subagent_type_missing() {
        // Parity guard: SpawnedAgent.agent_type default is the literal "agent" (not "Task").
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "t1", "name": "Task", "input": { "description": "d" } }
        ] } }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Agent { agents, .. } => {
                assert_eq!(agents[0].agent_type, "agent");
                assert_eq!(agents[0].id, "t1");
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }
```
- [ ] **Step 2: run `cargo test --lib stream::`, expect FAIL** (assistant branch still falls through to `Other`, so the new tests fail; the `assistant_with_no_content_is_other` one already passes).
- [ ] **Step 3: implement the assistant branch.** Replace the `// 8. assistant message — IMPLEMENTED IN TASK 3` block in `parse_line` with:
```rust
    // 8. assistant message — full tool_use inputs (Skill/Agent/Workflow/Ask + ordinary tools)
    if ty == Some("assistant") {
        if let Some(content) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
            let mut out = Vec::new();
            // tool_use blocks only
            let tools: Vec<&Value> = content
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                .collect();

            // Skill names
            let names: Vec<String> = tools
                .iter()
                .filter(|b| b.get("name").and_then(|v| v.as_str()) == Some("Skill"))
                .filter_map(|b| b.pointer("/input/skill").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if !names.is_empty() {
                out.push(ClaudeEvent::Skill { names, parent_tool_use_id: parent.clone(), raw: obj.clone() });
            }

            // Spawned subagents (Agent or Task with input)
            let agents: Vec<SpawnedAgent> = tools
                .iter()
                .filter(|b| {
                    let n = b.get("name").and_then(|v| v.as_str());
                    (n == Some("Agent") || n == Some("Task")) && b.get("input").is_some()
                })
                .map(|b| SpawnedAgent {
                    id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    agent_type: b.pointer("/input/subagent_type").and_then(|v| v.as_str()).unwrap_or("agent").to_string(),
                    description: b.pointer("/input/description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                })
                .collect();
            if !agents.is_empty() {
                out.push(ClaudeEvent::Agent { agents, parent_tool_use_id: parent.clone(), raw: obj.clone() });
            }

            // Workflows — one event per block, in array order
            for w in tools.iter().filter(|b| b.get("name").and_then(|v| v.as_str()) == Some("Workflow")) {
                let wf_name = w.pointer("/input/name").and_then(|v| v.as_str()).map(String::from)
                    .or_else(|| w.pointer("/input/title").and_then(|v| v.as_str()).map(String::from))
                    .or_else(|| meta_name(w.pointer("/input/script").unwrap_or(&Value::Null)))
                    .unwrap_or_else(|| "workflow".to_string());
                out.push(ClaudeEvent::Workflow {
                    id: w.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    name: wf_name,
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }

            // AskUserQuestion — first match only, questions must be an array
            if let Some(ask) = tools.iter().find(|b| {
                b.get("name").and_then(|v| v.as_str()) == Some("AskUserQuestion")
                    && b.pointer("/input/questions").map(|q| q.is_array()).unwrap_or(false)
            }) {
                let questions = ask.pointer("/input/questions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                out.push(ClaudeEvent::Ask { questions, parent_tool_use_id: parent.clone(), raw: obj.clone() });
            }

            // Every other tool call (Read/Edit/Bash/Write/…)
            const SPECIAL: [&str; 5] = ["Skill", "Agent", "Task", "Workflow", "AskUserQuestion"];
            for t in tools.iter().filter(|b| {
                let n = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                !SPECIAL.contains(&n)
            }) {
                out.push(ClaudeEvent::Tool {
                    name: t.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string(),
                    input: t.get("input").cloned().unwrap_or_else(|| Value::Object(Default::default())),
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }
            return out;
        }
    }
```
> **Parity note on the ordinary-tool filter** vs TS: TS does `tools.filter(b => !SPECIAL.has(b.name))`. A tool_use block with **no** `name` field: TS `b.name` is `undefined`, `SPECIAL.has(undefined)` is `false`, so it IS emitted as a `Tool` with `name="tool"` (the `String(t.name ?? "tool")` default). The Rust `.unwrap_or("")` makes a missing name an empty string `""`, which is also not in `SPECIAL`, so it is likewise emitted with `name="tool"` via the `t.get("name")...unwrap_or("tool")`. Behavior matches.
> **`?`-chaining note:** `w.pointer("/input/name")` returns `Option<&Value>`; `.and_then(|v| v.as_str()).map(String::from)` yields `Option<String>`; the `.or_else(...)` chain reproduces TS `a ?? b ?? c ?? "workflow"` left-to-right. `meta_name` already returns `Option<String>`.
- [ ] **Step 4: run `cargo test --lib stream::`, expect PASS** for all stream tests (Task 1 + 2 + 3). Then run the FULL suite `cargo test` from `server-rs/` and confirm the whole crate is still green (no regressions in store/transcript/api/auth/config).
- [ ] **Step 5: commit** (commit-only, NEVER push):
```
git -C agentic-dev add server-rs/src/stream.rs
git -C agentic-dev commit -m "$(cat <<'EOF'
Phase 2 T3: assistant-message branch (skill/agent/workflow/ask/tool) — parser complete

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Definition of done

- `server-rs/src/stream.rs` exists with `parse_line`, `ClaudeEvent`, `SpawnedAgent`, `result_text`, `result_text_field`, `meta_name`.
- Every `streamParser.test.ts` case is mirrored by a Rust test and passes.
- `cargo test` (whole crate) is green.
- `regex` is the only new dependency.
- `mod stream;` is registered in `main.rs`. No HTTP/engine wiring (deferred to Phase 4/5).
- Three commits (T1 skeleton, T2 simple events, T3 assistant branch); none pushed.
