//! PR4: SessionLifecycleLog sidecar — authoritative "when did the current
//! turn start / end" record.
//!
//! Stored as `<log_dir>/<id>.jsonl.state` — append-only JSONL, one event
//! per line. Same atomic-append discipline as `Store::append_log`
//! (store.rs:169). Deleted alongside the main log in `Store::remove`.
//!
//! Why a sidecar? The SDK's `result` event has no top-level timestamp,
//! only `duration_ms` / `ttft_ms` (durations since the turn started).
//! The current `recover()` bug is rooted in this: it cannot tell a 429
//! from 2 hours ago from a 429 that just landed. The sidecar solves it:
//! `last_turn_started_at()` gives the engine a wall-clock anchor; the
//! transcript scan can then bound itself to "events since the last turn
//! started" and skip stale results.
//!
//! PR4 wires the writes (TurnStarted on every fresh turn, TurnEnded on
//! every terminal). PR5 will add the reader used by `recover()`.

use crate::engine::status::SessionStatus;
use crate::engine::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Success,
    Error,
    Killed,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LifecycleEvent {
    TurnStarted {
        at: i64,
        prompt_len: usize,
    },
    TurnEnded {
        at: i64,
        outcome: TurnOutcome,
        cost_usd: Option<f64>,
        duration_ms: Option<i64>,
    },
    BootRecovered {
        at: i64,
        decided: SessionStatus,
        reason: String,
    },
}

/// The per-session current-turn outcome, used by `recover()` to decide
/// done vs failed vs interrupted. Distinct from `TurnOutcome` because
/// "no result in current turn" is also a state we need to represent.
#[derive(Debug, Clone, PartialEq)]
pub enum CurrentTurnOutcome {
    /// A success `result` event landed in the current turn.
    Success,
    /// An error `result` event landed in the current turn. The text is
    /// the SDK's `result` field (may be the 429 message, the crash
    /// diagnostic, or a Claude Error string).
    Error(String),
    /// No `result` event in the current turn window — the SDK died
    /// mid-turn (crash, OOM, SIGKILL).
    None,
}

pub fn sidecar_path(log_dir: &Path, id: &str) -> PathBuf {
    log_dir.join(format!("{id}.jsonl.state"))
}

impl Store {
    /// Append one lifecycle event to `<log_dir>/<id>.jsonl.state`. Same
    /// `OpenOptions::create(true).append(true)` discipline as `append_log`.
    pub async fn append_lifecycle(&self, id: &str, ev: &LifecycleEvent) -> Result<(), StoreError> {
        // The sidecar lives next to the main log. We can't reach
        // `self.log_dir` (private) so we route through a free function
        // that takes the path explicitly. The path is exposed via
        // `self.log_path(id).parent()` (the same dir the main log lives
        // in).
        let log_path = self.log_path(id);
        let log_dir = log_path
            .parent()
            .ok_or_else(|| StoreError::Io(std::io::Error::other("log_path has no parent")))?;
        let line = serde_json::to_string(ev).map_err(StoreError::Json)?;
        let path = sidecar_path(log_dir, id);
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            if let Some(parent) = path_clone.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_clone)?;
            writeln!(f, "{line}")?;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Read all lifecycle events for `id`. Missing or unreadable file
    /// returns `vec![]` (legacy compat: a session without a sidecar is
    /// treated as if no lifecycle events ever happened — `recover()`'s
    /// `last_outcome_in_current_turn` falls back to the existing
    /// "any result in the log is current" behavior).
    pub async fn read_lifecycle(&self, id: &str) -> Vec<LifecycleEvent> {
        let log_path = self.log_path(id);
        let log_dir = match log_path.parent() {
            Some(d) => d,
            None => return Vec::new(),
        };
        read_lifecycle(log_dir, id).await
    }
}

/// Standalone (non-async-friendly) read for use from `recover()`'s sync
/// path. Returns events in file order. Skips blank lines and corrupt
/// JSON lines (best-effort — we never want a bad line to crash recovery).
pub async fn read_lifecycle(log_dir: &Path, id: &str) -> Vec<LifecycleEvent> {
    let path = sidecar_path(log_dir, id);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&bytes);
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LifecycleEvent>(l).ok())
        .collect()
}

/// Convenience: the `at` of the most recent `TurnStarted`, or `None` if
/// the sidecar is missing/empty. This is the wall-clock anchor
/// `recover()` uses to bound the transcript scan.
pub fn last_turn_started_at(events: &[LifecycleEvent]) -> Option<i64> {
    events.iter().rev().find_map(|e| match e {
        LifecycleEvent::TurnStarted { at, .. } => Some(*at),
        _ => None,
    })
}

