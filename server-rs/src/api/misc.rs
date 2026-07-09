use axum::{extract::State, http::{StatusCode, HeaderMap}, response::{IntoResponse, Response}, Json};
use serde_json::json;
use crate::api::state::AppState;
use crate::util::now_ms;

const USAGE_FRESH_MS: i64 = 60_000;
const USAGE_STALE_MAX_MS: i64 = 10 * 60_000;

pub async fn usage_route(State(st): State<AppState>) -> Response {
    let now = now_ms();
    // Fast path: fresh cache.
    {
        let c = st.usage_cache.lock();
        if let Some(ref data) = c.data {
            if now - c.at < USAGE_FRESH_MS { return Json(data.clone()).into_response(); }
        }
    }
    // Coalesce concurrent misses behind the inflight async lock (single-flight).
    // This ensures exactly ONE upstream fetch fires per burst, even on failures —
    // single-flight: exactly one upstream fetch fires per burst, coalescing concurrent waiters.
    let _guard = st.usage_inflight.lock().await;
    // Re-check after acquiring the lock: another waiter may have just refreshed the cache,
    // OR may have just attempted a fetch (even a failed one). In the failure case we check
    // last_attempt_at so that waiters queued behind a failed fetch don't each fire their
    // own upstream call — they short-circuit to the stale/503 path immediately.
    {
        let c = st.usage_cache.lock();
        let t = now_ms();
        if let Some(ref data) = c.data {
            if t - c.at < USAGE_FRESH_MS { return Json(data.clone()).into_response(); }
        }
        // If the holder just attempted (and failed) within the fresh window, serve stale or 503
        // rather than re-firing the upstream call.
        if c.last_attempt_at > 0 && t - c.last_attempt_at < USAGE_FRESH_MS {
            if let Some(ref data) = c.data {
                if t - c.at < USAGE_STALE_MAX_MS {
                    let mut headers = HeaderMap::new();
                    headers.insert("x-usage-stale", "1".parse().unwrap());
                    return (headers, Json(data.clone())).into_response();
                }
            }
            // Return the real upstream error from the holder's failed fetch,
            // falling back to a generic message before any attempt has failed.
            let err = c.last_error.clone().unwrap_or_else(|| "usage fetch failed".to_string());
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": err}))).into_response();
        }
    }
    // Record attempt time BEFORE firing so late-arriving waiters (who acquire after us) skip.
    { let mut c = st.usage_cache.lock(); c.last_attempt_at = now_ms(); }
    let base = st.config.claude_config_base.clone();
    let fetched = match &st.usage_fn {
        Some(f) => f().await,
        None => crate::engine::usage::fetch_usage(&base, None).await,
    };
    match fetched {
        Ok(data) => {
            { let mut c = st.usage_cache.lock(); c.at = now_ms(); c.data = Some(data.clone()); c.last_error = None; }
            Json(data).into_response()
        }
        Err(e) => {
            // Transient failure: record the error (so single-flight losers can surface it) and
            // serve last-good while not too stale, with x-usage-stale: 1.
            let mut c = st.usage_cache.lock();
            c.last_error = Some(e.to_string());
            if let Some(ref data) = c.data {
                if now - c.at < USAGE_STALE_MAX_MS {
                    let mut headers = HeaderMap::new();
                    headers.insert("x-usage-stale", "1".parse().unwrap());
                    return (headers, Json(data.clone())).into_response();
                }
            }
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))).into_response()
        }
    }
}

pub async fn repos_route(State(st): State<AppState>) -> Json<serde_json::Value> {
    let local = crate::engine::repos::list_repos(&st.config.src_root);
    let remote = crate::engine::repos::list_remote_repos(&st.config.git_org, None);
    Json(json!({ "local": local, "remote": remote }))
}

pub async fn skills_route(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::skills::list_skills(&st.config.skills_dir))
}

/// GET /api/plugins — installed Claude Code plugins (`<plugin>@<marketplace>` ids), read from
/// `<claude_config_base>/plugins/installed_plugins.json`. Sessions share that config dir, so
/// this is exactly the plugin set a new session would load. Same shape philosophy as
/// [skills_route]: a plain JSON array of `{name}` objects.
pub async fn plugins_route(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::plugins::list_plugins(&st.config.claude_config_base))
}

