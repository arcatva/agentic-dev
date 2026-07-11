//! Integration test exercising the public library surface from *outside* the crate — only
//! possible now that the implementation lives in `lib.rs` (the binary was previously the sole
//! target). Guards that `Store`/`CreateInput`/`AppState`/`api::app` stay publicly usable.

use agentic_dev_server::api;
use agentic_dev_server::engine::store::{CreateInput, Store};

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("agentic-it-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[tokio::test]
async fn store_create_get_roundtrips_through_public_api() {
    let dir = tmp_dir("store");
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "it1".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = store.get("it1").await.unwrap().unwrap();
    assert_eq!(s.prompt, "hello");
}

#[tokio::test]
async fn router_builds_and_healthz_is_reachable() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let dir = tmp_dir("router");
    let store = std::sync::Arc::new(
        Store::open(dir.join("db.sqlite"), dir.join("logs"))
            .await
            .unwrap(),
    );
    let transcript = std::sync::Arc::new(
        agentic_dev_server::engine::transcript::TranscriptCache::new(8 * 1024 * 1024),
    );
    let cfg = agentic_dev_server::Config::for_test("s3cret", "pw");
    let engine_cfg = agentic_dev_server::EngineConfig {
        src_root: cfg.src_root.clone(),
        worktrees_root: dir.join("wt"),
        log_dir: cfg.log_dir.clone(),
        db_path: cfg.db_path.clone(),
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: cfg.retitle_enabled,
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
    };
    let engine = std::sync::Arc::new(agentic_dev_server::Engine::with_store(
        engine_cfg,
        store.clone(),
        Some(transcript.clone()),
    ));
    let state = agentic_dev_server::AppState {
        config: std::sync::Arc::new(cfg),
        throttle: std::sync::Arc::new(parking_lot::Mutex::new(
            agentic_dev_server::api::throttle::LoginThrottle::default(),
        )),
        store,
        transcript,
        engine,
        usage_cache: std::sync::Arc::new(parking_lot::Mutex::new(
            agentic_dev_server::api::state::UsageCache::default(),
        )),
        usage_inflight: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        usage_fn: None,
    };
    let resp = api::app(state)
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn patch_session_updates_model_effort_mode_permission_mode() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let dir = tmp_dir("patch");
    let store = std::sync::Arc::new(
        Store::open(dir.join("db.sqlite"), dir.join("logs"))
            .await
            .unwrap(),
    );
    let transcript = std::sync::Arc::new(
        agentic_dev_server::engine::transcript::TranscriptCache::new(8 * 1024 * 1024),
    );
    let cfg = agentic_dev_server::Config::for_test("s3cret", "pw");
    let engine_cfg = agentic_dev_server::EngineConfig {
        src_root: cfg.src_root.clone(),
        worktrees_root: dir.join("wt"),
        log_dir: cfg.log_dir.clone(),
        db_path: cfg.db_path.clone(),
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: cfg.retitle_enabled,
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
    };
    let engine = std::sync::Arc::new(agentic_dev_server::Engine::with_store(
        engine_cfg,
        store.clone(),
        Some(transcript.clone()),
    ));

    // Seed a session via the store (bypasses the POST route; proves the PATCH router is wired).
    store
        .create(CreateInput {
            id: "smoke-patch".into(),
            prompt: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // Pre-seed a scheduled auto-resume so the PATCH below can prove that turning the toggle
    // OFF clears it (the documented side effect).
    store
        .update(
            "smoke-patch",
            agentic_dev_server::engine::store::SessionPatch {
                auto_resume_at: Some(Some(9_999_999_999_999)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let state = agentic_dev_server::AppState {
        config: std::sync::Arc::new(cfg.clone()),
        throttle: std::sync::Arc::new(parking_lot::Mutex::new(
            agentic_dev_server::api::throttle::LoginThrottle::default(),
        )),
        store,
        transcript,
        engine,
        usage_cache: std::sync::Arc::new(parking_lot::Mutex::new(
            agentic_dev_server::api::state::UsageCache::default(),
        )),
        usage_inflight: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        usage_fn: None,
    };

    // Mint a valid Bearer token for the auth gate.
    let token = agentic_dev_server::api::auth::issue_token(
        &cfg.auth_secret,
        3600,
        agentic_dev_server::util::now_secs(),
    );
    let auth = format!("Bearer {token}");

    // PATCH the session.
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/sessions/smoke-patch")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"claude-sonnet-4-6","effort":"high","permissionMode":"plan","autoResume":false}"#))
        .unwrap();
    let resp = api::app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "PATCH must return 200");

    // GET the session and assert the fields round-tripped.
    let req = Request::builder()
        .uri("/api/sessions/smoke-patch")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let resp = api::app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET must return 200");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body["session"]["model"], "claude-sonnet-4-6",
        "model must persist"
    );
    assert_eq!(body["session"]["effort"], "high", "effort must persist");
    assert_eq!(
        body["session"]["permissionMode"], "plan",
        "permissionMode must persist"
    );
    assert_eq!(
        body["session"]["autoResume"],
        serde_json::json!(false),
        "autoResume toggle must persist (default is true)"
    );
    assert!(
        body["session"].get("autoResumeAt").is_none(),
        "turning autoResume OFF must clear the scheduled resume time"
    );

    // Re-seed a schedule, then PATCH autoResume:true — enabling must NOT touch the schedule.
    state
        .store
        .update(
            "smoke-patch",
            agentic_dev_server::engine::store::SessionPatch {
                auto_resume_at: Some(Some(8_888_888_888_888)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/sessions/smoke-patch")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"autoResume":true}"#))
        .unwrap();
    let resp = api::app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PATCH autoResume:true must return 200"
    );
    let req = Request::builder()
        .uri("/api/sessions/smoke-patch")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let resp = api::app(state).oneshot(req).await.unwrap();
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["session"]["autoResume"], serde_json::json!(true));
    assert_eq!(
        body["session"]["autoResumeAt"],
        serde_json::json!(8_888_888_888_888i64),
        "enabling the toggle must leave an existing schedule untouched"
    );
}

/// No-op title generator for smoke tests: never produces a title. The
/// smoke tests exercise public-library surface (Store, AppState, api::app)
/// — they don't assert on generated titles.
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
