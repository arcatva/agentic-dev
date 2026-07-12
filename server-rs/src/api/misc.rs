use crate::api::state::AppState;
use crate::util::now_ms;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

const USAGE_FRESH_MS: i64 = 60_000;
const USAGE_STALE_MAX_MS: i64 = 10 * 60_000;

pub async fn usage_route(State(st): State<AppState>) -> Response {
    let now = now_ms();
    // Fast path: fresh cache.
    {
        let c = st.usage_cache.lock();
        if let Some(ref data) = c.data {
            if now - c.at < USAGE_FRESH_MS {
                return Json(data.clone()).into_response();
            }
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
            if t - c.at < USAGE_FRESH_MS {
                return Json(data.clone()).into_response();
            }
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
            let err = c
                .last_error
                .clone()
                .unwrap_or_else(|| "usage fetch failed".to_string());
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": err}))).into_response();
        }
    }
    // Record attempt time BEFORE firing so late-arriving waiters (who acquire after us) skip.
    {
        let mut c = st.usage_cache.lock();
        c.last_attempt_at = now_ms();
    }
    let base = st.config.claude_config_base.clone();
    let fetched = match &st.usage_fn {
        Some(f) => f().await,
        None => crate::engine::usage::fetch_usage(&base, None).await,
    };
    match fetched {
        Ok(data) => {
            {
                let mut c = st.usage_cache.lock();
                c.at = now_ms();
                c.data = Some(data.clone());
                c.last_error = None;
            }
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
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
    Json(crate::engine::plugins::list_plugins(
        &st.config.claude_config_base,
    ))
}

/// GET /api/global-settings — unified skill+plugin components with their global on/off state.
pub async fn global_settings_route(
    State(st): State<AppState>,
) -> impl axum::response::IntoResponse {
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
        "plugin" | "skill" | "mcp" => {
            let components =
                crate::engine::components::list_components(base, &st.config.skills_dir);
            let known = components
                .iter()
                .any(|c| c.kind == req.kind && c.id == req.id);
            if !known {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("unknown {} id: {}", req.kind, req.id)})),
                )
                    .into_response();
            }
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown kind: {other}")})),
            )
                .into_response();
        }
    }

    let res = match req.kind.as_str() {
        "plugin" => crate::engine::global_settings::set_plugin_enabled(base, &req.id, req.enabled),
        "skill" => crate::engine::global_settings::set_skill_enabled(base, &req.id, req.enabled),
        // MCP: move the definition between mcpServers and the disabled parking key in
        // .claude.json. list_components enumerates both sides, so the id is known-valid
        // here; a concurrent external edit could still make it vanish → surface as an error.
        "mcp" => crate::engine::user_config::set_mcp_server_enabled(base, &req.id, req.enabled)
            .and_then(|found| {
                if found {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("unknown mcp id: {}", req.id),
                    ))
                }
            }),
        // Unreachable: the match above already validated kind.
        _ => unreachable!(),
    };
    match res {
        Ok(()) => Json(crate::engine::components::list_components(
            base,
            &st.config.skills_dir,
        ))
        .into_response(),
        // NotFound = the id vanished between the known-check and the mutate (external edit
        // race) — same logical class as the pre-check's "unknown id", so same 400, not a 500.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── DB-backed groups CRUD (replaces the old file-based groups_get / groups_put) ──

pub async fn groups_list(State(st): State<AppState>) -> Response {
    match st.store.list_groups().await {
        Ok(groups) => Json(json!({ "groups": groups })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct CreateGroupBody {
    pub name: Option<String>,
    pub icon: Option<String>,
}

pub async fn groups_create(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: CreateGroupBody = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let Some(name) = b.name.filter(|n| !n.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"name required"})),
        )
            .into_response();
    };
    match st.store.create_group(name.trim(), b.icon.as_deref()).await {
        Ok(group) => Json(json!({ "group": group })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct UpdateGroupBody {
    pub name: Option<String>,
    pub icon: Option<String>,
}

pub async fn groups_update(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let b: UpdateGroupBody = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    match st
        .store
        .update_group(&id, b.name.as_deref(), b.icon.as_deref())
        .await
    {
        Ok(Some(group)) => Json(json!({ "group": group })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"group not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn groups_delete(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match st.store.delete_group(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn templates_get(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::templates::list_templates(
        &st.config.templates_path,
    ))
}

pub async fn templates_put(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if !v.is_array() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"array of templates required"})),
        )
            .into_response();
    }
    match crate::engine::templates::save_templates(&st.config.templates_path, &v) {
        Ok(t) => Json(t).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
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
    let b: TemplateStartBody = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let Some(name) = b.name.filter(|n| !n.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"name required"})),
        )
            .into_response();
    };
    let templates = crate::engine::templates::list_templates(&st.config.templates_path);
    let Some(tpl) = templates.into_iter().find(|t| t.name == name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("template '{name}' not found")})),
        )
            .into_response();
    };
    let prompt =
        crate::engine::templates::resolve_prompt(&tpl.prompt_body, &b.vars.unwrap_or_default());
    let meta = crate::engine::SubmitMeta {
        model: b.model.or(tpl.model),
        effort: b.effort.or(tpl.effort),
        mode: b.mode.or(tpl.mode),
        permission_mode: None, // templates don't carry a permission_mode (yet)
        hidden_skills: Vec::new(), // templates don't carry a skill blacklist (yet)
        hidden_plugins: Vec::new(), // templates don't carry a plugin blacklist (yet)
        hidden_mcp_servers: Vec::new(), // templates don't carry an MCP blacklist (yet)
        extra_mcp_servers: Vec::new(), // templates don't carry extra MCP servers (yet)
        claude_md: None,       // templates don't carry session-scoped CLAUDE.md (yet)
        staged_uploads: Vec::new(), // templates don't carry pre-session attachments
        forced_on_plugins: Vec::new(), // templates don't carry forced-on overrides (yet)
        forced_on_skills: Vec::new(),
        forced_on_mcp_servers: Vec::new(),
    };
    match st
        .engine
        .submit_session(
            tpl.repos,
            tpl.skills,
            prompt,
            std::collections::HashMap::new(),
            meta,
        )
        .await
    {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct DeviceBody {
    pub token: Option<String>,
}

pub async fn devices_post(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: DeviceBody = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let token = b.token.unwrap_or_default();
    let token = token.trim().to_string();
    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"token required"})),
        )
            .into_response();
    }
    match crate::engine::push::save_device_token(&st.config.device_token_path, &token) {
        Ok(rec) => Json(json!({ "ok": true, "registeredAt": rec.registered_at })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
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
    enabled: bool,
    has_key: bool,
    /// True when this provider is authenticated via ChatGPT-subscription OAuth (no BYOK key).
    oauth: bool,
    /// ChatGPT account id, populated for a connected oauth provider (non-secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    /// Access-token expiry (epoch seconds), for oauth providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    /// True when the refresh token is dead and the user must log in again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    needs_reauth: bool,
}

fn provider_view(p: &crate::engine::providers::Provider) -> ProviderView {
    // Surface non-secret OAuth status (account / expiry / needs-reauth) for the connect UI.
    let (account_id, expires_at, needs_reauth) = if p.oauth {
        let s = crate::engine::oauth::status();
        (
            (!s.account_id.is_empty()).then_some(s.account_id),
            Some(s.expires_at),
            s.needs_reauth,
        )
    } else {
        (None, None, false)
    };
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
        enabled: p.enabled,
        has_key: !p.resolved_key().is_empty(),
        oauth: p.oauth,
        account_id,
        expires_at,
        needs_reauth,
    }
}

