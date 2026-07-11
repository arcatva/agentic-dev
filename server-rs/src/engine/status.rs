//! Session lifecycle status — strongly-typed enum that replaces the free-form
//! `String` previously stored in `Session.status`.
//!
//! The on-disk representation (DB column + JSON wire format) is unchanged:
//! the five lowercase strings `"pending"`, `"running"`, `"done"`, `"failed"`,
//! `"killed"`. The engine parses on read (`s.status.parse::<SessionStatus>()`)
//! and writes via `as_str()`, so DB rows written by old binaries still load
//! (unknown strings parse as `Failed` defensively — see PR1 tests).
//!
//! PR6 of the state-machine refactor will route all status mutations through
//! `Engine::transition()` (transition.rs) and the `legal_transition_to` matrix
//! below is the single source of truth for "is this move allowed".

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "killed")]
    Killed,
}

impl SessionStatus {
    /// Wire/DB representation. Matches the lowercase strings the engine has
    /// always written; do not change without a schema migration.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Pending => "pending",
            SessionStatus::Running => "running",
            SessionStatus::Done => "done",
            SessionStatus::Failed => "failed",
            SessionStatus::Killed => "killed",
        }
    }

    /// Terminal states: no further transitions are valid (the only "legal"
    /// transition from a terminal is to the same state via an idempotent
    /// no-op; see `legal_transition_to`).
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            SessionStatus::Done | SessionStatus::Failed | SessionStatus::Killed
        )
    }

    /// Authoritative transition matrix. PR6's `Engine::transition()` calls
    /// this; illegal pairs raise `EngineError::IllegalTransition`.
    ///
    /// Idempotent self-transitions are legal:
    /// - `Pending → Pending` is the `recover()` re-enqueue path
    /// - `Running → Running` is the `start()` already-running guard
    ///
    /// Everything else is a single-step move from a non-terminal to a
    /// non-idempotent target.
    pub fn legal_transition_to(&self, to: SessionStatus) -> bool {
        use SessionStatus::*;
        match (*self, to) {
            // Idempotent self-transitions
            (Pending, Pending) => true,
            (Running, Running) => true,

            // Out of Pending
            (Pending, Running) => true,
            (Pending, Failed) => true,
            (Pending, Killed) => true,

            // Out of Running
            (Running, Done) => true,
            (Running, Failed) => true,
            (Running, Killed) => true,

            // follow_up can re-queue a terminal session into pending
            // (the user explicitly clicked "send again"). This is the
            // "resume" path — clear error fields and start fresh.
            (Done, Pending) => true,
            (Failed, Pending) => true,
            (Killed, Pending) => true,

            // Terminals are otherwise dead-ends.
            (Done, _) => false,
            (Failed, _) => false,
            (Killed, _) => false,

            // Any remaining pair (e.g. Pending→Done, Running→Pending) is
            // an illegal shortcut. The unit test
            // `legal_transition_to_matches_matrix` pins the full 5x5 truth
            // table so this catch-all is safe.
            _ => false,
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = ();

    /// Parse a DB / JSON status string. Unknown values are treated as `Failed`
    /// so a future-added status never crashes an older binary — instead the
    /// session is conservatively marked as failed, which the user can resume.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(SessionStatus::Pending),
            "running" => Ok(SessionStatus::Running),
            "done" => Ok(SessionStatus::Done),
            "failed" => Ok(SessionStatus::Failed),
            "killed" => Ok(SessionStatus::Killed),
            _ => Ok(SessionStatus::Failed), // defensive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_as_str_round_trips() {
        for s in [
            SessionStatus::Pending,
            SessionStatus::Running,
            SessionStatus::Done,
            SessionStatus::Failed,
            SessionStatus::Killed,
        ] {
            assert_eq!(s.as_str().parse::<SessionStatus>().unwrap(), s);
        }
    }

    #[test]
    fn from_str_unknown_yields_failed() {
        // Defensive: a future-added status should never crash the parser.
        assert_eq!(
            "zombie".parse::<SessionStatus>().unwrap(),
            SessionStatus::Failed
        );
        assert_eq!("".parse::<SessionStatus>().unwrap(), SessionStatus::Failed);
        assert_eq!(
            "DONE".parse::<SessionStatus>().unwrap(),
            SessionStatus::Failed
        ); // case-sensitive
    }

    #[test]
    fn terminal_set_is_done_failed_killed() {
        assert!(!SessionStatus::Pending.terminal());
        assert!(!SessionStatus::Running.terminal());
        assert!(SessionStatus::Done.terminal());
        assert!(SessionStatus::Failed.terminal());
        assert!(SessionStatus::Killed.terminal());
    }

    #[test]
    fn legal_transition_to_matches_matrix() {
        use SessionStatus::*;
        // 5x5 = 25 pairs; enumerate and assert against the matrix.
        let cases = [
            // self-transitions (the two idempotent ones)
            (Pending, Pending, true),
            (Running, Running, true),
            // self-transitions of terminals are illegal
            (Done, Done, false),
            (Failed, Failed, false),
            (Killed, Killed, false),
            // out of Pending
            (Pending, Running, true),
            (Pending, Done, false),
            (Pending, Failed, true),
            (Pending, Killed, true),
            // out of Running
            (Running, Pending, false),
            (Running, Done, true),
            (Running, Failed, true),
            (Running, Killed, true),
            // out of Done (one allowed: follow_up resume → pending)
            (Done, Pending, true),
            (Done, Running, false),
            (Done, Failed, false),
            (Done, Killed, false),
            // out of Failed (one allowed: follow_up resume → pending)
            (Failed, Pending, true),
            (Failed, Running, false),
            (Failed, Done, false),
            (Failed, Killed, false),
            // out of Killed (one allowed: follow_up resume → pending)
            (Killed, Pending, true),
            (Killed, Running, false),
            (Killed, Done, false),
            (Killed, Failed, false),
        ];
        for (from, to, expected) in cases {
            assert_eq!(
                from.legal_transition_to(to),
                expected,
                "legal_transition_to({from:?} -> {to:?}) should be {expected}",
            );
        }
    }

    #[test]
    fn as_str_matches_db_column_values() {
        // Pin the wire format — DB rows are persisted with these exact strings.
        assert_eq!(SessionStatus::Pending.as_str(), "pending");
        assert_eq!(SessionStatus::Running.as_str(), "running");
        assert_eq!(SessionStatus::Done.as_str(), "done");
        assert_eq!(SessionStatus::Failed.as_str(), "failed");
        assert_eq!(SessionStatus::Killed.as_str(), "killed");
    }

    #[test]
    fn display_matches_as_str() {
        for s in [
            SessionStatus::Pending,
            SessionStatus::Running,
            SessionStatus::Done,
            SessionStatus::Failed,
            SessionStatus::Killed,
        ] {
            assert_eq!(format!("{s}"), s.as_str());
        }
    }

    #[test]
    fn serde_uses_lowercase_tag() {
        // Android client deserializes these exact strings.
        let json = serde_json::to_string(&SessionStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");
        let parsed: SessionStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(parsed, SessionStatus::Failed);
    }
}
