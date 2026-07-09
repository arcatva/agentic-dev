use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use axum::extract::Query as AxumQuery;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::api::state::AppState;
use crate::engine::search::SearchService;
use crate::engine::store::SessionPatch;

#[derive(Deserialize, Default)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
    pub before: Option<usize>,
}

pub async fn list_sessions(State(st): State<AppState>) -> Response {
    match st.engine.try_list().await {
        Ok(sessions) => Json(json!({ "sessions": sessions })).into_response(),
        // A store failure (e.g. transient sqlite lock from active sessions) MUST surface as 5xx, not a
        // 200 with an empty list. The Android client keeps its last-good session list on an HTTP error
        // (blip tolerance) but REPLACES it on a successful empty body — which briefly drops sessions
        // (most visibly the recently-active ones) on a background-return refresh until the next poll.
        Err(e) => engine_error_response(e),
    }
}

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    /// Free-text search needle. Required: a missing or empty value is a 400, per the
    /// `/api/sessions/search` contract — even though the underlying `SearchService` would
    /// return an empty result set for an empty query, the handler must reject it as a bad
    /// request so the wire contract is explicit.
    pub q: Option<String>,
    /// Max results to return. Defaults to 50, capped at 50 inside SearchService.
    pub limit: Option<usize>,
}

const SEARCH_DEFAULT_LIMIT: usize = 50;

/// How many recent LOGICAL events (cards/turns — prompts, assistant messages, tool/ask/delegate/
/// permission cards, results) the tail transcript window guarantees, regardless of how many
/// streaming-text `stream_event` deltas sit between them. Sized to cover whole real sessions (a very
/// long session in the wild had ~2.5k logical events); the client's `?limit` caps total lines as the
/// memory backstop. See [crate::engine::transcript::RenderedProjection::window_tail_events].
const TAIL_EVENT_BUDGET: usize = 2000;

/// `GET /api/sessions/search?q=...&limit=...`
///
/// Returns `{ "query": "...", "results": [{ "session": {...}, "score": ..., "matches": [...] }] }`.
/// `q` is required and must be non-empty; missing or empty `q` is a 400.
pub async fn search_sessions(
    State(st): State<AppState>,
    AxumQuery(q): AxumQuery<SearchQuery>,
) -> Response {
    let raw = q.q.unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"missing query"}))).into_response();
    }
    let limit = q.limit.unwrap_or(SEARCH_DEFAULT_LIMIT);
    let svc = SearchService::new(st.engine.clone());
    let resp = svc.search(trimmed, limit).await;
    Json(resp).into_response()
}

pub async fn get_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> Response {
    let Some(session) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    // Always use the windowed path — the legacy "whole filtered log" path can OOM the client
    // on long sessions (202 MB raw log → 300+ MB rendered JSON). When no ?limit / ?before is
    // given, default to TAIL_EVENT_BUDGET logical events capped at a generous line ceiling.
    let path = st.store.log_path(&id);
    let limit = q.limit.unwrap_or(100_000);
    let out = st.transcript.with(&id, &path, |p| {
        match q.before {
            Some(before) => {
                let lines: Vec<String> = p.range(before, limit).to_vec();
                let total = p.count();
                let start = before.min(total).saturating_sub(lines.len());
                (lines, start, total)
            }
            None => {
                let w = p.window_tail_events(TAIL_EVENT_BUDGET, limit);
                (w.lines, w.start, w.total)
            }
        }
    }).await;
    match out {
        Ok((lines, start, total)) =>
            Json(json!({ "session": session, "log": lines, "start": start, "total": total })).into_response(),
        Err(_) =>
            Json(json!({ "session": session, "log": [], "start": 0, "total": 0 })).into_response(),
    }
}

// ── Structured events endpoint (Discord-style cursor pagination) ──
//
// Modeled after Discord's GET /channels/{id}/messages:
// - Cursor = rendered-line offset (same space as WS `seq` — compatible)
// - Default limit = 100, max = 100
// - Returns only sealed events — no `stream_event` deltas
// - `before`/`after` refer to rendered-line offsets (same as WS `since`)

#[derive(Deserialize, Default)]
pub struct EventsQuery {
    pub limit: Option<usize>,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub around: Option<usize>,
}

/// Max rendered lines to scan when constructing a filtered-events window.
const EVENTS_MAX_SCAN_LINES: usize = 500_000;

/// GET /api/sessions/{id}/events?limit=N&before={cursor}
/// Returns structured ClaudeEvent wire JSON — only sealed events, no deltas.
/// **Cursor is rendered-line offset** (same as WS `seq`) so `latestEventId` is always
/// the full rendered-line count — compatible with `openStream(?since=N)`.
pub async fn get_session_events(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let Some(session) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let path = st.store.log_path(&id);
    let limit = q.limit.unwrap_or(100).min(100);
    let out = st.transcript.with(&id, &path, |p| {
        let total_lines = p.count();
        let w = if let Some(before) = q.before {
            p.window_filtered_before(before, limit, EVENTS_MAX_SCAN_LINES)
        } else if let Some(after) = q.after {
            p.window_filtered_range(after + 1, limit, EVENTS_MAX_SCAN_LINES)
        } else if let Some(around) = q.around {
            let start = around.saturating_sub(limit / 2);
            p.window_filtered_range(start, limit, EVENTS_MAX_SCAN_LINES)
        } else {
            // Tail: use the client's limit (capped at 100), not TAIL_EVENT_BUDGET
            p.window_tail_logical_events(limit, EVENTS_MAX_SCAN_LINES)
        };
	        let _start_event_line = w.start;
        let events: Vec<Value> = w.lines.iter()
            .flat_map(|line| {
                crate::engine::stream::parse_line(line)
                    .into_iter()
                    .map(|ev| {
                        let mut v = ev.to_wire();
                        if let Some(obj) = v.as_object_mut() { obj.remove("raw"); }
                        v
                    })
            })
            .collect();
        // has_more: there are events before this page (can scroll back).
        // has_more_after: there are events after this page (true for mid-log queries, false for tail).
        let start_event_line = w.start;
        let has_more = w.start > 0;
        let has_more_after = q.before.is_some() || q.after.is_some() || q.around.is_some();
        (events, total_lines, start_event_line, has_more, has_more_after)
    }).await;
    match out {
        Ok((events, latest_event_id, first_event_line, has_more, has_more_after)) =>
            Json(json!({ "session": session, "events": events, "latestEventId": latest_event_id, "firstEventLine": first_event_line, "hasMore": has_more, "hasMoreAfter": has_more_after })).into_response(),
        Err(_) =>
            Json(json!({ "session": session, "events": [], "latestEventId": 0, "firstEventLine": 0, "hasMore": false, "hasMoreAfter": false })).into_response(),
    }
}

// ── Write routes ─────────────────────────────────────────────────

use std::collections::HashMap;
use crate::engine::SubmitMeta;

