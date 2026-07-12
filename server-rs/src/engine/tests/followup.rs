use super::*;

/// Streaming session: one persistent process, each follow_up injects over stdin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_one_process_goes_idle_then_followup_injects_over_stdin() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            max_concurrent: Some(2),
            ..Default::default()
        },
    )
    .await;

    let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first turn".into(),
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

    // Wait for first result.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;

    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "running",
        "streaming session stays running after first result"
    );
    assert_eq!(
        s.awaiting_input,
        Some(true),
        "awaiting_input=true after first result"
    );
    assert_eq!(s.claude_session_id.as_deref(), Some("fake-stream-1"));
    assert!(
        s.cost_usd.unwrap_or(0.0) > 0.0,
        "cost captured after first result"
    );

    // Inject second turn.
    let since = e
        .follow_up(&id, "second turn", true, None, None, None)
        .await
        .unwrap();
    let _ = since;

    // awaiting_input must flip false immediately after inject.
    let s2 = e.get(&id).await.unwrap();
    assert_eq!(
        s2.awaiting_input,
        Some(false),
        "awaiting_input=false right after follow_up inject"
    );

    // Wait for second result.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;

    let s3 = e.get(&id).await.unwrap();
    assert_eq!(
        s3.awaiting_input,
        Some(true),
        "awaiting_input=true after second result"
    );
    assert_eq!(
        s3.activity.as_ref().map(|a| a.turns),
        Some(2),
        "activity.turns=2"
    );

    // Check both agentic_prompt entries in log.
    let prompts: Vec<String> = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|o| o["type"] == "agentic_prompt")
        .map(|o| o["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        prompts,
        vec!["first turn", "second turn"],
        "both prompts in log"
    );

    // Clean up.
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// follow_up on a live session succeeds (no error).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_does_not_error_on_live_session() {
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
            "first".into(),
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
    assert!(
        e.follow_up(&id, "second", true, None, None, None)
            .await
            .is_ok(),
        "follow_up on live session must succeed"
    );
    // Cleanup.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// follow_up stamps lastUserMessageAt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_bumps_last_user_message_at_on_live_inject() {
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
            "first".into(),
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
    let t1 = e.get(&id).await.unwrap().last_user_message_at;
    // Small delay to ensure clock advances.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    e.follow_up(&id, "second", true, None, None, None)
        .await
        .unwrap();
    let t2 = e.get(&id).await.unwrap().last_user_message_at;
    assert!(t2 >= t1, "lastUserMessageAt must not go backwards");
    // Cleanup.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// follow_up on a finished session re-queues it, accumulates cost, same worktree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_reruns_finished_session_accumulates_cost() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // Use default fake-sdk-bridge-ok.sh (one-shot, cost 0.0042 each turn).
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s1 = e.get(&id).await.unwrap();
    let wt = s1.worktree_path.clone();
    let log_len1 = e.get_log(&id).len();

    // Follow-up: re-queues for a new turn.
    e.follow_up(&id, "second", true, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;

    let s2 = e.get(&id).await.unwrap();
    assert_eq!(s2.worktree_path, wt, "same worktree_path after follow-up");
    assert!(
        s2.cost_usd.unwrap_or(0.0) > s1.cost_usd.unwrap_or(0.0),
        "cost should accumulate across turns"
    );
    assert!(
        e.get_log(&id).len() > log_len1,
        "log grew after follow-up turn"
    );
}

/// follow_up retitles only when set_title=true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_retitles_only_when_set_title_true() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "original".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;

    // set_title=true → prompt updates.
    e.follow_up(&id, "second", true, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    assert_eq!(
        e.get(&id).await.unwrap().prompt,
        "second",
        "prompt should update when set_title=true"
    );

    // set_title=false → prompt stays.
    e.follow_up(&id, "third", false, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    assert_eq!(
        e.get(&id).await.unwrap().prompt,
        "second",
        "prompt should not change when set_title=false"
    );
}

/// follow_up with set_title omitted (i.e. None) does NOT retitle — the title
/// is owned by the first submit and follow-ups leave it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_default_does_not_retitle() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "original".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;

    // After submit, the prompt may have been replaced by generate_title;
    // capture it for comparison.
    let original = e.get(&id).await.unwrap().prompt;

    // set_title omitted → prompt must NOT change to "second".
    e.follow_up(&id, "second", false, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    assert_eq!(
        e.get(&id).await.unwrap().prompt,
        original,
        "default behaviour must not retitle"
    );
}

/// follow_up with no claudeSessionId (noinit fixture) still succeeds and runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_retry_when_no_claude_session_id() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // fake-sdk-bridge-noinit.sh emits no init event → claude_session_id stays None.
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-noinit.sh")),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "first".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    // noinit results in exit 1 (no result seen) → "failed"
    for _ in 0..250 {
        let st = e.get(&id).await.map(|s| s.status).unwrap_or_default();
        if matches!(st.as_str(), "done" | "failed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // claude_session_id should be None (no init emitted).
    let s = e.get(&id).await.unwrap();
    assert!(
        s.claude_session_id.is_none(),
        "claude_session_id must be None with noinit fixture"
    );
    // follow_up should still succeed (resume_session_id=None → fresh start).
    assert!(
        e.follow_up(&id, "retry me", true, None, None, None)
            .await
            .is_ok(),
        "follow_up must succeed even without claude_session_id"
    );
}

/// follow_up on an errored (failed) session clears error/errorKind on the queued branch, not
/// just when the resumed turn eventually spawns. Drives the session to `failed`/`usage_limit`,
/// calls follow_up, and asserts the error fields are None the moment the queued branch writes
/// its patch (the row is then `pending`, awaiting the resumed turn). Without the queued-branch
/// clear (mod.rs follow_up queued branch) the Android client's
/// `hasError(status="pending", errorKind="usage_limit")` stays true during the pending window
/// and the error banner lingers looking like the recovery didn't take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_clears_error_in_queued_branch() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-error.sh")),
            ..Default::default()
        },
    )
    .await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "failed").await;
    // Sanity: first turn really did fail with a usage_limit errorKind — this is the state we
    // expect the queued branch to clear.
    let s_failed = e.get(&id).await.unwrap();
    assert_eq!(s_failed.status, "failed");
    assert_eq!(s_failed.error_kind.as_deref(), Some("usage_limit"));
    assert!(s_failed.error.as_deref().is_some_and(|m| !m.is_empty()));

    // Resume via the queued branch. follow_up returns once the queued-branch patch (status =
    // pending, error/errorKind/exitCode = None) has been written. The resumed turn will be
    // picked up by the pump on its own clock — we don't wait for it; the invariant under test
    // is that the queued branch itself cleared the error fields synchronously.
    e.follow_up(&id, "continue", true, None, None, None)
        .await
        .unwrap();
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.status, "pending",
        "queued branch should mark the session pending"
    );
    assert!(
        s.error.is_none(),
        "error must be cleared by the queued branch, got {:?}",
        s.error
    );
    assert!(
        s.error_kind.is_none(),
        "error_kind must be cleared by the queued branch, got {:?}",
        s.error_kind
    );
}