/// GET /api/providers — list registered providers, keys masked (api_key is never returned).
pub async fn providers_get() -> Response {
    let views: Vec<ProviderView> = crate::engine::providers::load_list()
        .iter()
        .map(provider_view)
        .collect();
    Json(json!({ "providers": views })).into_response()
}

/// POST /api/providers — add or replace a provider by name. Body (snake_case):
/// {name, base_url, api_key | api_key_env, model, protocol?, capability?, description?, priority?, router?}.
/// The key is write-only: on an EDIT (a name that already exists) a blank `api_key` means "keep the
/// stored key" — `upsert` preserves the existing credential rather than wiping it.
pub async fn providers_post(body: axum::body::Bytes) -> Response {
    let mut p: crate::engine::providers::Provider = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid provider: {e}")})),
            )
                .into_response()
        }
    };
    // Trim before saving: routing/deletion use exact (case-insensitive) name equality, so a stored
    // " minimax " could never be matched/deleted by "minimax".
    p.name = p.name.trim().to_string();
    p.base_url = p.base_url.trim().to_string();
    p.model = p.model.trim().to_string();
    // Capability is the routing axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.capability.is_nan() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"capability cannot be NaN"})),
        )
            .into_response();
    }
    p.capability = p.capability.clamp(0.0, 1.0);
    // Priority is the routing-preference axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.priority.is_nan() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"priority cannot be NaN"})),
        )
            .into_response();
    }
    p.priority = p.priority.clamp(0.0, 1.0);
    // Cost is the routing-cost axis (0–1): reject NaN (clamp panics on NaN) then clamp.
    if p.cost.is_nan() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"cost cannot be NaN"})),
        )
            .into_response();
    }
    p.cost = p.cost.clamp(0.0, 1.0);
    if p.name.is_empty() || p.base_url.is_empty() || p.model.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"name, base_url, model are required"})),
        )
            .into_response();
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
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("router validation failed: {e}")})),
            )
                .into_response();
        }
    }
    match crate::engine::providers::upsert(p) {
        Ok(()) => {
            // An openai provider may have been added/changed → regenerate the LiteLLM config + restart.
            crate::engine::litellm::request_reload();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /api/providers/{name} — remove a provider.
pub async fn providers_delete(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    // Disconnecting an oauth provider must also wipe its token store (else the refresher keeps a dead
    // token around and the next connect can't tell it apart from a fresh one).
    let was_oauth = crate::engine::providers::load_list()
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(&name) && p.oauth);
    match crate::engine::providers::remove(&name) {
        Ok(true) => {
            if was_oauth {
                crate::engine::oauth::disconnect();
            }
            crate::engine::litellm::request_reload();
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"no such provider"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── native Claude per-family routing overrides ──

#[derive(serde::Serialize)]
struct NativeModelRef {
    id: String,
    display_name: String,
}

#[derive(serde::Serialize)]
struct NativeFamilyView {
    family: String,
    label: String,
    models: Vec<NativeModelRef>,
    capability: f32,
    priority: f32,
    cost: f32,
    description: String,
    enabled: bool,
    customized: bool,
    editable: bool,
}

fn default_true() -> bool {
    true
}

#[derive(serde::Deserialize)]
struct NativeOverrideReq {
    capability: f32,
    priority: f32,
    cost: f32,
    #[serde(default)]
    description: String,
    /// Whether this native family participates in routing. Defaults true (old clients omit it).
    #[serde(default = "default_true")]
    enabled: bool,
}

fn is_editable_family(family: &str) -> bool {
    matches!(family, "opus" | "sonnet" | "haiku" | "fable")
}

fn family_label(family: &str) -> &'static str {
    match family {
        "opus" => "Opus",
        "sonnet" => "Sonnet",
        "haiku" => "Haiku",
        "fable" => "Fable",
        _ => "Other",
    }
}

/// GET /api/native-models — native Claude families with effective routing metrics + override state.
pub async fn native_models_get() -> Response {
    use crate::engine::providers::{
        family_default_metrics, family_of, native_claude_models, DEFAULT_NATIVE_PRIORITY,
    };
    let overrides = crate::engine::native_overrides::load_map();

    // Group discovered models by family, first-seen (newest-first) order.
    let mut order: Vec<&'static str> = Vec::new();
    let mut groups: std::collections::HashMap<&'static str, Vec<NativeModelRef>> =
        std::collections::HashMap::new();
    for m in native_claude_models() {
        let fam = family_of(&m.id);
        groups.entry(fam).or_default().push(NativeModelRef {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
        });
        if !order.contains(&fam) {
            order.push(fam);
        }
    }

    let mut views: Vec<NativeFamilyView> = order
        .into_iter()
        .map(|fam| {
            let (dc, dk) = family_default_metrics(fam);
            let (capability, priority, cost, description, enabled, customized) =
                match overrides.get(fam) {
                    Some(o) => (
                        o.capability,
                        o.priority,
                        o.cost,
                        o.description.clone(),
                        o.enabled,
                        true,
                    ),
                    None => (dc, DEFAULT_NATIVE_PRIORITY, dk, String::new(), true, false),
                };
            NativeFamilyView {
                family: fam.to_string(),
                label: family_label(fam).to_string(),
                models: groups.remove(fam).unwrap_or_default(),
                capability,
                priority,
                cost,
                description,
                enabled,
                customized,
                editable: is_editable_family(fam),
            }
        })
        .collect();
    // cheap → capable, family-name tiebreak (matches the /api/models ordering contract).
    views.sort_by(|a, b| {
        a.capability
            .total_cmp(&b.capability)
            .then_with(|| a.family.cmp(&b.family))
    });

    Json(json!({ "families": views })).into_response()
}

/// POST /api/native-models/{family} — set a family's routing override.
pub async fn native_models_post(
    axum::extract::Path(family): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let family = family.trim().to_lowercase();
    if !is_editable_family(&family) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("not an editable family: {family}")})),
        )
            .into_response();
    }
    let req: NativeOverrideReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid override: {e}")})),
            )
                .into_response()
        }
    };
    for (name, v) in [
        ("capability", req.capability),
        ("priority", req.priority),
        ("cost", req.cost),
    ] {
        if v.is_nan() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{name} cannot be NaN")})),
            )
                .into_response();
        }
    }
    let ov = crate::engine::native_overrides::NativeOverride {
        capability: req.capability.clamp(0.0, 1.0),
        priority: req.priority.clamp(0.0, 1.0),
        cost: req.cost.clamp(0.0, 1.0),
        description: req.description,
        enabled: req.enabled,
    };
    match crate::engine::native_overrides::upsert(&family, ov) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /api/native-models/{family} — reset a family to defaults (idempotent).
