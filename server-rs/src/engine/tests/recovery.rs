use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_graceful_detaches_running_run_kill_stops_it() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());

    // Graceful path: close_with(false) must NOT stop the in-flight claude run — it
    // detaches, leaving the child alive to be finalized by the next boot's recover().
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit("demo", "slow", slow.clone()).await.unwrap();
    wait_status(&e, &id, "running").await;
    let run =
        e.0.state
            .lock()
            .running
            .get(&id)
            .map(|t| t.run.clone())
            .expect("running turn");
    assert!(run.is_active(), "precondition: claude is running");
    e.close_with(false);
    assert!(
        run.is_active(),
        "graceful close (kill_running=false) must leave the run active"
    );
    run.stop(); // cleanup: don't leak the 120s fake sleep

    // Kill path: close_with(true) stops the in-flight run.
    let e2 = make_engine(&src, EngineOverrides::default()).await;
    let id2 = e2.submit("demo", "slow", slow).await.unwrap();
    wait_status(&e2, &id2, "running").await;
    let run2 =
        e2.0.state
            .lock()
            .running
            .get(&id2)
            .map(|t| t.run.clone())
            .expect("running turn 2");
    e2.close_with(true);
    // Generous deadline for loaded CI runners (kill + group-reap can take seconds there).
    let mut stopped = false;
    for _ in 0..375 {
        if !run2.is_active() {
            stopped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    assert!(stopped, "kill close (kill_running=true) must stop the run");
}

// ── Task 8: recover tests ──────────────────────────────────

/// Recover: a session that was "running" with a successful result → done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_finalizes_running_as_done_when_log_ended_in_result() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "s1".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("s1").to_string_lossy().into()),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "s1",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Append a successful result event
        store
            .append_log(
                "s1",
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            )
            .await
            .unwrap();
    } // store dropped
    let e = engine_from(&work).await;
    assert_eq!(
        e.get("s1").await.unwrap().status,
        "done",
        "running session with success result should recover as done"
    );
}

/// Recover: a session that was "running" with no result event → failed/interrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_finalizes_running_as_failed_interrupted_when_no_result() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "s2".into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "s2",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Append a non-result event (no result)
        store
            .append_log("s2", r#"{"type":"text","text":"partial output"}"#)
            .await
            .unwrap();
    }
    let e = engine_from(&work).await;
    let s = e.get("s2").await.unwrap();
    assert_eq!(s.status, "failed", "should be failed");
    assert!(
        s.error.as_deref().unwrap_or("").contains("interrupted"),
        "error should mention interrupted: {:?}",
        s.error
    );
    assert_eq!(s.error_kind.as_deref(), Some("interrupted"));
}

/// Recover: a session that was "running" with an error result → failed with classifed errorKind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_finalizes_running_as_failed_with_real_reason_on_error_result() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "s3".into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "s3",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Error result with usage-limit text
        store.append_log("s3", r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your session limit"}"#).await.unwrap();
    }
    let e = engine_from(&work).await;
    let s = e.get("s3").await.unwrap();
    assert_eq!(s.status, "failed");
    assert_eq!(
        s.error_kind.as_deref(),
        Some("usage_limit"),
        "should classify as usage_limit"
    );
    assert!(
        s.error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("session limit"),
        "error text should include 'session limit': {:?}",
        s.error
    );
}

/// Recover: a "pending" session gets re-enqueued and runs to done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_requeues_pending_and_runs_to_done() {
    let work = tmp();
    // We need a worktree_path that exists so start() can proceed.
    // Use a no-repo session (worktree_path=Some(session dir), repos=[]).
    let session_dir = work.join("worktrees").join("sp1");
    std::fs::create_dir_all(&session_dir).unwrap();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "sp1".into(),
                prompt: "go".into(),
                worktree_path: Some(session_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            })
            .await
            .unwrap();
        // Leave as "pending" (default status from create is "pending")
    }
    let e = engine_from(&work).await;
    // The pending session should get re-queued and run to done.
    wait_status(&e, "sp1", "done").await;
}

/// Recover: a pending session with no worktree_path → fails gracefully without panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_fails_unstartable_pending_without_crashing() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "sbad".into(),
                prompt: "go".into(),
                worktree_path: None, // no worktree → start() returns Err
                ..Default::default()
            })
            .await
            .unwrap();
        // Leave as "pending"
    }
    let e = engine_from(&work).await;
    // Should not panic; should eventually land in "failed"
    wait_status(&e, "sbad", "failed").await;
    let s = e.get("sbad").await.unwrap();
    assert_eq!(s.status, "failed");
}

