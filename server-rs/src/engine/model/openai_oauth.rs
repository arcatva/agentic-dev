//! ChatGPT subscription OAuth (Codex CLI shape): PKCE Authorization-Code flow, a 0600 token
//! store, and access-token refresh. HTTP-independent — the api layer owns the loopback callback.
//!
//! The subscription's rotating access token is surfaced to routing through the well-known
//! `PROVIDER_NAME` provider (see `Provider::effective_key`); the token is NEVER written to the
//! human-editable providers.json — it lives only in this 0600 store.
#![allow(dead_code)]

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::now_secs;

pub const PROVIDER_NAME: &str = "chatgpt";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const SCOPE: &str = "openid profile email offline_access";
/// Base the LiteLLM proxy points a subscription worker at (Responses API lives at `/responses`).
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// base64url(sha256(verifier)), no padding — the S256 PKCE challenge.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random verifier (96 hex chars from 3 uuids — meets the 43..128 PKCE length) + its challenge.
pub fn gen_pkce() -> (String, String) {
    let verifier = format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

/// Build the OAuth authorize URL. `reqwest::Url` (reqwest re-exports `url`) does the percent-encoding.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
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
    .map(|u| u.to_string())
    .unwrap_or_default()
}

/// Extract `chatgpt_account_id` from a JWT — top-level, else nested under the OpenAI auth claim.
pub fn account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if let Some(id) = v.get("chatgpt_account_id").and_then(|x| x.as_str()) {
        return Some(id.to_string());
    }
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: String,
    /// Unix seconds when the access token expires (0 = unknown).
    #[serde(default)]
    pub expires_at: u64,
    /// Set after a failed refresh — the user must log in again.
    #[serde(default)]
    pub needs_reauth: bool,
}

/// Test override for [store_path] — a data-race-free static, mirroring providers' override.
pub static STORE_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Serializes tests (across the whole crate) that mutate the process-global [STORE_FILE_OVERRIDE],
/// so parallel test threads don't clobber each other's override. parking_lot never poisons, so a
/// panicking test still releases it.
#[cfg(test)]
pub(crate) static TEST_STORE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub fn store_path() -> PathBuf {
    if let Some(p) = STORE_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_OPENAI_OAUTH_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("openai-oauth.json")
}

pub fn load_from(path: &Path) -> Option<StoredToken> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn save_to(path: &Path, tok: &StoredToken) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(tok).map_err(std::io::Error::other)?;
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

pub fn load() -> Option<StoredToken> {
    load_from(&store_path())
}
pub fn save(tok: &StoredToken) -> std::io::Result<()> {
    save_to(&store_path(), tok)
}
pub fn clear() {
    let _ = std::fs::remove_file(store_path());
}

/// The current usable access token: `None` when disconnected or a refresh is pending re-auth.
pub fn current_access_token() -> Option<String> {
    let t = load()?;
    if t.access_token.is_empty() || t.needs_reauth {
        return None;
    }
    Some(t.access_token)
}

pub fn is_expired(tok: &StoredToken, now: u64) -> bool {
    tok.expires_at != 0 && now >= tok.expires_at
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: u64,
}

fn token_from_response(body: TokenResponse) -> StoredToken {
    let account_id = account_id_from_jwt(&body.access_token)
        .or_else(|| account_id_from_jwt(&body.id_token))
        .unwrap_or_default();
    StoredToken {
        expires_at: if body.expires_in > 0 {
            now_secs() + body.expires_in
        } else {
            0
        },
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        id_token: body.id_token,
        account_id,
        needs_reauth: false,
    }
}

fn post_token_form(form: &[(&str, &str)]) -> Result<StoredToken, String> {
    let resp = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("token endpoint returned {}", resp.status()));
    }
    Ok(token_from_response(resp.json().map_err(|e| e.to_string())?))
}

pub fn exchange_code(code: &str, verifier: &str) -> Result<StoredToken, String> {
    post_token_form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", verifier),
    ])
}

pub fn refresh(refresh_token: &str) -> Result<StoredToken, String> {
    post_token_form(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
        ("scope", SCOPE),
    ])
}