/// Convenience: the `at` of the most recent `TurnEnded`, or `None` if the
/// sidecar is missing/empty / no turn ended yet. This is the AUTHORITATIVE
/// end timestamp `recover()` restores into `endedAt` so a session whose turn
/// actually finished before a server restart keeps its real end time instead
/// of being re-stamped to the restart wall-clock (which would resurrect the
/// client's unread dot).
pub fn last_turn_ended_at(events: &[LifecycleEvent]) -> Option<i64> {
    events.iter().rev().find_map(|e| match e {
        LifecycleEvent::TurnEnded { at, .. } => Some(*at),
        _ => None,
    })
}

/// Fallback end-time source for sessions written before the lifecycle sidecar
/// carried `TurnEnded` (no sidecar => no `last_turn_ended_at`): the `at` of the
/// most recent `agentic_prompt` marker in the transcript, i.e. the current
/// turn's START. It is necessarily <= the moment the user could have read the
/// session, so using it as `endedAt` never lights a spurious unread dot for an
/// already-read session, while still being a sane "last active" timestamp.
pub fn last_prompt_at(transcript: &[String]) -> Option<i64> {
    use crate::engine::stream::{parse_line, ClaudeEvent};
    // Scan in reverse and short-circuit on the first (= most recent) prompt: a
    // transcript can be tens of kB, and we only want the last turn's start, so a
    // forward scan that parses every line is wasteful. Reversing both the lines
    // and the per-line events preserves "newest in file order".
    for line in transcript.iter().rev() {
        for ev in parse_line(line).into_iter().rev() {
            if let ClaudeEvent::Prompt { at, .. } = ev {
                return Some(at);
            }
        }
    }
    None
}

