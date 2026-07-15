//! ChatGPT subscription OAuth: token store + PKCE + token exchange/refresh.
//!
//! A user connects their personal ChatGPT subscription (OAuth login, NOT an API key). The short-lived
//! access token (a JWT) is stored in a 0600 file SEPARATE from the human-editable providers.json — the
//! registered provider only holds a sentinel pointer (`AGENTIC_OAUTH:chatgpt`) in `api_key_env`, and
//! `Provider::resolved_key()` reads the live token from here. A background task refreshes the token
//! before it expires and reloads the LiteLLM proxy so the worker→proxy bearer stays fresh.
//!
//! GPT reaches the delegate fan-out as an ordinary `openai`-protocol provider THROUGH the LiteLLM proxy
//! (see `litellm.rs`), with the ChatGPT-specific headers layered on. GPT is a worker only — the main
//! session runs on the Claude Agent SDK.
#![allow(dead_code)]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::util::now_ms;

// ── OAuth endpoints / client (the same public client Codex CLI uses, mid-2026) ──
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPES: &str = "openid profile email offline_access";

// ── ChatGPT backend (Responses API) the worker's key/headers target ──
/// Base URL for the registered `chatgpt` provider. LiteLLM appends `/chat/completions`; the real
/// Codex endpoint is `/responses`.
// ponytail: base_url + rotating bearer + headers are the deliverable; full Responses-API path
// translation depends on the litellm build's codex support — upgrade litellm / add a `/responses`
// shim if a real subscription call needs the exact endpoint.
pub const BACKEND_BASE: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_MODEL: &str = "gpt-5";
pub const ORIGINATOR: &str = "codex_cli_rs";
pub const OPENAI_BETA: &str = "responses=experimental";
pub const ACCOUNT_HEADER: &str = "ChatGPT-Account-Id";

/// Sentinel stored in a provider's `api_key_env` to mark it OAuth-backed (token lives here, not in
/// providers.json). Only `chatgpt` is supported today.
pub const SENTINEL: &str = "AGENTIC_OAUTH:chatgpt";
/// Registered provider name for the ChatGPT subscription.
pub const PROVIDER_NAME: &str = "chatgpt";

/// Persisted subscription tokens. Serde-defaulted so an older/partial file still loads.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatGptTokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: String,
    /// Unix ms when `access_token` expires.
    #[serde(default)]
    pub expires_at: i64,
    /// Set when refresh hard-fails (invalid_grant): the user must log in again.
    #[serde(default)]
    pub needs_relogin: bool,
}

// ── token file location (0600) ──

/// Test-only override for the token file path (data-race-free, unlike `env::set_var`).
pub static TOKEN_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// The token file: the test override if set, else `AGENTIC_OAUTH_CHATGPT_FILE`, else
/// `~/.agentic-dev/oauth/chatgpt.json`.
pub fn token_file_path() -> PathBuf {
    if let Some(p) = TOKEN_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_OAUTH_CHATGPT_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("oauth")
        .join("chatgpt.json")
}

/// Serializes read-modify-write of the token file across concurrent API requests + the refresh task.
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub fn load() -> Option<ChatGptTokens> {
    load_from(&token_file_path())
}

pub fn load_from(path: &Path) -> Option<ChatGptTokens> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(t: &ChatGptTokens) -> io::Result<()> {
    let _g = FILE_LOCK.lock();
    save_to(&token_file_path(), t)
}

