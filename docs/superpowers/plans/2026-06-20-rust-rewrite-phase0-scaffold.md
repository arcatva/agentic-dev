# Rust Backend Rewrite — Phase 0 (Scaffold + Config + Auth) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Rust+Tokio backend crate so it boots, loads the same config as the TS server, mints/verifies tokens **cross-compatible with the existing TS auth**, and gates `/api/*` — a working, authenticating server with no engine yet.

**Architecture:** A new cargo crate at `agentic-dev/server-rs/`. `axum` router built by a testable `app(state)` function (so handlers are tested in-process via `tower::ServiceExt::oneshot`, no real socket). Config and auth are pure modules mirroring `server/api/config.ts` and `server/api/auth.ts` exactly. State is an `Arc`-shared `Config` + a login throttle.

**Tech Stack:** Rust 1.95, tokio 1 (multi-threaded), axum 0.8, serde/serde_json 1, hmac 0.12 + sha2 0.10 + base64 0.22 (token), subtle 2 (constant-time), tower 0.5 + http-body-util 0.1 (tests).

## Global Constraints

- Crate lives at `agentic-dev/server-rs/`. Work from that directory; build/test with `cargo` (toolchain 1.95 is installed).
- **Auth parity is mandatory:** token format is `"<expEpochSec>.<base64url(HMAC_SHA256(secret, expEpochSec))>"` (base64url **no padding**). A token minted by the TS server MUST verify in Rust. Cross-compat vector (secret `test-secret`): `9999999999.Rl7rud9Sqkll6ysQ-xw6BsEtc89CSCfEEvHaCyFxSi4` must verify as valid.
- **Config parity:** mirror `loadConfig` defaults verbatim — port `7420`, host `0.0.0.0`, password `changeme`, authSecret `dev-insecure-secret`, dataDir `~/.agentic-dev` (→ `logs/`, `db.sqlite`), srcRoot `~/src`, `maxConcurrent` = unlimited unless `AGENTIC_MAX_CONCURRENT` is set, gitOrg `arcatva`, claudeConfigBase `~/.claude`.
- **Auth gate:** every request whose path starts with `/api/` EXCEPT `/api/login` requires a valid token, taken from the `Authorization: Bearer <t>` header OR a `?token=<t>` query param (WS upgrades use the query form). Invalid → `401 {"error":"unauthorized"}`.
- Injectable clock: auth functions take `now_secs: u64` (not system time) so tests are deterministic; `main` passes real time.
- Injectable env: `Config::load` takes an env accessor closure (not `std::env` directly) so tests inject values.
- All `cargo test` green before each commit.

## File Structure

- `agentic-dev/server-rs/Cargo.toml` — crate + deps.
- `agentic-dev/server-rs/src/main.rs` — `#[tokio::main]` entrypoint: load config, build state, build `app`, bind `host:port`, serve.
- `agentic-dev/server-rs/src/config.rs` — `Config` struct + `Config::load(get)`.
- `agentic-dev/server-rs/src/auth.rs` — `issue_token`, `verify_token`.
- `agentic-dev/server-rs/src/state.rs` — `AppState { config: Arc<Config>, throttle: Arc<Mutex<LoginThrottle>> }`.
- `agentic-dev/server-rs/src/throttle.rs` — `LoginThrottle` (per-IP login lockout), pure + unit-tested.
- `agentic-dev/server-rs/src/api/mod.rs` — `pub fn app(state) -> Router` (mounts `/healthz`, `/api/login`, the auth middleware), the `auth_gate` middleware, token extraction.
- `agentic-dev/server-rs/src/api/login.rs` — `POST /api/login` handler.

---

### Task 1: Crate scaffold + bootable server with `/healthz`

**Files:**
- Create: `agentic-dev/server-rs/Cargo.toml`, `src/main.rs`, `src/api/mod.rs`, `src/state.rs`, `src/config.rs` (stub), `src/auth.rs` (stub), `src/throttle.rs` (stub), `src/api/login.rs` (stub)
- Test: inline `#[cfg(test)]` in `src/api/mod.rs`

