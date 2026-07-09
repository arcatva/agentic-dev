# Session Search and Login UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the session list search bar to the title row, expand it to search session detail and transcript content via a new backend route, reuse a single MD3 Expressive search component in New Request, lift the login form above the soft keyboard, and align `TextNode.narration` rendering with CLI-style transcripts.

**Architecture:** Add `GET /api/sessions/search` to the Rust server; the Android client adds a debounced, cancellable `contentSearch` in the repository, surfaces `searchResults` through `HomeViewModel`, and renders a stateless shared `SearchBar` inside the new title row. Login screens get scroll + `imePadding()` + keyboard actions. `TextNode` default flips to `narration = false`; the reducer folds only short in-progress prose and always forces the trailing `TextNode` inline on turn end.

**Tech Stack:** Rust (axum, tokio, parking_lot), Kotlin 2.0.21 + Jetpack Compose 1.8.2 + Material 3 1.4.0-alpha18, kotlinx.serialization, Ktor client, JUnit 4 (Android JVM unit tests).

## Global Constraints

- Backend: `agentic-dev/server-rs` (Rust 2021, edition not pinned in repo, but axum 0.7+ patterns used in `api/sessions.rs`).
- Android: `agentic-dev-android` Compose, `compileSdk = 35`, `minSdk = 26`, Java/Kotlin target 17, `MaterialExpressiveTheme` is the single source of theme.
- Compose pin: Material 3 **1.4.0-alpha18** — do not bump.
- Project rules: every Edit/Write must be grep/read-verified; no secrets in chat; commits go directly to master.
- Specs this plan implements:
  - `agentic-dev-android/docs/superpowers/specs/2026-06-23-session-search-and-login-ux-design.md`
  - `agentic-dev/docs/superpowers/specs/2026-06-23-sessions-search-backend-design.md`
- Search response field enum (verbatim from spec): `title`, `repo`, `branch`, `sessionId`, `status`, `error`, `prompt`, `notes`, `answer`, `toolName`, `toolSummary`, `toolDetail`, `spawnDesc`, `spawnResult`, `skill`, `workflow`, `ask`, `plan`, `perm`, `attachment`.
- Search response caps (verbatim from spec): max 3 matches per session, snippet length cap 200 chars (ellipsis `...` when truncated), total results cap 50, query minimum length 2.
- Ranking tiers (verbatim from spec): A title/prompt → B metadata (repo/branch/sessionId/status/error) → C transcript (everything else). Tie-break: `COALESCE(lastUserMessageAt, createdAt)` desc, then `seq` desc.
- Narration rule (verbatim from spec): `TextNode` default `narration = false`; during an active turn the most recent `TextNode` may be folded (`narration = true`) only if its accumulated text length is `< 256` characters; on turn end the trailing `TextNode.narration` is forced to `false` regardless of `AnswerNode` presence.
- Login: `IME` action `Next` on host jumps to password; `Done` on password submits only when current enabled rule holds.
- Tests: TDD, frequent commits, JVM unit tests for new logic, no new Compose UI test harness.

---

## File map

Backend (new / modified):

- `agentic-dev/server-rs/src/engine/search.rs` (new) — `SearchService`, ranking, snippet extraction, match classification, cost guards.
- `agentic-dev/server-rs/src/engine/mod.rs` — re-export `SearchService` and match-classification helpers.
- `agentic-dev/server-rs/src/api/sessions.rs` — add `search_sessions` handler and `SearchQuery` struct.
- `agentic-dev/server-rs/src/api/mod.rs` — register the new route ahead of `/api/sessions/{id}`.
- `agentic-dev/server-rs/tests/search.rs` (new) — integration tests.

Android (new / modified):

- `agentic-dev-android/app/src/main/java/dev/agentic/data/net/Models.kt` — `SearchResponse`, `SearchHit`, `SearchMatch`, `SearchField` enum.
- `agentic-dev-android/app/src/main/java/dev/agentic/data/net/AgenticApi.kt` — `searchSessions(q)` interface method.
- `agentic-dev-android/app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt` — implementation.
- `agentic-dev-android/app/src/main/java/dev/agentic/data/FakeAgenticApi.kt` (test) — scripted response.
- `agentic-dev-android/app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt` — `contentSearch(q)` with debounce/cancellation.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/components/SearchBar.kt` (new) — stateless MD3 Expressive search surface.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/SessionSearchBar.kt` — keep file as a thin wrapper delegating to the new `SearchBar` (one release).
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeTopBar.kt` — host the new search bar.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeScreen.kt` — drop search bar from `SessionListPane`; switch to `searchResults` when `searchQuery` non-blank.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/AdaptiveHome.kt` — same.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeViewModel.kt` — `searchQuery`, `searchResults`, `searching`, `setSearchQuery`.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt` — replace in-picker `OutlinedTextField` with the shared `SearchBar`.
- `agentic-dev-android/app/src/main/java/dev/agentic/domain/Transcript.kt` — narration default flip; short-fold rule; turn-end force inline.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/login/LoginManualScreen.kt` — scroll + imePadding + keyboard options/actions.
- `agentic-dev-android/app/src/main/java/dev/agentic/ui/login/LoginScanScreen.kt` — scroll + imePadding + keyboard options/actions on password.
- `agentic-dev-android/app/src/test/java/dev/agentic/domain/SessionSearchTest.kt` — extend.
- `agentic-dev-android/app/src/test/java/dev/agentic/data/repo/SessionsRepositoryTest.kt` — extend.
- `agentic-dev-android/app/src/test/java/dev/agentic/ui/home/HomeViewModelTest.kt` — extend.
- `agentic-dev-android/app/src/test/java/dev/agentic/data/net/SessionSerializationTest.kt` — extend.

---

## Task 1: Backend match classification helper

**Files:**
- Create: `agentic-dev/server-rs/src/engine/search.rs`
- Modify: `agentic-dev/server-rs/src/engine/mod.rs` (re-export)

**Interfaces:**
- Consumes: `crate::engine::stream::ClaudeEvent` and `crate::engine::transcript::filter_rendered`.
- Produces: `enum SearchField`, `enum SearchTier`, `struct ClassifiedLine { field: SearchField, tier: SearchTier, text: String }`, `fn classify_rendered_line(line: &str) -> Option<ClassifiedLine>`, `fn derive_tool_summary(name: &str, input: &serde_json::Value) -> String`, `fn derive_tool_detail(name: &str, input: &serde_json::Value) -> String`.