/// Atomic write (temp + rename), 0600 on unix — the file holds a bearer token.
pub fn save_to(path: &Path, t: &ChatGptTokens) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_string_pretty(t).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn remove() -> io::Result<()> {
    let _g = FILE_LOCK.lock();
    let path = token_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The current usable access token, or empty when not connected / relogin required. Read by
/// `Provider::resolved_key()` for the `chatgpt` sentinel and by the LiteLLM config builder.
pub fn current_access_token() -> String {
    match load() {
        Some(t) if !t.needs_relogin => t.access_token,
        _ => String::new(),
    }
}

/// The stored ChatGPT-Account-Id (empty when not connected). Emitted as a LiteLLM extra header.
pub fn current_account_id() -> String {
    load().map(|t| t.account_id).unwrap_or_default()
}

// ── PKCE + state ──

/// Read `n` random bytes from the OS. `/dev/urandom` keeps this dependency-free (no `rand`/`getrandom`
/// crate); on the unlikely read failure we fall back to a time+pid seed rather than panic.
fn rand_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = vec![0u8; n];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // Fallback: hash a time+pid seed to fill n bytes (never expected in production on Linux).
    let seed = format!("{}-{}", now_ms(), std::process::id());
    let mut out = Vec::with_capacity(n);
    let mut ctr = 0u64;
    while out.len() < n {
        let mut h = Sha256::new();
        h.update(seed.as_bytes());
        h.update(ctr.to_le_bytes());
        out.extend_from_slice(&h.finalize());
        ctr += 1;
    }
    out.truncate(n);
    out
}

/// PKCE code_verifier: 32 random bytes → base64url (43 chars, no padding). RFC 7636.
pub fn generate_verifier() -> String {
    URL_SAFE_NO_PAD.encode(rand_bytes(32))
}

/// PKCE code_challenge for the S256 method: base64url(SHA256(verifier)).
pub fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Opaque CSRF/state value: 32 random bytes → base64url.
pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(rand_bytes(32))
}

/// Build the authorize URL. `reqwest::Url` percent-encodes the params (scopes have spaces, the
/// redirect has `://`), so we never hand-roll escaping.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ],
    )
    .expect("authorize url is a valid base")
    .to_string()
}

/// Extract `chatgpt_account_id` from a JWT (id_token or access token). We decode the payload segment
/// only — no signature check: this is our own token from the OAuth exchange, used solely to read a
/// claim. Looks at the top-level claim and the `https://api.openai.com/auth` namespace Codex uses.
pub fn parse_account_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if let Some(s) = v.get("chatgpt_account_id").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

// ── token exchange / refresh (HTTPS; never called from tests) ──

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// Exchange an authorization `code` (+ PKCE verifier) for tokens.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<ChatGptTokens, String> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .await
}

/// Refresh the access token. The response may omit `refresh_token`/`account_id`; the caller carries
/// the previous values forward.
pub async fn refresh(refresh_token: &str) -> Result<ChatGptTokens, String> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", SCOPES),
    ])
    .await
}

async fn post_token(params: &[(&str, &str)]) -> Result<ChatGptTokens, String> {
    let resp = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(params)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Keep the upstream body so callers can detect invalid_grant.
        return Err(format!("token endpoint {}: {body}", status.as_u16()));
    }
    let tr: TokenResp =
        serde_json::from_str(&body).map_err(|e| format!("bad token response: {e}"))?;
    let account_id = parse_account_id(&tr.id_token)
        .or_else(|| parse_account_id(&tr.access_token))
        .unwrap_or_default();
    // Default to 1h when the endpoint omits expires_in: expires_at must stay in the future or the
    // refresh loop would treat the token as already-expired and hot-spin against the token endpoint.
    let ttl_secs = if tr.expires_in > 0 { tr.expires_in } else { 3600 };
    let expires_at = now_ms() + ttl_secs * 1000;
    Ok(ChatGptTokens {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        id_token: tr.id_token,
        account_id,
        expires_at,
        needs_relogin: false,
    })
}

/// An OAuth error is `invalid_grant` (refresh token revoked/expired) → user must re-login.
pub fn is_invalid_grant(err: &str) -> bool {
    err.contains("invalid_grant")
}

// ── pending-verifier map (start → complete correlation, keyed by state, TTL) ──

