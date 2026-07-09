//! Shared HTTP-test helpers (compiled only under cfg(test)).
//! Provides per-call-unique state so parallel #[tokio::test]s each get an isolated DB/WAL dir.
#![cfg(test)]
use std::sync::Arc;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use crate::api::state::AppState;
use crate::api::config::Config;
use crate::api::throttle::LoginThrottle;
use crate::util::now_secs;

static CTR: AtomicU64 = AtomicU64::new(0);

pub async fn test_state() -> AppState {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let mut c = Config::for_test("s3cret", "pw");
    let dir = std::env::temp_dir().join(format!("agentic-p5-{}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    c.db_path = dir.join("db.sqlite");
    c.log_dir = dir.join("logs");
    let _ = std::fs::create_dir_all(&c.log_dir);
    c.worktrees_root = dir.join("worktrees");
    c.src_root = dir.join("src");
    let _ = std::fs::create_dir_all(&c.src_root);
    // Phase 6 paths: use the test temp dir so no writes go to /root/.agentic-dev
    c.groups_path = dir.join("groups.json");
    c.templates_path = dir.join("templates.json");
    c.device_token_path = dir.join("device.json");
    c.skills_dir = dir.join("skills");
    let store = Arc::new(crate::engine::store::Store::open(c.db_path.clone(), c.log_dir.clone()).await.unwrap());
    let transcript = Arc::new(crate::engine::transcript::TranscriptCache::new(64 * 1024 * 1024));
    let engine_cfg = crate::engine::EngineConfig {
        src_root: c.src_root.clone(),
        worktrees_root: c.worktrees_root.clone(),
        log_dir: c.log_dir.clone(),
        db_path: c.db_path.clone(),
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: c.retitle_enabled,
        max_concurrent: Some(2),
        git_org: c.git_org.clone(),
        claude_config_base: c.claude_config_base.clone(),
        clone_fn: Some(Arc::new(|_u: &str, _d: &str| {
            Err(std::io::Error::other("clone disabled in tests"))
        })),
        // Drive turns through the production runner (SdkRunner) with a fake bridge (bash) — no real
        // claude. Routing/auth tests don't run turns, but this keeps the runner production-shaped.
        sync_fn: None, runner: Some(std::sync::Arc::new(crate::engine::sdk_runner::SdkRunner::with_node("bash", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-sdk-bridge-ok.sh").to_string_lossy().into_owned()))), log_fn: None, now_fn: None, push_fn: None, usage_fn: None,
        idle_max_ms: None, wall_max_ms: None, idle_ttl_ms: None,
        memory_max: None, memory_high: None, cpu_quota: None, tasks_max: None,
    };
    let engine = Arc::new(crate::engine::Engine::with_store(engine_cfg, store.clone(), Some(transcript.clone())));
    AppState {
        config: Arc::new(c),
        throttle: Arc::new(Mutex::new(LoginThrottle::default())),
        store, transcript, engine,
        usage_cache: Arc::new(Mutex::new(crate::api::state::UsageCache::default())),
        usage_inflight: Arc::new(tokio::sync::Mutex::new(())),
        usage_fn: None,
    }
}

/// Build a test AppState whose runner is an SdkRunner pointed at a named fake-bridge fixture.
pub async fn test_state_with_fixture(fixture_name: &str) -> AppState {
    let mut st = test_state().await;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures").join(fixture_name).to_string_lossy().into_owned();
    let c = (*st.config).clone();
    let engine_cfg = crate::engine::EngineConfig {
        src_root: c.src_root.clone(),
        worktrees_root: c.worktrees_root.clone(),
        log_dir: c.log_dir.clone(),
        db_path: c.db_path.clone(),
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: c.retitle_enabled,
        max_concurrent: Some(2),
        git_org: c.git_org.clone(),
        claude_config_base: c.claude_config_base.clone(),
        clone_fn: Some(Arc::new(|_u: &str, _d: &str| {
            Err(std::io::Error::other("clone disabled in tests"))
        })),
        // The `fixture` here is a fake SDK bridge script (e.g. fake-sdk-bridge-ok.sh); run it via
        // `bash` through the production SdkRunner so the test exercises the real turn transport.
        sync_fn: None, runner: Some(std::sync::Arc::new(crate::engine::sdk_runner::SdkRunner::with_node("bash", fixture.clone()))), log_fn: None, now_fn: None, push_fn: None, usage_fn: None,
        idle_max_ms: None, wall_max_ms: None, idle_ttl_ms: None,
        memory_max: None, memory_high: None, cpu_quota: None, tasks_max: None,
    };
    let engine = Arc::new(crate::engine::Engine::with_store(engine_cfg, st.store.clone(), Some(st.transcript.clone())));
    st.engine = engine;
    st.config = Arc::new(c);
    // usage_cache, usage_inflight, usage_fn are already initialized in test_state()
    st
}

/// A valid `Bearer <token>` header value for this state's secret.
pub fn auth(st: &AppState) -> String {
    let token = crate::api::auth::issue_token(&st.config.auth_secret, 3600, now_secs());
    format!("Bearer {token}")
}

/// Drive one request through the full router (auth gate included) and return (status, json body).
pub async fn oneshot_req(st: AppState, req: Request<Body>) -> (StatusCode, Value) {
    let resp = crate::api::app(st).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, body)
}

#[allow(dead_code)]
pub async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// No-op title generator for HTTP test fixtures: never produces a title.
/// The router tests don't care about generated titles — they only assert
/// on auth/routing shapes. Engine-level tests use `InMemoryTitleGenerator`
/// in `engine::title_client`.
pub struct NoopTitleGenerator;
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
