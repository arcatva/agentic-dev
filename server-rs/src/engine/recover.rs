//! PR5: rewritten boot-time recovery using the sidecar lifecycle log.
//!
//! The old `recover()` reverse-scanned the transcript and trusted the
//! last `Result` event's `is_error` flag — it couldn't tell a 429 from
//! 2 hours ago from a 429 that just landed. That's the bug that marked
//! 5 healthy sessions (5f20c824, bdf701f0, 5d0c769f, 6072af9b,
//! 04c7e2ea) as `failed, errorKind=rate_limited` on 2026-06-23 when the
//! API was actually healthy.
//!
//! The new algorithm uses the sidecar's `last_turn_started_at` as a
//! wall-clock anchor and calls `last_outcome_in_current_turn` to bound
//! the transcript scan to "results from the current turn only".
//!
//! Backward compat: when the sidecar is missing (sessions written
//! before PR4), `last_outcome_in_current_turn(transcript, None)` falls
//! back to "any result counts" — exactly the old `last_result()`
//! behavior. All 5 existing `recover_*` tests pass unchanged because
//! they don't write a sidecar.

use super::*;
use crate::engine::classify_error::classify_claude_error;
use crate::engine::lifecycle::{
    last_outcome_in_current_turn, last_prompt_at, last_turn_ended_at, last_turn_started_at,
    CurrentTurnOutcome, LifecycleEvent,
};
use crate::engine::status::SessionStatus;
use crate::engine::store::SessionUpdate;
use crate::engine::transition::TransitionReason;
use std::str::FromStr;

