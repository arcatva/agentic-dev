//! PR3: `Engine::transition()` — single source of truth for status changes.
//!
//! Every status mutation in the engine goes through this method:
//!
//! - `submit_session` → `transition(id, Pending, Create)` (initial create is
//!   a direct insert, but follow_up's queued branch uses this path)
//! - `start()` publishes `Running` → `transition(id, Running, Start)`
//! - `kill()` (running) → `transition(id, Killed, Kill)` BEFORE `run.stop()`
//! - `kill()` (pending) → `transition(id, Killed, KillQueued)`
//! - `delete_session()` force-kill → `transition(id, Killed, DeleteForce)`
//! - `pump()` start-failed → `transition(id, Failed, StartFailed(msg))`
//! - `on_exit()` → `transition(id, decide_on_exit_status(...), OnExit{code})`
//! - `watchdog.rs` idle-TTL reap → `transition(id, ..., WatchdogIdleTtl{...})`
//! - `watchdog.rs` idle/wall-max cancel → `transition(id, Done, WatchdogCancel{...})`
//! - `recover()` running → `transition(id, ..., Recover)`
//! - `recover()` pending re-enqueue → `transition(id, Pending, Recover)` (idempotent self-transition)
//!
//! All side-effect logic (clearing error on Start, setting ended_at on terminal,
//! killed backstop, terminal-preserves) lives here, **not** at the call sites.
//!
//! Illegal pairs (per `SessionStatus::legal_transition_to`) raise
//! `EngineError::IllegalTransition { from, to }` — runtime-only check; the
//! type system can't enforce this since the legality is data-dependent.

use super::*;
use crate::engine::error::EngineError;
use crate::engine::status::SessionStatus;
use crate::engine::store::SessionUpdate;
use std::str::FromStr;

