# ChatGPT Subscription (OAuth) Provider — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Connect a ChatGPT subscription via OAuth so GPT enters the model catalog and delegate routing pool (worker-only, never main session).

**Architecture:** New HTTP-independent `engine::model::openai_oauth` module (PKCE, token exchange/refresh, JWT account-id parse, 0600 token store). A reserved-name subscription provider (`"chatgpt"`) surfaces the rotating access token through `Provider::effective_key()`; the existing LiteLLM/delegate path carries it plus codex headers. API endpoints run a one-shot `127.0.0.1:1455` loopback callback listener. Android gets a connect/status card.

**Tech Stack:** Rust (axum 0.8, reqwest blocking, sha2, base64, uuid, parking_lot — all already deps), Kotlin/Compose + Ktor.

## Global Constraints

- Engine (`server-rs/src/engine/`) stays free of axum imports. `openai_oauth` is HTTP-independent.
- Tests never hit real APIs: `make test` runs through the fake bridge. New tests are hermetic (injected store path, `seed_claude_models_for_tests`). No new Rust or Android dependency.
- Token never written to `providers.json`; token store is mode 0600.
- Fixed OAuth constants: client_id `app_EMoamEEZ73f0CkXaXp7hrann`, redirect `http://localhost:1455/auth/callback`, authorize `https://auth.openai.com/oauth/authorize`, token `https://auth.openai.com/oauth/token`, scopes `openid profile email offline_access`, call base `https://chatgpt.com/backend-api/codex`, headers `ChatGPT-Account-Id`/`originator: codex_cli_rs`/`OpenAI-Beta: responses=experimental`.
- Reserved subscription provider name: `chatgpt`. Reuse `crate::util::now_secs()`.

---

## Task 1: `openai_oauth` engine module — PKCE + JWT + store (pure, TDD)

**Files:**
- Create: `server-rs/src/engine/model/openai_oauth.rs`
- Modify: `server-rs/src/engine/model/mod.rs` (add `pub mod openai_oauth;`)
- Modify: `server-rs/src/engine/mod.rs:697` (add `openai_oauth` to the `pub use model::{...}` re-export)

**Interfaces produced:**
- `pub const PROVIDER_NAME: &str = "chatgpt";` `pub const CODEX_BASE_URL`, `AUTHORIZE_URL`, `TOKEN_URL`, `CLIENT_ID`, `REDIRECT_URI`, `SCOPE`.
- `pub fn pkce_challenge(verifier: &str) -> String`
- `pub fn gen_pkce() -> (String, String)` → (verifier, challenge)
- `pub fn authorize_url(challenge: &str, state: &str) -> String`
- `pub fn account_id_from_jwt(token: &str) -> Option<String>`
- `pub struct StoredToken { access_token, refresh_token, id_token, account_id, expires_at: u64, needs_reauth: bool }`
- `pub fn load_from(&Path) -> Option<StoredToken>` / `save_to(&Path, &StoredToken) -> io::Result<()>` / `load()` / `save()` / `clear()`
- `pub fn current_access_token() -> Option<String>`
- `pub fn is_expired(&StoredToken, now: u64) -> bool`
- `pub fn exchange_code(code, verifier) -> Result<StoredToken,String>` / `refresh(refresh_token) -> Result<StoredToken,String>` / `refresh_if_needed(skew: u64) -> Result<bool,String>`
- `pub static STORE_FILE_OVERRIDE` + `pub fn store_path() -> PathBuf`

- [ ] **Step 1: Write the module with unit tests** (full code below)