const PENDING_TTL_MS: i64 = 10 * 60_000;
// LazyLock: HashMap::new() isn't const-constructible (RandomState seed), so it can't sit in a plain
// `static parking_lot::Mutex::new(HashMap::new())`.
static PENDING: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, (String, i64)>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn prune_pending(map: &mut HashMap<String, (String, i64)>, now: i64) {
    map.retain(|_, (_, at)| now - *at < PENDING_TTL_MS);
}

/// Remember the PKCE verifier for a `state` returned by /start (consumed by /complete).
pub fn remember_verifier(state: &str, verifier: &str) {
    let now = now_ms();
    let mut map = PENDING.lock();
    prune_pending(&mut map, now);
    map.insert(state.to_string(), (verifier.to_string(), now));
}

/// Take (and remove) the verifier for a `state`. `None` if unknown or expired.
pub fn take_verifier(state: &str) -> Option<String> {
    let now = now_ms();
    let mut map = PENDING.lock();
    prune_pending(&mut map, now);
    map.remove(state).map(|(v, _)| v)
}

// ── background auto-refresh ──

/// Spawn the token auto-refresh loop. Wakes ~5 min before expiry, refreshes, and reloads the LiteLLM
/// proxy so the worker→proxy bearer stays fresh. On invalid_grant it marks `needs_relogin` and idles
/// until the user logs in again. No-op cost when not connected. Call once after server boot.
pub fn start_refresh_task() {
    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};
        const MARGIN_MS: i64 = 5 * 60_000;
        // Cap a single sleep so a far-future expiry (or a login that lands after we slept) is
        // re-checked periodically instead of blocking for hours.
        const MAX_SLEEP_MS: i64 = 60 * 60_000;
        loop {
            let Some(tok) = load() else {
                sleep(Duration::from_secs(300)).await;
                continue;
            };
            if tok.needs_relogin || tok.refresh_token.is_empty() {
                sleep(Duration::from_secs(300)).await;
                continue;
            }
            let wait = tok.expires_at - now_ms() - MARGIN_MS;
            if wait > 0 {
                let ms = wait.min(MAX_SLEEP_MS) as u64;
                sleep(Duration::from_millis(ms)).await;
                continue; // re-load and re-check (token may have changed underneath us)
            }
            match refresh(&tok.refresh_token).await {
                Ok(mut fresh) => {
                    // Carry forward values the refresh response may omit.
                    if fresh.refresh_token.is_empty() {
                        fresh.refresh_token = tok.refresh_token.clone();
                    }
                    if fresh.account_id.is_empty() {
                        fresh.account_id = tok.account_id.clone();
                    }
                    if let Err(e) = save(&fresh) {
                        // Back off on a persistent write failure. Without this the next iteration
                        // re-reads the still-expired token and refreshes again immediately — hot-
                        // spinning against the token endpoint.
                        tracing::error!("[oauth] save refreshed token failed: {e}; retrying in 60s");
                        sleep(Duration::from_secs(60)).await;
                    } else {
                        tracing::info!("[oauth] refreshed ChatGPT access token");
                        crate::engine::litellm::request_reload();
                    }
                }
                Err(e) if is_invalid_grant(&e) => {
                    tracing::warn!("[oauth] refresh token invalid — ChatGPT needs re-login");
                    let mut t = tok.clone();
                    t.needs_relogin = true;
                    // If this save fails the loop can't learn `needs_relogin` from disk, so back off
                    // to avoid re-hitting the endpoint every iteration with a doomed refresh.
                    if save(&t).is_err() {
                        sleep(Duration::from_secs(60)).await;
                    }
                    crate::engine::litellm::request_reload();
                }
                Err(e) => {
                    tracing::warn!("[oauth] token refresh failed: {e}; retrying in 60s");
                    sleep(Duration::from_secs(60)).await;
                }
            }
        }
    });
}