/// Recover: one bad pending + one good pending → good reaches done, bad reaches failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_keeps_draining_after_one_fails_to_start() {
    let work = tmp();
    let good_dir = work.join("worktrees").join("sgood");
    std::fs::create_dir_all(&good_dir).unwrap();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        // bad: no worktree
        store
            .create(CreateInput {
                id: "sbad2".into(),
                prompt: "go".into(),
                worktree_path: None,
                ..Default::default()
            })
            .await
            .unwrap();
        // good: has a worktree dir
        store
            .create(CreateInput {
                id: "sgood".into(),
                prompt: "go".into(),
                worktree_path: Some(good_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let e = engine_from(&work).await;
    wait_status(&e, "sbad2", "failed").await;
    wait_status(&e, "sgood", "done").await;
}

/// Recover MUST restore the REAL turn-end time into endedAt, not the boot
/// wall-clock. Stamping `now` was the root cause of the spurious unread-dot
/// bug: every restart advanced endedAt past the client's lastReadAt, so the
/// dot re-lit. Here a running session whose transcript carries an
/// agentic_prompt at=1000 plus a success result recovers to done with
/// endedAt == 1000 (the turn-start fallback), never a fresh now() (~1.7e12).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_preserves_real_end_time_not_boot_clock() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "se1".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("se1").to_string_lossy().into()),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "se1",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .append_log("se1", r#"{"type":"agentic_prompt","text":"p","at":1000}"#)
            .await
            .unwrap();
        store
            .append_log(
                "se1",
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            )
            .await
            .unwrap();
    }
    let e = engine_from(&work).await;
    let s = e.get("se1").await.unwrap();
    assert_eq!(s.status, "done", "success result recovers as done");
    assert_eq!(
        s.ended_at,
        Some(1000),
        "recover must restore the real turn time (prompt-at fallback), not boot now()"
    );
}

/// When the lifecycle sidecar recorded a TurnEnded, recover restores THAT
/// authoritative timestamp, in preference to the transcript prompt-at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_prefers_lifecycle_turn_ended_at() {
    let work = tmp();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "se2".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("se2").to_string_lossy().into()),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "se2",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .append_log("se2", r#"{"type":"agentic_prompt","text":"p","at":1000}"#)
            .await
            .unwrap();
        store
            .append_log(
                "se2",
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            )
            .await
            .unwrap();
        store
            .append_lifecycle(
                "se2",
                &crate::engine::lifecycle::LifecycleEvent::TurnEnded {
                    at: 2000,
                    outcome: crate::engine::lifecycle::TurnOutcome::Success,
                    cost_usd: None,
                    duration_ms: None,
                },
            )
            .await
            .unwrap();
    }
    let e = engine_from(&work).await;
    let s = e.get("se2").await.unwrap();
    assert_eq!(s.status, "done");
    assert_eq!(
        s.ended_at,
        Some(2000),
        "recover must prefer the authoritative lifecycle TurnEnded.at"
    );
}

// ── Task 8: reconcile_worktrees tests ─────────────────────

/// reconcile_worktrees: orphan dirs are removed; live session dirs are kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_removes_orphan_worktree_dirs_keeps_live() {
    let work = tmp();
    let wt_root = work.join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    // Create a session in the store.
    let live_dir = wt_root.join("live-sess");
    std::fs::create_dir_all(&live_dir).unwrap();
    // Create an orphan dir with no matching session.
    let orphan_dir = wt_root.join("orphan-dir");
    std::fs::create_dir_all(&orphan_dir).unwrap();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        store
            .create(CreateInput {
                id: "live-sess".into(),
                prompt: "p".into(),
                worktree_path: Some(live_dir.to_string_lossy().into()),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "live-sess",
                SessionPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let _e = engine_from(&work).await;
    assert!(live_dir.exists(), "live session dir must be kept");
    assert!(
        !orphan_dir.exists(),
        "orphan dir must be removed by reconcile"
    );
}

/// SAFETY guard: an EMPTY store next to a populated worktrees_root is a misconfiguration (a fresh/wrong
/// DB pointed at a live worktrees dir), NOT "every worktree is an orphan". reconcile_worktrees must
/// delete NOTHING — this footgun once wiped live session worktrees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_deletes_nothing_when_store_is_empty() {
    let work = tmp();
    let wt_root = work.join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let looks_orphan = wt_root.join("looks-orphan-but-db-is-empty");
    std::fs::create_dir_all(&looks_orphan).unwrap();
    // engine_from boots with a fresh (empty) DB → reconcile_worktrees must bail, not delete.
    let _e = engine_from(&work).await;
    assert!(
        looks_orphan.exists(),
        "with an empty store reconcile must delete nothing"
    );
}

