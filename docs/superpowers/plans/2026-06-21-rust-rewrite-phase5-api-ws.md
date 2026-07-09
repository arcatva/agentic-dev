# Rust Rewrite — Phase 5 (API + WS: REST routes, windowed transcript, WS stream, client-disconnect downgrade) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `server-rs/`, wire the Phase 0–4 engine + store + transcript projection to a full axum HTTP/WS surface at behavioral parity with `server/api/routes.ts`, `server/api/stream.ts`, and `server/api/server.ts` — the core session REST routes, the windowed transcript read (`{ log, start, total }`) served from the in-memory `RenderedProjection` (no whole-file re-read on the hot path), the `/api/sessions/:id/stream` WebSocket (backfill from `?since` then live subscribe, closing on `engineExit`), and the client-disconnect log downgrade — so the Android client cannot tell which backend it is talking to.

**Architecture:** A `ClaudeEvent::to_wire() -> serde_json::Value` produces the exact kind-tagged camelCase JSON the client expects (the WS wire contract). HTTP handlers in `src/api/sessions.rs` call the already-built `Engine` read/write methods (`list`/`get`/`get_log`/`submit_session`/`follow_up`/`kill`/`interrupt`/`discard`/`delete_session`) and the `TranscriptCache` for the windowed read. The WS handler in `src/api/stream.rs` registers an `Engine::subscribe` callback that forwards `to_wire()` JSON into a `tokio::sync::mpsc` channel drained by the socket's send task, exactly mirroring `stream.ts`. All new routes mount behind the existing `auth_gate` in `src/api/mod.rs`.

**Tech Stack:** Rust, tokio, axum 0.8 (with the `ws` feature), serde_json, the existing `Engine`/`Store`/`TranscriptCache`. Tests use `tower::ServiceExt::oneshot` for HTTP and the real fake-claude fixtures for end-to-end session/WS flows.

## Global Constraints

These are copied verbatim from the strategy spec (`docs/superpowers/specs/2026-06-20-rust-backend-rewrite-design.md`) and the TS reference (`server/api/*`, `server/engine/streamParser.ts`). Every task's requirements implicitly include this section.

