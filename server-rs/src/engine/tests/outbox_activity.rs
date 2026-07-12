use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::title_client::{InMemoryTitleGenerator, TitleGenerator};

// ── with_activity outbox markers: restart must not re-emit ─────────────────────────

/// Count `agentic_file` marker lines in a session's log, grouped as (total, per-path max).
fn file_marker_count(e: &Engine, id: &str) -> usize {
    e.0.store
        .read_log(id)
        .iter()
        .filter(|l| l.contains("\"type\":\"agentic_file\""))
        .count()
}

#[tokio::test]
async fn with_activity_does_not_reemit_outbox_markers_after_state_loss() {
    // The engine tracks "already marked" outbox paths in the in-memory `logged_outbox` set.
    // A server restart (fresh EngineState) wiped it, so the next with_activity touch re-appended
    // an `agentic_file` marker for EVERY file already in the outbox — at the log TAIL, piling
    // old file cards at the bottom of the transcript and duplicating cards already inline.
    let e = test_engine().await;
    // Multi-repo session → the watched outbox is at the session worktree ROOT.
    let wt = tmp();
    std::fs::create_dir_all(wt.join("outbox")).unwrap();
    std::fs::write(wt.join("outbox").join("plan.md"), "x").unwrap();
    e.0.store
        .create(CreateInput {
            id: "s-outbox".into(),
            prompt: "p".into(),
            repos: vec!["r1".into(), "r2".into()],
            worktree_path: Some(wt.to_string_lossy().into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = e.0.store.get("s-outbox").await.unwrap().unwrap();

    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox"),
        1,
        "first touch marks the file once"
    );
    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox"),
        1,
        "same-process re-touch never re-marks"
    );

    // Simulate a server restart: in-memory engine state is reborn empty (EngineState::default()).
    e.0.state.lock().logged_outbox.clear();
    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox"),
        1,
        "state loss must NOT re-emit markers for files already marked in the log"
    );

    // A genuinely NEW file delivered after the restart still gets exactly one marker.
    std::fs::write(wt.join("outbox").join("build.apk"), "y").unwrap();
    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox"),
        2,
        "new file after restart marks once"
    );
    let _ = e.with_activity(s.clone());
    assert_eq!(file_marker_count(&e, "s-outbox"), 2, "and only once");
}

#[tokio::test]
async fn with_activity_seeding_covers_the_single_repo_outbox_layout() {
    // Single-repo sessions watch `<wt>/<repo>/outbox` (not the session root). The restart
    // seeding must behave identically for that layout.
    let e = test_engine().await;
    let wt = tmp();
    std::fs::create_dir_all(wt.join("r1").join("outbox")).unwrap();
    std::fs::write(wt.join("r1").join("outbox").join("report.md"), "x").unwrap();
    e.0.store
        .create(CreateInput {
            id: "s-outbox-1repo".into(),
            prompt: "p".into(),
            repos: vec!["r1".into()],
            worktree_path: Some(wt.to_string_lossy().into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = e.0.store.get("s-outbox-1repo").await.unwrap().unwrap();

    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox-1repo"),
        1,
        "first touch marks once"
    );
    // Restart, then touch again: the persisted marker must suppress a re-emit.
    e.0.state.lock().logged_outbox.clear();
    let _ = e.with_activity(s.clone());
    assert_eq!(
        file_marker_count(&e, "s-outbox-1repo"),
        1,
        "single-repo layout: state loss must not re-emit"
    );
}