// ── Task 8: discard tests ─────────────────────────────────

/// discard: sets worktree_state=discarded; branch is removed from the source repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discard_deletes_branch_and_sets_discarded() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;
    // Verify worktree_state is live before discard.
    assert_eq!(e.get(&id).await.unwrap().worktree_state, "live");
    e.discard(&id).await.unwrap();
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.worktree_state, "discarded",
        "worktree_state should be discarded"
    );
    // The worktree dir should be gone.
    if let Some(ref wt) = s.worktree_path {
        assert!(
            !std::path::Path::new(wt).exists(),
            "worktree dir should be removed after discard"
        );
    }
}

/// discard: rejects a running session (busy); rejects an already-discarded session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discard_rejects_running_and_already_cleaned() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
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
    wait_status(&e, &id, "done").await;
    // First discard succeeds.
    e.discard(&id).await.unwrap();
    // Second discard on already-discarded → error mentioning "cleaned" or similar.
    let err = e.discard(&id).await.unwrap_err().to_string();
    assert!(
        err.contains("cleaned") || err.contains("discarded"),
        "second discard must fail with a cleaned/discarded message: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discard_rejects_busy_running_session() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    let err = e.discard(&id).await.unwrap_err().to_string();
    assert!(
        err.contains("busy") || err.contains("running"),
        "discard of running session must fail with busy: {err}"
    );
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

// ── Task 8: delete_session tests ─────────────────────────

/// delete_session: removes a finished session row and worktree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_session_removes_finished_record_and_worktree() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
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
    wait_status(&e, &id, "done").await;
    let wt_path = e.get(&id).await.unwrap().worktree_path.clone();
    e.delete_session(&id, false).await.unwrap();
    assert!(
        e.get(&id).await.is_none(),
        "session row should be gone after delete"
    );
    if let Some(ref wt) = wt_path {
        assert!(
            !std::path::Path::new(wt).exists(),
            "worktree dir should be removed after delete"
        );
    }
}

/// delete_session: refuses to delete a running session without force.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_session_refuses_running_without_force() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    let err = e.delete_session(&id, false).await.unwrap_err().to_string();
    assert!(
        err.contains("busy") || err.contains("running"),
        "delete without force must fail with busy: {err}"
    );
    e.kill(&id).await;
    wait_status(&e, &id, "killed").await;
}

/// delete_session with force=true kills a running session and removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_session_force_kills_and_removes_running() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let mut env = HashMap::new();
    env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
    let id = e
        .submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    let wt_path = e.get(&id).await.unwrap().worktree_path.clone();
    e.delete_session(&id, true).await.unwrap();
    assert!(
        e.get(&id).await.is_none(),
        "session row should be gone after force delete"
    );
    if let Some(ref wt) = wt_path {
        assert!(
            !std::path::Path::new(wt).exists(),
            "worktree dir should be removed after force delete"
        );
    }
}

/// delete_session with force=true removes a queued/pending session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_session_force_removes_queued_pending() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // max_concurrent=1: slow first occupies slot; second stays pending.
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        },
    )
    .await;
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
    let id2 = e
        .submit_session(
            vec![],
            vec![],
            "target".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id2, "pending").await;
    let wt_path2 = e.get(&id2).await.unwrap().worktree_path.clone();
    e.delete_session(&id2, true).await.unwrap();
    assert!(
        e.get(&id2).await.is_none(),
        "queued session row should be gone after force delete"
    );
    if let Some(ref wt) = wt_path2 {
        assert!(
            !std::path::Path::new(wt).exists(),
            "queued session worktree should be removed"
        );
    }
    // Cleanup blocker.
    e.kill(&_blocker).await;
    wait_status(&e, &_blocker, "killed").await;
}

