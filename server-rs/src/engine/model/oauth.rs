//! ChatGPT-subscription OAuth (Authorization Code + PKCE S256) — token store + token exchange/refresh.
//!
//! Tokens live ONLY in this store (`~/.agentic-dev/oauth/openai.json`, dir 0700 / file 0600), NEVER in
//! the human-editable `providers.json`. The GPT provider entry in `providers.json` carries `oauth: true`
//! and no secret; `providers::Provider::resolved_key()` sources the live access token from here so every
//! existing eligibility gate (build_config, delegate router) works unchanged.
//!
//! The access token is short-lived JWT; a background task ([`spawn_refresher`]) keeps it fresh with the
//! refresh token and pokes the litellm proxy to pick up the new bearer. A revoked refresh token flips
//! `needs_reauth` so the UI can prompt a re-login (and the provider goes unroutable rather than serving a
//! dead bearer).

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// OAuth client + endpoints (Codex CLI's, mid-2026 — verified against litellm's chatgpt provider).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
/// Loopback port the OAuth redirect lands on (fixed by the registered client).
pub const CALLBACK_PORT: u16 = 1455;
const AUTH_BASE_DEFAULT: &str = "https://auth.openai.com";
/// Default GPT model id when the connect request doesn't pin one.
pub const DEFAULT_MODEL: &str = "gpt-5";
/// The api_base litellm points at for the ChatGPT subscription (Responses API lives under it).
pub const API_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// Refresh this many seconds before the JWT `exp`.
pub const EXPIRY_SKEW_SECS: i64 = 60;

/// Auth origin, overridable for offline tests (`AGENTIC_OPENAI_AUTH_BASE`).
pub fn auth_base() -> String {
    std::env::var("AGENTIC_OPENAI_AUTH_BASE").unwrap_or_else(|_| AUTH_BASE_DEFAULT.to_string())
}
fn token_url() -> String {
    format!("{}/oauth/token", auth_base())
}
fn authorize_endpoint() -> String {
    format!("{}/oauth/authorize", auth_base())
}

/// The GPT model id to register: explicit arg, else `AGENTIC_CHATGPT_MODEL`, else [`DEFAULT_MODEL`].
pub fn resolve_model(requested: Option<&str>) -> String {
    requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("AGENTIC_CHATGPT_MODEL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

// ── token store ────────────────────────────────────────────────────────────────────────────────

/// Persisted OAuth state. Mirrors the field names litellm's chatgpt authenticator uses, so the two
/// never disagree on shape (we own the file; litellm's own device flow is unused here).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TokenStore {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    /// JWT `exp`, epoch SECONDS.
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub account_id: String,
    /// Set when a refresh got a 4xx (refresh token dead) → user must log in again.
    #[serde(default)]
    pub needs_reauth: bool,
}

/// Test-only override for [`store_path`] (a static, not `env::set_var` — setenv races getenv in glibc).
pub static STORE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Serializes any test that mutates the global [`STORE_OVERRIDE`] — shared across modules (providers.rs
/// also exercises the oauth-backed `resolved_key`) so they don't interleave under cargo's test threads.
#[cfg(test)]
pub(crate) static STORE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The token store file: test override, else `AGENTIC_OAUTH_STORE`, else `~/.agentic-dev/oauth/openai.json`.
pub fn store_path() -> PathBuf {
    if let Some(p) = STORE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_OAUTH_STORE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("oauth")
        .join("openai.json")
}

pub fn load() -> Option<TokenStore> {
    load_from(&store_path())
}
pub fn load_from(path: &Path) -> Option<TokenStore> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(s: &TokenStore) -> std::io::Result<()> {
    save_to(&store_path(), s)
}
/// Atomic write (temp + rename); dir 0700, file 0600 — it holds the refresh token in clear.
pub fn save_to(path: &Path, s: &TokenStore) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Explicit: create_dir_all leaves umask (0755); the store dir must not be world-listable.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
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

/// Best-effort wipe (on disconnect / provider delete).
pub fn clear() {
    let _ = std::fs::remove_file(store_path());
}

/// The live access token for `resolved_key`/`build_config`. Empty when disconnected or `needs_reauth`
/// (→ the provider is treated as keyless and drops out of routing instead of serving a dead bearer).
/// Does NOT hit the network; the refresher keeps the stored token current.
pub fn current_access_token() -> String {
    match load() {
        Some(s) if !s.needs_reauth => s.access_token,
        _ => String::new(),
    }
}

/// Account id (JWT `chatgpt_account_id`) for the `ChatGPT-Account-Id` header, or "".
pub fn account_id() -> String {
    load().map(|s| s.account_id).unwrap_or_default()
}

// ── PKCE + authorize URL ─────────────────────────────────────────────────────────────────────────

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Fresh PKCE pair. Verifier = 64 hex chars (2× uuid, 128 bits entropy) — within RFC 7636's 43..128
/// and drawn from the unreserved set `[0-9a-f]`.
pub fn gen_pkce() -> Pkce {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = s256_challenge(&verifier);
    Pkce { verifier, challenge }
}

/// `base64url(sha256(verifier))` without padding (S256).
pub fn s256_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Opaque CSRF state.
pub fn gen_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The provider authorize URL to open in a browser.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    reqwest::Url::parse_with_params(
        &authorize_endpoint(),
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ],
    )
    .expect("authorize url is well-formed")
    .to_string()
}

