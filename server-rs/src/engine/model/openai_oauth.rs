//! ChatGPT subscription sign-in (OAuth 2.0 Authorization-Code + PKCE) — lets a user attach their
//! ChatGPT plan instead of an OpenAI API key. The GPT model then joins the delegate routing pool as
//! an ordinary `openai`-protocol provider (called through the LiteLLM proxy, `engine::litellm`).
//!
//! GPT can NOT be the main session model (that's the Claude Agent SDK); it only runs as a delegate
//! worker. See `docs/internals.md` and the provider registry in `engine::providers`.
//!
//! Secrets never touch the human-editable `providers.json`: the access/refresh tokens live in a
//! tightened credential store (`~/.agentic-dev/oauth/chatgpt.json`, dir 0700 / file 0600). The
//! provider row only carries the codex base URL — `providers::Provider::resolved_key` recognises it
//! and pulls the CURRENT (auto-refreshed) access token from here, so the bearer rotates without the
//! file ever holding a token.
#![allow(dead_code)]

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::engine::providers::{Protocol, Provider};

// ── OAuth client constants (Codex CLI's public client; same values, mid-2026) ──
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPES: &str = "openid profile email offline_access";
const ORIGINATOR: &str = "codex_cli_rs";

/// The ChatGPT backend "codex" endpoint (Responses API, forced streaming). The provider row's
/// `base_url` is set to this; `resolved_key` / `litellm` key off it.
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Provider registry name for the subscription row.
pub const PROVIDER_NAME: &str = "chatgpt";

/// The GPT model id exposed to the router / model list. Overridable for when the plan's default
/// codex model id changes.
pub fn model_id() -> String {
    std::env::var("AGENTIC_CHATGPT_MODEL").unwrap_or_else(|_| "gpt-5-codex".to_string())
}

/// Refresh this many seconds before the access token's `exp` (short-lived JWT).
const REFRESH_SKEW_SECS: i64 = 120;

// ── credential store ──

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub email: String,
    /// unix seconds the access token expires (from the JWT `exp`, else exchange time + expires_in)
    #[serde(default)]
    pub expires_at: i64,
    /// set once a refresh fails with a definitive auth error — the user must sign in again
    #[serde(default)]
    pub needs_relogin: bool,
}

/// Test-only override for [store_path] (a plain static, not `env::set_var` — setenv racing a
/// concurrent getenv on another test thread is UB). Always `None` in production.
pub static STORE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

pub fn store_path() -> PathBuf {
    if let Some(p) = STORE_OVERRIDE.lock().clone() {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("oauth")
        .join("chatgpt.json")
}

pub fn load_from(path: &Path) -> Option<Credentials> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn load() -> Option<Credentials> {
    load_from(&store_path())
}

/// Write creds atomically; dir 0700, file 0600 (it holds refresh + access tokens).
pub fn save_to(path: &Path, creds: &Credentials) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_string_pretty(creds).map_err(std::io::Error::other)?;
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

pub fn save(creds: &Credentials) -> std::io::Result<()> {
    save_to(&store_path(), creds)
}

fn delete_store() {
    let _ = std::fs::remove_file(store_path());
}

// ── PKCE + authorize URL ──

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh 256-bit PKCE code verifier (uuid v4 is a CSPRNG source; two give 32 bytes → 43 chars,
/// inside the RFC 7636 43–128 range).
fn gen_verifier() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    b64url(&bytes)
}

/// S256 code challenge = base64url(sha256(verifier)).
fn challenge_of(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    b64url(&h.finalize())
}

fn urlencode(s: &str) -> String {
    // Encode the few characters that actually appear in our params (scopes have spaces, redirect has
    // ':' '/'). A tiny hand-rolled encoder avoids pulling in a url/percent-encoding crate.
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

fn authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={cid}&redirect_uri={ru}&scope={sc}&code_challenge={ch}&code_challenge_method=S256&state={st}",
        cid = urlencode(CLIENT_ID),
        ru = urlencode(REDIRECT_URI),
        sc = urlencode(SCOPES),
        ch = urlencode(challenge),
        st = urlencode(state),
    )
}

/// Pending interactive login (one at a time). The verifier NEVER leaves the server.
struct Pending {
    state: String,
    verifier: String,
}
static PENDING: parking_lot::Mutex<Option<Pending>> = parking_lot::Mutex::new(None);