```rust
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
pub static STORE_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

pub fn store_path() -> PathBuf {
    if let Some(p) = STORE_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_OPENAI_OAUTH_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agentic-dev").join("openai-oauth.json")
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
        // Known vector: verifier "abc" → base64url(sha256("abc")) no pad.
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let c = pkce_challenge("abc");
        assert_eq!(c, "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
        assert!(!c.contains('='), "no padding");
        assert!(!c.contains('+') && !c.contains('/'), "url-safe alphabet");
    }

    #[test]
    fn gen_pkce_lengths_valid() {
        let (v, c) = gen_pkce();
        assert!((43..=128).contains(&v.len()), "verifier length in PKCE range");
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
        // redirect + scope are percent-encoded
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(u.contains("scope=openid+profile+email+offline_access") || u.contains("scope=openid%20profile"));
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
        assert_eq!(account_id_from_jwt(&jwt(serde_json::json!({"sub":"x"}))), None);
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
        let t = StoredToken { expires_at: 100, ..Default::default() };
        assert!(!is_expired(&t, 99));
        assert!(is_expired(&t, 100));
        assert!(is_expired(&t, 101));
        // unknown expiry (0) is never "expired"
        assert!(!is_expired(&StoredToken { expires_at: 0, ..Default::default() }, 999));
    }

    #[test]
    fn current_access_token_gates_on_state() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        // no file → None
        assert_eq!(current_access_token(), None);
        // usable
        save(&StoredToken { access_token: "live".into(), ..Default::default() }).unwrap();
        assert_eq!(current_access_token().as_deref(), Some("live"));
        // needs_reauth → None
        save(&StoredToken { access_token: "live".into(), needs_reauth: true, ..Default::default() }).unwrap();
        assert_eq!(current_access_token(), None);
        *STORE_FILE_OVERRIDE.lock() = None;
    }

    #[test]
    fn refresh_if_needed_noop_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        // far-future expiry → no network, returns Ok(false)
        save(&StoredToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_secs() + 100_000,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(refresh_if_needed(300), Ok(false));
        // no token → Ok(false)
        clear();
        assert_eq!(refresh_if_needed(300), Ok(false));
        *STORE_FILE_OVERRIDE.lock() = None;
    }
}
```

- [ ] **Step 2: Register the module.** In `mod.rs` add `pub mod openai_oauth;`; in `engine/mod.rs` add `openai_oauth` to `pub use model::{...}`.

- [ ] **Step 3: Run tests**

Run: `cd server-rs && cargo test openai_oauth`
Expected: all new tests PASS.

- [ ] **Step 4: Commit** `feat(oauth): openai_oauth engine module (PKCE, JWT, token store, refresh)`

---

## Task 2: Subscription-aware provider key

**Files:**
- Modify: `server-rs/src/engine/model/providers.rs` (add const + two methods + a test)

**Interfaces produced:**
- `Provider::is_subscription(&self) -> bool`, `Provider::effective_key(&self) -> String`.

- [ ] **Step 1: Add the failing test** (in providers.rs `tests`):

```rust
#[test]
fn subscription_effective_key_reads_oauth_store() {
    use crate::engine::model::openai_oauth;
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("s.json");
    *openai_oauth::STORE_FILE_OVERRIDE.lock() = Some(f.clone());
    let sub = Provider {
        name: openai_oauth::PROVIDER_NAME.into(),
        base_url: openai_oauth::CODEX_BASE_URL.into(),
        api_key: String::new(),
        api_key_env: None,
        model: "gpt-5".into(),
        protocol: Protocol::Openai,
        capability: 0.9,
        description: None,
        priority: 0.5,
        cost: 0.9,
        router: false,
        enabled: true,
    };
    assert!(sub.is_subscription());
    // empty store → empty key (delegate filter drops it)
    assert_eq!(sub.effective_key(), "");
    openai_oauth::save(&openai_oauth::StoredToken { access_token: "tok".into(), ..Default::default() }).unwrap();
    assert_eq!(sub.effective_key(), "tok");
    // a normal provider ignores the store, uses resolved_key
    let normal = Provider { name: "minimax".into(), ..sub.clone() };
    assert!(!normal.is_subscription());
    *openai_oauth::STORE_FILE_OVERRIDE.lock() = None;
}
```

- [ ] **Step 2: Implement** (after `resolved_key_with`, inside `impl Provider`):