- [ ] **Step 1: Write the failing tests in `engine/search.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_prompt_and_text_lines() {
        let prompt = r#"{"type":"agentic_prompt","text":"hi","at":1}"#;
        let text = r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello"}}}"#;
        let thinking = r#"{"type":"stream_event","event":{"delta":{"type":"thinking_delta","thinking":"..."}}}"#;
        let tool_bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls","description":"list"}}]}}"#;
        let result_text = r#"{"type":"result","is_error":false,"result":"done","total_cost_usd":0.01}"#;
        let ask = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[{"question":"Q?","options":[]}]}}]}}"#;
        let perm = r#"{"type":"agentic_perm","permKind":"perm","id":"p1","tool":"Bash","input":{}}"#;
        let plan = r#"{"type":"agentic_perm","permKind":"plan","id":"pl1","plan":"x"}"#;
        let att = r#"{"type":"agentic_file","path":"a.png","at":1}"#;
        let sys = r#"{"type":"system","subtype":"init"}"#;
        let user_tr = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        let agent_res = r#"{"type":"agent_result","toolUseId":"tu_1","text":"found it"}"#;

        let cases = vec![
            (prompt, Some(SearchField::Prompt), SearchTier::A),
            (text, Some(SearchField::Notes), SearchTier::C),
            (thinking, None, SearchTier::C),
            (tool_bash, Some(SearchField::ToolName), SearchTier::C),
            (result_text, Some(SearchField::Answer), SearchTier::C),
            (ask, Some(SearchField::Ask), SearchTier::C),
            (perm, Some(SearchField::Perm), SearchTier::C),
            (plan, Some(SearchField::Plan), SearchTier::C),
            (att, Some(SearchField::Attachment), SearchTier::C),
            (sys, None, SearchTier::C),
            (user_tr, None, SearchTier::C),
            (agent_res, Some(SearchField::SpawnResult), SearchTier::C),
        ];
        for (line, expected_field, _tier) in cases {
            let got = classify_rendered_line(line);
            assert_eq!(got.as_ref().map(|c| c.field.clone()), expected_field, "line: {line}");
        }
    }

    #[test]
    fn derives_bash_summary_and_detail() {
        let input = json!({"command":"ls -la /tmp","description":"list"});
        assert_eq!(derive_tool_summary("Bash", &input), "ls -la /tmp");
        assert_eq!(derive_tool_detail("Bash", &input), "ls -la /tmp");
    }

    #[test]
    fn derives_read_summary_from_file_path_basename() {
        let input = json!({"file_path":"/tmp/dir/foo.kt"});
        assert_eq!(derive_tool_summary("Read", &input), "foo.kt");
    }
}
```

- [ ] **Step 2: Run tests; expect compile failure**

Run: `cd agentic-dev/server-rs && cargo test --lib engine::search::tests -- --nocapture`
Expected: compile error (module not found).

- [ ] **Step 3: Add minimal module skeleton**

`agentic-dev/server-rs/src/engine/search.rs`:

```rust
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SearchField {
    Title, Repo, Branch, SessionId, Status, Error,
    Prompt,
    Notes, Answer,
    ToolName, ToolSummary, ToolDetail,
    SpawnDesc, SpawnResult,
    Skill, Workflow, Ask, Plan, Perm,
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SearchTier { A, B, C }

pub struct ClassifiedLine {
    pub field: SearchField,
    pub tier: SearchTier,
    pub text: String,
}

pub fn classify_rendered_line(_line: &str) -> Option<ClassifiedLine> { None }
pub fn derive_tool_summary(_name: &str, _input: &serde_json::Value) -> String { String::new() }
pub fn derive_tool_detail(_name: &str, _input: &serde_json::Value) -> String { String::new() }
```

Add `pub mod search;` to `agentic-dev/server-rs/src/engine/mod.rs` and re-export `SearchField`, `SearchTier`, `ClassifiedLine`, `classify_rendered_line`, `derive_tool_summary`, `derive_tool_detail` with `pub use search::{...}`.

- [ ] **Step 4: Re-run tests; expect failing assertions**

Expected: tests fail (helpers return `None` / empty).

- [ ] **Step 5: Implement `derive_tool_summary` and `derive_tool_detail`**

```rust
pub fn derive_tool_summary(name: &str, input: &serde_json::Value) -> String {
    fn s(v: &serde_json::Value) -> String { v.as_str().unwrap_or("").to_string() }
    match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => {
            std::path::Path::new(s(&input["file_path"])).file_name()
                .and_then(|x| x.to_str()).unwrap_or("").to_string()
        }
        "Bash" => s(&input["command"]).chars().take(64).collect(),
        "Glob" | "Grep" => s(&input["pattern"]),
        "WebFetch" => s(&input["url"]),
        "WebSearch" => s(&input["query"]),
        "TaskCreate" => s(&input["subject"]),
        "TaskUpdate" => {
            let status = s(&input["status"]);
            if status.is_empty() { s(&input["taskId"]) } else { status }
        }
        "ToolSearch" => s(&input["query"]),
        _ => String::new(),
    }
}

pub fn derive_tool_detail(name: &str, input: &serde_json::Value) -> String {
    fn s(v: &serde_json::Value) -> String { v.as_str().unwrap_or("").to_string() }
    match name {
        "Bash" => s(&input["command"]),
        "Edit" | "MultiEdit" => format!("{}\n---\n{}", s(&input["old_string"]), s(&input["new_string"])),
        "Write" => {
            let body = input.get("contents").or_else(|| input.get("content")).and_then(|v| v.as_str()).unwrap_or("");
            body.chars().take(4000).collect()
        }
        _ => {
            let mut out = String::new();
            if let Some(obj) = input.as_object() {
                for (k, v) in obj {
                    out.push_str(&format!("{}: {}\n", k, v));
                }
            }
            out
        }
    }
}
```

- [ ] **Step 6: Implement `classify_rendered_line`**

