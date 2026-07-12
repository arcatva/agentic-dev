//! Provider registry for the delegate fan-out.
//!
//! A `Provider` is a user-registered model: `{name, base_url, api_key, model, protocol, capability,
//! description, priority, cost, router}`. The registry loads from a JSON file (`AGENTIC_PROVIDERS_FILE`,
//! else `~/.agentic-dev/providers.json`) — the BYOK store the Android "add a model" screen writes —
//! falling back to env-configured minimax/deepseek when no file is present.
//!
//! Model SELECTION is split in two:
//!   - the smart, task-aware pick (N-way across the WHOLE catalog) is the LLM-as-router in
//!     `engine::router`, which reads each provider's `description` / `capability` / `priority` / `cost`;
//!   - `route()` here is only the deterministic fallback — an explicit `model` hint wins, else the
//!     cheapest registered provider. No difficulty heuristic.
#![allow(dead_code)]

use crate::engine::native_overrides::OverrideMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Wire protocol the provider's endpoint speaks. `anthropic` (default) endpoints are called
/// directly by the worker bridge; `openai` endpoints are reached THROUGH the LiteLLM proxy
/// (Anthropic-in → OpenAI-out), wired in the OpenAI-compat slice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Anthropic,
    Openai,
}

fn default_capability() -> f32 {
    0.5
}

fn default_priority() -> f32 {
    0.5
}

fn default_cost() -> f32 {
    0.5
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// The API key literal. Empty → fall back to `api_key_env`.
    #[serde(default)]
    pub api_key: String,
    /// Alternative to a literal key: the name of an env var holding it (keeps secrets out of the file).
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    /// Endpoint wire protocol (anthropic | openai). Default anthropic (back-compat; old files load).
    #[serde(default)]
    pub protocol: Protocol,
    /// How capable this model is, 0.0–1.0 — the routing axis that REPLACES the old cheap/strong
    /// tier. Auto routing picks the cheapest model whose capability ≥ the task's difficulty.
    /// Defaults to 0.5 when unset (old files / unspecified).
    #[serde(default = "default_capability")]
    pub capability: f32,
    /// Free-text "what this model is good at" — read by the router/judge and shown in the UI.
    #[serde(default)]
    pub description: Option<String>,
    /// Scheduling priority 0.0–1.0 — higher = more preferred by the router among capable models.
    /// Defaults to 0.5 (old files / unspecified).
    #[serde(default = "default_priority")]
    pub priority: f32,
    /// Relative cost 0.0–1.0 — lower = cheaper. Among models of equal capability and priority,
    /// the router prefers the cheaper one (cost tiebreaker). Defaults to 0.5 (unspecified).
    #[serde(default = "default_cost")]
    pub cost: f32,
    /// Mark this provider as the model that MAKES routing decisions (the LLM-as-router). At most one
    /// should be set; `engine::router::router_provider` prefers it over the priority-based default.
    #[serde(default)]
    pub router: bool,
    /// Whether this model participates in routing at all. `false` → excluded from the candidate
    /// pool AND cannot act as the router (a disabled model neither receives work nor spends the
    /// user's key routing). Defaults `true` (old files / unspecified).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Marks the ChatGPT-subscription provider. Its bearer is NOT stored here (only this marker
    /// lives in the human-editable providers file) — the OAuth token is persisted separately by
    /// `chatgpt_oauth` and read by the LiteLLM `chatgpt/` provider. `build_config` emits a
    /// `chatgpt/<model>` stanza for it even though `api_key` is empty. Defaults false (old files).
    #[serde(default)]
    pub chatgpt_oauth: bool,
}

impl Provider {
    /// The effective API key: the literal if set, else read from `api_key_env`, else empty.
    pub fn resolved_key(&self) -> String {
        self.resolved_key_with(|e| std::env::var(e).ok())
    }