- **Parity bar:** "No new product features. Behavioral parity with the current TS backend is the bar." The Android client is the parity oracle — it must not be able to tell which backend it is talking to.
- **Auth:** HMAC bearer token, **same secret + same scheme** (`AGENTIC_AUTH_SECRET`). WS uses `?token=` query param (browsers can't set WS auth headers). Already implemented in `auth_gate` (`src/api/mod.rs`) — new routes inherit it; the WS handler re-verifies `?token=` itself and closes `1008 "unauthorized"` on failure (parity with `stream.ts`).
- **Rendered-coordinate cursor (single source of truth — `renderedLog.ts`):** `GET /api/sessions/:id` and the WS backfill send only the rendered line types and the client counts THIS filtered view to derive its stream cursor (`since`). The rendered prefixes are **verbatim**: `{"type":"agentic_prompt"`, `{"type":"stream_event"`, `{"type":"assistant"`, `{"type":"result"`. (Already in `src/transcript.rs::RENDERED_PREFIXES` — **reuse `is_rendered`/`filter_rendered`; do NOT duplicate them.**)
- **`filterRendered` fallback (verbatim — `renderedLog.ts`):** returns the filtered lines, or the raw lines if filtering yields nothing (defensive against format drift). Already in `transcript::filter_rendered`.
- **`rawToRenderedOffset` (verbatim — `renderedLog.ts`):** `raw.slice(0, rawOffset).filter(isRendered).length` — the number of rendered lines that precede `rawOffset`. `POST /api/sessions/:id/messages` returns this filtered offset (NOT the raw offset `followUp` returns) or the resumed turn's prompt/early frames are skipped.
- **Windowed transcript response (verbatim — spec line 38):** the windowed `{ log, start, total }` response. Default (no `?limit`/`?before`) keeps the legacy `{ session, log }` shape. Served from the projection: **no whole-file re-read, no `rawToRenderedOffset`, ever on the hot path** (spec line 98).
- **WS event wire shape (verbatim — `streamParser.ts`):** each event is a JSON object with a `kind` field and **camelCase** keys. Exact kinds + fields:
  - `{ kind:"init", sessionId, raw }`
  - `{ kind:"prompt", text, at, raw }`
  - `{ kind:"retry", attempt, maxRetries, category, raw }`
  - `{ kind:"text", text, parentToolUseId, raw }`
  - `{ kind:"thinking", text, parentToolUseId, raw }`
  - `{ kind:"result", isError, costUsd, text, raw }`
  - `{ kind:"agentResult", toolUseId, text, raw }`
  - `{ kind:"skill", names, parentToolUseId, raw }`
  - `{ kind:"agent", agents, parentToolUseId, raw }` (each agent: `{ id, agentType, description }`)
  - `{ kind:"workflow", id, name, parentToolUseId, raw }`
  - `{ kind:"ask", questions, parentToolUseId, raw }`
  - `{ kind:"tool", name, input, parentToolUseId, raw }`
  - `{ kind:"other", raw }`
  - `parentToolUseId` is **`null`** (JSON null, not omitted) when absent — `streamParser.ts` always sets the key. `costUsd` is `null` when absent.
- **WS backfill (verbatim — `stream.ts`):** backfill `filterRendered(getLog(id)).slice(since)`; for each line, `parseLine(line)` and send each event; if `parseLine` returns empty, send `{ kind:"backfill", raw: <JSON.parse(line) or line string> }`. After backfill, re-read status: if `done`/`failed`/`killed`, send `{ kind:"other", raw:{ engineExit:{ code:null, status } } }` then close `1000 "ended"`. Else live-subscribe; on each event send `to_wire()`; if the event is `kind:"other"` with `raw.engineExit`, close `1000 "ended"`.
- **WS `since` parse (verbatim — `stream.ts`):** `since = sinceRaw != null ? max(0, parseInt(sinceRaw,10) || 0) : 0`.
- **`isClientDisconnect` (verbatim — `server.ts`):** returns true iff `code === "ERR_STREAM_PREMATURE_CLOSE" || code === "ECONNRESET" || code === "EPIPE" || msg === "Premature close"`, where `code = a.code ?? a.err.code` and `msg = a.message ?? a.msg`. Used to downgrade benign client-disconnect stream errors to debug so a real 500 still stands out.
- **HTTP status codes (verbatim — `routes.ts`):** missing session → `404 {"error":"not found"}`; missing prompt / bad input / engine error → `400 {"error": <msg>}`; double-discard → `400`; unauthenticated `/api/*` → `401 {"error":"unauthorized"}`. `GET /api/sessions` → `{ "sessions": [...] }`. `GET /api/sessions/:id` → `{ "session": {...}, "log": [...] }`. `POST /api/sessions` / `POST .../messages` success → `{ "id": ... }` / `{ "ok": true, "since": ... }`. `DELETE`/`interrupt`/`discard`/`delete` success → `{ "ok": true }`.
- **Empty-JSON-body parity (verbatim — `server.ts`):** discard/delete/interrupt POSTs send `content-type: application/json` with an empty body; treat an empty/absent body as `{}` (do NOT 400). In axum: accept an optional/absent body (`Option<Json<...>>` or `Bytes`-tolerant), never require a populated body for these no-arg POSTs.
- **`submitSession(repos, skills, prompt, env, meta)` body mapping (verbatim — `routes.ts`):** `repos = b.repos ?? (b.repo ? [b.repo] : [])`; `skills = b.skills ?? []`; `prompt` required (repos optional — pure-skill tasks allowed); `meta = { model: b.model, effort: b.effort, mode: b.mode }`. On engine error → `400 {"error": msg}`.
- **`followUp` default `setTitle` (verbatim — `routes.ts`):** `engine.followUp(id, prompt, b.setTitle ?? true)`.
- **Workflows route (verbatim — `routes.ts`):** `GET /api/sessions/:id/workflows`: 404 if no session; if `s.worktreePath` is empty/None → `{ "workflows": [] }` (NOT a 500); else `{ "workflows": listWorkflows(<worktree>/.claude-config) }`. **Phase 5 scope:** return `{ "workflows": [] }` when there is no active workflow and otherwise the minimal run list the engine can already read; the full per-agent transcript payload + `/workflows/:runId/agents/:agentId` is Phase 6.
- **Out of scope for Phase 5 (Phase 6):** `/api/usage` (usage cache + coalescing), `/api/repos`, `/api/skills`, `/api/groups`, `/api/templates`, `/api/devices`, `/api/sessions/:id/file`, `.../upload`, `.../outbox`, `.../commits[...]`, the full workflow-agent transcript, and the FCM push send. These stay on the existing `// Phase 6` stubs; Phase 5 does NOT add them. (Strategy spec phase table: Phase 6 = "push (FCM), usage cache, diff/upload/file, misc endpoints".)
- **No real `claude`:** integration tests spawn only the fixtures under `server/test/fixtures/` (`fake-claude.sh`, etc.), resolved via `env!("CARGO_MANIFEST_DIR")` + `../server/test/fixtures/<name>`.
- **Commit discipline:** `cargo test` green before every commit. Commit-only, **never push**.

---

## File Structure

- `server-rs/Cargo.toml` — MODIFY: enable axum's `ws` feature; add dev-deps for a WS test client (`tokio-tungstenite`, `futures-util`).
- `server-rs/src/stream.rs` — MODIFY: add `impl ClaudeEvent { pub fn to_wire(&self) -> serde_json::Value }` producing the kind-tagged camelCase wire JSON. This is the single source for both the WS live forward and the backfill `parseLine` output. (Tested in `stream.rs`'s own `#[cfg(test)]`.)
- `server-rs/src/transcript.rs` — MODIFY: add `pub fn raw_to_rendered_offset(raw: &[String], raw_offset: usize) -> usize` next to the existing `is_rendered`/`filter_rendered` (reuse `is_rendered`; do NOT duplicate it).
- `server-rs/src/api/sessions.rs` — CREATE: all session REST handlers (`list`, `get` + windowed, `create`, `messages`, `delete`/kill, `interrupt`, `discard`, `delete` (remove), `workflows`). Pure axum handlers over `AppState`.
- `server-rs/src/api/stream.rs` — CREATE: the `/api/sessions/:id/stream` WS handler + `is_client_disconnect` helper (mirror of `server.ts`).
- `server-rs/src/api/mod.rs` — MODIFY: mount the new routes (replace the Phase-0 `/api/ping` probe), add the WS route, keep the `auth_gate` layer.
- `server-rs/src/main.rs` — no change required (it already builds `AppState` with engine/store/transcript and serves `api::app(state)` with `ConnectInfo`).

---

## Task 1: `ClaudeEvent::to_wire()` — the WS wire JSON contract

**Files:**
- Modify: `server-rs/src/stream.rs` (add `to_wire` in the existing `impl`/new `impl ClaudeEvent` block; tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: the existing `ClaudeEvent` enum + `SpawnedAgent` (already in `stream.rs`).
- Produces: `impl ClaudeEvent { pub fn to_wire(&self) -> serde_json::Value }` — used by `api/stream.rs` (Task 6) to serialize both backfilled (`parse_line`) and live (subscribed) events. Wire keys are camelCase with a `kind` tag; `parentToolUseId`/`costUsd` are JSON `null` when absent.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `server-rs/src/stream.rs`:

```rust
    #[test]
    fn to_wire_emits_kind_tagged_camelcase() {
        use serde_json::json;
        // text with no parent → parentToolUseId must be JSON null (present, not omitted).
        let ev = ClaudeEvent::Text { text: "hi".into(), parent_tool_use_id: None, raw: json!({"type":"x"}) };
        assert_eq!(ev.to_wire(), json!({"kind":"text","text":"hi","parentToolUseId":null,"raw":{"type":"x"}}));

        // text with a parent → camelCase parentToolUseId carries the id.
        let ev = ClaudeEvent::Text { text: "yo".into(), parent_tool_use_id: Some("t1".into()), raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"text","text":"yo","parentToolUseId":"t1","raw":{}}));

        // init → sessionId.
        let ev = ClaudeEvent::Init { session_id: "sess-1".into(), raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"init","sessionId":"sess-1","raw":{}}));

        // prompt → text + at.
        let ev = ClaudeEvent::Prompt { text: "go".into(), at: 42, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"prompt","text":"go","at":42,"raw":{}}));

        // result with a cost → isError + costUsd. Absent cost → costUsd null.
        let ev = ClaudeEvent::Result { is_error: false, cost_usd: Some(0.0042), text: Some("ok".into()), raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"result","isError":false,"costUsd":0.0042,"text":"ok","raw":{}}));
        let ev = ClaudeEvent::Result { is_error: true, cost_usd: None, text: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"result","isError":true,"costUsd":null,"text":null,"raw":{}}));

        // agentResult → toolUseId.
        let ev = ClaudeEvent::AgentResult { tool_use_id: "tu".into(), text: "done".into(), raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"agentResult","toolUseId":"tu","text":"done","raw":{}}));

        // retry → attempt + maxRetries + category.
        let ev = ClaudeEvent::Retry { attempt: 1, max_retries: 3, category: "overloaded".into(), raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"retry","attempt":1,"maxRetries":3,"category":"overloaded","raw":{}}));

        // agent → agents array of {id, agentType, description}.
        let ev = ClaudeEvent::Agent {
            agents: vec![SpawnedAgent { id: "a1".into(), agent_type: "coder".into(), description: "d".into() }],
            parent_tool_use_id: Some("p".into()), raw: json!({}),
        };
        assert_eq!(ev.to_wire(), json!({
            "kind":"agent",
            "agents":[{"id":"a1","agentType":"coder","description":"d"}],
            "parentToolUseId":"p","raw":{}
        }));

        // workflow / skill / ask / tool / thinking / other shapes.
        let ev = ClaudeEvent::Workflow { id: "w".into(), name: "n".into(), parent_tool_use_id: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"workflow","id":"w","name":"n","parentToolUseId":null,"raw":{}}));
        let ev = ClaudeEvent::Skill { names: vec!["s1".into()], parent_tool_use_id: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"skill","names":["s1"],"parentToolUseId":null,"raw":{}}));
        let ev = ClaudeEvent::Ask { questions: vec![json!({"q":1})], parent_tool_use_id: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"ask","questions":[{"q":1}],"parentToolUseId":null,"raw":{}}));
        let ev = ClaudeEvent::Tool { name: "bash".into(), input: json!({"cmd":"ls"}), parent_tool_use_id: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"tool","name":"bash","input":{"cmd":"ls"},"parentToolUseId":null,"raw":{}}));
        let ev = ClaudeEvent::Thinking { text: "hmm".into(), parent_tool_use_id: None, raw: json!({}) };
        assert_eq!(ev.to_wire(), json!({"kind":"thinking","text":"hmm","parentToolUseId":null,"raw":{}}));
        let ev = ClaudeEvent::Other { raw: json!({"engineExit":{"code":null,"status":"done"}}) };
        assert_eq!(ev.to_wire(), json!({"kind":"other","raw":{"engineExit":{"code":null,"status":"done"}}}));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml to_wire_emits_kind_tagged_camelcase`
Expected: FAIL — `no method named to_wire found for enum ClaudeEvent`.

- [ ] **Step 3: Write minimal implementation**

Add to `server-rs/src/stream.rs` (a new `impl ClaudeEvent` block, e.g. right after the enum definition):

```rust
impl ClaudeEvent {
    /// Serialize to the kind-tagged, camelCase wire JSON the Android client consumes
    /// (mirror of `streamParser.ts`'s returned objects). `parentToolUseId`/`costUsd` are
    /// emitted as JSON `null` when absent — the TS parser always sets the key.
    pub fn to_wire(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            ClaudeEvent::Init { session_id, raw } =>
                json!({ "kind": "init", "sessionId": session_id, "raw": raw }),
            ClaudeEvent::Prompt { text, at, raw } =>
                json!({ "kind": "prompt", "text": text, "at": at, "raw": raw }),
            ClaudeEvent::Text { text, parent_tool_use_id, raw } =>
                json!({ "kind": "text", "text": text, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::Thinking { text, parent_tool_use_id, raw } =>
                json!({ "kind": "thinking", "text": text, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::Skill { names, parent_tool_use_id, raw } =>
                json!({ "kind": "skill", "names": names, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::Ask { questions, parent_tool_use_id, raw } =>
                json!({ "kind": "ask", "questions": questions, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::Agent { agents, parent_tool_use_id, raw } => {
                let agents: Vec<serde_json::Value> = agents.iter().map(|a| json!({
                    "id": a.id, "agentType": a.agent_type, "description": a.description,
                })).collect();
                json!({ "kind": "agent", "agents": agents, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Workflow { id, name, parent_tool_use_id, raw } =>
                json!({ "kind": "workflow", "id": id, "name": name, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::Tool { name, input, parent_tool_use_id, raw } =>
                json!({ "kind": "tool", "name": name, "input": input, "parentToolUseId": parent_tool_use_id, "raw": raw }),
            ClaudeEvent::AgentResult { tool_use_id, text, raw } =>
                json!({ "kind": "agentResult", "toolUseId": tool_use_id, "text": text, "raw": raw }),
            ClaudeEvent::Retry { attempt, max_retries, category, raw } =>
                json!({ "kind": "retry", "attempt": attempt, "maxRetries": max_retries, "category": category, "raw": raw }),
            ClaudeEvent::Result { is_error, cost_usd, text, raw } =>
                json!({ "kind": "result", "isError": is_error, "costUsd": cost_usd, "text": text, "raw": raw }),
            ClaudeEvent::Other { raw } =>
                json!({ "kind": "other", "raw": raw }),
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml to_wire_emits_kind_tagged_camelcase`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/stream.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T1: ClaudeEvent::to_wire — kind-tagged camelCase WS wire JSON"
```

---

## Task 2: `raw_to_rendered_offset` — filtered cursor for resumed turns

**Files:**
- Modify: `server-rs/src/transcript.rs` (add the function next to `is_rendered`/`filter_rendered`; tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: the existing `is_rendered(&str) -> bool` (reuse — do NOT re-implement the prefix check).
- Produces: `pub fn raw_to_rendered_offset(raw: &[String], raw_offset: usize) -> usize` — used by `POST /api/sessions/:id/messages` (Task 4) to convert `follow_up`'s raw log offset to the filtered (rendered) coordinate the client counts.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `server-rs/src/transcript.rs`:

```rust
    #[test]
    fn raw_to_rendered_offset_counts_rendered_lines_before_offset() {
        // raw[0]=rendered, raw[1]=noise, raw[2]=rendered, raw[3]=noise, raw[4]=rendered
        let raw: Vec<String> = vec![
            RENDERED[0].to_string(), NOISE[0].to_string(),
            RENDERED[1].to_string(), NOISE[1].to_string(),
            RENDERED[0].to_string(),
        ];
        // before raw index 0 → 0 rendered lines precede it
        assert_eq!(raw_to_rendered_offset(&raw, 0), 0);
        // before raw index 2 → raw[0] is the only rendered line that precedes → 1
        assert_eq!(raw_to_rendered_offset(&raw, 2), 1);
        // before raw index 3 → raw[0], raw[2] precede → 2
        assert_eq!(raw_to_rendered_offset(&raw, 3), 2);
        // offset past the end → all 3 rendered lines counted
        assert_eq!(raw_to_rendered_offset(&raw, 99), 3);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml raw_to_rendered_offset_counts_rendered_lines_before_offset`
Expected: FAIL — `cannot find function raw_to_rendered_offset`.

- [ ] **Step 3: Write minimal implementation**

Add to `server-rs/src/transcript.rs`, right after `filter_rendered`:

```rust
/// Convert a RAW log offset (as the engine counts log lines) into the FILTERED offset the
/// client uses as its stream cursor: the number of rendered lines that precede `raw_offset`.
/// Mirror of `renderedLog.ts::rawToRenderedOffset`. `POST /api/sessions/:id/messages` returns
/// this so a resumed turn's backfill lines up with the filtered log the client counted.
pub fn raw_to_rendered_offset(raw: &[String], raw_offset: usize) -> usize {
    let end = raw_offset.min(raw.len());
    raw[..end].iter().filter(|l| is_rendered(l)).count()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml raw_to_rendered_offset_counts_rendered_lines_before_offset`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/transcript.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T2: transcript::raw_to_rendered_offset (filtered cursor for resumed turns)"
```

---

## Task 3: Session REST read routes — list + get (+ windowed transcript)

**Files:**
- Create: `server-rs/src/api/sessions.rs`
- Modify: `server-rs/src/api/mod.rs` (declare `mod sessions;`, mount `GET /api/sessions` and `GET /api/sessions/:id`, remove the `/api/ping` probe line + its handler)
- Test: in `server-rs/src/api/sessions.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `AppState { config, store, transcript, engine, throttle }` (`src/state.rs`); `Engine::list() -> Vec<Session>`, `Engine::get(&str) -> Option<Session>`, `Engine::get_log(&str) -> Vec<String>`; `transcript::filter_rendered`; `TranscriptCache::with(id, path, f)`; `RenderedProjection::{window_tail, range, count}`; `Store::log_path(&str) -> PathBuf`.
- Produces:
  - `pub async fn list_sessions(State<AppState>) -> Json<Value>` → `{ "sessions": [...] }`.
  - `pub async fn get_session(State<AppState>, Path<String>, Query<TranscriptQuery>) -> Response` → 404 `{"error":"not found"}` or `{ "session", "log" }` (default) / `{ "session", "log", "start", "total" }` (windowed). Both consumed by Task 7's mount.
  - `pub struct TranscriptQuery { limit: Option<usize>, before: Option<usize> }`.

- [ ] **Step 1: Write the failing test**

Create `server-rs/src/api/sessions.rs` with the handlers stubbed `todo!()` so it compiles, plus this test module (the test harness mirrors `api/mod.rs`'s existing `test_state`). Write the test FIRST against the intended behavior:

```rust
use axum::{extract::{Path, Query, State}, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::state::AppState;
use crate::transcript::filter_rendered;

#[derive(Deserialize, Default)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
    pub before: Option<usize>,
}

pub async fn list_sessions(State(_st): State<AppState>) -> Json<Value> { todo!() }
pub async fn get_session(State(_st): State<AppState>, Path(_id): Path<String>, Query(_q): Query<TranscriptQuery>) -> Response { todo!() }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{test_state, auth, body_json, oneshot_req};
    use axum::body::Body;
    use axum::http::Request;

    #[tokio::test]
    async fn get_unknown_session_is_404() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::get("/api/sessions/nope")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"error":"not found"}));
    }

    #[tokio::test]
    async fn list_is_empty_for_fresh_store() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::get("/api/sessions")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"sessions": []}));
    }

    #[tokio::test]
    async fn get_session_returns_session_and_filtered_log() {
        let st = test_state().await;
        // Seed a session row + a log with one rendered + one noise line.
        let id = "s-get";
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("agentic/x".into()),
            ..Default::default()
        }).await.unwrap();
        st.store.append_log(id, "{\"type\":\"assistant\",\"text\":\"hi\"}").await.unwrap();
        st.store.append_log(id, "{\"type\":\"tool_result\"}").await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["session"]["id"], json!(id));
        // log is filtered to rendered types: only the assistant line survives.
        assert_eq!(body["log"], json!(["{\"type\":\"assistant\",\"text\":\"hi\"}"]));
        // default (non-windowed) shape: no start/total keys.
        assert!(body.get("start").is_none() && body.get("total").is_none());
    }

    #[tokio::test]
    async fn get_session_windowed_returns_tail_with_start_and_total() {
        let st = test_state().await;
        let id = "s-win";
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("agentic/x".into()),
            ..Default::default()
        }).await.unwrap();
        for i in 0..5 { st.store.append_log(id, &format!("{{\"type\":\"assistant\",\"i\":{i}}}")).await.unwrap(); }
        let (status, body) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}?limit=2"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], json!(5));
        assert_eq!(body["start"], json!(3));          // tail of 2 → start at index 3
        assert_eq!(body["log"].as_array().unwrap().len(), 2);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: FAIL — `todo!()` panics / `test_support` not found. (Task 7 adds `test_support`; for THIS task, add a minimal `test_support` module — see Step 3 — so the test compiles.)

