//! agentic-dev-server — the HTTP + WebSocket backend that spawns headless `claude` sessions in
//! git worktrees and streams them to the Android client.
//!
//! Crate shape: this `lib` holds the whole implementation so it can be unit- AND
//! integration-tested as a library (see `tests/`); `main.rs` is a thin binary that builds the
//! config/store/engine and serves the router.
//!
//! ## Layers
//! - **`engine/`** — the HTTP-independent core: the `Engine`
//!   orchestrator plus `store`, `transcript`, `spawner`, `runner`, `sdk_runner`, `tailer`,
//!   `stream`, `structured_diff`, `worktree`, `workflows`, `repos`, `templates`, `groups`,
//!   `skills`, `usage`, `push`, `session_guide`, `classify_error`,
//!   `atomic_write`. Must stay free of the HTTP framework (axum).
//! - **`api/`** — the HTTP layer: routes + WS + middleware, plus
//!   `auth`, `config`, `throttle`, `state` (the axum `AppState`).
//! - **Cross-cutting leaf**: `util`.

pub mod api;
pub mod engine;
pub mod util;

// Convenience re-exports for the binary and integration tests.
pub use api::config::Config;
pub use api::state::AppState;
pub use engine::{Engine, EngineConfig};
