use super::*;

// ── Task 4 tests (preserved) ─────────────────────────────

#[test]
fn has_active_workflow_detects_live_meta_and_ignores_empty() {
    let base = tmp();
    assert!(
        !has_active_workflow(&base, None),
        "no workflows dir → false"
    );
    // A live agent meta (in-flight run, no summary) → list_workflows synthesises a
    // run with status "running" → active.
    let rd = base
        .join("projects")
        .join("-slug")
        .join("sess")
        .join("subagents")
        .join("workflows")
        .join("wf_live");
    std::fs::create_dir_all(&rd).unwrap();
    std::fs::write(
        rd.join("agent-a1.meta.json"),
        r#"{"agentType":"workflow-subagent"}"#,
    )
    .unwrap();
    assert!(
        has_active_workflow(&base, None),
        "a live agent meta file → active"
    );
}

#[test]
fn has_active_workflow_normalises_terminal_status() {
    // A session whose ONLY run has a terminal summary → not active.
    // The status is mixed-case + padded on purpose: the parity fix trims+lowercases
    // before comparing (the old hand-rolled scan compared the raw string, so it would
    // have (wrongly) treated "  Completed " as non-terminal → active).
    let base = tmp();
    let wf = base
        .join("projects")
        .join("-slug")
        .join("sess")
        .join("workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("wf_done.json"),
        r#"{"runId":"wf_done","status":"  Completed "}"#,
    )
    .unwrap();
    assert!(
        !has_active_workflow(&base, None),
        "terminal (normalised) summary → not active"
    );
    // Add a non-terminal summary alongside it → now active.
    std::fs::write(
        wf.join("wf_run.json"),
        r#"{"runId":"wf_run","status":"running"}"#,
    )
    .unwrap();
    assert!(
        has_active_workflow(&base, None),
        "a running summary → active"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn links_delegate_card_to_run_id_via_fifo() {
    // A delegate workflow card (Workflow{delegate:true}) followed by its DelegateRequest must append
    // a `workflowRun` marker linking the card's tool_use id to the run id, so the client opens the
    // exact run on click. The background fan-out (run_delegate) is irrelevant to the link itself —
    // the marker is written synchronously before it is spawned.
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;

    e.on_event(
        &id,
        ClaudeEvent::Workflow {
            id: "toolu_D".into(),
            name: "audit".into(),
            parent_tool_use_id: None,
            raw: serde_json::json!({}),
            delegate: true,
        },
    )
    .await;
    e.on_event(
        &id,
        ClaudeEvent::DelegateRequest {
            id: "deleg-1".into(),
            run_id: "wfdeleg-1-1".into(),
            tasks: vec![],
            title: Some("audit".into()),
            raw: serde_json::json!({}),
        },
    )
    .await;

    let marker = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|o: &serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("workflowRun"))
        .expect("a workflowRun marker is appended for the delegate card");
    assert_eq!(marker["id"], "toolu_D");
    assert_eq!(marker["runId"], "wfdeleg-1-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn links_native_workflow_card_to_run_id_from_tool_result() {
    // A native Workflow card (Workflow{delegate:false}) whose tool_result reports a `wf_…` run id
    // must append a `workflowRun` marker linking the two — and must NOT be mistaken for an agent
    // result (it is not a spawned subagent).
    let src = tmp();
    make_temp_git_repo_in(&src, "demo");
    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
    wait_status(&e, &id, "done").await;

    e.on_event(
        &id,
        ClaudeEvent::Workflow {
            id: "toolu_W".into(),
            name: "review".into(),
            parent_tool_use_id: None,
            raw: serde_json::json!({}),
            delegate: false,
        },
    )
    .await;
    e.on_event(
        &id,
        ClaudeEvent::AgentResult {
            tool_use_id: "toolu_W".into(),
            text: "Workflow started — runId: wf_abc123. Watch /workflows.".into(),
            raw: serde_json::json!({ "type": "user" }),
        },
    )
    .await;

    let parsed: Vec<serde_json::Value> = e
        .get_log(&id)
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let wf = parsed
        .iter()
        .find(|o| o.get("type").and_then(|t| t.as_str()) == Some("workflowRun"))
        .expect("a workflowRun marker is appended for the native Workflow card");
    assert_eq!(wf["id"], "toolu_W");
    assert_eq!(wf["runId"], "wf_abc123");
    assert!(
        parsed
            .iter()
            .all(|o| o.get("type").and_then(|t| t.as_str()) != Some("agent_result")),
        "a native Workflow result must not be persisted as an agent_result",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_does_not_idle_cancel_during_pending_delegate() {
    let src = tmp();
    std::fs::create_dir_all(&src).unwrap();
    // idle_max=500ms; a delegate fan-out can run minutes with no events on the main session.
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
        .submit_session(vec![], vec![], "deleg".into(), env, SubmitMeta::default())
        .await
        .unwrap();
    wait_status(&e, &id, "running").await;
    // Back-date last_event_at well past the idle cap → would normally be idle-cancelled...
    e.test_set_last_event_at(&id, e.now() - 60_000);
    // ...but a delegate fan-out is in flight, so the watchdog must leave the turn alone.
    e.mark_delegate_pending(&id);
    e.trigger_watchdog().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        e.get(&id).await.unwrap().status,
        "running",
        "pending delegate must exempt the idle turn from watchdog cancel"
    );
    // Once the fan-out completes the exemption lifts; the still-idle turn is then cancelled → done.
    e.clear_delegate_pending(&id);
    e.trigger_watchdog().await;
    wait_status(&e, &id, "done").await;
}

// ── Task 8: workflowRunning surfacing ─────────────────────

/// with_activity surfaces workflowRunning=true for a finished session with a live workflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_workflow_running_true_for_finished_session_with_live_workflow() {
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
    let s = e.get(&id).await.unwrap();
    // workflowRunning should be falsy before we seed the workflow file.
    assert!(
        s.workflow_running.is_none() || s.workflow_running == Some(false),
        "workflowRunning should be falsy with no workflow"
    );
    // Workflow data now lives in the SHARED config dir, namespaced by claude under
    // projects/<cwd-slug>/<sessionUuid>/. with_activity scopes the scan to this session's
    // claude_session_id (set by the fake bridge), so seed the live agent meta under that uuid.
    let uuid = s
        .claude_session_id
        .clone()
        .expect("the fake bridge sets a claude_session_id");
    let wf_dir =
        e.0.cfg
            .claude_config_base
            .join("projects")
            .join("-slug")
            .join(&uuid)
            .join("subagents")
            .join("workflows")
            .join("wf_live");
    std::fs::create_dir_all(&wf_dir).unwrap();
    std::fs::write(
        wf_dir.join("agent-a1.meta.json"),
        r#"{"agentType":"workflow-subagent"}"#,
    )
    .unwrap();
    // Now get() should surface workflow_running=true
    let s2 = e.get(&id).await.unwrap();
    assert_eq!(
        s2.workflow_running,
        Some(true),
        "workflowRunning should be true when a live agent meta file is present"
    );
}