- [ ] **Step 3: Write minimal implementation**

First add a shared test harness. Create `server-rs/src/api/test_support.rs`:

```rust
//! Shared HTTP-test helpers (compiled only under cfg(test)). Mirrors api/mod.rs's existing
//! per-call-unique state so parallel #[tokio::test]s each get an isolated DB/WAL dir.
#![cfg(test)]
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use crate::state::AppState;
use crate::config::Config;
use crate::throttle::LoginThrottle;

static CTR: AtomicU64 = AtomicU64::new(0);

pub async fn test_state() -> AppState {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let mut c = Config::for_test("s3cret", "pw");
    let dir = std::env::temp_dir().join(format!("agentic-p5-{}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    c.db_path = dir.join("db.sqlite");
    c.log_dir = dir.join("logs");
    let _ = std::fs::create_dir_all(&c.log_dir);
    c.worktrees_root = dir.join("worktrees");
    let store = Arc::new(crate::store::Store::open(c.db_path.clone(), c.log_dir.clone()).await.unwrap());
    let transcript = Arc::new(crate::transcript::TranscriptCache::new(64 * 1024 * 1024));
    let engine_cfg = crate::engine::EngineConfig {
        src_root: c.src_root.clone(),
        worktrees_root: c.worktrees_root.clone(),
        log_dir: c.log_dir.clone(),
        db_path: c.db_path.clone(),
        claude_bin: c.claude_bin.clone(),
        max_concurrent: Some(2),
        git_org: c.git_org.clone(),
        claude_config_base: c.claude_config_base.clone(),
        clone_fn: Some(Arc::new(|_u: &str, _d: &str| {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "clone disabled in tests"))
        })),
        sync_fn: None, runner: None, log_fn: None, now_fn: None, push_fn: None,
        idle_max_ms: None, wall_max_ms: None, idle_ttl_ms: None,
        memory_max: None, memory_high: None, cpu_quota: None, tasks_max: None,
    };
    let engine = Arc::new(crate::engine::Engine::with_store(engine_cfg, store.clone(), Some(transcript.clone())));
    AppState {
        config: Arc::new(c),
        throttle: Arc::new(Mutex::new(LoginThrottle::default())),
        store, transcript, engine,
    }
}

/// A valid `Bearer <token>` header value for this state's secret.
pub fn auth(st: &AppState) -> String {
    let token = crate::auth::issue_token(&st.config.auth_secret, 3600, now_secs());
    format!("Bearer {token}")
}
fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Drive one request through the full router (auth gate included) and return (status, json body).
pub async fn oneshot_req(st: AppState, req: Request<Body>) -> (StatusCode, Value) {
    let resp = crate::api::app(st).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, body)
}

pub async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}
```

