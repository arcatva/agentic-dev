//! Engine unit tests, split by domain from the former monolithic tests.rs.
//! Shared harness (helpers, fixtures, EngineOverrides) lives here; each submodule
//! pulls it in via `use super::*`.

use super::*;
use std::sync::atomic::Ordering;

static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("agentic-engine-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[derive(Default)]
struct EngineOverrides {
    bridge_path: Option<String>,
    retitle_enabled: Option<bool>,
    max_concurrent: Option<u64>,
    log_fn: Option<LogFn>,
    sync_fn: Option<SyncFn>,
    idle_max_ms: Option<i64>,
    wall_max_ms: Option<i64>,
    idle_ttl_ms: Option<i64>,
    push_fn: Option<PushFn>,
    now_fn: Option<NowFn>,
    usage_fn: Option<UsageFn>,
    title_generator: Option<std::sync::Arc<dyn crate::engine::title_client::TitleGenerator>>,
}

/// No-op title generator used as the default for engine tests that
/// don't care about generated titles. Task 4 swaps this out for the
/// in-memory variant.
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

async fn make_engine(src: &PathBuf, overrides: EngineOverrides) -> Engine {
    let dir = tmp();
    std::fs::create_dir_all(src).unwrap();
    let bridge_path = overrides
        .bridge_path
        .clone()
        .unwrap_or_else(|| fixture("fake-sdk-bridge-ok.sh"));
    let cfg = EngineConfig {
        src_root: src.clone(),
        worktrees_root: dir.join("worktrees"),
        log_dir: dir.join("logs"),
        db_path: dir.join("db.sqlite"),
        title_generator: overrides.title_generator.clone().unwrap_or_else(|| {
            std::sync::Arc::new(NoopTitleGenerator)
                as std::sync::Arc<dyn crate::engine::title_client::TitleGenerator>
        }),
        retitle_enabled: overrides.retitle_enabled.unwrap_or(true),
        max_concurrent: overrides.max_concurrent,
        git_org: "arcatva".into(),
        claude_config_base: dir.join("claude-config"),
        clone_fn: Some(Arc::new(|_url, _dest| {
            Err(std::io::Error::other("clone disabled in tests"))
        })),
        sync_fn: overrides.sync_fn,
        // Tests drive turns through the SAME runner production uses (SdkRunner), with a fake
        // bridge script invoked via `bash` instead of `node sdk-bridge.mjs`. The fake bridge
        // appends canned stream-json to $SDK_BRIDGE_LOG — no real claude, no API cost.
        runner: Some(std::sync::Arc::new(
            crate::engine::sdk_runner::SdkRunner::with_node("bash", bridge_path),
        )),
        log_fn: overrides.log_fn,
        now_fn: overrides.now_fn,
        push_fn: overrides.push_fn,
        usage_fn: overrides.usage_fn,
        idle_max_ms: overrides.idle_max_ms,
        wall_max_ms: overrides.wall_max_ms,
        idle_ttl_ms: overrides.idle_ttl_ms,
        memory_max: None,
        memory_high: None,
        cpu_quota: None,
        tasks_max: None,
    };
    Engine::new(cfg).await.expect("engine::new")
}

// ── Task 6 test-only helpers ─────────────────────────────

impl Engine {
    /// Test-only: back-date last_event_at for a session.
    #[cfg(test)]
    pub fn test_set_last_event_at(&self, id: &str, ts: i64) {
        self.0.state.lock().last_event_at.insert(id.to_string(), ts);
    }
    /// Test-only: back-date turn_started_at for a session.
    #[cfg(test)]
    pub fn test_set_turn_started_at(&self, id: &str, ts: i64) {
        self.0
            .state
            .lock()
            .turn_started_at
            .insert(id.to_string(), ts);
    }
    /// Test-only: set awaiting state for a session.
    #[cfg(test)]
    pub fn test_set_awaiting(&self, id: &str, v: bool) {
        self.0.state.lock().awaiting.insert(id.to_string(), v);
    }
}

async fn test_engine() -> Engine {
    let dir = tmp();
    make_engine(&dir.join("src"), EngineOverrides::default()).await
}

/// Initialize a bare git repo in a temp dir, then copy it into src/<name> as a
/// real git repo with a commit.
fn make_temp_git_repo_in(src: &PathBuf, name: &str) {
    let dest = src.join(name);
    std::fs::create_dir_all(&dest).unwrap();
    // Init git repo
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&dest)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&dest)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&dest)
        .output()
        .unwrap();
    // Create a file and commit
    std::fs::write(dest.join("README.md"), "# test\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&dest)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&dest)
        .output()
        .unwrap();
}

/// Wait for a session to reach the given status, polling periodically.
/// Times out after 30s (panic, not hang).
async fn wait_status(e: &Engine, id: &str, target: &str) {
    // 240s (not 30s): these tests drive real fake-bridge subprocesses through a 120ms-polled
    // lifecycle; under heavy parallel `cargo test` load — especially 2-vCPU CI runners — the
    // status transitions are correct but slow to get scheduled, and a tight deadline produces
    // load-induced flakes (watchdog_idle_reap hit 90s on GitHub Actions). A genuine hang still
    // fails, just later; the CI job timeout is the backstop.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("wait_status timed out waiting for session {id} to reach status={target}; current status={:?}",
                    e.get(id).await.map(|s| s.status));
        }
        if let Some(s) = e.get(id).await {
            if s.status == target {
                return;
            }
            // If we're waiting for something and the session is already in a terminal state
            // that's different, bail early to avoid hanging.
            if matches!(s.status.as_str(), "done" | "failed" | "killed") && s.status != target {
                // Only bail if both are terminal (won't transition further).
                if matches!(target, "done" | "failed" | "killed") {
                    panic!(
                        "wait_status: session {id} reached {}, not {target}",
                        s.status
                    );
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// ── Task 7 tests: followUp / streaming awaitingInput ────────

/// Poll a predicate every 20ms, up to 5 seconds (panic on timeout).
async fn wait_until<F: Fn() -> bool>(pred: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if pred() {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("wait_until: timed out");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// ── Task 8 helpers ─────────────────────────────────────────

/// Build an Engine over a fixed work dir (db_path + log_dir deterministic from `work`).
/// The recover tests open a bare Store, seed rows/logs, drop it, then call this to get
/// a fresh Engine that runs recover() over the same data.
async fn engine_from(work: &PathBuf) -> Engine {
    let cfg = EngineConfig {
        src_root: work.clone(),
        worktrees_root: work.join("worktrees"),
        log_dir: work.join("logs"),
        db_path: work.join("db.sqlite"),
        title_generator: std::sync::Arc::new(NoopTitleGenerator),
        retitle_enabled: true,
        max_concurrent: None,
        git_org: "arcatva".into(),
        claude_config_base: work.join("claude-config"),
        clone_fn: Some(Arc::new(|_url, _dest| {
            Err(std::io::Error::other("clone disabled"))
        })),
        sync_fn: None,
        runner: Some(std::sync::Arc::new(
            crate::engine::sdk_runner::SdkRunner::with_node(
                "bash",
                fixture("fake-sdk-bridge-ok.sh"),
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
    Engine::new(cfg).await.expect("engine_from::new")
}

mod auto_resume;
mod followup;
mod fork_session;
mod interaction;
mod lifecycle_control;
mod native_sync;
mod outbox_activity;
mod recovery;
mod run;
mod titles;
mod workflow;
