use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_skill_session_no_repo_runs_done() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "just answer".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    assert!(s.repos.is_empty());
    assert_eq!(s.status, "done");
}

// ── Task 6 tests: kill / interrupt / watchdog ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_running_marks_killed() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e.submit("demo", "slow", slow).await.unwrap();
    wait_status(&e, &id, "running").await;
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// A user-initiated Stop must stay a clean interrupt even when the aborted turn surfaces a late
/// is_error result line. kill() flips status to "killed" BEFORE the run is aborted, so the abort's
/// synthetic error result (today "claude query failed: …"; on the live build the
/// "[ede_diagnostic] result_type=user … stop_reason=tool_use" line) reaches on_event AFTER the kill.
/// A deliberate Stop is not an error: the engine must not classify it as claude_error, or the
/// Android client paints a red dot + "⚠ Claude error" banner on a turn the user chose to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_session_ignores_a_late_error_result() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e.submit("demo", "slow", slow).await.unwrap();
    wait_status(&e, &id, "running").await;

    // User presses Stop → status flips to "killed".
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;

    // The aborted turn's late error result lands after the kill.
    e.on_event(
        &id,
        ClaudeEvent::Result {
            is_error: true,
            cost_usd: None,
            text: Some(
                "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"
                    .into(),
            ),
            raw: serde_json::json!({}),
        },
    )
    .await;

    let s = e.get(&id).await.unwrap();
    assert_eq!(s.status, "killed", "a stopped session stays killed");
    assert!(
        s.error_kind.is_none(),
        "a deliberate Stop must not be classified as an error, got {:?}",
        s.error_kind
    );
    assert!(
        s.error.is_none(),
        "a deliberate Stop must not carry error text, got {:?}",
        s.error
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_queued_marks_killed_immediately() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    // max_concurrent=1 so second session stays queued
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        },
    )
    .await;
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let _id1 = e.submit("demo", "first", slow).await.unwrap();
    let id2 = e.submit("demo", "second", HashMap::new()).await.unwrap();
    // Wait for first to be running so second is definitely queued.
    wait_status(&e, &_id1, "running").await;
    assert_eq!(e.get(&id2).await.unwrap().status, "pending");
    e.kill(&id2).await;
    // Should be killed now without ever running.
    let s = e.get(&id2).await.unwrap();
    assert_eq!(s.status, "killed");
    assert!(
        s.ended_at.is_some(),
        "killed queued session must have endedAt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_does_not_change_status() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e.submit("demo", "slow", slow).await.unwrap();
    wait_status(&e, &id, "running").await;
    e.interrupt(&id); // must not panic or change status
                      // Status should still be running or transition normally (not killed/failed by interrupt).
    let s = e.get(&id).await.unwrap();
    assert!(
        s.status == "running" || s.status == "done" || s.status == "failed",
        "interrupt should not force-kill the session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_noop_on_absent_session() {
    let e = test_engine().await;
    // Should not panic
    e.interrupt("nonexistent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_idle_reap_marks_done_and_resumable() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // idle_max=500ms; wall left unset (= unlimited).
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_max_ms: Some(500),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(
            vec![],
            vec![],
            "stay forever".into(),
            env,
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Back-date last_event_at by 60 seconds → idle.
    e.test_set_last_event_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    // Graceful cancel: marked `done` (NOT failed), with no error / error_kind.
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "done",
        "idle reap must be a graceful done, not failed"
    );
    assert!(
        s.error.is_none(),
        "graceful cancel must not set an error: {:?}",
        s.error
    );
    assert!(
        s.error_kind.is_none(),
        "graceful cancel must not set error_kind: {:?}",
        s.error_kind
    );
    // And it must stay resumable — the user can continue the conversation.
    assert!(
        e.follow_up(&id, "continue please", true, None, None, None)
            .await
            .is_ok(),
        "session must be resumable after a graceful idle cancel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_idle_reap_frees_slot_for_next_session() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            idle_max_ms: Some(500),
            wall_max_ms: Some(3_600_000),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(
            vec![],
            vec![],
            "stay forever".into(),
            env,
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    e.test_set_last_event_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    wait_status(&e, &id, "done").await; // graceful cancel → done (not failed)
                                        // Now submit a second session — the slot should be free.
    let id2 = e
        .submit_session(
            vec![],
            vec![],
            "after".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id2, "done").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_wall_reap_marks_done_when_cap_set() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // wall_max=500ms (opt-in), idle_max=very high.
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_max_ms: Some(3_600_000),
            wall_max_ms: Some(500),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Back-date turn_started_at by 60 seconds (keep last_event_at fresh → not idle).
    e.test_set_turn_started_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    // Graceful cancel → done (resumable), not failed.
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(s.status, "done");
    assert!(
        s.error.is_none(),
        "wall graceful cancel must not set an error: {:?}",
        s.error
    );
    assert!(
        s.error_kind.is_none(),
        "wall graceful cancel must not set error_kind: {:?}",
        s.error_kind
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_wall_unlimited_by_default() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // No wall_max set → wall is unlimited (idle high too). A long-running but recently-active
    // turn must NOT be reaped.
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_max_ms: Some(3_600_000),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Back-date turn_started_at by an hour; keep last_event_at fresh.
    e.test_set_turn_started_at(&id, e.now() - 3_600_000);
    e.trigger_watchdog().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        e.get(&id).await.unwrap().status,
        "running",
        "wall is unlimited by default — a long-running active turn must not be reaped"
    );
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_does_not_wall_reap_parked_session() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // wall_max=200ms, idle_max=very high
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_max_ms: Some(3_600_000),
            wall_max_ms: Some(200),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Mark as parked (awaiting=true) and back-date turn_started_at.
    e.test_set_awaiting(&id, true);
    e.test_set_turn_started_at(&id, e.now() - 60_000);
    // last_event_at is fresh → not idle either
    e.trigger_watchdog().await;
    // Small wait to let watchdog execute; session should NOT have been reaped.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "running",
        "parked session must not be wall-reaped"
    );
    // Clean up
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_ttl_reaps_parked_session_as_done() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // idle_ttl_ms=500ms
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_ttl_ms: Some(500),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Mark as parked and back-date last_event_at by 60 seconds.
    e.test_set_awaiting(&id, true);
    e.test_set_last_event_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    // Session should be reaped as "done" (no error).
    wait_status(&e, &id, "done").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_ttl_reaps_parked_session_as_failed_when_error_present() {
    // The idle-TTL reaper marks a parked session 'done' UNLESS it already carries an error,
    // in which case it must be 'failed'. The 'failed' branch had no coverage.
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_ttl_ms: Some(500),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Give the session a prior error so the reaper takes the 'failed' branch.
    e.0.store
        .update(
            &id,
            SessionPatch {
                error: Some(Some("prior turn error".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.test_set_awaiting(&id, true);
    e.test_set_last_event_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    // Let the reap + process exit settle, then assert it stuck at 'failed' with the error kept.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "failed",
        "errored parked session must reap as 'failed', got {:?}",
        s.status
    );
    assert_eq!(
        s.error.as_deref(),
        Some("prior turn error"),
        "the prior error must be preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_ttl_reaped_parked_stays_done_after_process_exits() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            idle_ttl_ms: Some(500),
            ..Default::default()
        },
    )
    .await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    e.test_set_awaiting(&id, true);
    e.test_set_last_event_at(&id, e.now() - 60_000);
    e.trigger_watchdog().await;
    // Wait for the process to actually exit (on_exit runs after stop()).
    // Status must remain "done" after exit, not flip back to "failed".
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let s = e.get(&id).await.unwrap();
    // After the TTL reap, on_exit sees cur.status="done" → keeps "done".
    assert!(
        s.status == "done",
        "status should be 'done' after idle-TTL reap, got {:?}",
        s.status
    );
    assert!(
        s.error.is_none(),
        "error should be None after idle-TTL reap: {:?}",
        s.error
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn honors_kill_during_worktree_sync_window_no_spawn() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered2 = entered.clone();
    let sync_fn: SyncFn = Arc::new(move |_p| {
        let rx = rx.clone();
        let entered = entered2.clone();
        Box::pin(async move {
            entered.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(r) = rx.lock().await.take() {
                let _ = r.await;
            }
        })
    });
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            sync_fn: Some(sync_fn),
            ..Default::default()
        },
    )
    .await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    // Wait until the sync_fn has been entered.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !entered.load(std::sync::atomic::Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for sync_fn to be entered");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Kill during the blocked sync window.
    e.kill(&id).await;
    // Release start().
    let _ = tx.send(());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "killed",
        "session should be killed after kill-during-sync"
    );
    // No claude process should have been spawned (session is not in running map).
    assert!(
        !e.0.state.lock().running.contains_key(&id),
        "no claude process should have been spawned after kill-during-sync"
    );
}

/// interrupt clears pending_ask but does not change status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_leaves_session_alive_and_clears_pending_ask() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        },
    )
    .await;
    let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let id = e
        .submit_session(
            vec![],
            vec![],
            "t".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    let r = results.clone();
    let _unsub = e.subscribe(
        &id,
        Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }),
    );
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
    // Interrupt must not kill the process.
    e.interrupt(&id);
    // interrupt("nope") is a no-op.
    e.interrupt("nope");
    // Status must still be running.
    let s = e.get(&id).await.unwrap();
    assert_eq!(s.status, "running", "interrupt must not change status");
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// kill() while the turn is still in start() (registered in `starting`, not yet in
/// `running`): the kill takes the not-running branch (drops from queue / pending→killed),
/// and start() must honor the kill during its sync window and NOT spawn a process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_during_starting_before_running_map_populated_no_spawn() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    // A sync_fn we can block on lets us hold start() open in the window AFTER `starting`
    // is set but BEFORE attach() populates `running`.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered2 = entered.clone();
    let sync_fn: SyncFn = Arc::new(move |_p| {
        let rx = rx.clone();
        let entered = entered2.clone();
        Box::pin(async move {
            entered.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(r) = rx.lock().await.take() {
                let _ = r.await;
            }
        })
    });
    let e = make_engine(
        &src,
        EngineOverrides {
            sync_fn: Some(sync_fn),
            ..Default::default()
        },
    )
    .await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    // Wait until start() is in the sync window: `starting` is populated, `running` is not.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !entered.load(std::sync::atomic::Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for start() to enter sync");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    {
        let st = e.0.state.lock();
        assert!(
            st.starting.contains(&id),
            "precondition: session is in `starting`"
        );
        assert!(
            !st.running.contains_key(&id),
            "precondition: not yet in `running`"
        );
    }
    // kill() here must take the not-running branch (run_handle is None).
    e.kill(&id).await;
    // Release start() — it should observe status=killed after sync and NOT spawn.
    let _ = tx.send(());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "killed",
        "kill while starting must stick as killed"
    );
    assert!(
        !e.0.state.lock().running.contains_key(&id),
        "no process should be spawned when killed during the starting window"
    );
}