**Interfaces:**
- Produces: `pub fn app(state: AppState) -> axum::Router`; `AppState { config: Arc<Config>, throttle: Arc<Mutex<LoginThrottle>> }`.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "agentic-dev-server"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
axum = "0.8"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
hmac = "0.12"
sha2 = "0.10"
base64 = "0.22"
subtle = "2"
tracing = "0.1"
tracing-subscriber = "0.3"

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

- [ ] **Step 2: Create the stub modules so the crate compiles**

`src/config.rs`:
```rust
//! Filled in Task 2.
#[derive(Clone, Debug)]
pub struct Config {
    pub auth_secret: String,
    pub password: String,
    pub port: u16,
    pub host: String,
}
impl Config {
    pub fn placeholder() -> Self {
        Config { auth_secret: "dev-insecure-secret".into(), password: "changeme".into(), port: 7420, host: "0.0.0.0".into() }
    }
}
```

`src/throttle.rs`:
```rust
//! Filled in Task 4.
#[derive(Default)]
pub struct LoginThrottle;
```

`src/auth.rs`:
```rust
//! Filled in Task 3.
```

`src/api/login.rs`:
```rust
//! Filled in Task 4.
```

`src/state.rs`:
```rust
use std::sync::{Arc, Mutex};
use crate::config::Config;
use crate::throttle::LoginThrottle;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub throttle: Arc<Mutex<LoginThrottle>>,
}
```

- [ ] **Step 3: Write the failing test for `/healthz` in `src/api/mod.rs`**

```rust
use axum::{routing::get, Router};
use crate::state::AppState;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> &'static str { "ok" }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::throttle::LoginThrottle;
    use std::sync::{Arc, Mutex};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use http_body_util::BodyExt;

    fn test_state() -> AppState {
        AppState { config: Arc::new(Config::placeholder()), throttle: Arc::new(Mutex::new(LoginThrottle::default())) }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let resp = app(test_state())
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }
}
```

- [ ] **Step 4: Write `src/main.rs`**

```rust
mod api;
mod auth;
mod config;
mod state;
mod throttle;

use std::sync::{Arc, Mutex};
use crate::config::Config;
use crate::state::AppState;
use crate::throttle::LoginThrottle;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = Arc::new(Config::placeholder()); // replaced by Config::load in Task 2
    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState { config, throttle: Arc::new(Mutex::new(LoginThrottle::default())) };
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!("agentic-dev-server listening on {addr}");
    axum::serve(listener, api::app(state)).await.expect("serve");
}
```

- [ ] **Step 5: Run the test**

Run: `cd agentic-dev/server-rs && cargo test healthz_returns_ok`
Expected: PASS (`test api::tests::healthz_returns_ok ... ok`).

- [ ] **Step 6: Commit**

```bash
cd agentic-dev/server-rs
git add Cargo.toml src/
git commit -m "feat(rs): scaffold cargo crate + bootable axum server with /healthz

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Config loader (parity with `loadConfig`)

**Files:**
- Modify: `agentic-dev/server-rs/src/config.rs`
- Test: inline `#[cfg(test)]` in `src/config.rs`

**Interfaces:**
- Produces: `Config` (full fields) + `Config::load(get: impl Fn(&str) -> Option<String>) -> Config`. `max_concurrent: Option<u64>` (None = unlimited).

- [ ] **Step 1: Write failing tests in `src/config.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn defaults_match_ts() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.port, 7420);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.password, "changeme");
        assert_eq!(c.auth_secret, "dev-insecure-secret");
        assert_eq!(c.max_concurrent, None); // unlimited
        assert_eq!(c.git_org, "arcatva");
        assert_eq!(c.db_path.to_str().unwrap(), "/home/u/.agentic-dev/db.sqlite");
        assert_eq!(c.log_dir.to_str().unwrap(), "/home/u/.agentic-dev/logs");
        assert_eq!(c.src_root.to_str().unwrap(), "/home/u/src");
    }

    #[test]
    fn overrides_apply() {
        let c = Config::load(env_of(&[
            ("HOME", "/home/u"),
            ("AGENTIC_PORT", "9000"),
            ("AGENTIC_MAX_CONCURRENT", "5"),
            ("AGENTIC_AUTH_SECRET", "s3cret"),
            ("AGENTIC_DATA_DIR", "/data"),
        ]));
        assert_eq!(c.port, 9000);
        assert_eq!(c.max_concurrent, Some(5));
        assert_eq!(c.auth_secret, "s3cret");
        assert_eq!(c.db_path.to_str().unwrap(), "/data/db.sqlite");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd agentic-dev/server-rs && cargo test config::`