/// follow_up on unknown session returns Err.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_unknown_session_errors() {
    let e = test_engine().await;
    assert!(
        e.follow_up("nope", "x", true, None, None, None)
            .await
            .is_err(),
        "follow_up on unknown session must error"
    );
}

/// follow_up while another follow_up is already queued → Err("session busy").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_second_followup_while_first_queued() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // max_concurrent=1 so second follow-up can't start immediately.
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        },
    )
    .await;
    // Saturate the slot with a slow first session.
    let mut slow_env = HashMap::new();
    slow_env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let _blocker = e
        .submit_session(
            vec![],
            vec![],
            "blocker".into(),
            slow_env,
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &_blocker, "running").await;

    // Now submit the target session (will be queued, then done after blocker).
    let id = e
        .submit_session(
            vec![],
            vec![],
            "target".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    // Wait for target to reach pending (it's in queue).
    wait_status(&e, &id, "pending").await;

    // The target is pending/queued. A follow_up queues another turn.
    // But is_busy returns true because the item is in the queue → Err.
    let result = e.follow_up(&id, "extra turn", true, None, None, None).await;
    assert!(
        result.is_err(),
        "second follow_up must fail when session is busy/queued"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("busy") || err.contains("pending"),
        "error should mention busy or pending, got: {err}"
    );
}