pub async fn native_models_delete(
    axum::extract::Path(family): axum::extract::Path<String>,
) -> Response {
    let family = family.trim().to_lowercase();
    if !is_editable_family(&family) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("not an editable family: {family}")})),
        )
            .into_response();
    }
    match crate::engine::native_overrides::remove(&family) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/routing — the global cost⇄quality tradeoff knob (0=cheapest .. 1=strongest).
pub async fn routing_get() -> Response {
    Json(json!({ "tradeoff": crate::engine::routing_config::load().tradeoff })).into_response()
}

#[derive(serde::Deserialize)]
struct RoutingReq {
    tradeoff: f32,
}

/// POST /api/routing — set the global tradeoff. Rejects NaN; `save` clamps to [0,1]. Responds with
/// the effective (clamped) value so the client can reflect it.
pub async fn routing_post(body: axum::body::Bytes) -> Response {
    let req: RoutingReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid routing config: {e}")})),
            )
                .into_response()
        }
    };
    if req.tradeoff.is_nan() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"tradeoff cannot be NaN"})),
        )
            .into_response();
    }
    let cfg = crate::engine::routing_config::RoutingConfig {
        tradeoff: req.tradeoff,
    };
    match crate::engine::routing_config::save(&cfg) {
        Ok(()) => Json(json!({
            "ok": true,
            "tradeoff": crate::engine::routing_config::load().tradeoff
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── model catalog (native Claude tiers + registered BYOK providers) ──

#[derive(serde::Serialize)]
struct ModelEntry {
    key: String,     // "claude-opus-4-8"
    label: String,   // "Opus 4.8"
    native: bool,    // true for subscription tiers
    default: bool,   // true for the strongest native Claude
    capability: f32, // 0..1, for ordering
    cost: f32,       // 0..1 (lower = cheaper), for the UI cost indicator
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
    entries.sort_by(|a, b| {
        a.capability
            .total_cmp(&b.capability)
            .then_with(|| a.key.cmp(&b.key))
    });
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
    entries.sort_by(|a, b| {
        a.capability
            .total_cmp(&b.capability)
            .then_with(|| a.key.cmp(&b.key))
    });
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

// ── Component CRUD — add/delete MCP servers, skills, plugins ──

pub async fn mcp_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let def: crate::engine::store::McpServerDef = match serde_json::from_slice(&body) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response()
        }
    };
    if !crate::api::validation::valid_component_name(&def.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid or reserved MCP name: {:?}", def.name)})),
        )
            .into_response();
    }
    if let Err(e) = crate::api::validation::validate_mcp_def(&def) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    match crate::engine::user_config::add_mcp_server(&base, &def) {
        Ok(()) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn mcp_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_component_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid MCP name: {name:?}")})),
        )
            .into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    match crate::engine::user_config::delete_mcp_server(&base, &name) {
        Ok(true) => {
            Json(crate::engine::components::list_components(&base, &skills)).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("MCP server '{name}' not found")})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct AddSkillBody {
    pub name: String,
    pub description: String,
    /// The skill's markdown body — the actual instructions the agent loads. Optional for
    /// back-compat; without it the created skill is an empty shell (frontmatter only).
    #[serde(default)]
    pub instructions: String,
}

pub async fn skills_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: AddSkillBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response()
        }
    };
    if !crate::api::validation::valid_component_name(&b.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid skill name: {:?}", b.name)})),
        )
            .into_response();
    }
    let skills = st.config.skills_dir.clone();
    let base = st.config.claude_config_base.clone();
    match crate::engine::skills::add_skill(&skills, &b.name, &b.description, &b.instructions) {
        Ok(()) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("skill '{}' already exists", b.name)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct CatalogQuery {
    #[serde(default)]
    pub refresh: bool,
}

/// GET /api/skills/catalog[?refresh=true] — the aggregated external skill store across every
/// configured source. A broken source shows up in `errors` instead of failing the whole store.
pub async fn skills_catalog_route(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<CatalogQuery>,
) -> Response {
    let (skills, errors) =
        crate::engine::skill_install::fetch_catalog(&st.config.claude_config_base, q.refresh).await;
    // Per-request (never cached): compare each entry's content fingerprint against the
    // installed copy's provenance metadata so the app can show Update only when one exists.
    // Blocking file reads → spawn_blocking, keeping the async executor free.
    let skills_dir = st.config.skills_dir.clone();
    let skills = tokio::task::spawn_blocking(move || {
        let mut skills = skills;
        crate::engine::skill_install::annotate_update_available(&mut skills, &skills_dir);
        skills
    })
    .await
    // JoinError = the annotation task panicked (it can't, absent fs pathology). An empty
    // list beats a 500 for a read-only catalog.
    .unwrap_or_default();
    Json(json!({ "skills": skills, "errors": errors })).into_response()
}

/// GET /api/skills/sources — the configured store sources.
pub async fn skills_sources_route(State(st): State<AppState>) -> Response {
    Json(json!({ "sources": crate::engine::skill_install::read_sources(&st.config.claude_config_base) })).into_response()
}

#[derive(serde::Deserialize)]
pub struct SkillSourceBody {
    pub source: String,
}

/// POST /api/skills/sources — add a store source (owner/repo[/path] or github.com URL).
pub async fn skills_sources_add_route(
    State(st): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let b: SkillSourceBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response()
        }
    };
    // Caller-fault (bad syntax) → 400 up front; anything add_source itself fails on afterwards
    // is a server-side write problem → 500 (consistent with the DELETE route).
    if let Err(e) = crate::engine::skill_install::parse_github_source(&b.source) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    match crate::engine::skill_install::add_source(&st.config.claude_config_base, &b.source) {
        Ok(sources) => Json(json!({ "sources": sources })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct SkillSourceQuery {
    pub source: String,
}

/// DELETE /api/skills/sources?source=… — remove a store source (query param: sources contain
/// slashes, which a path segment would mangle).
pub async fn skills_sources_delete_route(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SkillSourceQuery>,
) -> Response {
    match crate::engine::skill_install::remove_source(&st.config.claude_config_base, &q.source) {
        Ok((sources, true)) => Json(json!({ "sources": sources })).into_response(),
        Ok((_, false)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown source: {}", q.source)})),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct InstallSkillBody {
    pub source: String,
    /// true = update: replace an existing install of the same name (atomic swap).
    #[serde(default)]
    pub update: bool,
}

/// POST /api/skills/install — download a skill (SKILL.md + companion files) from a GitHub
/// source (`owner/repo[/path]` or a github.com URL) into the skills dir.
pub async fn skills_install_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: InstallSkillBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response()
        }
    };
    // Parse errors are the caller's fault (400) BEFORE any network is touched.
    if let Err(e) = crate::engine::skill_install::parse_github_source(&b.source) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    let skills = st.config.skills_dir.clone();
    let base = st.config.claude_config_base.clone();
    match crate::engine::skill_install::install_from_source(&skills, &b.source, b.update).await {
        Ok(_name) => {
            Json(crate::engine::components::list_components(&base, &skills)).into_response()
        }
        // Everything else mixes remote and local causes; BAD_GATEWAY for remote-ish messages
        // would be guesswork — a 400 with the human-readable reason serves the app either way.
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn skills_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_component_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid skill name: {name:?}")})),
        )
            .into_response();
    }
    let skills = st.config.skills_dir.clone();
    let base = st.config.claude_config_base.clone();
    match crate::engine::skills::delete_skill(&skills, &name) {
        Ok(true) => {
            Json(crate::engine::components::list_components(&base, &skills)).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("skill '{name}' not found")})),
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid skill path: {e}")})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct AddPluginBody {
    pub id: String,
}