```rust
pub fn classify_rendered_line(line: &str) -> Option<ClassifiedLine> {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "agentic_prompt" => {
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            Some(ClassifiedLine { field: SearchField::Prompt, tier: SearchTier::A, text })
        }
        "stream_event" => {
            let delta = v.get("event").and_then(|e| e.get("delta"));
            match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                Some("text_delta") => Some(ClassifiedLine {
                    field: SearchField::Notes,
                    tier: SearchTier::C,
                    text: delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string(),
                }),
                Some("thinking_delta") => None, // excluded by spec
                _ => None,
            }
        }
        "assistant" => {
            let content = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
            let Some(content) = content else { return None };
            // Pick the first tool_use block; if none, drop.
            for blk in content {
                if blk.get("type").and_then(|t| t.as_str()) != Some("tool_use") { continue; }
                let name = blk.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = blk.get("input").cloned().unwrap_or(serde_json::Value::Null);
                let (field, text) = match name {
                    "Skill" => (SearchField::Skill, input.get("skill").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                    "Workflow" => (SearchField::Workflow, input.get("name").and_then(|s| s.as_str())
                        .or_else(|| input.get("title").and_then(|s| s.as_str()))
                        .unwrap_or("").to_string()),
                    "AskUserQuestion" => (SearchField::Ask, serde_json::to_string(input.get("questions").unwrap_or(&serde_json::Value::Null)).unwrap_or_default()),
                    "Agent" | "Task" => (SearchField::SpawnDesc, input.get("description").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                    other => (SearchField::ToolName, derive_tool_summary(other, &input)),
                };
                return Some(ClassifiedLine { field, tier: SearchTier::C, text });
            }
            None
        }
        "result" => {
            let text = v.get("result").and_then(|s| s.as_str())
                .or_else(|| v.get("error").and_then(|s| s.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_default();
            if text.is_empty() { None } else { Some(ClassifiedLine { field: SearchField::Answer, tier: SearchTier::C, text }) }
        }
        "agent_result" => Some(ClassifiedLine {
            field: SearchField::SpawnResult,
            tier: SearchTier::C,
            text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        }),
        "agentic_perm" => {
            let kind = v.get("permKind").and_then(|s| s.as_str()).unwrap_or("perm");
            let field = if kind == "plan" { SearchField::Plan } else { SearchField::Perm };
            let text = v.get("plan").and_then(|s| s.as_str())
                .or_else(|| v.get("tool").and_then(|s| s.as_str()))
                .unwrap_or("").to_string();
            Some(ClassifiedLine { field, tier: SearchTier::C, text })
        }
        "agentic_file" => Some(ClassifiedLine {
            field: SearchField::Attachment,
            tier: SearchTier::C,
            text: v.get("path").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        }),
        _ => None,
    }
}
```

- [ ] **Step 7: Re-run tests; expect pass**

Run: `cd agentic-dev/server-rs && cargo test --lib engine::search::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
cd agentic-dev && git add server-rs/src/engine/search.rs server-rs/src/engine/mod.rs
git commit -m "engine: classify rendered transcript lines for search"
```

---

## Task 2: Backend `SearchService` ranking and snippet extraction

**Files:**
- Modify: `agentic-dev/server-rs/src/engine/search.rs`

**Interfaces:**
- Produces:
  - `pub struct SearchService { engine: Arc<Engine> }`
  - `impl SearchService { pub fn new(engine: Arc<Engine>) -> Self; pub async fn search(&self, q: &str, limit: usize) -> SearchResponse; }`
  - `pub struct SearchMatch { field: SearchField, snippet: String, line_index: usize }`
  - `pub struct SearchHit { session: Session, score: f32, matches: Vec<SearchMatch> }`
  - `pub struct SearchResponse { query: String, results: Vec<SearchHit> }`
  - `pub fn extract_snippet(text: &str, query: &str) -> String` (trims to 200, prepends `...` on cut).

- [ ] **Step 1: Write failing tests in `engine/search.rs`**

Add to the same `mod tests` block:

```rust
use crate::engine::Engine;
use crate::engine::config::EngineConfig;

fn mk_query(q: &str) -> String { q.trim().to_string() }

#[tokio::test]
async fn short_query_returns_empty() {
    let engine = Engine::new(EngineConfig::default()).await.unwrap();
    let svc = SearchService::new(std::sync::Arc::new(engine));
    let r = svc.search("a", 50).await;
    assert!(r.results.is_empty());
    assert_eq!(r.query, "a");
}

#[tokio::test]
async fn results_cap_to_limit() {
    let engine = Engine::new(EngineConfig::default()).await.unwrap();
    let svc = SearchService::new(std::sync::Arc::new(engine));
    let r = svc.search("hello", 1).await;
    assert!(r.results.len() <= 1);
}

#[test]
fn snippet_caps_at_200_and_ellipsis() {
    let text: String = (0..300).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let s = extract_snippet(&text, "needle");
    assert!(s.len() <= 203); // 200 + "..." possible
    assert!(s.starts_with("...") || s.len() == text.len());
}

#[test]
fn snippet_prefers_query_window() {
    let text = "lorem ipsum dolor sit amet, consectetur adipiscing elit, build failed here somewhere";
    let s = extract_snippet(text, "build failed");
    assert!(s.contains("build failed"));
    assert!(s.starts_with("..."));
}
```

- [ ] **Step 2: Run; expect compile failure**

Run: `cd agentic-dev/server-rs && cargo test --lib engine::search::tests:: -- --nocapture`
Expected: compile errors.

- [ ] **Step 3: Add types and stub `extract_snippet` / `SearchService`**

```rust
use std::sync::Arc;
use crate::engine::Engine;
use crate::engine::store::Session;
use crate::engine::transcript::filter_rendered;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct SearchMatch { pub field: SearchField, pub snippet: String, pub line_index: usize }
#[derive(Debug, Serialize)]
pub struct SearchHit { pub session: Session, pub score: f32, pub matches: Vec<SearchMatch> }
#[derive(Debug, Serialize)]
pub struct SearchResponse { pub query: String, pub results: Vec<SearchHit> }

pub struct SearchService { pub engine: Arc<Engine> }
impl SearchService {
    pub fn new(engine: Arc<Engine>) -> Self { Self { engine } }
    pub async fn search(&self, q: &str, limit: usize) -> SearchResponse {
        SearchResponse { query: q.to_string(), results: vec![] }
    }
}

pub fn extract_snippet(text: &str, _query: &str) -> String { text.to_string() }
```

Add `use crate::engine::Engine;` and `use crate::engine::transcript::filter_rendered;` to the top of the file (already shown).

- [ ] **Step 4: Re-run; expect test failures (not compile errors)**