#[derive(Deserialize, Default)]
pub struct CreateBody {
    pub repo: Option<String>,
    pub repos: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    /// Blacklist: skills to HIDE from this session (camelCase `hiddenSkills` on the wire).
    #[serde(rename = "hiddenSkills")] pub hidden_skills: Option<Vec<String>>,
    /// Blacklist: plugins (`<plugin>@<marketplace>` ids) to DISABLE for this session
    /// (camelCase `hiddenPlugins` on the wire). Absent = all installed plugins stay enabled.
    #[serde(rename = "hiddenPlugins")] pub hidden_plugins: Option<Vec<String>>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    /// Permission mode for this session (e.g. "plan" / "default" / "acceptEdits" /
    /// "bypassPermissions"). Empty string is treated as "no override".
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// Optional session-scoped CLAUDE.md content (camelCase `claudeMd` on the wire). Written into the
    /// session dir so Claude Code loads it as project memory for this session, on top of each repo's
    /// own CLAUDE.md. Absent / blank = no extra guidance.
    #[serde(rename = "claudeMd")]
    pub claude_md: Option<String>,
    /// Files staged via `POST /api/uploads` before this session existed (New-request attachments).
    /// The engine moves each into the new session's `uploads/` dir before the prompt runs, so the
    /// agent can read the `[attached: uploads/<name>]` paths in the very first prompt. camelCase on
    /// the wire; absent = no attachments.
    #[serde(rename = "stagedUploads")]
    pub staged_uploads: Option<Vec<crate::engine::StagedUpload>>,
    /// MCP server names to disable for this session (blacklist). camelCase on the wire.
    #[serde(rename = "hiddenMcpServers")]
    pub hidden_mcp_servers: Option<Vec<String>>,
    /// Ad-hoc MCP server defs for this session only. Validated: name non-empty, exactly one transport.
    #[serde(rename = "extraMcpServers")]
    pub extra_mcp_servers: Option<Vec<crate::engine::store::McpServerDef>>,
    /// Plugin ids to force ON for this session (overrides global-off). Must be disjoint from hiddenPlugins.
    #[serde(rename = "forcedOnPlugins")]
    pub forced_on_plugins: Option<Vec<String>>,
    /// Skill names to force ON for this session (overrides global-off). Must be disjoint from hiddenSkills.
    #[serde(rename = "forcedOnSkills")]
    pub forced_on_skills: Option<Vec<String>>,
    /// MCP server names to force ON (stored; no-op at spawn until global MCP disable exists).
    #[serde(rename = "forcedOnMcpServers")]
    pub forced_on_mcp_servers: Option<Vec<String>>,
}

/// Parse a JSON request body leniently: treat absent, empty, or unparseable bodies as the
/// default struct. Only a content-type:application/json body with valid but semantically wrong
/// JSON (wrong types) falls back to default here — we never surface 422 to the client.
/// This avoids axum's Option<Json<T>> propagating a 422 JsonRejection when content-type is
/// present but the body is empty.
pub(crate) fn parse_body_lenient<T: serde::de::DeserializeOwned + Default>(bytes: &Bytes) -> T {
    if bytes.is_empty() {
        return T::default();
    }
    serde_json::from_slice(bytes).unwrap_or_default()
}

/// Returns `Some(error_message)` if the same component id appears in both a hidden list
/// and the corresponding forced-on list, which is invalid (contradictory overrides).
fn validate_disjoint(hidden: &[String], forced_on: &[String], kind: &str) -> Option<String> {
    let hidden_set: std::collections::HashSet<&str> = hidden.iter().map(|s| s.as_str()).collect();
    for id in forced_on {
        if hidden_set.contains(id.as_str()) {
            return Some(format!("{kind}: \"{id}\" appears in both hidden and forcedOn lists — choose one"));
        }
    }
    None
}

pub async fn create_session(State(st): State<AppState>, body: Bytes) -> Response {
    let b: CreateBody = parse_body_lenient(&body);
    let repos = b.repos.unwrap_or_else(|| b.repo.into_iter().collect());
    let skills = b.skills.unwrap_or_default();
    let Some(prompt) = b.prompt.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"prompt required"}))).into_response();
    };
    let extra_mcp_servers = b.extra_mcp_servers.unwrap_or_default();
    for def in &extra_mcp_servers {
        if def.name.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"extraMcpServers: name must be non-empty"}))).into_response();
        }
        if def.name.trim() == "agentic" {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"extraMcpServers: name \"agentic\" is reserved by the platform delegate server and cannot be overridden"}))).into_response();
        }
        let has_stdio = def.command.is_some();
        let has_http = def.url.is_some();
        if !has_stdio && !has_http {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: must have either command (stdio) or url (http/sse)", def.name)}))).into_response();
        }
        if has_stdio && has_http {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: cannot have both command and url", def.name)}))).into_response();
        }
        if has_stdio {
            if def.command.as_deref().map(|s| s.trim().is_empty()).unwrap_or(false) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: command must be non-empty", def.name)}))).into_response();
            }
        }
        if has_http {
            if def.url.as_deref().map(|s| s.trim().is_empty()).unwrap_or(false) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: url must be non-empty", def.name)}))).into_response();
            }
        }
    }
    let hidden_plugins  = b.hidden_plugins.unwrap_or_default();
    let hidden_skills   = b.hidden_skills.unwrap_or_default();
    let hidden_mcp      = b.hidden_mcp_servers.unwrap_or_default();
    let forced_on_plugins = b.forced_on_plugins.unwrap_or_default();
    let forced_on_skills  = b.forced_on_skills.unwrap_or_default();
    let forced_on_mcp     = b.forced_on_mcp_servers.unwrap_or_default();

    if let Some(msg) = validate_disjoint(&hidden_plugins, &forced_on_plugins, "hiddenPlugins/forcedOnPlugins") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Some(msg) = validate_disjoint(&hidden_skills, &forced_on_skills, "hiddenSkills/forcedOnSkills") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Some(msg) = validate_disjoint(&hidden_mcp, &forced_on_mcp, "hiddenMcpServers/forcedOnMcpServers") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }

    let meta = SubmitMeta {
        model: b.model,
        effort: b.effort,
        mode: b.mode,
        permission_mode: b.permission_mode,
        hidden_skills,
        hidden_plugins,
        hidden_mcp_servers: hidden_mcp,
        extra_mcp_servers,
        claude_md: b.claude_md,
        staged_uploads: b.staged_uploads.unwrap_or_default(),
        forced_on_plugins,
        forced_on_skills,
        forced_on_mcp_servers: forced_on_mcp,
    };
    match st.engine.submit_session(repos, skills, prompt, HashMap::new(), meta).await {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// Map a typed `EngineError` to an HTTP response. NotFound→404, Store/Internal→500, everything
/// else→400 (blanket 400 for caller-fixable engine errors). The body keeps the engine's exact
/// error string, so the Android client sees identical messages.
fn engine_error_response(e: crate::engine::EngineError) -> Response {
    use crate::engine::EngineError as E;
    let status = match &e {
        E::NotFound(_) => StatusCode::NOT_FOUND,
        E::SourceUnhealthy(_) => StatusCode::CONFLICT,
        E::Store(_) | E::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

#[derive(Deserialize, Default)]
pub struct MessageBody {
    pub prompt: Option<String>,
    #[serde(rename = "setTitle")]
    pub set_title: Option<bool>,
    /// Per-turn model override. `None` (or empty string) → fall back to the
    /// session-level value. Server does not validate; SDK rejects unknown models.
    pub model: Option<String>,
    /// Per-turn effort override. Same fallback rules as `model`.
    pub effort: Option<String>,
    /// Per-turn permission mode override. Same fallback rules as `model`.
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct PatchBody {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// Assign session to a group. Empty string = remove from group (uncategorized).
    /// `None` (absent) = leave unchanged.
    #[serde(rename = "groupId")]
    pub group_id: Option<String>,
    /// Auto-resume the session after a usage-limit reset. `None` (absent) = leave unchanged.
    /// Turning it OFF also clears any already-scheduled resume time.
    #[serde(rename = "autoResume")]
    pub auto_resume: Option<bool>,
}

pub async fn patch_session(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Response {
    // First check: session must exist.
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let b: PatchBody = parse_body_lenient(&body);
    let patch = SessionPatch {
        model: b.model.filter(|s| !s.is_empty()),
        effort: b.effort.filter(|s| !s.is_empty()),
        mode: b.mode.filter(|s| !s.is_empty()),
        permission_mode: b.permission_mode.filter(|s| !s.is_empty()),
        group_id: b.group_id.map(|gid| if gid.is_empty() { None } else { Some(gid) }),
        auto_resume: b.auto_resume,
        // Disabling the toggle cancels a pending scheduled resume; enabling leaves any
        // schedule untouched (the scheduler recomputes on its next tick if needed).
        auto_resume_at: if b.auto_resume == Some(false) { Some(None) } else { None },
        ..Default::default()
    };
    match st.engine.patch_session_meta(&id, patch).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => engine_error_response(e),
    }
}

pub async fn post_message(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let b: MessageBody = parse_body_lenient(&body);
    let Some(prompt) = b.prompt.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"prompt required"}))).into_response();
    };
