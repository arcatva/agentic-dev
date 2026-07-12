//! ChatGPT subscription OAuth (Authorization Code + PKCE) — lets a user connect their personal
//! ChatGPT plan instead of pasting an API key.
//!
//! We own the OAuth: generate the PKCE challenge, exchange the authorization code, persist the
//! tokens, and refresh them ahead of expiry. The tokens land in a 0600 file SEPARATE from the
//! human-editable `providers.json` (the plaintext token never goes in that file). The persisted
//! file is the Codex/LiteLLM-shaped `auth.json` that LiteLLM's `chatgpt/` provider reads, so a
//! delegate worker's Anthropic request is translated by LiteLLM into a ChatGPT Responses API call
//! with the right bearer + headers — we do not reimplement that translation.
//!
//! Everything here is axum-free so it stays unit-testable in isolation (engine invariant).
#![allow(dead_code)]

use base64::Engine;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const SCOPES: &str = "openid profile email offline_access";
/// ChatGPT backend the LiteLLM `chatgpt/` provider calls (Responses API).
pub const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
pub const CHATGPT_ORIGINATOR: &str = "codex_cli_rs";

/// Refresh this long before the access token expires (tokens live ~1h; refresh with margin so the
/// token LiteLLM reads is always valid and LiteLLM itself rarely needs to refresh).
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
/// A pending PKCE login is only valid this long (state → verifier is discarded after).
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
/// Never sleep less than this between refresh attempts — a floor so a bogus tiny `expires_in`
/// (0 < ttl ≤ margin) can't turn the refresher into a busy loop that hammers the token endpoint
/// and restarts the proxy. ponytail: fixed 60s floor; fine because real tokens live ~1h.
const MIN_REFRESH_SLEEP_MS: i64 = 60 * 1000;

// ── test-only overrides (data-race-free; env::set_var racing getenv on another thread is UB) ──
static AUTH_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
static TOKEN_URL_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

fn token_url() -> String {
    if let Some(u) = TOKEN_URL_OVERRIDE.lock().clone() {
        return u;
    }
    TOKEN_URL.to_string()
}

/// The token store path: the test override, else `$AGENTIC_CHATGPT_AUTH_FILE`, else
/// `~/.agentic-dev/chatgpt-auth.json`.
pub fn auth_file_path() -> PathBuf {
    if let Some(p) = AUTH_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_CHATGPT_AUTH_FILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("chatgpt-auth.json")
}

/// The GPT model id to register (single model; override with `AGENTIC_CHATGPT_MODEL`).
pub fn chatgpt_model() -> String {
    std::env::var("AGENTIC_CHATGPT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gpt-5".to_string())
}