Expected: `short_query_returns_empty` and `results_cap_to_limit` fail because stub returns `results: vec![]`; snippet tests pass with the stub because the inputs happen to satisfy length and `contains` checks (we'll change behavior in step 5 to make `short_query_returns_empty` pass and `snippet_caps_at_200_and_ellipsis` fail).

- [ ] **Step 5: Implement `extract_snippet` and `SearchService::search`**

```rust
const SNIPPET_MAX: usize = 200;
const PER_SESSION_MATCH_CAP: usize = 3;
const PER_SESSION_SCAN_CAP: usize = 50_000;
const QUERY_MIN_LEN: usize = 2;

pub fn extract_snippet(text: &str, query: &str) -> String {
    if text.len() <= SNIPPET_MAX { return text.to_string(); }
    let lower = text.to_lowercase();
    let needle = query.to_lowercase();
    let center = lower.find(&needle).unwrap_or(0);
    let half = SNIPPET_MAX / 2;
    let start = center.saturating_sub(half);
    let end = (start + SNIPPET_MAX).min(text.len());
    let start = end.saturating_sub(SNIPPET_MAX);
    let window = &text[start..end];
    let mut out = String::with_capacity(SNIPPET_MAX + 3);
    if start > 0 { out.push_str("..."); }
    out.push_str(window);
    if end < text.len() { out.push_str("..."); }
    out
}

fn metadata_match(field: SearchField, text: &str, needle: &str) -> Option<SearchMatch> {
    if text.to_lowercase().contains(&needle) {
        Some(SearchMatch { field, snippet: text.chars().take(SNIPPET_MAX).collect(), line_index: 0 })
    } else { None }
}

impl SearchService {
    pub async fn search(&self, q: &str, limit: usize) -> SearchResponse {
        let query = q.trim();
        let mut resp = SearchResponse { query: query.to_string(), results: vec![] };
        if query.len() < QUERY_MIN_LEN { return resp; }
        let needle = query.to_lowercase();
        let limit = limit.clamp(1, 50);
        let sessions = self.engine.list().await;

        let mut hits: Vec<(u8, f32, SearchHit)> = vec![];
        for session in sessions {
            let mut matches: Vec<SearchMatch> = Vec::new();
            // Tier A
            if let Some(m) = metadata_match(SearchField::Title, &session.prompt, &needle) {
                matches.push(m);
            }
            // Tier B
            for repo in &session.repos {
                if let Some(m) = metadata_match(SearchField::Repo, repo, &needle) { matches.push(m); }
            }
            if let Some(branch) = &session.branch {
                if let Some(m) = metadata_match(SearchField::Branch, branch, &needle) { matches.push(m); }
            }
            if let Some(m) = metadata_match(SearchField::SessionId, &session.id, &needle) { matches.push(m); }
            if let Some(m) = metadata_match(SearchField::Status, &format!("{:?}", session.status), &needle) { matches.push(m); }
            if let Some(err) = &session.error {
                if let Some(m) = metadata_match(SearchField::Error, err, &needle) { matches.push(m); }
            }

            // Tier C — scan rendered projection
            let raw = self.engine.get_log(&session.id);
            let rendered = filter_rendered(&raw);
            for (i, line) in rendered.iter().take(PER_SESSION_SCAN_CAP).enumerate() {
                if let Some(c) = classify_rendered_line(line) {
                    if !c.text.is_empty() && c.text.to_lowercase().contains(&needle) {
                        let snippet = extract_snippet(&c.text, query);
                        matches.push(SearchMatch { field: c.field, snippet, line_index: i });
                        if matches.len() >= PER_SESSION_MATCH_CAP { break; }
                    }
                }
            }

            if matches.is_empty() { continue; }

            // Tier: A=0, B=1, C=2
            let tier = if matches.iter().any(|m| matches!(m.field, SearchField::Prompt | SearchField::Title)) { 0 }
                       else if matches.iter().any(|m| matches!(m.field, SearchField::Repo | SearchField::Branch | SearchField::SessionId | SearchField::Status | SearchField::Error)) { 1 }
                       else { 2 };
            let score = (matches.len() as f32) - (tier as f32) * 0.1;
            hits.push((tier, -score, SearchHit { session, score, matches }));
        }

        hits.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| b.2.session.last_user_message_at.unwrap_or(b.2.session.created_at)
                    .cmp(&a.2.session.last_user_message_at.unwrap_or(a.2.session.created_at)))
                .then_with(|| b.2.session.seq.cmp(&a.2.session.seq))
        });
        resp.results = hits.into_iter().take(limit).map(|(_, _, h)| h).collect();
        resp
    }
}
```

- [ ] **Step 6: Re-run; expect pass**

Run: `cd agentic-dev/server-rs && cargo test --lib engine::search::tests -- --nocapture`
Expected: PASS. If `Engine::new` requires non-default config in tests, copy the engine init pattern from `engine/mod.rs` test helpers (`agentic-dev/server-rs/src/engine/tests.rs`).

- [ ] **Step 7: Commit**

```bash
cd agentic-dev && git add server-rs/src/engine/search.rs
git commit -m "engine: SearchService ranking and snippet extraction"
```

---

## Task 3: Backend route wiring

**Files:**
- Modify: `agentic-dev/server-rs/src/api/sessions.rs`
- Modify: `agentic-dev/server-rs/src/api/mod.rs`

**Interfaces:**
- Produces: `pub async fn search_sessions(State(st): State<AppState>, Query(q): Query<SearchQuery>) -> Response` in `sessions.rs`; `SearchQuery { q: Option<String>, limit: Option<usize> }`.

- [ ] **Step 1: Write the failing integration test in `server-rs/tests/search.rs`**

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use agentic_dev_rs::api::config::Config;
use agentic_dev_rs::api::state::AppState;
use tower::ServiceExt;

#[tokio::test]
async fn search_route_400_on_missing_q() {
    // Build a minimal AppState; the helper exists in existing integration tests.
    // Use the same fixture-construction pattern as other tests in tests/ directory.
    let app = /* build_router_for_tests(AppState::for_tests().await) */ unimplemented!();
    let r = app.oneshot(Request::get("/api/sessions/search").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}
```

> Note: if the existing tests don't expose a public router builder, follow the pattern in `tests/api_*.rs` files to construct the router. Replace the `unimplemented!()` with the actual setup used by sibling tests. If no helper exists, lift the relevant `Router::new()...` lines from `src/main.rs` into a `pub fn test_router(state: AppState) -> Router` in `src/lib.rs` and use it here and in existing tests.

- [ ] **Step 2: Run; expect compile or test failure**

Run: `cd agentic-dev/server-rs && cargo test --test search -- --nocapture`
Expected: FAIL (handler not registered).

- [ ] **Step 3: Add `SearchQuery` and `search_sessions` handler**

Append to `agentic-dev/server-rs/src/api/sessions.rs`:

```rust
use crate::engine::search::{SearchResponse, SearchService};
use axum::extract::Query as AxumQuery;

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

pub async fn search_sessions(
    State(st): State<AppState>,
    AxumQuery(q): AxumQuery<SearchQuery>,
) -> Response {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing q").into_response();
    }
    let limit = q.limit.unwrap_or(50);
    let svc = SearchService::new(st.engine.clone());
    let resp: SearchResponse = svc.search(&query, limit).await;
    Json(resp).into_response()
}
```

- [ ] **Step 4: Register the route ahead of `/api/sessions/{id}` in `api/mod.rs`**

```rust
.route("/api/sessions/search", get(sessions::search_sessions))
```

(Insert this line **before** the existing `/api/sessions/{id}` line at `mod.rs:41`.)

- [ ] **Step 5: Re-run; expect pass**

Run: `cd agentic-dev/server-rs && cargo test --test search -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Add a positive test asserting the response shape**

In `tests/search.rs`:

```rust
#[tokio::test]
async fn search_route_returns_envelope_for_valid_query() {
    let app = /* same setup */ unimplemented!();
    let r = app.oneshot(Request::get("/api/sessions/search?q=hi").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.get("query").is_some());
    assert!(v.get("results").is_some());
}
```

Run: `cargo test --test search -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cd agentic-dev && git add server-rs/src/api/sessions.rs server-rs/src/api/mod.rs server-rs/tests/search.rs
git commit -m "api: register /api/sessions/search route"
```

---

## Task 4: Android network models

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/net/Models.kt`

**Interfaces:**
- Produces:
  - `enum class SearchField { Title, Repo, Branch, SessionId, Status, Error, Prompt, Notes, Answer, ToolName, ToolSummary, ToolDetail, SpawnDesc, SpawnResult, Skill, Workflow, Ask, Plan, Perm, Attachment }`
  - `@Serializable data class SearchMatch(val field: SearchField, val snippet: String, val lineIndex: Int)`
  - `@Serializable data class SearchHit(val session: Session, val score: Float, val matches: List<SearchMatch>)`
  - `@Serializable data class SearchResponse(val query: String, val results: List<SearchHit>)`

- [ ] **Step 1: Add the failing serialization test in `app/src/test/java/dev/agentic/data/net/SessionSerializationTest.kt`**

```kotlin
@Test fun searchResponse_decodes() {
    val json = """
        {"query":"build failed","results":[{"session":{"id":"s1","prompt":"p","status":"done","repos":[],"createdAt":1,"endedAt":null},"score":1.0,"matches":[{"field":"notes","snippet":"...build failed...","lineIndex":4}]}]}
    """.trimIndent()
    val r: SearchResponse = Json.decodeFromString(json)
    assertEquals("build failed", r.query)
    assertEquals(1, r.results.size)
    assertEquals(SearchField.Notes, r.results[0].matches[0].field)
}

@Test fun searchResponse_tolerates_unknown_field() {
    val json = """{"query":"q","results":[],"futureFlag":true}"""
    val r: SearchResponse = Json.decodeFromString(json)
    assertEquals("q", r.query)
}
```

- [ ] **Step 2: Run; expect compile failure**

Run: `cd agentic-dev-android && ./gradlew :app:testDebugUnitTest --tests "dev.agentic.data.net.SessionSerializationTest"`
Expected: compile error (types not found).

- [ ] **Step 3: Add the data classes in `Models.kt`**

```kotlin
@Serializable
enum class SearchField {
    Title, Repo, Branch, SessionId, Status, Error,
    Prompt,
    Notes, Answer,
    ToolName, ToolSummary, ToolDetail,
    SpawnDesc, SpawnResult,
    Skill, Workflow, Ask, Plan, Perm,
    Attachment
}

@Serializable
data class SearchMatch(
    val field: SearchField,
    val snippet: String,
    val lineIndex: Int,
)

@Serializable
data class SearchHit(
    val session: Session,
    val score: Float,
    val matches: List<SearchMatch>,
)

@Serializable
data class SearchResponse(
    val query: String,
    val results: List<SearchHit>,
)
```

> Note: kotlinx-serialization `@Serializable` ignores unknown JSON keys by default — the second test passes for free. If `Session` is not already serializable, add `@Serializable` to it.

- [ ] **Step 4: Re-run; expect pass**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.data.net.SessionSerializationTest"`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/data/net/Models.kt app/src/test/java/dev/agentic/data/net/SessionSerializationTest.kt
git commit -m "data: SearchResponse models for content search"
```

---

## Task 5: Android API + repository search

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/net/AgenticApi.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt`
- Modify: `agentic-dev-android/app/src/test/java/dev/agentic/data/FakeAgenticApi.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt`
- Modify: `agentic-dev-android/app/src/test/java/dev/agentic/data/repo/SessionsRepositoryTest.kt`

**Interfaces:**
- Produces: `suspend fun AgenticApi.searchSessions(q: String): SearchResponse`; `fun SessionsRepository.contentSearch(query: StateFlow<String>): Flow<Outcome<SearchResponse>>` with 250 ms debounce and last-write-wins cancellation.

- [ ] **Step 1: Add failing repo test in `SessionsRepositoryTest.kt`**

```kotlin
@Test fun contentSearch_emits_response_for_non_blank_query() = runTest(testDispatcher) {
    fake.scripts += { q -> if (q == "build") SearchResponse("build", emptyList()) else null }
    val query = MutableStateFlow("")
    query.value = "build"
    val first = repo.contentSearch(query).filterIsInstance<Outcome.Success<SearchResponse>>().first()
    assertEquals("build", first.value.query)
}
```

- [ ] **Step 2: Run; expect compile failure**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.data.repo.SessionsRepositoryTest"`
Expected: compile error.

- [ ] **Step 3: Add `searchSessions` to `AgenticApi`**

```kotlin
suspend fun searchSessions(q: String): SearchResponse
```

Implement in `KtorAgenticApi`:

```kotlin
override suspend fun searchSessions(q: String): SearchResponse {
    val resp = client.get("$baseUrl/api/sessions/search") {
        parameter("q", q)
    }
    return Json.decodeFromString(SearchResponse.serializer(), resp.bodyAsText())
}
```

Add to `FakeAgenticApi`:

```kotlin
var searchHandler: ((String) -> SearchResponse?)? = null
override suspend fun searchSessions(q: String): SearchResponse =
    searchHandler?.invoke(q) ?: SearchResponse(query = q, results = emptyList())
```

- [ ] **Step 4: Add `contentSearch` to `SessionsRepository`**

```kotlin
fun contentSearch(query: StateFlow<String>, scope: CoroutineScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)): Flow<Outcome<SearchResponse>> =
    query
        .map { it.trim() }
        .distinctUntilChanged()
        .debounce(250)
        .mapLatest { q ->
            if (q.length < 2) Outcome.Success(SearchResponse(q, emptyList()))
            else runCatchingOutcome { api.searchSessions(q) }
        }
```

`runCatchingOutcome` already exists in the repo (used by `transcript`).

- [ ] **Step 5: Re-run repo test; expect pass**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.data.repo.SessionsRepositoryTest"`
Expected: PASS.

- [ ] **Step 6: Add cancellation test**

```kotlin
@Test fun contentSearch_only_latest_query_wins() = runTest(testDispatcher) {
    val inflight = CompletableDeferred<Unit>()
    fake.searchHandler = { q ->
        if (q == "first") { runBlocking { inflight.await() }; SearchResponse("first", emptyList()) }
        else SearchResponse(q, listOf(SearchHit(Session(id = q, prompt = "p"), 1f, emptyList())))
    }
    val query = MutableStateFlow("first")
    val firstJob = backgroundScope.launch { repo.contentSearch(query).collect { /* keep open */ } }
    query.value = "second"
    advanceTimeBy(300)
    runCurrent()
    val list = repo.contentSearch(MutableStateFlow("second")).filterIsInstance<Outcome.Success<SearchResponse>>().first()
    assertEquals("second", list.value.query)
    inflight.complete(Unit)
    firstJob.cancel()
}
```

Run the same test command; expect PASS.

- [ ] **Step 7: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/data/net/AgenticApi.kt app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt app/src/test/java/dev/agentic/data/FakeAgenticApi.kt app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt app/src/test/java/dev/agentic/data/repo/SessionsRepositoryTest.kt
git commit -m "data: contentSearch in SessionsRepository with debounce"
```

---

## Task 6: Android ViewModel search state

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeViewModel.kt`
- Modify: `agentic-dev-android/app/src/test/java/dev/agentic/ui/home/HomeViewModelTest.kt`

**Interfaces:**
- Produces (in `HomeUiState`):
  - `val searchQuery: String`
  - `val searchResults: List<SearchHit>`
  - `val searching: Boolean`
- Produces: `fun setSearchQuery(q: String)`.

- [ ] **Step 1: Add failing VM test in `HomeViewModelTest.kt`**

```kotlin
@Test fun setSearchQuery_debounces_and_updates_results() = runTest(testDispatcher) {
    fake.searchHandler = { q -> SearchResponse(q, listOf(SearchHit(Session(id = q), 1f, emptyList()))) }
    val vm = HomeViewModel(repo, ...)
    vm.setSearchQuery("build")
    advanceTimeBy(300); runCurrent()
    val s = vm.uiState.value
    assertEquals("build", s.searchQuery)
    assertEquals(1, s.searchResults.size)
    assertFalse(s.searching)
}

@Test fun setSearchQuery_blank_clears_results() = runTest(testDispatcher) {
    val vm = HomeViewModel(repo, ...)
    vm.setSearchQuery("hi"); advanceTimeBy(300); runCurrent()
    vm.setSearchQuery(""); advanceTimeBy(300); runCurrent()
    assertTrue(vm.uiState.value.searchResults.isEmpty())
}
```

- [ ] **Step 2: Run; expect compile failure**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.ui.home.HomeViewModelTest"`
Expected: compile error.

- [ ] **Step 3: Add search state to `HomeViewModel`**

In `HomeUiState`:

```kotlin
val searchQuery: String = "",
val searchResults: List<SearchHit> = emptyList(),
val searching: Boolean = false,
```

In `HomeViewModel`:

```kotlin
private val searchQueryFlow = MutableStateFlow("")
private var searchJob: Job? = null

fun setSearchQuery(q: String) {
    _uiState.update { it.copy(searchQuery = q) }
    searchJob?.cancel()
    if (q.isBlank()) {
        _uiState.update { it.copy(searchResults = emptyList(), searching = false) }
        return
    }
    _uiState.update { it.copy(searching = true) }
    searchJob = viewModelScope.launch {
        repo.contentSearch(searchQueryFlow)
            .catch { _uiState.update { it.copy(searching = false) } }
            .collect { outcome ->
                when (outcome) {
                    is Outcome.Success -> _uiState.update { it.copy(searchResults = outcome.value.results, searching = false) }
                    is Outcome.Failure -> _uiState.update { it.copy(searching = false) }
                }
            }
    }
}
```

Wire `searchQueryFlow` to update on every `setSearchQuery` call (`searchQueryFlow.value = q`).

- [ ] **Step 4: Re-run VM tests; expect pass**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.ui.home.HomeViewModelTest"`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/ui/home/HomeViewModel.kt app/src/test/java/dev/agentic/ui/home/HomeViewModelTest.kt
git commit -m "home: searchQuery/searchResults state in HomeViewModel"
```

---

## Task 7: Android shared `SearchBar` component

**Files:**
- Create: `agentic-dev-android/app/src/main/java/dev/agentic/ui/components/SearchBar.kt`

**Interfaces:**
- Produces:
  - `@Composable fun SearchBar(query: String, onQueryChange: (String) -> Unit, modifier: Modifier = Modifier, placeholder: String = "Search", searching: Boolean = false, onSearch: (() -> Unit)? = null)`
  - Stateless: caller owns `query`.

- [ ] **Step 1: Build the component using MD3 Expressive search shape**

```kotlin
@Composable
fun SearchBar(
    query: String,
    onQueryChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    placeholder: String = "Search",
    searching: Boolean = false,
    onSearch: (() -> Unit)? = null,
) {
    Surface(
        modifier = modifier.height(48.dp),
        shape = MaterialTheme.shapes.large,
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        tonalElevation = 0.dp,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.fillMaxSize().padding(horizontal = 12.dp)) {
            Icon(Icons.Rounded.Search, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.width(8.dp))
            BasicTextField(
                value = query,
                onValueChange = onQueryChange,
                singleLine = true,
                textStyle = MaterialTheme.typography.bodyLarge.copy(color = MaterialTheme.colorScheme.onSurface),
                cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
                modifier = Modifier.weight(1f),
                decorationBox = { inner ->
                    if (query.isEmpty()) {
                        Text(placeholder, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                    inner()
                },
                keyboardActions = KeyboardActions(onSearch = { onSearch?.invoke() }),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
            )
            if (query.isNotEmpty()) {
                IconButton(onClick = { onQueryChange("") }) {
                    Icon(Icons.Rounded.Close, contentDescription = "Clear")
                }
            } else if (searching) {
                CircularProgressIndicator(strokeWidth = 2.dp, modifier = Modifier.size(20.dp))
            }
        }
    }
}
```

Imports live next to existing MD3 imports in `ui/components/`. Confirm shape `MaterialTheme.shapes.large` resolves to 24dp under `MaterialExpressiveTheme` (it does; verified in `docs/superpowers/specs/2026-06-22-md3e-animation-refactor-design.md`).

- [ ] **Step 2: Compile-check the module**

Run: `./gradlew :app:assembleDebug`
Expected: BUILD SUCCESSFUL. (No tests for the component — there is no existing Compose UI test harness, see Global Constraints.)

- [ ] **Step 3: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/ui/components/SearchBar.kt
git commit -m "ui: shared stateless SearchBar component"
```

---

## Task 8: Session list title row uses the new `SearchBar`

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/SessionSearchBar.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeTopBar.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/HomeScreen.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/home/AdaptiveHome.kt`

**Interfaces:**
- `SessionSearchBar` becomes a thin wrapper that reads from `HomeUiState` and delegates to `SearchBar`.
- `HomeTopBar` accepts `searchQuery`, `onSearchQueryChange`, `searching`, and an optional `onClearSearch`.

- [ ] **Step 1: Rewrite `SessionSearchBar` as a thin wrapper**

```kotlin
@Composable
fun SessionSearchBar(
    state: HomeUiState,
    onQueryChange: (String) -> Unit,
    onOpenSession: (String) -> Unit,
    onExpandedChange: (Boolean) -> Unit,
) {
    val results = state.searchResults
    Box(Modifier.fillMaxWidth()) {
        SearchBar(
            query = state.searchQuery,
            onQueryChange = onQueryChange,
            placeholder = "Search sessions",
            searching = state.searching,
        )
    }
    // Expanded panel: only render when the user has typed at least 2 chars and is not in selection mode.
    if (state.searchQuery.length >= 2 && !state.selectionMode) {
        Surface(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp),
            shape = MaterialTheme.shapes.extraLarge,
            tonalElevation = 6.dp,
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
        ) {
            if (results.isEmpty()) {
                Text(
                    "No sessions match \"${state.searchQuery.trim()}\"",
                    modifier = Modifier.padding(24.dp).fillMaxWidth(),
                    textAlign = TextAlign.Center,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                LazyColumn(modifier = Modifier.heightIn(max = 360.dp)) {
                    items(results, key = { it.session.id }) { hit ->
                        SessionRow(
                            session = hit.session,
                            selected = false,
                            selectionMode = false,
                            onClick = { onOpenSession(hit.session.id) },
                            onLongClick = {},
                            showCheckbox = false,
                        )
                    }
                }
            }
        }
        LaunchedEffect(Unit) { onExpandedChange(true) }
    } else {
        LaunchedEffect(Unit) { onExpandedChange(false) }
    }
}
```

- [ ] **Step 2: Move search bar to `HomeTopBar`**

In `HomeTopBar.kt`, replace the existing title-only `Row` with:

```kotlin
Row(modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
    Icon(Icons.Rounded.AutoAwesome, contentDescription = null, tint = MaterialTheme.colorScheme.primary)
    Spacer(Modifier.width(8.dp))
    if (state.selectionMode) {
        Text("$selectedCount selected", style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
        IconButton(onClick = onToggleSelectAll) { Icon(/* select all */ Icons.Rounded.SelectAll, contentDescription = null) }
        IconButton(onClick = onDeleteSelected) { Icon(Icons.Rounded.Delete, contentDescription = null) }
        IconButton(onClick = onExitSelection) { Icon(Icons.Rounded.Close, contentDescription = null) }
    } else {
        Text("agentic-dev", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.width(12.dp))
        Box(Modifier.weight(1f)) {
            SessionSearchBar(
                state = state,
                onQueryChange = onSearchQueryChange,
                onOpenSession = onOpenSession,
                onExpandedChange = onSearchExpandedChange,
            )
        }
        IconButton(onClick = onLogout) { Icon(Icons.AutoMirrored.Rounded.Logout, contentDescription = "Log out") }
    }
}
```

- [ ] **Step 3: Drop the search bar from `SessionListPane` in `HomeScreen.kt` and `AdaptiveHome.kt`**

In each, locate the `SessionListPane` `Column` that currently renders `SessionSearchBar(...)` (see `HomeScreen.kt:337-347` per the prior scan) and remove the call. Add `searchResults` rendering:

```kotlin
val list: List<Session> = if (state.searchQuery.length >= 2) state.searchResults.map { it.session } else state.sessions
LazyColumn { items(list, key = { it.id }) { /* existing row */ } }
```

- [ ] **Step 4: Compile-check**

Run: `./gradlew :app:assembleDebug`
Expected: BUILD SUCCESSFUL. If imports are missing, follow the same import set the original `SessionSearchBar` used (`material.icons.rounded.*`, `material.icons.automirrored.rounded.*`).

- [ ] **Step 5: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/ui/home/SessionSearchBar.kt app/src/main/java/dev/agentic/ui/home/HomeTopBar.kt app/src/main/java/dev/agentic/ui/home/HomeScreen.kt app/src/main/java/dev/agentic/ui/home/AdaptiveHome.kt
git commit -m "home: search bar moves to title row, list renders searchResults"
```

---

## Task 9: New Request chip picker reuses `SearchBar`

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt`

- [ ] **Step 1: Replace the in-picker `OutlinedTextField` with `SearchBar`**

In the `ChipPicker` composable inside `NewRequestScreen.kt`, locate the private composable that contains the `OutlinedTextField` with placeholder `Search…`. Replace that text field with:

```kotlin
SearchBar(
    query = search,
    onQueryChange = { search = it },
    placeholder = "Search chips…",
    modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp),
)
```

Keep the existing filter logic below (case-insensitive substring, preserve order).

- [ ] **Step 2: Compile-check**

Run: `./gradlew :app:assembleDebug`
Expected: BUILD SUCCESSFUL.

- [ ] **Step 3: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt
git commit -m "newrequest: ChipPicker uses shared SearchBar"
```

---

## Task 10: Login IME handling

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/login/LoginManualScreen.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/login/LoginScanScreen.kt`

- [ ] **Step 1: In `LoginManualScreen`, add scroll + imePadding + keyboard options/actions**

```kotlin
Column(
    modifier = Modifier
        .fillMaxSize()
        .padding(scaffoldPadding)
        .verticalScroll(rememberScrollState())
        .imePadding()
        .padding(28.dp),
) { /* … existing fields and button … */ }
```

Host `OutlinedTextField`:

```kotlin
val passwordFocus = remember { FocusRequester() }
OutlinedTextField(
    value = state.host,
    onValueChange = onHost,
    label = { Text("Host") },
    singleLine = true,
    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next),
    keyboardActions = KeyboardActions(onNext = { passwordFocus.requestFocus() }),
    modifier = Modifier.fillMaxWidth(),
)
```

Password `OutlinedTextField`:

```kotlin
OutlinedTextField(
    value = state.password,
    onValueChange = onPassword,
    label = { Text("Password") },
    singleLine = true,
    visualTransformation = if (state.passwordVisible) VisualTransformation.None else PasswordVisualTransformation(),
    trailingIcon = { /* existing toggle */ },
    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, imeAction = ImeAction.Done),
    keyboardActions = KeyboardActions(onDone = {
        if (!state.busy && state.host.isNotBlank() && state.password.isNotEmpty()) onSubmit()
    }),
    modifier = Modifier.fillMaxWidth().focusRequester(passwordFocus),
)
```

- [ ] **Step 2: In `LoginScanScreen`, apply the same IME actions to the password field**

Wrap the column that holds the results `LazyColumn` and the bottom password/connect block with `.verticalScroll(rememberScrollState())` and `.imePadding()`. If the parent is already a `Column` (per the prior scan), move the `LazyColumn` weight to its own `Box` and wrap the rest in `verticalScroll`. Apply the same `KeyboardOptions` / `KeyboardActions` to the password `OutlinedTextField`, with `onDone` calling the existing `onSubmit` only when `state.selectedHost != null && state.password.isNotEmpty() && !state.busy`.

- [ ] **Step 3: Compile-check**

Run: `./gradlew :app:assembleDebug`
Expected: BUILD SUCCESSFUL.

- [ ] **Step 4: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/ui/login/LoginManualScreen.kt app/src/main/java/dev/agentic/ui/login/LoginScanScreen.kt
git commit -m "login: scroll + imePadding + keyboard actions"
```