```rust
    /// True for the single well-known ChatGPT-subscription provider (identified by name).
    pub fn is_subscription(&self) -> bool {
        self.name
            .eq_ignore_ascii_case(super::openai_oauth::PROVIDER_NAME)
    }

    /// The key routing/LiteLLM should use: the subscription's rotating OAuth access token for the
    /// subscription provider, else the literal/`api_key_env` key. Empty when disconnected → the
    /// delegate candidate filter drops the provider.
    pub fn effective_key(&self) -> String {
        if self.is_subscription() {
            super::openai_oauth::current_access_token().unwrap_or_default()
        } else {
            self.resolved_key()
        }
    }
```

- [ ] **Step 3: Run** `cd server-rs && cargo test -p agentic-dev providers::` → PASS.
- [ ] **Step 4: Commit** `feat(providers): subscription-aware effective_key`

---

## Task 3: LiteLLM config carries rotating bearer + codex headers

**Files:**
- Modify: `server-rs/src/engine/model/litellm.rs` (extract `render_config`, use `effective_key`, add subscription `extra_headers`; add test)

**Interfaces produced:** `fn render_config(&[Provider]) -> (String, Vec<(String,String)>)` (private, tested in-module).

- [ ] **Step 1: Add the failing test:**

```rust
    #[test]
    fn render_config_subscription_has_codex_base_and_headers() {
        use crate::engine::model::openai_oauth;
        use crate::engine::providers::{Protocol, Provider};
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *openai_oauth::STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        openai_oauth::save(&openai_oauth::StoredToken {
            access_token: "tok".into(),
            account_id: "acc-9".into(),
            ..Default::default()
        })
        .unwrap();
        let sub = Provider {
            name: openai_oauth::PROVIDER_NAME.into(),
            base_url: openai_oauth::CODEX_BASE_URL.into(),
            api_key: String::new(),
            api_key_env: None,
            model: "gpt-5".into(),
            protocol: Protocol::Openai,
            capability: 0.9,
            description: None,
            priority: 0.5,
            cost: 0.9,
            router: false,
            enabled: true,
        };
        let (yaml, envs) = render_config(&[sub]);
        assert!(yaml.contains("api_base: \"https://chatgpt.com/backend-api/codex\""));
        assert!(yaml.contains("ChatGPT-Account-Id: \"acc-9\""));
        assert!(yaml.contains("originator: \"codex_cli_rs\""));
        assert!(yaml.contains("OpenAI-Beta: \"responses=experimental\""));
        assert_eq!(envs.len(), 1, "rotating bearer injected via env");
        assert_eq!(envs[0].1, "tok");
        *openai_oauth::STORE_FILE_OVERRIDE.lock() = None;
    }
```

- [ ] **Step 2: Refactor `build_config` to delegate to `render_config`** (replace the body loop):

```rust
/// Pure config renderer: YAML for the openai providers + the `(env_var, key)` pairs to inject.
/// Subscription providers get the codex headers and their rotating bearer via `effective_key`.
fn render_config(providers: &[crate::engine::providers::Provider]) -> (String, Vec<(String, String)>) {
    let mut yaml = String::from("model_list:\n");
    let mut envs: Vec<(String, String)> = Vec::new();
    for p in providers {
        if !matches!(p.protocol, Protocol::Openai) {
            continue;
        }
        let key = p.effective_key();
        if key.is_empty() {
            continue;
        }
        let var = format!("AGENTIC_LITELLM_KEY_{}", envs.len());
        yaml.push_str(&format!(
            "  - model_name: {name}\n    litellm_params:\n      model: {model}\n      api_base: {base}\n      api_key: os.environ/{var}\n",
            name = yaml_q(&p.model),
            model = yaml_q(&format!("openai/{}", p.model)),
            base = yaml_q(&p.base_url),
            var = var,
        ));
        if p.is_subscription() {
            let account_id = crate::engine::model::openai_oauth::load()
                .map(|t| t.account_id)
                .unwrap_or_default();
            yaml.push_str(&format!(
                "      extra_headers:\n        ChatGPT-Account-Id: {id}\n        originator: {orig}\n        OpenAI-Beta: {beta}\n",
                id = yaml_q(&account_id),
                orig = yaml_q("codex_cli_rs"),
                beta = yaml_q("responses=experimental"),
            ));
        }
        envs.push((var, key));
    }
    (yaml, envs)
}

fn build_config() -> Vec<(String, String)> {
    let reg = ProviderRegistry::load();
    let (yaml, envs) = render_config(&reg.providers);
    if let Some(parent) = config_path().parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::error!("[litellm] create config dir {:?} failed: {e}", parent);
        }
    }
    if let Err(e) = std::fs::write(config_path(), &yaml) {
        tracing::error!("[litellm] write config {:?} failed: {e}", config_path());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(config_path(), std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("[litellm] chmod 600 config failed: {e}");
        }
    }
    envs
}
```