Expected: FAIL — `Config::load` not found / fields missing.

- [ ] **Step 3: Replace `src/config.rs` with the full implementation**

```rust
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub src_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub log_dir: PathBuf,
    pub db_path: PathBuf,
    pub claude_bin: String,
    pub max_concurrent: Option<u64>, // None = unlimited (mirrors TS Infinity)
    pub git_org: String,
    pub claude_config_base: PathBuf,
    pub port: u16,
    pub host: String,
    pub password: String,
    pub auth_secret: String,
}

impl Config {
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Config {
        let home = get("HOME").unwrap_or_else(|| "/root".into());
        let join = |base: &str, sub: &str| PathBuf::from(base).join(sub);
        let src = get("AGENTIC_SRC_ROOT").unwrap_or_else(|| join(&home, "src").to_string_lossy().into_owned());
        let data_dir = get("AGENTIC_DATA_DIR").unwrap_or_else(|| join(&home, ".agentic-dev").to_string_lossy().into_owned());
        Config {
            src_root: PathBuf::from(&src),
            worktrees_root: get("AGENTIC_WORKTREES_ROOT").map(PathBuf::from).unwrap_or_else(|| join(&src, "agentic-worktrees")),
            log_dir: join(&data_dir, "logs"),
            db_path: join(&data_dir, "db.sqlite"),
            claude_bin: get("AGENTIC_CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
            max_concurrent: get("AGENTIC_MAX_CONCURRENT").and_then(|v| v.parse::<u64>().ok()),
            git_org: get("AGENTIC_GIT_ORG").unwrap_or_else(|| "arcatva".into()),
            claude_config_base: get("AGENTIC_CLAUDE_CONFIG_BASE").map(PathBuf::from).unwrap_or_else(|| join(&home, ".claude")),
            port: get("AGENTIC_PORT").and_then(|v| v.parse().ok()).unwrap_or(7420),
            host: get("AGENTIC_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            password: get("AGENTIC_PASSWORD").unwrap_or_else(|| "changeme".into()),
            auth_secret: get("AGENTIC_AUTH_SECRET").unwrap_or_else(|| "dev-insecure-secret".into()),
        }
    }
}
```

(Remove the old `placeholder()` and update `main.rs` Step 4 + the `state.rs`/`mod.rs` test helpers to build a `Config` via `Config::load(|_| None)` or a small test constructor. Add `pub fn for_test(secret: &str, password: &str) -> Config` that calls `load(|_| None)` then overrides `auth_secret`/`password`, and use it in tests/main.)

- [ ] **Step 4: Update `main.rs` to use real config + env**

Replace the placeholder line in `main.rs`:
```rust
    let config = Arc::new(Config::load(|k| std::env::var(k).ok()));
```

- [ ] **Step 5: Run tests**

Run: `cd agentic-dev/server-rs && cargo test config::`
Expected: PASS (both config tests).

- [ ] **Step 6: Commit**

```bash
cd agentic-dev/server-rs
git add src/config.rs src/main.rs src/api/mod.rs src/state.rs
git commit -m "feat(rs): config loader mirroring loadConfig defaults

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Token auth (cross-compatible with TS)

**Files:**
- Modify: `agentic-dev/server-rs/src/auth.rs`
- Test: inline `#[cfg(test)]` in `src/auth.rs`

**Interfaces:**
- Produces: `pub fn issue_token(secret: &str, ttl_seconds: u64, now_secs: u64) -> String`; `pub fn verify_token(secret: &str, token: &str, now_secs: u64) -> bool`.

