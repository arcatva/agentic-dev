//! Typed engine errors. Deliberately HTTP-framework-free (the engine never imports axum); the
//! API layer owns the `EngineError` → HTTP-status mapping (see `api::engine_error_response`).
//!
//! `Display` reproduces the exact strings the engine returned when these were `Result<_, String>`,
//! so error bodies stay byte-for-byte identical for the Android client.

use crate::engine::status::SessionStatus;
use crate::engine::store::StoreError;

#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    /// Session id not found (a race vs the route's own pre-check). → HTTP 404.
    #[error("unknown session: {0}")]
    NotFound(String),

    /// Session is running/pending/queued — the requested op needs an idle session. → 400.
    #[error("session busy")]
    Busy,

    /// The worktree was already discarded. → 400.
    #[error("worktree already cleaned")]
    WorktreeCleaned,

    /// The session has no live worktree. → 400.
    #[error("session has no worktree")]
    NoWorktree,

    /// Caller-fixable bad input (unknown repo, bad sha, …); carries the exact message. → 400.
    #[error("{0}")]
    BadInput(String),

    /// A source resource (worktree, HEAD) the operation depends on is missing or unreadable.
    /// Distinct from `NotFound` (which is a session id lookup miss): the SOURCE exists, but its
    /// git state is broken. → 409 Conflict — the caller can retry after fixing the source.
    #[error("source unhealthy: {0}")]
    SourceUnhealthy(String),

    /// Underlying store failure. → 500.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Unexpected internal failure (e.g. a join error). → 500.
    #[error("{0}")]
    Internal(String),

    /// PR3: a state transition was rejected by the legal-transitions matrix.
    /// The compiler enforces "all `to`s are handled" at the call site; this
    /// error fires only at runtime when the engine is asked to make a move
    /// the matrix says is illegal (e.g. `Running → Pending`, or any move
    /// from a terminal). The on_exit preserves-terminal path catches this
    /// and downgrades to a tracing::warn rather than propagating.
    #[error("illegal session transition: {from} → {to}")]
    IllegalTransition {
        from: SessionStatus,
        to: SessionStatus,
    },
}