// ── JWT claim decode (NO signature verification — we only read exp / account id) ──────────────────

pub struct Claims {
    pub exp: i64,
    pub account_id: String,
}

/// Decode the `exp` (epoch seconds) and `chatgpt_account_id` claim from a JWT payload segment.
pub fn decode_claims(jwt: &str) -> Option<Claims> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = v.get("exp").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let account_id = v
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(Claims { exp, account_id })
}

// ── token exchange / refresh ──────────────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum TokenError {
    /// Non-2xx from the token endpoint. 4xx on refresh ⇒ re-login required.
    #[error("token endpoint status {0}")]
    Status(u16),
    #[error("{0}")]
    Other(String),
}

/// Injectable token-endpoint call for offline tests: `(url, body_repr) -> Ok(json) | Err(status)`.
/// `None` in production ⇒ a real HTTPS POST. Mirrors `engine::transcripts::usage::fetch_usage`.
pub type TokenFetch<'a> =
    &'a (dyn Fn(&str, &str) -> Result<serde_json::Value, u16> + Send + Sync);

static HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(reqwest::Client::new);

async fn post_form(
    params: &[(&str, &str)],
    fetch: Option<TokenFetch<'_>>,
) -> Result<serde_json::Value, TokenError> {
    let url = token_url();
    if let Some(f) = fetch {
        let body = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        return f(&url, &body).map_err(TokenError::Status);
    }
    let res = HTTP
        .post(&url)
        .form(params)
        .send()
        .await
        .map_err(|e| TokenError::Other(e.to_string()))?;
    if !res.status().is_success() {
        return Err(TokenError::Status(res.status().as_u16()));
    }
    res.json()
        .await
        .map_err(|e| TokenError::Other(e.to_string()))
}

async fn post_json(
    body: &serde_json::Value,
    fetch: Option<TokenFetch<'_>>,
) -> Result<serde_json::Value, TokenError> {
    let url = token_url();
    if let Some(f) = fetch {
        return f(&url, &body.to_string()).map_err(TokenError::Status);
    }
    let res = HTTP
        .post(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| TokenError::Other(e.to_string()))?;
    if !res.status().is_success() {
        return Err(TokenError::Status(res.status().as_u16()));
    }
    res.json()
        .await
        .map_err(|e| TokenError::Other(e.to_string()))
}