- [ ] **Step 3: Run** `cd server-rs && cargo test litellm` → PASS (incl. existing `yaml_q_escapes`).
- [ ] **Step 4: Commit** `feat(litellm): subscription codex base_url + headers + rotating bearer`

---

## Task 4: Delegate filter + provider view use `effective_key`

**Files:**
- Modify: `server-rs/src/engine/workflow/delegate.rs:872` (`!p.resolved_key().is_empty()` → `!p.effective_key().is_empty()`)
- Modify: `server-rs/src/api/misc.rs:452` (`provider_view.has_key`: `resolved_key` → `effective_key`)

- [ ] **Step 1: Edit both call sites** (swap `resolved_key` → `effective_key` — exact one-token change each).
- [ ] **Step 2: Add a delegate test** (in delegate.rs tests) that a `chatgpt` provider is kept only when the store has a token. If the existing delegate tests don't easily expose the filter, assert on `effective_key` via a provider vec instead (see Task 2 coverage) and skip a bespoke delegate test — the filter is a one-token swap over an already-tested predicate. Note the decision in the commit.
- [ ] **Step 3: Run** `cd server-rs && cargo build && cargo test` → compiles, green.
- [ ] **Step 4: Commit** `feat(delegate): keep subscription provider only when connected`

---

## Task 5: API — login/status/logout + loopback callback

**Files:**
- Create: `server-rs/src/api/subscription.rs`
- Modify: `server-rs/src/api/mod.rs` (add `pub(crate) mod subscription;` + 3 routes)

**Interfaces produced:** `subscription::login_start`, `subscription::status`, `subscription::logout` (axum handlers).

- [ ] **Step 1: Write `subscription.rs`:**