    /// [resolved_key] with an injectable env lookup — tests pass a closure instead of mutating
    /// the process environment (`env::set_var` races concurrent getenv in parallel tests: UB).
    pub fn resolved_key_with(&self, lookup: impl Fn(&str) -> Option<String>) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        self.api_key_env
            .as_ref()
            .and_then(|e| lookup(e))
            .unwrap_or_default()
    }

    /// Does this provider match a model `hint` — by provider name or model id, case-insensitive and
    /// substring-tolerant (so "deepseek-chat" matches the "deepseek" provider, and the router's
    /// abbreviated "MiniMax" matches "MiniMax-M3")?
    pub fn matches(&self, hint: &str) -> bool {
        let needle = hint.trim();
        if needle.is_empty() {
            return false;
        }
        let h = needle.to_lowercase();
        let name = self.name.to_lowercase();
        let model = self.model.to_lowercase();
        self.name.eq_ignore_ascii_case(needle)
            || self.model.eq_ignore_ascii_case(needle)
            // guard empties: `x.contains("")` is always true, which would mis-match
            || (!name.is_empty() && h.contains(&name))
            || (!model.is_empty() && model.contains(&h))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderRegistry {
    pub providers: Vec<Provider>,
}

impl ProviderRegistry {
    /// Match a hint (the task's `model`) to a provider by name or model id (case-insensitive,
    /// substring-tolerant so "deepseek-chat" matches the "deepseek" provider).
    pub fn find(&self, hint: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.matches(hint))
    }

    /// Load the registry: the JSON file at `AGENTIC_PROVIDERS_FILE` if present + non-empty, else the
    /// env-configured minimax/deepseek defaults (back-compat).
    pub fn load() -> Self {
        let path = providers_file_path();
        if let Some(reg) = Self::from_file(&path) {
            return reg;
        }
        if path.exists() {
            tracing::warn!(
                "[providers] providers file at {} exists but could not be loaded (empty or invalid JSON); using env defaults",
                path.display()
            );
        }
        Self::from_env_defaults()
    }

    /// Load the registry from a JSON file. `None` if the file is missing, unreadable, invalid JSON,
    /// or an empty list. Takes the path directly so callers/tests need no global env var.
    pub fn from_file(path: &Path) -> Option<Self> {
        let providers: Vec<Provider> = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())?;
        if providers.is_empty() {
            return None;
        }
        Some(Self { providers })
    }

    fn from_env_defaults() -> Self {
        let mut providers = Vec::new();
        if let Ok(k) = std::env::var("MINIMAX_API_KEY") {
            providers.push(Provider {
                name: "minimax".into(),
                base_url: std::env::var("MINIMAX_BASE_URL")
                    .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic".into()),
                api_key: k,
                api_key_env: None,
                model: "MiniMax-M3".into(),
                protocol: Protocol::Anthropic,
                capability: 0.5,
                description: Some("cheap, fast general-purpose worker".into()),
                priority: 0.5,
                cost: 0.3,
                router: false,
                enabled: true,
                chatgpt_oauth: false,
            });
        }
        if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
            providers.push(Provider {
                name: "deepseek".into(),
                base_url: std::env::var("DEEPSEEK_BASE_URL")
                    .unwrap_or_else(|_| "https://api.deepseek.com/anthropic".into()),
                api_key: k,
                api_key_env: None,
                model: "deepseek-chat".into(),
                protocol: Protocol::Anthropic,
                capability: 0.6,
                description: Some("cheap coding / reasoning worker".into()),
                priority: 0.5,
                cost: 0.5,
                router: false,
                enabled: true,
                chatgpt_oauth: false,
            });
        }
        Self { providers }
    }
}

/// Resolve an explicit `model_hint` to the registered provider it names (by provider name or model
/// id, case-insensitive). `None` when there is no hint or it matches no registered provider — the
/// caller then either takes the LLM router's pick or falls back to native Claude (see `run_delegate`).
///
/// There is deliberately NO automatic provider fallback here: the smart, task-aware pick lives in
/// `engine::router` (LLM-as-router, reading each provider's `description`/`capability`/`priority`/`cost`), and
/// when that is unavailable the work runs on native Claude Code, not a guessed cheap model.
pub fn route<'a>(model_hint: Option<&str>, reg: &'a ProviderRegistry) -> Option<&'a Provider> {
    let m = model_hint.map(str::trim).filter(|m| !m.is_empty())?;
    reg.find(m)
}

/// Resolve a model/provider `hint` to one of `candidates`. Resolution order:
/// 1. EXACT match (name or model id, case-insensitive);
/// 2. substring match among NATIVE subscription candidates — this keeps a bare family keyword
///    ("opus"/"sonnet"/"haiku"/"fable") bound to the newest native model of that family even when
///    a registered provider's model id (e.g. "claude-3-5-haiku-latest") substring-contains the
///    keyword AND appears earlier in the candidate list; without this pass the registered provider
///    would shadow the native model and run the task on the user's paid endpoint;
/// 3. substring match among the remaining candidates.
pub fn resolve_candidate<'a>(candidates: &[&'a Provider], hint: &str) -> Option<&'a Provider> {
    let h = hint.trim();
    if h.is_empty() {
        return None;
    }
    candidates
        .iter()
        .copied()
        .find(|p| p.name.eq_ignore_ascii_case(h) || p.model.eq_ignore_ascii_case(h))
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|p| is_native(p) && p.matches(h))
        })
        .or_else(|| candidates.iter().copied().find(|p| p.matches(h)))
}

// ── Claude model discovery (Anthropic Models API) ──

/// One Claude model discovered from the Anthropic Models API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeModel {
    /// Full model id, e.g. "claude-opus-4-8".
    pub id: String,
    /// Human-readable name from the API, e.g. "Claude Opus 4.8".
    pub display_name: String,
}

/// All Claude models the account can access, populated once at startup, in API order (newest
/// first). NOT bucketed or filtered by family — whatever the Models API returns is what the
/// model selector offers. Empty when the API is unreachable (no auth, network error) — in that
/// case native Claude models are omitted from the selector.
static CLAUDE_MODELS: OnceLock<Vec<ClaudeModel>> = OnceLock::new();

