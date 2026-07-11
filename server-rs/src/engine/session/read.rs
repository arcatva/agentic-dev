use std::future::Future;

// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
use crate::engine::runner::Runner;
use crate::engine::store::{Session, SessionPatch};
use crate::engine::title_client::TitleGenerator;

use crate::engine::*;

impl Engine {
    // ── Public read surface ───────────────────────────────────

    /// Best-effort session list — degrades a store failure to an EMPTY list. Kept for internal callers
    /// (search, recovery) that prefer a degraded result over surfacing an error. The HTTP handler must
    /// use [Engine::try_list] instead: a 200-with-empty laundered from a store error makes the Android
    /// client REPLACE its good list with nothing, briefly "losing" sessions (most visibly the
    /// recently-active ones) until the next poll.
    pub async fn list(&self) -> Vec<Session> {
        self.try_list().await.unwrap_or_default()
    }

    /// Fallible session list. A store error (e.g. transient sqlite lock contention from concurrently
    /// running sessions writing their status/activity) propagates as [EngineError::Store] so the API
    /// can answer 5xx — the Android client then keeps its last-good list (blip tolerance) instead of
    /// blanking it on a successful-but-empty response.
    pub async fn try_list(&self) -> Result<Vec<Session>, EngineError> {
        let sessions = self.0.store.list().await?;
        Ok(sessions
            .into_iter()
            .map(|s| self.with_activity(s))
            .collect())
    }

    pub async fn get(&self, id: &str) -> Option<Session> {
        self.0
            .store
            .get(id)
            .await
            .ok()
            .flatten()
            .map(|s| self.with_activity(s))
    }

    /// Update session metadata (model/effort/mode/permission_mode) and return the refreshed session.
    /// Returns Err(StoreError) on DB failure; returns Err(EngineError::NotFound) if the session
    /// does not exist after the update (session was deleted concurrently).
    pub async fn patch_session_meta(
        &self,
        id: &str,
        patch: SessionPatch,
    ) -> Result<Session, EngineError> {
        self.0
            .store
            .update(id, patch)
            .await
            .map_err(EngineError::Store)?;
        self.0
            .store
            .get(id)
            .await
            .map_err(EngineError::Store)?
            .map(|s| self.with_activity(s))
            .ok_or_else(|| EngineError::NotFound(id.to_string()))
    }

    pub fn get_log(&self, id: &str) -> Vec<String> {
        self.0.store.read_log(id)
    }

    /// The Claude config base dir (`~/.claude` by default) this engine reads native
    /// transcripts from. Thin accessor so the API layer can call
    /// [crate::engine::native_transcript::scan_adoptable] without reaching into engine
    /// internals — keeps the engine axum-free.
    pub fn config_base(&self) -> std::path::PathBuf {
        self.0.cfg.claude_config_base.clone()
    }

    /// The set of `claudeSessionId`s already linked to a stored session — the exclusion
    /// set for [crate::engine::native_transcript::scan_adoptable] so an already-adopted (or
    /// natively-linked) transcript is hidden from the adoptable list. Rows without a linked
    /// csid contribute nothing; a store error degrades to an empty set (scan then offers
    /// everything, which the adopt guard still rejects on a double-adopt).
    pub async fn known_claude_session_ids(&self) -> std::collections::HashSet<String> {
        self.0
            .store
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| s.claude_session_id)
            .collect()
    }
}