/// Build a `TokenStore` from a token-endpoint JSON response. `old_refresh` is carried over when the
/// response omits a new refresh token (refresh responses often do).
fn store_from_response(v: &serde_json::Value, old_refresh: &str) -> Result<TokenStore, TokenError> {
    let get = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or_default();
    let access_token = get("access_token").to_string();
    if access_token.is_empty() {
        return Err(TokenError::Other("token response missing access_token".into()));
    }
    let id_token = get("id_token").to_string();
    let refresh_token = match get("refresh_token") {
        "" => old_refresh.to_string(),
        r => r.to_string(),
    };
    // exp + account id come from the JWT; prefer id_token, fall back to access token.
    let claims = decode_claims(&id_token).or_else(|| decode_claims(&access_token));
    let (expires_at, account_id) = claims
        .map(|c| (c.exp, c.account_id))
        .unwrap_or((0, String::new()));
    Ok(TokenStore {
        access_token,
        refresh_token,
        id_token,
        expires_at,
        account_id,
        needs_reauth: false,
    })
}

/// Exchange an authorization code (+ PKCE verifier) for tokens (form-encoded per the OAuth spec).
pub async fn exchange_code(
    code: &str,
    verifier: &str,
    fetch: Option<TokenFetch<'_>>,
) -> Result<TokenStore, TokenError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let v = post_form(&params, fetch).await?;
    store_from_response(&v, "")
}

/// Refresh the access token (JSON body, matching the provider's refresh contract).
pub async fn refresh(
    refresh_token: &str,
    fetch: Option<TokenFetch<'_>>,
) -> Result<TokenStore, TokenError> {
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    });
    let v = post_json(&body, fetch).await?;
    store_from_response(&v, refresh_token)
}

// ── login orchestration (shared by the /api handler AND the :1455 loopback listener) ─────────────

/// The registry name of the ChatGPT-subscription provider.
pub const PROVIDER_NAME: &str = "ChatGPT";

const PENDING_TTL_SECS: u64 = 600;

/// In-flight PKCE verifiers keyed by CSRF `state`. Process-global (single server) so both the axum
/// `/complete` handler and the standalone loopback listener share it.
static PENDING: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>> =
    std::sync::LazyLock::new(Default::default);

fn pending_put(state: String, verifier: String) {
    let now = std::time::Instant::now();
    let mut m = PENDING.lock();
    m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < PENDING_TTL_SECS);
    m.insert(state, (verifier, now));
}
fn pending_take(state: &str) -> Option<String> {
    let mut m = PENDING.lock();
    let (verifier, t) = m.remove(state)?;
    (std::time::Instant::now().duration_since(t).as_secs() < PENDING_TTL_SECS).then_some(verifier)
}

pub struct StartedLogin {
    pub authorize_url: String,
    pub state: String,
}

/// Begin a login: mint PKCE + state, stash the verifier, return the browser authorize URL.
pub fn start_login() -> StartedLogin {
    let pkce = gen_pkce();
    let state = gen_state();
    let authorize_url = authorize_url(&pkce.challenge, &state);
    pending_put(state.clone(), pkce.verifier);
    StartedLogin { authorize_url, state }
}

#[derive(Debug)]
pub struct CompletedLogin {
    pub account_id: String,
    pub model: String,
    pub expires_at: i64,
}

#[derive(thiserror::Error, Debug)]
pub enum CompleteError {
    #[error("unknown or expired state")]
    BadState,
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error("{0}")]
    Other(String),
}

/// Finish a login: validate `state`, exchange the code, persist tokens, register the GPT provider, and
/// reload the proxy. Store is written BEFORE the provider is registered so `resolved_key` is never
/// momentarily empty for a registered provider.
pub async fn complete_login(
    code: &str,
    state: &str,
    model: Option<&str>,
    fetch: Option<TokenFetch<'_>>,
) -> Result<CompletedLogin, CompleteError> {
    let verifier = pending_take(state).ok_or(CompleteError::BadState)?;
    let store = exchange_code(code, &verifier, fetch).await?;
    save(&store).map_err(|e| CompleteError::Other(e.to_string()))?;
    let model = resolve_model(model);
    register_provider(&model).map_err(|e| CompleteError::Other(e.to_string()))?;
    crate::engine::litellm::request_reload();
    Ok(CompletedLogin {
        account_id: store.account_id,
        model,
        expires_at: store.expires_at,
    })
}