/// Init the Claude model list from the Anthropic Models API. Call once at startup (own thread).
/// Retries with backoff so a transient boot-time failure (network not up yet, token refresh in
/// flight) doesn't hide the native Claude models until the next restart. On persistent failure
/// (no auth anywhere, endpoint unreachable) the list stays empty — no hardcoded fallback.
pub fn init_claude_models() {
    const BACKOFF_SECS: [u64; 4] = [2, 4, 8, 16];
    let mut models = None;
    for attempt in 0..=BACKOFF_SECS.len() {
        if let Some(m) = fetch_claude_models() {
            models = Some(m);
            break;
        }
        if attempt < BACKOFF_SECS.len() {
            let delay = BACKOFF_SECS[attempt];
            tracing::warn!(
                target: "providers",
                "Claude model discovery attempt {} failed; retrying in {delay}s",
                attempt + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
    }
    let models = models.unwrap_or_default();
    if models.is_empty() {
        tracing::warn!(
            target: "providers",
            "Claude model discovery failed after retries — native Claude models will be \
             missing from the model selector (need ANTHROPIC_AUTH_TOKEN or a Claude Code \
             login credential, and api.anthropic.com reachable)"
        );
    }
    let _ = CLAUDE_MODELS.set(models);
}

/// Beta header value required when authenticating with a Claude subscription OAuth token.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// True when the token is a subscription OAuth access token (`sk-ant-oat...`) rather than an
/// API key (`sk-ant-api...`). OAuth tokens must go on `Authorization: Bearer` with the oauth
/// beta header; sending one via `x-api-key` returns 401.
fn is_oauth_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat")
}

/// Ordered auth candidates for the Models API. `ANTHROPIC_AUTH_TOKEN` first (it may be an API
/// key or an OAuth token; a ccswitch gateway token — the primary production shape, see
/// `title_client` — is not valid against api.anthropic.com and falls through), then the Claude
/// Code subscription credential. Duplicates and empties dropped.
fn claude_token_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
        if !t.is_empty() {
            out.push(t);
        }
    }
    if let Some(t) = oauth_token_from_credentials(&claude_credentials_path()) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The Claude Code credentials file: `$AGENTIC_CLAUDE_CONFIG_BASE/.credentials.json` when the
/// service overrides the config base (mirrors `api::config::claude_config_base`, which native
/// workers use for the subscription login), else `~/.claude/.credentials.json`.
fn claude_credentials_path() -> PathBuf {
    if let Ok(base) = std::env::var("AGENTIC_CLAUDE_CONFIG_BASE") {
        if !base.is_empty() {
            return PathBuf::from(base).join(".credentials.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".claude")
        .join(".credentials.json")
}

/// Parse the OAuth access token out of a Claude Code credentials file. Returns `None` when the
/// file is missing, unparseable, or has an empty token. Path-injected so tests never touch the
/// real `~/.claude`.
fn oauth_token_from_credentials(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let tok = v.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
    if tok.is_empty() {
        return None;
    }
    Some(tok.to_string())
}

/// Fetch the current Claude model list from the Anthropic API, trying each auth candidate in
/// order until one succeeds. Returns `None` when every candidate fails.
fn fetch_claude_models() -> Option<Vec<ClaudeModel>> {
    claude_token_candidates()
        .iter()
        .find_map(|t| fetch_claude_models_with(t))
}

/// One Models API call with one token. Returns `None` on any failure (auth, network, parse).
fn fetch_claude_models_with(token: &str) -> Option<Vec<ClaudeModel>> {
    // Always call the real Anthropic endpoint — not the ccswitch-proxied base URL.
    let url = "https://api.anthropic.com/v1/models";
    let req = reqwest::blocking::Client::new()
        .get(url)
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(10));
    // Header must match the token kind or the endpoint replies 401 (see title_client):
    // subscription OAuth → Bearer + beta header; API key → x-api-key; anything else
    // (ANTHROPIC_AUTH_TOKEN convention) → plain Bearer.
    let req = if is_oauth_token(token) {
        req.header("authorization", format!("Bearer {token}"))
            .header("anthropic-beta", OAUTH_BETA)
    } else if token.starts_with("sk-ant-api") {
        req.header("x-api-key", token)
    } else {
        req.header("authorization", format!("Bearer {token}"))
    };
    let resp = req.send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().ok()?;
    let models = body.get("data")?.as_array()?;

    // Keep EVERY claude-* model the account can access, in API order (newest first).
    // No tier bucketing, no latest-per-family filtering — the selector shows what exists.
    let list: Vec<ClaudeModel> = models
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            if !id.starts_with("claude-") {
                return None;
            }
            let display = m.get("display_name").and_then(|v| v.as_str()).unwrap_or(id);
            Some(ClaudeModel {
                id: id.to_string(),
                display_name: display.to_string(),
            })
        })
        .collect();
    if list.is_empty() {
        return None;
    }
    Some(list)
}

/// The discovered Claude model list (empty when discovery failed or hasn't run).
pub fn native_claude_models() -> &'static [ClaudeModel] {
    CLAUDE_MODELS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Default scheduling priority for a native Claude candidate with no family override.
pub(crate) const DEFAULT_NATIVE_PRIORITY: f32 = 0.5;

/// Family bucket for a Claude model id — the single source of truth for classification.
pub(crate) fn family_of(id: &str) -> &'static str {
    if id.contains("fable") || id.contains("mythos") {
        "fable"
    } else if id.contains("opus") {
        "opus"
    } else if id.contains("sonnet") {
        "sonnet"
    } else if id.contains("haiku") {
        "haiku"
    } else {
        "other"
    }
}

/// Default (capability, cost) for a family — the values `family_metrics` returned before.
pub(crate) fn family_default_metrics(family: &str) -> (f32, f32) {
    match family {
        "fable" => (0.99, 1.0),
        "opus" => (0.97, 0.9),
        "sonnet" => (0.85, 0.5),
        "haiku" => (0.60, 0.3),
        _ => (0.85, 0.6),
    }
}