// ── PKCE ──

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE (S256) verifier + challenge. Verifier = two v4 UUIDs (64 hex chars, all in the
/// unreserved set); challenge = base64url(sha256(verifier)) with no padding.
pub fn gen_pkce() -> Pkce {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = code_challenge(&verifier);
    Pkce { verifier, challenge }
}

fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Build the authorize URL for the browser. Uses `reqwest::Url` (reqwest re-exports the `url`
/// crate) for correct query-parameter encoding rather than hand-rolling it.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut u = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid constant URL");
    u.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    u.to_string()
}

// ── pending PKCE map (state → verifier) ──
static PENDING: Mutex<Option<HashMap<String, (String, i64)>>> = Mutex::new(None);

/// Start a login: returns `(authorize_url, state)` and remembers the verifier keyed by state.
pub fn begin_login() -> (String, String) {
    let pkce = gen_pkce();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = authorize_url(&pkce.challenge, &state);
    let mut guard = PENDING.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    let now = crate::util::now_ms();
    // Opportunistically drop stale pending logins so the map can't grow unbounded.
    map.retain(|_, (_, created)| now - *created < PENDING_TTL_MS);
    map.insert(state.clone(), (pkce.verifier, now));
    (url, state)
}

/// Pop the verifier for `state`. `None` if unknown or older than the TTL (replay / stale).
pub fn take_verifier(state: &str) -> Option<String> {
    let mut guard = PENDING.lock();
    let map = guard.as_mut()?;
    let (verifier, created) = map.remove(state)?;
    if crate::util::now_ms() - created >= PENDING_TTL_MS {
        return None;
    }
    Some(verifier)
}

// ── token exchange / refresh ──

/// Tokens as returned by the OAuth token endpoint (before we shape them for storage).
#[derive(Debug, Clone)]
pub struct FreshTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    /// Absolute expiry in epoch-ms (now + expires_in).
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

fn tokens_from_response(r: TokenResponse) -> FreshTokens {
    let ttl = if r.expires_in > 0 { r.expires_in } else { 3600 };
    FreshTokens {
        access_token: r.access_token,
        refresh_token: r.refresh_token,
        id_token: r.id_token,
        expires_at: crate::util::now_ms() + ttl * 1000,
    }
}

async fn post_token(params: &[(&str, &str)]) -> std::io::Result<FreshTokens> {
    let resp = reqwest::Client::new()
        .post(token_url())
        .form(params)
        .send()
        .await
        .map_err(|e| std::io::Error::other(format!("token request failed: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status();
        return Err(std::io::Error::other(format!(
            "token endpoint returned {code}"
        )));
    }
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|e| std::io::Error::other(format!("token parse failed: {e}")))?;
    Ok(tokens_from_response(body))
}

/// Exchange an authorization `code` (+ PKCE verifier) for tokens.
pub async fn exchange_code(code: &str, verifier: &str) -> std::io::Result<FreshTokens> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .await
}

/// Exchange a refresh token for a new access token.
pub async fn refresh(refresh_token: &str) -> std::io::Result<FreshTokens> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", SCOPES),
    ])
    .await
}

// ── JWT account id ──