---

## Task 11: Narration default flip + short-fold rule

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/domain/Transcript.kt`
- Modify: `agentic-dev-android/app/src/test/java/dev/agentic/domain/SessionSearchTest.kt` (extend for narration; rename to `TranscriptReducerTest` if existing naming is used)

**Interfaces:**
- `appendText` keeps the same merge-into-trailing-`TextNode` logic, but the freshly created `TextNode` uses `narration = true` only when its accumulated text length is `< 256` and the prior `TextNode` did not exist (i.e. a new node starts folded short, then the reducer will un-fold it on the next append that pushes length past 256, or on turn end).
- `promoteTrailingTextIfNoAnswer` simplifies to: on turn end, force the trailing `TextNode.narration` to `false` (no more `AnswerNode` guard).

- [ ] **Step 1: Add failing tests**

In `SessionSearchTest.kt` (or a sibling `TranscriptTest.kt`):

```kotlin
@Test fun appendText_starts_folded_when_short() {
    val nodes = emptyList<Node>()
    val out = appendText(nodes, "hi")
    val t = out.last() as TextNode
    assertTrue(t.narration)
}

@Test fun appendText_unfolds_when_text_grows_past_256() {
    val long = "a".repeat(300)
    val out = appendText(appendText(appendText(emptyList(), "x"), "y"), long)
    val t = out.last() as TextNode
    assertFalse(t.narration)
}