/// Begin a login: returns `(authorize_url, state)`. The client opens the URL in a browser and
/// captures the loopback redirect, then calls [complete_login] with the code.
pub fn start_login() -> (String, String) {
    let verifier = gen_verifier();
    let challenge = challenge_of(&verifier);
    let state = uuid::Uuid::new_v4().to_string();
    *PENDING.lock() = Some(Pending {
        state: state.clone(),
        verifier,
    });
    (authorize_url(&challenge, &state), state)
}

// ── JWT claim helpers ──

fn decode_claims(jwt: &str) -> Option<serde_json::Value> {
    let seg = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(seg)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn account_id_from_claims(v: &serde_json::Value) -> Option<String> {
    let pick = |x: &serde_json::Value| {
        x.get("chatgpt_account_id")
            .and_then(|c| c.as_str())
            .map(str::to_string)
    };
    // Top-level, else nested under OpenAI's auth-namespace claim.
    pick(v).or_else(|| v.get("https://api.openai.com/auth").and_then(pick))
}

fn str_claim(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|c| c.as_str()).map(str::to_string)
}

/// Build Credentials from a token endpoint JSON body.
fn credentials_from_token_body(
    body: &serde_json::Value,
    fallback_refresh: &str,
) -> Result<Credentials, String> {
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("token response missing access_token")?
        .to_string();
    // A refresh may or may not rotate the refresh_token; keep the old one if absent.
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_refresh.to_string());
    let id_token = body
        .get("id_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let access_claims = decode_claims(&access_token);
    let id_claims = decode_claims(&id_token);

    let account_id = access_claims
        .as_ref()
        .and_then(account_id_from_claims)
        .or_else(|| id_claims.as_ref().and_then(account_id_from_claims))
        .unwrap_or_default();
    let email = id_claims
        .as_ref()
        .and_then(|c| str_claim(c, "email"))
        .unwrap_or_default();

    // Prefer the JWT `exp`; fall back to now + expires_in.
    let expires_at = access_claims
        .as_ref()
        .and_then(|c| c.get("exp").and_then(|e| e.as_i64()))
        .unwrap_or_else(|| {
            // No `exp` claim: use `expires_in`, but if that's absent/0 too, assume a conservative
            // hour so `maybe_refresh` doesn't see expires_at≈now and refresh-storm every 60s tick.
            let ttl = body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0);
            unix_now() + if ttl > 0 { ttl } else { 3600 }
        });

    Ok(Credentials {
        access_token,
        refresh_token,
        id_token,
        account_id,
        email,
        expires_at,
        needs_relogin: false,
    })
}

// ── token endpoint calls (blocking; called off the async path) ──

fn post_token_form(form: &[(&str, &str)]) -> Result<serde_json::Value, RefreshErr> {
    let resp = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(20))
        .form(form)
        .send()
        .map_err(|e| RefreshErr::Transient(e.to_string()))?;
    let status = resp.status();
    if status.is_success() {
        resp.json::<serde_json::Value>()
            .map_err(|e| RefreshErr::Transient(e.to_string()))
    } else {
        let body = resp.text().unwrap_or_default();
        // Most 4xx from the token endpoint (invalid_grant, expired refresh) is a definitive auth
        // failure → sign out. But 429/408 are retryable: don't sign the user out over a transient
        // rate-limit during a background refresh.
        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT;
        if status.is_client_error() && !retryable {
            Err(RefreshErr::Auth(format!("{status}: {body}")))
        } else {
            Err(RefreshErr::Transient(format!("{status}: {body}")))
        }
    }
}

/// Exchange an authorization `code` (with the matching PKCE verifier) for tokens.
fn exchange_code(code: &str, verifier: &str) -> Result<Credentials, String> {
    let body = post_token_form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .map_err(|e| e.to_string())?;
    credentials_from_token_body(&body, "")
}

enum RefreshErr {
    /// Definitive — the refresh token is dead; the user must sign in again.
    Auth(String),
    /// Network / 5xx — retry later, don't clear the login.
    Transient(String),
}

impl std::fmt::Display for RefreshErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshErr::Auth(m) | RefreshErr::Transient(m) => f.write_str(m),
        }
    }
}

fn refresh(creds: &Credentials) -> Result<Credentials, RefreshErr> {
    if creds.refresh_token.is_empty() {
        return Err(RefreshErr::Auth("no refresh token".into()));
    }
    let body = post_token_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &creds.refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", SCOPES),
    ])?;
    credentials_from_token_body(&body, &creds.refresh_token).map_err(RefreshErr::Transient)
}

