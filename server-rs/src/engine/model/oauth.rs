//! ChatGPT-subscription OAuth: let a user connect their paid ChatGPT account (OAuth login, no API
//! key) so GPT enters the delegate routing pool.
//!
//! The credential is an OAuth 2.0 Authorization-Code + PKCE(S256) token (the same public client the
//! Codex CLI uses). Unlike an API key it ROTATES: the access token is a short-lived JWT refreshed
//! with a refresh token. We keep it OUT of the human-editable providers file — it lives in a
//! separate 0600 secrets store (`~/.agentic-dev/oauth-tokens.json`) keyed by provider name. A
//! provider "is" a subscription provider iff the store holds a token under its name; no schema
//! change to `Provider` is needed (`resolved_key` falls back to the store, `litellm::build_config`
//! adds the Codex headers when a token is present).
//!
//! Endpoints/headers are the Codex-CLI set (mid-2026): authorize/token at auth.openai.com, calls to
//! chatgpt.com/backend-api/codex/responses with an account-id header taken from the JWT.
//!
//! This module is axum-free (engine layer) — the HTTP handlers live in `api::misc`.

#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ── constants (Codex CLI public client) ──
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPES: &str = "openid profile email offline_access";
/// The loopback the fixed redirect points at; bound only while a login is in flight.
pub const CALLBACK_ADDR: &str = "127.0.0.1:1455";

/// The ChatGPT backend the worker/proxy actually calls. Stored as the provider's `base_url`.
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const ORIGINATOR: &str = "codex_cli_rs";
pub const OPENAI_BETA: &str = "responses=experimental";

/// Default provider name/model for the single subscription provider.
pub const DEFAULT_PROVIDER: &str = "gpt";
pub const DEFAULT_MODEL: &str = "gpt-5-codex";

/// Refresh this many seconds before the token's `expires_at`.
const REFRESH_BUFFER_SECS: i64 = 60;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

// ── token store (secrets, 0600) ──

/// One persisted subscription credential.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubscriptionToken {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(default)]
    pub account_email: Option<String>,
    /// Unix seconds when the access token expires.
    pub expires_at: i64,
    /// Set when a refresh failed irrecoverably (invalid_grant) → user must log in again.
    #[serde(default)]
    pub needs_reauth: bool,
}

impl SubscriptionToken {
    fn is_expiring(&self, now: i64) -> bool {
        self.expires_at != 0 && now >= self.expires_at - REFRESH_BUFFER_SECS
    }
}

/// Test override for the store path (data-race-free, mirrors providers.rs). Always `None` in prod.
pub static TOKENS_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// The store override is process-global; any test that swaps it (here OR in litellm's config test)
/// must hold this lock for its duration so parallel tests don't stomp each other's temp path.
#[cfg(test)]
pub(crate) static TEST_STORE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Serializes read-modify-write of the store across threads.
static STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn tokens_file_path() -> PathBuf {
    if let Some(p) = TOKENS_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_OAUTH_TOKENS_FILE") {
        return PathBuf::from(p);
    }
    home().join(".agentic-dev").join("oauth-tokens.json")
}

type Store = std::collections::HashMap<String, SubscriptionToken>;

fn load_store_from(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the store atomically (temp + rename), mode 0600 (it holds tokens).
fn save_store_to(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(store).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // This file holds OAuth tokens; if we can't lock it down, say so loudly rather than leave a
        // world-readable secret silently (umask would otherwise make it 0644).
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("[oauth] chmod 600 on token store failed: {e}");
        }
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Full stored record for a provider (ignores `needs_reauth`; used for headers/status/account).
pub fn subscription_token(name: &str) -> Option<SubscriptionToken> {
    load_store_from(&tokens_file_path()).remove(name)
}

/// The current access token usable as a bearer — `None` if absent, empty, or needs re-login.
/// `providers::Provider::resolved_key` falls back to this for subscription providers.
pub fn access_token_for(name: &str) -> Option<String> {
    let t = subscription_token(name)?;
    if t.needs_reauth || t.access_token.is_empty() {
        return None;
    }
    Some(t.access_token)
}

/// Persist (insert/replace) a token under `name`.
pub fn put_token(name: &str, token: SubscriptionToken) -> std::io::Result<()> {
    let _g = STORE_LOCK.lock();
    let path = tokens_file_path();
    let mut store = load_store_from(&path);
    store.insert(name.to_string(), token);
    save_store_to(&path, &store)
}

/// Remove a token; `Ok(true)` if one was present.
pub fn remove_token(name: &str) -> std::io::Result<bool> {
    let _g = STORE_LOCK.lock();
    let path = tokens_file_path();
    let mut store = load_store_from(&path);
    let existed = store.remove(name).is_some();
    if existed {
        save_store_to(&path, &store)?;
    }
    Ok(existed)
}

// ── PKCE + authorize URL ──

/// A 32-byte PKCE `code_verifier`, base64url (no pad). Uses two v4 UUIDs for entropy (uuid is
/// already a dependency — no need to add `rand`).
pub fn code_verifier() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256 challenge: `base64url_nopad(sha256(verifier))`.
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A random CSRF `state`.
pub fn gen_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn urlencode(s: &str) -> String {
    // Minimal application/x-www-form component encoding for URL query values.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn authorize_url(state: &str, challenge: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={cid}&redirect_uri={redir}&scope={scope}&code_challenge={ch}&code_challenge_method=S256&state={state}",
        cid = urlencode(CLIENT_ID),
        redir = urlencode(REDIRECT_URI),
        scope = urlencode(SCOPES),
        ch = urlencode(challenge),
        state = urlencode(state),
    )
}

