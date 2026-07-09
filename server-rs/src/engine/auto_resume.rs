//! Auto-resume after a usage-limit reset.
//!
//! When a turn ends with `errorKind == "usage_limit"` (the account's 5-hour or 7-day token
//! window is exhausted — see `classify_error.rs`), the session is stuck until the window
//! resets. With the per-session `autoResume` toggle ON (the default), the scheduler below:
//!
//!   1. computes WHEN the limit resets — in priority order:
//!        a. the `|<unix-seconds>` epoch the Claude CLI appends to its limit message
//!           (e.g. `"Claude AI usage limit reached|1735246800"`),
//!        b. the OAuth usage endpoint (`engine::usage::fetch_usage`): the MAX `resets_at`
//!           across windows whose `utilization` is at/above the exhausted threshold
//!           (if the 7-day window is also exhausted, resuming after the 5-hour reset
//!           alone would just fail again),
//!        c. fallback: retry in [AUTO_RESUME_FALLBACK_MS] (a too-early attempt fails with
//!           `usage_limit` again and is simply rescheduled — self-limiting),
//!      then persists it (+[AUTO_RESUME_BUFFER_MS]) into `Session.auto_resume_at`;
//!   2. once the instant passes, re-sends the turn through the normal [Engine::follow_up]
//!      path, which handles both a live parked process (stdin inject) and a terminal
//!      session (re-enqueue with `--resume`) and clears the error fields + schedule.
//!
//! The tick runs from the existing watchdog loop (every `WATCHDOG_TICK_MS`). All state is
//! persisted on the session row, so a scheduled resume survives a server restart. A user
//! `kill` is a deliberate stop — killed sessions are never auto-resumed.

use super::*;
use crate::engine::status::SessionStatus;
use std::str::FromStr;

/// Wait this long past the computed reset instant before resuming (clock-skew guard, and
/// the reset is a boundary — firing a second early would burn an attempt).
pub const AUTO_RESUME_BUFFER_MS: i64 = 60_000;
/// Retry cadence when no reset time can be determined from the error text or usage API.
pub const AUTO_RESUME_FALLBACK_MS: i64 = 30 * 60_000;
/// A usage window with `utilization` at/above this percentage counts as exhausted.
const EXHAUSTED_UTILIZATION: f64 = 95.0;
/// The follow-up prompt the scheduler sends when the limit has reset.
pub const AUTO_RESUME_PROMPT: &str = "Continue where you left off — the previous turn was \
interrupted by a usage limit that has since reset. Resume the task from its last state; do \
not redo work that already completed.";

/// Extract a reset instant from the Claude CLI's limit message, which carries the epoch as a
/// trailing `|<digits>` (e.g. `"Claude AI usage limit reached|1735246800"`). Accepts 10-digit
/// seconds or 13-digit milliseconds; rejects other digit counts (not a plausible timestamp).
pub fn reset_epoch_ms_from_error(text: &str) -> Option<i64> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\|\s*(\d{10}|\d{13})\b").expect("valid regex"));
    let n: i64 = RE.captures(text)?.get(1)?.as_str().parse().ok()?;
    // 10 digits = seconds, 13 = already ms.
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

/// Extract the reset instant from the OAuth usage endpoint's JSON: the MAX `resets_at` across
/// windows (`five_hour`, `seven_day`, `seven_day_*`, …) whose utilization is at/above
/// [EXHAUSTED_UTILIZATION]. Instants at/before `now_ms` are ignored (already reset). Returns
/// `None` when nothing looks exhausted — the caller falls back to the retry cadence.
///
/// Shape armor: the confirmed live shape is top-level windows with `utilization` (0..100) +
/// `resets_at` (the Android client renders exactly that today), but we also accept the
/// `used_percentage` alias and windows nested one level down (e.g. under `rate_limits`).
pub fn reset_epoch_ms_from_usage(usage: &serde_json::Value, now_ms: i64) -> Option<i64> {
    let obj = usage.as_object()?;
    let mut best: Option<i64> = None;
    let mut consider = |at: Option<i64>| {
        if let Some(at) = at {
            best = Some(best.map_or(at, |b| b.max(at)));
        }
    };
    for v in obj.values() {
        let Some(w) = v.as_object() else { continue };
        if w.contains_key("resets_at") {
            consider(window_reset_ms(w, now_ms));
        } else {
            // Container object (e.g. `rate_limits`): descend one level.
            for v2 in w.values() {
                if let Some(w2) = v2.as_object() {
                    consider(window_reset_ms(w2, now_ms));
                }
            }
        }
    }
    best
}