// Default to NOT retitling on follow-up — the title is set once at
    // submit time by generate_title. Clients that want to rename explicitly
    // can still pass setTitle=true in the body.
    match st.engine.follow_up(&id, &prompt, b.set_title.unwrap_or(false), b.model, b.effort, b.permission_mode).await {
        Ok(raw_since) => {
            // Convert the RAW offset follow_up returns to the FILTERED coordinate the client counts.
            let since = crate::engine::transcript::raw_to_rendered_offset(&st.engine.get_log(&id), raw_since as usize);
            Json(json!({ "ok": true, "since": since })).into_response()
        }
        Err(e) => engine_error_response(e),
    }
}

// ── Lifecycle routes ──────────────────────────────────────────────

pub async fn delete_session_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    st.engine.kill(&id).await;
    Json(json!({"ok": true})).into_response()
}

pub async fn interrupt_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    st.engine.interrupt(&id);
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize, Default)]
pub struct PermissionBody {
    /// "allow" or "deny" — anything else is a 400.
    pub decision: Option<String>,
    /// Optional deny reason / plan-revision note; relayed to claude as the deny message (ignored on allow).
    pub feedback: Option<String>,
}

/// `POST /api/sessions/{id}/permission` — answer a parked allow/deny or plan-approval prompt. Distinct
/// from `/messages` (a chat turn) and `/interrupt` (stop the turn): the decision is relayed to the live
/// turn's bridge, which resolves the parked `canUseTool` and writes the resolution marker. Mirrors the
/// `/interrupt` shape; a no-op (still `{ok:true}`) if the turn already ended.
pub async fn permission_route(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let b: PermissionBody = parse_body_lenient(&body);
    let decision = b.decision.unwrap_or_default();
    if decision != "allow" && decision != "deny" {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"decision must be 'allow' or 'deny'"}))).into_response();
    }
    st.engine.respond_permission(&id, &decision, b.feedback.as_deref());
    Json(json!({"ok": true})).into_response()
}

pub async fn discard_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    match st.engine.discard(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => engine_error_response(e),
    }
}

pub async fn remove_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    match st.engine.delete_session(&id, true).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => engine_error_response(e),
    }
}

/// `POST /api/sessions/{id}/fork` — create a new session that is a fork of the source.
/// Returns 201 with `{ id, session }`. The new session has status `"done"` (idle, can
/// accept follow-ups) and is not auto-spawned; it runs when the user opens it and sends a
/// real follow-up prompt.
pub async fn fork_session_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.engine.fork_session(&id).await {
        Ok(session) => (StatusCode::CREATED, Json(json!({ "id": session.id, "session": session }))).into_response(),
        Err(e) => engine_error_response(e),
    }
}

// ── Adopt / detach / adoptable (native Claude re-sync) ────────────

/// Body of `POST /api/sessions/adopt`. camelCase `claudeSessionId` on the wire.
#[derive(Deserialize)]
pub struct AdoptBody {
    #[serde(rename = "claudeSessionId")]
    pub claude_session_id: String,
    pub cwd: String,
}

/// `GET /api/adoptable` — native Claude transcripts not yet tracked by a session, newest
/// first. Excludes csids already linked to a stored session (so an adopted session never
/// re-appears as adoptable). Always 200 with a JSON array.
pub async fn list_adoptable(State(st): State<AppState>) -> Response {
    let known = st.engine.known_claude_session_ids().await;
    let items = crate::engine::native_transcript::scan_adoptable(&st.engine.config_base(), &known);
    Json(items).into_response()
}

/// `POST /api/sessions/adopt` body `{claudeSessionId, cwd}` → `201 {id}`. 400 on a
/// malformed body or when the engine rejects the adopt (no transcript at the slug path,
/// or the csid is already adopted). Mirrors [create_session]'s Bytes-body style, but adopt
/// requires a well-formed body so a parse error is a 400 (not a lenient default).
pub async fn adopt_session_route(State(st): State<AppState>, body: Bytes) -> Response {
    let b: AdoptBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    };
    match st.engine.adopt_session(&b.claude_session_id, &b.cwd).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"id": id}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// `POST /api/sessions/{id}/detach` → `200 {cwd, claudeSessionId, resumeCmd}`. Hands an
/// adopted session off to a terminal `claude --resume`: hard-stops the live process,
/// freezes the native watermark, marks the row detached, and returns the resume command.
/// 400 on engine error (unknown session / no linked csid).
pub async fn detach_session_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.engine.detach_session(&id).await {
        Ok(info) => Json(json!({
            "cwd": info.cwd,
            "claudeSessionId": info.claude_session_id,
            "resumeCmd": info.resume_cmd,
        })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn workflows_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    // Workflow data lives in the shared ~/.claude config dir; scope to THIS session's claude
    // transcript uuid so we never surface another session's runs. No uuid (never ran claude) → [].
    let runs = match s.claude_session_id.as_deref() {
        Some(uuid) => crate::engine::workflows::list_workflows(&st.config.claude_config_base, Some(uuid)),
        None => Vec::new(),
    };
    Json(json!({ "workflows": runs })).into_response()
}

pub async fn workflow_agent_route(
    State(st): State<AppState>,
    Path((id, run_id, agent_id)): Path<(String, String, String)>,
) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let transcript = match s.claude_session_id.as_deref() {
        Some(uuid) => crate::engine::workflows::read_workflow_agent(
            &st.config.claude_config_base, Some(uuid), &run_id, &agent_id),
        None => String::new(),
    };
    Json(json!({ "transcript": transcript })).into_response()
}

pub async fn commits_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    match st.engine.commit_graph(&id).await {
        Ok(repos) => Json(json!({ "repos": repos })).into_response(),
        Err(e) => engine_error_response(e),
    }
}

#[derive(Deserialize, Default)]
pub struct RepoQuery { pub repo: Option<String> }

pub async fn commit_files_route(
    State(st): State<AppState>,
    Path((id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let repo = q.repo.unwrap_or_default();
    match st.engine.commit_files(&id, &repo, &sha).await {
        Ok(files) => Json(json!({ "files": files })).into_response(),
        Err(e) => engine_error_response(e),
    }
}

#[derive(Deserialize, Default)]
pub struct DiffQuery { pub repo: Option<String>, pub path: Option<String> }

/// `GET /api/sessions/{id}/commits/{sha}/diff?repo=&path=` — line-level diff for ONE file in a
/// commit (or the working tree, `sha=working`). Returns `{ diff: FileDiff }` with parsed hunks.
/// `path` is required.
pub async fn commit_diff_route(
    State(st): State<AppState>,
    Path((id, sha)): Path<(String, String)>,
    Query(q): Query<DiffQuery>,
) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let repo = q.repo.unwrap_or_default();
    let Some(path) = q.path.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"path required"}))).into_response();
    };
    match st.engine.commit_diff(&id, &repo, &sha, &path).await {
        Ok(diff) => Json(json!({ "diff": diff })).into_response(),
        Err(e) => engine_error_response(e),
    }
}