/// Scan the transcript + sidecar to decide what happened in the current
/// turn. Mirrors the algorithm in the plan:
///
/// - If `turn_start` is `None` (no sidecar), any result in the log is
///   treated as the current turn's outcome. Legacy compat: preserves
///   the behavior of the existing `last_result()` for sessions written
///   before PR4.
/// - If `turn_start` is `Some(ts)`, only results whose preceding
///   `agentic_prompt.at` is `>= ts` count. The reverse-scan walks the
///   file, tracking the most recent prompt marker, and stops at the
///   first result in the current turn.
///
/// Returns the outcome AND the result text (for `recover()` to feed into
/// `classify_claude_error`).
pub fn last_outcome_in_current_turn(
    transcript: &[String],
    turn_start: Option<i64>,
) -> CurrentTurnOutcome {
    use crate::engine::stream::parse_line;

    // Forward scan. The invariant: a result's "current turn" is the
    // turn that started with the most recent `agentic_prompt` BEFORE
    // it in the file. If that prompt's `at >= turn_start`, the result
    // belongs to the current turn; otherwise it's stale.
    //
    // We track `current_turn_prompt_at` as we walk. When we see a
    // result, we check it against turn_start. The LAST result in the
    // current turn is the answer (later results are newer). Stale
    // results (those with prompt < turn_start) are skipped — but if we
    // never see a current-turn result, we return None.
    let mut current_turn_prompt_at: Option<i64> = None;
    let mut latest_current_turn_result: Option<CurrentTurnOutcome> = None;

    for line in transcript.iter() {
        for ev in parse_line(line) {
            match ev {
                crate::engine::stream::ClaudeEvent::Prompt { at, .. } => {
                    // Each prompt starts a new turn. Update the
                    // "current turn" anchor.
                    current_turn_prompt_at = Some(at);
                }
                crate::engine::stream::ClaudeEvent::Result { is_error, text, .. } => {
                    // Decide if this result is "in current turn".
                    let in_current_turn = match turn_start {
                        None => true, // legacy: any result counts
                        Some(ts) => match current_turn_prompt_at {
                            // No prompt seen before this result yet —
                            // the result belongs to "an even earlier
                            // session" (rare; skip when anchored).
                            None => false,
                            Some(p_at) => p_at >= ts,
                        },
                    };
                    if in_current_turn {
                        latest_current_turn_result = Some(if is_error {
                            CurrentTurnOutcome::Error(
                                text.unwrap_or_else(|| "turn ended with an error".to_string()),
                            )
                        } else {
                            CurrentTurnOutcome::Success
                        });
                        // Keep walking — a LATER result in the same
                        // turn is more recent and wins.
                    }
                    // Pre-current-turn result: skip.
                }
                _ => {}
            }
        }
    }
    latest_current_turn_result.unwrap_or(CurrentTurnOutcome::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::store::CreateInput;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agentic-lifecycle-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn record_and_read_round_trip() {
        let work = tmp();
        let logs = work.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let store = Store::open(work.join("db.sqlite"), &logs).await.unwrap();
        store
            .create(CreateInput {
                id: "lc1".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("wt").to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .append_lifecycle(
                "lc1",
                &LifecycleEvent::TurnStarted {
                    at: 100,
                    prompt_len: 5,
                },
            )
            .await
            .unwrap();
        store
            .append_lifecycle(
                "lc1",
                &LifecycleEvent::TurnEnded {
                    at: 200,
                    outcome: TurnOutcome::Success,
                    cost_usd: Some(0.01),
                    duration_ms: Some(100),
                },
            )
            .await
            .unwrap();

        let events = store.read_lifecycle("lc1").await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            LifecycleEvent::TurnStarted { at: 100, .. }
        ));
        assert!(matches!(
            events[1],
            LifecycleEvent::TurnEnded { at: 200, .. }
        ));
    }

    #[tokio::test]
    async fn missing_file_reads_as_empty() {
        let work = tmp();
        let logs = work.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let store = Store::open(work.join("db.sqlite"), &logs).await.unwrap();
        let events = store.read_lifecycle("nonexistent").await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn partial_write_survives_corrupt_tail() {
        // Write valid + invalid + valid; reader must return the 2 valid
        // events and skip the corrupt one.
        let work = tmp();
        let logs = work.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = sidecar_path(&logs, "lc2");
        let good1 = serde_json::to_string(&LifecycleEvent::TurnStarted {
            at: 1,
            prompt_len: 0,
        })
        .unwrap();
        let good2 = serde_json::to_string(&LifecycleEvent::TurnEnded {
            at: 2,
            outcome: TurnOutcome::Success,
            cost_usd: None,
            duration_ms: None,
        })
        .unwrap();
        std::fs::write(&path, format!("{good1}\n{{not valid json}}\n{good2}\n")).unwrap();

        let events = read_lifecycle(&logs, "lc2").await;
        assert_eq!(events.len(), 2, "corrupt line should be skipped");
    }

    #[test]
    fn last_turn_started_at_returns_most_recent() {
        let events = vec![
            LifecycleEvent::TurnStarted {
                at: 100,
                prompt_len: 0,
            },
            LifecycleEvent::TurnEnded {
                at: 200,
                outcome: TurnOutcome::Success,
                cost_usd: None,
                duration_ms: None,
            },
            LifecycleEvent::TurnStarted {
                at: 300,
                prompt_len: 0,
            },
            LifecycleEvent::BootRecovered {
                at: 400,
                decided: SessionStatus::Done,
                reason: "x".into(),
            },
        ];
        assert_eq!(last_turn_started_at(&events), Some(300));
    }

    #[test]
    fn last_turn_started_at_empty_returns_none() {
        assert_eq!(last_turn_started_at(&[]), None);
    }

    #[test]
    fn last_outcome_in_current_turn_legacy_no_sidecar_treats_any_result_as_current() {
        // No turn_start → legacy compat: any result in the log wins.
        let transcript = vec![
            r#"{"type":"agentic_prompt","text":"a","at":1000}"#.to_string(),
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.01}"#
                .to_string(),
        ];
        let outcome = last_outcome_in_current_turn(&transcript, None);
        assert_eq!(outcome, CurrentTurnOutcome::Success);
    }

    #[test]
    fn last_outcome_in_current_turn_distinguishes_old_and_new_results() {
        // The core regression test for the 2026-06-23 incident:
        //   - old turn: prompt at T0=100, result=error (the 429)
        //   - new turn: prompt at T1=1000, result=success
        // With turn_start=Some(1000) the algorithm should find the
        // success result, NOT the old 429. This is what the new
        // recover() uses to make the right decision.
        let transcript = vec![
            r#"{"type":"agentic_prompt","text":"a","at":100}"#.to_string(),
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 429 Token Plan..."#.to_string(),
            r#"{"type":"agentic_prompt","text":"b","at":1000}"#.to_string(),
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.05}"#.to_string(),
        ];

        // With turn_start=Some(1000) (the new turn), the 429 is stale.
        let outcome = last_outcome_in_current_turn(&transcript, Some(1000));
        assert_eq!(
            outcome,
            CurrentTurnOutcome::Success,
            "stale 429 must be ignored when turn_start is set to the new turn",
        );

        // With turn_start=Some(50) (an even older anchor, before either
        // prompt), both prompts are in current turn → the latest result
        // (success, after prompt@1000) wins.
        let outcome = last_outcome_in_current_turn(&transcript, Some(50));
        assert_eq!(outcome, CurrentTurnOutcome::Success);

        // With turn_start=Some(500) (between the two prompts): prompt@100
        // is stale (100 < 500), prompt@1000 is current. So the only
        // result in the current turn is the success.
        let outcome = last_outcome_in_current_turn(&transcript, Some(500));
        assert_eq!(outcome, CurrentTurnOutcome::Success);
    }

    #[test]
    fn last_outcome_in_current_turn_no_result_returns_none() {
        // The SDK died mid-turn (no result event at all). recover()
        // maps None → "interrupted" reason text.
        let transcript = vec![
            r#"{"type":"agentic_prompt","text":"a","at":1000}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#.to_string(),
        ];
        assert_eq!(
            last_outcome_in_current_turn(&transcript, Some(1000)),
            CurrentTurnOutcome::None
        );
    }
}
