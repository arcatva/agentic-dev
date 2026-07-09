//! Engine: the watchdog reaper — idle/wall-time timeouts + opt-in idle-TTL reap of parked turns.

use super::*;
use crate::engine::status::SessionStatus;

impl Engine {
    // ── Watchdog ─────────────────────────────────────────────

    pub(super) fn start_watchdog(&self) {
        let inner = self.0.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_millis(WATCHDOG_TICK_MS),
            );
            interval.tick().await; // skip the first immediate tick
            loop {
                interval.tick().await;
                if inner.state.lock().closed {
                    break;
                }
                Engine(inner.clone()).tick_watchdog().await;
                // Auto-resume scheduler rides the same cadence: schedules a resume time for
                // usage-limit-blocked sessions and re-sends the turn once the limit resets.
                Engine(inner.clone()).tick_auto_resume().await;
            }
        });
        *self.0.watchdog.lock() = Some(handle);
    }

    /// Trigger a watchdog tick (for tests).
    pub async fn trigger_watchdog(&self) {
        self.tick_watchdog().await;
    }

    /// Watchdog tick body — reaps sessions that exceeded idle/wall time limits.
    pub(crate) async fn tick_watchdog(&self) {
        let closed = self.0.state.lock().closed;
        if closed {
            return;
        }

        let now = self.now();
        let idle_max = self.0.cfg.idle_max_ms.unwrap_or(DEFAULT_IDLE_MAX_MS as i64);
        // Wall time is OPT-IN: None → no total-runtime cap (unlimited by default). Only enforced
        // when AGENTIC_TURN_WALL_SEC is set.
        let wall_max_ms = self.0.cfg.wall_max_ms;
        let idle_ttl = self.0.cfg.idle_ttl_ms;

        // Collect session ids in running to avoid holding the lock while doing async work.
        let ids: Vec<String> = {
            let state = self.0.state.lock();
            state.running.keys().cloned().collect()
        };

        for id in ids {
            // Idle-TTL reap (opt-in): reap parked sessions that have been idle too long.
            if let Some(ttl_ms) = idle_ttl {
                let (is_awaiting, no_pending_ask, no_pending_delegate, last_ev, run_handle) = {
                    let state = self.0.state.lock();
                    if !state.running.contains_key(&id) {
                        continue; // already reaped in an earlier iteration
                    }
                    let is_awaiting = state.awaiting.get(&id) == Some(&true);
                    let no_pending_ask = !state.pending_ask.contains(&id);
                    let no_pending_delegate = !state.pending_delegate.contains(&id);
                    let last_ev = state.last_event_at.get(&id).copied().unwrap_or(now);
                    let run_handle = state.running.get(&id).map(|r| r.run.clone());
                    (is_awaiting, no_pending_ask, no_pending_delegate, last_ev, run_handle)
                };

                if is_awaiting && no_pending_ask && no_pending_delegate && (now - last_ev) > ttl_ms {
                    // Reap: set status done (or failed if there's already an error).
                    let store = self.0.store.clone();
                    let id2 = id.clone();
                    let _now2 = now;
                    // Read current session to check for error.
                    let final_status = {
                        store.get(&id2).await
                        .ok()
                        .flatten()
                        .map(|s| if s.error.is_some() { "failed" } else { "done" })
                        .unwrap_or("done")
                        .to_string()
                    };

                    // PR6: route through transition() with WatchdogIdleTtl
                    // reason. transition() owns status + ended_at; the
                    // had_error flag drives the target (done|failed).
                    let _ = crate::engine::Engine(self.0.clone())
                        .transition(
                            &id2,
                            std::str::FromStr::from_str(&final_status).unwrap_or(SessionStatus::Done),
                            TransitionReason::WatchdogIdleTtl { had_error: final_status == "failed" },
                        )
                        .await
                        .map_err(|e| tracing::error!("[engine] transition watchdog idle-ttl: {e}"));

                    if let Some(run) = run_handle {
                        run.stop();
                    }
                    continue;
                }
            }

            // Check parked state and compute idle/wall times.
            let (parked, idle_ms, wall_ms, run_handle) = {
                let state = self.0.state.lock();
                if !state.running.contains_key(&id) {
                    continue;
                }
                let is_awaiting = state.awaiting.get(&id) == Some(&true);
                let has_pending_ask = state.pending_ask.contains(&id);
                let has_pending_delegate = state.pending_delegate.contains(&id);
                let parked = is_awaiting || has_pending_ask || has_pending_delegate;
                let (idle_ms, wall_ms) = if parked {
                    (0i64, 0i64)
                } else {
                    let last_ev = state.last_event_at.get(&id).copied().unwrap_or(now);
                    let turn_start = state.turn_started_at.get(&id).copied().unwrap_or(now);
                    (now - last_ev, now - turn_start)
                };
                let run_handle = state.running.get(&id).map(|r| r.run.clone());
                (parked, idle_ms, wall_ms, run_handle)
            };

            let _ = parked; // used implicitly: idle_ms/wall_ms are 0 when parked

            let wall_exceeded = wall_max_ms.map_or(false, |w| wall_ms > w);
            if idle_ms > idle_max || wall_exceeded {
                // GRACEFUL CANCEL (not a failure): a turn idle (no output) too long — or, if a wall
                // cap is explicitly configured, running too long — is stopped and marked `done`.
                // The session stays fully resumable: the user can send another message to continue
                // the conversation. We log the reason for observability but do NOT surface it as an
                // error (no error/error_kind), so the app shows it as a normal ended turn.
                let reason = if idle_ms > idle_max {
                    let idle_s = ((idle_ms as f64) / 1000.0).round() as i64;
                    let cap_s = ((idle_max as f64) / 1000.0).round() as i64;
                    format!("idle for {idle_s}s (cap {cap_s}s)")
                } else {
                    let wall_s = ((wall_ms as f64) / 1000.0).round() as i64;
                    let cap_s = wall_max_ms.unwrap_or(0) / 1000;
                    format!("wall time {wall_s}s exceeded the {cap_s}s cap")
                };
                tracing::info!("[engine] watchdog gracefully cancelling turn {id} ({reason}) — marked done, resumable");

                // Mark BEFORE stop() so the exit handler keeps "done", not "killed".
                let _store = self.0.store.clone();
                let id2 = id.clone();
                let _now2 = now;
                if let Err(e) = crate::engine::Engine(self.0.clone())
                    .transition(
                        &id2,
                        SessionStatus::Done,
                        TransitionReason::WatchdogCancel { kind: "idle_or_wall_max" },
                    )
                    .await
                {
                    tracing::error!("[engine] transition watchdog cancel: {e}");
                }

                if let Some(run) = run_handle {
                    run.stop();
                }
            }
        }
    }
}
