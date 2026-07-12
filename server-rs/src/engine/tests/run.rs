use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_then_unsub_drops_the_entry() {
    let e = test_engine().await;
    let unsub = e.subscribe("ghost", Box::new(|_| {}));
    assert!(e.0.state.lock().subs.contains_key("ghost"));
    unsub();
    assert!(
        !e.0.state.lock().subs.contains_key("ghost"),
        "empty sub set must be dropped (no Map leak)"
    );
}

// ── Task 5 tests (happy path + errors + lifecycle logs) ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_a_session_to_done_capturing_session_id_cost_and_logs() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit("demo", "do something", HashMap::new())
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(s.claude_session_id.as_deref(), Some("fake-sess-123"));
    assert!(
        (s.cost_usd.unwrap_or(0.0) - 0.0042).abs() < 1e-9,
        "cost should be 0.0042, got {:?}",
        s.cost_usd
    );
    assert_eq!(s.exit_code, Some(0));
    assert!(!e.get_log(&id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_an_agentic_prompt_marker() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit("demo", "my first prompt", HashMap::new())
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let prompts: Vec<serde_json::Value> = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|o: &serde_json::Value| {
            o.get("type").and_then(|t| t.as_str()) == Some("agentic_prompt")
        })
        .collect();
    assert!(
        !prompts.is_empty(),
        "should have an agentic_prompt entry in log"
    );
    assert_eq!(prompts[0]["text"], "my first prompt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persists_agent_result_marker_only_for_spawned_agents() {
    // A turn that spawns a subagent (Agent → tu_agent) AND runs a plain tool (Bash → tu_bash),
    // then returns both tool_results. The engine must persist an `agent_result` marker for the
    // SPAWNED agent only — not for the plain Bash tool — so a reopened session restores the agent
    // card's body without bloating the log with every tool's output.
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-agentresult.sh")),
            ..Default::default()
        },
    )
    .await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;
    let markers: Vec<serde_json::Value> = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|o: &serde_json::Value| {
            o.get("type").and_then(|t| t.as_str()) == Some("agent_result")
        })
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one agent_result marker (the spawned agent, not Bash)"
    );
    assert_eq!(markers[0]["toolUseId"], "tu_agent");
    assert_eq!(markers[0]["text"], "AGENT OUTPUT");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_repersist_a_retailed_agent_result_marker() {
    // REGRESSION (runaway log growth): the engine persists a rendered `agent_result` marker for a
    // spawned subagent's result. That marker line is written to the SAME log the EventTailer reads,
    // and `parse_line` decodes an `{"type":"agent_result"}` line straight back into a
    // ClaudeEvent::AgentResult — so the marker re-enters on_event. If on_event re-persists it, each
    // cycle appends another marker that is itself re-tailed: an unbounded feedback loop (observed in
    // prod — a single subagent result echoed tens of thousands of times, logs growing to hundreds of
    // MB with no claude process running). on_event must persist ONLY for a genuine `user`
    // tool_result, never for an event decoded from an already-persisted `agent_result` marker.
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;

    // tu_agent is a genuine spawned subagent (recorded from an Agent event).
    e.on_event(
        &id,
        ClaudeEvent::Agent {
            agents: vec![crate::engine::stream::SpawnedAgent {
                id: "tu_agent".into(),
                agent_type: "Explore".into(),
                description: "d".into(),
            }],
            parent_tool_use_id: None,
            raw: serde_json::json!({}),
        },
    )
    .await;

    // 1) Genuine subagent tool_result (raw type = "user") → persists exactly one marker.
    e.on_event(
        &id,
        ClaudeEvent::AgentResult {
            tool_use_id: "tu_agent".into(),
            text: "AGENT OUTPUT".into(),
            raw: serde_json::json!({ "type": "user" }),
        },
    )
    .await;

    // 2) That marker is re-tailed and decoded back into an AgentResult whose raw IS the marker
    //    (type = "agent_result"). This MUST NOT append a second marker.
    e.on_event(&id, ClaudeEvent::AgentResult {
            tool_use_id: "tu_agent".into(),
            text: "AGENT OUTPUT".into(),
            raw: serde_json::json!({ "type": "agent_result", "toolUseId": "tu_agent", "text": "AGENT OUTPUT" }),
        }).await;

    let markers = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|o: &serde_json::Value| {
            o.get("type").and_then(|t| t.as_str()) == Some("agent_result")
        })
        .count();
    assert_eq!(
        markers, 1,
        "a re-tailed agent_result marker must not be re-persisted (feedback loop)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribers_receive_live_events() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let id = e.submit("demo", "p", HashMap::new()).await.unwrap();
    let g = got.clone();
    let _unsub = e.subscribe(
        &id,
        Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Text { .. }) {
                g.lock().push("text".into());
            }
        }),
    );
    wait_status(&e, &id, "done").await;
    assert!(!got.lock().is_empty(), "saw at least one text event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn respects_max_concurrent_second_stays_pending() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        },
    )
    .await;
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "2".into());
    let id1 = e.submit("demo", "first", slow).await.unwrap();
    let id2 = e.submit("demo", "second", HashMap::new()).await.unwrap();
    // Wait for id1 to be running
    wait_status(&e, &id1, "running").await;
    // id2 should still be pending
    assert_eq!(
        e.get(&id2).await.unwrap().status,
        "pending",
        "second session should be pending while first is running"
    );
    // Now wait for both to finish
    wait_status(&e, &id1, "done").await;
    wait_status(&e, &id2, "done").await;
    assert_eq!(e.get(&id1).await.unwrap().status, "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_session_returns_before_starting_the_turn() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec!["demo".into()],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    // Status should be pending immediately (deferred start).
    assert_eq!(e.get(&id).await.unwrap().status, "pending");
    wait_status(&e, &id, "done").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_submit_for_unknown_repo() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await; // clone_fn errors
    assert!(e.submit("nope", "p", HashMap::new()).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_limit_error_sets_failed_and_errorkind() {
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
    let s = e.get(&id).await.unwrap();
    assert!(
        s.error
            .unwrap_or_default()
            .to_lowercase()
            .contains("session limit"),
        "error should contain 'session limit'"
    );
    assert_eq!(s.error_kind.as_deref(), Some("usage_limit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashed_turn_is_not_mislabeled_as_usage_limit() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-crash.sh")),
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
    let s = e.get(&id).await.unwrap();
    let err = s.error.unwrap_or_default();
    assert!(!err.is_empty(), "error must not be empty for a crash");
    let limit_re = regex::Regex::new("(?i)limit|resets").unwrap();
    assert!(
        !limit_re.is_match(&err),
        "crash message must not contain 'limit' or 'resets': got {err:?}"
    );
    assert_eq!(s.error_kind.as_deref(), Some("crashed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_tagged_rate_limited() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-ratelimit.sh")),
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
    let s = e.get(&id).await.unwrap();
    assert_eq!(s.error_kind.as_deref(), Some("rate_limited"));
    assert!(
        s.error
            .unwrap_or_default()
            .to_lowercase()
            .contains("not your usage limit"),
        "error should contain the rate-limit message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emits_structured_turn_lifecycle_logs() {
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let logs = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let l = logs.clone();
    let e = make_engine(
        &src,
        EngineOverrides {
            log_fn: Some(Arc::new(move |r| l.lock().push(r))),
            ..Default::default()
        },
    )
    .await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;
    // turn_end is emitted during finalization, which can land a hair AFTER status flips to "done"
    // under load — wait for it so the assertions below don't race the log_fn.
    wait_until(|| {
        logs.lock()
            .iter()
            .any(|r| r["evt"] == "turn_end" && r["sessionId"] == id.as_str())
    })
    .await;
    let recs = logs.lock();
    let find = |evt: &str| {
        recs.iter()
            .find(|r| r["evt"] == evt && r["sessionId"] == id.as_str())
            .cloned()
    };
    let start = find("turn_start").expect("turn_start log record must be present");
    assert!(
        start.get("queueWaitMs").is_some(),
        "turn_start must have queueWaitMs"
    );
    assert!(start.get("max").is_some(), "turn_start must have max");
    let result = find("turn_result").expect("turn_result log record must be present");
    assert!(
        result.as_object().unwrap().contains_key("ttftMs"),
        "turn_result must have ttftMs key"
    );
    let end = find("turn_end").expect("turn_end log record must be present");
    assert_eq!(end["status"], "done", "turn_end status must be 'done'");
}

/// A later turn crash after a success must finalize as failed / errorKind=crashed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalizes_failed_when_later_turn_crashes_after_success() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream-crash2.sh")),
            ..Default::default()
        },
    )
    .await;
    let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let id = e
        .submit_session(
            vec![],
            vec![],
            "turn1".into(),
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
    // Wait for turn 1 result.
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
    // Inject turn 2 (crash2 fixture exits 1 on second read).
    e.follow_up(&id, "turn2", true, None, None, None)
        .await
        .unwrap();
    // Session should end as failed.
    wait_status(&e, &id, "failed").await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.error_kind.as_deref(),
        Some("crashed"),
        "errorKind should be crashed after crash on turn2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fires_push_fn_on_session_exit() {
    use parking_lot::Mutex;
    use std::sync::Arc;
    let src = tmp().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let b = bodies.clone();
    let push_fn: PushFn = Arc::new(move |v| b.lock().push(v));
    let engine = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            push_fn: Some(push_fn),
            ..Default::default()
        },
    )
    .await;
    // repos empty → pure-skill scratch session, runs immediately under the fake-bridge fixture.
    let id = engine
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            std::collections::HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    // Wait for the session to reach "done" (poll the store like the other engine tests).
    for _ in 0..250 {
        if engine.get(&id).await.map(|s| s.status) == Some("done".into()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Give the fire-and-forget push a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let got = bodies.lock();
    assert!(!got.is_empty(), "push_fn must fire when the turn finishes");
    assert_eq!(got[0]["sessionId"], id);
    assert_eq!(got[0]["status"], "done");
    // The turn-end (Result) push carries the session title so the notification names the session.
    assert_eq!(got[0]["title"], "go");
    // Exactly ONE push for result-then-exit: the turn-end hook announced the completion and
    // on_exit must skip its "done" push for an already-parked session (no double notification).
    assert_eq!(
        got.len(),
        1,
        "result+exit must not double-push, got: {:?}",
        *got
    );
}

/// Push errorText must include the crash-fallback message when a session fails
/// with no prior error (re-reads final session state after store.update to pick up the fallback).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_error_text_contains_crash_fallback_for_failed_with_no_error() {
    use parking_lot::Mutex;
    use std::sync::Arc;
    let src = tmp().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let b = bodies.clone();
    let push_fn: PushFn = Arc::new(move |v| b.lock().push(v));
    // fake-sdk-bridge-crash.sh exits 1 without a result event → status="failed", error=None
    // before the patch. The exit handler should then set the crash-fallback error and the push
    // body should include that text.
    let engine = make_engine(
        &src,
        EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-crash.sh")),
            max_concurrent: Some(1),
            push_fn: Some(push_fn),
            ..Default::default()
        },
    )
    .await;
    let id = engine
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            std::collections::HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    for _ in 0..250 {
        let s = engine.get(&id).await.map(|s| s.status);
        if s == Some("failed".into()) || s == Some("done".into()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Give the fire-and-forget push a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let got = bodies.lock();
    assert!(!got.is_empty(), "push_fn must fire on exit");
    let payload = &got[0];
    assert_eq!(
        payload["status"], "failed",
        "session must be failed, got {:?}",
        payload
    );
    let error_text = payload["errorText"].as_str().unwrap_or("");
    assert!(
        error_text.contains("interrupted")
            || error_text.contains("crashed")
            || error_text.contains("resume"),
        "errorText must contain crash-fallback message, got: {:?}",
        error_text
    );
}

/// max_concurrent=1: two queued sessions run in FIFO order — the second only starts
/// after the first finishes (its slot frees). Asserts the second is still pending while
/// the first runs, and reaches done only after the first does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_concurrent_one_runs_queue_in_fifo_and_starts_next_on_finish() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(
        &src,
        EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        },
    )
    .await;
    // First is slow so it holds the single slot while the second waits.
    let mut slow = HashMap::new();
    slow.insert("FAKE_CLAUDE_SLEEP".into(), "2".into());
    let id1 = e
        .submit_session(vec![], vec![], "first".into(), slow, SubmitMeta::default())
        .await
        .unwrap();
    let id2 = e
        .submit_session(
            vec![],
            vec![],
            "second".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    // First is the head of the queue → it starts; second must wait.
    wait_status(&e, &id1, "running").await;
    assert_eq!(
        e.get(&id2).await.unwrap().status,
        "pending",
        "second (FIFO tail) must stay pending while first holds the only slot"
    );
    // Second must not be in the running map while first occupies the slot.
    assert!(
        !e.0.state.lock().running.contains_key(&id2),
        "second must not be started before first frees its slot"
    );
    // Finishing the first must free the slot and start the second.
    wait_status(&e, &id1, "done").await;
    wait_status(&e, &id2, "done").await;
}