/// CLI-reported failures (nonzero exit: unknown plugin/marketplace…) are the caller's
/// error → 400; spawn/timeout/join failures are ours → 500. Prefixes match the exact
/// error formats of `plugin_cli::run_plugin_command` plus this file's spawn_blocking wrapper.
fn plugin_error_status(e: &str) -> StatusCode {
    if e.starts_with("failed to spawn")
        || e.starts_with("process error")
        || e.starts_with("task error")
        || e.starts_with("plugin command timed out")
    {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    }
}

pub async fn plugins_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: AddPluginBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response()
        }
    };
    if !crate::api::validation::valid_plugin_id(&b.id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid plugin id: {:?}", b.id)})),
        )
            .into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    let id = b.id.clone();
    let result =
        tokio::task::spawn_blocking(move || crate::engine::plugin_cli::install_plugin(&base, &id))
            .await
            .map_err(|e| format!("task error: {e}"))
            .and_then(|r| r);
    match result {
        Ok(_stdout) => Json(crate::engine::components::list_components(
            &st.config.claude_config_base,
            &skills,
        ))
        .into_response(),
        Err(e) => (plugin_error_status(&e), Json(json!({"error": e}))).into_response(),
    }
}

pub async fn plugins_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_plugin_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid plugin id: {id:?}")})),
        )
            .into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    let id2 = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::engine::plugin_cli::uninstall_plugin(&base, &id2)
    })
    .await
    .map_err(|e| format!("task error: {e}"))
    .and_then(|r| r);
    match result {
        Ok(_stdout) => Json(crate::engine::components::list_components(
            &st.config.claude_config_base,
            &skills,
        ))
        .into_response(),
        Err(e) => (plugin_error_status(&e), Json(json!({"error": e}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{auth, oneshot_req, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn provider_view_masks_the_key() {
        let p = crate::engine::providers::Provider {
            name: "minimax".into(),
            base_url: "https://x".into(),
            api_key: "SECRET".into(),
            api_key_env: None,
            model: "MiniMax-M3".into(),
            protocol: crate::engine::providers::Protocol::Anthropic,
            capability: 0.5,
            description: None,
            priority: 0.5,
            cost: 0.3,
            router: false,
            enabled: true,
            oauth: false,
        };
        let json = serde_json::to_string(&provider_view(&p)).unwrap();
        assert!(
            !json.contains("SECRET"),
            "key must never appear in the view: {json}"
        );
        assert!(!json.contains("api_key"));
        assert!(json.contains("\"has_key\":true"));
        let p2 = crate::engine::providers::Provider {
            api_key: String::new(),
            ..p
        };
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
        *crate::engine::providers::PROVIDERS_FILE_OVERRIDE.lock() =
            Some(dir.path().join("providers.json"));
        ProvidersFileGuard {
            _dir: dir,
            _lock: lock,
        }
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
                enabled: true,
                oauth: false,
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
                enabled: true,
                oauth: false,
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
        assert!(
            !models.is_empty(),
            "native Claude candidates should be present"
        );
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
        let default_key = models.iter().find(|m| m["default"] == true).unwrap()["key"]
            .as_str()
            .unwrap();
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
        let (s, b) = oneshot_req(
            st.clone(),
            Request::post("/api/providers")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"x","base_url":"https://x/anthropic","model":"","api_key":"k"}"#,
                ))
                .unwrap(),
        )
        .await;
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
        assert!(b["error"]
            .as_str()
            .unwrap()
            .contains("router validation failed"));
    }

    #[tokio::test]
    async fn usage_serves_fetch_then_caches() {
        let mut st = test_state().await;
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        st.usage_fn = Some(Arc::new(move || {
            let c = c.clone();
            Box::pin(async move {
                *c.lock() += 1;
                Ok(serde_json::json!({"five_hour":{"utilization":12,"resets_at":"x"}}))
            })
        }));
        for _ in 0..2 {
            let (s, b) = oneshot_req(
                st.clone(),
                Request::get("/api/usage")
                    .header("authorization", auth(&st))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(b["five_hour"]["utilization"], 12);
        }
        // Within the 60s freshness window the second call is served from cache → fetch ran once.
        assert_eq!(*calls.lock(), 1);
    }

    #[tokio::test]
    async fn usage_failure_with_no_cache_is_503() {
        let mut st = test_state().await;
        st.usage_fn = Some(Arc::new(|| {
            Box::pin(async { Err(crate::engine::usage::UsageError::Status(429)) })
        }));
        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/usage")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
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
                oneshot_req(
                    (*st2).clone(),
                    Request::get("/api/usage")
                        .header("authorization", auth(&st2))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
            }));
        }
        let results: Vec<_> = futures_util::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // All must return an error status (503 or 503/stale).
        for (s, _b) in &results {
            assert!(
                *s == StatusCode::SERVICE_UNAVAILABLE,
                "expected 503 on failure, got {}",
                s
            );
        }
        // The upstream usage_fn must have been called exactly ONCE.
        let n = *calls.lock();
        assert_eq!(n, 1, "expected exactly 1 upstream call, got {n}");
    }

    #[tokio::test]
    async fn devices_post_requires_token() {
        let st = test_state().await;
        let (s, b) = oneshot_req(
            st.clone(),
            Request::post("/api/devices")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "token required");
    }

    #[tokio::test]
    async fn devices_post_saves_and_returns_ok() {
        let st = test_state().await;
        let (s, b) = oneshot_req(
            st.clone(),
            Request::post("/api/devices")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"my-device-token"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["ok"], true);
        assert!(b["registeredAt"].is_i64());
    }

    #[tokio::test]
    async fn templates_start_no_name_is_400() {
        let st = test_state().await;
        let (s, b) = oneshot_req(
            st.clone(),
            Request::post("/api/templates/start")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "name required");
    }

    #[tokio::test]
    async fn templates_start_unknown_name_is_404() {
        let st = test_state().await;
        let (s, b) = oneshot_req(
            st.clone(),
            Request::post("/api/templates/start")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"no-such"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(b["error"].as_str().unwrap().contains("no-such"));
    }

    // ── Component CRUD tests ──

    #[tokio::test]
    async fn mcp_add_rejects_bad_name() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(
            st,
            Request::post("/api/mcp-servers")
                .header("authorization", tok)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"../evil","command":"x"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("invalid"));
    }

    #[tokio::test]
    async fn mcp_add_rejects_reserved_agentic_name() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(
            st,
            Request::post("/api/mcp-servers")
                .header("authorization", tok)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"agentic","command":"x"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let err = b["error"].as_str().unwrap();
        assert!(
            err.to_lowercase().contains("reserved") || err.contains("invalid"),
            "error should mention reserved or invalid: {err}"
        );
    }

    #[tokio::test]
    async fn mcp_add_and_delete_round_trip() {
        let st = test_state().await;
        let tok = auth(&st);
        // Add
        let (s, _) = oneshot_req(
            st.clone(),
            Request::post("/api/mcp-servers")
                .header("authorization", tok.clone())
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"test-mcp","command":"node","args":["s.js"]}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // Verify in list
        let (s2, arr) = oneshot_req(
            st.clone(),
            Request::get("/api/global-settings")
                .header("authorization", tok.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);
        assert!(
            arr.as_array()
                .unwrap()
                .iter()
                .any(|c| c["kind"] == "mcp" && c["id"] == "test-mcp"),
            "mcp must appear after add: {arr}"
        );
        // Delete
        let (s3, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/mcp-servers/test-mcp")
                .header("authorization", tok.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s3, StatusCode::OK);
        // Verify gone
        let (_, arr2) = oneshot_req(
            st.clone(),
            Request::get("/api/global-settings")
                .header("authorization", tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            !arr2
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == "test-mcp"),
            "mcp must be gone after delete: {arr2}"
        );
    }

    #[tokio::test]
    async fn mcp_delete_absent_is_404() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(
            st,
            Request::delete("/api/mcp-servers/no-such")
                .header("authorization", tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(b["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn skills_add_rejects_bad_name() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(
            st,
            Request::post("/api/skills")
                .header("authorization", tok)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"a/b","description":"d"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn skills_add_and_delete_round_trip() {
        let st = test_state().await;
        let tok = auth(&st);
        // Ensure skills_dir exists (test_state sets it to temp/skills)
        std::fs::create_dir_all(&st.config.skills_dir).unwrap();
        // Add
        let (s, arr) = oneshot_req(
            st.clone(),
            Request::post("/api/skills")
                .header("authorization", tok.clone())
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"my-skill","description":"does stuff"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            arr.as_array()
                .unwrap()
                .iter()
                .any(|c| c["kind"] == "skill" && c["id"] == "my-skill"),
            "skill must appear in component list: {arr}"
        );
        // Delete
        let (s2, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/skills/my-skill")
                .header("authorization", tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);
    }

    #[tokio::test]
    async fn plugins_add_rejects_bad_id() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(
            st,
            Request::post("/api/plugins")
                .header("authorization", tok)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"a b"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().is_some());
    }

    async fn post_json(
        st: &crate::api::state::AppState,
        path: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        oneshot_req(
            st.clone(),
            Request::post(path)
                .header("authorization", auth(st))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    #[tokio::test]
    async fn mcp_add_rejects_incomplete_defs() {
        let st = test_state().await;
        for (body, want) in [
            (r#"{"name":"x"}"#, "command"),
            (r#"{"name":"x","command":"  "}"#, "command"),
            (r#"{"name":"x","type":"stdio"}"#, "command"),
            (r#"{"name":"x","type":"http"}"#, "url"),
            (r#"{"name":"x","type":"http","url":"ftp://e.com"}"#, "url"),
            (r#"{"name":"x","type":"sse","url":"https://"}"#, "url"),
            (r#"{"name":"x","type":"http","url":"https:///mcp"}"#, "url"),
            (r#"{"name":"x","type":"http","url":"http://?x=1"}"#, "url"),
            (r#"{"name":"x","type":"ws","url":"https://e.com"}"#, "type"),
            (
                r#"{"name":"x","command":"c","url":"https://e.com"}"#,
                "both",
            ),
            (
                r#"{"name":"x","type":"http","command":"c","url":"https://e.com"}"#,
                "command",
            ),
        ] {
            let (s, b) = post_json(&st, "/api/mcp-servers", body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "body: {body}");
            assert!(b["error"].as_str().unwrap().contains(want), "{body} -> {b}");
        }
    }

    #[tokio::test]
    async fn mcp_add_http_round_trip() {
        let st = test_state().await;
        let (s, _) = post_json(
            &st,
            "/api/mcp-servers",
            r#"{"name":"e2e-http","type":"http","url":"https://example.com/mcp"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let cj = st
            .config
            .claude_config_base
            .parent()
            .unwrap()
            .join(".claude.json");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(saved["mcpServers"]["e2e-http"]["type"], "http");
        assert_eq!(
            saved["mcpServers"]["e2e-http"]["url"],
            "https://example.com/mcp"
        );
        let (s2, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/mcp-servers/e2e-http")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);
        // url without "type" is accepted and persists with the default type "http".
        let (s3, _) = post_json(
            &st,
            "/api/mcp-servers",
            r#"{"name":"e2e-default","url":"https://example.com/mcp"}"#,
        )
        .await;
        assert_eq!(s3, StatusCode::OK);
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(saved["mcpServers"]["e2e-default"]["type"], "http");
    }

    #[tokio::test]
    async fn skill_sources_crud_round_trip() {
        let st = test_state().await;
        let tok = auth(&st);
        // Default seed present.
        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/skills/sources")
                .header("authorization", tok.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["sources"], serde_json::json!(["anthropics/skills"]));
        // Add.
        let (s, b) = post_json(
            &st,
            "/api/skills/sources",
            r#"{"source":"octocat/Hello-World"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["sources"].as_array().unwrap().len(), 2);
        // Duplicate (trailing-slash variant) is deduped, not appended.
        let (s, b) = post_json(
            &st,
            "/api/skills/sources",
            r#"{"source":"octocat/Hello-World/"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["sources"].as_array().unwrap().len(), 2);
        // Bad syntax → 400.
        let (s, _) = post_json(&st, "/api/skills/sources", r#"{"source":"not a source"}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // Remove.
        let (s, b) = oneshot_req(
            st.clone(),
            Request::delete("/api/skills/sources?source=octocat%2FHello-World")
                .header("authorization", tok.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["sources"], serde_json::json!(["anthropics/skills"]));
        // Remove unknown → 404.
        let (s, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/skills/sources?source=never%2Fadded")
                .header("authorization", tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn skills_install_rejects_bad_source_before_network() {
        let st = test_state().await;
        for body in [
            r#"{"source":"not a source"}"#,
            r#"{"source":""}"#,
            r#"{"source":"https://gitlab.com/x/y"}"#,
        ] {
            let (s, b) = post_json(&st, "/api/skills/install", body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "body: {body}");
            assert!(b["error"].as_str().is_some());
        }
    }

    struct NativeOvGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for NativeOvGuard {
        fn drop(&mut self) {
            *crate::engine::native_overrides::NATIVE_OVERRIDES_FILE_OVERRIDE.lock() = None;
        }
    }
    fn isolated_native_overrides_file() -> NativeOvGuard {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        *crate::engine::native_overrides::NATIVE_OVERRIDES_FILE_OVERRIDE.lock() =
            Some(dir.path().join("native-overrides.json"));
        NativeOvGuard {
            _dir: dir,
            _lock: lock,
        }
    }

    struct RoutingGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for RoutingGuard {
        fn drop(&mut self) {
            *crate::engine::routing_config::ROUTING_FILE_OVERRIDE.lock() = None;
        }
    }
    fn isolated_routing_file() -> RoutingGuard {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        *crate::engine::routing_config::ROUTING_FILE_OVERRIDE.lock() =
            Some(dir.path().join("routing.json"));
        RoutingGuard {
            _dir: dir,
            _lock: lock,
        }
    }

    #[tokio::test]
    async fn routing_get_post_roundtrip_and_clamp() {
        let _rc = isolated_routing_file();
        let st = test_state().await;
        let get = |st: AppState| async move {
            oneshot_req(
                st.clone(),
                Request::get("/api/routing")
                    .header("authorization", auth(&st))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        };
        let post = |st: AppState, body: &'static str| async move {
            oneshot_req(
                st.clone(),
                Request::post("/api/routing")
                    .header("authorization", auth(&st))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
        };
        // Default when no file exists.
        let (s, b) = get(st.clone()).await;
        assert_eq!(s, StatusCode::OK);
        assert!((b["tradeoff"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        // In-range round-trips and GET reflects it.
        let (s, b) = post(st.clone(), r#"{"tradeoff":0.2}"#).await;
        assert_eq!(s, StatusCode::OK);
        assert!((b["tradeoff"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        let (_s, b) = get(st.clone()).await;
        assert!((b["tradeoff"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        // Out-of-range is clamped by `save`, and the response echoes the effective value.
        let (_s, b) = post(st.clone(), r#"{"tradeoff":1.7}"#).await;
        assert!((b["tradeoff"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        // A non-numeric tradeoff (JSON can't carry NaN) fails to parse → 400.
        let (s, _b) = post(st.clone(), r#"{"tradeoff": null}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn native_models_get_groups_then_post_marks_customized() {
        let _ov = isolated_native_overrides_file();
        let st = test_state().await;
        crate::engine::providers::seed_claude_models_for_tests();

        // GET: families present, opus editable and not customized
        let (s, b) = oneshot_req(
            st.clone(),
            Request::get("/api/native-models")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let fams = b["families"].as_array().unwrap();
        // Seed families: fable/opus/opus/sonnet/haiku → 4 groups (opus collapses in routing, but GET
        // groups by discovered family). Ordering contract: cheap → capable (family-name tiebreak).
        assert_eq!(fams.len(), 4);
        let order: Vec<&str> = fams.iter().map(|f| f["family"].as_str().unwrap()).collect();
        assert_eq!(order, vec!["haiku", "sonnet", "opus", "fable"]);
        let opus = fams.iter().find(|f| f["family"] == "opus").unwrap();
        assert_eq!(opus["editable"], true);
        assert_eq!(opus["customized"], false);
        // A non-overridden family returns an empty description verbatim (the generated per-model
        // fallback happens only at routing time, not in this view).
        assert_eq!(
            fams.iter().find(|f| f["family"] == "sonnet").unwrap()["description"],
            ""
        );

        // POST with mixed-case family normalizes and applies
        let (s2, _) = oneshot_req(
            st.clone(),
            Request::post("/api/native-models/Opus")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"capability":0.9,"priority":0.85,"cost":0.2,"description":"hard only"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);

        // GET again: opus is now customized with the new priority
        let (_, b3) = oneshot_req(
            st.clone(),
            Request::get("/api/native-models")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let opus3 = b3["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["family"] == "opus")
            .unwrap()
            .clone();
        assert_eq!(opus3["customized"], true);
        // f32→JSON widens to f64, so compare with a tolerance rather than `== 0.85` (which would fail).
        assert!((opus3["priority"].as_f64().unwrap() - 0.85).abs() < 1e-6);
        assert_eq!(opus3["description"], "hard only");
    }

    #[tokio::test]
    async fn native_models_post_rejects_other_and_bad_family_and_clamps() {
        let _ov = isolated_native_overrides_file();
        let st = test_state().await;

        // `other` is read-only
        let (s_other, _) = oneshot_req(
            st.clone(),
            Request::post("/api/native-models/other")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"capability":0.5,"priority":0.5,"cost":0.5}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s_other, StatusCode::BAD_REQUEST);

        // unknown family
        let (s_bad, _) = oneshot_req(
            st.clone(),
            Request::post("/api/native-models/nope")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"capability":0.5,"priority":0.5,"cost":0.5}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s_bad, StatusCode::BAD_REQUEST);

        // out-of-range clamps (stored value is 1.0, not 5.0)
        let (s_ok, _) = oneshot_req(
            st.clone(),
            Request::post("/api/native-models/sonnet")
                .header("authorization", auth(&st))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"capability":5.0,"priority":-1.0,"cost":0.5}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(s_ok, StatusCode::OK);
        let m = crate::engine::native_overrides::load_map();
        assert_eq!(m["sonnet"].capability, 1.0);
        assert_eq!(m["sonnet"].priority, 0.0);
    }

    #[tokio::test]
    async fn native_models_delete_resets_and_validates_family() {
        let _ov = isolated_native_overrides_file();
        let st = test_state().await;

        // seed an override, then reset it
        crate::engine::native_overrides::upsert(
            "opus",
            crate::engine::native_overrides::NativeOverride {
                capability: 0.9,
                priority: 0.8,
                cost: 0.2,
                description: String::new(),
                enabled: true,
            },
        )
        .unwrap();
        let (s, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/native-models/opus")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(crate::engine::native_overrides::load_map()
            .get("opus")
            .is_none());

        // idempotent: deleting again is still OK
        let (s2, _) = oneshot_req(
            st.clone(),
            Request::delete("/api/native-models/opus")
                .header("authorization", auth(&st))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);

        // invalid family (`other` catch-all, and an unknown name) → 400
        for bad in ["other", "nope"] {
            let (sb, _) = oneshot_req(
                st.clone(),
                Request::delete(format!("/api/native-models/{bad}"))
                    .header("authorization", auth(&st))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(sb, StatusCode::BAD_REQUEST, "DELETE {bad} must be 400");
        }
    }
}