// ── public surface used by providers / litellm / api ──

/// True for the ChatGPT subscription provider row (openai protocol at the codex base URL).
pub fn is_subscription_provider(p: &Provider) -> bool {
    matches!(p.protocol, Protocol::Openai)
        && p.base_url.trim_end_matches('/') == CODEX_BASE_URL.trim_end_matches('/')
}

/// The current access token for the subscription, or `None` when not signed in / needs re-login.
/// (Not force-refreshed here — the background task keeps it fresh and reloads LiteLLM on rotation.)
pub fn current_access_token() -> Option<String> {
    let c = load()?;
    if c.needs_relogin || c.access_token.is_empty() {
        return None;
    }
    Some(c.access_token)
}

/// The `ChatGPT-Account-Id` header value (from the JWT), or `None` when not signed in.
pub fn codex_account_id() -> Option<String> {
    let c = load()?;
    (!c.account_id.is_empty()).then_some(c.account_id)
}

/// The codex request headers LiteLLM must add for the subscription endpoint.
pub fn codex_extra_headers() -> Vec<(String, String)> {
    vec![
        (
            "ChatGPT-Account-Id".to_string(),
            codex_account_id().unwrap_or_default(),
        ),
        ("originator".to_string(), ORIGINATOR.to_string()),
        ("OpenAI-Beta".to_string(), "responses=experimental".to_string()),
    ]
}

#[derive(Debug, Serialize)]
pub struct OAuthStatus {
    pub connected: bool,
    pub email: String,
    pub account_id: String,
    pub expires_at: i64,
    pub needs_relogin: bool,
}

pub fn status() -> OAuthStatus {
    match load() {
        Some(c) => OAuthStatus {
            connected: !c.access_token.is_empty() && !c.needs_relogin,
            email: c.email,
            account_id: c.account_id,
            expires_at: c.expires_at,
            needs_relogin: c.needs_relogin,
        },
        None => OAuthStatus {
            connected: false,
            email: String::new(),
            account_id: String::new(),
            expires_at: 0,
            needs_relogin: false,
        },
    }
}

/// The provider row for the subscription. `enabled`/`capability` mirror a strong general worker.
fn subscription_provider() -> Provider {
    Provider {
        name: PROVIDER_NAME.to_string(),
        base_url: CODEX_BASE_URL.to_string(),
        api_key: String::new(),
        api_key_env: None,
        model: model_id(),
        protocol: Protocol::Openai,
        capability: 0.9,
        description: Some("ChatGPT subscription (GPT) via OAuth — delegate worker".to_string()),
        priority: 0.5,
        cost: 0.4,
        router: false,
        enabled: true,
    }
}

/// Bumped on every sign-out (disconnect / generic delete). An in-flight refresh captures the value
/// before its network call and refuses to write the store back if it changed meanwhile — otherwise a
/// refresh that started before a disconnect would recreate the credential file after sign-out.
static DISCONNECT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Register (or refresh) the subscription provider row and reload the LiteLLM proxy. Preserves any
/// user-set routing metadata (enabled toggle, capability/priority/cost) on an existing row so a
/// boot-time / refresh re-register doesn't silently re-enable a model the user disabled. Returns the
/// upsert error (unwritable/corrupt providers file) so callers can surface a failed registration.
fn register_provider() -> std::io::Result<()> {
    let mut p = subscription_provider();
    if let Some(existing) = crate::engine::providers::load_list()
        .into_iter()
        .find(|x| x.name.eq_ignore_ascii_case(PROVIDER_NAME))
    {
        p = carry_user_metadata(p, &existing);
    }
    crate::engine::providers::upsert(p)?;
    crate::engine::litellm::request_reload();
    Ok(())
}

/// Copy the user-tunable routing metadata from an `existing` row onto the freshly-templated one, so a
/// re-register keeps the user's Enabled toggle and capability/priority/cost edits.
fn carry_user_metadata(mut fresh: Provider, existing: &Provider) -> Provider {
    fresh.enabled = existing.enabled;
    fresh.capability = existing.capability;
    fresh.priority = existing.priority;
    fresh.cost = existing.cost;
    if existing.description.is_some() {
        fresh.description = existing.description.clone();
    }
    fresh
}