/// Upsert the ChatGPT provider into `providers.json` (oauth-backed, no secret in the file).
fn register_provider(model: &str) -> std::io::Result<()> {
    use crate::engine::providers::{Protocol, Provider};
    crate::engine::providers::upsert(Provider {
        name: PROVIDER_NAME.to_string(),
        base_url: API_BASE.to_string(),
        api_key: String::new(),
        api_key_env: None,
        model: model.to_string(),
        protocol: Protocol::Openai,
        capability: 0.9,
        description: Some("ChatGPT subscription (OAuth)".to_string()),
        priority: 0.5,
        cost: 0.2,
        router: false,
        enabled: true,
        oauth: true,
    })
}

/// Disconnect: wipe the token store (called when the oauth provider is deleted). The refresher then
/// finds no token and idles; the provider is already gone from the registry.
pub fn disconnect() {
    clear();
}

/// Non-secret connection status for the UI.
#[derive(serde::Serialize)]
pub struct OauthStatus {
    pub connected: bool,
    pub account_id: String,
    pub expires_at: i64,
    pub needs_reauth: bool,
}

pub fn status() -> OauthStatus {
    match load() {
        Some(s) => OauthStatus {
            connected: !s.access_token.is_empty() && !s.needs_reauth,
            account_id: s.account_id,
            expires_at: s.expires_at,
            needs_reauth: s.needs_reauth,
        },
        None => OauthStatus {
            connected: false,
            account_id: String::new(),
            expires_at: 0,
            needs_reauth: false,
        },
    }
}

// ── background refresher ──────────────────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One refresher tick: refresh if the stored token is within the skew of expiry. Returns the seconds to
/// sleep before the next tick (clamped to 5..=300 so a fresh login is picked up promptly without spinning).
/// Split out from the loop so it is unit-testable with an injected token endpoint.
pub async fn refresh_tick(fetch: Option<TokenFetch<'_>>) -> i64 {
    let Some(store) = load() else {
        return 300; // not connected
    };
    if store.needs_reauth || store.access_token.is_empty() || store.refresh_token.is_empty() {
        return 300;
    }
    let due_in = store.expires_at - EXPIRY_SKEW_SECS - now_secs();
    if due_in > 0 {
        return due_in.clamp(5, 300);
    }
    match refresh(&store.refresh_token, fetch).await {
        Ok(fresh) => {
            if save(&fresh).is_ok() {
                // The running proxy holds the OLD bearer in its env; restart it to pick up the new one.
                crate::engine::litellm::request_reload();
            }
            (fresh.expires_at - EXPIRY_SKEW_SECS - now_secs()).clamp(5, 300)
        }
        Err(TokenError::Status(code)) if (400..500).contains(&code) => {
            // Refresh token dead → require re-login; stop hammering the endpoint.
            let dead = TokenStore {
                needs_reauth: true,
                access_token: String::new(),
                ..store
            };
            let _ = save(&dead);
            crate::engine::litellm::request_reload();
            300
        }
        Err(_) => 30, // transient (network); retry soon-ish
    }
}