/// submit_session with multiple repos creates per-repo worktrees, writes the multi-repo
/// session guide (CLAUDE.md) into the session dir, and runs to done with cwd = session dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_with_multiple_repos_creates_worktrees_and_session_guide() {
    let src = tmp();
    make_temp_git_repo_in(&src, "repoA");
    make_temp_git_repo_in(&src, "repoB");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec!["repoA".into(), "repoB".into()],
            vec![],
            "do multi".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    assert_eq!(
        s.repos,
        vec!["repoA".to_string(), "repoB".to_string()],
        "both repos recorded on the session"
    );
    let wt = s
        .worktree_path
        .as_ref()
        .expect("multi-repo session has a worktree_path");
    let wt_dir = std::path::Path::new(wt);
    // Each repo got its own worktree subdir.
    assert!(
        wt_dir.join("repoA").exists(),
        "repoA worktree subdir must exist"
    );
    assert!(
        wt_dir.join("repoB").exists(),
        "repoB worktree subdir must exist"
    );
    // Multi-repo orientation guide written into the session dir (gated on wts.len() > 1).
    assert!(
        wt_dir.join("CLAUDE.md").exists(),
        "multi-repo session must get a CLAUDE.md orientation guide"
    );
    // Per-repo base shas captured.
    assert!(
        s.base_shas
            .get("repoA")
            .map(|o| o.is_some())
            .unwrap_or(false),
        "repoA base sha recorded"
    );
    assert!(
        s.base_shas
            .get("repoB")
            .map(|o| o.is_some())
            .unwrap_or(false),
        "repoB base sha recorded"
    );
}