/// Finish a login started by [start_login]: exchange the code, persist tokens, register the model.
pub fn complete_login(code: &str, state: &str) -> Result<(), String> {
    let pending = PENDING.lock().take().ok_or("no pending login")?;
    if pending.state != state {
        return Err("state mismatch (possible CSRF) — restart the sign-in".into());
    }
    // Refuse to clobber a user's pre-existing non-subscription provider that happens to be named
    // `chatgpt` (sign-in would overwrite it and sign-out would delete it).
    if let Some(existing) = crate::engine::providers::load_list()
        .into_iter()
        .find(|x| x.name.eq_ignore_ascii_case(PROVIDER_NAME))
    {
        if !is_subscription_provider(&existing) {
            return Err(
                "a provider named 'chatgpt' already exists — rename or delete it before signing in"
                    .into(),
            );
        }
    }
    let creds = exchange_code(code, &pending.verifier)?;
    // The ChatGPT access token is short-lived; without a refresh token the account silently dies at
    // expiry. offline_access should always yield one — treat its absence as a failed login.
    if creds.refresh_token.is_empty() {
        return Err(
            "ChatGPT did not return a refresh token (offline_access denied?) — try again".into(),
        );
    }
    save(&creds).map_err(|e| e.to_string())?;
    register_provider().map_err(|e| format!("signed in but failed to register the model: {e}"))?;
    Ok(())
}

/// Sign out: drop the tokens and remove the provider row.
pub fn disconnect() {
    DISCONNECT_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    delete_store();
    let _ = crate::engine::providers::remove(PROVIDER_NAME);
    crate::engine::litellm::request_reload();
}

/// Drop just the stored tokens, without touching the provider row or reloading — used when the row is
/// deleted through the generic `DELETE /api/providers/{name}` path, so signing out via the normal
/// providers UI doesn't leave the account connected (and boot doesn't re-register it).
pub fn forget_credentials() {
    DISCONNECT_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    delete_store();
}

/// Refresh the token if it's within the skew window. Returns true if it rotated (→ reload LiteLLM).
fn maybe_refresh() -> bool {
    let Some(c) = load() else { return false };
    if c.needs_relogin || c.refresh_token.is_empty() {
        return false;
    }
    // exp==0 means we never learned an expiry — refresh once to establish one.
    if c.expires_at != 0 && c.expires_at - unix_now() > REFRESH_SKEW_SECS {
        return false;
    }
    // Snapshot the sign-out generation before the network call; if a disconnect lands while we're
    // waiting, don't write the store back (that would resurrect a signed-out account).
    let gen = DISCONNECT_GEN.load(std::sync::atomic::Ordering::Acquire);
    let disconnected_since = || DISCONNECT_GEN.load(std::sync::atomic::Ordering::Acquire) != gen;
    match refresh(&c) {
        Ok(nc) => {
            if disconnected_since() {
                return false;
            }
            if let Err(e) = save(&nc) {
                tracing::warn!("[chatgpt-oauth] save after refresh failed: {e}");
                return false;
            }
            tracing::info!("[chatgpt-oauth] access token refreshed");
            true
        }
        Err(RefreshErr::Auth(msg)) => {
            tracing::warn!("[chatgpt-oauth] refresh rejected ({msg}); sign-in required");
            if disconnected_since() {
                return false;
            }
            let mut dead = c;
            dead.needs_relogin = true;
            let _ = save(&dead);
            crate::engine::litellm::request_reload();
            false
        }
        Err(RefreshErr::Transient(msg)) => {
            tracing::warn!("[chatgpt-oauth] refresh transient failure ({msg}); will retry");
            false
        }
    }
}

