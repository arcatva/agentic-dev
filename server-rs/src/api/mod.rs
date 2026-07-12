mod login;
pub(crate) mod misc;
pub(crate) mod oauth;
mod sessions;
pub mod stream;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod validation;

// HTTP-layer support modules.
pub mod auth;
pub mod config;
pub mod state;
pub mod throttle;
pub mod tls;

use crate::api::auth::verify_token;
use crate::api::state::AppState;
use crate::util::now_secs;
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::{predicate::SizeAbove, CompressionLayer};
use tower_http::trace::TraceLayer;

/// Gzip responses larger than this (2048-byte threshold).
const COMPRESS_MIN_BYTES: u16 = 2048;

/// Build the Axum router. **Must** be served with
/// `.into_make_service_with_connect_info::<SocketAddr>()` in production so that the
/// per-IP throttle in `/api/login` receives a real client address.
pub fn app(state: AppState) -> Router {
    // Raise the body limit for the upload route to upload_max_bytes + a small header
    // slack (4 KB). Axum 0.8's default is 2 MB which silently rejects larger payloads.
    let upload_limit = state.config.upload_max_bytes + 4 * 1024;
    // Routes whose JSON bodies benefit from gzip. The CompressionLayer's predicate sees
    // only the response (not the request path), so the two routes that must NOT be
    // compressed are added *after* this layer (below): /file (keep its Content-Length for
    // client download progress; cf. commit 8be1345) and /stream (a WebSocket upgrade).
    let compressed = Router::new()
        .route("/healthz", get(healthz))
        // Public: the active server certificate (PEM). Safe to serve unauthenticated — it's the
        // same public cert presented in every TLS handshake — so a client can fetch + pin it
        // (trust-on-first-use) before it can log in. Exempted from auth in `auth_gate` below.
        .route("/api/tls/cert.pem", get(tls_cert_pem))
        .route("/api/login", post(login::login))
        .route(
            "/api/sessions",
            get(sessions::list_sessions).post(sessions::create_session),
        )
        // /api/sessions/search and /api/sessions/{id}/events MUST be registered BEFORE
        // /api/sessions/{id} — otherwise axum's `{id}` capture would match the literal
        // path segments and the route would 404.
        .route("/api/sessions/search", get(sessions::search_sessions))
        .route(
            "/api/sessions/{id}/events",
            get(sessions::get_session_events),
        )
        // Native Claude re-sync surface. `/api/adoptable` lists importable transcripts;
        // `/api/sessions/adopt` MUST be registered BEFORE `/api/sessions/{id}` — otherwise
        // axum's `{id}` capture would swallow the literal "adopt" segment and 404 the POST.
        .route("/api/adoptable", get(sessions::list_adoptable))
        .route("/api/sessions/adopt", post(sessions::adopt_session_route))
        .route(
            "/api/sessions/{id}",
            get(sessions::get_session)
                .patch(sessions::patch_session)
                .delete(sessions::delete_session_route),
        )
        .route(
            "/api/sessions/{id}/detach",
            post(sessions::detach_session_route),
        )
        .route("/api/sessions/{id}/messages", post(sessions::post_message))
        .route(
            "/api/sessions/{id}/interrupt",
            post(sessions::interrupt_route),
        )
        .route(
            "/api/sessions/{id}/permission",
            post(sessions::permission_route),
        )
        .route("/api/sessions/{id}/discard", post(sessions::discard_route))
        .route("/api/sessions/{id}/delete", post(sessions::remove_route))
        .route(
            "/api/sessions/{id}/fork",
            post(sessions::fork_session_route),
        )
        .route(
            "/api/sessions/{id}/workflows",
            get(sessions::workflows_route),
        )
        .route(
            "/api/sessions/{id}/workflows/{runId}/agents/{agentId}",
            get(sessions::workflow_agent_route),
        )
        .route("/api/sessions/{id}/commits", get(sessions::commits_route))
        .route(
            "/api/sessions/{id}/commits/{sha}/files",
            get(sessions::commit_files_route),
        )
        .route(
            "/api/sessions/{id}/commits/{sha}/diff",
            get(sessions::commit_diff_route),
        )
        .route("/api/sessions/{id}/rewind", post(sessions::rewind_route))
        .route(
            "/api/sessions/{id}/upload",
            post(sessions::upload_route).layer(DefaultBodyLimit::max(upload_limit)),
        )
        // Pre-session staging upload (New-request attachments): no session id, returns a token the
        // create request references. Same raised body limit as the per-session upload route.
        .route(
            "/api/uploads",
            post(sessions::upload_staging_route).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/sessions/{id}/outbox", get(sessions::outbox_route))
        .route("/api/usage", get(misc::usage_route))
        .route("/api/repos", get(misc::repos_route))
        .route(
            "/api/skills",
            get(misc::skills_route).post(misc::skills_add_route),
        )
        .route("/api/skills/catalog", get(misc::skills_catalog_route))
        .route(
            "/api/skills/sources",
            get(misc::skills_sources_route)
                .post(misc::skills_sources_add_route)
                .delete(misc::skills_sources_delete_route),
        )
        .route("/api/skills/install", post(misc::skills_install_route))
        .route("/api/skills/{name}", delete(misc::skills_delete_route))
        .route(
            "/api/plugins",
            get(misc::plugins_route).post(misc::plugins_add_route),
        )
        .route("/api/plugins/{id}", delete(misc::plugins_delete_route))
        .route("/api/mcp-servers", post(misc::mcp_add_route))
        .route("/api/mcp-servers/{name}", delete(misc::mcp_delete_route))
        .route("/api/global-settings", get(misc::global_settings_route))
        .route(
            "/api/global-settings/toggle",
            post(misc::global_settings_toggle_route),
        )
        .route(
            "/api/groups",
            get(misc::groups_list).post(misc::groups_create),
        )
        .route(
            "/api/groups/{id}",
            patch(misc::groups_update).delete(misc::groups_delete),
        )
        .route("/api/devices", post(misc::devices_post))
        .route(
            "/api/templates",
            get(misc::templates_get).put(misc::templates_put),
        )
        .route("/api/templates/start", post(misc::templates_start))
        .route("/api/models", get(misc::models_get))
        .route(
            "/api/providers",
            get(misc::providers_get).post(misc::providers_post),
        )
        .route("/api/providers/{name}", delete(misc::providers_delete))
        .route("/api/oauth/openai/start", post(oauth::oauth_start))
        .route("/api/oauth/openai/complete", post(oauth::oauth_complete))
        .route("/api/native-models", get(misc::native_models_get))
        .route(
            "/api/native-models/{family}",
            post(misc::native_models_post).delete(misc::native_models_delete),
        )
        .route(
            "/api/routing",
            get(misc::routing_get).post(misc::routing_post),
        )
        .layer(CompressionLayer::new().compress_when(SizeAbove::new(COMPRESS_MIN_BYTES)));

    compressed
        // NOT gzipped — added after the compression layer so it doesn't wrap them.
        .route("/api/sessions/{id}/ack", put(sessions::ack_session))
        .route("/api/sessions/{id}/stream", get(stream::stream_session))
        .route("/api/sessions/{id}/file", get(sessions::file_route))
        .layer(middleware::from_fn_with_state(state.clone(), auth_gate))
        // Outer layers (applied last = outermost): TraceLayer logs request
        // method/path/status/latency (DEBUG) + 5xx failures (ERROR); CatchPanicLayer turns any
        // handler panic into a logged 500 JSON instead of a dropped connection. CatchPanic sits
        // inside Trace so Trace records the converted 500.
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Serve the active server certificate as PEM so a client can download + pin it (trust-on-first-use).
/// Public (see `auth_gate`): a server cert is not secret. 404 when TLS is disabled or the file is
/// missing. The path is derived from config the same way the listener resolves it.
async fn tls_cert_pem(State(st): State<AppState>) -> Response {
    use crate::api::tls::TlsMode;
    let path = match TlsMode::from_config(&st.config) {
        TlsMode::Disabled => return (StatusCode::NOT_FOUND, "TLS is disabled").into_response(),
        TlsMode::Byo { cert, .. } => cert,
        TlsMode::SelfSigned { dir, .. } => dir.join("cert.pem"),
    };
    match tokio::fs::read(&path).await {
        // Strip to CERTIFICATE blocks only — never leak a private key even if an operator points
        // TLS_CERT at a combined cert+key PEM.
        Ok(bytes) => {
            let pem = crate::api::tls::certs_only_pem(&bytes);
            if pem.is_empty() {
                return (StatusCode::NOT_FOUND, "certificate not available").into_response();
            }
            (
                [(axum::http::header::CONTENT_TYPE, "application/x-pem-file")],
                pem,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "certificate not available").into_response(),
    }
}

/// CatchPanicLayer handler: log the panic with context and return a clean 500 JSON body instead
/// of a reset connection.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let msg = err
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| err.downcast_ref::<String>().map(|s| s.as_str()))
        .unwrap_or("<non-string panic>");
    tracing::error!(target: "panic", "request handler panicked: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"internal server error"})),
    )
        .into_response()
}

async fn auth_gate(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    // Public routes: login, the (public) server cert download, and anything outside /api/.
    if path == "/api/login" || path == "/api/tls/cert.pem" || !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let bearer = header.strip_prefix("Bearer ").unwrap_or("");
    let query_token = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .unwrap_or("");
    let token = if !bearer.is_empty() {
        bearer
    } else {
        query_token
    };
    if !verify_token(&st.config.auth_secret, token, now_secs()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::issue_token;
    use crate::api::config::Config;
    use crate::api::throttle::LoginThrottle;
    use axum::body::Body;
    use axum::extract::connect_info::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use parking_lot::Mutex;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering as AO};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Per-call unique counter so parallel `#[tokio::test]`s each get their own DB/WAL directory.
    static API_CTR: AtomicU64 = AtomicU64::new(0);

    fn c_budget() -> usize {
        64 * 1024 * 1024
    }

    async fn test_state() -> AppState {
        let n = API_CTR.fetch_add(1, AO::SeqCst);
        let mut c = Config::load(|_| None);
        c.password = "pw".into();
        c.auth_secret = "s3cret".into();
        let dir = std::env::temp_dir().join(format!("agentic-apitest-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Phase 6: redirect writable paths into the temp dir
        c.groups_path = dir.join("groups.json");
        c.templates_path = dir.join("templates.json");
        c.device_token_path = dir.join("device.json");
        c.skills_dir = dir.join("skills");
        c.claude_config_base = dir.join("claude");
        let _ = std::fs::create_dir_all(&c.claude_config_base);
        let store = Arc::new(
            crate::engine::store::Store::open(dir.join("db.sqlite"), dir.join("logs"))
                .await
                .unwrap(),
        );
        let transcript = Arc::new(crate::engine::transcript::TranscriptCache::new(c_budget()));
        let engine_cfg = crate::engine::EngineConfig {
            src_root: c.src_root.clone(),
            worktrees_root: dir.join("worktrees"),
            log_dir: c.log_dir.clone(),
            db_path: c.db_path.clone(),
            title_generator: std::sync::Arc::new(NoopTitleGenerator),
            retitle_enabled: c.retitle_enabled,
            max_concurrent: Some(1),
            git_org: c.git_org.clone(),
            claude_config_base: c.claude_config_base.clone(),
            clone_fn: Some(Arc::new(|_url: &str, _dest: &str| {
                Err(std::io::Error::other("clone disabled in tests"))
            })),
            sync_fn: None,
            // Test AppState: drive turns through the production runner (SdkRunner) with a fake
            // bridge script (bash) — no real claude, no API cost.
            runner: Some(std::sync::Arc::new(
                crate::engine::sdk_runner::SdkRunner::with_node(
                    "bash",
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-sdk-bridge-ok.sh")
                        .to_string_lossy()
                        .into_owned(),
                ),
            )),
            log_fn: None,
            now_fn: None,
            push_fn: None,
            usage_fn: None,
            idle_max_ms: None,
            wall_max_ms: None,
            idle_ttl_ms: None,
            memory_max: None,
            memory_high: None,
            cpu_quota: None,
            tasks_max: None,
        };
        let engine = Arc::new(crate::engine::Engine::with_store(
            engine_cfg,
            store.clone(),
            Some(transcript.clone()),
        ));
        AppState {
            config: Arc::new(c),
            throttle: Arc::new(Mutex::new(LoginThrottle::default())),
            store,
            transcript,
            engine,
            usage_cache: Arc::new(Mutex::new(crate::api::state::UsageCache::default())),
            usage_inflight: Arc::new(tokio::sync::Mutex::new(())),
            usage_fn: None,
            oauth_fn: None,
        }
    }

    fn with_ip(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&b).unwrap()
    }

    /// No-op title generator for the router test fixture: never produces a
    /// title (so the session keeps its original prompt). Replaced by
    /// `InMemoryTitleGenerator` for the engine-level tests in engine/tests.rs.
    struct NoopTitleGenerator;
    #[async_trait::async_trait]
    impl crate::engine::title_client::TitleGenerator for NoopTitleGenerator {
        async fn generate(
            &self,
            _p: &str,
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, crate::engine::title_client::TitleGeneratorError> {
            Ok(None)
        }
        async fn maybe_retitle(
            &self,
            _c: &str,
            _m: &[(String, String)],
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, crate::engine::title_client::TitleGeneratorError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let resp = app(test_state().await)
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn catch_panic_layer_converts_panic_to_logged_500_json() {
        // A handler that panics must yield a controlled 500 JSON, not a dropped connection.
        async fn boom() -> &'static str {
            panic!("kaboom")
        }
        let app = Router::new()
            .route("/boom", get(boom))
            .layer(CatchPanicLayer::custom(handle_panic));
        let resp = app
            .oneshot(Request::get("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "internal server error");
    }

    #[tokio::test]
    async fn login_rejects_bad_password() {
        let resp = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"nope"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_with_empty_body_is_401_not_415() {
        // Lenient body parse: an empty / missing
        // content-type body → empty password → 401, not a 415/422 from a strict Json extractor.
        let resp = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login").body(Body::empty()).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_returns_a_valid_token() {
        let resp = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"pw"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let token = v["token"].as_str().unwrap();
        assert!(crate::api::auth::verify_token("s3cret", token, 1_000));
    }

    // Fix 1: SHA-256 hashing means length differences don't leak — both must 401.
    #[tokio::test]
    async fn login_rejects_shorter_and_longer_passwords() {
        // Much shorter than "pw"
        let r1 = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"p"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            r1.status(),
            StatusCode::UNAUTHORIZED,
            "shorter password must 401"
        );
        // Much longer than "pw"
        let r2 = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"password":"password_much_longer_than_two_chars"}"#,
                    ))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            r2.status(),
            StatusCode::UNAUTHORIZED,
            "longer password must 401"
        );
        // Correct password still 200
        let r3 = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"pw"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(r3.status(), StatusCode::OK, "correct password must 200");
    }

    // Fix 2: missing ConnectInfo must not 500.
    #[tokio::test]
    async fn login_without_connect_info_does_not_500() {
        // Bad password — no ConnectInfo extension, must 401 (not 500).
        let r1 = app(test_state().await)
            .oneshot(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r1.status(),
            StatusCode::UNAUTHORIZED,
            "bad pw without ConnectInfo must 401"
        );
        // Correct password — no ConnectInfo extension, must 200.
        let r2 = app(test_state().await)
            .oneshot(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"pw"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r2.status(),
            StatusCode::OK,
            "correct pw without ConnectInfo must 200"
        );
    }

    #[tokio::test]
    async fn appstate_engine_list_returns_empty_for_fresh_store() {
        let state = test_state().await;
        let sessions = state.engine.list().await;
        assert!(sessions.is_empty(), "fresh engine should have no sessions");
    }

    #[tokio::test]
    async fn api_gate_blocks_without_token_and_allows_with() {
        // no token
        let r1 = app(test_state().await)
            .oneshot(Request::get("/api/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::UNAUTHORIZED);
        // with a valid bearer token
        let token = issue_token("s3cret", 3600, now_secs());
        let r2 = app(test_state().await)
            .oneshot(
                Request::get("/api/sessions")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        // with ?token= (WS-style)
        let r3 = app(test_state().await)
            .oneshot(
                Request::get(format!("/api/sessions?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
    }

    // The auth gate must 401 on every kind of bad credential — a missing header, a non-Bearer
    // scheme, a token signed with the wrong secret, and an expired-but-well-formed token — and
    // only let a freshly issued valid token through. Each case hits a fresh state so the throttle
    // (which only guards /api/login) can never interfere.
    #[tokio::test]
    async fn api_gate_rejects_every_bad_credential_shape() {
        // expired: issued far in the past so exp <= now.
        let expired = issue_token("s3cret", 1, now_secs() - 10_000);
        // wrong secret: well-formed token but signed with a different key.
        let wrong_secret = issue_token("not-the-secret", 3600, now_secs());
        let cases: Vec<(&str, Option<String>)> = vec![
            ("missing header", None),
            (
                "no Bearer prefix",
                Some(issue_token("s3cret", 3600, now_secs())),
            ), // raw token, no "Bearer "
            (
                "wrong scheme",
                Some(format!("Basic {}", issue_token("s3cret", 3600, now_secs()))),
            ),
            ("garbage token", Some("Bearer not-a-real-token".to_string())),
            ("expired token", Some(format!("Bearer {expired}"))),
            ("wrong secret", Some(format!("Bearer {wrong_secret}"))),
        ];
        for (label, header) in cases {
            let mut b = Request::get("/api/sessions");
            if let Some(h) = header {
                b = b.header("authorization", h);
            }
            let resp = app(test_state().await)
                .oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "case `{label}` must 401"
            );
            let v = body_json(resp).await;
            assert_eq!(v["error"], "unauthorized", "case `{label}` body shape");
        }
        // Sanity: a valid token on the same route is allowed.
        let good = issue_token("s3cret", 3600, now_secs());
        let ok = app(test_state().await)
            .oneshot(
                Request::get("/api/sessions")
                    .header("authorization", format!("Bearer {good}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            ok.status(),
            StatusCode::OK,
            "valid token must pass the gate"
        );
    }

    // /healthz and /api/login are the only two routes that must be reachable with NO auth header.
    // /healthz is not under /api/ (gate skips it); /api/login is explicitly exempted in auth_gate.
    #[tokio::test]
    async fn healthz_and_login_bypass_the_auth_gate() {
        // /healthz, no token → 200 "ok".
        let h = app(test_state().await)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(h.status(), StatusCode::OK, "/healthz must not require auth");

        // /api/login, no token, correct password → 200 with a token (gate did not block it).
        let l = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"pw"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            l.status(),
            StatusCode::OK,
            "/api/login must be reachable without a token"
        );
        let v = body_json(l).await;
        assert!(v["token"].as_str().is_some(), "login should return a token");
    }

    // The per-IP login throttle locks after MAX_FAILS (8) consecutive bad passwords from the SAME
    // IP: the first 8 bad attempts return 401 "bad password", and the 9th is rejected with 429
    // "too many attempts" — even though it carries the SAME bad password. Reuse ONE state so the
    // throttle map persists across requests; with_ip pins a stable client IP for the counter.
    #[tokio::test]
    async fn login_throttle_locks_same_ip_after_eight_bad_attempts() {
        let st = test_state().await;
        for i in 0..8 {
            let resp = app(st.clone())
                .oneshot(with_ip(
                    Request::post("/api/login")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"password":"wrong"}"#))
                        .unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "bad attempt {i} should 401"
            );
            let v = body_json(resp).await;
            assert_eq!(v["error"], "bad password", "attempt {i} body");
        }
        // 9th attempt from the same IP is now locked out.
        let locked = app(st.clone())
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            locked.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "9th attempt must be throttled"
        );
        let v = body_json(locked).await;
        assert_eq!(v["error"], "too many attempts");
        // Even the CORRECT password is locked out while the IP lock holds (lock precedes the
        // password check in the handler).
        let still_locked = app(st.clone())
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"pw"}"#))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            still_locked.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "correct password must still be locked out while the IP is locked"
        );
    }

    // An authenticated request to a route the router has no handler for must 404 (the gate passed,
    // but no match) — NOT 401 and NOT 500. Confirms the gate lets /api/ traffic through to routing.
    #[tokio::test]
    async fn unknown_api_route_with_valid_token_is_404() {
        let token = issue_token("s3cret", 3600, now_secs());
        let resp = app(test_state().await)
            .oneshot(
                Request::get("/api/does-not-exist")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // A malformed JSON body on a POST must produce a controlled 4xx from the Json extractor, never
    // a panic-driven 500. /api/login is the gate-exempt POST we can exercise without a token; a
    // body that is not valid JSON must be rejected as a client error before the handler runs.
    #[tokio::test]
    async fn malformed_json_body_on_post_is_client_error_not_500() {
        let resp = app(test_state().await)
            .oneshot(with_ip(
                Request::post("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from("{not valid json"))
                    .unwrap(),
            ))
            .await
            .unwrap();
        let s = resp.status();
        assert!(s.is_client_error(), "malformed JSON must be a 4xx, got {s}");
        assert_ne!(s, StatusCode::INTERNAL_SERVER_ERROR, "must not be a 500");
    }

    #[tokio::test]
    async fn global_settings_get_and_toggle() {
        let st = test_state().await;
        let base = st.config.claude_config_base.clone();
        let skills = st.config.skills_dir.clone();

        // Seed one installed plugin and one user skill.
        std::fs::create_dir_all(base.join("plugins")).unwrap();
        std::fs::write(
            base.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"gh@m":[{"scope":"user"}]}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("rke2-ops")).unwrap();
        std::fs::write(
            skills.join("rke2-ops").join("SKILL.md"),
            "---\nname: rke2-ops\ndescription: d\n---\nb",
        )
        .unwrap();

        let token = issue_token("s3cret", 3600, now_secs());

        // GET → both components present, default enabled.
        let resp = app(st.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/global-settings")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let arr: serde_json::Value = body_json(resp).await;
        assert!(arr
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "gh@m" && c["globalEnabled"] == true));
        assert!(arr
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "skill" && c["id"] == "rke2-ops" && c["globalEnabled"] == true));

        // POST toggle → disable the plugin globally.
        let resp = app(st.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/global-settings/toggle")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"kind":"plugin","id":"gh@m","enabled":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // settings.local.json now records the disable; settings.json untouched.
        let local: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(base.join("settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(local["enabledPlugins"]["gh@m"], serde_json::json!(false));
        assert!(!base.join("settings.json").exists());
    }

    #[tokio::test]
    async fn global_settings_toggle_unknown_kind_is_400() {
        // "mcp" is a VALID kind now (global MCP toggle) — use a genuinely unknown kind.
        let st = test_state().await;
        let token = issue_token("s3cret", 3600, now_secs());
        let resp = app(st.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/global-settings/toggle")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"widget","id":"x","enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("unknown kind"));
    }

    #[tokio::test]
    async fn global_settings_toggle_unknown_mcp_id_is_400() {
        // mcp is a valid kind, but an id not present in .claude.json must 400 on the known-check.
        let st = test_state().await;
        let token = issue_token("s3cret", 3600, now_secs());
        let resp = app(st.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/global-settings/toggle")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"kind":"mcp","id":"no-such-server","enabled":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("unknown mcp id"));
    }

    #[tokio::test]
    async fn global_settings_toggle_unknown_id_is_400() {
        // A valid kind but an id that has no installed component must return 400 with no side effect.
        let st = test_state().await;
        let base = st.config.claude_config_base.clone();
        let token = issue_token("s3cret", 3600, now_secs());

        // No plugins or skills seeded — "does-not-exist@x" is unknown.
        let resp = app(st.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/global-settings/toggle")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"kind":"plugin","id":"does-not-exist@x","enabled":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(
            v["error"].as_str().unwrap().contains("does-not-exist@x"),
            "error message should name the bogus id: {}",
            v["error"]
        );

        // No settings.local.json must have been written — rejection produces no side effect.
        assert!(
            !base.join("settings.local.json").exists(),
            "settings.local.json must not be created on a rejected toggle"
        );
    }
}