```rust
//! ChatGPT subscription OAuth endpoints. Login runs a one-shot loopback listener on
//! 127.0.0.1:1455 (the fixed OAuth redirect target); the OAuth consent must be completed in a
//! browser that can reach the server host (documented co-location assumption).
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::engine::model::openai_oauth as oauth;
use crate::engine::providers::{Protocol, Provider};

/// The provider row written to providers.json on a successful login (no key — token lives in the
/// 0600 store). base_url points the LiteLLM proxy at the codex backend.
fn subscription_provider() -> Provider {
    Provider {
        name: oauth::PROVIDER_NAME.into(),
        base_url: oauth::CODEX_BASE_URL.into(),
        api_key: String::new(),
        api_key_env: None,
        model: "gpt-5".into(),
        protocol: Protocol::Openai,
        capability: 0.9,
        description: Some("ChatGPT subscription (OAuth) — GPT worker".into()),
        priority: 0.5,
        cost: 0.9,
        router: false,
        enabled: true,
    }
}

fn callback_port() -> u16 {
    std::env::var("AGENTIC_OPENAI_CALLBACK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1455)
}

#[derive(Clone)]
struct CbState {
    verifier: Arc<String>,
    expected_state: Arc<String>,
    done: Arc<tokio::sync::Notify>,
}

#[derive(serde::Deserialize)]
struct CbQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

const SUCCESS_HTML: &str =
    "<html><body style=\"font-family:sans-serif\"><h2>ChatGPT connected ✓</h2>\
     <p>You can close this tab and return to agentic-dev.</p></body></html>";

async fn callback(State(st): State<CbState>, Query(q): Query<CbQuery>) -> Html<String> {
    if let Some(err) = q.error {
        st.done.notify_one();
        return Html(format!("<h2>Login failed: {err}</h2>"));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Html("<h2>Missing code/state</h2>".to_string());
    };
    if state != *st.expected_state {
        // Ignore a bad-state hit; keep waiting for the real redirect.
        return Html("<h2>State mismatch — ignore this tab.</h2>".to_string());
    }
    let verifier = st.verifier.clone();
    let exchanged =
        tokio::task::spawn_blocking(move || oauth::exchange_code(&code, &verifier)).await;
    st.done.notify_one();
    match exchanged {
        Ok(Ok(tok)) => {
            if let Err(e) = oauth::save(&tok) {
                return Html(format!("<h2>Could not persist token: {e}</h2>"));
            }
            if let Err(e) = crate::engine::providers::upsert(subscription_provider()) {
                return Html(format!("<h2>Could not register provider: {e}</h2>"));
            }
            crate::engine::litellm::request_reload();
            Html(SUCCESS_HTML.to_string())
        }
        Ok(Err(e)) => Html(format!("<h2>Token exchange failed: {e}</h2>")),
        Err(e) => Html(format!("<h2>Token exchange task failed: {e}</h2>")),
    }
}

/// POST /api/providers/openai-subscription/login — start OAuth, return the authorize URL.
pub async fn login_start() -> Response {
    let (verifier, challenge) = oauth::gen_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::authorize_url(&challenge, &state);
    let st = CbState {
        verifier: Arc::new(verifier),
        expected_state: Arc::new(state),
        done: Arc::new(tokio::sync::Notify::new()),
    };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", callback_port())).await {
        Ok(l) => l,
        Err(e) => {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({"error": format!("callback port {} busy (login already in progress?): {e}", callback_port())})),
            )
                .into_response()
        }
    };
    let done = st.done.clone();
    let app = axum::Router::new()
        .route("/auth/callback", axum::routing::get(callback))
        .with_state(st);
    tokio::spawn(async move {
        let shutdown = async move {
            tokio::select! {
                _ = done.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
            }
        };
        let _ = axum::serve(listener, app).with_graceful_shutdown(shutdown).await;
    });
    Json(json!({ "authorize_url": url })).into_response()
}

/// GET /api/providers/openai-subscription/status
pub async fn status() -> Response {
    let tok = oauth::load();
    let connected = tok
        .as_ref()
        .map(|t| !t.access_token.is_empty() && !t.needs_reauth)
        .unwrap_or(false);
    Json(json!({
        "connected": connected,
        "account_id": tok.as_ref().map(|t| t.account_id.clone()).unwrap_or_default(),
        "expires_at": tok.as_ref().map(|t| t.expires_at).unwrap_or(0),
        "needs_reauth": tok.as_ref().map(|t| t.needs_reauth).unwrap_or(false),
    }))
    .into_response()
}

/// POST /api/providers/openai-subscription/logout
pub async fn logout() -> Response {
    oauth::clear();
    let _ = crate::engine::providers::remove(oauth::PROVIDER_NAME);
    crate::engine::litellm::request_reload();
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subscription_provider_shape() {
        let p = subscription_provider();
        assert!(p.is_subscription());
        assert!(matches!(p.protocol, Protocol::Openai));
        assert_eq!(p.base_url, oauth::CODEX_BASE_URL);
        assert!(p.api_key.is_empty(), "token never in providers.json");
    }
}
```

- [ ] **Step 2: Wire routes** in `api/mod.rs`: add `pub(crate) mod subscription;` at top and, near the `/api/providers` routes:

```rust
        .route(
            "/api/providers/openai-subscription/login",
            post(misc_subscription_login),
        )
        .route(
            "/api/providers/openai-subscription/status",
            get(crate::api::subscription::status),
        )
        .route(
            "/api/providers/openai-subscription/logout",
            post(crate::api::subscription::logout),
        )
```
(use direct paths `crate::api::subscription::login_start` etc.; no alias needed — replace the placeholder names with the real handler paths.)

- [ ] **Step 3: Run** `cd server-rs && cargo build && cargo test subscription` → PASS.
- [ ] **Step 4: Commit** `feat(api): ChatGPT subscription login/status/logout + loopback callback`

---

## Task 6: Startup refresh task

**Files:**
- Modify: `server-rs/src/main.rs` (near `litellm::start_supervisor();` ~line 194)