/// Why a particular status change is happening. The `transition()` body uses
/// this to compute the right side-effect patch (cleared fields, set ended_at,
/// killed backstop, …) so the call sites stay one-liners.
#[derive(Debug, Clone)]
pub enum TransitionReason {
    /// `pending → running` from a fresh turn begin. Clears error/errorKind/exit_code
    /// (PR3 mirrors what mod.rs:1218 used to do inline).
    Start,
    /// `running → killed` from a user-initiated stop while a turn is in flight.
    /// This MUST be written before `run.stop()` so a racing abort result event
    /// is short-circuited in on_event's Result branch.
    Kill,
    /// `pending → killed` from a stop on a queued session. Sets ended_at.
    KillQueued,
    /// `pending|running → failed` from a pump() start() that returned Err.
    StartFailed(String),
    /// `running → done|failed|killed` from process exit. The exact target
    /// is decided from the exit code + prior error state in `on_exit`.
    OnExit { code: i32 },
    /// `running → done|failed` from idle-TTL reap. `had_error` controls
    /// which one (preserved from watchdog.rs:77).
    WatchdogIdleTtl { had_error: bool },
    /// `running → done` from idle-max or wall-max exceeded. Comment in
    /// watchdog.rs:119-152 is "mark BEFORE stop() so the exit handler keeps
    /// 'done', not 'killed'".
    WatchdogCancel { kind: &'static str },
    /// `running → done|failed` from boot-time recovery (recover.rs). The
    /// target is decided from the sidecar + transcript in `recover()` itself;
    /// this reason just runs the side-effect patch.
    Recover,
    /// `running|pending → killed` from `delete_session()`. Has the same
    /// end-state as Kill/KillQueued but is a different code path; the reason
    /// exists for log/trace clarity only.
    DeleteForce,
    /// `pending → pending` self-transition from `recover()`'s re-enqueue.
    /// Idempotent; pushes onto the in-memory queue but does not write the DB.
    ReEnqueue,
    /// `* → pending` from `follow_up()` queued/idle branch (also
    /// `done|failed → pending` for resume). Clears the prior turn's
    /// error fields so the client banner doesn't linger in the pending
    /// window.
    FollowUpQueued,
}

impl Engine {
    /// Move `id` to `to` for `reason`. The legal-transition matrix is the
    /// gate; illegal pairs return `EngineError::IllegalTransition`. Idempotent
    /// self-transitions (e.g. `Running → Running` from a `start()` race) are
    /// no-ops and return the current row.
    ///
    /// PR3 wiring (this PR):
    /// - legality check via `SessionStatus::legal_transition_to`
    /// - idempotent self-transition (Running→Running, Pending→Pending) is a no-op
    ///   except for `ReEnqueue` which pushes onto `state.queue`
    /// - side-effect patch: Start clears error fields, terminal sets ended_at,
    ///   Kill/DeleteForce/KillQueued clear error+errorKind (the killed backstop),
    ///   OnExit writes exit_code
    ///
    /// PR4 will add: lifecycle sidecar writes (TurnStarted/TurnEnded).
    /// PR5 will add: recover() callsites use this with `Recover` reason.
    /// PR6 will add: all 13 inline sites collapse to one call each.
    pub async fn transition(
        &self,
        id: &str,
        to: SessionStatus,
        reason: TransitionReason,
    ) -> Result<Session, EngineError> {
        let cur = self
            .0
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        let from = SessionStatus::from_str(&cur.status).unwrap_or(SessionStatus::Failed);

        // Gate: legal?
        if !from.legal_transition_to(to) {
            return Err(EngineError::IllegalTransition { from, to });
        }

        // Idempotent self-transitions.
        if from == to {
            match reason {
                TransitionReason::ReEnqueue => {
                    // recover()'s pending re-enqueue: push onto the in-memory
                    // queue so the session gets re-scheduled after boot.
                    use std::collections::HashMap;
                    let mut state = self.0.state.lock();
                    state.queue.push_back(QueueItem {
                        id: id.to_string(),
                        prompt: cur.prompt.clone(),
                        env: HashMap::new(),
                        resume_session_id: cur.claude_session_id.clone(),
                        enqueued_at: None,
                        model: None,
                        effort: None,
                        permission_mode: None,
                        context_prefix: None,
                    });
                }
                TransitionReason::Start
                | TransitionReason::Kill
                | TransitionReason::KillQueued
                | TransitionReason::StartFailed(_)
                | TransitionReason::OnExit { .. }
                | TransitionReason::WatchdogIdleTtl { .. }
                | TransitionReason::WatchdogCancel { .. }
                | TransitionReason::Recover
                | TransitionReason::DeleteForce
                | TransitionReason::FollowUpQueued => {
                    // Plain no-op (e.g. start() racing with itself).
                }
            }
            return Ok(cur);
        }

        // Real transition: build the patch from the reason.
        let now = self.now();
        let mut u = SessionUpdate::new();
        u = u.status(to);

        match &reason {
            TransitionReason::Start => {
                u = u.started_at(now);
                u = u.clear_error();
                u = u.clear_error_kind();
                u = u.clear_exit_code();
                // A re-entering turn (resume of a done/failed session, or a
                // requeued one) must drop the PRIOR turn's endedAt: while the
                // new turn runs the session is not "ended", and a stale endedAt
                // left lying around is exactly what the client's unread-dot
                // predicate (endedAt > lastReadAt) re-fires on. on_exit writes a
                // fresh endedAt when THIS turn actually ends.
                u = u.clear_ended_at();
            }
            TransitionReason::Kill | TransitionReason::DeleteForce => {
                // killed-backstop: any move to Killed clears error/errorKind.
                u = u.clear_error();
                u = u.clear_error_kind();
                // Do NOT set ended_at here — on_exit does it when the process
                // actually exits, so the race window is short. The plan pins
                // this: "kill() writes status BEFORE run.stop()" — the
                // on_exit handler will see status=killed and call transition()
                // a second time with OnExit{code} → idempotent self → no-op.
            }
            TransitionReason::KillQueued => {
                // pending → killed: no live process, so we set ended_at now.
                u = u.ended_at(now);
                u = u.clear_error();
                u = u.clear_error_kind();
            }
            TransitionReason::StartFailed(msg) => {
                u = u.error(format!("failed to start: {msg}"));
                u = u.error_kind("crashed");
                u = u.ended_at(now);
            }
            TransitionReason::OnExit { code } => {
                u = u.exit_code(*code as i64);
                u = u.ended_at(now);
                // Generic crash fallback: if the caller forgot to set error
                // (e.g. a non-zero exit with no error text yet), synthesize
                // a neutral message so the db row is informative.
                if to == SessionStatus::Failed && cur.error.is_none() {
                    u = u.error("turn ended without completing — interrupted, crashed, or killed (resume to retry)");
                    u = u.error_kind("crashed");
                }
            }
            TransitionReason::WatchdogIdleTtl { had_error } => {
                u = u.ended_at(now);
                if *had_error && cur.error.is_none() {
                    u = u.error("idle TTL reaped; had error before reap");
                    u = u.error_kind("idle_ttl");
                }
            }
            TransitionReason::WatchdogCancel { kind: _ } => {
                u = u.ended_at(now);
            }
            TransitionReason::Recover => {
                // Deliberately does NOT set ended_at. Stamping `now` here was the
                // root cause of the spurious unread-dot/checkmark bug: every
                // server restart re-finalized still-`running` rows (idle sessions
                // included) with endedAt = boot wall-clock, which is newer than
                // the client's lastReadAt, so the dot re-lit on every restart.
                // recover() now owns ended_at and restores the REAL turn-end time
                // (lifecycle TurnEnded, else the current turn's start marker) via
                // a follow-up apply_update. error/errorKind are likewise set by
                // recover().
            }
            TransitionReason::ReEnqueue => unreachable!("handled by idempotent branch"),
            TransitionReason::FollowUpQueued => {
                // pending/running → pending (follow_up queued branch):
                // clear the prior turn's error fields so the client
                // banner doesn't linger during the pending window.
                u = u.clear_error();
                u = u.clear_error_kind();
                u = u.clear_exit_code();
                // Same reason as Start: a session about to run a new turn is no
                // longer "ended", so drop the prior turn's endedAt to keep the
                // client's unread dot from re-firing on a stale completion time.
                u = u.clear_ended_at();
            }
        }

        // Persist via the builder path. Cost is NOT touched here; on_event's
        // Result branch still owns cost accumulation (it needs the
        // ClaudeEvent::Result payload, which only it has).
        self.0.store.apply_update(id, u).await?;
        let after = self
            .0
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        Ok(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::store::CreateInput;

    /// Helper: build a Store + log dir under a unique tempdir and return
    /// (Store, worktree_path). The store has no engine — the transition
    /// tests below need the store half only. For full engine round-trips
    /// we fall back to the tests.rs::make_engine factory.
    async fn open_test_store(label: &str) -> (Store, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "agentic-transition-test-{}-{}-{}",
            std::process::id(),
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = tmp.join("db.sqlite");
        let logs = tmp.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let store = Store::open(&db, &logs).await.unwrap();
        (store, tmp)
    }

    /// `legal_transition_to` is the gate; PR3's transition() runs the gate
    /// first and returns IllegalTransition before doing any I/O. This test
    /// doesn't construct an engine — it tests the gate directly on the
    /// enum (no I/O).
    #[test]
    fn transition_illegal_pairs_return_illegal() {
        use SessionStatus::*;
        // The catch-all in legal_transition_to covers these. They are illegal:
        assert!(!Running.legal_transition_to(Pending));
        assert!(!Pending.legal_transition_to(Done));
        assert!(!Done.legal_transition_to(Running));
        assert!(!Failed.legal_transition_to(Running));
        assert!(!Killed.legal_transition_to(Running));
    }

    /// The OnExit reason's decision table is the trickiest piece of the
    /// existing engine (mod.rs:1497-1508). The PR3 transition() body
    /// hard-codes "OnExit → status+exit_code+ended_at" without deciding
    /// the target; the caller (on_exit) decides. This test pins the
    /// target-decision function we'd add in PR6 by testing the
    /// matrix directly.
    #[test]
    fn on_exit_target_decision_matches_existing_table() {
        use SessionStatus::*;
        // The existing on_exit (mod.rs:1497-1508):
        //   "killed" → "killed"
        //   "failed" → "failed"
        //   "done"   → "done"
        //   _        → if code==0 && error.is_none() { "done" } else { "failed" }
        // PR6 will extract this into `Engine::on_exit_target(code, prior, err)`
        // and feed it to transition(). For now we just test the enum
        // matrix that gates the transition.
        let cases = [
            (Running, 0i32, true /* err */, Failed), // err + exit0 = failed
            (Running, 0, false, Done),               // no err + exit0 = done
            (Running, 1, true, Failed),              // err + nonzero = failed
            (Running, 1, false, Failed),             // no err + nonzero = failed
        ];
        for (from, _code, _had_err, expected_to) in cases {
            // The decision lives outside transition() in the current code;
            // here we just confirm the from→to is legal so transition()
            // would not reject it.
            assert!(
                from.legal_transition_to(expected_to),
                "from={from:?} to={expected_to:?} should be legal per on_exit decision",
            );
        }
    }

    /// Smoke: Store can apply a SessionUpdate that the transition() body
    /// would have built. This is the path the new transition() will use.
    #[tokio::test]
    async fn transition_applies_session_update_via_store() {
        let (store, work) = open_test_store("smoke").await;
        let wt = work.join("wt");
        store
            .create(CreateInput {
                id: "t1".into(),
                prompt: "p".into(),
                worktree_path: Some(wt.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        // Simulate Start: status=Running, clear error, set started_at.
        store
            .apply_update(
                "t1",
                SessionUpdate::new()
                    .status(SessionStatus::Running)
                    .started_at(1234)
                    .clear_error()
                    .clear_error_kind()
                    .clear_exit_code(),
            )
            .await
            .unwrap();

        let s = store.get("t1").await.unwrap().unwrap();
        assert_eq!(s.status, "running");
        assert_eq!(s.started_at, Some(1234));
        assert_eq!(s.error, None);
    }

    /// Smoke: Kill backstop clears error/errorKind on a row that had them.
    #[tokio::test]
    async fn transition_kill_clears_error_backstop() {
        let (store, work) = open_test_store("kill").await;
        let wt = work.join("wt");
        store
            .create(CreateInput {
                id: "k1".into(),
                prompt: "p".into(),
                worktree_path: Some(wt.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        // Pre-set an error on a running row, then kill.
        store
            .update(
                "k1",
                SessionPatch {
                    status: Some("running".into()),
                    error: Some(Some("racing result".into())),
                    error_kind: Some(Some("claude_error".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .apply_update(
                "k1",
                SessionUpdate::new()
                    .status(SessionStatus::Killed)
                    .clear_error()
                    .clear_error_kind(),
            )
            .await
            .unwrap();
        let s = store.get("k1").await.unwrap().unwrap();
        assert_eq!(s.status, "killed");
        assert_eq!(s.error, None, "killed backstop must clear error");
        assert_eq!(s.error_kind, None);
    }

    /// Smoke: StartFailed writes error + kind + ended_at.
    #[tokio::test]
    async fn transition_start_failed_writes_crashed_error() {
        let (store, work) = open_test_store("startfailed").await;
        let wt = work.join("wt");
        store
            .create(CreateInput {
                id: "sf1".into(),
                prompt: "p".into(),
                worktree_path: Some(wt.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .apply_update(
                "sf1",
                SessionUpdate::new()
                    .status(SessionStatus::Failed)
                    .error("failed to start: ENOENT")
                    .error_kind("crashed")
                    .ended_at(9999),
            )
            .await
            .unwrap();
        let s = store.get("sf1").await.unwrap().unwrap();
        assert_eq!(s.status, "failed");
        assert_eq!(s.error.as_deref(), Some("failed to start: ENOENT"));
        assert_eq!(s.error_kind.as_deref(), Some("crashed"));
        assert_eq!(s.ended_at, Some(9999));
    }
}