/// Test-only isolation of the token file, shared across the whole crate's tests so the process-global
/// `TOKEN_FILE_OVERRIDE` is never clobbered by two test modules running in parallel. Any test that
/// also isolates the providers file must take THAT lock first, then this one, to keep a single order.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    pub(crate) static FILE_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) struct TokenFileGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for TokenFileGuard {
        fn drop(&mut self) {
            *TOKEN_FILE_OVERRIDE.lock() = None;
        }
    }

    /// Point the token store at a fresh temp file for the duration of a test.
    pub(crate) fn isolate() -> TokenFileGuard {
        let lock = FILE_OVERRIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        *TOKEN_FILE_OVERRIDE.lock() = Some(dir.path().join("chatgpt.json"));
        TokenFileGuard {
            _dir: dir,
            _lock: lock,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_util::isolate as isolated_file;

    fn sample() -> ChatGptTokens {
        ChatGptTokens {
            access_token: "at-123".into(),
            refresh_token: "rt-456".into(),
            id_token: "id-789".into(),
            account_id: "acct_abc".into(),
            expires_at: 1_700_000_000_000,
            needs_relogin: false,
        }
    }

    #[test]
    fn save_load_roundtrip_and_remove() {
        let _g = isolated_file();
        assert!(load().is_none());
        let t = sample();
        save(&t).unwrap();
        assert_eq!(load().unwrap(), t);
        remove().unwrap();
        assert!(load().is_none());
        // remove is idempotent.
        remove().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _g = isolated_file();
        save(&sample()).unwrap();
        let mode = std::fs::metadata(token_file_path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be private");
    }

    #[test]
    fn current_access_token_respects_relogin() {
        let _g = isolated_file();
        assert_eq!(current_access_token(), "");
        save(&sample()).unwrap();
        assert_eq!(current_access_token(), "at-123");
        assert_eq!(current_account_id(), "acct_abc");
        // A relogin flag hides the (now useless) token from routing.
        let mut t = sample();
        t.needs_relogin = true;
        save(&t).unwrap();
        assert_eq!(current_access_token(), "");
    }

    #[test]
    fn pkce_encoding_matches_spec() {
        let v = generate_verifier();
        assert_eq!(v.len(), 43, "32 bytes → 43 base64url chars");
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // RFC 7636 vector: verifier "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // → challenge "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_ne!(generate_state(), generate_state(), "state is random");
    }

    #[test]
    fn authorize_url_carries_pkce_and_client() {
        let url = authorize_url("CHAL", "STATE");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("response_type=code"));
        // redirect + scopes are percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid+profile+email+offline_access"));
    }

    #[test]
    fn account_id_parsed_from_jwt_claim_and_namespace() {
        let mk = |payload: &serde_json::Value| {
            let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
            format!("header.{body}.sig")
        };
        // top-level claim
        let jwt = mk(&serde_json::json!({"chatgpt_account_id": "acct_top"}));
        assert_eq!(parse_account_id(&jwt).as_deref(), Some("acct_top"));
        // nested under the openai auth namespace
        let jwt2 = mk(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_ns"}
        }));
        assert_eq!(parse_account_id(&jwt2).as_deref(), Some("acct_ns"));
        // malformed → None (no panic)
        assert_eq!(parse_account_id("not-a-jwt"), None);
        assert_eq!(parse_account_id("a.b.c"), None);
    }

    #[test]
    fn pending_verifier_take_is_one_shot() {
        remember_verifier("s-1", "v-1");
        assert_eq!(take_verifier("s-1").as_deref(), Some("v-1"));
        // consumed
        assert_eq!(take_verifier("s-1"), None);
        // unknown state
        assert_eq!(take_verifier("nope"), None);
    }

    #[test]
    fn invalid_grant_detection() {
        assert!(is_invalid_grant("token endpoint 400: {\"error\":\"invalid_grant\"}"));
        assert!(!is_invalid_grant("token endpoint 500: server error"));
    }
}