// ── JWT claim extraction (no signature check — token came from the token endpoint over TLS and is
//    used only as an opaque bearer; we just read two claims) ──

/// Decode a JWT's payload segment to JSON.
fn jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let seg = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(seg).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Recursively find the first string value for `key` anywhere in the JSON (OpenAI nests
/// `chatgpt_account_id` under the `https://api.openai.com/auth` namespaced claim).
fn find_str(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get(key) {
                return Some(s.clone());
            }
            map.values().find_map(|child| find_str(child, key))
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(|child| find_str(child, key)),
        _ => None,
    }
}

/// `(account_id, email)` from any of the provided JWTs (access token, then id token).
pub fn claims_from(jwts: &[&str]) -> (Option<String>, Option<String>) {
    let mut account = None;
    let mut email = None;
    for jwt in jwts {
        if let Some(p) = jwt_payload(jwt) {
            account = account.or_else(|| find_str(&p, "chatgpt_account_id"));
            email = email.or_else(|| find_str(&p, "email"));
        }
    }
    (account, email)
}

// ── token endpoint (exchange / refresh) ──

/// Raw token endpoint JSON.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Build a `SubscriptionToken` from a token endpoint JSON body. Pure (no network) so it is unit
/// testable. `prev_refresh` is carried over when the response omits a new refresh token (refresh
/// responses often do). `now` is injected for deterministic expiry.
pub fn token_from_json(
    body: &str,
    prev_refresh: &str,
    now: i64,
) -> Result<SubscriptionToken, String> {
    let r: TokenResponse =
        serde_json::from_str(body).map_err(|e| format!("bad token response: {e}"))?;
    if r.access_token.is_empty() {
        return Err("token response had empty access_token".into());
    }
    let id = r.id_token.clone().unwrap_or_default();
    let (account_id, email) = claims_from(&[&r.access_token, &id]);
    let account_id = account_id.ok_or("token JWT missing chatgpt_account_id claim")?;
    let refresh_token = match r.refresh_token {
        Some(rt) if !rt.is_empty() => rt,
        _ => prev_refresh.to_string(),
    };
    // Prefer the response's expires_in; else fall back to the JWT `exp`; else 1h.
    let expires_at = r
        .expires_in
        .map(|s| now + s)
        .or_else(|| {
            jwt_payload(&r.access_token)
                .and_then(|p| p.get("exp").and_then(|e| e.as_i64()))
        })
        .unwrap_or(now + 3600);
    Ok(SubscriptionToken {
        access_token: r.access_token,
        refresh_token,
        account_id,
        account_email: email,
        expires_at,
        needs_reauth: false,
    })
}

fn post_token_form(form: &[(&str, &str)]) -> Result<String, String> {
    let resp = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .form(form)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("token endpoint {status}: {body}"));
    }
    Ok(body)
}

/// Exchange an authorization `code` (+ PKCE verifier) for a token.
pub fn exchange_code(code: &str, verifier: &str) -> Result<SubscriptionToken, String> {
    let body = post_token_form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])?;
    token_from_json(&body, "", now_unix())
}

/// Refresh with a stored refresh token.
pub fn refresh(refresh_token: &str) -> Result<SubscriptionToken, String> {
    let body = post_token_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", SCOPES),
    ])?;
    token_from_json(&body, refresh_token, now_unix())
}

