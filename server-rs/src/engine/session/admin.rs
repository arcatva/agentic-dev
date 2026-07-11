use std::path::PathBuf;

// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
use crate::engine::status::SessionStatus;
use crate::engine::store::{Session, SessionPatch};
use crate::engine::transition::TransitionReason;

use crate::engine::*;

impl Engine {
    // ── Task 8: discard / delete_session ─────────────────────

    /// Return the session if it exists, busy-check, and live-worktree-check.
    /// Used by discard().
    async fn live_session(&self, id: &str) -> Result<Session, EngineError> {
        let s = self
            .0
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        if self.is_busy(id, &s.status) {
            return Err(EngineError::Busy);
        }
        if s.worktree_state != "live" {
            return Err(EngineError::WorktreeCleaned);
        }
        Ok(s)
    }

    /// True when `path` lives INSIDE the managed worktrees root (`cfg.worktrees_root`).
    ///
    /// DATA-LOSS guard for adopt-in-place: an adopted session's `worktree_path` is the user's
    /// REAL project cwd, which is OUTSIDE `worktrees_root`. Every `remove_dir_all` on a session's
    /// worktree dir must first pass this check so `delete`/`discard` never blow away the user's
    /// actual project directory — they may only remove the managed `<worktrees_root>/<id>` dirs.
    /// Canonicalizes both sides where possible (resolving symlinks/`..`); when a path can't be
    /// canonicalized (e.g. already gone) it falls back to raw-prefix comparison against both the
    /// canonicalized and raw root, so a genuinely-managed dir is never mis-skipped.
    fn within_worktrees_root(&self, path: &std::path::Path) -> bool {
        crate::engine::native_transcript::path_within(path, &self.0.cfg.worktrees_root)
    }