- [ ] **Step 1: Add after `start_supervisor()`:**

```rust
    // Keep the ChatGPT subscription access token fresh; reload the LiteLLM proxy on rotation so the
    // worker→proxy hop always carries a live bearer.
    tokio::spawn(async {
        loop {
            if let Ok(true) = tokio::task::spawn_blocking(|| {
                crate::engine::model::openai_oauth::refresh_if_needed(300)
            })
            .await
            .unwrap_or(Ok(false))
            {
                crate::engine::litellm::request_reload();
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
```

- [ ] **Step 2: Run** `cd server-rs && cargo build` → compiles. `make test` → green.
- [ ] **Step 3: Commit** `feat(main): background refresh of subscription token`

---

## Task 7: Android — API + repo + DTOs

**Files:**
- Modify: `core/network/src/main/kotlin/dev/agentic/data/net/AgenticApi.kt` (3 suspend fns + DTOs, with test-fake defaults)
- Modify: `core/network/src/main/kotlin/dev/agentic/data/net/KtorAgenticApi.kt` (impls)
- Modify: `core/data/src/main/kotlin/dev/agentic/data/repo/ProvidersRepository.kt` (pass-throughs)
- Modify: `core/testing/src/main/kotlin/dev/agentic/data/FakeAgenticApi.kt` if it overrides these

**Interfaces produced:** `AgenticApi.startSubscriptionLogin(): SubscriptionLogin`, `subscriptionStatus(): SubscriptionStatus`, `subscriptionLogout()`.

- [ ] **Step 1: DTOs** (in `AgenticApi.kt`, matching existing `@Serializable` DTO style):

```kotlin
@Serializable
data class SubscriptionLogin(@SerialName("authorize_url") val authorizeUrl: String)

@Serializable
data class SubscriptionStatus(
    val connected: Boolean = false,
    @SerialName("account_id") val accountId: String = "",
    @SerialName("expires_at") val expiresAt: Long = 0,
    @SerialName("needs_reauth") val needsReauth: Boolean = false,
)
```

- [ ] **Step 2: Interface methods** (defaults so the test fake stays valid):

```kotlin
    /** Start ChatGPT subscription OAuth (POST /api/providers/openai-subscription/login). */
    suspend fun startSubscriptionLogin(): SubscriptionLogin = SubscriptionLogin("")
    /** Subscription connection status (GET .../status). */
    suspend fun subscriptionStatus(): SubscriptionStatus = SubscriptionStatus()
    /** Disconnect the subscription (POST .../logout). */
    suspend fun subscriptionLogout() {}
```

- [ ] **Step 3: Ktor impls** (mirror the existing `providers()` shape with `auth()` + try/catch logging):

```kotlin
    override suspend fun startSubscriptionLogin(): SubscriptionLogin =
        client.post("$baseUrl/api/providers/openai-subscription/login") { auth() }.body()

    override suspend fun subscriptionStatus(): SubscriptionStatus =
        client.get("$baseUrl/api/providers/openai-subscription/status") { auth() }.body()

    override suspend fun subscriptionLogout() {
        client.post("$baseUrl/api/providers/openai-subscription/logout") { auth() }
    }
```

- [ ] **Step 4: Repo pass-throughs** in `ProvidersRepository.kt`:

```kotlin
    suspend fun startSubscriptionLogin() = api.startSubscriptionLogin()
    suspend fun subscriptionStatus() = api.subscriptionStatus()
    suspend fun subscriptionLogout() = api.subscriptionLogout()
```

- [ ] **Step 5: Commit** `feat(net): ChatGPT subscription API + repo`

---

## Task 8: Android — connect/status UI in ProvidersViewModel + ProvidersScreen

**Files:**
- Modify: `feature/providers/src/main/kotlin/dev/agentic/ui/providers/ProvidersViewModel.kt`
- Modify: `feature/providers/src/main/kotlin/dev/agentic/ui/providers/ProvidersScreen.kt`
- Test: `feature/providers/src/test/.../ProvidersViewModelTest.kt` (create if the feature has a test source set; otherwise assert via existing test infra)