/// Fix B: a session re-entering flight must drop the PRIOR turn's endedAt,
/// so the client's unread-dot predicate (endedAt > lastReadAt) can't re-fire
/// on a stale completion time. Both the FollowUpQueued (resume) and Start
/// edges clear it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transition_start_and_followup_clear_ended_at() {
    use crate::engine::status::SessionStatus;
    use crate::engine::transition::TransitionReason;
    let work = tmp();
    let e = engine_from(&work).await; // recover runs on an empty db — no-op
                                      // Seed a done session with a stale endedAt AFTER recover so it is untouched.
    e.0.store
        .create(CreateInput {
            id: "txb".into(),
            prompt: "p".into(),
            worktree_path: Some(work.join("worktrees").join("txb").to_string_lossy().into()),
            ..Default::default()
        })
        .await
        .unwrap();
    e.0.store
        .update(
            "txb",
            SessionPatch {
                status: Some("done".into()),
                ended_at: Some(Some(5000)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        e.get("txb").await.unwrap().ended_at,
        Some(5000),
        "stale endedAt seeded"
    );

    // done -> pending (FollowUpQueued) clears endedAt.
    e.transition(
        "txb",
        SessionStatus::Pending,
        TransitionReason::FollowUpQueued,
    )
    .await
    .unwrap();
    assert_eq!(
        e.get("txb").await.unwrap().ended_at,
        None,
        "FollowUpQueued must clear endedAt"
    );

    // Re-stamp a stale endedAt while pending, then pending -> running (Start) clears it.
    e.0.store
        .update(
            "txb",
            SessionPatch {
                ended_at: Some(Some(7000)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.transition("txb", SessionStatus::Running, TransitionReason::Start)
        .await
        .unwrap();
    assert_eq!(
        e.get("txb").await.unwrap().ended_at,
        None,
        "Start must clear endedAt"
    );
}

/// Two live follow_up injects in a row over one persistent process produce three total
/// turns (initial + 2 injects), each logging an agentic_prompt marker in order, and the
/// activity turn counter reaching 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_live_inject_twice_in_a_row_streams_three_turns() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            max_concurrent: Some(2),
            ..Default::default()
        },
    )
    .await;
    let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let id = e
        .submit_session(
            vec![],
            vec![],
            "turn one".into(),
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
    // Turn 1 result.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
    // First inject (turn 2).
    assert!(e
        .follow_up(&id, "turn two", true, None, None, None)
        .await
        .is_ok());
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
    // Second inject right after (turn 3) — same live process, no re-queue.
    assert!(e
        .follow_up(&id, "turn three", true, None, None, None)
        .await
        .is_ok());
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 3).await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.activity.as_ref().map(|a| a.turns),
        Some(3),
        "three turns total after two consecutive live injects"
    );
    // All three prompts present in log order.
    let prompts: Vec<String> = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|o| o["type"] == "agentic_prompt")
        .map(|o| o["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        prompts,
        vec!["turn one", "turn two", "turn three"],
        "all three prompts logged in order"
    );
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

#[tokio::test]
async fn follow_up_attaches_per_turn_overrides_to_queued_item() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta {
                model: Some("claude-opus-4-8".into()),
                effort: None,
                mode: None,
                permission_mode: None,
                hidden_skills: vec![],
                hidden_plugins: vec![],
                hidden_mcp_servers: vec![],
                extra_mcp_servers: vec![],
                claude_md: None,
                staged_uploads: vec![],
                forced_on_plugins: vec![],
                forced_on_skills: vec![],
                forced_on_mcp_servers: vec![],
            },
        )
        .await
        .unwrap();

    // Wait for the initial turn to finish so the session is idle when we call follow_up.
    wait_status(&e, &id, "done").await;

    e.follow_up(
        &id,
        "next turn".into(),
        true,
        Some("claude-haiku-4-5-20251001".into()),
        Some("low".into()),
        None,
    )
    .await
    .unwrap();

    let pushed = {
        let st = e.0.state.lock();
        st.queue
            .iter()
            .find(|q| q.id == id && q.prompt == "next turn")
            .cloned()
            .expect("follow-up should have enqueued a QueueItem")
    };
    assert_eq!(pushed.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    assert_eq!(pushed.effort.as_deref(), Some("low"));
}