/// Refresh the stored token when within `skew` secs of expiry. `Ok(true)` = rotated (caller reloads
/// litellm), `Ok(false)` = nothing to do, `Err` = refresh failed (store marked `needs_reauth`).
pub fn refresh_if_needed(skew: u64) -> Result<bool, String> {
    let Some(tok) = load() else {
        return Ok(false);
    };
    if tok.access_token.is_empty() || tok.refresh_token.is_empty() {
        return Ok(false);
    }
    let now = now_secs();
    if tok.expires_at == 0 || now + skew < tok.expires_at {
        return Ok(false); // still fresh
    }
    match refresh(&tok.refresh_token) {
        Ok(mut fresh) => {
            if fresh.refresh_token.is_empty() {
                fresh.refresh_token = tok.refresh_token;
            }
            if fresh.account_id.is_empty() {
                fresh.account_id = tok.account_id;
            }
            save(&fresh).map_err(|e| e.to_string())?;
            Ok(true)
        }
        Err(e) => {
            let mut t = tok;
            t.needs_reauth = true;
            let _ = save(&t);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_base64url_nopad() {
        // sha256("abc") = ba7816bf... → base64url(no pad) below.
        let c = pkce_challenge("abc");
        assert_eq!(c, "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
        assert!(!c.contains('='), "no padding");
        assert!(!c.contains('+') && !c.contains('/'), "url-safe alphabet");
    }

    #[test]
    fn gen_pkce_lengths_valid() {
        let (v, c) = gen_pkce();
        assert!(
            (43..=128).contains(&v.len()),
            "verifier length in PKCE range"
        );
        assert_eq!(c, pkce_challenge(&v));
    }

    #[test]
    fn authorize_url_encodes_params() {
        let u = authorize_url("CH", "ST");
        assert!(u.starts_with(AUTHORIZE_URL));
        assert!(u.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(u.contains("code_challenge=CH"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("state=ST"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(
            u.contains("scope=openid+profile+email+offline_access")
                || u.contains("scope=openid%20profile")
        );
    }

    fn jwt(payload: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"none"}"#),
            b64(serde_json::to_string(&payload).unwrap().as_bytes()),
            "sig"
        )
    }

    #[test]
    fn account_id_top_level_and_nested_and_malformed() {
        assert_eq!(
            account_id_from_jwt(&jwt(serde_json::json!({"chatgpt_account_id":"acc-1"}))).as_deref(),
            Some("acc-1")
        );
        assert_eq!(
            account_id_from_jwt(&jwt(
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"acc-2"}})
            ))
            .as_deref(),
            Some("acc-2")
        );
        assert_eq!(account_id_from_jwt("not.a.jwt"), None);
        assert_eq!(account_id_from_jwt("garbage"), None);
        assert_eq!(
            account_id_from_jwt(&jwt(serde_json::json!({"sub":"x"}))),
            None
        );
    }

    #[test]
    fn store_roundtrip_and_mode_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("openai-oauth.json");
        assert!(load_from(&f).is_none());
        let tok = StoredToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acc".into(),
            expires_at: 123,
            ..Default::default()
        };
        save_to(&f, &tok).unwrap();
        let got = load_from(&f).unwrap();
        assert_eq!(got.access_token, "at");
        assert_eq!(got.account_id, "acc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "store must be 0600");
        }
        std::fs::write(&f, "not json").unwrap();
        assert!(load_from(&f).is_none(), "corrupt store → None, not panic");
    }

    #[test]
    fn is_expired_boundary() {
        let t = StoredToken {
            expires_at: 100,
            ..Default::default()
        };
        assert!(!is_expired(&t, 99));
        assert!(is_expired(&t, 100));
        assert!(is_expired(&t, 101));
        assert!(!is_expired(
            &StoredToken {
                expires_at: 0,
                ..Default::default()
            },
            999
        ));
    }

    #[test]
    fn current_access_token_gates_on_state() {
        let _g = TEST_STORE_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        assert_eq!(current_access_token(), None);
        save(&StoredToken {
            access_token: "live".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(current_access_token().as_deref(), Some("live"));
        save(&StoredToken {
            access_token: "live".into(),
            needs_reauth: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(current_access_token(), None);
        *STORE_FILE_OVERRIDE.lock() = None;
    }

    #[test]
    fn refresh_if_needed_noop_when_fresh() {
        let _g = TEST_STORE_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        save(&StoredToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_secs() + 100_000,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(refresh_if_needed(300), Ok(false));
        clear();
        assert_eq!(refresh_if_needed(300), Ok(false));
        *STORE_FILE_OVERRIDE.lock() = None;
    }
}