// ── register a subscription provider from a fresh token ──

/// Persist the token and upsert the subscription provider (openai protocol, codex base_url, no key
/// in the providers file), then reload the proxy so routing picks up the model.
pub fn register_from_token(name: &str, token: SubscriptionToken) -> Result<(), String> {
    put_token(name, token).map_err(|e| format!("persist token: {e}"))?;
    let provider = crate::engine::providers::Provider {
        name: name.to_string(),
        base_url: CODEX_BASE_URL.to_string(),
        api_key: String::new(),
        api_key_env: None,
        model: DEFAULT_MODEL.to_string(),
        protocol: crate::engine::providers::Protocol::Openai,
        capability: 0.9,
        description: Some("ChatGPT subscription (GPT) — OAuth, routed via delegate".into()),
        priority: 0.5,
        cost: 0.6,
        router: false,
        enabled: true,
    };
    crate::engine::providers::upsert(provider).map_err(|e| format!("upsert provider: {e}"))?;
    crate::engine::litellm::request_reload();
    Ok(())
}

// ── connection status (for the status endpoint / UI) ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    NotConnected,
    Pending,
    Connected {
        account_email: Option<String>,
        expires_at: i64,
    },
    NeedsReauth,
}

pub fn status(name: &str) -> ConnectionStatus {
    if PENDING.lock().as_ref().map(|p| p.provider_name.as_str()) == Some(name) {
        return ConnectionStatus::Pending;
    }
    match subscription_token(name) {
        None => ConnectionStatus::NotConnected,
        Some(t) if t.needs_reauth => ConnectionStatus::NeedsReauth,
        Some(t) => ConnectionStatus::Connected {
            account_email: t.account_email,
            expires_at: t.expires_at,
        },
    }
}

// ── pending login + one-shot loopback callback listener ──

pub struct PendingLogin {
    pub state: String,
    pub verifier: String,
    pub provider_name: String,
    pub created_at: i64,
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);
/// Holds the one-shot callback listener so `cancel_pending` can DROP it — dropping frees the loopback
/// port synchronously, so an immediate retry can rebind. (A poll-based drop in the thread would leave
/// a ~200ms window where the port is still bound and a same-call retry fails.)
static LISTENER: Mutex<Option<std::net::TcpListener>> = Mutex::new(None);

/// Overall time a login may stay pending before the listener gives up.
const LOGIN_TIMEOUT_SECS: u64 = 300;

/// Start an OAuth login: generate PKCE + state, bind the one-shot loopback listener, and return the
/// authorize URL for the client to open. The listener runs on its own thread and, on callback,
/// validates `state`, exchanges the code, and registers the provider.
pub fn start_login(provider_name: &str) -> Result<String, String> {
    let listener = std::net::TcpListener::bind(CALLBACK_ADDR)
        .map_err(|e| format!("cannot bind {CALLBACK_ADDR} (login already in progress?): {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener nonblocking: {e}"))?;

    let verifier = code_verifier();
    let state = gen_state();
    let challenge = code_challenge(&verifier);
    let url = authorize_url(&state, &challenge);

    *PENDING.lock() = Some(PendingLogin {
        state: state.clone(),
        verifier: verifier.clone(),
        provider_name: provider_name.to_string(),
        created_at: now_unix(),
    });
    *LISTENER.lock() = Some(listener);

    std::thread::spawn(run_callback_listener);
    Ok(url)
}

/// Cancel any in-flight login (used by logout / re-login). Dropping the listener frees port 1455 so
/// the next `start_login` can bind immediately.
pub fn cancel_pending() {
    *PENDING.lock() = None;
    *LISTENER.lock() = None;
}

/// Accept a single callback connection (with a timeout), handle it, then exit. The listener lives in
/// the `LISTENER` static so `cancel_pending` can free the port; each iteration re-checks it and bails
/// if it's gone (cancelled).
fn run_callback_listener() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS);
    loop {
        if std::time::Instant::now() >= deadline {
            tracing::warn!("[oauth] login callback timed out; clearing pending");
            cancel_pending();
            return;
        }
        // Nonblocking accept under the lock (returns immediately), so cancel_pending never blocks.
        let accepted = {
            let guard = LISTENER.lock();
            match guard.as_ref() {
                None => return, // cancelled — listener already dropped
                Some(l) => l.accept(),
            }
        };
        match accepted {
            Ok((stream, _)) => {
                handle_callback(stream);
                cancel_pending();
                return;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                tracing::warn!("[oauth] callback accept error: {e}");
                cancel_pending();
                return;
            }
        }
    }
}

