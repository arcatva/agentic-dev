use std::sync::Arc;
use parking_lot::Mutex;
use crate::api::config::Config;
use crate::engine::Engine;
use crate::api::throttle::LoginThrottle;
use crate::engine::store::Store;
use crate::engine::transcript::TranscriptCache;

#[derive(Default)]
pub struct UsageCache {
    pub at: i64,                       // epoch ms of last SUCCESSFUL fetch
    pub last_attempt_at: i64,          // epoch ms of last fetch attempt (success OR failure)
    pub data: Option<serde_json::Value>,
    pub last_error: Option<String>,    // upstream error from the most recent failed fetch
}

/// Boxed async function type for injectable usage fetcher (test seam).
pub type UsageFn = Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, crate::engine::usage::UsageError>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub throttle: Arc<Mutex<LoginThrottle>>,
    pub store: Arc<Store>,
    pub transcript: Arc<TranscriptCache>,
    pub engine: Arc<Engine>,
    pub usage_cache: Arc<Mutex<UsageCache>>,
    pub usage_inflight: Arc<tokio::sync::Mutex<()>>,
    pub usage_fn: Option<UsageFn>,
}