Then add `mod test_support;` (guarded by `#[cfg(test)]`) to `server-rs/src/api/mod.rs` next to the existing `mod login;`:

```rust
mod login;
mod sessions;
#[cfg(test)]
mod test_support;
```

Now implement the handlers in `server-rs/src/api/sessions.rs` (replace the `todo!()` bodies):

```rust
pub async fn list_sessions(State(st): State<AppState>) -> Json<Value> {
    let sessions = st.engine.list().await;
    Json(json!({ "sessions": sessions }))
}

pub async fn get_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> Response {
    let Some(session) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    // Windowed read (any of ?limit / ?before present): serve from the in-memory projection —
    // no whole-file re-read on the hot path. { session, log, start, total }.
    if q.limit.is_some() || q.before.is_some() {
        let path = st.store.log_path(&id);
        let limit = q.limit.unwrap_or(usize::MAX);
        let out = st.transcript.with(&id, &path, |p| {
            match q.before {
                Some(before) => {
                    let lines: Vec<String> = p.range(before, limit).to_vec();
                    let total = p.count();
                    let start = before.min(total).saturating_sub(lines.len());
                    (lines, start, total)
                }
                None => {
                    let w = p.window_tail(limit);
                    (w.lines, w.start, w.total)
                }
            }
        }).await;
        return match out {
            Ok((lines, start, total)) =>
                Json(json!({ "session": session, "log": lines, "start": start, "total": total })).into_response(),
            Err(_) =>
                Json(json!({ "session": session, "log": [], "start": 0, "total": 0 })).into_response(),
        };
    }
    // Default (legacy) shape: the whole filtered log. Parity with routes.ts GET /api/sessions/:id.
    let log = filter_rendered(&st.engine.get_log(&id));
    Json(json!({ "session": session, "log": log })).into_response()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: FAIL still — the routes aren't mounted yet (the `oneshot_req` 404s on the path). Mount them in `server-rs/src/api/mod.rs`'s `app()`:

```rust
        .route("/api/sessions", get(sessions::list_sessions))
        .route("/api/sessions/{id}", get(sessions::get_session))
```

Add `use sessions::*;` is NOT needed (we reference `sessions::`). Remove the Phase-0 probe line `.route("/api/ping", get(ping))` and delete the `async fn ping` function. Re-run:

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: PASS (all four tests).

Note: any `api::mod` test that referenced `/api/ping` must move to `/api/sessions` (the auth-gate test): change `api_gate_blocks_without_token_and_allows_with` to hit `/api/sessions` instead of `/api/ping`. Run the full `cargo test` once to confirm nothing else referenced `ping`.

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/sessions.rs server-rs/src/api/test_support.rs server-rs/src/api/mod.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T3: GET /api/sessions + GET /api/sessions/:id (windowed transcript from projection)"
```

---

## Task 4: Session write routes — create + messages (follow-up)

**Files:**
- Modify: `server-rs/src/api/sessions.rs` (add `create_session`, `post_message`)
- Modify: `server-rs/src/api/mod.rs` (mount `POST /api/sessions`, `POST /api/sessions/:id/messages`)
- Test: `server-rs/src/api/sessions.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Engine::submit_session(repos, skills, prompt, env, SubmitMeta) -> Result<String, String>`; `Engine::get(&str)`; `Engine::follow_up(&str, &str, bool) -> Result<i64, String>`; `Engine::get_log(&str)`; `transcript::raw_to_rendered_offset`.
- Produces:
  - `pub async fn create_session(State<AppState>, body) -> Response` → `400 {"error":"prompt required"}` / `400 {"error": engine_msg}` / `{ "id": ... }`.
  - `pub async fn post_message(State<AppState>, Path<String>, body) -> Response` → 404 / `400 {"error":"prompt required"}` / `400 {"error": msg}` / `{ "ok": true, "since": <filtered offset> }`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `server-rs/src/api/sessions.rs`:

```rust
    #[tokio::test]
    async fn create_requires_prompt() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"repos":["demo"]}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({"error":"prompt required"}));
    }

    #[tokio::test]
    async fn create_unknown_repo_is_400() {
        let st = test_state().await;
        // clone_fn errors in tests → an absent repo cannot resolve → submit_session Err → 400.
        let (status, _body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"repo":"nope","prompt":"x"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn message_unknown_session_is_404() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions/nope/messages")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"x"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"error":"not found"}));
    }

    #[tokio::test]
    async fn message_without_prompt_is_400() {
        let st = test_state().await;
        let id = "s-msg";
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("agentic/x".into()), ..Default::default()
        }).await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/messages"))
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({"error":"prompt required"}));
    }
```

(The happy-path create→GET round-trip is exercised end-to-end in Task 7 with the fake-claude fixture; here we cover the validation/error branches that don't need a real process.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: FAIL — `create_session`/`post_message` not found / routes 404.

- [ ] **Step 3: Write minimal implementation**

Add to `server-rs/src/api/sessions.rs`:

```rust
use std::collections::HashMap;
use crate::engine::SubmitMeta;

#[derive(Deserialize, Default)]
pub struct CreateBody {
    pub repo: Option<String>,
    pub repos: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
}

pub async fn create_session(State(st): State<AppState>, body: Option<Json<CreateBody>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let repos = b.repos.unwrap_or_else(|| b.repo.into_iter().collect());
    let skills = b.skills.unwrap_or_default();
    let Some(prompt) = b.prompt.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"prompt required"}))).into_response();
    };
    let meta = SubmitMeta { model: b.model, effort: b.effort, mode: b.mode };
    match st.engine.submit_session(repos, skills, prompt, HashMap::new(), meta) {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct MessageBody {
    pub prompt: Option<String>,
    #[serde(rename = "setTitle")]
    pub set_title: Option<bool>,
}

pub async fn post_message(State(st): State<AppState>, Path(id): Path<String>, body: Option<Json<MessageBody>>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let Some(prompt) = b.prompt.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"prompt required"}))).into_response();
    };
    match st.engine.follow_up(&id, &prompt, b.set_title.unwrap_or(true)) {
        Ok(raw_since) => {
            // Convert the RAW offset follow_up returns to the FILTERED coordinate the client counts.
            let since = crate::transcript::raw_to_rendered_offset(&st.engine.get_log(&id), raw_since as usize);
            Json(json!({ "ok": true, "since": since })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}
```

Mount in `server-rs/src/api/mod.rs` `app()`:

```rust
        .route("/api/sessions", get(sessions::list_sessions).post(sessions::create_session))
        .route("/api/sessions/{id}/messages", post(sessions::post_message))
```

(Replace the earlier `.route("/api/sessions", get(sessions::list_sessions))` line with the combined `get(...).post(...)` form.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/sessions.rs server-rs/src/api/mod.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T4: POST /api/sessions + POST /api/sessions/:id/messages (rendered-coord since)"
```

---

## Task 5: Lifecycle routes — kill (DELETE), interrupt, discard, delete, workflows

**Files:**
- Modify: `server-rs/src/api/sessions.rs` (add `delete_session_route`, `interrupt_route`, `discard_route`, `remove_route`, `workflows_route`)
- Modify: `server-rs/src/api/mod.rs` (mount the five routes)
- Test: `server-rs/src/api/sessions.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Engine::get(&str)`; `Engine::kill(&str)`; `Engine::interrupt(&str)`; `Engine::discard(&str) -> Result<(),String>`; `Engine::delete_session(&str, bool) -> Result<(),String>`; `engine::has_active_workflow(&Path) -> bool`.
- Produces: five `pub async fn ... -> Response` handlers, each 404 on missing session (except where noted), `{ "ok": true }` on success, `400 {"error": msg}` on engine error, and `workflows_route` → `{ "workflows": [...] }` (never 500 on a no-worktree session).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `server-rs/src/api/sessions.rs`:

```rust
    #[tokio::test]
    async fn delete_interrupt_discard_on_unknown_session_is_404() {
        let st = test_state().await;
        for (method_path, m) in [
            ("/api/sessions/nope", "DELETE"),
            ("/api/sessions/nope/interrupt", "POST"),
            ("/api/sessions/nope/discard", "POST"),
            ("/api/sessions/nope/delete", "POST"),
        ] {
            let req = Request::builder().method(m).uri(method_path)
                .header("authorization", auth(&st)).header("content-type", "application/json")
                .body(Body::empty()).unwrap();
            let (status, body) = oneshot_req(st.clone(), req).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{m} {method_path}");
            assert_eq!(body, json!({"error":"not found"}), "{m} {method_path}");
        }
    }

    #[tokio::test]
    async fn interrupt_with_empty_json_body_is_ok() {
        let st = test_state().await;
        let id = "s-int";
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("agentic/x".into()), ..Default::default()
        }).await.unwrap();
        // Empty body + application/json (the client always sends this) must NOT 400.
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/interrupt"))
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
    }

    #[tokio::test]
    async fn workflows_no_worktree_returns_empty_not_500() {
        let st = test_state().await;
        let id = "s-wf";
        // A no-worktree row (worktreePath cleared) must not 500 on /workflows.
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("x".into()), branch: Some("b".into()), ..Default::default()
        }).await.unwrap();
        st.store.update(id, crate::store::SessionPatch {
            // clear the worktree path via a fresh update path; if SessionPatch lacks worktree_path,
            // seed with None directly in create instead (see note below).
            ..Default::default()
        }).await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/workflows"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"workflows": []}));
    }
```

Note on the `workflows_no_worktree` seed: `SessionPatch` (Phase 1) has no `worktree_path` field, so to test the None case, create the row with `worktree_path: None` directly:

```rust
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: None, branch: Some("b".into()), ..Default::default()
        }).await.unwrap();
```

Use that form and delete the `st.store.update(...)` line in the test.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: FAIL — handlers not found / routes 404.

- [ ] **Step 3: Write minimal implementation**

Add to `server-rs/src/api/sessions.rs`:

```rust
pub async fn delete_session_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    st.engine.kill(&id);
    Json(json!({"ok": true})).into_response()
}

pub async fn interrupt_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    st.engine.interrupt(&id);
    Json(json!({"ok": true})).into_response()
}

pub async fn discard_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    match st.engine.discard(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn remove_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    // Explicit "make it gone": force-stop a running/queued turn first (parity with routes.ts /delete).
    match st.engine.delete_session(&id, true).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn workflows_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    // No worktree → empty (never a join(null) 500). Parity with routes.ts.
    let Some(wt) = s.worktree_path.as_deref().filter(|p| !p.is_empty()) else {
        return Json(json!({"workflows": []})).into_response();
    };
    // Phase 5: status-only — surface whether a workflow is active. The full per-run/per-agent
    // payload (listWorkflows) is Phase 6. has_active_workflow already lives in engine.rs.
    let base = std::path::Path::new(wt).join(".claude-config");
    let _active = crate::engine::has_active_workflow(&base);
    // The empty list is the parity-safe default until Phase 6 wires the full listWorkflows shape.
    Json(json!({"workflows": []})).into_response()
}
```

Mount in `server-rs/src/api/mod.rs` `app()`:

```rust
        .route("/api/sessions/{id}", get(sessions::get_session).delete(sessions::delete_session_route))
        .route("/api/sessions/{id}/interrupt", post(sessions::interrupt_route))
        .route("/api/sessions/{id}/discard", post(sessions::discard_route))
        .route("/api/sessions/{id}/delete", post(sessions::remove_route))
        .route("/api/sessions/{id}/workflows", get(sessions::workflows_route))
```

(Replace the earlier `.route("/api/sessions/{id}", get(sessions::get_session))` line with the combined `get(...).delete(...)` form.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::sessions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/sessions.rs server-rs/src/api/mod.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T5: DELETE/interrupt/discard/delete/workflows session lifecycle routes"
```

---

## Task 6: `is_client_disconnect` helper