    /// Discard the worktree for a done/idle session.
    pub async fn discard(&self, id: &str) -> Result<(), EngineError> {
        let s = self.live_session(id).await?;
        let wt_path = match s.worktree_path.as_ref() {
            Some(p) => PathBuf::from(p),
            None => return Err(EngineError::BadInput("session has no worktree path".into())),
        };
        let branch = s.branch.as_deref().unwrap_or("");
        // For each repo: call discard_worktree(src_root/repo, wt_path/repo, &branch)
        for repo in &s.repos {
            let repo_path = self.0.cfg.src_root.join(repo);
            let repo_wt = wt_path.join(repo);
            // Best-effort: ignore errors per individual repo
            if let Err(e) = crate::engine::worktree::discard_worktree(&repo_path, &repo_wt, branch)
            {
                tracing::warn!("[engine] discard_worktree {repo} failed: {e}");
            }
        }
        // Remove the session worktree dir itself — but ONLY if it is inside the managed
        // worktrees root. An adopt-in-place session's worktree_path is the user's real project
        // cwd (outside the root); removing it would destroy the user's actual directory.
        if self.within_worktrees_root(&wt_path) {
            if let Err(e) = std::fs::remove_dir_all(&wt_path) {
                tracing::warn!("[engine] remove worktree dir failed: {e}");
            }
        } else {
            tracing::warn!(
                "[engine] discard: skipping remove_dir_all of {} — outside worktrees_root (adopt-in-place cwd)",
                wt_path.display()
            );
        }
        // Drop any per-turn rewind snapshot refs for this session (best-effort).
        for repo in &s.repos {
            crate::engine::worktree::delete_snapshot_refs(&self.0.cfg.src_root.join(repo), id);
        }
        // Mark as discarded in the store
        self.0
            .store
            .update(
                id,
                SessionPatch {
                    worktree_state: Some("discarded".into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// Best-effort: snapshot every repo's working tree before a turn, labeled with `turn_index`, so
    /// the user can later rewind to it. Failures are logged, never propagated — a snapshot problem
    /// must never break the turn that triggered it.
    pub(crate) fn snapshot_worktrees(&self, s: &Session, turn_index: usize) {
        let Some(wt_root) = s.worktree_path.as_deref() else {
            return;
        };
        for repo in &s.repos {
            let wt = std::path::Path::new(wt_root).join(repo);
            let snapshot_ref = format!("refs/agentic/snapshots/{}/{}", s.id, turn_index);
            if let Err(e) = crate::engine::worktree::snapshot_worktree(&wt, &snapshot_ref) {
                tracing::warn!("[engine] snapshot {repo}@turn{turn_index} failed: {e}");
            }
        }
    }

    /// Rewind the working tree to the snapshot taken just before `turn_index` ran — restoring tracked
    /// files (and recreating tracked deletions) while KEEPING untracked files created since (no git
    /// clean). `turn_index == 0` restores the per-repo base SHA (state before the first prompt). The
    /// branch/HEAD is not moved; chat history is unchanged (code-only rewind). Requires an idle
    /// session (a live turn is mid-write — restoring under it would corrupt its work).
    pub async fn rewind(&self, id: &str, turn_index: usize) -> Result<(), EngineError> {
        let s = self.live_session(id).await?;
        let wt_root = s.worktree_path.clone().ok_or(EngineError::NoWorktree)?;
        // Phase 1: resolve + verify EVERY repo's restore target before touching anything, so a
        // validation failure on repo N cannot leave repos 1..N-1 already restored (a partially
        // rewound multi-repo workspace).
        let mut plan: Vec<(std::path::PathBuf, String, String)> = Vec::with_capacity(s.repos.len());
        for repo in &s.repos {
            let wt = std::path::Path::new(&wt_root).join(repo);
            let target = if turn_index == 0 {
                s.base_shas.get(repo).cloned().flatten().ok_or_else(|| {
                    EngineError::BadInput(format!("no base snapshot for repo {repo}"))
                })?
            } else {
                let snapshot_ref = format!("refs/agentic/snapshots/{id}/{turn_index}");
                let wt_str = wt.to_string_lossy();
                // Verify the snapshot exists so the caller gets a clean 400, not a raw git error.
                if crate::engine::worktree::git_sync(&[
                    "-C",
                    &wt_str,
                    "rev-parse",
                    "--verify",
                    &format!("{snapshot_ref}^{{commit}}"),
                ])
                .is_err()
                {
                    return Err(EngineError::BadInput(format!(
                        "no snapshot for turn {turn_index} in repo {repo}"
                    )));
                }
                snapshot_ref
            };
            plan.push((wt, target, repo.clone()));
        }
        // Phase 2: all targets validated — restore. (A restore *failure* can still stop midway;
        // what this removes is partial state from a mere validation error.)
        for (wt, target, repo) in &plan {
            crate::engine::worktree::restore_worktree(wt, target)
                .map_err(|e| EngineError::Internal(format!("rewind restore {repo} failed: {e}")))?;
        }
        Ok(())
    }

    /// Wait until the pump task for `id` has finished (i.e., on_exit removed it from running).
    /// Polls every 10ms up to 5s for the pump task to complete.
    pub(crate) async fn wait_for_exit(&self, id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let state = self.0.state.lock();
                if !state.running.contains_key(id) {
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return; // timed out — best effort
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Delete a session. If force=true, stops any running turn first.
    pub async fn delete_session(&self, id: &str, force: bool) -> Result<(), EngineError> {
        // Get the session — if missing, return Ok (already gone).
        let s = match self.0.store.get(id).await? {
            Some(s) => s,
            None => return Ok(()),
        };
        let busy = self.is_busy(id, &s.status);
        if busy && !force {
            return Err(EngineError::Busy);
        }
        if busy {
            // Force: kill and wait.
            // Drop from queue first.
            {
                let mut state = self.0.state.lock();
                state.queue.retain(|q| q.id != id);
            }
            let run_handle = {
                let state = self.0.state.lock();
                state.running.get(id).map(|r| r.run.clone())
            };
            if let Some(run) = run_handle {
                // PR6: route through transition() with DeleteForce
                // reason (same effect as Kill but distinct for tracing).
                if let Err(e) = self
                    .transition(id, SessionStatus::Killed, TransitionReason::DeleteForce)
                    .await
                {
                    tracing::error!("[engine] transition delete kill-running failed: {e}");
                }
                run.stop();
                // Wait for on_exit to run (removes from running map).
                self.wait_for_exit(id).await;
            } else {
                // Was in starting or just queued (already removed) — mark killed+ended.
                // PR6: route through transition() with DeleteForce
                // reason. transition()'s DeleteForce branch (like KillQueued)
                // sets ended_at = now.
                if let Err(e) = self
                    .transition(id, SessionStatus::Killed, TransitionReason::DeleteForce)
                    .await
                {
                    tracing::error!("[engine] transition delete kill-queued failed: {e}");
                }
            }
        }
        // Re-read current session to get latest worktree_state after potential kill.
        let cur = match self.0.store.get(id).await? {
            Some(c) => c,
            None => return Ok(()), // deleted by on_exit race — fine
        };
        // Discard worktrees if still live.
        if cur.worktree_state == "live" {
            if let Some(ref wt_path_str) = cur.worktree_path {
                let wt_path = PathBuf::from(wt_path_str);
                let branch = cur.branch.as_deref().unwrap_or("");
                for repo in &cur.repos {
                    let repo_path = self.0.cfg.src_root.join(repo);
                    let repo_wt = wt_path.join(repo);
                    if let Err(e) =
                        crate::engine::worktree::discard_worktree(&repo_path, &repo_wt, branch)
                    {
                        tracing::warn!("[engine] delete discard_worktree {repo} failed: {e}");
                    }
                }
                // Remove the whole session dir (worktrees + any per-session scratch). Credentials
                // and transcripts live in the shared ~/.claude, so there is no per-session config
                // dir to clean up here anymore. Guard with within_worktrees_root: an adopt-in-place
                // session's worktree_path is the user's real project cwd (outside the root) —
                // remove_dir_all'ing it would destroy the user's actual directory (DATA LOSS).
                if self.within_worktrees_root(&wt_path) {
                    if let Err(e) = std::fs::remove_dir_all(&wt_path) {
                        tracing::warn!("[engine] remove session dir failed: {e}");
                    }
                } else {
                    tracing::warn!(
                        "[engine] delete: skipping remove_dir_all of {} — outside worktrees_root (adopt-in-place cwd)",
                        wt_path.display()
                    );
                }
            }
        }
        // Remove the store row.
        self.0.store.remove(id).await?;
        // Clean up all runtime state including subs.
        self.forget_session(id);
        {
            let mut state = self.0.state.lock();
            state.subs.remove(id);
        }
        // Drop cached transcript projection if we have a TranscriptCache.
        if let Some(ref tc) = self.0.transcript {
            tc.drop_session(id);
        }
        Ok(())
    }
}