/// Background thread: keep the subscription provider registered and its access token fresh.
pub fn spawn_refresh_task() {
    std::thread::spawn(|| {
        // On boot, if we already have a login, make sure the provider row exists (providers.json may
        // have been reset) and the token is fresh.
        if load().is_some() {
            if let Err(e) = register_provider() {
                tracing::warn!("[chatgpt-oauth] boot re-register failed: {e}");
            }
            if maybe_refresh() {
                crate::engine::litellm::request_reload();
            }
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
            // ponytail: a rotation (~hourly) reloads the SHARED LiteLLM proxy, which hard-restarts
            //   it and interrupts any in-flight openai worker of OTHER providers — same blast radius
            //   as the existing "reload on every provider CRUD". Upgrade path if it bites: have the
            //   proxy read the codex bearer from a file it re-reads per request, so rotation needs no
            //   restart. Out of scope here.
            if maybe_refresh() {
                crate::engine::litellm::request_reload();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_base64url() {
        // Known RFC 7636 test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_of(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        // A generated verifier is 43 chars (256 bits, base64url no pad) and round-trips through S256.
        let v = gen_verifier();
        assert_eq!(v.len(), 43);
        assert!(!challenge_of(&v).is_empty());
    }

    #[test]
    fn authorize_url_has_required_params() {
        let url = authorize_url("CHAL", "STATE");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        // redirect + scopes are percent-encoded
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
    }

    fn make_jwt(claims: serde_json::Value) -> String {
        let header = b64url(br#"{"alg":"none"}"#);
        let payload = b64url(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn credentials_parsed_from_token_body() {
        let access = make_jwt(serde_json::json!({
            "exp": 1_900_000_000i64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_123"}
        }));
        let id = make_jwt(serde_json::json!({"email": "user@example.com"}));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "rt_new",
            "id_token": id,
            "expires_in": 3600
        });
        let c = credentials_from_token_body(&body, "rt_old").unwrap();
        assert_eq!(c.account_id, "acct_123");
        assert_eq!(c.email, "user@example.com");
        assert_eq!(c.expires_at, 1_900_000_000);
        assert_eq!(c.refresh_token, "rt_new");
        assert!(!c.needs_relogin);

        // A refresh with no rotated refresh_token keeps the old one; top-level account claim works.
        let access2 = make_jwt(serde_json::json!({"exp": 42, "chatgpt_account_id": "acct_9"}));
        let body2 = serde_json::json!({"access_token": access2});
        let c2 = credentials_from_token_body(&body2, "rt_old").unwrap();
        assert_eq!(c2.refresh_token, "rt_old");
        assert_eq!(c2.account_id, "acct_9");
        assert_eq!(c2.expires_at, 42);

        // Missing access_token → error.
        assert!(credentials_from_token_body(&serde_json::json!({}), "x").is_err());
    }

    #[test]
    fn store_roundtrips_with_tight_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("chatgpt.json");
        assert!(load_from(&path).is_none());
        let creds = Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            email: "e@x.com".into(),
            expires_at: 123,
            ..Default::default()
        };
        save_to(&path, &creds).unwrap();
        let got = load_from(&path).unwrap();
        assert_eq!(got.access_token, "at");
        assert_eq!(got.account_id, "acct");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be 0600");
        }
    }

    #[test]
    fn subscription_provider_is_detected() {
        let p = subscription_provider();
        assert!(is_subscription_provider(&p));
        assert_eq!(p.base_url, CODEX_BASE_URL);
        assert!(matches!(p.protocol, Protocol::Openai));
        // A trailing slash on the base URL still matches.
        let mut p2 = p.clone();
        p2.base_url = format!("{CODEX_BASE_URL}/");
        assert!(is_subscription_provider(&p2));
        // An anthropic provider or a different URL is NOT the subscription.
        let other = Provider {
            protocol: Protocol::Anthropic,
            ..subscription_provider()
        };
        assert!(!is_subscription_provider(&other));
    }

    #[test]
    fn re_register_preserves_user_routing_metadata() {
        // A boot / refresh re-register must not silently re-enable a model the user disabled, nor
        // wipe their capability/priority/cost edits.
        let fresh = subscription_provider();
        assert!(fresh.enabled, "template starts enabled");
        let existing = Provider {
            enabled: false,
            capability: 0.42,
            priority: 0.1,
            cost: 0.9,
            description: Some("my note".into()),
            ..subscription_provider()
        };
        let merged = carry_user_metadata(fresh, &existing);
        assert!(!merged.enabled, "disable toggle preserved");
        assert!((merged.capability - 0.42).abs() < f32::EPSILON);
        assert!((merged.priority - 0.1).abs() < f32::EPSILON);
        assert!((merged.cost - 0.9).abs() < f32::EPSILON);
        assert_eq!(merged.description.as_deref(), Some("my note"));
        // Identity fields still come from the template (the codex endpoint/model/protocol).
        assert_eq!(merged.base_url, CODEX_BASE_URL);
        assert!(matches!(merged.protocol, Protocol::Openai));
    }

    #[test]
    fn extra_headers_present() {
        let h = codex_extra_headers();
        assert!(h.iter().any(|(k, _)| k == "originator"));
        assert!(h.iter().any(|(k, v)| k == "OpenAI-Beta" && v == "responses=experimental"));
        assert!(h.iter().any(|(k, _)| k == "ChatGPT-Account-Id"));
    }
}