#[derive(Deserialize, Default)]
pub struct RewindBody {
    /// 0-based index of the user turn to rewind to (restore the code state just before it ran).
    /// 0 = before the first prompt (the session's base).
    #[serde(rename = "turnIndex")]
    pub turn_index: Option<usize>,
}

/// `POST /api/sessions/{id}/rewind { turnIndex }` — restore the working tree to the snapshot taken
/// just before the given turn ran (tracked files only; untracked files created since are kept). The
/// branch/HEAD and chat history are unchanged. 400 if the session is busy or the snapshot is missing.
pub async fn rewind_route(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let b: RewindBody = parse_body_lenient(&body);
    let Some(turn_index) = b.turn_index else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"turnIndex required"}))).into_response();
    };
    match st.engine.rewind(&id, turn_index).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => engine_error_response(e),
    }
}

// ── File / upload / outbox routes ─────────────────────────────────

use crate::engine::store::Session;

fn session_cwd(s: &Session) -> std::path::PathBuf {
    let root = std::path::PathBuf::from(s.worktree_path.clone().unwrap_or_default());
    if s.repos.len() == 1 { root.join(&s.repos[0]) } else { root }
}

fn safe_path(s: &Session, rel: &str) -> Option<std::path::PathBuf> {
    let wt = s.worktree_path.as_deref()?;
    let base = std::fs::canonicalize(wt).ok()?;
    let candidate = session_cwd(s).join(rel);
    let full = std::fs::canonicalize(&candidate).ok()?;  // needs the file to exist
    if full == base || full.starts_with(&base) { Some(full) } else { None }
}

const MIME_TABLE: &[(&str, &str)] = &[
    ("png","image/png"),("jpg","image/jpeg"),("jpeg","image/jpeg"),("gif","image/gif"),
    ("webp","image/webp"),("bmp","image/bmp"),("svg","image/svg+xml"),("pdf","application/pdf"),
    ("txt","text/plain"),("md","text/markdown"),("json","application/json"),("csv","text/csv"),
    ("html","text/html"),("mp4","video/mp4"),("webm","video/webm"),("mp3","audio/mpeg"),("wav","audio/wav"),
];

fn mime_for(path: &std::path::Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).unwrap_or_default();
    MIME_TABLE.iter().find(|(k, _)| *k == ext).map(|(_, v)| *v).unwrap_or("application/octet-stream")
}

#[derive(Deserialize, Default)]
pub struct FileQuery { pub path: Option<String> }

pub async fn file_route(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<FileQuery>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Some(rel) = q.path.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"path required"}))).into_response();
    };
    let Some(full) = safe_path(&s, &rel) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Ok(meta) = std::fs::metadata(&full) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    if !meta.is_file() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let Ok(bytes) = tokio::fs::read(&full).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let filename = full.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    use axum::http::header;
    use axum::body::Body;
    match axum::response::Response::builder()
        .header(header::CONTENT_TYPE, mime_for(&full))
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{filename}\""))
        .header(header::CONTENT_LENGTH, meta.len())
        .body(Body::from(bytes))
    {
        Ok(r) => r,
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn upload_route(State(st): State<AppState>, Path(id): Path<String>, mut multipart: axum::extract::Multipart) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    if s.worktree_path.as_deref().unwrap_or("").is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"session has no worktree"}))).into_response();
    }
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f, _ => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    let raw_name = field.file_name().unwrap_or("upload").to_string();
    let safe = crate::engine::sanitize_upload_name(&raw_name);
    let dir = session_cwd(&s).join("uploads");
    if std::fs::create_dir_all(&dir).is_err() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cannot create uploads dir"}))).into_response();
    }
    let dest = dir.join(&safe);
    let data = match field.bytes().await {
        Ok(b) => b, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    if data.len() as u64 > st.config.upload_max_bytes as u64 {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error":"file too large"}))).into_response();
    }
    if tokio::fs::write(&dest, &data).await.is_err() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"write failed"}))).into_response();
    }
    Json(json!({ "path": format!("uploads/{safe}") })).into_response()
}

/// `POST /api/uploads` — stage a file BEFORE any session exists. The New-request form uploads
/// attachments here (there is no session id yet), then lists them under `stagedUploads` in the create
/// request; [crate::engine::Engine::submit_session] moves each into the new session's `uploads/` dir
/// before the first prompt runs. Writes to `<worktrees_root>/.staging/<token>/<name>` and returns
/// `{ token, name, path }`, where `path` (`uploads/<name>`) is what the client embeds in the prompt's
/// `[attached: ...]` marker. Mirrors [upload_route]'s multipart parsing + size cap; needs no session.
pub async fn upload_staging_route(State(st): State<AppState>, mut multipart: axum::extract::Multipart) -> Response {
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f,
        _ => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    let raw_name = field.file_name().unwrap_or("upload").to_string();
    let safe = crate::engine::sanitize_upload_name(&raw_name);
    let data = match field.bytes().await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    if data.len() as u64 > st.config.upload_max_bytes as u64 {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error":"file too large"}))).into_response();
    }
    // Do the staging filesystem work off the async executor: the orphan sweep can scan + delete many
    // dirs, and create_dir_all + write are blocking syscalls — running them on the Tokio runtime would
    // stall it. The sweep is best-effort (prunes staging dirs >24h from New-request flows abandoned
    // before create). Returns the token + sanitized name written.
    let worktrees_root = st.config.worktrees_root.clone();
    let staged = tokio::task::spawn_blocking(move || -> std::io::Result<(String, String)> {
        crate::engine::sweep_stale_staging(&worktrees_root, std::time::Duration::from_secs(24 * 60 * 60));
        let token = crate::engine::new_staging_token();
        let dir = crate::engine::staging_root(&worktrees_root).join(&token);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(&safe), &data)?;
        Ok((token, safe))
    })
    .await;
    match staged {
        Ok(Ok((token, name))) => {
            let path = format!("uploads/{name}");
            Json(json!({ "token": token, "name": name, "path": path })).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"write failed"}))).into_response(),
    }
}

/// Recursively collect regular files under `base`, emitting each as `{label}/<subpath>` relative to
/// the session cwd. The agent is told to "write into ./outbox/" and routinely nests deliverables in
/// subfolders (e.g. `outbox/logo/icon.svg`); a flat, non-recursive scan silently hid those so the app
/// showed nothing. Recursion is bounded by depth and total count, and only REAL directories are
/// descended (a symlinked dir reports a non-dir `file_type()` and is treated as a leaf), so a symlink
/// cycle can never loop. Symlinked regular files are still listed (metadata() follows), matching the
/// old `e.path().is_file()` behavior.
fn collect_outbox(base: &std::path::Path, label: &str, out: &mut Vec<serde_json::Value>) {
    const MAX_DEPTH: usize = 8;
    const MAX_FILES: usize = 1000;
    let mut stack: Vec<(std::path::PathBuf, String, usize)> = vec![(base.to_path_buf(), label.to_string(), 0)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue; };
        // read_dir order is OS-dependent; sort for a deterministic listing.
        let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
        entries.sort_by_cached_key(|e| e.file_name());
        for e in entries {
            if out.len() >= MAX_FILES { return; }
            let name = e.file_name().to_string_lossy().into_owned();
            let child_rel = format!("{rel}/{name}");
            // file_type() does NOT follow symlinks → only descend genuine subdirectories.
            let Ok(ftype) = e.file_type() else { continue; };
            if ftype.is_dir() {
                if depth < MAX_DEPTH { stack.push((e.path(), child_rel, depth + 1)); }
                continue;
            }
            // metadata() follows symlinks: confirm a regular file and read its mtime.
            let Ok(meta) = std::fs::metadata(e.path()) else { continue; };
            if !meta.is_file() { continue; }
            let mtime = meta.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(json!({ "path": child_rel, "name": name, "mtime": mtime }));
        }
    }
}

