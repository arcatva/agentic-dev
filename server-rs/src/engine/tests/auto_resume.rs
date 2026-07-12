use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::title_client::{InMemoryTitleGenerator, TitleGenerator};

// ── Auto-resume after usage-limit reset ─────────────────────────────

/// Fixed test clock. Returns (overrides-ready NowFn, handle to advance time).
fn test_clock(start_ms: i64) -> (NowFn, Arc<std::sync::atomic::AtomicI64>) {
    let t = Arc::new(std::sync::atomic::AtomicI64::new(start_ms));
    let t2 = t.clone();
    (Arc::new(move || t2.load(Ordering::SeqCst)), t)
}

/// Create a session row parked in `failed` with a usage-limit error.
async fn seed_limited_session(e: &Engine, id: &str, error_text: &str) {
    e.0.store
        .create(crate::engine::store::CreateInput {
            id: id.into(),
            prompt: "task".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    e.0.store
        .update(
            id,
            SessionPatch {
                status: Some("failed".into()),
                error: Some(Some(error_text.into())),
                error_kind: Some(Some("usage_limit".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_resume_schedules_from_error_epoch() {
    const NOW: i64 = 1_800_000_000_000;
    let reset_s = (NOW + 2 * 3600 * 1000) / 1000; // resets in 2h, epoch-seconds
    let src = tmp();
    let (now_fn, _) = test_clock(NOW);
    let e = make_engine(
        &src,
        EngineOverrides {
            now_fn: Some(now_fn),
            ..Default::default()
        },
    )
    .await;
    seed_limited_session(
        &e,
        "s1",
        &format!("Claude AI usage limit reached|{reset_s}"),
    )
    .await;

    e.trigger_auto_resume().await;

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(
        s.auto_resume_at,
        Some(reset_s * 1000 + crate::engine::auto_resume::AUTO_RESUME_BUFFER_MS),
        "schedule = error-text epoch + buffer"
    );
    assert_eq!(s.status, "failed", "not resumed before the reset time");

    // A second tick before the reset time must not fire or reschedule.
    e.trigger_auto_resume().await;
    let s2 = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(s2.auto_resume_at, s.auto_resume_at);
    assert_eq!(s2.status, "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_resume_fires_once_reset_passes() {
    const NOW: i64 = 1_800_000_000_000;
    let reset_s = (NOW + 3600 * 1000) / 1000;
    let src = tmp();
    let (now_fn, clock) = test_clock(NOW);
    let e = make_engine(
        &src,
        EngineOverrides {
            now_fn: Some(now_fn),
            ..Default::default()
        },
    )
    .await;
    seed_limited_session(&e, "s1", &format!("usage limit reached|{reset_s}")).await;

    e.trigger_auto_resume().await; // schedules
    clock.store(
        reset_s * 1000 + crate::engine::auto_resume::AUTO_RESUME_BUFFER_MS + 1,
        Ordering::SeqCst,
    );
    e.trigger_auto_resume().await; // fires follow_up

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(s.auto_resume_at, None, "schedule consumed");
    assert_eq!(s.error_kind, None, "follow_up cleared the error");
    assert_ne!(
        s.status, "failed",
        "session re-enqueued (pending/running/done)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_resume_uses_usage_api_when_error_has_no_epoch() {
    const NOW: i64 = 1_800_000_000_000;
    let reset_ms = NOW + 5 * 3600 * 1000;
    let src = tmp();
    let (now_fn, _) = test_clock(NOW);
    let usage: UsageFn = Arc::new(move || {
        Ok(serde_json::json!({
            "five_hour": {"utilization": 100, "resets_at": reset_ms / 1000},
            "seven_day": {"utilization": 30, "resets_at": (NOW + 3 * 86_400_000) / 1000},
        }))
    });
    let e = make_engine(
        &src,
        EngineOverrides {
            now_fn: Some(now_fn),
            usage_fn: Some(usage),
            ..Default::default()
        },
    )
    .await;
    seed_limited_session(&e, "s1", "You've hit your session limit · resets 3:30pm").await;

    e.trigger_auto_resume().await;

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(
        s.auto_resume_at,
        Some(reset_ms + crate::engine::auto_resume::AUTO_RESUME_BUFFER_MS),
        "schedule from the exhausted five_hour window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_resume_falls_back_to_retry_cadence() {
    const NOW: i64 = 1_800_000_000_000;
    let src = tmp();
    let (now_fn, _) = test_clock(NOW);
    let usage: UsageFn = Arc::new(|| Err("no credentials".into()));
    let e = make_engine(
        &src,
        EngineOverrides {
            now_fn: Some(now_fn),
            usage_fn: Some(usage),
            ..Default::default()
        },
    )
    .await;
    seed_limited_session(&e, "s1", "usage limit reached").await;

    e.trigger_auto_resume().await;

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(
        s.auto_resume_at,
        Some(NOW + crate::engine::auto_resume::AUTO_RESUME_FALLBACK_MS),
        "unknown reset time → fallback retry cadence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_resume_respects_toggle_and_kill() {
    const NOW: i64 = 1_800_000_000_000;
    let src = tmp();
    let (now_fn, _) = test_clock(NOW);
    let e = make_engine(
        &src,
        EngineOverrides {
            now_fn: Some(now_fn),
            ..Default::default()
        },
    )
    .await;

    // Toggle OFF → never scheduled.
    seed_limited_session(&e, "off", "usage limit reached|1800003600").await;
    e.0.store
        .update(
            "off",
            SessionPatch {
                auto_resume: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Killed → deliberate stop, never scheduled even with the toggle on.
    seed_limited_session(&e, "killed", "usage limit reached|1800003600").await;
    e.0.store
        .update(
            "killed",
            SessionPatch {
                status: Some("killed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    e.trigger_auto_resume().await;

    assert_eq!(
        e.0.store.get("off").await.unwrap().unwrap().auto_resume_at,
        None
    );
    assert_eq!(
        e.0.store
            .get("killed")
            .await
            .unwrap()
            .unwrap()
            .auto_resume_at,
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_follow_up_supersedes_scheduled_auto_resume() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    seed_limited_session(&e, "s1", "usage limit reached").await;
    e.0.store
        .update(
            "s1",
            SessionPatch {
                auto_resume_at: Some(Some(9_999_999_999_999)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    e.follow_up("s1", "user takes over", false, None, None, None)
        .await
        .unwrap();

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(
        s.auto_resume_at, None,
        "manual follow-up cancels the scheduled resume"
    );
}

#[tokio::test]
async fn kill_cancels_scheduled_auto_resume_even_on_terminal_session() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    seed_limited_session(&e, "s1", "usage limit reached").await;
    e.0.store
        .update(
            "s1",
            SessionPatch {
                auto_resume_at: Some(Some(9_999_999_999_999)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // The session is already FAILED (terminal) — kill() must still cancel the schedule,
    // otherwise a user "stop" is ignored and the scheduler resurrects the session later.
    e.kill("s1").await;

    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert_eq!(
        s.auto_resume_at, None,
        "kill must cancel a scheduled auto-resume"
    );
}

#[tokio::test]
async fn schedule_auto_resume_requires_live_usage_limit_episode() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    seed_limited_session(&e, "s1", "usage limit reached").await;

    // Normal path: errored row with no schedule → write lands.
    assert!(e.0.store.schedule_auto_resume("s1", 5000).await.unwrap());
    // Already scheduled → refuse (no silent overwrite).
    assert!(!e.0.store.schedule_auto_resume("s1", 6000).await.unwrap());
    assert_eq!(
        e.0.store.get("s1").await.unwrap().unwrap().auto_resume_at,
        Some(5000)
    );

    // A user follow-up cleared the episode → a stale schedule must NOT land.
    e.0.store
        .update(
            "s1",
            SessionPatch {
                error_kind: Some(None),
                auto_resume_at: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!e.0.store.schedule_auto_resume("s1", 7000).await.unwrap());
    assert_eq!(
        e.0.store.get("s1").await.unwrap().unwrap().auto_resume_at,
        None
    );
}

#[tokio::test]
async fn claim_auto_resume_wins_exactly_once() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    seed_limited_session(&e, "s1", "usage limit reached").await;
    e.0.store
        .update(
            "s1",
            SessionPatch {
                auto_resume_at: Some(Some(1000)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Wrong `at` (stale snapshot) → no claim.
    assert!(!e.0.store.claim_auto_resume("s1", 999).await.unwrap());
    // First correct claim wins; second loses (already cleared).
    assert!(e.0.store.claim_auto_resume("s1", 1000).await.unwrap());
    assert!(!e.0.store.claim_auto_resume("s1", 1000).await.unwrap());

    // A session whose error was cleared (user follow-up) can no longer be claimed.
    e.0.store
        .update(
            "s1",
            SessionPatch {
                auto_resume_at: Some(Some(2000)),
                error_kind: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!e.0.store.claim_auto_resume("s1", 2000).await.unwrap());
}

#[tokio::test]
async fn session_defaults_auto_resume_on_and_survives_patch() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    e.0.store
        .create(crate::engine::store::CreateInput {
            id: "s1".into(),
            prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert!(s.auto_resume, "default ON");
    assert_eq!(s.auto_resume_at, None);

    e.0.store
        .update(
            "s1",
            SessionPatch {
                auto_resume: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let s = e.0.store.get("s1").await.unwrap().unwrap();
    assert!(!s.auto_resume, "toggle persists");

    // Wire format: autoResume always present; autoResumeAt omitted when unscheduled.
    let wire = serde_json::to_value(&s).unwrap();
    assert_eq!(wire["autoResume"], serde_json::json!(false));
    assert!(wire.get("autoResumeAt").is_none());
}