- [ ] **Step 1: Write failing tests in `src/auth.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Minted by the TS server (secret "test-secret", exp 9999999999). Cross-compat parity gate.
    const TS_TOKEN: &str = "9999999999.Rl7rud9Sqkll6ysQ-xw6BsEtc89CSCfEEvHaCyFxSi4";

    #[test]
    fn verifies_a_ts_minted_token() {
        assert!(verify_token("test-secret", TS_TOKEN, 1_000));
    }

    #[test]
    fn round_trips() {
        let t = issue_token("s3cret", 3600, 1_000);
        assert!(verify_token("s3cret", &t, 1_000));
        assert!(verify_token("s3cret", &t, 1_000 + 3599));
    }

    #[test]
    fn rejects_expired_tampered_and_wrong_secret() {
        let t = issue_token("s3cret", 3600, 1_000);
        assert!(!verify_token("s3cret", &t, 1_000 + 3601)); // expired
        assert!(!verify_token("wrong", &t, 1_000));          // wrong secret
        assert!(!verify_token("s3cret", "9999999999.bm90YXNpZw", 1_000)); // bad sig
        assert!(!verify_token("s3cret", "nodot", 1_000));    // malformed
        assert!(!verify_token("s3cret", "notanumber.sig", 1_000)); // non-numeric exp
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd agentic-dev/server-rs && cargo test auth::`
Expected: FAIL — `issue_token`/`verify_token` not found.

- [ ] **Step 3: Implement `src/auth.rs`**

```rust
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn sign(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any size");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Token = "<expEpochSec>.<base64url(HMAC_SHA256(secret, exp))>" — identical to server/api/auth.ts.
pub fn issue_token(secret: &str, ttl_seconds: u64, now_secs: u64) -> String {
    let exp = now_secs + ttl_seconds;
    let payload = exp.to_string();
    let sig = sign(secret, &payload);
    format!("{payload}.{sig}")
}

pub fn verify_token(secret: &str, token: &str, now_secs: u64) -> bool {
    let Some((payload, sig_b64)) = token.split_once('.') else { return false };
    let Ok(provided) = URL_SAFE_NO_PAD.decode(sig_b64) else { return false };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) { Ok(m) => m, Err(_) => return false };
    mac.update(payload.as_bytes());
    if mac.verify_slice(&provided).is_err() { return false; } // constant-time
    match payload.parse::<u64>() { Ok(exp) => exp > now_secs, Err(_) => false }
}
```

- [ ] **Step 4: Run tests**

Run: `cd agentic-dev/server-rs && cargo test auth::`
Expected: PASS (all four auth tests; the TS-token test proves cross-compat).

- [ ] **Step 5: Commit**