pub async fn outbox_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Some(wt) = s.worktree_path.as_deref().filter(|p| !p.is_empty()) else {
        return Json(json!({"files": []})).into_response();
    };
    let mut dirs: Vec<(std::path::PathBuf, String)> = vec![(session_cwd(&s).join("outbox"), "outbox".to_string())];
    if s.repos.len() > 1 {
        for repo in &s.repos {
            dirs.push((std::path::Path::new(wt).join(repo).join("outbox"), format!("{repo}/outbox")));
        }
    }
    let mut files = Vec::new();
    for (dir, rel) in dirs {
        collect_outbox(&dir, &rel, &mut files);
    }
    // Stable order across the (possibly several) scanned roots and the recursive walk.
    files.sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
    Json(json!({ "files": files })).into_response()
}

/// `PUT /api/sessions/{id}/ack` — acknowledge the current unread event.
/// Body: `{"eventId": N}`. Sets ackedEventId = MAX(ackedEventId, N) on the server.
pub async fn ack_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(event_id) = body.get("eventId").and_then(|v| v.as_i64()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"missing eventId"}))).into_response();
    };
    match st.store.ack_event(&id, event_id).await {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Err(e) => {
            tracing::warn!("[api] ack {id} failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{test_state, auth, oneshot_req};
    use crate::api::test_support::test_state_with_fixture;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

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
        st.store.create(crate::engine::store::CreateInput {
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
        // always windowed: start/total are present even without query params.
        assert_eq!(body["start"], json!(0));
        assert_eq!(body["total"], json!(1));
    }

    #[tokio::test]
    async fn get_session_windowed_returns_tail_with_start_and_total() {
        let st = test_state().await;
        let id = "s-win";
        st.store.create(crate::engine::store::CreateInput {
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
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some("/tmp/x".into()), branch: Some("agentic/x".into()), ..Default::default()
        }).await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/messages"))
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({"error":"prompt required"}));
    }

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
        st.store.create(crate::engine::store::CreateInput {
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
        // A no-worktree row: create with worktree_path: None
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: None, branch: Some("b".into()), ..Default::default()
        }).await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/workflows"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"workflows": []}));
    }

    #[tokio::test]
    async fn discard_on_a_busy_session_returns_400_session_busy() {
        // A 'running' (busy) session cannot be discarded — assert the exact HTTP status + body
        // at the route layer. Seed the running status directly (no process needed: is_busy is
        // status-based).
        let st = test_state().await;
        let id = "s-busy";
        st.store.create(crate::engine::store::CreateInput { id: id.into(), prompt: "p".into(), ..Default::default() })
            .await.unwrap();
        st.store.update(id, crate::engine::store::SessionPatch { status: Some("running".into()), ..Default::default() })
            .await.unwrap();
        let (status, body) = oneshot_req(st.clone(),
            Request::post(format!("/api/sessions/{id}/discard"))
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "session busy");
    }

    #[test]
    fn engine_error_response_maps_status_and_preserves_message() {
        use crate::engine::EngineError as E;
        let cases = [
            (E::NotFound("x".into()), StatusCode::NOT_FOUND, "unknown session: x"),
            (E::Busy, StatusCode::BAD_REQUEST, "session busy"),
            (E::WorktreeCleaned, StatusCode::BAD_REQUEST, "worktree already cleaned"),
            (E::NoWorktree, StatusCode::BAD_REQUEST, "session has no worktree"),
            (E::BadInput("unknown repo: z".into()), StatusCode::BAD_REQUEST, "unknown repo: z"),
            (E::SourceUnhealthy("worktree missing".into()), StatusCode::CONFLICT, "source unhealthy: worktree missing"),
            (E::Internal("boom".into()), StatusCode::INTERNAL_SERVER_ERROR, "boom"),
        ];
        for (err, want_status, want_msg) in cases {
            assert_eq!(err.to_string(), want_msg, "Display must preserve the exact message");
            assert_eq!(super::engine_error_response(err).status(), want_status);
        }
    }

    async fn seed_demo_repo(st: &crate::api::state::AppState) {
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

    async fn wait_done(st: &crate::api::state::AppState, id: &str) {
        for _ in 0..250 {
            if st.engine.get(id).await.map(|s| s.status) == Some("done".into()) { return; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for done");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_run_get_resume_and_delete_round_trip() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
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
        // Classify by PARSED type, not a byte-prefix: the engine writes its markers with alphabetical
        // keys (`type` not first), so a `starts_with("{\"type\":…")` assertion is the brittle pattern
        // that shipped the vanishing-prompt bug.
        let types: Vec<String> = body["log"].as_array().unwrap().iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.as_str().unwrap()).ok())
            .filter_map(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
            .collect();
        assert!(types.iter().any(|t| t == "assistant" || t == "result"),
            "filtered log must carry claude's rendered lines, got {:?}", body["log"]);
        // REGRESSION (the integration gap that let the bug ship): the engine-emitted agentic_prompt
        // ("go") is serialized with `type` LAST; it MUST survive filter_rendered, or the user's own
        // message silently vanishes from the transcript and never replays on reseed.
        assert!(types.iter().any(|t| t == "agentic_prompt"),
            "engine agentic_prompt must survive filter_rendered (type-last serialization), got {:?}", body["log"]);

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

    #[tokio::test]
    async fn file_route_serves_a_worktree_file_with_length_and_mime() {
        let st = test_state().await;
        let id = "s-file";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("hello.txt"), "hi there").unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let resp = crate::api::app(st.clone()).oneshot(
            Request::get(format!("/api/sessions/{id}/file?path=hello.txt"))
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
        assert_eq!(resp.headers().get("content-length").unwrap(), "8");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hi there");
    }

    #[tokio::test]
    async fn gzip_compresses_large_json_but_not_file_downloads() {
        use axum::http::header;
        let st = test_state().await;
        // Seed enough sessions that GET /api/sessions exceeds the 2 KB threshold.
        for i in 0..40 {
            st.store.create(crate::engine::store::CreateInput {
                id: format!("s-{i:03}"), repos: vec![], skills: vec![],
                prompt: format!("prompt {i} with padding text so each row is non-trivial"),
                worktree_path: Some(format!("/tmp/wt/s-{i:03}")), branch: Some("main".into()),
                ..Default::default()
            }).await.unwrap();
        }
        // (1) A >2 KB JSON list, client accepts gzip → response is gzipped.
        let resp = crate::api::app(st.clone()).oneshot(
            Request::get("/api/sessions")
                .header("authorization", auth(&st))
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "a >2KB JSON response must be gzipped when the client accepts gzip");

        // (2) /file is NOT gzipped even when large — keeps its explicit Content-Length.
        let id = "s-bigfile";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("big.txt"), "x".repeat(5000)).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let fresp = crate::api::app(st.clone()).oneshot(
            Request::get(format!("/api/sessions/{id}/file?path=big.txt"))
                .header("authorization", auth(&st))
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(fresp.status(), StatusCode::OK);
        assert_eq!(fresp.headers().get(header::CONTENT_ENCODING), None,
            "/file must NOT be gzipped");
        assert_eq!(
            fresp.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some("5000"),
            "/file must keep its explicit Content-Length");
    }

    #[tokio::test]
    async fn file_route_rejects_escape_and_missing() {
        let st = test_state().await;
        let id = "s-esc";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        // missing path param → 400
        let (s, b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/file"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b, json!({"error":"path required"}));
        // escape attempt → 404
        let (s2, _b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/file?path=../../etc/passwd"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s2, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn outbox_lists_floored_mtimes() {
        let st = test_state().await;
        let id = "s-ob";
        let wt = st.config.worktrees_root.join(id);
        let ob = wt.join("outbox");
        std::fs::create_dir_all(&ob).unwrap();
        std::fs::write(ob.join("report.md"), "x").unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let (s, b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/outbox"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        let files = b["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "outbox/report.md");
        assert_eq!(files[0]["name"], "report.md");
        assert!(files[0]["mtime"].is_i64());
    }

    // Regression: an agent that nests deliverables in subfolders of outbox/ (e.g. outbox/logo/icon.svg)
    // must still have every file surfaced. A flat, non-recursive scan listed nothing and the app showed
    // an empty outbox even though files were delivered.
    #[tokio::test]
    async fn outbox_lists_nested_files_recursively() {
        let st = test_state().await;
        let id = "s-ob-nested";
        let wt = st.config.worktrees_root.join(id);
        let ob = wt.join("outbox");
        std::fs::create_dir_all(ob.join("logo").join("sub")).unwrap();
        std::fs::write(ob.join("report.md"), "x").unwrap();
        std::fs::write(ob.join("logo").join("icon.svg"), "<svg/>").unwrap();
        std::fs::write(ob.join("logo").join("sub").join("deep.png"), "x").unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let (s, b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/outbox"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        let files = b["files"].as_array().unwrap();
        // sorted by path → nested entries first, then the top-level file.
        let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["outbox/logo/icon.svg", "outbox/logo/sub/deep.png", "outbox/report.md"]);
        // name is always the basename, even for a nested file.
        assert_eq!(files[0]["name"], "icon.svg");
    }

    #[tokio::test]
    async fn commits_route_404_on_unknown_session() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::get("/api/sessions/nope/commits")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(b["error"], "not found");
    }

    #[tokio::test]
    async fn commit_diff_route_requires_path() {
        let st = test_state().await;
        let id = "s-diff-nopath";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let (s, b) = oneshot_req(st.clone(),
            Request::get(format!("/api/sessions/{id}/commits/working/diff?repo=demo"))
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b, json!({"error":"path required"}));
    }

    #[tokio::test]
    async fn commit_diff_route_returns_hunks_for_working_change() {
        let st = test_state().await;
        let id = "s-diff-ok";
        let wt = seed_repo_with_working_change(&st, id, "# r\nadded line\n").await;
        let _ = wt;
        let (s, b) = oneshot_req(st.clone(),
            Request::get(format!("/api/sessions/{id}/commits/working/diff?repo=demo&path=README.md"))
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK, "body: {b}");
        let hunks = b["diff"]["hunks"].as_array().unwrap();
        assert!(!hunks.is_empty(), "expected a hunk, got {b}");
        let has_add = hunks.iter().flat_map(|h| h["lines"].as_array().unwrap())
            .any(|l| l["kind"] == "add" && l["content"] == "added line");
        assert!(has_add, "expected an added line in {b}");
    }

    /// Seed a session whose worktree has a `demo` git repo with one committed README and an
    /// (optional) uncommitted working change. Returns the worktree root path.
    async fn seed_repo_with_working_change(
        st: &crate::api::state::AppState, id: &str, working_readme: &str,
    ) -> std::path::PathBuf {
        use std::process::Command;
        let wt = st.config.worktrees_root.join(id);
        let repo_dir = wt.join("demo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        for args in [vec!["init","-q"], vec!["config","user.email","t@t"], vec!["config","user.name","t"]] {
            Command::new("git").args(&args).current_dir(&repo_dir).status().unwrap();
        }
        std::fs::write(repo_dir.join("README.md"), "# r\n").unwrap();
        Command::new("git").args(["add","."]).current_dir(&repo_dir).status().unwrap();
        Command::new("git").args(["commit","-q","-m","init"]).current_dir(&repo_dir).status().unwrap();
        if !working_readme.is_empty() {
            std::fs::write(repo_dir.join("README.md"), working_readme).unwrap();
        }
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        wt
    }

    #[tokio::test]
    async fn rewind_route_404_and_requires_turn_index() {
        let st = test_state().await;
        // 404 unknown session
        let (s, _b) = oneshot_req(st.clone(), Request::post("/api/sessions/nope/rewind")
            .header("authorization", auth(&st)).header("content-type","application/json")
            .body(Body::from("{\"turnIndex\":0}")).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // existing session, missing turnIndex → 400
        let id = "s-rw-noidx";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
        let (s2, b2) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/rewind"))
            .header("authorization", auth(&st)).header("content-type","application/json")
            .body(Body::from("{}")).unwrap()).await;
        assert_eq!(s2, StatusCode::BAD_REQUEST);
        assert_eq!(b2, json!({"error":"turnIndex required"}));
    }

    // Full engine rewind round-trip: snapshot a state, diverge, rewind back; turn-0 restores base;
    // a missing snapshot is a clean BadInput.
    #[tokio::test]
    async fn engine_rewind_restores_snapshot_and_base() {
        use std::process::Command;
        let st = test_state().await;
        let id = "s-rewind";
        let wt = st.config.worktrees_root.join(id);
        let repo_dir = wt.join("demo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        for args in [vec!["init","-q"], vec!["config","user.email","t@t"], vec!["config","user.name","t"]] {
            Command::new("git").args(&args).current_dir(&repo_dir).status().unwrap();
        }
        std::fs::write(repo_dir.join("README.md"), "base\n").unwrap();
        Command::new("git").args(["add","."]).current_dir(&repo_dir).status().unwrap();
        Command::new("git").args(["commit","-q","-m","init"]).current_dir(&repo_dir).status().unwrap();
        let base = String::from_utf8(Command::new("git").args(["rev-parse","HEAD"])
            .current_dir(&repo_dir).output().unwrap().stdout).unwrap().trim().to_string();

        let mut base_shas = std::collections::HashMap::new();
        base_shas.insert("demo".to_string(), Some(base.clone()));
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("agentic/x".into()),
            base_shas,
            ..Default::default()
        }).await.unwrap();
        // Make the session idle so live_session() accepts it.
        st.store.update(id, crate::engine::store::SessionPatch { status: Some("done".into()), ..Default::default() })
            .await.unwrap();

        // Snapshot the "turn 1" state, then diverge.
        std::fs::write(repo_dir.join("README.md"), "base\nturn1\n").unwrap();
        crate::engine::worktree::snapshot_worktree(&repo_dir, &format!("refs/agentic/snapshots/{id}/1")).unwrap();
        std::fs::write(repo_dir.join("README.md"), "base\nlater-garbage\n").unwrap();
        std::fs::write(repo_dir.join("new.txt"), "created-after\n").unwrap();

        // Rewind to turn 1: README restored, new.txt kept.
        st.engine.rewind(id, 1).await.unwrap();
        assert_eq!(std::fs::read_to_string(repo_dir.join("README.md")).unwrap(), "base\nturn1\n");
        assert!(repo_dir.join("new.txt").exists(), "files created after the snapshot must be kept");

        // Rewind to turn 0: README back to base content.
        st.engine.rewind(id, 0).await.unwrap();
        assert_eq!(std::fs::read_to_string(repo_dir.join("README.md")).unwrap(), "base\n");

        // A turn with no snapshot → BadInput.
        let err = st.engine.rewind(id, 99).await.unwrap_err();
        assert!(matches!(err, crate::engine::EngineError::BadInput(_)), "missing snapshot must be BadInput");
    }

    // ── Upload size tests ───────────────────────────────────────────────────────

    /// Build a minimal multipart body for a single file field named "file".
    fn multipart_body(filename: &str, data: Vec<u8>) -> (Vec<u8>, String) {
        let boundary = "testboundary1234567890";
        let mut body = Vec::new();
        let header = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        );
        body.extend_from_slice(header.as_bytes());
        body.extend_from_slice(&data);
        let footer = format!("\r\n--{boundary}--\r\n");
        body.extend_from_slice(footer.as_bytes());
        let content_type = format!("multipart/form-data; boundary={boundary}");
        (body, content_type)
    }

    async fn seed_session_with_worktree(st: &crate::api::state::AppState, id: &str) {
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
            ..Default::default()
        }).await.unwrap();
    }

    /// A payload between 2 MB and upload_max_bytes (default 64 MB) must return 200 + path.
    /// Before the fix, axum's 2 MB default body limit caused field.bytes() to Err → 400 "no file".
    #[tokio::test]
    async fn upload_between_2mb_and_max_returns_200() {
        let mut st = test_state().await;
        // Set upload_max_bytes to 8 MB so the 3 MB payload is within bounds.
        let mut c = (*st.config).clone();
        c.upload_max_bytes = 8 * 1024 * 1024;
        st.config = std::sync::Arc::new(c);

        let id = "s-up-ok";
        seed_session_with_worktree(&st, id).await;

        // 3 MB payload — above axum's old 2 MB default, but below upload_max_bytes.
        let payload = vec![0u8; 3 * 1024 * 1024];
        let (body_bytes, ct) = multipart_body("image.bin", payload);
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/upload"))
            .header("authorization", auth(&st))
            .header("content-type", ct)
            .body(Body::from(body_bytes)).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "3 MB upload within max must succeed, got: {:?}", body);
        assert!(body["path"].as_str().unwrap_or("").starts_with("uploads/"), "expected uploads/ path");
    }

    /// A payload above upload_max_bytes must return 413 (not 400).
    #[tokio::test]
    async fn upload_above_max_returns_413() {
        let mut st = test_state().await;
        // Set upload_max_bytes to 1024 bytes so a 2 KB payload is over the limit.
        let mut c = (*st.config).clone();
        c.upload_max_bytes = 1024;
        st.config = std::sync::Arc::new(c);

        let id = "s-up-big";
        seed_session_with_worktree(&st, id).await;

        let payload = vec![0u8; 2 * 1024]; // 2 KB > upload_max_bytes of 1 KB
        let (body_bytes, ct) = multipart_body("big.bin", payload);
        let (status, _body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/upload"))
            .header("authorization", auth(&st))
            .header("content-type", ct)
            .body(Body::from(body_bytes)).unwrap()).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "oversize upload must return 413");
    }

    #[tokio::test]
    async fn fork_unknown_session_is_404() {
        let st = test_state().await;
        let (status, _body) = oneshot_req(st.clone(), Request::post("/api/sessions/does-not-exist/fork")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Source session exists, but its worktree dir is missing on disk. The engine surfaces this
    /// as `SourceUnhealthy`, which the API layer maps to 409 Conflict (was 500 Internal before
    /// the fix — conflating "broken git state" with a server bug).
    #[tokio::test]
    async fn fork_missing_source_worktree_is_409() {
        let st = test_state().await;
        // Seed a real local git repo (just an empty .git dir is enough for ensure_local to skip
        // the clone call), then a session that *claims* to have that repo, but with a
        // worktree_path pointing at a directory that does not exist on disk.
        let id = "s-fork-bad";
        std::fs::create_dir_all(st.config.src_root.join("demo").join(".git")).unwrap();
        // Intentionally do NOT create worktrees_root/<id>/demo. fork_session's
        // `if !wt.exists()` branch must fire.
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec!["demo".into()], skills: vec![], prompt: "hi".into(),
            worktree_path: Some(st.config.worktrees_root.join(id).to_string_lossy().into_owned()),
            branch: Some("agentic/x".into()),
            ..Default::default()
        }).await.unwrap();
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/fork"))
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
        // Body is the engine's own error string — preserves the wire contract for the Android client.
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("source worktree missing"),
            "expected 'source worktree missing' in body, got: {msg}");
    }

    #[tokio::test]
    async fn fork_returns_201_with_new_session_id_and_parent_link() {
        let st = test_state().await;
        // Create a no-repos session: the test fixture disables clone_fn, so any repos
        // entry would require a real git repo on disk. fork_session's no-repos branch
        // exercises the same route + engine path without that constraint.
        let id = "s-fork";
        let wt = st.config.worktrees_root.join(id);
        std::fs::create_dir_all(&wt).unwrap();
        st.store.create(crate::engine::store::CreateInput {
            id: id.into(), repos: vec![], skills: vec![], prompt: "hi".into(),
            worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("agentic/x".into()),
            ..Default::default()
        }).await.unwrap();

        // Fork it.
        let (status, body) = oneshot_req(st.clone(), Request::post(format!("/api/sessions/{id}/fork"))
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap()).await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let new_id = body["id"].as_str().unwrap();
        assert_ne!(new_id, id);
        let parent = body["session"]["parentSessionId"].as_str();
        assert_eq!(parent, Some(id));
        // New session is idle (not auto-spawned): status == "done".
        assert_eq!(body["session"]["status"].as_str(), Some("done"));
    }

    // ── Task 8: adopt / detach / adoptable HTTP surface ──────────────
    //
    // NOTE on the harness: the shared `test_state` helper wires the engine's
    // `claude_config_base` to the real default (`~/.claude`), with NO per-test
    // override hook — so the full adopt round-trip (which must read a native
    // transcript at `<config_base>/projects/<slug(cwd)>/<csid>.jsonl`) can't seed
    // into a temp dir through it. `adopt_state_with_config_base` rebuilds the
    // engine over the SAME store, overriding only `claude_config_base` to a temp
    // dir, so we CAN run a full 201 + origin:"adopted" HTTP test. The error/edge
    // paths (bad body, unknown session, missing transcript) don't need a seeded
    // transcript and run against plain `test_state`.

    use std::sync::atomic::{AtomicU64, Ordering as AO};
    static ADOPT_CTR: AtomicU64 = AtomicU64::new(0);

    /// A `test_state` whose engine reads native transcripts from `cb` (a temp dir)
    /// instead of the real `~/.claude`. Rebuilds the engine over the existing store
    /// (so `GET /api/sessions` still sees rows the adopt writes), overriding only
    /// `claude_config_base`. Mirrors `test_support::test_state`'s engine wiring.
    async fn adopt_state_with_config_base(cb: std::path::PathBuf) -> crate::api::state::AppState {
        use std::sync::Arc;
        let mut st = test_state().await;
        let c = (*st.config).clone();
        let engine_cfg = crate::engine::EngineConfig {
            src_root: c.src_root.clone(),
            worktrees_root: c.worktrees_root.clone(),
            log_dir: c.log_dir.clone(),
            db_path: c.db_path.clone(),
            title_generator: Arc::new(crate::api::test_support::NoopTitleGenerator),
            retitle_enabled: c.retitle_enabled,
            max_concurrent: Some(2),
            git_org: c.git_org.clone(),
            claude_config_base: cb,
            clone_fn: Some(Arc::new(|_u: &str, _d: &str| Err(std::io::Error::other("clone disabled in tests")))),
            sync_fn: None,
            runner: Some(Arc::new(crate::engine::sdk_runner::SdkRunner::with_node(
                "bash",
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake-sdk-bridge-ok.sh")
                    .to_string_lossy()
                    .into_owned(),
            ))),
            log_fn: None, now_fn: None, push_fn: None, usage_fn: None,
            idle_max_ms: None, wall_max_ms: None, idle_ttl_ms: None,
            memory_max: None, memory_high: None, cpu_quota: None, tasks_max: None,
        };
        st.engine = Arc::new(crate::engine::Engine::with_store(
            engine_cfg, st.store.clone(), Some(st.transcript.clone()),
        ));
        st
    }

    #[tokio::test]
    async fn adopt_route_creates_session_with_adopted_origin() {
        let n = ADOPT_CTR.fetch_add(1, AO::SeqCst);
        let cb = std::env::temp_dir().join(format!("agentic-adopt-http-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cb);
        let st = adopt_state_with_config_base(cb.clone()).await;

        // Seed a native transcript (#2) at the slug path the engine computes for this cwd.
        let cwd = format!("/tmp/adopt-http-proj-{n}");
        let csid = "csidHTTP";
        let tp = crate::engine::native_transcript::transcript_path(&cb, &cwd, csid);
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(&tp, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hey\"}}\n").unwrap();

        // Adopt → 201 { id }.
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions/adopt")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"claudeSessionId":"{csid}","cwd":"{cwd}"}}"#))).unwrap()).await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let id = body["id"].as_str().unwrap().to_string();

        // The created session serializes origin:"adopted" on the list route.
        let (status, list) = oneshot_req(st.clone(), Request::get("/api/sessions")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(list["sessions"].as_array().unwrap().iter()
            .any(|s| s["id"] == json!(id) && s["origin"] == json!("adopted")),
            "adopted session must appear with origin:adopted, got {list}");

        // /api/adoptable now EXCLUDES the just-adopted csid (its csid is a known linked id).
        let (status, adoptable) = oneshot_req(st.clone(), Request::get("/api/adoptable")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(adoptable.as_array().unwrap().iter().all(|a| a["sessionId"] != json!(csid)),
            "adopted csid must not remain adoptable, got {adoptable}");

        let _ = std::fs::remove_dir_all(&cb);
    }

    #[tokio::test]
    async fn adoptable_route_lists_seeded_transcript() {
        let n = ADOPT_CTR.fetch_add(1, AO::SeqCst);
        let cb = std::env::temp_dir().join(format!("agentic-adoptable-http-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cb);
        let st = adopt_state_with_config_base(cb.clone()).await;

        let cwd = format!("/tmp/adoptable-http-proj-{n}");
        let csid = "csidLIST";
        let tp = crate::engine::native_transcript::transcript_path(&cb, &cwd, csid);
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(&tp, "{\"type\":\"user\",\"cwd\":\"x\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();

        let (status, body) = oneshot_req(st.clone(), Request::get("/api/adoptable")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.as_array().unwrap().iter().any(|a| a["sessionId"] == json!(csid)),
            "seeded transcript must be listed as adoptable, got {body}");

        let _ = std::fs::remove_dir_all(&cb);
    }

    #[tokio::test]
    async fn adopt_route_missing_transcript_is_400() {
        // No seeded transcript → engine rejects the adopt → 400 with an error body.
        // Also proves the route is registered (a 404 here would mean it isn't).
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions/adopt")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from(r#"{"claudeSessionId":"nope","cwd":"/tmp/does-not-exist-xyz"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string(), "error body expected, got {body}");
    }

    #[tokio::test]
    async fn adopt_route_malformed_body_is_400() {
        let st = test_state().await;
        let (status, _body) = oneshot_req(st.clone(), Request::post("/api/sessions/adopt")
            .header("authorization", auth(&st)).header("content-type", "application/json")
            .body(Body::from("{not json")).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_session_rejects_extra_mcp_with_no_transport() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"bad-mcp"}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("must have either command"), "expected transport error, got: {msg}");
    }

    #[tokio::test]
    async fn create_session_rejects_extra_mcp_with_empty_name() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"","command":"npx"}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("name must be non-empty"), "expected name error, got: {msg}");
    }

    #[tokio::test]
    async fn create_session_rejects_extra_mcp_with_both_transports() {
        let st = test_state().await;
        let (status, _body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"bad","command":"npx","url":"https://x.com"}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn detach_route_unknown_session_is_400() {
        // Route is registered (not 404); engine errors on an unknown id → 400.
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions/nope/detach")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string(), "error body expected, got {body}");
    }

    #[tokio::test]
    async fn create_session_accepts_valid_extra_mcp_servers() {
        let st = test_state().await;
        let (status, _body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"stdio-mcp","command":"npx","args":["server"]},{"name":"http-mcp","url":"https://example.com/mcp","type":"http"}],"hiddenMcpServers":["some-mcp"]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }

    // Fix 1: reserved name "agentic" must be rejected.
    #[tokio::test]
    async fn create_session_rejects_reserved_agentic_name() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"agentic","command":"npx"}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("reserved"), "expected reserved-name error, got: {msg}");
    }

    // Fix 2: empty command string must be rejected (stdio transport).
    #[tokio::test]
    async fn create_session_rejects_empty_command() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"my-mcp","command":""}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("command must be non-empty"), "expected empty-command error, got: {msg}");
    }

    // Fix 2: whitespace-only command string must be rejected (stdio transport).
    #[tokio::test]
    async fn create_session_rejects_whitespace_command() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"my-mcp","command":"   "}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("command must be non-empty"), "expected whitespace-command error, got: {msg}");
    }

    // Fix 2: empty url string must be rejected (http/sse transport).
    #[tokio::test]
    async fn create_session_rejects_empty_url() {
        let st = test_state().await;
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","extraMcpServers":[{"name":"my-mcp","url":""}]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("url must be non-empty"), "expected empty-url error, got: {msg}");
    }

    #[test]
    fn validate_disjoint_catches_conflict() {
        let err = validate_disjoint(
            &["gh@m".to_string(), "sp@m".to_string()],
            &["gh@m".to_string()],
            "plugins",
        );
        assert!(err.is_some());
        let msg = err.unwrap();
        assert!(msg.contains("gh@m"), "error must name the conflicting id: {msg}");
    }

    #[test]
    fn validate_disjoint_allows_non_overlapping_lists() {
        let err = validate_disjoint(
            &["gh@m".to_string()],
            &["sp@m".to_string()],
            "plugins",
        );
        assert!(err.is_none());
    }

    #[test]
    fn validate_disjoint_empty_lists_ok() {
        assert!(validate_disjoint(&[], &[], "skills").is_none());
        assert!(validate_disjoint(&["a".to_string()], &[], "mcp").is_none());
        assert!(validate_disjoint(&[], &["a".to_string()], "plugins").is_none());
    }

    #[tokio::test]
    async fn create_session_rejects_same_id_in_hidden_and_forced_on() {
        let st = test_state().await;
        // Plugin conflict
        let (status, body) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","hiddenPlugins":["gh@m"],"forcedOnPlugins":["gh@m"]}"#))
            .unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("gh@m"), "error must name conflicting id: {msg}");
        // Skill conflict
        let (status2, body2) = oneshot_req(st.clone(), Request::post("/api/sessions")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"test","hiddenSkills":["rke2-ops"],"forcedOnSkills":["rke2-ops"]}"#))
            .unwrap()).await;
        assert_eq!(status2, StatusCode::BAD_REQUEST, "body: {body2}");
        let msg2 = body2["error"].as_str().unwrap_or("");
        assert!(msg2.contains("rke2-ops"), "error must name conflicting id: {msg2}");
    }
}