**Files:**
- Create: `server-rs/src/api/stream.rs` (the helper + its tests; the WS handler is added in Task 7)
- Modify: `server-rs/src/api/mod.rs` (declare `mod stream;`)
- Test: `server-rs/src/api/stream.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub fn is_client_disconnect(code: Option<&str>, msg: Option<&str>) -> bool` — mirror of `server.ts`'s `isClientDisconnect`. Used by the WS send loop (Task 7) to downgrade benign disconnects to `tracing::debug!` instead of `error!`.

- [ ] **Step 1: Write the failing test**

Create `server-rs/src/api/stream.rs`:

```rust
/// A stream/connection error caused by the CLIENT going away (app closed/backgrounded or a
/// network blip mid-stream), not a server fault — downgraded to debug so a genuine 500 still
/// stands out. Mirror of server.ts isClientDisconnect. The Rust side has no Node error codes;
/// we match on the code/message text a tungstenite/axum send error surfaces.
pub fn is_client_disconnect(code: Option<&str>, msg: Option<&str>) -> bool {
    matches!(code, Some("ERR_STREAM_PREMATURE_CLOSE") | Some("ECONNRESET") | Some("EPIPE"))
        || msg == Some("Premature close")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_benign_client_disconnects() {
        assert!(is_client_disconnect(Some("ERR_STREAM_PREMATURE_CLOSE"), None));
        assert!(is_client_disconnect(None, Some("Premature close")));
        assert!(is_client_disconnect(Some("ECONNRESET"), None));
        assert!(is_client_disconnect(Some("EPIPE"), None));
    }

    #[test]
    fn does_not_flag_real_server_errors() {
        assert!(!is_client_disconnect(Some("ERR_SOMETHING_ELSE"), None));
        assert!(!is_client_disconnect(None, Some("internal error")));
        assert!(!is_client_disconnect(None, None));
    }
}
```

Add `mod stream;` to `server-rs/src/api/mod.rs` next to `mod sessions;`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::stream`
Expected: FAIL only if the module isn't wired — once `mod stream;` is added and the file exists, this is a green-on-first-write helper. If it compiles and passes immediately, that is acceptable (the helper is pure); proceed.

- [ ] **Step 3: Write minimal implementation**

(Already written in Step 1 — the helper body is complete.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::stream`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/stream.rs server-rs/src/api/mod.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T6: api::stream::is_client_disconnect (benign-disconnect log downgrade)"
```

---

## Task 7: WS `/api/sessions/:id/stream` — backfill from `?since` then live subscribe

**Files:**
- Modify: `server-rs/Cargo.toml` (enable axum `ws` feature; add dev-deps `tokio-tungstenite`, `futures-util`)
- Modify: `server-rs/src/api/stream.rs` (add the WS handler `stream_session`)
- Modify: `server-rs/src/api/mod.rs` (mount `GET /api/sessions/:id/stream`)
- Test: `server-rs/src/api/stream.rs` `#[cfg(test)] mod tests` (end-to-end over a real listener + the fake-claude fixture)

**Interfaces:**
- Consumes: `Engine::get(&str)`, `Engine::get_log(&str)`, `Engine::subscribe(&str, Subscriber) -> Box<dyn FnOnce()+Send>` (where `Subscriber = Box<dyn Fn(&ClaudeEvent)+Send+Sync>`); `transcript::filter_rendered`; `stream::parse_line`; `ClaudeEvent::to_wire`; `auth::verify_token`.
- Produces: `pub async fn stream_session(ws: WebSocketUpgrade, State<AppState>, Path<String>, Query<StreamQuery>) -> Response`, where `pub struct StreamQuery { token: Option<String>, since: Option<String> }`. Mounted at `GET /api/sessions/:id/stream`.

- [ ] **Step 1: Add deps + write the failing test**

In `server-rs/Cargo.toml`, change the axum line and add dev-deps:

```toml
axum = { version = "0.8", features = ["ws"] }
```

```toml
[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
tokio-tungstenite = "0.24"
futures-util = "0.3"
```

Add to the `#[cfg(test)] mod tests` in `server-rs/src/api/stream.rs`:

```rust
    use crate::api::test_support::test_state;
    use crate::auth::issue_token;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use std::time::Duration;

    fn fixture(name: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../server/test/fixtures").join(name)
            .to_string_lossy().into_owned()
    }

    async fn serve(st: crate::state::AppState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = crate::api::app(st)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        port
    }

    async fn wait_status(st: &crate::state::AppState, id: &str, want: &str) {
        for _ in 0..250 {
            if st.engine.get(id).await.map(|s| s.status) == Some(want.to_string()) { return; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for status={want}");
    }

    #[tokio::test]
    async fn ws_rejects_a_bad_token() {
        let st = test_state().await;
        let id = "s-ws-bad";
        st.store.create(crate::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("b".into()), ..Default::default()
        }).await.unwrap();
        let port = serve(st).await;
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token=bad");
        let res = tokio_tungstenite::connect_async(url).await;
        // The auth gate rejects the upgrade (401) → connect fails OR the socket closes immediately.
        assert!(res.is_err(), "bad token must not establish a WS");
    }

    #[tokio::test]
    async fn ws_streams_a_session_to_completion() {
        let mut st = test_state().await;
        // Point the engine at a real git repo + the fake-claude fixture so a turn actually runs.
        // Reuse the engine test seam: submit needs a resolvable repo. Use a no-repo session driven
        // by the streaming fixture via submit_session with an empty repo set is not enough (no run).
        // Instead use the engine's submit() against a temp repo: see helper below.
        let (port, id) = spawn_live_session(&mut st).await;
        let token = issue_token(&st.config.auth_secret, 3600, now_secs_test());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}");
        let (mut sock, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

        let mut kinds = Vec::new();
        // Read until the socket closes (server closes after engineExit).
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                kinds.push(v["kind"].as_str().unwrap_or("").to_string());
            }
        }
        assert!(kinds.iter().any(|k| k == "text"), "must deliver at least one text event, got {kinds:?}");
    }

    #[tokio::test]
    async fn ws_since_at_log_length_sends_only_engine_exit() {
        let mut st = test_state().await;
        let (port, id) = spawn_live_session(&mut st).await;
        wait_status(&st, &id, "done").await;
        let since = st.engine.get_log(&id).len(); // past every existing line
        let token = issue_token(&st.config.auth_secret, 3600, now_secs_test());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}&since={since}");
        let (mut sock, _r) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut msgs = Vec::new();
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                msgs.push(serde_json::from_str::<Value>(&t).unwrap());
            }
        }
        // No backfill lines (since skipped them all); only the synthetic terminal engineExit.
        assert!(!msgs.iter().any(|m| m["kind"] == "text"));
        assert!(msgs.iter().all(|m| m["kind"] == "other" && m["raw"].get("engineExit").is_some()),
            "only engineExit frames expected, got {msgs:?}");
    }

    fn now_secs_test() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    /// Build a single-repo session driven by the streaming fake-claude, serve the app, and return
    /// (port, session_id). The engine needs a resolvable repo, so seed a temp git repo + a runner
    /// that points at fake-claude-stream.sh. Mirrors the engine's own integration-test setup.
    async fn spawn_live_session(st: &mut crate::state::AppState) -> (u16, String) {
        use std::process::Command;
        // Create a temp src repo "demo".
        let dir = st.config.src_root.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&dir).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&dir).status().unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&dir).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&dir).status().unwrap();
        // NOTE: st was built with clone_fn that errors; but the repo now EXISTS locally, so
        // ensure_local returns it without cloning. The engine's default LocalRunner spawns
        // claude_bin — set it to the streaming fixture.
        // Rebuild AppState's engine with claude_bin pointed at the fixture:
        let port = serve(st.clone()).await;
        let id = st.engine.submit("demo", "go", std::collections::HashMap::new()).unwrap();
        (port, id)
    }
```

Important wiring note for `spawn_live_session`: the engine's `claude_bin` must be the streaming fixture. `test_state()` (Task 3) builds the engine with `claude_bin: c.claude_bin` (default `"claude"`). For these two live WS tests, override it: in `test_support::test_state`, set `c.claude_bin = fixture("fake-claude-stream.sh")` BEHIND a parameter, OR add a sibling helper `test_state_with_fixture(name)` that sets `claude_bin` before building the engine. Add this helper to `test_support.rs`:

```rust
pub async fn test_state_with_fixture(fixture_name: &str) -> AppState {
    let mut st = test_state().await; // builds dirs + store
    // Rebuild the engine with claude_bin = the fixture path.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../server/test/fixtures").join(fixture_name).to_string_lossy().into_owned();
    let mut c = (*st.config).clone();
    c.claude_bin = fixture.clone();
    let engine_cfg = crate::engine::EngineConfig {
        src_root: c.src_root.clone(), worktrees_root: c.worktrees_root.clone(),
        log_dir: c.log_dir.clone(), db_path: c.db_path.clone(), claude_bin: fixture,
        max_concurrent: Some(2), git_org: c.git_org.clone(),
        claude_config_base: c.claude_config_base.clone(),
        clone_fn: Some(std::sync::Arc::new(|_u: &str, _d: &str| {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "clone disabled in tests"))
        })),
        sync_fn: None, runner: None, log_fn: None, now_fn: None, push_fn: None,
        idle_max_ms: None, wall_max_ms: None, idle_ttl_ms: None,
        memory_max: None, memory_high: None, cpu_quota: None, tasks_max: None,
    };
    let engine = std::sync::Arc::new(crate::engine::Engine::with_store(engine_cfg, st.store.clone(), Some(st.transcript.clone())));
    st.engine = engine;
    st.config = std::sync::Arc::new(c);
    st
}
```

Then in the two live tests, replace `test_state().await` + `spawn_live_session` with `test_state_with_fixture("fake-claude.sh").await`, create the temp `demo` repo inline, `serve(st.clone()).await`, and `st.engine.submit("demo","go", HashMap::new())`. (Use `fake-claude.sh` — the one-shot fixture that emits two text deltas + a result — for the "streams to completion" and "engineExit only" tests; it does not need stdin.) `Config` already derives `Clone` (verified: `config.rs` has `#[derive(Clone, Debug)]`), so `(*st.config).clone()` compiles with no change to `config.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::stream::tests::ws_`
Expected: FAIL — `stream_session` not found / route 404 / WS upgrade unhandled.

- [ ] **Step 3: Write minimal implementation**

Add to `server-rs/src/api/stream.rs` (top of file):

```rust
use axum::extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, Query, State};
use axum::response::Response;
use serde::Deserialize;
use crate::state::AppState;
use crate::stream::parse_line;
use crate::transcript::filter_rendered;

#[derive(Deserialize, Default)]
pub struct StreamQuery {
    pub token: Option<String>,
    pub since: Option<String>,
}

pub async fn stream_session(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Response {
    // The shared auth_gate already accepts ?token=; this is the WS-specific re-check so we can
    // close with the exact 1008 code the client expects on a bad/expired token (parity stream.ts).
    let token = q.token.unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let authed = crate::auth::verify_token(&st.config.auth_secret, &token, now);
    ws.on_upgrade(move |socket| handle_socket(socket, st, id, q.since, authed))
}

async fn handle_socket(mut socket: WebSocket, st: AppState, id: String, since_raw: Option<String>, authed: bool) {
    if !authed {
        let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1008, reason: "unauthorized".into(),
        }))).await;
        return;
    }
    if st.engine.get(&id).await.is_none() {
        let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1008, reason: "not found".into(),
        }))).await;
        return;
    }

    // since = max(0, parseInt || 0). Absent → 0.
    let since: usize = match since_raw {
        Some(s) => s.trim().parse::<i64>().unwrap_or(0).max(0) as usize,
        None => 0,
    };

    // Backfill the FILTERED log from `since`. parseLine each line; if empty, send a `backfill` frame.
    let lines = filter_rendered(&st.engine.get_log(&id));
    for line in lines.iter().skip(since) {
        let evs = parse_line(line);
        if evs.is_empty() {
            let raw: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|_| serde_json::Value::String(line.clone()));
            if send_json(&mut socket, &serde_json::json!({"kind":"backfill","raw": raw})).await.is_err() { return; }
        } else {
            for ev in &evs {
                if send_json(&mut socket, &ev.to_wire()).await.is_err() { return; }
            }
        }
    }

    // Re-read status AFTER backfill (a fast turn may have finished between connect and now).
    let session = match st.engine.get(&id).await { Some(s) => s, None => return };
    if matches!(session.status.as_str(), "done" | "failed" | "killed") {
        let exit = serde_json::json!({"kind":"other","raw":{"engineExit":{"code":serde_json::Value::Null,"status":session.status}}});
        let _ = send_json(&mut socket, &exit).await;
        let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "ended".into() }))).await;
        return;
    }

    // Live subscription. The engine's subscribe callback is sync (Fn(&ClaudeEvent)); bridge it to
    // the async socket via an mpsc channel drained below.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let unsub = st.engine.subscribe(&id, Box::new(move |ev| {
        let wire = ev.to_wire();
        let is_exit = matches!(ev, crate::stream::ClaudeEvent::Other { raw } if raw.get("engineExit").is_some());
        let _ = tx.send(wire);
        if is_exit {
            // Signal end-of-stream with a sentinel the drain loop recognizes (a null value).
            let _ = tx.send(serde_json::Value::Null);
        }
    }));

    while let Some(wire) = rx.recv().await {
        if wire.is_null() { break; } // engineExit sentinel → stop and close
        if send_json(&mut socket, &wire).await.is_err() { break; } // client gone
    }
    unsub();
    let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "ended".into() }))).await;
}

/// Send a JSON value as a text frame. On a send error, log benign client-disconnects at debug.
async fn send_json(socket: &mut WebSocket, v: &serde_json::Value) -> Result<(), ()> {
    match socket.send(Message::Text(v.to_string().into())).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if is_client_disconnect(None, Some(msg.as_str())) {
                tracing::debug!("ws client disconnect: {msg}");
            } else {
                tracing::debug!("ws send error: {msg}");
            }
            Err(())
        }
    }
}
```

Mount in `server-rs/src/api/mod.rs` `app()`:

```rust
        .route("/api/sessions/{id}/stream", get(stream::stream_session))
```

Wire-shape note (parity caveat): the engine emits the resumed-turn prompt and `engineExit` as `ClaudeEvent::Other` (engine.rs), so the live `engineExit` arrives as `{kind:"other","raw":{"engineExit":{...}}}` — exactly what the close-detection above matches and what `stream.ts` sends. The backfilled prompt marker (`{"type":"agentic_prompt",...}`) is parsed by `parse_line` into a `prompt` event, so `?since` from `follow_up` replays it correctly (parity with the TS `?since from followUp` test).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server api::stream`
Expected: PASS (bad-token rejected; live stream delivers a `text` event then closes; `?since` at log length sends only `engineExit`).

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/Cargo.toml server-rs/src/api/stream.rs server-rs/src/api/mod.rs server-rs/src/api/test_support.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T7: WS /api/sessions/:id/stream — backfill from ?since then live subscribe"
```

---

## Task 8: End-to-end HTTP session lifecycle over the fake-claude fixture