/// Parse the callback request, exchange the code, register the provider, and reply with a small page.
fn handle_callback(mut stream: std::net::TcpStream) {
    // The accepted socket is blocking regardless of the listener's nonblocking flag; a stalled client
    // that connects but never sends the request line would otherwise hang this read (and the port)
    // forever. Cap it.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    // First line: "GET /auth/callback?code=...&state=... HTTP/1.1"
    let target = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    let (code, state) = parse_callback_query(target);

    let result = finish_login(code.as_deref(), state.as_deref());
    let (title, msg) = match &result {
        Ok(_) => ("Connected", "ChatGPT connected. You can close this tab."),
        Err(e) => {
            tracing::warn!("[oauth] login failed: {e}");
            ("Login failed", "ChatGPT login failed. You can close this tab and retry.")
        }
    };
    let html = format!("<!doctype html><meta charset=utf-8><title>{title}</title><body style=\"font-family:sans-serif\"><h3>{title}</h3><p>{msg}</p></body>");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.flush();
}

/// Extract `code` and `state` from a `/auth/callback?...` request target.
pub fn parse_callback_query(target: &str) -> (Option<String>, Option<String>) {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = urldecode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
                _ => {}
            }
        }
    }
    (code, state)
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Validate state, exchange the code, register the provider. Clears the pending login.
fn finish_login(code: Option<&str>, state: Option<&str>) -> Result<(), String> {
    let pending = PENDING.lock().take().ok_or("no login in progress")?;
    let code = code.ok_or("callback missing code")?;
    let state = state.ok_or("callback missing state")?;
    if state != pending.state {
        return Err("callback state mismatch".into());
    }
    let token = exchange_code(code, &pending.verifier)?;
    register_from_token(&pending.provider_name, token)
}

// ── background refresher ──

/// Refresh any token within the expiry buffer. `do_refresh` is injectable for tests. Returns the
/// number of tokens refreshed. On invalid refresh, marks the token `needs_reauth`.
pub fn run_refresh_once(
    now: i64,
    do_refresh: impl Fn(&str) -> Result<SubscriptionToken, String>,
) -> usize {
    let path = tokens_file_path();
    let store = load_store_from(&path);
    let mut refreshed = 0;
    for (name, tok) in store.iter() {
        if tok.needs_reauth || !tok.is_expiring(now) {
            continue;
        }
        match do_refresh(&tok.refresh_token) {
            Ok(new) => {
                if put_token(name, new).is_ok() {
                    refreshed += 1;
                }
            }
            Err(e) => {
                tracing::warn!("[oauth] refresh for {name} failed: {e}; marking needs_reauth");
                let mut dead = tok.clone();
                dead.needs_reauth = true;
                let _ = put_token(name, dead);
            }
        }
    }
    if refreshed > 0 {
        crate::engine::litellm::request_reload();
    }
    refreshed
}