impl Engine {
    /// Finalize sessions left running/pending at shutdown.
    pub async fn recover(&self) {
        let now = self.now();
        let sessions = match self.0.store.list().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("[engine] recover: store.list failed — skipping recovery: {e}");
                return;
            }
        };
        for s in sessions {
            let status = SessionStatus::from_str(&s.status).unwrap_or(SessionStatus::Failed);
            match status {
                SessionStatus::Running => {
                    let events = self.0.store.read_lifecycle(&s.id).await;
                    let turn_start = last_turn_started_at(&events);
                    let transcript = self.0.store.read_log(&s.id);
                    let outcome = last_outcome_in_current_turn(&transcript, turn_start);
                    let reason_str = match &outcome {
                        CurrentTurnOutcome::Success => "Success",
                        CurrentTurnOutcome::Error(_) => "Error",
                        CurrentTurnOutcome::None => "None(interrupted)",
                    };
                    let (target, error_text, error_kind): (
                        SessionStatus,
                        Option<String>,
                        Option<String>,
                    ) = match &outcome {
                        CurrentTurnOutcome::Success => (SessionStatus::Done, None, None),
                        CurrentTurnOutcome::Error(text) => {
                            let truncated = truncate_chars(text, 500);
                            let kind = classify_claude_error(truncated).to_string();
                            (
                                SessionStatus::Failed,
                                Some(truncated.to_string()),
                                Some(kind),
                            )
                        }
                        CurrentTurnOutcome::None => (
                            SessionStatus::Failed,
                            Some("interrupted by server restart".to_string()),
                            Some("interrupted".to_string()),
                        ),
                    };
                    // The REAL turn-end time, restored into endedAt so a restart
                    // never advances it past the client's lastReadAt (the bug:
                    // transition(Recover) used to stamp `now`). Priority:
                    //   1. lifecycle TurnEnded.at  — authoritative; written when
                    //      the turn actually finished (process parked as awaiting).
                    //   2. last agentic_prompt.at  — the current turn's START;
                    //      legacy fallback for sessions with no TurnEnded sidecar.
                    //      Always <= the user's read time, so it never lights a
                    //      spurious dot for an already-read session.
                    //   3. now()                   — only when the transcript has
                    //      no timestamp at all (genuinely interrupted/empty). Such
                    //      sessions recover to Failed, which is never an unread dot.
                    let real_end = last_turn_ended_at(&events)
                        .or_else(|| last_prompt_at(&transcript))
                        .unwrap_or(now);
                    let _ = self
                        .transition(&s.id, target, TransitionReason::Recover)
                        .await
                        .map_err(|e| {
                            tracing::warn!("[engine] recover transition failed for {}: {e}", s.id)
                        });
                    let mut patch = SessionUpdate::new().ended_at(real_end);
                    if let Some(text) = error_text {
                        patch = patch
                            .error(text)
                            .error_kind(error_kind.unwrap_or_else(|| "claude_error".into()));
                    }
                    let _ = self.0.store.apply_update(&s.id, patch).await.map_err(|e| {
                        tracing::warn!("[engine] recover end/error patch failed for {}: {e}", s.id)
                    });
                    let _ = self
                        .0
                        .store
                        .append_lifecycle(
                            &s.id,
                            &LifecycleEvent::BootRecovered {
                                at: now,
                                decided: target,
                                reason: reason_str.to_string(),
                            },
                        )
                        .await
                        .map_err(|e| {
                            tracing::warn!(
                                "[engine] recover lifecycle append failed for {}: {e}",
                                s.id
                            )
                        });
                    tracing::info!(
                        "[engine] recover: {} {} (turn_start={:?}, reason={})",
                        s.id,
                        target.as_str(),
                        turn_start,
                        reason_str,
                    );
                }
                SessionStatus::Pending => {
                    let _ = self
                        .transition(&s.id, SessionStatus::Pending, TransitionReason::ReEnqueue)
                        .await
                        .map_err(|e| {
                            tracing::warn!("[engine] recover re-enqueue failed for {}: {e}", s.id)
                        });
                }
                _ => {} // terminals: nothing to do
            }
        }
        self.defer_pump();
    }

    pub async fn reconcile_worktrees(&self) {
        let root = &self.0.cfg.worktrees_root;
        let dir_iter = match std::fs::read_dir(root) {
            Ok(it) => it,
            Err(_) => return,
        };
        let known_ids: std::collections::HashSet<String> = self
            .0
            .store
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.id)
            .collect();
        if known_ids.is_empty() {
            return;
        }
        for entry in dir_iter.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known_ids.contains(&name) {
                if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                    tracing::warn!("[engine] remove orphan dir failed: {e}");
                }
            }
        }
    }

    /// Boot-time recovery for `delegate` fan-outs interrupted by a restart. `recover()` finalizes
    /// sessions and `reconcile_worktrees()` cleans orphan worktree dirs, but neither touches the
    /// per-session delegate journals — so a fan-out cut off mid-run leaves a
    /// `subagents/workflows/<run>/` dir with no completion summary, which the workflow reader shows
    /// as "running" forever. This walks every session, resolves its journal dir, and writes a
    /// terminal summary for each such orphaned run. Like `recover()`, it is safe ONLY at boot, when
    /// no fan-out is in flight (a summary-less live dir then unambiguously means "interrupted").
    pub async fn reconcile_delegate_runs(&self) {
        let sessions = match self.0.store.list().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    "[engine] reconcile_delegate_runs: store.list failed — skipping: {e}"
                );
                return;
            }
        };
        let now = self.now();
        let mut total = 0usize;
        for s in sessions {
            let Some(csid) = s.claude_session_id.as_deref().filter(|c| !c.is_empty()) else {
                continue;
            };
            let Some(journal_dir) =
                crate::engine::delegate::resolve_journal_dir(&self.0.cfg.claude_config_base, csid)
            else {
                continue;
            };
            total += crate::engine::delegate::finalize_orphaned_runs(&journal_dir, now);
        }
        if total > 0 {
            tracing::info!(
                "[engine] reconcile_delegate_runs: finalized {total} interrupted delegate run(s)"
            );
        }
    }
}