**Files:**
- Modify: `server-rs/src/api/sessions.rs` `#[cfg(test)] mod tests` (one end-to-end test mirroring `server.test.ts`'s "creates a session and reports it via GET" + "POST /messages resumes a finished session" + "POST /delete removes the session record")

**Interfaces:**
- Consumes: everything mounted in Tasks 3–5 + `test_support::test_state_with_fixture` (Task 7). No new production code — this task is the integration gate proving the routes drive a real engine turn end-to-end.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `server-rs/src/api/sessions.rs`:

```rust
    use crate::api::test_support::test_state_with_fixture;

    async fn seed_demo_repo(st: &crate::state::AppState) {
        use std::process::Command;
        let dir = st.config.src_root.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q"], vec!["config", "user.email", "t@t"], vec!["config", "user.name", "t"],
        ] { Command::new("git").args(&args).current_dir(&dir).status().unwrap(); }
        std::fs::write(dir.join("README.md"), "x").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&dir).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&dir).status().unwrap();
    }

    async fn wait_done(st: &crate::state::AppState, id: &str) {
        for _ in 0..250 {
            if st.engine.get(id).await.map(|s| s.status) == Some("done".into()) { return; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for done");
    }

    #[tokio::test]
    async fn create_run_get_resume_and_delete_round_trip() {
        let st = test_state_with_fixture("fake-claude.sh").await;
        seed_demo_repo(&st).await;
        // create
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"repo":"demo","prompt":"go"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_str().unwrap().to_string();

        // run to completion, then GET reports the session with repo "demo"
        wait_done(&st, &id).await;
        let (status, body) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["session"]["repo"], json!("demo"));
        assert!(body["log"].as_array().unwrap().iter().any(|l|
            l.as_str().unwrap().starts_with("{\"type\":\"assistant\"")
            || l.as_str().unwrap().starts_with("{\"type\":\"result\"")),
            "filtered log must carry rendered lines, got {:?}", body["log"]);

        // resume (POST /messages) → 200 with a numeric `since`
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/messages"))
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"second","setTitle":false}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], json!(true));
        assert!(body["since"].is_u64(), "since must be a filtered offset");

        // delete removes the record → subsequent GET is 404
        wait_done(&st, &id).await;
        let (status, _b) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/delete"))
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server create_run_get_resume_and_delete_round_trip`
Expected: FAIL initially if any route wiring is off; otherwise this is the integration gate — if it passes immediately, the routes are correctly wired (acceptable; this task is a guard test that exercises the whole flow). If it fails on timing, raise the poll bound (250 iterations × 20ms = 5s is the same budget as the TS `waitForStatus`).

- [ ] **Step 3: Write minimal implementation**

No production code — Tasks 3–7 already implement the routes. If the test reveals a gap (e.g. `since` not numeric, or `/delete` not 200), fix the offending handler from Tasks 4–5 inline and re-run.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path server-rs/Cargo.toml -p agentic-dev-server create_run_get_resume_and_delete_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/sessions.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T8: end-to-end create/run/get/resume/delete HTTP round-trip (fake-claude)"
```

---

## Task 9: Full-suite green + final wiring check

**Files:**
- Modify: `server-rs/src/api/mod.rs` (only if the auth-gate test still referenced `/api/ping`; ensure it hits `/api/sessions`)

**Interfaces:** none new.

- [ ] **Step 1: Run the whole suite**

Run: `cargo test --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml`
Expected: ALL tests pass (Phase 0–4 + the new Phase 5 tests). If the Phase-0 `api_gate_blocks_without_token_and_allows_with` test still pointed at `/api/ping` (now removed), repoint it to `/api/sessions` (a mounted authed GET) and re-run.

- [ ] **Step 2: Confirm the server still builds + boots**

Run: `cargo build --manifest-path /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev/server-rs/Cargo.toml`
Expected: clean build (main.rs needs no change — it already serves `api::app(state)` with `ConnectInfo`).

- [ ] **Step 3: Commit any final fixups**

```bash
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev add server-rs/src/api/mod.rs
git -C /home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev commit -m "Phase 5 T9: full-suite green — repoint auth-gate probe to /api/sessions"
```

(If there is nothing to commit, skip this commit.)

---

## Self-Review

**1. Spec coverage** (against the strategy spec + the TS reference `routes.ts`/`stream.ts`/`server.ts`):
- "all REST routes + WS stream to full parity" — Phase 5 covers the **core session surface**: list, get (+ windowed), create, messages, delete/kill, interrupt, discard, delete, workflows (status-only), WS stream. The remaining routes (`/usage`, `/repos`, `/skills`, `/groups`, `/templates`, `/devices`, `/file`, `/upload`, `/outbox`, `/commits`, full workflow transcript, push) are **explicitly deferred to Phase 6** per the strategy phase table ("push (FCM), usage cache, diff/upload/file, misc endpoints") — listed in Global Constraints / Out of scope. Covered by Tasks 3–7.
- "windowed transcript endpoints" — Task 3 (`?limit`/`?before` → `{ log, start, total }` from the projection; `count`/`window_tail`/`range` reused, no whole-file re-read). Matches spec lines 38/95/98.
- "client-disconnect log downgrade" — Tasks 6 (`is_client_disconnect`) + 7 (`send_json` downgrades benign disconnects to debug). Matches `server.ts`.
- WS backfill / `?since` / `engineExit` close / bad-token close — Task 7, mirroring all four `stream.test.ts` cases (streams to completion; rejects bad token; `?since` at log length → engineExit only; `?since` replays a finished follow-up turn — the last via the prompt marker that `parse_line` turns into a `prompt` event).
- `rawToRenderedOffset` for resumed turns — Task 2 + Task 4 (the `messages` route returns the filtered offset). Matches `routes.ts` lines 226-227 + `renderedLog.ts`.
- Wire JSON shape (`kind` + camelCase, `parentToolUseId`/`costUsd` null) — Task 1, mirroring `streamParser.ts`.
- Empty-JSON-body parity for no-arg POSTs — Tasks 4/5 use `Option<Json<...>>` so an absent/empty body is `{}` (mirrors `server.ts`'s content-type parser); test in Task 5.
- Status codes (404/400/401) — every handler; tested in Tasks 3–5.

**2. Placeholder scan:** No "TBD"/"TODO"/"handle errors appropriately"/"similar to Task N" remain. The one `todo!()` (Task 3 Step 1) is a deliberate compile-first stub that the same task's Step 3 replaces — explicitly called out, not a leftover. The Phase-6-deferred routes are named exhaustively, not hand-waved. The `workflows_route` returns `{workflows: []}` by design in Phase 5 (status-only), with the Phase 6 boundary stated — not a placeholder.

**3. Type consistency:**
- `ClaudeEvent::to_wire(&self) -> serde_json::Value` — defined Task 1, consumed Task 7. ✓
- `raw_to_rendered_offset(&[String], usize) -> usize` — defined Task 2, consumed Task 4. ✓
- `Engine` method names match `engine.rs` exactly: `list`/`get`/`get_log`/`submit_session`/`follow_up`/`kill`/`interrupt`/`discard`/`delete_session`/`subscribe`. ✓ (`follow_up` returns `Result<i64, String>`; Task 4 casts `i64 as usize` for `raw_to_rendered_offset`. ✓)
- `SubmitMeta { model, effort, mode }` — matches `engine.rs:121`. ✓
- `CreateInput`/`SessionPatch` field names match `store.rs`. The Task 5 workflows test seeds `worktree_path: None` directly (SessionPatch has no `worktree_path` field — verified against `store.rs:82-94`), avoiding a non-existent patch field. ✓
- `Store::log_path(&str) -> PathBuf`, `Store::read_log`, `Store::append_log(async)` — match `store.rs`. ✓
- `TranscriptCache::with(id, &Path, FnOnce(&RenderedProjection) -> R) -> io::Result<R>` + `RenderedProjection::{window_tail -> Window{start,lines,total}, range -> &[String], count -> usize}` — match `transcript.rs`. ✓
- `Config` already derives `Clone` (`config.rs` `#[derive(Clone, Debug)]`), so `test_state_with_fixture` clones it with no `config.rs` change. ✓ `Config::for_test(secret, password)` and `auth::{issue_token(secret, ttl, now_secs), verify_token(secret, token, now_secs)}` signatures verified against source. ✓
- `auth::{verify_token(secret, token, now_secs), issue_token(secret, ttl, now_secs)}` — match `auth.rs` usage already in `api/mod.rs` tests. ✓

No gaps found; the plan is internally consistent and compiles against the existing crate.