/// One usage window → its future reset instant, only when the window is exhausted.
fn window_reset_ms(w: &serde_json::Map<String, serde_json::Value>, now_ms: i64) -> Option<i64> {
    let util = w
        .get("utilization")
        .or_else(|| w.get("used_percentage"))?
        .as_f64()?;
    if util < EXHAUSTED_UTILIZATION {
        return None;
    }
    let at = w.get("resets_at").and_then(timestamp_ms)?;
    (at > now_ms).then_some(at)
}

/// Parse a JSON timestamp value: a number is epoch seconds (or ms when it's too large to be
/// seconds); a string is RFC 3339.
fn timestamp_ms(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => {
            let n = n.as_f64()? as i64;
            Some(if n < 100_000_000_000 { n * 1000 } else { n })
        }
        serde_json::Value::String(s) => rfc3339_to_epoch_ms(s),
        _ => None,
    }
}

/// Minimal RFC 3339 → epoch ms. Handles `YYYY-MM-DDTHH:MM:SS(.frac)?(Z|±HH:MM)`.
/// (No chrono/time dependency in this crate; this is the one place we parse dates.)
pub fn rfc3339_to_epoch_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_at(s.find(['T', 't', ' '])?);
    let rest = &rest[1..];

    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    // Split the time part from the offset suffix (Z, +HH:MM, -HH:MM).
    // (Unsigned parses everywhere below so "00:-5:00"-style negatives are rejected.)
    let (time, offset_min) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0i64)
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(pos);
        let sign: i64 = if off.starts_with('-') { -1 } else { 1 };
        let mut op = off[1..].split(':');
        let oh: u32 = op.next()?.parse().ok()?;
        let om: u32 = op.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh as i64 * 60 + om as i64))
    } else {
        return None; // offset is required in RFC 3339
    };

    let mut tp = time.split(':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    let sec_str = tp.next()?;
    if tp.next().is_some() || h > 23 || mi > 59 {
        return None;
    }
    let (sec_whole, frac_ms) = match sec_str.split_once('.') {
        Some((w, f)) => {
            let ms: u32 = format!("{:0<3}", f.chars().take(3).collect::<String>()).parse().ok()?;
            (w, ms)
        }
        None => (sec_str, 0),
    };
    let sec: u32 = sec_whole.parse().ok()?;
    if sec > 60 {
        return None; // 60 allowed for leap seconds
    }
    let (h, mi, sec, frac_ms) = (h as i64, mi as i64, sec as i64, frac_ms as i64);

    let days = days_from_civil(y, mo, d);
    Some((((days * 24 + h) * 60 + mi - offset_min) * 60 + sec) * 1000 + frac_ms)
}