/// Rough routing metrics `(capability, cost)` per model id. Kept for `native_model_entries`
/// (the `/api/models` picker); now derived from the family classifier.
pub(crate) fn family_metrics(id: &str) -> (f32, f32) {
    family_default_metrics(family_of(id))
}

/// Native Claude candidates for delegate routing: ONE per family (the newest discovered model),
/// with per-family override metrics layered on top of the family defaults. Injecting `overrides`
/// (rather than reading the file here) keeps this hermetic — the only production caller
/// (`delegate.rs`) loads the map at the call boundary; tests pass an empty map.
pub fn native_claude_candidates(overrides: &OverrideMap) -> Vec<Provider> {
    candidates_from(native_claude_models(), overrides)
}

/// Pure core of [native_claude_candidates] — takes the model list explicitly so tests need no
/// global `OnceLock` or file.
fn candidates_from(models: &[ClaudeModel], overrides: &OverrideMap) -> Vec<Provider> {
    let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in models {
        let fam = family_of(&m.id);
        // One routing candidate per family: keep the newest (models are newest-first).
        if !seen.insert(fam) {
            continue;
        }
        let default_desc = format!("Anthropic {} — native (subscription)", m.display_name);
        let (capability, priority, cost, description, enabled) = match overrides.get(fam) {
            Some(o) => (
                o.capability,
                o.priority,
                o.cost,
                if o.description.is_empty() {
                    default_desc
                } else {
                    o.description.clone()
                },
                o.enabled,
            ),
            None => {
                let (c, k) = family_default_metrics(fam);
                (c, DEFAULT_NATIVE_PRIORITY, k, default_desc, true)
            }
        };
        out.push(Provider {
            name: m.id.clone(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: None,
            model: m.id.clone(),
            protocol: Protocol::Anthropic,
            capability,
            description: Some(description),
            priority,
            cost,
            router: false,
            enabled,
            chatgpt_oauth: false,
        });
    }
    out
}

/// True for a native Claude candidate (from `native_claude_candidates`): an empty base_url means it
/// runs on the subscription with no provider env overlay. Also require the model to be one of the
/// DISCOVERED Claude models, so a misconfigured registered provider (empty base_url by mistake) is
/// NOT mistaken for a native subscription model.
pub fn is_native(p: &Provider) -> bool {
    p.base_url.is_empty() && native_claude_models().iter().any(|m| m.id == p.model)
}

/// Build the worker env overlay (`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`) for a provider.
/// Anthropic-protocol providers are called directly; openai-protocol providers are pointed at the
/// local LiteLLM proxy (which translates Anthropic↔OpenAI) — the real key lives in the proxy's env.
pub fn env_overlay(p: &Provider) -> HashMap<String, String> {
    let mut overlay = HashMap::new();
    match p.protocol {
        Protocol::Openai => {
            overlay.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                crate::engine::litellm::proxy_base_url(),
            );
            overlay.insert(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                crate::engine::litellm::PROXY_TOKEN.to_string(),
            );
        }
        Protocol::Anthropic => {
            overlay.insert("ANTHROPIC_BASE_URL".to_string(), p.base_url.clone());
            let key = p.resolved_key();
            if !key.is_empty() {
                overlay.insert("ANTHROPIC_AUTH_TOKEN".to_string(), key);
            }
        }
    }
    overlay
}

// ── file-backed CRUD (the /api/providers routes operate on this) ──

/// Serializes the read-modify-write of the providers file across concurrent API requests
/// (axum runs handlers on a multi-threaded executor).
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Test-only override for [providers_file_path]. A plain Rust static instead of `env::set_var`:
/// setenv racing a concurrent getenv from ANY other test thread is undefined behaviour in glibc
/// (the "process-global set_var race" flake noted on the HTTPS PR) — this override is data-race-free.
/// Always `None` in production.
pub static PROVIDERS_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// The providers JSON file: the test override if set, else `AGENTIC_PROVIDERS_FILE`, else
/// `~/.agentic-dev/providers.json`.
pub fn providers_file_path() -> PathBuf {
    if let Some(p) = PROVIDERS_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_PROVIDERS_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("providers.json")
}