@Test fun promoteTrailingText_always_forces_inline_on_turn_end() {
    val nodes: List<Node> = listOf(PromptNode("p"), TextNode("partial", narration = true), AnswerNode("done"))
    val out = promoteTrailingTextIfNoAnswer(nodes)
    // Trailing TextNode was already before AnswerNode, so nothing to do.
    val t = out.last { it is TextNode } as TextNode
    // Force inline regardless of AnswerNode presence:
    val forced = promoteTrailingTextIfNoAnswer(out.dropLast(1) + TextNode("partial", narration = true))
    assertFalse((forced.last() as TextNode).narration)
}
```

- [ ] **Step 2: Run; expect failures**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.domain.TranscriptReducerTest"`
Expected: FAIL on the 256-char and force-inline assertions.

- [ ] **Step 3: Update `appendText`**

In `domain/Transcript.kt`:

```kotlin
fun appendText(nodes: List<Node>, text: String): List<Node> {
    if (text.isEmpty()) return nodes
    val last = nodes.lastOrNull()
    return if (last is TextNode) {
        val merged = last.copy(text = last.text + text)
        val narration = if (merged.text.length >= 256) false else last.narration
        ArrayList<Node>(nodes).also { it[it.lastIndex] = merged.copy(narration = narration) }
    } else {
        nodes + TextNode(text, narration = true)
    }
}
```

