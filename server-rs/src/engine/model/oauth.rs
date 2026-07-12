//! ChatGPT subscription OAuth (Authorization Code + PKCE/S256) for the delegate router.
//!
//! A subscription "provider" is a normal registered [`Provider`](super::providers::Provider) with
//! `protocol = openai` and `oauth = Some(<account>)`. The bearer sent to the ChatGPT backend is a
//! short-lived JWT access token; it is deliberately NOT stored in the human-editable providers.json
//! — it lives here in a separate 0600 store (`~/.agentic-dev/oauth/<account>.json`) alongside the
//! long-lived refresh token, and is rotated before expiry by [`spawn_refresh_task`].
//!
//! This is the same OAuth app the Codex CLI uses (client_id / redirect / scopes below), so the login
//! is completed by a browser hitting the fixed loopback redirect `http://localhost:1455/...`; the
//! callback listener lives in `api::oauth`.
#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const REDIRECT_PORT: u16 = 1455;
pub const SCOPES: &str = "openid profile email offline_access";

/// Base URL of the ChatGPT subscription (codex) backend. The bearer resolved from the OAuth store
/// is ONLY handed to a provider pointed here (see `Provider::oauth_account`) — a provider with any
/// other base_url must never receive the subscription token.
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// The default account key — one ChatGPT login backs every registered GPT model.
pub const DEFAULT_ACCOUNT: &str = "chatgpt";

/// Sentinel written into a `Provider.api_key_env` to mark "the bearer is the rotating OAuth token
/// for this account", e.g. `oauth:chatgpt`. Reuses the existing "reference to where the secret
/// lives" field so the token itself never lands in the human-editable providers.json.
pub const KEY_REF_PREFIX: &str = "oauth:";

/// Build the `api_key_env` reference value for `account`.
pub fn key_ref(account: &str) -> String {
    format!("{KEY_REF_PREFIX}{account}")
}

/// The account named by an `oauth:<account>` reference, else None. The account is also the file
/// stem in the 0600 store, so reject anything but `[A-Za-z0-9_-]` — a provider registered (via the
/// API, from user input) with `api_key_env = "oauth:../secret"` must NOT let `store_path` escape the
/// oauth dir. An invalid ref reads as "no oauth account", leaving the provider inert.
pub fn account_from_key_ref(s: &str) -> Option<&str> {
    let account = s.strip_prefix(KEY_REF_PREFIX).filter(|a| !a.is_empty())?;
    if account
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(account)
    } else {
        None
    }
}

/// Refresh once the access token has this many seconds or fewer left (short-lived JWT, ~1h).
const REFRESH_SKEW_SECS: u64 = 300;

/// Persisted OAuth credentials for one ChatGPT account. `access_token` is the rotating bearer;
/// `refresh_token` mints the next one. Stored 0600 — never in providers.json.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OauthCreds {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch seconds at which `access_token` expires.
    pub expires_at: u64,
    /// `chatgpt_account_id` claim — sent as the `ChatGPT-Account-Id` header on every call.
    #[serde(default)]
    pub account_id: String,
    /// `email` claim, for the UI "signed in as" line. Best-effort.
    #[serde(default)]
    pub email: String,
    /// Set when the refresh token was rejected (`invalid_grant`) — the UI must prompt a re-login.
    #[serde(default)]
    pub needs_reauth: bool,
}