/// Days from 1970-01-01 for a civil date (Howard Hinnant's `days_from_civil` algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Engine {
    /// Trigger an auto-resume tick (for tests).
    pub async fn trigger_auto_resume(&self) {
        self.tick_auto_resume().await;
    }

    /// One scheduler pass: schedule a resume time for newly limit-blocked sessions, and
    /// fire `follow_up` for sessions whose scheduled time has passed. Runs from the
    /// watchdog loop; everything it decides is persisted on the session row.
    pub(crate) async fn tick_auto_resume(&self) {
        if self.0.state.lock().closed {
            return;
        }
        let now = self.now();
        let sessions = match self.0.store.list().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[engine] auto-resume: store.list failed — skipping tick: {e}");
                return;
            }
        };
        // The usage endpoint is account-level — fetch at most once per tick, lazily.
        let mut usage_json: Option<Option<serde_json::Value>> = None;

        for s in sessions {
            if !s.auto_resume || s.error_kind.as_deref() != Some("usage_limit") {
                continue;
            }
            let status = SessionStatus::from_str(&s.status).unwrap_or(SessionStatus::Failed);
            // Eligible: errored terminals (failed; done covers a watchdog-reaped error turn)
            // and live sessions parked awaiting input after the errored turn. Killed is a
            // deliberate user stop — never auto-resume. Pending is already queued.
            let eligible = match status {
                SessionStatus::Failed | SessionStatus::Done => true,
                SessionStatus::Running => {
                    let state = self.0.state.lock();
                    state.awaiting.get(&s.id) == Some(&true)
                        && !state.pending_ask.contains(&s.id)
                        && !state.pending_delegate.contains(&s.id)
                }
                _ => false,
            };
            if !eligible {
                continue;
            }

            match s.auto_resume_at {
                // New limit episode: compute + persist the schedule.
                None => {
                    let from_error = s.error.as_deref().and_then(reset_epoch_ms_from_error);
                    let (target, source) = match from_error {
                        Some(t) => (Some(t), "error_text"),
                        None => {
                            if usage_json.is_none() {
                                usage_json = Some(self.fetch_usage_json().await);
                            }
                            let t = usage_json
                                .as_ref()
                                .and_then(|u| u.as_ref())
                                .and_then(|u| reset_epoch_ms_from_usage(u, now));
                            (t, if t.is_some() { "usage_api" } else { "fallback" })
                        }
                    };
                    let resume_at = target
                        .map(|t| t.max(now) + AUTO_RESUME_BUFFER_MS)
                        .unwrap_or(now + AUTO_RESUME_FALLBACK_MS);
                    // Conditional write: lands only while the row is STILL usage-limit-errored
                    // with no schedule. A user follow-up between our snapshot and here (clears
                    // errorKind + autoResumeAt) must not get a stale schedule written back.
                    match self.0.store.schedule_auto_resume(&s.id, resume_at).await {
                        Ok(true) => {}
                        Ok(false) => continue, // superseded while we were computing
                        Err(e) => {
                            tracing::warn!("[engine] auto-resume schedule persist failed for {}: {e}", s.id);
                            continue;
                        }
                    }
                    self.log(serde_json::json!({
                        "evt": "auto_resume_scheduled",
                        "sessionId": s.id,
                        "resumeAt": resume_at,
                        "source": source,
                    }));
                    tracing::info!(
                        "[engine] auto-resume: {} scheduled at {} (source={}, in {}s)",
                        s.id, resume_at, source, (resume_at - now) / 1000,
                    );
                }

                // Scheduled time reached: fire the follow-up.
                Some(at) if at <= now => {
                    // Atomically CLAIM the schedule before firing: clear `autoResumeAt` only
                    // while it still equals `at` and the row is still usage-limit-errored.
                    // Exactly one concurrent claimer can win, so an overlapping tick (or a
                    // user follow-up that already cleared the fields) can never double-fire.
                    // Clearing before firing also means a follow_up failure cannot hot-loop:
                    // if the turn errors with usage_limit again, the None arm above computes
                    // a fresh schedule. (A user message can still land in the tiny window
                    // between this claim and the follow_up below; the cost is one redundant
                    // "continue" prompt — accepted for a 30s-cadence scheduler.)
                    match self.0.store.claim_auto_resume(&s.id, at).await {
                        Ok(true) => {}
                        Ok(false) => continue, // superseded — someone else owns this session now
                        Err(e) => {
                            tracing::warn!("[engine] auto-resume claim failed for {}: {e}", s.id);
                            continue;
                        }
                    }
                    // With no resume target (no claudeSessionId — the limit hit before the
                    // SDK Init, or the transcript is unresumable) the turn runs FRESH and a
                    // bare "continue" carries zero context. Anchor it with the session's
                    // original request (the prompt/title column) so the model knows the task.
                    let prompt = if s.claude_session_id.as_deref().unwrap_or("").is_empty()
                        && !s.prompt.is_empty()
                    {
                        format!(
                            "{AUTO_RESUME_PROMPT}\n\nThe prior transcript could not be carried \
                             over; the original request was: {}",
                            s.prompt
                        )
                    } else {
                        AUTO_RESUME_PROMPT.to_string()
                    };
                    match self.follow_up(&s.id, &prompt, false, None, None, None).await {
                        Ok(_) => {
                            self.log(serde_json::json!({
                                "evt": "auto_resume_fired",
                                "sessionId": s.id,
                                "scheduledAt": at,
                            }));
                            tracing::info!("[engine] auto-resume: {} resumed (scheduled at {at})", s.id);
                        }
                        Err(e) => {
                            tracing::warn!("[engine] auto-resume follow_up failed for {}: {e}", s.id);
                        }
                    }
                }

                // Scheduled in the future: nothing to do this tick.
                Some(_) => {}
            }
        }
    }

    /// Fetch the account usage JSON: injected `usage_fn` (tests) or the real OAuth endpoint.
    /// Hard 10s timeout — this runs on the watchdog task, and a stalled endpoint (the shared
    /// reqwest client has no request timeout) must not freeze idle/wall reaping and all later
    /// ticks. Any failure (no credentials, HTTP error, timeout) → `None` → fallback cadence.
    async fn fetch_usage_json(&self) -> Option<serde_json::Value> {
        if let Some(ref f) = self.0.cfg.usage_fn {
            return f().ok();
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::engine::usage::fetch_usage(&self.0.cfg.claude_config_base, None),
        )
        .await
        {
            Ok(res) => res.ok(),
            Err(_) => {
                tracing::warn!("[engine] auto-resume: usage endpoint timed out (10s)");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_epoch_seconds_and_ms() {
        assert_eq!(
            reset_epoch_ms_from_error("Claude AI usage limit reached|1735246800"),
            Some(1_735_246_800_000)
        );
        assert_eq!(
            reset_epoch_ms_from_error("usage limit reached | 1735246800000"),
            Some(1_735_246_800_000)
        );
        assert_eq!(reset_epoch_ms_from_error("usage limit reached"), None);
        // Not a plausible timestamp (wrong digit count).
        assert_eq!(reset_epoch_ms_from_error("limit|12345"), None);
    }

    #[test]
    fn rfc3339_parses_utc_and_offsets() {
        // 2026-07-09T00:00:00Z = 1783555200
        assert_eq!(rfc3339_to_epoch_ms("2026-07-09T00:00:00Z"), Some(1_783_555_200_000));
        // Same instant expressed at +08:00.
        assert_eq!(rfc3339_to_epoch_ms("2026-07-09T08:00:00+08:00"), Some(1_783_555_200_000));
        // Fractional seconds.
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00.250Z"), Some(250));
        // Epoch itself.
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        // Garbage / missing offset.
        assert_eq!(rfc3339_to_epoch_ms("not a date"), None);
        assert_eq!(rfc3339_to_epoch_ms("2026-07-09T00:00:00"), None);
        // Negative components must be rejected, not silently subtracted.
        assert_eq!(rfc3339_to_epoch_ms("2026-07-09T00:-5:00Z"), None);
        assert_eq!(rfc3339_to_epoch_ms("2026-07-09T00:00:-5Z"), None);
    }

    #[test]
    fn usage_json_accepts_alias_and_nested_windows() {
        let now = 1_000_000;
        // `used_percentage` alias instead of `utilization`.
        let u = serde_json::json!({
            "five_hour": {"used_percentage": 100, "resets_at": 3600},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), Some(3_600_000));

        // Windows nested one level down (e.g. under `rate_limits`).
        let u = serde_json::json!({
            "rate_limits": {
                "five_hour": {"utilization": 100, "resets_at": 3600},
                "seven_day": {"utilization": 20, "resets_at": 999_999},
            }
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), Some(3_600_000));
    }

    #[test]
    fn usage_json_picks_max_reset_among_exhausted_windows() {
        let now = 1_000_000;
        // Only the five_hour window is exhausted → its reset wins.
        let u = serde_json::json!({
            "five_hour": {"utilization": 100, "resets_at": "1970-01-01T01:00:00Z"},
            "seven_day": {"utilization": 40, "resets_at": "1970-01-03T00:00:00Z"},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), Some(3_600_000));

        // Both exhausted → the LATER reset wins (resuming after the earlier one would fail).
        let u = serde_json::json!({
            "five_hour": {"utilization": 100, "resets_at": "1970-01-01T01:00:00Z"},
            "seven_day": {"utilization": 99, "resets_at": "1970-01-03T00:00:00Z"},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), Some(2 * 86_400_000));

        // Nothing exhausted → None.
        let u = serde_json::json!({
            "five_hour": {"utilization": 10, "resets_at": "1970-01-01T01:00:00Z"},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), None);

        // Epoch-seconds resets_at is accepted too.
        let u = serde_json::json!({
            "five_hour": {"utilization": 100, "resets_at": 3600},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, now), Some(3_600_000));

        // A reset already in the past is ignored.
        let u = serde_json::json!({
            "five_hour": {"utilization": 100, "resets_at": 1},
        });
        assert_eq!(reset_epoch_ms_from_usage(&u, 2_000_000), None);
    }
}