/// Extract `chatgpt_account_id` from a JWT (access or id token) WITHOUT verifying the signature —
/// the token came straight from the TLS-authenticated token endpoint, so we only read a claim.
/// Looks at the top level and under the `https://api.openai.com/auth` claim (Codex shape).
pub fn account_id_from_jwt(jwt: &str) -> Option<String> {
    let payload_seg = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_seg.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let pick = |val: &serde_json::Value| {
        val.get("chatgpt_account_id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    pick(&v)
        .or_else(|| v.get("https://api.openai.com/auth").and_then(pick))
        .filter(|s| !s.is_empty())
}

// ── persisted token store (Codex/LiteLLM auth.json shape) ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredTokens {
    #[serde(default)]
    pub id_token: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    // Codex-compatible fields LiteLLM's chatgpt/ provider reads.
    #[serde(rename = "OPENAI_API_KEY", default)]
    pub openai_api_key: Option<String>,
    pub tokens: StoredTokens,
    // ponytail: `last_refresh` is the one field whose exact format LiteLLM may parse strictly
    // (Codex writes an ISO-8601 string; we write epoch-ms). If a deployed LiteLLM rejects it,
    // this is the single place to change. Our refresher keeps the token valid ahead of expiry so
    // LiteLLM rarely needs to act on last_refresh at all.
    #[serde(default)]
    pub last_refresh: i64,
    // Our scheduling/status fields (LiteLLM ignores unknown keys).
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub needs_relogin: bool,
}

impl AuthFile {
    pub fn from_fresh(t: &FreshTokens, account_id: Option<String>) -> Self {
        let now = crate::util::now_ms();
        AuthFile {
            openai_api_key: None,
            tokens: StoredTokens {
                id_token: t.id_token.clone(),
                access_token: t.access_token.clone(),
                refresh_token: t.refresh_token.clone(),
                account_id,
            },
            last_refresh: now,
            expires_at: t.expires_at,
            needs_relogin: false,
        }
    }

    pub fn access_token(&self) -> &str {
        &self.tokens.access_token
    }

    /// True when now (+margin) has reached the expiry.
    pub fn needs_refresh(&self, now_ms: i64, margin_ms: i64) -> bool {
        now_ms + margin_ms >= self.expires_at
    }
}

/// Account id derived from either the id token or the access token.
pub fn derive_account_id(t: &FreshTokens) -> Option<String> {
    account_id_from_jwt(&t.access_token).or_else(|| account_id_from_jwt(&t.id_token))
}

pub fn load() -> Option<AuthFile> {
    load_from(&auth_file_path())
}

pub fn load_from(path: &Path) -> Option<AuthFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(auth: &AuthFile) -> std::io::Result<()> {
    save_to(&auth_file_path(), auth)
}

pub fn save_to(path: &Path, auth: &AuthFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(auth).map_err(std::io::Error::other)?;
    // 0600 — the file holds live OAuth tokens.
    crate::engine::atomic_write::write_file_atomic_mode(path, &body, 0o600)
}

/// Delete the token store (logout). Ok if it was already absent.
pub fn clear() -> std::io::Result<()> {
    match std::fs::remove_file(auth_file_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// True when a token store exists with a non-empty access token.
pub fn is_connected() -> bool {
    load()
        .map(|a| !a.tokens.access_token.is_empty())
        .unwrap_or(false)
}

fn mark_needs_relogin() {
    if let Some(mut a) = load() {
        a.needs_relogin = true;
        let _ = save(&a);
    }
}

// ── background refresher ──

static REFRESHER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Start the refresher if one isn't already running and a token store exists. Idempotent — safe to
/// call at boot and after every login. Self-stops when the store is gone or a refresh fails
/// (needs re-login).
pub fn spawn_refresher() {
    if load().is_none() {
        return;
    }
    // Only one refresher at a time.
    if REFRESHER_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    tokio::spawn(async move {
        refresher_loop().await;
        REFRESHER_RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Milliseconds to sleep before the next refresh, floored at [MIN_REFRESH_SLEEP_MS] so a bogus
/// tiny expiry can't cause a busy loop. Pure, so it's unit-testable.
fn refresh_sleep_ms(expires_at: i64, now_ms: i64) -> u64 {
    (expires_at - now_ms - REFRESH_MARGIN_MS).max(MIN_REFRESH_SLEEP_MS) as u64
}

async fn refresher_loop() {
    loop {
        let auth = match load() {
            Some(a) if !a.needs_relogin && !a.tokens.refresh_token.is_empty() => a,
            _ => return,
        };
        let sleep_ms = refresh_sleep_ms(auth.expires_at, crate::util::now_ms());
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        // Re-load so we adopt any rotation LiteLLM performed while we slept.
        let auth = match load() {
            Some(a) if !a.needs_relogin && !a.tokens.refresh_token.is_empty() => a,
            _ => return,
        };
        match refresh(&auth.tokens.refresh_token).await {
            Ok(mut fresh) => {
                // The refresh call can take seconds; if the user logged out (token file deleted)
                // in that window, do NOT resurrect it — stop instead of re-creating the file.
                if load().is_none() {
                    return;
                }
                // OpenAI may omit a new refresh token → keep the existing one.
                if fresh.refresh_token.is_empty() {
                    fresh.refresh_token = auth.tokens.refresh_token.clone();
                }
                // A refresh response may lack an id token → keep the known account id.
                let account_id = derive_account_id(&fresh).or(auth.tokens.account_id.clone());
                let updated = AuthFile::from_fresh(&fresh, account_id);
                if save(&updated).is_ok() {
                    // Re-bake the proxy so it picks up the fresh token.
                    crate::engine::litellm::request_reload();
                }
            }
            Err(e) => {
                tracing::warn!("[chatgpt] token refresh failed ({e}); re-login required");
                mark_needs_relogin();
                return;
            }
        }
    }
}

/// Test-only: point the token store at `p` (data-race-free override; `None` restores default).
#[cfg(test)]
pub fn test_override_auth_file(p: Option<PathBuf>) {
    *AUTH_FILE_OVERRIDE.lock() = p;
}

/// Test-only: point the OAuth token endpoint at `u` (e.g. a wiremock server).
#[cfg(test)]
pub fn test_override_token_url(u: Option<String>) {
    *TOKEN_URL_OVERRIDE.lock() = u;
}

/// Test-only: serializes every test (in ANY module) that mutates the process-global auth-file /
/// token-url overrides, so they never race. Acquire it before setting an override.
#[cfg(test)]
pub static TEST_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for TestEnv {
        fn drop(&mut self) {
            *AUTH_FILE_OVERRIDE.lock() = None;
            *TOKEN_URL_OVERRIDE.lock() = None;
        }
    }
    fn with_temp_auth() -> (TestEnv, PathBuf) {
        let lock = TEST_OVERRIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatgpt-auth.json");
        *AUTH_FILE_OVERRIDE.lock() = Some(path.clone());
        (
            TestEnv {
                _dir: dir,
                _lock: lock,
            },
            path,
        )
    }

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        // Fixed verifier → deterministic challenge (RFC 7636 S256).
        let verifier = "abc123";
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(code_challenge(verifier), expect);
        // No padding, url-safe alphabet only.
        assert!(!expect.contains('='));
        assert!(!expect.contains('+') && !expect.contains('/'));
        // A generated verifier is in the PKCE length range and unreserved-only.
        let p = gen_pkce();
        assert!((43..=128).contains(&p.verifier.len()));
        assert!(p
            .verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')));
    }

    #[test]
    fn authorize_url_has_pkce_params() {
        let u = authorize_url("CHAL", "STATE");
        assert!(u.starts_with(AUTHORIZE_URL));
        // Parse back so the assertions are independent of `+` vs `%20` space encoding.
        let parsed = reqwest::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<String, String> =
            parsed.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(REDIRECT_URI));
        assert_eq!(q.get("scope").map(String::as_str), Some(SCOPES));
        assert_eq!(q.get("code_challenge").map(String::as_str), Some("CHAL"));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("state").map(String::as_str), Some("STATE"));
    }

    fn jwt_with(payload: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b64(b"{\"alg\":\"none\"}"),
            b64(payload.to_string().as_bytes()),
            "sig"
        )
    }

    #[test]
    fn account_id_extraction() {
        // top-level claim
        let t = jwt_with(serde_json::json!({"chatgpt_account_id":"acct_top"}));
        assert_eq!(account_id_from_jwt(&t).as_deref(), Some("acct_top"));
        // nested under the openai auth claim
        let n = jwt_with(
            serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"acct_nested"}}),
        );
        assert_eq!(account_id_from_jwt(&n).as_deref(), Some("acct_nested"));
        // missing claim → None
        let m = jwt_with(serde_json::json!({"sub":"u1"}));
        assert_eq!(account_id_from_jwt(&m), None);
        // empty string claim → None (treated as absent)
        let e = jwt_with(serde_json::json!({"chatgpt_account_id":""}));
        assert_eq!(account_id_from_jwt(&e), None);
        // malformed / not a JWT → None
        assert_eq!(account_id_from_jwt("not-a-jwt"), None);
        assert_eq!(account_id_from_jwt("a.!!!.c"), None);
    }

    #[test]
    fn store_round_trips_and_is_0600() {
        let (_env, path) = with_temp_auth();
        let fresh = FreshTokens {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: "it".into(),
            expires_at: crate::util::now_ms() + 3_600_000,
        };
        let a = AuthFile::from_fresh(&fresh, Some("acct1".into()));
        save(&a).unwrap();
        let loaded = load().expect("loads");
        assert_eq!(loaded.tokens, a.tokens);
        assert_eq!(loaded.access_token(), "at");
        assert!(is_connected());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be owner-only");
        }
        // logout clears it
        clear().unwrap();
        assert!(load().is_none());
        assert!(!is_connected());
    }

    #[test]
    fn needs_refresh_margin_boundaries() {
        let fresh = FreshTokens {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: String::new(),
            expires_at: 1_000_000,
        };
        let a = AuthFile::from_fresh(&fresh, None);
        let margin = 5 * 60 * 1000;
        assert!(!a.needs_refresh(1_000_000 - margin - 1, margin)); // safely before
        assert!(a.needs_refresh(1_000_000 - margin, margin)); // exactly at margin
        assert!(a.needs_refresh(2_000_000, margin)); // already expired
    }

    #[test]
    fn take_verifier_unknown_and_replay() {
        let (url, state) = begin_login();
        assert!(url.contains("state="));
        assert_eq!(take_verifier("nope"), None);
        let v = take_verifier(&state).expect("valid state");
        assert!(!v.is_empty());
        // second take of the same state → None (single-use)
        assert_eq!(take_verifier(&state), None);
    }

    #[test]
    fn take_verifier_expired_state_is_rejected() {
        // Inject a pending entry whose `created` is older than the TTL → treated as stale.
        {
            let mut g = PENDING.lock();
            let m = g.get_or_insert_with(HashMap::new);
            m.insert(
                "stale-state".to_string(),
                ("verifier".to_string(), crate::util::now_ms() - PENDING_TTL_MS - 1),
            );
        }
        assert_eq!(take_verifier("stale-state"), None, "expired state must be rejected");
        // and it was removed (not left to leak)
        assert_eq!(take_verifier("stale-state"), None);
    }

    #[test]
    fn refresh_sleep_is_floored_never_zero() {
        // Far-future expiry → the real (expiry - now - margin) delay.
        assert_eq!(refresh_sleep_ms(10_000_000, 0), (10_000_000 - REFRESH_MARGIN_MS) as u64);
        // Tiny positive expiry (< margin) or already-expired → floored, never 0 (the busy-loop bug).
        assert_eq!(refresh_sleep_ms(1_000, 0), MIN_REFRESH_SLEEP_MS as u64);
        assert_eq!(refresh_sleep_ms(0, 5_000_000), MIN_REFRESH_SLEEP_MS as u64);
        assert!(refresh_sleep_ms(REFRESH_MARGIN_MS, 0) >= MIN_REFRESH_SLEEP_MS as u64);
    }

    #[tokio::test]
    async fn exchange_and_refresh_against_mock() {
        let (_env, _path) = with_temp_auth();
        let server = wiremock::MockServer::start().await;
        *TOKEN_URL_OVERRIDE.lock() = Some(format!("{}/token", server.uri()));
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token":"AT","refresh_token":"RT","id_token":"IT","expires_in":3600
            })))
            .mount(&server)
            .await;
        let t = exchange_code("code", "verifier").await.expect("exchange ok");
        assert_eq!(t.access_token, "AT");
        assert_eq!(t.refresh_token, "RT");
        assert!(t.expires_at > crate::util::now_ms());
        let r = refresh("RT").await.expect("refresh ok");
        assert_eq!(r.access_token, "AT");
    }

    #[tokio::test]
    async fn exchange_and_refresh_failure_is_err() {
        let (_env, _path) = with_temp_auth();
        let server = wiremock::MockServer::start().await;
        *TOKEN_URL_OVERRIDE.lock() = Some(format!("{}/token", server.uri()));
        use wiremock::matchers::{method, path};
        // Non-2xx from the token endpoint → both exchange and refresh return Err (drives the
        // login/complete 502 and the refresher's needs-relogin paths respectively).
        wiremock::Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(400))
            .mount(&server)
            .await;
        assert!(exchange_code("bad-code", "verifier").await.is_err());
        assert!(refresh("dead-token").await.is_err());
    }
}