impl OauthCreds {
    /// True when the access token is missing or within the refresh skew of expiry.
    pub fn is_stale(&self) -> bool {
        self.access_token.is_empty() || self.expires_at <= now() + REFRESH_SKEW_SECS
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── store (0600) ──

/// Test-only override for the oauth dir (data-race-free, unlike `env::set_var`; mirrors
/// `providers::PROVIDERS_FILE_OVERRIDE`). Always `None` in production.
pub static OAUTH_DIR_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// `~/.agentic-dev/oauth` (or the test override).
pub fn oauth_dir() -> PathBuf {
    if let Some(p) = OAUTH_DIR_OVERRIDE.lock().clone() {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agentic-dev").join("oauth")
}

pub fn store_path(account: &str) -> PathBuf {
    oauth_dir().join(format!("{account}.json"))
}

pub fn load(account: &str) -> Option<OauthCreds> {
    load_from(&store_path(account))
}

fn load_from(path: &Path) -> Option<OauthCreds> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(account: &str, c: &OauthCreds) -> std::io::Result<()> {
    save_to(&store_path(account), c)
}

fn save_to(path: &Path, c: &OauthCreds) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_string_pretty(c).map_err(std::io::Error::other)?;
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

/// Every stored account (file stems in the oauth dir). Empty when none.
pub fn list_accounts() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(oauth_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out
}

/// Delete an account's stored credentials. Ok(true) when a file was removed.
pub fn logout(account: &str) -> std::io::Result<bool> {
    let path = store_path(account);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// ── PKCE ──

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Fresh PKCE pair: 256-bit verifier (base64url, no pad) + its S256 challenge. Entropy comes from
/// two v4 UUIDs (the crate already depends on `uuid`), avoiding a `rand`/`getrandom` dependency.
pub fn gen_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// A random opaque `state` for CSRF protection on the callback.
pub fn gen_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The authorize URL to open in the browser.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut u = reqwest::Url::parse(AUTH_URL).expect("static authorize URL parses");
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        // Codex-app flags: attach org info to the id_token and use the simplified consent flow so the
        // issued token carries the `chatgpt_account_id` the codex backend expects. (Connector scopes
        // are intentionally NOT requested — the confirmed scope set is identity + offline only.)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    u.to_string()
}

// ── token endpoint ──

/// Exchange an authorization `code` (+ PKCE verifier) for tokens.
pub fn exchange_code(code: &str, verifier: &str) -> Result<OauthCreds, String> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let body = token_post(&params)?;
    parse_token_response(&body, "")
}

/// Mint a new access token from a refresh token.
pub fn refresh_creds(refresh_token: &str) -> Result<OauthCreds, String> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", SCOPES),
    ];
    let body = token_post(&params)?;
    // The refresh response may omit refresh_token/account_id — keep the old refresh token as fallback.
    parse_token_response(&body, refresh_token)
}

fn token_post(params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let resp = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .form(params)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| format!("token response not JSON: {e}"))?;
    if !status.is_success() {
        // Surface the OpenAI `error` code (e.g. `invalid_grant`) so callers can detect a dead
        // refresh token vs. a transient failure.
        let code = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(format!("token endpoint {status}: {code}"));
    }
    Ok(body)
}

/// Build creds from a token response. `fallback_refresh` fills refresh_token when the response
/// omits it (refresh grants often do). Pure — unit-tested without network.
fn parse_token_response(
    body: &serde_json::Value,
    fallback_refresh: &str,
) -> Result<OauthCreds, String> {
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("token response missing access_token")?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_refresh)
        .to_string();
    let expires_in = body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
    // Prefer claims from the access token, then the id_token, for account id + email.
    let access_claims = jwt_claims(&access_token);
    let id_claims = body
        .get("id_token")
        .and_then(|v| v.as_str())
        .and_then(jwt_claims);
    let account_id = access_claims
        .as_ref()
        .and_then(account_id_from_claims)
        .or_else(|| id_claims.as_ref().and_then(account_id_from_claims))
        .unwrap_or_default();
    let email = id_claims
        .as_ref()
        .and_then(email_from_claims)
        .or_else(|| access_claims.as_ref().and_then(email_from_claims))
        .unwrap_or_default();
    Ok(OauthCreds {
        access_token,
        refresh_token,
        expires_at: now() + expires_in,
        account_id,
        email,
        needs_reauth: false,
    })
}