/// recover() over a mixed table: a running-with-success row finalizes to done, a
/// running-with-no-result row finalizes to failed/interrupted, an already-done row is
/// untouched, and a pending row is re-enqueued and runs to done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recover_handles_mixed_running_pending_done_rows() {
    let work = tmp();
    let pend_dir = work.join("worktrees").join("rp");
    std::fs::create_dir_all(&pend_dir).unwrap();
    {
        let store = Store::open(work.join("db.sqlite"), work.join("logs"))
            .await
            .unwrap();
        // running + success result → recover to done
        store
            .create(CreateInput {
                id: "rr_ok".into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "rr_ok",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .append_log(
                "rr_ok",
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            )
            .await
            .unwrap();
        // running + no result → recover to failed/interrupted
        store
            .create(CreateInput {
                id: "rr_int".into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "rr_int",
                SessionPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .append_log("rr_int", r#"{"type":"text","text":"partial"}"#)
            .await
            .unwrap();
        // already done → must be left untouched (no spurious re-finalize)
        store
            .create(CreateInput {
                id: "rr_done".into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                "rr_done",
                SessionPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // pending with a real worktree dir → re-enqueued and runs to done
        store
            .create(CreateInput {
                id: "rp".into(),
                prompt: "go".into(),
                worktree_path: Some(pend_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let e = engine_from(&work).await;
    assert_eq!(
        e.get("rr_ok").await.unwrap().status,
        "done",
        "running+success recovers to done"
    );
    let int = e.get("rr_int").await.unwrap();
    assert_eq!(int.status, "failed", "running+no-result recovers to failed");
    assert_eq!(
        int.error_kind.as_deref(),
        Some("interrupted"),
        "running+no-result errorKind is interrupted"
    );
    assert_eq!(
        e.get("rr_done").await.unwrap().status,
        "done",
        "already-done row is left untouched"
    );
    // The pending row gets re-enqueued and runs.
    wait_status(&e, "rp", "done").await;
}

/// New-request attachments: a file staged before the session existed is moved into the new
/// single-repo session's `uploads/` dir (under `session_dir/<repo>/uploads`, matching the
/// per-session upload route's cwd), and the staging token dir is cleaned up afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_adopts_staged_uploads_into_session_uploads_dir() {
    let src = tmp();
    make_temp_git_repo_in(&src, "repoA");
    let e = make_engine(&src, EngineOverrides::default()).await;
    // Stage a file exactly as `POST /api/uploads` does: <worktrees_root>/.staging/<token>/<name>.
    let token = new_staging_token();
    let token_dir = staging_root(&e.0.cfg.worktrees_root).join(&token);
    std::fs::create_dir_all(&token_dir).unwrap();
    std::fs::write(token_dir.join("shot.png"), b"PNGDATA").unwrap();

    let id = e
        .submit_session(
            vec!["repoA".into()],
            vec![],
            "look at [attached: uploads/shot.png]".into(),
            HashMap::new(),
            SubmitMeta {
                staged_uploads: vec![StagedUpload {
                    token: token.clone(),
                    name: "shot.png".into(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let s = e.get(&id).await.unwrap();
    let wt_dir = std::path::Path::new(
        s.worktree_path
            .as_ref()
            .expect("session has a worktree_path"),
    );
    // Single-repo cwd is session_dir/<repo>, so adopted uploads live there.
    let adopted = wt_dir.join("repoA").join("uploads").join("shot.png");
    let bytes = std::fs::read(&adopted).expect("staged file must be adopted into uploads/");
    assert_eq!(bytes, b"PNGDATA", "adopted file keeps its bytes");
    assert!(
        !token_dir.exists(),
        "staging token dir must be removed after adoption"
    );
}

#[tokio::test]
async fn forget_session_clears_reconcile_lock() {
    // Teardown must drop the per-session reconcile lock, or reconcile_locks grows without bound
    // as sessions are created and deleted (a memory leak).
    let e = test_engine().await;
    e.0.reconcile_locks.lock().insert(
        "sess-x".to_string(),
        std::sync::Arc::new(tokio::sync::Mutex::new(())),
    );
    assert!(e.0.reconcile_locks.lock().contains_key("sess-x"));
    e.forget_session("sess-x");
    assert!(
        !e.0.reconcile_locks.lock().contains_key("sess-x"),
        "forget_session must remove the session's reconcile lock"
    );
}