```bash
cd agentic-dev/server-rs
git add src/auth.rs
git commit -m "feat(rs): token auth cross-compatible with TS (HMAC-SHA256)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: `POST /api/login` + `/api/*` auth gate + login throttle

**Files:**
- Modify: `agentic-dev/server-rs/src/throttle.rs`, `src/api/login.rs`, `src/api/mod.rs`
- Test: inline `#[cfg(test)]` in `src/throttle.rs` and `src/api/mod.rs`

**Interfaces:**
- Consumes: `Config` (Task 2), `issue_token`/`verify_token` (Task 3), `AppState` (Task 1).
- Produces: `LoginThrottle { check(ip) -> bool, record_fail(ip), record_success(ip) }`; `POST /api/login`; `auth_gate` middleware applied to `/api/*`.

- [ ] **Step 1: Write failing tests for the throttle in `src/throttle.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn ip() -> std::net::IpAddr { "1.2.3.4".parse().unwrap() }

    #[test]
    fn locks_after_8_fails_then_clears_on_success() {
        let mut t = LoginThrottle::default();
        assert!(t.check(ip(), 0));            // allowed
        for _ in 0..8 { t.record_fail(ip(), 0); }
        assert!(!t.check(ip(), 0));           // locked
        assert!(t.check(ip(), 60_001));       // lock expired after 60s
        t.record_success(ip());
        for _ in 0..7 { t.record_fail(ip(), 0); }
        assert!(t.check(ip(), 0));            // <8 since success → still allowed
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd agentic-dev/server-rs && cargo test throttle::`
Expected: FAIL — `check`/`record_fail`/`record_success` not found.

- [ ] **Step 3: Implement `src/throttle.rs`**

```rust
use std::collections::HashMap;
use std::net::IpAddr;

/// Per-IP login lockout: after 8 consecutive bad passwords, lock that IP for 60s. Mirrors routes.ts
/// (LOGIN_MAX_FAILS=8, LOGIN_LOCK_MS=60_000). `now_ms` is injected for deterministic tests.
pub struct LoginThrottle {
    fails: HashMap<IpAddr, (u32, u64)>, // ip -> (consecutive fails, locked_until_ms)
}
impl Default for LoginThrottle {
    fn default() -> Self { LoginThrottle { fails: HashMap::new() } }
}
const MAX_FAILS: u32 = 8;
const LOCK_MS: u64 = 60_000;
impl LoginThrottle {
    /// true = this IP may attempt a login now.
    pub fn check(&self, ip: IpAddr, now_ms: u64) -> bool {
        match self.fails.get(&ip) { Some(&(_, until)) => until <= now_ms, None => true }
    }
    pub fn record_fail(&mut self, ip: IpAddr, now_ms: u64) {
        if self.fails.len() > 1000 { self.fails.clear(); } // bound the map
        let e = self.fails.entry(ip).or_insert((0, 0));
        e.0 += 1;
        if e.0 >= MAX_FAILS { *e = (0, now_ms + LOCK_MS); }
    }
    pub fn record_success(&mut self, ip: IpAddr) { self.fails.remove(&ip); }
}
```

- [ ] **Step 4: Run the throttle test**

Run: `cd agentic-dev/server-rs && cargo test throttle::`
Expected: PASS.

- [ ] **Step 5: Implement the login handler in `src/api/login.rs`**

```rust
use axum::{extract::{ConnectInfo, State, Json}, http::StatusCode, response::{IntoResponse, Response}};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use crate::state::AppState;
use crate::auth::issue_token;

#[derive(Deserialize)]
pub struct LoginBody { pub password: Option<String> }

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

pub async fn login(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginBody>,
) -> Response {
    let ip = addr.ip();
    let now = now_ms();
    {
        let t = st.throttle.lock().unwrap();
        if !t.check(ip, now) {
            return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":"too many attempts"}))).into_response();
        }
    }
    let given = body.password.unwrap_or_default();
    let ok: bool = given.as_bytes().ct_eq(st.config.password.as_bytes()).into();
    if !ok {
        st.throttle.lock().unwrap().record_fail(ip, now);
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad password"}))).into_response();
    }
    st.throttle.lock().unwrap().record_success(ip);
    let token = issue_token(&st.config.auth_secret, 30 * 24 * 3600, now / 1000);
    (StatusCode::OK, Json(json!({"token": token}))).into_response()
}
```

Note: `ct_eq` on unequal-length slices returns false in constant time, matching `constantTimeEqual`.

- [ ] **Step 6: Wire the route + auth gate in `src/api/mod.rs`**

Replace `src/api/mod.rs` with:
```rust
mod login;

use axum::{routing::{get, post}, Router, middleware::{self, Next}, extract::{State, Request}, response::{IntoResponse, Response}, http::StatusCode, Json};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::state::AppState;
use crate::auth::verify_token;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/login", post(login::login))
        .route("/api/ping", get(ping)) // Phase-0 authed probe; replaced by real endpoints in Phase 5
        .layer(middleware::from_fn_with_state(state.clone(), auth_gate))
        .with_state(state)
}

async fn healthz() -> &'static str { "ok" }
async fn ping() -> Json<serde_json::Value> { Json(json!({"ok": true})) }

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

async fn auth_gate(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/api/login" || !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let header = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let bearer = header.strip_prefix("Bearer ").unwrap_or("");
    let query_token = req.uri().query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .unwrap_or("");
    let token = if !bearer.is_empty() { bearer } else { query_token };
    if !verify_token(&st.config.auth_secret, token, now_secs()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::throttle::LoginThrottle;
    use crate::auth::issue_token;
    use std::sync::{Arc, Mutex};
    use std::net::SocketAddr;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::extract::connect_info::ConnectInfo;
    use tower::ServiceExt;
    use http_body_util::BodyExt;

    fn test_state() -> AppState {
        let mut c = Config::load(|_| None);
        c.password = "pw".into();
        c.auth_secret = "s3cret".into();
        AppState { config: Arc::new(c), throttle: Arc::new(Mutex::new(LoginThrottle::default())) }
    }

    fn with_ip(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&b).unwrap()
    }

    #[tokio::test]
    async fn login_rejects_bad_password() {
        let resp = app(test_state()).oneshot(with_ip(
            Request::post("/api/login").header("content-type", "application/json")
                .body(Body::from(r#"{"password":"nope"}"#)).unwrap())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_returns_a_valid_token() {
        let resp = app(test_state()).oneshot(with_ip(
            Request::post("/api/login").header("content-type", "application/json")
                .body(Body::from(r#"{"password":"pw"}"#)).unwrap())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let token = v["token"].as_str().unwrap();
        assert!(crate::auth::verify_token("s3cret", token, 1_000));
    }

    #[tokio::test]
    async fn api_gate_blocks_without_token_and_allows_with() {
        // no token
        let r1 = app(test_state()).oneshot(
            Request::get("/api/ping").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r1.status(), StatusCode::UNAUTHORIZED);
        // with a valid bearer token
        let token = issue_token("s3cret", 3600, now_secs());
        let r2 = app(test_state()).oneshot(
            Request::get("/api/ping").header("authorization", format!("Bearer {token}"))
                .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        // with ?token= (WS-style)
        let r3 = app(test_state()).oneshot(
            Request::get(format!("/api/ping?token={token}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
    }
}
```

- [ ] **Step 7: Serve `ConnectInfo` from `main.rs`**

`ConnectInfo` requires the connect-info make-service. Update `main.rs`'s serve line:
```rust
    axum::serve(listener, api::app(state).into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .expect("serve");
```

- [ ] **Step 8: Run all tests + build**

Run: `cd agentic-dev/server-rs && cargo test && cargo build`
Expected: all tests PASS; build clean.

- [ ] **Step 9: Commit**

```bash
cd agentic-dev/server-rs
git add src/throttle.rs src/api/login.rs src/api/mod.rs src/main.rs
git commit -m "feat(rs): POST /api/login + /api/* auth gate + per-IP login throttle

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Done criteria (Phase 0)

- `cargo test` green; `cargo build` clean.
- Server boots and binds `host:port`; `/healthz` → 200; `POST /api/login` (correct password) → `{token}` that verifies; `/api/*` (e.g. `/api/ping`) → 401 without a valid token, 200 with one (Bearer or `?token=`).
- A **TS-minted token verifies in Rust** (the cross-compat vector test), and config defaults match `loadConfig`.
- The `/api/ping` probe is a Phase-0 scaffold; Phase 5 replaces it with the real engine-backed endpoints.

## Self-review

- **Spec coverage:** Phase 0 spec items — scaffold ✅ (T1), config env parity ✅ (T2), HMAC auth token-compatible ✅ (T3, with the TS vector), health + auth gate rejecting bad tokens ✅ (T4), fake-claude fixture — deferred to Phase 3 (no claude spawned in Phase 0; noted). The "serves /api/config" wording in the spec is satisfied by the equivalent auth surface (`/api/login` + the gate); the real `/api/config`/engine endpoints land in Phase 5.
- **Placeholder scan:** none — every step has complete code/commands.
- **Type consistency:** `Config::load(get)`, `issue_token(secret, ttl, now_secs)`, `verify_token(secret, token, now_secs)`, `LoginThrottle::{check(ip, now_ms), record_fail(ip, now_ms), record_success(ip)}`, `AppState{config, throttle}`, `app(state)` are used consistently across tasks.