**Interfaces produced:** VM `subscription: SubscriptionStatus` in `ProvidersUiState`; `fun connectSubscription(open: (String) -> Unit)`, `fun disconnectSubscription()`, `refresh()` also loads status.

- [ ] **Step 1: Extend `ProvidersUiState`** with `val subscription: SubscriptionStatus = SubscriptionStatus()` and load it in `refresh()` (best-effort, like `getRouting()`):

```kotlin
            when (val s = runCatchingOutcome { repo.subscriptionStatus() }) {
                is Outcome.Success -> _uiState.update { it.copy(subscription = s.value) }
                is Outcome.Failure -> AppLog.w("VM", "subscription status failed err=${s.error}")
            }
```

- [ ] **Step 2: Add VM actions:**

```kotlin
    /** Start OAuth: fetch the authorize URL, hand it to [open] (browser), then poll status until
     *  connected (or a bounded number of tries), refreshing the provider/model list on success. */
    fun connectSubscription(open: (String) -> Unit) {
        viewModelScope.launch {
            when (val r = runCatchingOutcome { repo.startSubscriptionLogin() }) {
                is Outcome.Success -> {
                    if (r.value.authorizeUrl.isNotEmpty()) open(r.value.authorizeUrl)
                    repeat(60) {
                        kotlinx.coroutines.delay(3000)
                        val s = runCatchingOutcome { repo.subscriptionStatus() }
                        if (s is Outcome.Success && s.value.connected) {
                            _uiState.update { it.copy(subscription = s.value) }
                            refresh()
                            return@launch
                        }
                    }
                }
                is Outcome.Failure -> _uiState.update { it.copy(error = r.error.toString()) }
            }
        }
    }

    fun disconnectSubscription() {
        viewModelScope.launch {
            runCatchingOutcome { repo.subscriptionLogout() }
            refresh()
        }
    }
```

- [ ] **Step 3: UI card** in `ProvidersScreen.kt` at the top of the provider list: show a "ChatGPT subscription" card — when `subscription.connected`, show account id + expiry + a Disconnect button; when `subscription.needsReauth`, show "Re-login needed" + Connect; else a Connect button. Wire Connect to `vm.connectSubscription { url -> uriHandler.openUri(url) }` using `val uriHandler = LocalUriHandler.current`. Follow the existing card composables/spacing in the file.

- [ ] **Step 4: ViewModel test** (if `FakeAgenticApi` is available to the feature test set): assert `refresh()` populates `subscription`, and `connectSubscription` opens the URL and flips state when the fake reports connected. If the feature has no test set, skip and note it.

- [ ] **Step 5: Build** `./gradlew :feature:providers:assembleDebug :core:network:assembleDebug` (or `./gradlew assembleDebug`) → compiles.
- [ ] **Step 6: Commit** `feat(providers): ChatGPT subscription connect/status card`

---

## Task 9: Docs + verify + open PRs

- [ ] Append a short note to `agentic-dev/docs/internals.md` on the subscription flow (loopback :1455, 0600 store, rotating bearer via litellm reload, co-location assumption).
- [ ] `cd server-rs && cargo build && make test` → green. Android `./gradlew assembleDebug` → compiles.
- [ ] Adversarial `delegate` review (per repo CLAUDE.md), fix real findings.
- [ ] Open **non-draft** PRs on both repos; then STOP (benchmark arm — do not merge).

## Self-review notes

- Criterion 1 (register via OAuth, persisted, tightened perms, auto-refresh): Tasks 1,5,6 — 0600 store, never in providers.json, background refresh + `needs_reauth`.
- Criterion 2 (GPT in `GET /api/models` default scope): free via `full_model_entries()` once the `chatgpt` provider row exists (Task 5 upsert); covered structurally, add an assertion in Task 5 if cheap.
- Criterion 3 (routable via LiteLLM/delegate, openai + rotating bearer): Tasks 2,3,4,6.
- Criterion 4 (Android login entry + status + refresh): Tasks 7,8.
- Criterion 5 (cargo build + make test green, Android compiles): Tasks 3,4,6,8,9.