/// Spawn the token refresher. Simple poll loop (no Notify → no select!/permit race): each tick refreshes
/// when near expiry, then sleeps 5..=300s. `/complete` does its own synchronous register+reload, so this
/// task only keeps the bearer fresh over time; a <=300s poll is ample.
// ponytail: 300s poll ceiling — fine for hour-scale JWT lifetimes; go event-driven only if that changes.
pub fn spawn_refresher() {
    tokio::spawn(async move {
        loop {
            let sleep = refresh_tick(None).await;
            tokio::time::sleep(std::time::Duration::from_secs(sleep.max(1) as u64)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TmpStore {
        _dir: tempfile::TempDir,
        _guard: parking_lot::MutexGuard<'static, ()>,
    }
    fn with_tmp_store() -> TmpStore {
        let guard = super::STORE_TEST_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        *STORE_OVERRIDE.lock() = Some(dir.path().join("oauth").join("openai.json"));
        TmpStore { _dir: dir, _guard: guard }
    }
    impl Drop for TmpStore {
        fn drop(&mut self) {
            *STORE_OVERRIDE.lock() = None;
        }
    }

    /// Minimal unsigned JWT with the given exp + account id (header.payload.sig, sig ignored).
    fn fake_jwt(exp: i64, account: &str) -> String {
        let payload = serde_json::json!({
            "exp": exp,
            "https://api.openai.com/auth": { "chatgpt_account_id": account },
        });
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.sig",
            b64(b"{\"alg\":\"none\"}"),
            b64(payload.to_string().as_bytes())
        )
    }

    #[test]
    fn pkce_verifier_is_valid_and_challenge_matches() {
        let p = gen_pkce();
        assert!((43..=128).contains(&p.verifier.len()), "len {}", p.verifier.len());
        assert!(p.verifier.chars().all(|c| c.is_ascii_hexdigit()));
        // challenge is deterministic S256 of the verifier, no padding
        assert_eq!(p.challenge, s256_challenge(&p.verifier));
        assert!(!p.challenge.contains('='));
        // known-answer: S256("abc") base64url no-pad
        assert_eq!(
            s256_challenge("abc"),
            "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
        );
    }

    #[test]
    fn authorize_url_pins_all_params() {
        let u = authorize_url("CHAL", "STATE");
        for needle in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge=CHAL",
            "code_challenge_method=S256",
            "state=STATE",
            "scope=openid+profile+email+offline_access",
        ] {
            assert!(u.contains(needle), "missing {needle} in {u}");
        }
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn decode_claims_reads_exp_and_account() {
        let c = decode_claims(&fake_jwt(1_800_000_000, "acct_42")).unwrap();
        assert_eq!(c.exp, 1_800_000_000);
        assert_eq!(c.account_id, "acct_42");
    }

    #[test]
    fn store_round_trip_and_current_token_respects_needs_reauth() {
        let _t = with_tmp_store();
        let s = TokenStore {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            id_token: "IT".into(),
            expires_at: 1_800_000_000,
            account_id: "acct_1".into(),
            needs_reauth: false,
        };
        save(&s).unwrap();
        assert_eq!(load().unwrap(), s);
        assert_eq!(current_access_token(), "AT");
        assert_eq!(account_id(), "acct_1");

        let dead = TokenStore { needs_reauth: true, ..s };
        save(&dead).unwrap();
        assert_eq!(current_access_token(), "", "needs_reauth ⇒ keyless");
    }

    #[cfg(unix)]
    #[test]
    fn store_file_and_dir_perms_are_tight() {
        use std::os::unix::fs::PermissionsExt;
        let _t = with_tmp_store();
        save(&TokenStore { access_token: "x".into(), ..Default::default() }).unwrap();
        let path = store_path();
        let fmode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dmode = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "file perms");
        assert_eq!(dmode, 0o700, "dir perms");
    }

    #[tokio::test]
    async fn exchange_builds_store_from_canned_response() {
        let id = fake_jwt(1_900_000_000, "acct_x");
        let idc = id.clone();
        let fetch = move |_url: &str, body: &str| {
            assert!(body.contains("grant_type=authorization_code"));
            assert!(body.contains("code_verifier=VER"));
            Ok(serde_json::json!({
                "access_token": "AT1", "refresh_token": "RT1", "id_token": idc,
            }))
        };
        let s = exchange_code("CODE", "VER", Some(&fetch)).await.unwrap();
        assert_eq!(s.access_token, "AT1");
        assert_eq!(s.refresh_token, "RT1");
        assert_eq!(s.expires_at, 1_900_000_000);
        assert_eq!(s.account_id, "acct_x");
        assert!(!s.needs_reauth);
    }

    #[tokio::test]
    async fn refresh_preserves_old_refresh_token_when_omitted() {
        let id = fake_jwt(1_950_000_000, "acct_y");
        let idc = id.clone();
        let fetch = move |_u: &str, _b: &str| {
            Ok(serde_json::json!({ "access_token": "AT2", "id_token": idc }))
        };
        let s = refresh("OLD_RT", Some(&fetch)).await.unwrap();
        assert_eq!(s.access_token, "AT2");
        assert_eq!(s.refresh_token, "OLD_RT", "carried over when response omits it");
    }

    #[tokio::test]
    async fn refresh_4xx_surfaces_status() {
        let fetch = move |_u: &str, _b: &str| Err(401u16);
        match refresh("RT", Some(&fetch)).await {
            Err(TokenError::Status(401)) => {}
            other => panic!("expected 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_tick_flips_needs_reauth_on_dead_token() {
        let _t = with_tmp_store();
        save(&TokenStore {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            expires_at: 1, // long past ⇒ due now
            ..Default::default()
        })
        .unwrap();
        let fetch = move |_u: &str, _b: &str| Err(400u16);
        refresh_tick(Some(&fetch)).await;
        let after = load().unwrap();
        assert!(after.needs_reauth);
        assert_eq!(after.access_token, "");
    }

    #[test]
    fn resolve_model_precedence() {
        assert_eq!(resolve_model(Some("gpt-9")), "gpt-9");
        assert_eq!(resolve_model(Some("  ")), DEFAULT_MODEL);
        assert_eq!(resolve_model(None), DEFAULT_MODEL);
    }

    #[tokio::test]
    async fn complete_login_rejects_unknown_state() {
        let _t = with_tmp_store();
        let fetch = move |_u: &str, _b: &str| Ok(serde_json::json!({}));
        match complete_login("code", "never-issued", None, Some(&fetch)).await {
            Err(CompleteError::BadState) => {}
            other => panic!("expected BadState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_login_persists_tokens_and_registers_provider() {
        let _t = with_tmp_store();
        // isolate providers.json too (register_provider upserts into it)
        let pdir = tempfile::tempdir().unwrap();
        *crate::engine::providers::PROVIDERS_FILE_OVERRIDE.lock() =
            Some(pdir.path().join("providers.json"));

        let started = start_login(); // seeds a valid state → verifier
        let id = fake_jwt(2_000_000_000, "acct_login");
        let idc = id.clone();
        let fetch = move |_u: &str, body: &str| {
            assert!(body.contains("grant_type=authorization_code"));
            Ok(serde_json::json!({
                "access_token": "AT_DONE", "refresh_token": "RT_DONE", "id_token": idc,
            }))
        };

        let done = complete_login("the-code", &started.state, Some("gpt-5"), Some(&fetch))
            .await
            .unwrap();
        assert_eq!(done.account_id, "acct_login");
        assert_eq!(done.model, "gpt-5");
        assert_eq!(done.expires_at, 2_000_000_000);

        // token persisted to the store (not providers.json)
        assert_eq!(current_access_token(), "AT_DONE");
        // provider registered: oauth-backed, openai protocol, codex base, NO secret in the file
        let list = crate::engine::providers::load_list();
        let p = list.iter().find(|p| p.name == PROVIDER_NAME).expect("provider registered");
        assert!(p.oauth);
        assert!(p.enabled);
        assert_eq!(p.model, "gpt-5");
        assert_eq!(p.base_url, API_BASE);
        assert!(p.api_key.is_empty() && p.api_key_env.is_none());
        let raw = std::fs::read_to_string(pdir.path().join("providers.json")).unwrap();
        assert!(!raw.contains("AT_DONE") && !raw.contains("RT_DONE"), "no token in providers.json");

        // state is single-use: replaying it fails
        match complete_login("the-code", &started.state, None, Some(&fetch)).await {
            Err(CompleteError::BadState) => {}
            other => panic!("state must be single-use, got {other:?}"),
        }

        *crate::engine::providers::PROVIDERS_FILE_OVERRIDE.lock() = None;
    }
}