/// Decode a JWT's payload segment (base64url) into JSON. `None` on any malformation — we never
/// verify the signature (the token is opaque to us; we only read claims we already trust the issuer for).
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `chatgpt_account_id`, looked up under the namespaced `auth` claim first, then top-level.
fn account_id_from_claims(c: &serde_json::Value) -> Option<String> {
    c.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .or_else(|| c.get("chatgpt_account_id").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn email_from_claims(c: &serde_json::Value) -> Option<String> {
    c.get("email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ── accessors + refresh ──

/// The cached access token for `account` (kept fresh by the refresh task; callers on the hot path
/// must not block on the network). `None` when logged out, empty, past expiry, or needing re-auth —
/// so a dead account's provider drops out of delegate candidacy and the LiteLLM config instead of
/// shipping a token every call would 401 on.
pub fn access_token(account: &str) -> Option<String> {
    let c = load(account)?;
    if c.needs_reauth || c.access_token.is_empty() || c.expires_at <= now() {
        return None;
    }
    Some(c.access_token)
}

/// The `chatgpt_account_id` for `account` (for the `ChatGPT-Account-Id` header).
pub fn account_id(account: &str) -> Option<String> {
    load(account).map(|c| c.account_id).filter(|s| !s.is_empty())
}

/// UI-facing login status for one account.
#[derive(Debug, Serialize)]
pub struct Status {
    pub account: String,
    pub logged_in: bool,
    pub email: String,
    pub account_id: String,
    pub expires_at: u64,
    pub needs_reauth: bool,
}

pub fn status(account: &str) -> Status {
    match load(account) {
        Some(c) => Status {
            account: account.to_string(),
            logged_in: !c.access_token.is_empty(),
            email: c.email,
            account_id: c.account_id,
            expires_at: c.expires_at,
            needs_reauth: c.needs_reauth,
        },
        None => Status {
            account: account.to_string(),
            logged_in: false,
            email: String::new(),
            account_id: String::new(),
            expires_at: 0,
            needs_reauth: false,
        },
    }
}

/// Refresh `account` if its token is stale. Ok(true) when it actually refreshed (caller should then
/// ask litellm to reload so the new bearer reaches the proxy). Blocking network — call off the async
/// runtime (`spawn_blocking`).
pub fn refresh_if_needed(account: &str) -> Result<bool, String> {
    let c = match load(account) {
        Some(c) => c,
        None => return Ok(false),
    };
    if c.needs_reauth {
        return Ok(false); // dead refresh token — don't hammer the endpoint; UI prompts re-login
    }
    if !c.is_stale() {
        return Ok(false);
    }
    // The refresh is a blocking network round-trip; a concurrent login could replace the creds
    // while it's in flight. Guard both outcomes on "the refresh token we used is still the current
    // one", so a stale in-flight refresh never clobbers a freshly-issued login (TOCTOU).
    match refresh_creds(&c.refresh_token) {
        Ok(mut fresh) => {
            if superseded(account, &c.refresh_token) {
                return Ok(false);
            }
            // Preserve identity fields the refresh response may have omitted.
            if fresh.account_id.is_empty() {
                fresh.account_id = c.account_id.clone();
            }
            if fresh.email.is_empty() {
                fresh.email = c.email.clone();
            }
            save(account, &fresh).map_err(|e| format!("saving refreshed creds: {e}"))?;
            Ok(true)
        }
        Err(e) => {
            // Only latch needs_reauth if the token we tried is still current — an old token failing
            // must not mark a just-completed re-login as dead.
            if e.contains("invalid_grant") && !superseded(account, &c.refresh_token) {
                if let Some(mut cur) = load(account) {
                    cur.needs_reauth = true;
                    let _ = save(account, &cur);
                }
            }
            Err(e)
        }
    }
}

/// True when the stored refresh token no longer matches `used` — i.e. a newer login/refresh landed
/// while we were mid-request, so our result is stale and must not be written back.
fn superseded(account: &str, used: &str) -> bool {
    match load(account) {
        Some(cur) => cur.refresh_token != used,
        None => true, // logged out mid-refresh → don't resurrect
    }
}

/// Background task: every 60s, refresh any account nearing expiry and reload the LiteLLM proxy so the
/// rotated bearer takes effect. Cheap no-op when no accounts are stored. Runs an immediate first pass
/// so a boot with an already-expired token self-heals within one tick.
pub fn spawn_refresh_task() {
    // Idempotent: called both at startup and after each login — a second loop would double-refresh
    // and race the single-use refresh token (the loser gets `invalid_grant` and wrongly latches
    // `needs_reauth`). Mirror `litellm::start_supervisor`'s OnceLock guard.
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let mut changed = false;
            for account in list_accounts() {
                let acct = account.clone();
                match tokio::task::spawn_blocking(move || refresh_if_needed(&acct)).await {
                    Ok(Ok(true)) => {
                        changed = true;
                        tracing::info!("[oauth] refreshed access token for '{account}'");
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(e)) => tracing::warn!("[oauth] refresh '{account}' failed: {e}"),
                    Err(e) => tracing::warn!("[oauth] refresh task join error: {e}"),
                }
            }
            if changed {
                crate::engine::litellm::request_reload();
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the process-global `OAUTH_DIR_OVERRIDE` (cargo runs tests in
    /// parallel; two of them pointing the override at different tempdirs would clobber each other).
    static OVERRIDE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = gen_pkce();
        // verifier is 43 chars (32 bytes base64url-no-pad), challenge is its SHA-256 base64url.
        assert_eq!(p.verifier.len(), 43);
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // no padding / url-unsafe chars leak through
        assert!(!p.verifier.contains('=') && !p.verifier.contains('+') && !p.verifier.contains('/'));
        // two fresh pairs differ
        assert_ne!(gen_pkce().verifier, gen_pkce().verifier);
    }

    #[test]
    fn authorize_url_has_required_params() {
        let u = authorize_url("CHAL", "STATE");
        assert!(u.starts_with(AUTH_URL));
        for needle in [
            "response_type=code",
            "code_challenge=CHAL",
            "code_challenge_method=S256",
            "state=STATE",
            "scope=openid+profile+email+offline_access",
        ] {
            assert!(u.contains(needle), "authorize URL missing {needle}: {u}");
        }
        // client_id + redirect are percent-encoded but present
        assert!(u.contains(CLIENT_ID));
        assert!(u.contains("redirect_uri=http"));
    }

    /// Build a fake unsigned JWT with the given payload JSON (header.payload.sig, base64url no pad).
    fn fake_jwt(payload: serde_json::Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let p = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        format!("{h}.{p}.sig")
    }

    #[test]
    fn parse_token_response_reads_claims_and_expiry() {
        let access = fake_jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"},
            "email": "should-not-win@x.com"
        }));
        let id = fake_jwt(serde_json::json!({"email": "user@example.com"}));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "rt-1",
            "id_token": id,
            "expires_in": 3600
        });
        let c = parse_token_response(&body, "").unwrap();
        assert_eq!(c.account_id, "acct-123");
        // id_token email wins over the access token's for the display line
        assert_eq!(c.email, "user@example.com");
        assert_eq!(c.refresh_token, "rt-1");
        assert!(c.expires_at >= now() + 3590 && c.expires_at <= now() + 3610);
        assert!(!c.needs_reauth);
    }

    #[test]
    fn parse_token_response_uses_fallback_refresh_and_toplevel_account() {
        // refresh grant: no refresh_token in body, account id at top level of the access JWT.
        let access = fake_jwt(serde_json::json!({"chatgpt_account_id": "acct-top"}));
        let body = serde_json::json!({"access_token": access, "expires_in": 60});
        let c = parse_token_response(&body, "old-rt").unwrap();
        assert_eq!(c.refresh_token, "old-rt", "omitted refresh_token falls back");
        assert_eq!(c.account_id, "acct-top");
    }

    #[test]
    fn parse_token_response_rejects_missing_access_token() {
        assert!(parse_token_response(&serde_json::json!({"expires_in": 60}), "").is_err());
        assert!(parse_token_response(&serde_json::json!({"access_token": ""}), "").is_err());
    }

    #[test]
    fn jwt_claims_tolerates_garbage() {
        assert!(jwt_claims("not-a-jwt").is_none());
        assert!(jwt_claims("a.b").is_none()); // b isn't valid base64url json
        assert!(jwt_claims("").is_none());
    }

    #[test]
    fn store_roundtrip_and_perms() {
        let _lock = OVERRIDE_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        *OAUTH_DIR_OVERRIDE.lock() = Some(dir.path().to_path_buf());
        // guard resets the override even if an assert panics
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                *OAUTH_DIR_OVERRIDE.lock() = None;
            }
        }
        let _r = Reset;

        assert!(load("chatgpt").is_none());
        assert!(access_token("chatgpt").is_none());
        let c = OauthCreds {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: now() + 3600,
            account_id: "acct".into(),
            email: "e@x.com".into(),
            needs_reauth: false,
        };
        save("chatgpt", &c).unwrap();
        let back = load("chatgpt").unwrap();
        assert_eq!(back.access_token, "at");
        assert_eq!(access_token("chatgpt").as_deref(), Some("at"));
        assert_eq!(account_id("chatgpt").as_deref(), Some("acct"));
        assert_eq!(list_accounts(), vec!["chatgpt".to_string()]);
        let st = status("chatgpt");
        assert!(st.logged_in && !st.needs_reauth && st.email == "e@x.com");

        // A Provider carrying the `oauth:` key ref resolves its bearer from this store (the wiring
        // litellm's build_config depends on) — and reports it as native-keyed for candidacy.
        let p = crate::engine::providers::Provider {
            name: "gpt-5".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            api_key: String::new(),
            api_key_env: Some(key_ref("chatgpt")),
            model: "gpt-5".into(),
            protocol: crate::engine::providers::Protocol::Openai,
            capability: 0.9,
            description: None,
            priority: 0.5,
            cost: 0.6,
            router: false,
            enabled: true,
        };
        assert_eq!(p.oauth_account(), Some("chatgpt"));
        assert_eq!(p.resolved_key(), "at", "bearer comes from the oauth store, not env");
        // Security: the SAME oauth ref on a provider pointed at a NON-codex endpoint must NOT resolve
        // the subscription bearer (else a user-registered provider could exfiltrate the token).
        let evil = crate::engine::providers::Provider {
            base_url: "https://evil.example.com".into(),
            ..p.clone()
        };
        assert_eq!(evil.oauth_account(), None, "non-codex base_url must not get the token");
        assert_eq!(evil.resolved_key(), "");

        // needs_reauth / expiry hide the token so a dead account drops out of candidacy.
        save(
            "chatgpt",
            &OauthCreds {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: now() + 3600,
                needs_reauth: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(access_token("chatgpt"), None, "needs_reauth hides the token");
        assert_eq!(p.resolved_key(), "", "reauth-needed provider resolves to no key");
        save(
            "chatgpt",
            &OauthCreds {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: now().saturating_sub(1), // expired
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(access_token("chatgpt"), None, "expired token is hidden");
        // superseded() detects a rotated refresh token vs. the one a stale refresh used.
        assert!(!superseded("chatgpt", "rt"), "current token is not superseded");
        assert!(superseded("chatgpt", "old-rt"), "a rotated token is superseded");
        assert!(superseded("nobody", "rt"), "logged-out account is superseded");
        // restore a valid record for the remaining assertions
        save(&"chatgpt".to_string(), &c).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path("chatgpt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "creds file must be 0600");
        }

        assert!(logout("chatgpt").unwrap());
        assert!(!logout("chatgpt").unwrap());
        assert!(load("chatgpt").is_none());
    }

    #[test]
    fn key_ref_rejects_path_traversal() {
        assert_eq!(account_from_key_ref("oauth:chatgpt"), Some("chatgpt"));
        assert_eq!(account_from_key_ref("oauth:my-acct_2"), Some("my-acct_2"));
        // anything that could escape the store dir (or isn't a plain stem) → None (inert provider)
        assert_eq!(account_from_key_ref("oauth:../secret"), None);
        assert_eq!(account_from_key_ref("oauth:a/b"), None);
        assert_eq!(account_from_key_ref("oauth:a.json"), None);
        assert_eq!(account_from_key_ref("oauth:"), None);
        assert_eq!(account_from_key_ref("MINIMAX_API_KEY"), None);
        // and store_path for a valid account stays inside the oauth dir
        assert_eq!(
            store_path("chatgpt").file_name().unwrap().to_str(),
            Some("chatgpt.json")
        );
    }

    #[test]
    fn is_stale_boundary() {
        let mut c = OauthCreds {
            access_token: "at".into(),
            expires_at: now() + 3600,
            ..Default::default()
        };
        assert!(!c.is_stale());
        c.expires_at = now() + 10; // within skew
        assert!(c.is_stale());
        c.expires_at = now() + 3600;
        c.access_token.clear(); // no token → always stale
        assert!(c.is_stale());
    }

    #[test]
    fn refresh_if_needed_skips_fresh_and_dead_tokens() {
        let _lock = OVERRIDE_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        *OAUTH_DIR_OVERRIDE.lock() = Some(dir.path().to_path_buf());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                *OAUTH_DIR_OVERRIDE.lock() = None;
            }
        }
        let _r = Reset;

        // No account → Ok(false), no network.
        assert_eq!(refresh_if_needed("chatgpt").unwrap(), false);
        // Fresh token → Ok(false), no network.
        save(
            "chatgpt",
            &OauthCreds {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: now() + 3600,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(refresh_if_needed("chatgpt").unwrap(), false);
        // needs_reauth → Ok(false), no network (even though stale).
        save(
            "chatgpt",
            &OauthCreds {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: now(),
                needs_reauth: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(refresh_if_needed("chatgpt").unwrap(), false);
    }
}