/// Spawn the refresher loop. Wakes near the soonest expiry (or every 5 min when idle).
pub fn start_refresher() {
    std::thread::spawn(|| loop {
        // A refresh persists the new bearer and requests a proxy reload inside run_refresh_once;
        // GPT is served live from the provider list, so nothing else to poke here.
        let _ = run_refresh_once(now_unix(), |rt| refresh(rt));
        // Sleep until just before the soonest expiry, clamped to [30s, 5min].
        let now = now_unix();
        let soonest = load_store_from(&tokens_file_path())
            .values()
            .filter(|t| !t.needs_reauth && t.expires_at != 0)
            .map(|t| t.expires_at - REFRESH_BUFFER_SECS)
            .min();
        let sleep_secs = match soonest {
            Some(t) => (t - now).clamp(30, 300),
            None => 300,
        };
        std::thread::sleep(std::time::Duration::from_secs(sleep_secs as u64));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_store<T>(f: impl FnOnce() -> T) -> T {
        let _serial = TEST_STORE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("oauth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        *TOKENS_FILE_OVERRIDE.lock() = Some(dir.join("oauth-tokens.json"));
        let out = f();
        *TOKENS_FILE_OVERRIDE.lock() = None;
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn pkce_challenge_matches_sha256_b64url() {
        // Known RFC 7636 test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_is_43_chars_base64url() {
        let v = code_verifier();
        assert_eq!(v.len(), 43); // 32 bytes → 43 base64url chars (no pad)
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    fn b64url(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    /// Build a fake unsigned JWT with the given JSON payload.
    fn fake_jwt(payload: &str) -> String {
        format!("{}.{}.{}", b64url("{\"alg\":\"none\"}"), b64url(payload), "sig")
    }

    #[test]
    fn extracts_nested_account_id_and_email() {
        let jwt = fake_jwt(
            r#"{"email":"u@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct_123"}}"#,
        );
        let (acct, email) = claims_from(&[&jwt]);
        assert_eq!(acct.as_deref(), Some("acct_123"));
        assert_eq!(email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn token_from_json_uses_expires_in_and_prev_refresh() {
        let jwt = fake_jwt(r#"{"chatgpt_account_id":"acct_9"}"#);
        let body = format!(r#"{{"access_token":"{jwt}","expires_in":3600}}"#);
        let t = token_from_json(&body, "keep-me", 1000).unwrap();
        assert_eq!(t.account_id, "acct_9");
        assert_eq!(t.expires_at, 4600);
        assert_eq!(t.refresh_token, "keep-me"); // response omitted one → carried over
        assert!(!t.needs_reauth);
    }

    #[test]
    fn token_from_json_rejects_missing_account() {
        let jwt = fake_jwt(r#"{"sub":"x"}"#);
        let body = format!(r#"{{"access_token":"{jwt}"}}"#);
        assert!(token_from_json(&body, "", 0).is_err());
    }

    #[test]
    fn store_round_trip_and_access_gating() {
        with_temp_store(|| {
            assert!(access_token_for("gpt").is_none());
            let jwt = fake_jwt(r#"{"chatgpt_account_id":"a"}"#);
            put_token(
                "gpt",
                SubscriptionToken {
                    access_token: jwt.clone(),
                    refresh_token: "r".into(),
                    account_id: "a".into(),
                    account_email: None,
                    expires_at: now_unix() + 3600,
                    needs_reauth: false,
                },
            )
            .unwrap();
            assert_eq!(access_token_for("gpt").as_deref(), Some(jwt.as_str()));
            // needs_reauth → no usable bearer
            let mut dead = subscription_token("gpt").unwrap();
            dead.needs_reauth = true;
            put_token("gpt", dead).unwrap();
            assert!(access_token_for("gpt").is_none());
            assert!(remove_token("gpt").unwrap());
            assert!(subscription_token("gpt").is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        with_temp_store(|| {
            put_token("gpt", SubscriptionToken::default()).unwrap();
            let mode = std::fs::metadata(tokens_file_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        });
    }

    #[test]
    fn refresh_swaps_token_and_marks_dead_on_failure() {
        with_temp_store(|| {
            put_token(
                "gpt",
                SubscriptionToken {
                    access_token: "old".into(),
                    refresh_token: "r0".into(),
                    account_id: "a".into(),
                    account_email: None,
                    expires_at: 1000, // already expiring at now=1000
                    needs_reauth: false,
                },
            )
            .unwrap();
            // Successful refresh.
            let n = run_refresh_once(1000, |_rt| {
                Ok(SubscriptionToken {
                    access_token: "new".into(),
                    refresh_token: "r1".into(),
                    account_id: "a".into(),
                    account_email: None,
                    expires_at: 999_999,
                    needs_reauth: false,
                })
            });
            assert_eq!(n, 1);
            assert_eq!(subscription_token("gpt").unwrap().access_token, "new");

            // Force it expiring again, then fail the refresh.
            let mut t = subscription_token("gpt").unwrap();
            t.expires_at = 1000;
            put_token("gpt", t).unwrap();
            let n = run_refresh_once(1000, |_rt| Err("invalid_grant".into()));
            assert_eq!(n, 0);
            assert!(subscription_token("gpt").unwrap().needs_reauth);
        });
    }

    #[test]
    fn authorize_url_has_pkce_and_state() {
        let url = authorize_url("st8", "chal");
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st8"));
        assert!(url.contains(CLIENT_ID));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn parse_callback_extracts_code_state() {
        let (c, s) = parse_callback_query("/auth/callback?code=abc123&state=xyz&other=1");
        assert_eq!(c.as_deref(), Some("abc123"));
        assert_eq!(s.as_deref(), Some("xyz"));
    }
}