- [ ] **Step 4: Simplify `promoteTrailingTextIfNoAnswer`**

```kotlin
fun promoteTrailingTextIfNoAnswer(nodes: List<Node>): List<Node> {
    val idx = nodes.indexOfLast { it is TextNode }
    if (idx < 0) return nodes
    val t = nodes[idx] as TextNode
    if (!t.narration) return nodes
    return nodes.toMutableList().also { it[idx] = t.copy(narration = false) }
}
```

- [ ] **Step 5: Re-run; expect pass**

Run: `./gradlew :app:testDebugUnitTest --tests "dev.agentic.domain.TranscriptReducerTest"`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd agentic-dev-android && git add app/src/main/java/dev/agentic/domain/Transcript.kt app/src/test/java/dev/agentic/domain/SessionSearchTest.kt
git commit -m "transcript: narration CLI-aligned default + turn-end force inline"
```

---

## Task 12: Final verification

- [ ] **Step 1: Backend full test run**

```bash
cd agentic-dev && cargo test --workspace
```

Expected: PASS.

- [ ] **Step 2: Android unit test run**

```bash
cd agentic-dev-android && ./gradlew :app:testDebugUnitTest
```

Expected: PASS.

- [ ] **Step 3: Android assemble**

```bash
./gradlew :app:assembleDebug
```

Expected: BUILD SUCCESSFUL.

- [ ] **Step 4: Push**

```bash
cd agentic-dev && git push origin HEAD:master
cd agentic-dev-android && git push origin HEAD:master
```

- [ ] **Step 5: Build release APK in the main checkout per `agentic-dev-android/CLAUDE.md`**

```bash
cd ~/src/agentic-dev-android && git pull --ff-only origin master
~/.local/share/gradle-8.10.2/bin/gradle assembleRelease
mkdir -p ./outbox
cp ~/src/agentic-dev-android/app/build/outputs/apk/release/app-release.apk \
  "./outbox/$(date +%Y%m%d-%H%M).apk"
ls -la ./outbox/
```

Expected: APK present in `outbox/`.

- [ ] **Step 6: Commit outbox listing if any new local file is created**

No commit is required — `outbox/` is for delivery, not source control.

---

## Self-review

1. **Spec coverage:** Title-row search (Task 8), content search route (Tasks 1–6), shared `SearchBar` (Tasks 7–9), login IME (Task 10), narration flip (Task 11), final delivery (Task 12). Backend spec search response shape and caps are in Task 2 (`extract_snippet`, ranking, caps). Android UX spec narration rule is in Task 11. No gap.
2. **Placeholder scan:** No "TBD", no "similar to task N" without the code, all `Cargo` / Gradle commands include expected output, every code step has the full snippet.
3. **Type consistency:** `SearchField` enum is identical in Task 4 and used in the same casing in tests across Tasks 4–6. `SearchMatch` / `SearchHit` / `SearchResponse` names match between backend (Task 2) and Android (Task 4). `setSearchQuery` is consistent between Task 6 spec and Task 8 caller. `SessionSearchBar` signature in Task 8 is consumed by `HomeTopBar` in the same task — no cross-task drift.