/// interrupt() on a parked/awaiting session (after a Result, awaiting_input=true) clears
/// pending_ask and forwards an interrupt to the live run WITHOUT changing status — the
/// session stays running+parked, then a follow_up can still inject the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_on_parked_awaiting_session_keeps_it_alive() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        },
    )
    .await;
    let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let id = e
        .submit_session(
            vec![],
            vec![],
            "t1".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    let r = results.clone();
    let _unsub = e.subscribe(
        &id,
        Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }),
    );
    // Wait until the session has parked after its first result.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
    wait_until(|| {
        let st = e.0.state.lock();
        st.awaiting.get(&id) == Some(&true)
    })
    .await;
    // Seed a pending_ask to prove interrupt clears it on a parked session.
    e.0.state.lock().pending_ask.insert(id.clone());
    e.interrupt(&id);
    assert!(
        !e.0.state.lock().pending_ask.contains(&id),
        "interrupt must clear pending_ask even on a parked session"
    );
    // Status unchanged: still running (parked), not killed/failed.
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "running",
        "interrupt on parked session must not change status"
    );
    assert_eq!(
        s.awaiting_input,
        Some(true),
        "still parked/awaiting after interrupt"
    );
    // The live process survives → a follow_up still injects another turn.
    assert!(
        e.follow_up(&id, "t2", true, None, None, None).await.is_ok(),
        "session must still be live-injectable after interrupting a parked turn"
    );
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}