/// Read the provider list from `path`. `Ok([])` when the file is missing; `Err` when it exists but
/// is unreadable or invalid JSON — callers MUST NOT then overwrite it (that would wipe valid data).
pub fn load_list_from(path: &Path) -> std::io::Result<Vec<Provider>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write the provider list to `path` atomically (temp + rename); mode 0600 on unix (it holds keys).
pub fn save_list_to(path: &Path, providers: &[Provider]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(providers).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add or replace (by case-insensitive name) a provider in the file at `path`.
///
/// Key preservation on edit: the API key is write-only (the GET list never returns it), so the
/// Android edit form leaves the key field blank to mean "keep the stored key". When `p` carries no
/// key (empty `api_key` AND no `api_key_env`) and a provider with the same name already exists, the
/// existing key/env is carried over instead of being wiped. Adding a brand-new provider with a blank
/// key still stores it blank (there is nothing to preserve).
pub fn upsert_at(path: &Path, mut p: Provider) -> std::io::Result<()> {
    let _guard = FILE_LOCK.lock();
    let mut list = load_list_from(path)?;
    if let Some(slot) = list
        .iter_mut()
        .find(|x| x.name.eq_ignore_ascii_case(&p.name))
    {
        if p.api_key.is_empty() && p.api_key_env.is_none() {
            // `slot` is fully overwritten by `*slot = p` below, so move the stored key out of it
            // rather than cloning (no extra allocation).
            p.api_key = std::mem::take(&mut slot.api_key);
            p.api_key_env = slot.api_key_env.take();
        }
        *slot = p;
    } else {
        list.push(p);
    }
    save_list_to(path, &list)
}

/// Remove a provider by name from the file at `path`. Returns true if one was removed.
pub fn remove_at(path: &Path, name: &str) -> std::io::Result<bool> {
    let _guard = FILE_LOCK.lock();
    let mut list = load_list_from(path)?;
    let before = list.len();
    list.retain(|x| !x.name.eq_ignore_ascii_case(name));
    let removed = list.len() != before;
    if removed {
        save_list_to(path, &list)?;
    }
    Ok(removed)
}

// Convenience wrappers operating on the configured providers file.
pub fn load_list() -> Vec<Provider> {
    load_list_from(&providers_file_path()).unwrap_or_default()
}
pub fn upsert(p: Provider) -> std::io::Result<()> {
    upsert_at(&providers_file_path(), p)
}
pub fn remove(name: &str) -> std::io::Result<bool> {
    remove_at(&providers_file_path(), name)
}

#[cfg(test)]
pub(crate) fn seed_claude_models_for_tests() {
    let _ = CLAUDE_MODELS.set(vec![
        ClaudeModel {
            id: "claude-fable-5".into(),
            display_name: "Claude Fable 5".into(),
        },
        ClaudeModel {
            id: "claude-opus-4-8".into(),
            display_name: "Claude Opus 4.8".into(),
        },
        ClaudeModel {
            id: "claude-opus-4-7".into(),
            display_name: "Claude Opus 4.7".into(),
        },
        ClaudeModel {
            id: "claude-sonnet-4-6".into(),
            display_name: "Claude Sonnet 4.6".into(),
        },
        ClaudeModel {
            id: "claude-haiku-4-5-20251001".into(),
            display_name: "Claude Haiku 4.5".into(),
        },
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_defaults_true_and_roundtrips() {
        // Old file with no `enabled` field → true (back-compat).
        let p: Provider =
            serde_json::from_str(r#"{"name":"m","base_url":"u","model":"m1"}"#).unwrap();
        assert!(p.enabled);
        // Explicit false round-trips.
        let p2: Provider =
            serde_json::from_str(r#"{"name":"m","base_url":"u","model":"m1","enabled":false}"#)
                .unwrap();
        assert!(!p2.enabled);
        // NativeOverride without `enabled` also defaults true.
        let ov: crate::engine::native_overrides::NativeOverride =
            serde_json::from_str(r#"{"capability":0.6,"priority":0.5,"cost":0.3}"#).unwrap();
        assert!(ov.enabled);
    }

    #[test]
    fn disabled_native_family_yields_disabled_candidate() {
        use crate::engine::native_overrides::{NativeOverride, OverrideMap};
        let models = vec![
            ClaudeModel {
                id: "claude-sonnet-4-6".into(),
                display_name: "S".into(),
            },
            ClaudeModel {
                id: "claude-opus-4-8".into(),
                display_name: "O".into(),
            },
        ];
        let mut ov = OverrideMap::new();
        ov.insert(
            "sonnet".into(),
            NativeOverride {
                capability: 0.85,
                priority: 0.0,
                cost: 0.5,
                description: String::new(),
                enabled: false,
            },
        );
        let c = candidates_from(&models, &ov);
        // The disabled family flows through to the candidate's `enabled` flag, which the delegate
        // candidate filter (`.filter(|p| p.enabled)`) then drops. Un-overridden families stay on.
        let sonnet = c.iter().find(|p| p.model.contains("sonnet")).unwrap();
        assert!(!sonnet.enabled, "disabled family → disabled candidate");
        let opus = c.iter().find(|p| p.model.contains("opus")).unwrap();
        assert!(opus.enabled, "un-overridden family stays enabled");
    }

    #[test]
    fn family_of_classifies_and_metrics_are_unchanged() {
        assert_eq!(family_of("claude-opus-4-8"), "opus");
        assert_eq!(family_of("claude-sonnet-4-6"), "sonnet");
        assert_eq!(family_of("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(family_of("claude-fable-5"), "fable");
        assert_eq!(family_of("claude-mythos-1"), "fable");
        assert_eq!(family_of("claude-3-5-something"), "other");
        // family_metrics must return exactly what it returned before the refactor
        assert_eq!(family_metrics("claude-opus-4-8"), (0.97, 0.9));
        assert_eq!(family_metrics("claude-fable-5"), (0.99, 1.0));
        assert_eq!(family_metrics("claude-sonnet-4-6"), (0.85, 0.5));
        assert_eq!(family_metrics("claude-haiku-4-5"), (0.60, 0.3));
        assert_eq!(family_metrics("claude-weird-9"), (0.85, 0.6));
        assert_eq!(DEFAULT_NATIVE_PRIORITY, 0.5);
    }

    #[test]
    fn candidates_collapse_to_newest_and_inherit_family_override() {
        // `OverrideMap` is already in scope via `use super::*` (module-level import from Step 3);
        // only `NativeOverride` needs importing here.
        use crate::engine::native_overrides::NativeOverride;
        // newest-first, two opus siblings + one haiku
        let models = vec![
            ClaudeModel {
                id: "claude-opus-4-9".into(),
                display_name: "Claude Opus 4.9".into(),
            },
            ClaudeModel {
                id: "claude-opus-4-8".into(),
                display_name: "Claude Opus 4.8".into(),
            },
            ClaudeModel {
                id: "claude-haiku-5".into(),
                display_name: "Claude Haiku 5".into(),
            },
        ];
        let mut ov = OverrideMap::new();
        ov.insert(
            "opus".into(),
            NativeOverride {
                capability: 0.9,
                priority: 0.85,
                cost: 0.2,
                description: String::new(),
                enabled: true,
            },
        );

        let c = candidates_from(&models, &ov);
        // opus collapses to the NEWEST (4-9); haiku stays → 2 candidates
        assert_eq!(c.len(), 2);
        assert!(c.iter().any(|p| p.model == "claude-opus-4-9"));
        assert!(!c.iter().any(|p| p.model == "claude-opus-4-8"));
        // the opus family override is inherited by the NEW id (family keying)
        let opus = c.iter().find(|p| p.model == "claude-opus-4-9").unwrap();
        assert!((opus.priority - 0.85).abs() < f32::EPSILON);
        assert!((opus.capability - 0.9).abs() < f32::EPSILON);
        assert!((opus.cost - 0.2).abs() < f32::EPSILON);
        // empty override description → generated per-model description
        assert_eq!(
            opus.description.as_deref(),
            Some("Anthropic Claude Opus 4.9 — native (subscription)")
        );
        // non-overridden family keeps defaults + priority 0.5
        let haiku = c.iter().find(|p| p.model == "claude-haiku-5").unwrap();
        assert!((haiku.priority - DEFAULT_NATIVE_PRIORITY).abs() < f32::EPSILON);
        assert_eq!(
            (haiku.capability, haiku.cost),
            family_default_metrics("haiku")
        );
    }

    #[test]
    fn oauth_token_detection() {
        // Subscription OAuth tokens → Bearer path; API keys and junk → x-api-key path.
        assert!(is_oauth_token("sk-ant-oat01-abcdef"));
        assert!(!is_oauth_token("sk-ant-api03-abcdef"));
        assert!(!is_oauth_token(""));
        assert!(!is_oauth_token("some-random-token"));
    }

    #[test]
    fn oauth_credentials_file_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");

        // Missing file → None
        assert_eq!(oauth_token_from_credentials(&path), None);

        // Valid Claude Code shape → token
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-xyz","refreshToken":"r","expiresAt":1}}"#,
        )
        .unwrap();
        assert_eq!(
            oauth_token_from_credentials(&path).as_deref(),
            Some("sk-ant-oat01-xyz")
        );

        // Empty token → None (treat as no auth rather than sending a blank header)
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":""}}"#).unwrap();
        assert_eq!(oauth_token_from_credentials(&path), None);

        // Wrong shape / invalid JSON → None
        std::fs::write(&path, r#"{"other":{}}"#).unwrap();
        assert_eq!(oauth_token_from_credentials(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(oauth_token_from_credentials(&path), None);
    }

    fn reg() -> ProviderRegistry {
        ProviderRegistry {
            providers: vec![
                Provider {
                    name: "minimax".into(),
                    base_url: "https://mm/anthropic".into(),
                    api_key: "mk".into(),
                    api_key_env: None,
                    model: "MiniMax-M3".into(),
                    protocol: Protocol::Anthropic,
                    capability: 0.5,
                    description: None,
                    priority: 0.3,
                    cost: 0.5,
                    router: false,
                    enabled: true,
                    chatgpt_oauth: false,
                },
                Provider {
                    name: "deepseek".into(),
                    base_url: "https://ds/anthropic".into(),
                    api_key: "dk".into(),
                    api_key_env: None,
                    model: "deepseek-chat".into(),
                    protocol: Protocol::Anthropic,
                    capability: 0.6,
                    description: None,
                    priority: 0.5,
                    cost: 0.5,
                    router: false,
                    enabled: true,
                    chatgpt_oauth: false,
                },
                Provider {
                    name: "opus".into(),
                    base_url: "https://an/anthropic".into(),
                    api_key: "ak".into(),
                    api_key_env: None,
                    model: "claude-opus-4-5".into(),
                    protocol: Protocol::Anthropic,
                    capability: 0.95,
                    description: None,
                    priority: 0.9,
                    cost: 0.5,
                    router: false,
                    enabled: true,
                    chatgpt_oauth: false,
                },
            ],
        }
    }

    #[test]
    fn explicit_hint_wins_by_name_or_model_substring() {
        let r = reg();
        assert_eq!(route(Some("deepseek"), &r).unwrap().name, "deepseek");
        // the worker model id "deepseek-chat" still resolves to the deepseek provider
        assert_eq!(route(Some("deepseek-chat"), &r).unwrap().name, "deepseek");
        assert_eq!(route(Some("opus"), &r).unwrap().name, "opus");
    }

    fn seed_models() {
        super::seed_claude_models_for_tests();
    }

    #[test]
    fn native_claude_candidates_compete_as_routing_candidates() {
        seed_models();
        let c = native_claude_candidates(&Default::default());
        // one candidate per family (newest): fable, opus, sonnet, haiku.
        assert_eq!(c.len(), 4);
        // all native: no base_url / key → run on the subscription with no provider overlay
        assert!(c
            .iter()
            .all(|p| is_native(p) && p.base_url.is_empty() && p.resolved_key().is_empty()));
        // full model ids, not tier keywords
        assert!(c.iter().any(|p| p.model == "claude-opus-4-8"));
        assert!(!c.iter().any(|p| p.model == "claude-opus-4-7"));
        assert!(c.iter().any(|p| p.model == "claude-fable-5"));
        // substring-tolerant matching resolves the router's pick to a native candidate
        assert!(c.iter().any(|p| p.matches("sonnet")));
        assert!(c.iter().any(|p| p.matches("claude-haiku")));
        // family metrics: fable above opus above sonnet above haiku on capability
        let cap = |needle: &str| {
            c.iter()
                .find(|p| p.model.contains(needle))
                .unwrap()
                .capability
        };
        assert!(
            cap("fable") > cap("opus")
                && cap("opus") > cap("sonnet")
                && cap("sonnet") > cap("haiku")
        );
        // a registered provider is NOT native
        let reg_p = reg().providers.into_iter().next().unwrap();
        assert!(!is_native(&reg_p));
    }

    #[test]
    fn no_hint_or_unknown_hint_returns_none() {
        let r = reg();
        // No explicit hint → None (the caller routes via the LLM router or falls back to native Claude).
        assert!(route(None, &r).is_none());
        assert!(route(Some("  "), &r).is_none());
        // An explicit hint that names no registered provider → None (runs as a native Claude override).
        assert!(route(Some("gpt-5"), &r).is_none());
    }

    #[test]
    fn resolve_candidate_prefers_exact_over_substring() {
        seed_models();
        // A registered anthropic provider whose model id substring-contains a family keyword must
        // NOT shadow the native candidate when the hint is the bare keyword (else a router pick of
        // "haiku" would run on the user's paid api.anthropic.com endpoint instead of the subscription).
        let registered = Provider {
            name: "claude-api".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "k".into(),
            api_key_env: None,
            model: "claude-3-5-haiku-latest".into(),
            protocol: Protocol::Anthropic,
            capability: 0.5,
            description: None,
            priority: 0.5,
            cost: 0.5,
            router: false,
            enabled: true,
            chatgpt_oauth: false,
        };
        let native = native_claude_candidates(&Default::default());
        // candidate order mirrors run_delegate: registered FIRST, then native.
        let cands: Vec<&Provider> = std::iter::once(&registered).chain(native.iter()).collect();
        // bare "haiku" → the NATIVE haiku (newest discovered), not the substring-matching registered one.
        let got = resolve_candidate(&cands, "haiku").expect("resolves");
        assert!(
            is_native(got),
            "bare family keyword must bind to the native candidate, got '{}'",
            got.name
        );
        assert_eq!(got.model, "claude-haiku-4-5-20251001");
        // bare "opus" → the newest native opus (API order), not an older sibling.
        assert_eq!(
            resolve_candidate(&cands, "opus").unwrap().model,
            "claude-opus-4-8"
        );
        // bare "fable" → the native fable candidate (family keywords generalize beyond 3 tiers).
        assert_eq!(
            resolve_candidate(&cands, "fable").unwrap().model,
            "claude-fable-5"
        );
        // the registered provider's full model id still resolves to IT (exact match wins).
        assert_eq!(
            resolve_candidate(&cands, "claude-3-5-haiku-latest")
                .unwrap()
                .name,
            "claude-api"
        );
        // a bare/empty hint resolves to nothing.
        assert!(resolve_candidate(&cands, "   ").is_none());
    }

    #[test]
    fn empty_registry_routes_to_none() {
        let r = ProviderRegistry::default();
        assert!(route(Some("minimax"), &r).is_none());
        assert!(route(None, &r).is_none());
    }

    #[test]
    fn loads_providers_from_json_file() {
        let dir = std::env::temp_dir().join(format!(
            "agentic-prov-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("providers.json");
        std::fs::write(&f, serde_json::json!([
            {"name":"kimi","base_url":"https://kimi/anthropic","api_key":"kk","model":"kimi-k2","tier":"cheap"},
            {"name":"sonnet","base_url":"https://an/anthropic","api_key_env":"X_PROVIDERS_TEST_KEY","model":"claude-sonnet-4-5","tier":"strong"}
        ]).to_string()).unwrap();
        // from_file takes the path directly — no global AGENTIC_PROVIDERS_FILE env (avoids
        // cross-test coupling). The api_key_env path is exercised through resolved_key_with an
        // injected lookup instead of env::set_var — setenv racing any concurrent getenv on other
        // test threads is UB (the flake the HTTPS PR called the "process-global set_var race").
        let r = ProviderRegistry::from_file(&f).expect("file loads");
        assert_eq!(r.providers.len(), 2);
        assert_eq!(r.find("kimi").unwrap().model, "kimi-k2");
        let lookup = |k: &str| (k == "X_PROVIDERS_TEST_KEY").then(|| "from-env".to_string());
        assert_eq!(
            r.find("sonnet").unwrap().resolved_key_with(lookup),
            "from-env"
        );
        // resolved_key() (the real-env variant) falls back to empty when the env var is absent.
        assert_eq!(r.find("sonnet").unwrap().resolved_key(), "");
        // old file without a `capability` field → defaults to 0.5 (back-compat), `tier` ignored
        assert_eq!(r.find("sonnet").unwrap().capability, 0.5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crud_roundtrip_on_a_file() {
        let dir = std::env::temp_dir().join(format!(
            "agentic-crud-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("providers.json");
        let mk = |n: &str, m: &str| Provider {
            name: n.into(),
            base_url: "https://x/anthropic".into(),
            api_key: "k".into(),
            api_key_env: None,
            model: m.into(),
            protocol: Protocol::Anthropic,
            capability: 0.5,
            description: None,
            priority: 0.5,
            cost: 0.5,
            router: false,
            enabled: true,
            chatgpt_oauth: false,
        };
        assert!(load_list_from(&f).unwrap().is_empty());
        upsert_at(&f, mk("minimax", "MiniMax-M3")).unwrap();
        upsert_at(&f, mk("deepseek", "deepseek-chat")).unwrap();
        assert_eq!(load_list_from(&f).unwrap().len(), 2);
        // upsert by same name replaces (case-insensitive), not duplicates
        upsert_at(&f, mk("MiniMax", "MiniMax-M2")).unwrap();
        let list = load_list_from(&f).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list.iter()
                .find(|x| x.name.eq_ignore_ascii_case("minimax"))
                .unwrap()
                .model,
            "MiniMax-M2"
        );
        // remove (case-insensitive); missing → false
        assert!(remove_at(&f, "MINIMAX").unwrap());
        assert_eq!(load_list_from(&f).unwrap().len(), 1);
        assert!(!remove_at(&f, "nope").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_preserves_key_when_blank_on_edit() {
        // The key is write-only, so the edit form leaves it blank to mean "keep the stored key".
        // A blank key on an EDIT must preserve the existing credential, not wipe it; a non-blank key
        // replaces it; and a blank key on a brand-NEW provider has nothing to preserve.
        let dir = std::env::temp_dir().join(format!(
            "agentic-preserve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("providers.json");
        let mk = |key: &str, model: &str, cap: f32| Provider {
            name: "minimax".into(),
            base_url: "https://x/anthropic".into(),
            api_key: key.into(),
            api_key_env: None,
            model: model.into(),
            protocol: Protocol::Anthropic,
            capability: cap,
            description: None,
            priority: 0.5,
            cost: 0.5,
            router: false,
            enabled: true,
            chatgpt_oauth: false,
        };
        // seed with a real key
        upsert_at(&f, mk("secret-key", "MiniMax-M3", 0.5)).unwrap();
        // edit other fields with a BLANK key → key preserved, the other field still updates
        upsert_at(&f, mk("", "MiniMax-M2", 0.8)).unwrap();
        let after_edit = load_list_from(&f).unwrap();
        let p = after_edit.iter().find(|x| x.name == "minimax").unwrap();
        assert_eq!(
            p.resolved_key(),
            "secret-key",
            "blank key on edit must keep the stored key"
        );
        assert_eq!(p.model, "MiniMax-M2", "other fields still update");
        assert!((p.capability - 0.8).abs() < f32::EPSILON);
        // edit with a NEW non-blank key → replaces
        upsert_at(&f, mk("rotated-key", "MiniMax-M2", 0.8)).unwrap();
        assert_eq!(
            load_list_from(&f)
                .unwrap()
                .iter()
                .find(|x| x.name == "minimax")
                .unwrap()
                .resolved_key(),
            "rotated-key"
        );
        // a brand-new provider with a blank key has nothing to preserve → stays blank
        upsert_at(
            &f,
            Provider {
                name: "fresh".into(),
                base_url: "https://y/anthropic".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "fresh-1".into(),
                protocol: Protocol::Anthropic,
                capability: 0.5,
                description: None,
                priority: 0.5,
                cost: 0.5,
                router: false,
                enabled: true,
                chatgpt_oauth: false,
            },
        )
        .unwrap();
        assert_eq!(
            load_list_from(&f)
                .unwrap()
                .iter()
                .find(|x| x.name == "fresh")
                .unwrap()
                .resolved_key(),
            ""
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_errors_instead_of_wiping_providers() {
        let dir = std::env::temp_dir().join(format!(
            "agentic-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("providers.json");
        upsert_at(
            &f,
            Provider {
                name: "minimax".into(),
                base_url: "https://x/anthropic".into(),
                api_key: "k".into(),
                api_key_env: None,
                model: "MiniMax-M3".into(),
                protocol: Protocol::Anthropic,
                capability: 0.5,
                description: None,
                priority: 0.5,
                cost: 0.5,
                router: false,
                enabled: true,
                chatgpt_oauth: false,
            },
        )
        .unwrap();
        // the file gets corrupted (a bad manual edit, a partial write, ...)
        std::fs::write(&f, "{ this is : not json").unwrap();
        // CRUD must ERROR rather than read an empty list and overwrite it (which would wipe data).
        assert!(load_list_from(&f).is_err());
        assert!(remove_at(&f, "minimax").is_err());
        assert!(upsert_at(
            &f,
            Provider {
                name: "x".into(),
                base_url: "y".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "z".into(),
                protocol: Protocol::Anthropic,
                capability: 0.5,
                description: None,
                priority: 0.5,
                cost: 0.5,
                router: false,
                enabled: true,
                chatgpt_oauth: false,
            }
        )
        .is_err());
        // the corrupt file is left intact, NOT wiped to "[]"
        assert!(std::fs::read_to_string(&f).unwrap().contains("not json"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
