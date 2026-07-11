//! Integration tests for `GET /api/sessions/search`.
//!
//! - 400 on missing `q` (required per the API contract).
//! - 400 on empty `q` (matches the contract — the underlying SearchService would
//!   return an empty result set, but the handler must reject the request with 400
//!   to keep the wire contract explicit).
//! - 401 without an auth token (the auth gate runs before the handler).
//! - 200 on a valid `q`, with the shape
//!   `{ "query": "...", "results": [{ "session": {...}, "score": ..., "matches": [...] }] }`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AO};
use std::sync::Arc;

use agentic_dev_server::api;
use agentic_dev_server::api::auth::issue_token;
use agentic_dev_server::api::throttle::LoginThrottle;
use agentic_dev_server::engine::store::{CreateInput, Store};
use agentic_dev_server::engine::transcript::TranscriptCache;
use agentic_dev_server::engine::Engine;
use agentic_dev_server::engine::EngineConfig;
use agentic_dev_server::AppState;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use tower::ServiceExt;

// Per-test counter so each call to make_state() gets its own temp dir. Without this,
// two parallel #[tokio::test]s sharing a tag would race on Store::open (each one's
// remove_dir_all clobbers the other's still-being-created DB).
static DIR_CTR: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(tag: &str) -> PathBuf {
    let n = DIR_CTR.fetch_add(1, AO::SeqCst);
    let d = std::env::temp_dir().join(format!(
        "agentic-search-it-{}-{}-{}",
        std::process::id(),
        tag,
        n,
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn make_state() -> AppState {
    let dir = tmp_dir("state");
    let store = Arc::new(
        Store::open(dir.join("db.sqlite"), dir.join("logs"))
            .await
            .unwrap(),
    );
    let transcript = Arc::new(TranscriptCache::new(8 * 1024 * 1024));
    let cfg = agentic_dev_server::Config::for_test("s3cret", "pw");
    let engine_cfg = EngineConfig {
        src_root: cfg.src_root.clone(),
        worktrees_root: dir.join("wt"),
        log_dir: cfg.log_dir.clone(),
        db_path: cfg.db_path.clone(),
        max_concurrent: Some(1),
        git_org: cfg.git_org.clone(),
        claude_config_base: cfg.claude_config_base.clone(),
        clone_fn: None,
        sync_fn: None,
        runner: None,
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
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: false,
    };
    let engine = Arc::new(Engine::with_store(
        engine_cfg,
        store.clone(),
        Some(transcript.clone()),
    ));
    AppState {
        config: Arc::new(cfg),
        throttle: Arc::new(Mutex::new(LoginThrottle::default())),
        store,
        transcript,
        engine,
        usage_cache: Arc::new(Mutex::new(
            agentic_dev_server::api::state::UsageCache::default(),
        )),
        usage_inflight: Arc::new(tokio::sync::Mutex::new(())),
        usage_fn: None,
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap()
}

fn auth_header(secret: &str) -> String {
    format!(
        "Bearer {}",
        issue_token(secret, 3600, agentic_dev_server::util::now_secs())
    )
}

#[tokio::test]
async fn search_missing_q_returns_400() {
    let st = make_state().await;
    let resp = api::app(st)
        .oneshot(
            Request::get("/api/sessions/search")
                .header("authorization", auth_header("s3cret"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "missing `q` must be 400"
    );
    let v = body_json(resp).await;
    assert_eq!(v["error"], "missing query");
}

#[tokio::test]
async fn search_empty_q_returns_400() {
    let st = make_state().await;
    let resp = api::app(st)
        .oneshot(
            Request::get("/api/sessions/search?q=")
                .header("authorization", auth_header("s3cret"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "empty `q` must be 400"
    );
    let v = body_json(resp).await;
    assert_eq!(v["error"], "missing query");
}

#[tokio::test]
async fn search_without_token_returns_401() {
    // The auth gate must block before the handler runs. This guards against the route being
    // accidentally registered outside the auth-gated sub-router.
    let st = make_state().await;
    let resp = api::app(st)
        .oneshot(
            Request::get("/api/sessions/search?q=hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn search_valid_q_returns_200_with_response_shape() {
    // Seed one session so SearchService has something to return. The response shape must be
    // { "query": ..., "results": [{ "session": {...}, "score": ..., "matches": [...] }] }.
    let st = make_state().await;
    st.store
        .create(CreateInput {
            id: "s1".into(),
            prompt: "fix the routing bug in the login flow".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let resp = api::app(st)
        .oneshot(
            Request::get("/api/sessions/search?q=routing")
                .header("authorization", auth_header("s3cret"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "valid `q` must be 200");
    let v = body_json(resp).await;
    assert_eq!(
        v["query"], "routing",
        "response must echo the trimmed query"
    );
    let results = v["results"].as_array().expect("results must be an array");
    assert!(
        !results.is_empty(),
        "session with prompt containing 'routing' must match"
    );
    let hit = &results[0];
    // Each hit must have { session, score, matches[] } in that shape.
    assert!(hit["session"].is_object(), "hit.session must be an object");
    assert_eq!(
        hit["session"]["id"], "s1",
        "hit.session.id must be the seeded session"
    );
    assert!(hit["score"].is_number(), "hit.score must be a number");
    let matches = hit["matches"]
        .as_array()
        .expect("hit.matches must be an array");
    assert!(
        !matches.is_empty(),
        "must surface at least one match for a Tier-A prompt hit"
    );
    let m = &matches[0];
    assert!(m["field"].is_string(), "match.field must be a string");
    assert!(m["snippet"].is_string(), "match.snippet must be a string");
    assert!(
        m["lineIndex"].is_number(),
        "match.lineIndex must be a number"
    );
}

/// No-op title generator for search integration tests: never produces a
/// title. The search tests assert on the SearchResponse shape, not on
/// generated titles.
struct NoopTitleGenerator;
#[async_trait::async_trait]
impl agentic_dev_server::engine::title_client::TitleGenerator for NoopTitleGenerator {
    async fn generate(
        &self,
        _p: &str,
        _cwd: &std::path::Path,
    ) -> Result<Option<String>, agentic_dev_server::engine::title_client::TitleGeneratorError> {
        Ok(None)
    }
    async fn maybe_retitle(
        &self,
        _c: &str,
        _m: &[(String, String)],
        _cwd: &std::path::Path,
    ) -> Result<Option<String>, agentic_dev_server::engine::title_client::TitleGeneratorError> {
        Ok(None)
    }
}