/// submit_session with a custom CLAUDE.md (single repo) writes it into the session dir verbatim.
/// Single-repo gets no orientation guide, so the file is the user's content alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_with_custom_claude_md_writes_it_into_session_dir() {
    let src = tmp();
    make_temp_git_repo_in(&src, "repoA");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec!["repoA".into()],
            vec![],
            "do it".into(),
            HashMap::new(),
            SubmitMeta {
                claude_md: Some("Run my tests before committing.".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    let wt_dir = std::path::Path::new(
        s.worktree_path
            .as_ref()
            .expect("session has a worktree_path"),
    );
    let body = std::fs::read_to_string(wt_dir.join("CLAUDE.md"))
        .expect("custom CLAUDE.md must be written into the session dir");
    assert!(
        body.contains("Run my tests before committing."),
        "custom guidance must be present"
    );
    assert!(
        !body.contains("# agentic-dev multi-repo session"),
        "single-repo session must NOT get the multi-repo orientation guide"
    );
    // Regression guard for the real submit_session assembly site: the build-env guide is
    // injected on every session (single-repo too), not just hand-built in a unit test.
    assert!(
        body.contains("## Build environment — inherit it from the main checkout"),
        "submit_session must inject the worktree build-env guide"
    );
    // The routing / fan-out rules are Tier-1 (appended to the main turn's system prompt), NOT
    // project memory — assert at the real engine assembly site that they're gone from CLAUDE.md,
    // so a regression that re-adds them to the `sections` vec is caught.
    assert!(!body.contains("Model routing") && !body.contains("Fan-out discipline"),
            "routing/fan-out guide must NOT be in the session CLAUDE.md (it's the appended system prompt)");
}

/// Multi-repo + custom CLAUDE.md combines both into one file, separated by a horizontal rule:
/// orientation guide first, the user's session-scoped guidance after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_multi_repo_with_custom_claude_md_combines_guide_and_custom() {
    let src = tmp();
    make_temp_git_repo_in(&src, "repoA");
    make_temp_git_repo_in(&src, "repoB");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec!["repoA".into(), "repoB".into()],
            vec![],
            "do multi".into(),
            HashMap::new(),
            SubmitMeta {
                claude_md: Some("No new dependencies.".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    wait_status(&e, &id, "done").await;
    let s = e.get(&id).await.unwrap();
    let wt_dir = std::path::Path::new(
        s.worktree_path
            .as_ref()
            .expect("session has a worktree_path"),
    );
    let body = std::fs::read_to_string(wt_dir.join("CLAUDE.md")).unwrap();
    assert!(
        body.contains("# agentic-dev multi-repo session"),
        "orientation guide present"
    );
    assert!(
        body.contains("No new dependencies."),
        "custom guidance present"
    );
    assert!(
        body.contains("\n---\n"),
        "guide and custom guidance separated by a horizontal rule"
    );
    // Orientation guide comes before the user's custom section.
    assert!(
        body.find("# agentic-dev multi-repo session").unwrap()
            < body.find("No new dependencies.").unwrap(),
        "orientation guide must precede the custom guidance"
    );
}

/// A crafted `name`/`token` with path-traversal components is sanitized to a safe leaf, so a
/// staged upload can never be adopted outside the session's `uploads/` dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_staged_upload_name_is_sanitized_against_traversal() {
    let src = tmp();
    make_temp_git_repo_in(&src, "repoA");
    let e = make_engine(&src, EngineOverrides::default()).await;
    // Stage under the sanitized token/name so the source actually exists; the create request then
    // sends the RAW (malicious) strings, which adoption must sanitize to the same safe leaves.
    let raw_name = "../../escape.png";
    // sanitize_upload_name strips the directory part via Path::file_name → just "escape.png".
    let safe_name = sanitize_upload_name(raw_name);
    assert_eq!(safe_name, "escape.png");
    let token = new_staging_token();
    let token_dir = staging_root(&e.0.cfg.worktrees_root).join(&token);
    std::fs::create_dir_all(&token_dir).unwrap();
    std::fs::write(token_dir.join(&safe_name), b"X").unwrap();

    let id = e
        .submit_session(
            vec!["repoA".into()],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta {
                staged_uploads: vec![StagedUpload {
                    token: token.clone(),
                    name: raw_name.into(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let s = e.get(&id).await.unwrap();
    let wt_dir = std::path::Path::new(s.worktree_path.as_ref().expect("worktree_path"));
    let uploads = wt_dir.join("repoA").join("uploads");
    assert!(
        uploads.join(&safe_name).exists(),
        "file lands under uploads/ with a sanitized name"
    );
    // No directory escape: nothing was written above the uploads dir.
    assert!(
        !wt_dir.join("escape.png").exists(),
        "must not escape into the worktree root"
    );
}

// ── Task 3 tests: spawn_opts per-turn override + fallback ─

/// spawn_opts: a per-turn model/effort on the QueueItem wins over the session's stored values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_uses_per_turn_model_override() {
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
                effort: Some("high".into()),
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

    // Build the QueueItem follow_up WOULD push, with per-turn overrides. Construct it directly
    // rather than racing the pump for st.queue.front() — under load the turn can already be
    // dequeued/started by the time we read, leaving the queue empty.
    let item = QueueItem {
        id: id.clone(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: Some("claude-haiku-4-5-20251001".into()),
        effort: Some("low".into()),
        permission_mode: None,
        context_prefix: None,
    };

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    let opts = e.spawn_opts(&s, &item);

    assert_eq!(opts.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    assert_eq!(opts.effort.as_deref(), Some("low"));
}

/// spawn_opts: with no per-turn override, the session's stored model/effort are used.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_falls_back_to_session_when_no_per_turn_override() {
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
                effort: Some("high".into()),
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

    // Construct the item directly (no per-turn override) rather than racing the pump for
    // st.queue.front() — under load the turn can already be dequeued by the time we read.
    let item = QueueItem {
        id: id.clone(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    let opts = e.spawn_opts(&s, &item);

    assert_eq!(opts.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(opts.effort.as_deref(), Some("high"));
}

/// hidden_plugins round-trips submit → store → spawn_opts, where the blacklist is resolved
/// against the installed-plugin registry into an EXPLICIT enable map: selected (non-hidden)
/// installed plugins → `true` (force-enabled via the command-line settings layer), hidden →
/// `false`. SdkRunner then forwards the map to the bridge as SDK_BRIDGE_ENABLED_PLUGINS →
/// settings `enabledPlugins:{<id>:true|false}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_resolves_hidden_plugins_to_enable_map() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    // Installed-plugin registry in the test config dir: one hidden, one selected.
    let plugins_dir = e.0.cfg.claude_config_base.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{
                "superpowers@claude-plugins-official":[{"scope":"user","version":"6.1.1"}],
                "github@claude-plugins-official":[{"scope":"user","version":"1.0.0"}]
            }}"#,
    )
    .unwrap();
    let id = e
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta {
                model: None,
                effort: None,
                mode: None,
                permission_mode: None,
                hidden_skills: vec!["skill-a".into()],
                hidden_plugins: vec!["superpowers@claude-plugins-official".into()],
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

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    // Store keeps the raw blacklist (API/DB model unchanged) …
    assert_eq!(
        s.hidden_plugins,
        vec!["superpowers@claude-plugins-official".to_string()]
    );

    let item = QueueItem {
        id: id.clone(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(opts.hidden_skills, vec!["skill-a".to_string()]);
    // … while spawn_opts carries the resolved explicit map.
    assert_eq!(
        opts.enabled_plugins,
        std::collections::BTreeMap::from([
            ("github@claude-plugins-official".to_string(), true),
            ("superpowers@claude-plugins-official".to_string(), false),
        ])
    );
}

/// Fix 4 (coverage): hidden_mcp_servers and extra_mcp_servers must thread through
/// submit_session → store → spawn_opts and appear verbatim on SpawnOptions.
/// Removing the copy line at mod.rs ~713-714 (CreateInput build) or ~1841-1842
/// (spawn_opts build) causes this test to FAIL — verified by temporary deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_threads_mcp_fields() {
    use crate::engine::store::McpServerDef;
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let extra = vec![
        McpServerDef {
            name: "my-stdio".into(),
            command: Some("npx".into()),
            args: Some(vec!["my-server".into()]),
            env: None,
            transport: None,
            url: None,
            headers: None,
        },
        McpServerDef {
            name: "my-http".into(),
            command: None,
            args: None,
            env: None,
            transport: Some("http".into()),
            url: Some("https://example.com/mcp".into()),
            headers: None,
        },
    ];
    let hidden = vec!["disabled-mcp".to_string()];
    let id = e
        .submit_session(
            vec![],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta {
                model: None,
                effort: None,
                mode: None,
                permission_mode: None,
                hidden_skills: vec![],
                hidden_plugins: vec![],
                hidden_mcp_servers: hidden.clone(),
                extra_mcp_servers: extra.clone(),
                claude_md: None,
                staged_uploads: vec![],
                forced_on_plugins: vec![],
                forced_on_skills: vec![],
                forced_on_mcp_servers: vec![],
            },
        )
        .await
        .unwrap();

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    // Verify store persistence (mod.rs ~713-714 — CreateInput build seam).
    assert_eq!(
        s.hidden_mcp_servers, hidden,
        "hidden_mcp_servers not persisted to store"
    );
    assert_eq!(
        s.extra_mcp_servers.len(),
        2,
        "extra_mcp_servers not persisted to store"
    );
    assert_eq!(s.extra_mcp_servers[0].name, "my-stdio");
    assert_eq!(s.extra_mcp_servers[1].name, "my-http");

    let item = QueueItem {
        id: id.clone(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    // Verify spawn_opts propagation (mod.rs ~1841-1842 — SpawnOptions build seam).
    assert_eq!(
        opts.hidden_mcp_servers, hidden,
        "hidden_mcp_servers not in SpawnOptions"
    );
    assert_eq!(
        opts.extra_mcp_servers.len(),
        2,
        "extra_mcp_servers not in SpawnOptions"
    );
    assert_eq!(opts.extra_mcp_servers[0].name, "my-stdio");
    assert_eq!(opts.extra_mcp_servers[0].command, Some("npx".into()));
    assert_eq!(opts.extra_mcp_servers[1].name, "my-http");
    assert_eq!(
        opts.extra_mcp_servers[1].url,
        Some("https://example.com/mcp".into())
    );
}

/// forced_on_plugins / forced_on_skills / forced_on_mcp_servers must thread from
/// submit_session → store → spawn_opts and appear on SpawnOptions.
/// A missing forced_on_* assignment in the spawn_opts build causes this test to FAIL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_threads_forced_on_fields() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;

    let plugins = vec!["superpowers@claude-plugins-official".to_string()];
    let skills = vec!["rke2-ops".to_string()];
    let mcp = vec!["forced-server".to_string()];

    let id = e
        .submit_session(
            vec![],
            vec![],
            "forced-on threading test".into(),
            HashMap::new(),
            SubmitMeta {
                forced_on_plugins: plugins.clone(),
                forced_on_skills: skills.clone(),
                forced_on_mcp_servers: mcp.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    // Verify store persistence.
    assert_eq!(
        s.forced_on_plugins, plugins,
        "forced_on_plugins not persisted to store"
    );
    assert_eq!(
        s.forced_on_skills, skills,
        "forced_on_skills not persisted to store"
    );
    assert_eq!(
        s.forced_on_mcp_servers, mcp,
        "forced_on_mcp_servers not persisted to store"
    );

    let item = QueueItem {
        id: id.clone(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    // forced_on_plugins threads verbatim to SpawnOptions.
    assert_eq!(
        opts.forced_on_plugins, plugins,
        "forced_on_plugins not in SpawnOptions"
    );
    // forced_on_mcp_servers threads verbatim (spawn no-op, but must be stored).
    assert_eq!(
        opts.forced_on_mcp_servers, mcp,
        "forced_on_mcp_servers not in SpawnOptions"
    );
    // forced_on_skills: rke2-ops is globally on (no global overrides in test env),
    // so resolve_session_forced_on_skills returns [] — no explicit "on" needed.
    // The raw session value is still preserved (tested above via store assertion).
    assert!(
        opts.forced_on_skills.is_empty(),
        "forced_on_skills in SpawnOptions should be empty when skill is globally on: {:?}",
        opts.forced_on_skills
    );
}

// ── cwd resolution: Claude CLI's --resume is cwd-scoped (project slug = pwd-derived).
// A resume turn (session.claude_session_id is set) must spawn from the worktree ROOT so
// the CLI's slug-search finds the existing transcript jsonl. A first turn (no resume)
// keeps the original <worktree>/<repo> behavior so tooling sees repo-relative paths. ──

use crate::engine::store::Session;

/// First turn (claudeSessionId = None): cwd is `<worktree>/<repo>` (single-repo case),
/// preserving the original behavior so tooling finds repo-relative paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_first_turn_uses_repo_subdir_cwd() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let s = Session {
        id: "s1".into(),
        repos: vec!["agentic-dev".into()],
        worktree_path: Some(
            e.0.cfg
                .worktrees_root
                .join("s1")
                .to_string_lossy()
                .into_owned(),
        ),
        prompt: "go".into(),
        ..Default::default()
    };
    let item = QueueItem {
        id: "s1".into(),
        prompt: "go".into(),
        env: HashMap::new(),
        resume_session_id: None,
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(
        opts.cwd,
        e.0.cfg
            .worktrees_root
            .join("s1")
            .join("agentic-dev")
            .to_string_lossy()
            .to_string(),
        "first turn of a single-repo session must use <worktree>/<repo> cwd"
    );
}

/// Resume turn (claudeSessionId = Some, single-repo): cwd is the WORKTREE ROOT, not the
/// repo subdir. Without this, the CLI's cwd-derived slug search misses the original
/// transcript and every follow-up fails with "No conversation found" → exit code 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_resume_turn_uses_worktree_root_cwd() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let wt = e.0.cfg.worktrees_root.join("s2");
    // Lay down the transcript file so the resume sanity check passes (see
    // `resume_sanity_check` in spawn_opts).
    let slug = wt
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let transcript_dir = e.0.cfg.claude_config_base.join("projects").join(&slug);
    std::fs::create_dir_all(&transcript_dir).unwrap();
    // Must carry a real server id (`msg_...`) so resume_gate treats it as resumable; otherwise the
    // engine correctly drops `--resume` and starts fresh (see engine::resume_gate).
    std::fs::write(
            transcript_dir.join("00000000-0000-0000-0000-000000000001.jsonl"),
            b"{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"id\":\"msg_01TESTRESUME\"}}\n",
        )
        .unwrap();
    let s = Session {
        id: "s2".into(),
        repos: vec!["agentic-dev".into()],
        worktree_path: Some(wt.to_string_lossy().into_owned()),
        claude_session_id: Some("00000000-0000-0000-0000-000000000001".into()),
        prompt: "follow up".into(),
        ..Default::default()
    };
    let item = QueueItem {
        id: "s2".into(),
        prompt: "follow up".into(),
        env: HashMap::new(),
        resume_session_id: Some("00000000-0000-0000-0000-000000000001".into()),
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(
        opts.cwd,
        wt.to_string_lossy().to_string(),
        "resume turn must spawn from worktree root so CLI slug-search matches original transcript"
    );
    assert_eq!(
        opts.resume_session_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000001")
    );
}

/// Resume turn with claudeSessionId but **no** backing transcript jsonl: the resume
/// id is dropped (decides `fresh_start`) so the CLI never receives a doomed
/// `--resume <stale-id>`. This is the defensive fallback for sessions whose
/// transcript was deleted out from under the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_drops_resume_session_id_when_transcript_missing() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let wt = e.0.cfg.worktrees_root.join("s-missing");
    // Deliberately DO NOT create the transcript jsonl.
    let s = Session {
        id: "s-missing".into(),
        repos: vec!["agentic-dev".into()],
        worktree_path: Some(wt.to_string_lossy().into_owned()),
        claude_session_id: Some("11111111-2222-3333-4444-555555555555".into()),
        prompt: "follow up".into(),
        ..Default::default()
    };
    let item = QueueItem {
        id: "s-missing".into(),
        prompt: "follow up".into(),
        env: HashMap::new(),
        resume_session_id: Some("11111111-2222-3333-4444-555555555555".into()),
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(
        opts.resume_session_id, None,
        "missing transcript → resume must be dropped"
    );
    assert_eq!(
        opts.cwd,
        wt.to_string_lossy().to_string(),
        "cwd still worktree root"
    );
}

/// Multi-repo session with resume: still uses worktree root, not a per-repo subdir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_resume_turn_uses_worktree_root_even_for_multi_repo() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let wt = e.0.cfg.worktrees_root.join("s3");
    let s = Session {
        id: "s3".into(),
        repos: vec!["repo-a".into(), "repo-b".into()],
        worktree_path: Some(wt.to_string_lossy().into_owned()),
        claude_session_id: Some("00000000-0000-0000-0000-000000000002".into()),
        prompt: "follow up".into(),
        ..Default::default()
    };
    let item = QueueItem {
        id: "s3".into(),
        prompt: "follow up".into(),
        env: HashMap::new(),
        resume_session_id: Some("00000000-0000-0000-0000-000000000002".into()),
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(opts.cwd, wt.to_string_lossy().to_string());
}

/// Edge case: session with claudeSessionId but no worktree_path (should fall back to
/// worktrees_root and not panic).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_opts_resume_turn_without_worktree_path_falls_back_gracefully() {
    let src = tmp();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let s = Session {
        id: "s4".into(),
        repos: vec!["agentic-dev".into()],
        worktree_path: None,
        claude_session_id: Some("00000000-0000-0000-0000-000000000003".into()),
        prompt: "follow up".into(),
        ..Default::default()
    };
    let item = QueueItem {
        id: "s4".into(),
        prompt: "follow up".into(),
        env: HashMap::new(),
        resume_session_id: Some("00000000-0000-0000-0000-000000000003".into()),
        enqueued_at: None,
        model: None,
        effort: None,
        permission_mode: None,
        context_prefix: None,
    };
    let opts = e.spawn_opts(&s, &item);
    assert_eq!(
        opts.cwd,
        e.0.cfg.worktrees_root.to_string_lossy().to_string()
    );
}