/// GET /api/global-settings — unified skill+plugin components with their global on/off state.
pub async fn global_settings_route(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::components::list_components(
        &st.config.claude_config_base,
        &st.config.skills_dir,
    ))
}

#[derive(serde::Deserialize)]
pub struct ToggleReq {
    pub kind: String,
    pub id: String,
    pub enabled: bool,
}

/// POST /api/global-settings/toggle — flip one component globally (writes settings.local.json).
pub async fn global_settings_toggle_route(
    State(st): State<AppState>,
    Json(req): Json<ToggleReq>,
) -> Response {
    let base = &st.config.claude_config_base;

    // Validate kind first; then confirm the id is an installed/enumerable component of that kind.
    // Globally-disabled components are still enumerated by list_components, so toggling them back
    // on must succeed — we only reject ids that are not installed at all.
    match req.kind.as_str() {
        "plugin" | "skill" => {
            let components = crate::engine::components::list_components(base, &st.config.skills_dir);
            let known = components.iter().any(|c| c.kind == req.kind && c.id == req.id);
            if !known {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("unknown {} id: {}", req.kind, req.id)}))).into_response();
            }
        }
        other => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("unknown kind: {other}")}))).into_response();
        }
    }

    let res = match req.kind.as_str() {
        "plugin" => crate::engine::global_settings::set_plugin_enabled(base, &req.id, req.enabled),
        "skill" => crate::engine::global_settings::set_skill_enabled(base, &req.id, req.enabled),
        // Unreachable: the match above already validated kind.
        _ => unreachable!(),
    };
    match res {
        Ok(()) => Json(crate::engine::components::list_components(base, &st.config.skills_dir)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ── DB-backed groups CRUD (replaces the old file-based groups_get / groups_put) ──

pub async fn groups_list(State(st): State<AppState>) -> Response {
    match st.store.list_groups().await {
        Ok(groups) => Json(json!({ "groups": groups })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct CreateGroupBody {
    pub name: Option<String>,
    pub icon: Option<String>,
}

pub async fn groups_create(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: CreateGroupBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let Some(name) = b.name.filter(|n| !n.trim().is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"name required"}))).into_response();
    };
    match st.store.create_group(name.trim(), b.icon.as_deref()).await {
        Ok(group) => Json(json!({ "group": group })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct UpdateGroupBody {
    pub name: Option<String>,
    pub icon: Option<String>,
}

pub async fn groups_update(State(st): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes) -> Response {
    let b: UpdateGroupBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    match st.store.update_group(&id, b.name.as_deref(), b.icon.as_deref()).await {
        Ok(Some(group)) => Json(json!({ "group": group })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error":"group not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn groups_delete(State(st): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    match st.store.delete_group(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn templates_get(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::templates::list_templates(&st.config.templates_path))
}

pub async fn templates_put(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if !v.is_array() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"array of templates required"}))).into_response();
    }
    match crate::engine::templates::save_templates(&st.config.templates_path, &v) {
        Ok(t) => Json(t).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct TemplateStartBody {
    pub name: Option<String>,
    pub vars: Option<std::collections::HashMap<String, String>>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
}

pub async fn templates_start(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: TemplateStartBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let Some(name) = b.name.filter(|n| !n.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"name required"}))).into_response();
    };
    let templates = crate::engine::templates::list_templates(&st.config.templates_path);
    let Some(tpl) = templates.into_iter().find(|t| t.name == name) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("template '{name}' not found")}))).into_response();
    };
    let prompt = crate::engine::templates::resolve_prompt(&tpl.prompt_body, &b.vars.unwrap_or_default());
    let meta = crate::engine::SubmitMeta {
        model: b.model.or(tpl.model),
        effort: b.effort.or(tpl.effort),
        mode: b.mode.or(tpl.mode),
        permission_mode: None, // templates don't carry a permission_mode (yet)
        hidden_skills: Vec::new(), // templates don't carry a skill blacklist (yet)
        hidden_plugins: Vec::new(), // templates don't carry a plugin blacklist (yet)
        hidden_mcp_servers: Vec::new(), // templates don't carry an MCP blacklist (yet)
        extra_mcp_servers: Vec::new(), // templates don't carry extra MCP servers (yet)
        claude_md: None, // templates don't carry session-scoped CLAUDE.md (yet)
        staged_uploads: Vec::new(), // templates don't carry pre-session attachments
        forced_on_plugins: Vec::new(), // templates don't carry forced-on overrides (yet)
        forced_on_skills: Vec::new(),
        forced_on_mcp_servers: Vec::new(),
    };
    match st.engine.submit_session(tpl.repos, tpl.skills, prompt, std::collections::HashMap::new(), meta).await {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct DeviceBody { pub token: Option<String> }

pub async fn devices_post(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: DeviceBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let token = b.token.unwrap_or_default();
    let token = token.trim().to_string();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"token required"}))).into_response();
    }
    match crate::engine::push::save_device_token(&st.config.device_token_path, &token) {
        Ok(rec) => Json(json!({ "ok": true, "registeredAt": rec.registered_at })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ── provider registry CRUD (BYOK: keys live server-side; GET masks them) ──

#[derive(serde::Serialize)]
struct ProviderView {
    name: String,
    base_url: String,
    model: String,
    protocol: String,
    capability: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    priority: f32,
    cost: f32,
    router: bool,
    has_key: bool,
}

fn provider_view(p: &crate::engine::providers::Provider) -> ProviderView {
    ProviderView {
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        model: p.model.clone(),
        protocol: format!("{:?}", p.protocol).to_lowercase(),
        capability: p.capability,
        description: p.description.clone(),
        priority: p.priority,
        cost: p.cost,
        router: p.router,
        has_key: !p.resolved_key().is_empty(),
    }
}

/// GET /api/providers — list registered providers, keys masked (api_key is never returned).
pub async fn providers_get() -> Response {
    let views: Vec<ProviderView> =
        crate::engine::providers::load_list().iter().map(provider_view).collect();
    Json(json!({ "providers": views })).into_response()
}

/// POST /api/providers — add or replace a provider by name. Body (snake_case):
/// {name, base_url, api_key | api_key_env, model, protocol?, capability?, description?, priority?, router?}.
/// The key is write-only: on an EDIT (a name that already exists) a blank `api_key` means "keep the
/// stored key" — `upsert` preserves the existing credential rather than wiping it.
pub async fn providers_post(body: axum::body::Bytes) -> Response {
    let mut p: crate::engine::providers::Provider = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid provider: {e}")}))).into_response(),
    };
    // Trim before saving: routing/deletion use exact (case-insensitive) name equality, so a stored
    // " minimax " could never be matched/deleted by "minimax".
    p.name = p.name.trim().to_string();
    p.base_url = p.base_url.trim().to_string();
    p.model = p.model.trim().to_string();
    // Capability is the routing axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.capability.is_nan() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"capability cannot be NaN"}))).into_response();
    }
    p.capability = p.capability.clamp(0.0, 1.0);
    // Priority is the routing-preference axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.priority.is_nan() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"priority cannot be NaN"}))).into_response();
    }
    p.priority = p.priority.clamp(0.0, 1.0);
    // Cost is the routing-cost axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.cost.is_nan() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cost cannot be NaN"}))).into_response();
    }
    p.cost = p.cost.clamp(0.0, 1.0);
    if p.name.is_empty() || p.base_url.is_empty() || p.model.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"name, base_url, model are required"}))).into_response();
    }
    // Validate the ROUTER relationship at set time: a provider flagged as the router that can't produce
    // a usable routing reply would silently make every later delegate fan-out fall back to native
    // Claude. Probe it now and reject the save with a clear reason instead. Only runs when router=true,
    // so ordinary provider saves are unaffected. Use the EFFECTIVE key — a blank incoming api_key on an
    // edit means "keep the stored key" (mirrors upsert), so resolve it from the existing record first.
    if p.router {
        let mut probe = p.clone();
        if probe.api_key.is_empty() && probe.api_key_env.is_none() {
            if let Some(existing) = crate::engine::providers::load_list()
                .into_iter()
                .find(|x| x.name.eq_ignore_ascii_case(&p.name))
            {
                probe.api_key = existing.api_key;
                probe.api_key_env = existing.api_key_env;
            }
        }
        if let Err(e) = crate::engine::router::validate_router(&probe, None).await {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("router validation failed: {e}")}))).into_response();
        }
    }
    match crate::engine::providers::upsert(p) {
        Ok(()) => {
            // An openai provider may have been added/changed → regenerate the LiteLLM config + restart.
            crate::engine::litellm::request_reload();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// DELETE /api/providers/{name} — remove a provider.
pub async fn providers_delete(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match crate::engine::providers::remove(&name) {
        Ok(true) => {
            crate::engine::litellm::request_reload();
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error":"no such provider"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ── model catalog (native Claude tiers + registered BYOK providers) ──

#[derive(serde::Serialize)]
struct ModelEntry {
    key: String,        // "claude-opus-4-8"
    label: String,      // "Opus 4.8"
    native: bool,       // true for subscription tiers
    default: bool,      // true for the strongest native Claude
    capability: f32,    // 0..1, for ordering
    cost: f32,          // 0..1 (lower = cheaper), for the UI cost indicator
}

#[derive(serde::Deserialize)]
pub struct ModelsQuery {
    scope: Option<String>,
}

fn native_model_entries() -> Vec<ModelEntry> {
    use crate::engine::providers::{family_metrics, native_claude_models};

    let models = native_claude_models();
    // Default = the newest Opus-family model (the subscription's daily driver), else the first
    // discovered model. The list is in API order (newest first), so `find` picks the newest.
    let default_key = models
        .iter()
        .find(|m| m.id.contains("opus"))
        .or_else(|| models.first())
        .map(|m| m.id.clone())
        .unwrap_or_default();
    let mut entries: Vec<ModelEntry> = models
        .iter()
        .map(|m| {
            let (capability, cost) = family_metrics(&m.id);
            ModelEntry {
                key: m.id.clone(),
                label: m.display_name.clone(),
                native: true,
                default: m.id == default_key,
                capability,
                cost,
            }
        })
        .collect();
    // Cheap → capable for the slider, id as a deterministic tiebreaker. For the current
    // single-generation id shapes this reads oldest → newest within a family (…Sonnet 4.5, 4.6, 5,
    // Opus 4.5…4.8, Fable 5), so the whole slider is monotonically "stronger going right".
    // Cross-generation legacy ids (claude-3-5-* vs claude-3-*) or a two-digit minor ("…-4-10")
    // would read out of order — cosmetic only: same capability tier, key-based selection unaffected.
    entries.sort_by(|a, b| a.capability.total_cmp(&b.capability).then_with(|| a.key.cmp(&b.key)));
    entries
}

fn full_model_entries() -> Vec<ModelEntry> {
    let mut entries = native_model_entries();
    for p in &crate::engine::providers::load_list() {
        entries.push(ModelEntry {
            key: p.name.clone(),
            label: format!("{} ({})", p.model, p.name),
            native: false,
            default: false,
            capability: p.capability,
            cost: p.cost,
        });
    }
    // Same ordering contract as native_model_entries: cheap → capable, id as the deterministic
    // tiebreaker (within a Claude family that reads oldest → newest).
    entries.sort_by(|a, b| a.capability.total_cmp(&b.capability).then_with(|| a.key.cmp(&b.key)));
    entries
}

/// GET /api/models — model catalog.
/// - no scope: native Claude tiers + registered BYOK providers
/// - scope=session_start: native Claude tiers only, for main-thread model pickers
pub async fn models_get(axum::extract::Query(q): axum::extract::Query<ModelsQuery>) -> Response {
    let entries = match q.scope.as_deref() {
        None | Some("") => full_model_entries(),
        Some("session_start") => native_model_entries(),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid models scope: {other}") })),
            )
                .into_response();
        }
    };

    Json(json!({ "models": entries })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{test_state, auth, oneshot_req};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use parking_lot::Mutex;

    #[test]
    fn provider_view_masks_the_key() {
        let p = crate::engine::providers::Provider {
            name: "minimax".into(), base_url: "https://x".into(), api_key: "SECRET".into(),
            api_key_env: None, model: "MiniMax-M3".into(),
            protocol: crate::engine::providers::Protocol::Anthropic, capability: 0.5,
            description: None, priority: 0.5, cost: 0.3, router: false,
        };
        let json = serde_json::to_string(&provider_view(&p)).unwrap();
        assert!(!json.contains("SECRET"), "key must never appear in the view: {json}");
        assert!(!json.contains("api_key"));
        assert!(json.contains("\"has_key\":true"));
        let p2 = crate::engine::providers::Provider { api_key: String::new(), ..p };
        assert!(!provider_view(&p2).has_key);
    }

    /// Points `providers_file_path()` at a fresh temp file for the duration of a test, so tests
    /// exercising the BYOK registry NEVER touch the real `~/.agentic-dev/providers.json` (a
    /// non-empty upsert there would overwrite the user's stored API keys). Uses the data-race-free
    /// PROVIDERS_FILE_OVERRIDE static instead of `env::set_var` — setenv racing a concurrent getenv
    /// from any other test thread is UB in glibc (the "process-global set_var race" flake). The
    /// override is process-global, so a lock still serializes the tests that use it; Drop clears it.
    struct ProvidersFileGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for ProvidersFileGuard {
        fn drop(&mut self) {
            *crate::engine::providers::PROVIDERS_FILE_OVERRIDE.lock() = None;
        }
    }
    fn isolated_providers_file() -> ProvidersFileGuard {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        *crate::engine::providers::PROVIDERS_FILE_OVERRIDE.lock() = Some(dir.path().join("providers.json"));
        ProvidersFileGuard { _dir: dir, _lock: lock }
    }

    #[tokio::test]
    async fn models_get_default_scope_includes_registered_providers() {
        let _providers = isolated_providers_file();
        let st = test_state().await;
        crate::engine::providers::seed_claude_models_for_tests();

        crate::engine::providers::upsert_at(
            &crate::engine::providers::providers_file_path(),
            crate::engine::providers::Provider {
                name: "minimax".into(),
                base_url: "https://api.minimaxi.com/anthropic".into(),
                api_key: "SECRET".into(),
                api_key_env: None,
                model: "MiniMax-M3".into(),
                protocol: crate::engine::providers::Protocol::Anthropic,
                capability: 0.5,
                description: None,
                priority: 0.5,
                cost: 0.3,
                router: false,
            },
        )
        .unwrap();

        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/models")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(s, StatusCode::OK);
        let models = b["models"].as_array().unwrap();
        assert!(models.iter().any(|m| m["key"] == "minimax"));
        assert!(models.iter().any(|m| m["native"] == false));
    }

    #[tokio::test]
    async fn models_get_session_start_scope_excludes_registered_providers() {
        let _providers = isolated_providers_file();
        let st = test_state().await;
        crate::engine::providers::seed_claude_models_for_tests();

        crate::engine::providers::upsert_at(
            &crate::engine::providers::providers_file_path(),
            crate::engine::providers::Provider {
                name: "deepseek".into(),
                base_url: "https://api.deepseek.com/anthropic".into(),
                api_key: "SECRET".into(),
                api_key_env: None,
                model: "deepseek-chat".into(),
                protocol: crate::engine::providers::Protocol::Anthropic,
                capability: 0.6,
                description: None,
                priority: 0.5,
                cost: 0.5,
                router: false,
            },
        )
        .unwrap();

        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/models?scope=session_start")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(s, StatusCode::OK);
        let models = b["models"].as_array().unwrap();
        assert!(!models.is_empty(), "native Claude candidates should be present");
        assert!(!models.iter().any(|m| m["key"] == "deepseek"));
        assert!(models.iter().all(|m| m["native"] == true));
        // Slider reads monotonically "stronger going right": families cheap → capable, and WITHIN
        // a family oldest → newest (opus 4.7 before 4.8). Seed: fable-5, opus-4-8, opus-4-7,
        // sonnet-4-6, haiku-4-5-20251001.
        let keys: Vec<&str> = models.iter().map(|m| m["key"].as_str().unwrap()).collect();
        assert_eq!(
            keys,
            vec![
                "claude-haiku-4-5-20251001",
                "claude-sonnet-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-fable-5",
            ]
        );
        // Default = the NEWEST opus, even though it's not the last entry.
        let default_key = models.iter().find(|m| m["default"] == true).unwrap()["key"].as_str().unwrap();
        assert_eq!(default_key, "claude-opus-4-8");
    }

    #[tokio::test]
    async fn models_get_rejects_unknown_scope() {
        let st = test_state().await;

        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/models?scope=delegate")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("scope"));
    }

    #[tokio::test]
    async fn providers_post_rejects_missing_model() {
        // Validation fires BEFORE the upsert, so a 400 here never touches the providers file.
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/providers")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"x","base_url":"https://x/anthropic","model":"","api_key":"k"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("required"));
    }

    #[tokio::test]
    async fn providers_post_rejects_unvalidatable_router() {
        // router=true on an openai-protocol provider can never run the (anthropic) routing call, so the
        // set-time validation rejects it BEFORE the upsert — the providers file is never written. This
        // path is network-free: validate_router checks protocol before making any HTTP call.
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/providers")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"rtr","base_url":"https://x/anthropic","model":"m","api_key":"k","protocol":"openai","router":true}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("router validation failed"));
    }

    #[tokio::test]
    async fn usage_serves_fetch_then_caches() {
        let mut st = test_state().await;
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        st.usage_fn = Some(Arc::new(move || {
            let c = c.clone();
            Box::pin(async move { *c.lock() += 1;
                Ok(serde_json::json!({"five_hour":{"utilization":12,"resets_at":"x"}})) })
        }));
        for _ in 0..2 {
            let (s, b) = oneshot_req(st.clone(), Request::get("/api/usage")
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(b["five_hour"]["utilization"], 12);
        }
        // Within the 60s freshness window the second call is served from cache → fetch ran once.
        assert_eq!(*calls.lock(), 1);
    }

    #[tokio::test]
    async fn usage_failure_with_no_cache_is_503() {
        let mut st = test_state().await;
        st.usage_fn = Some(Arc::new(|| Box::pin(async {
            Err(crate::engine::usage::UsageError::Status(429)) })));
        let (s, b) = oneshot_req(st.clone(), Request::get("/api/usage")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(b["error"], "usage endpoint 429");
    }

    /// 5 concurrent /api/usage calls where the usage_fn always errors should
    /// result in exactly ONE upstream call (single-flight coalescing on failure path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn usage_single_flight_coalesces_on_failure() {
        let mut st = test_state().await;
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        st.usage_fn = Some(Arc::new(move || {
            let c = c.clone();
            Box::pin(async move {
                // Slight delay so concurrent callers pile up behind the inflight lock.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                *c.lock() += 1;
                Err(crate::engine::usage::UsageError::Status(429))
            })
        }));

        let st = Arc::new(st);
        let mut handles = Vec::new();
        for _ in 0..5 {
            let st2 = st.clone();
            handles.push(tokio::spawn(async move {
                oneshot_req((*st2).clone(), Request::get("/api/usage")
                    .header("authorization", auth(&st2)).body(Body::empty()).unwrap()).await
            }));
        }
        let results: Vec<_> = futures_util::future::join_all(handles).await
            .into_iter().map(|r| r.unwrap()).collect();

        // All must return an error status (503 or 503/stale).
        for (s, _b) in &results {
            assert!(*s == StatusCode::SERVICE_UNAVAILABLE,
                "expected 503 on failure, got {}", s);
        }
        // The upstream usage_fn must have been called exactly ONCE.
        let n = *calls.lock();
        assert_eq!(n, 1, "expected exactly 1 upstream call, got {n}");
    }

    #[tokio::test]
    async fn devices_post_requires_token() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/devices")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from("{}")).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "token required");
    }

    #[tokio::test]
    async fn devices_post_saves_and_returns_ok() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/devices")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"token":"my-device-token"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["ok"], true);
        assert!(b["registeredAt"].is_i64());
    }

    #[tokio::test]
    async fn templates_start_no_name_is_400() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/templates/start")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from("{}")).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "name required");
    }

    #[tokio::test]
    async fn templates_start_unknown_name_is_404() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st.clone(), Request::post("/api/templates/start")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"no-such"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(b["error"].as_str().unwrap().contains("no-such"));
    }
}
