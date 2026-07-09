//! Engine unit tests (extracted verbatim from the former engine.rs inline `mod tests`).

    use super::*;
    use std::sync::atomic::Ordering;

    static CTR: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    fn tmp() -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir()
            .join(format!("agentic-engine-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fixture(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Default)]
    struct EngineOverrides {
        bridge_path: Option<String>,
        retitle_enabled: Option<bool>,
        max_concurrent: Option<u64>,
        log_fn: Option<LogFn>,
        sync_fn: Option<SyncFn>,
        idle_max_ms: Option<i64>,
        wall_max_ms: Option<i64>,
        idle_ttl_ms: Option<i64>,
        push_fn: Option<PushFn>,
        now_fn: Option<NowFn>,
        usage_fn: Option<UsageFn>,
        title_generator: Option<std::sync::Arc<dyn crate::engine::title_client::TitleGenerator>>,
    }

    /// No-op title generator used as the default for engine tests that
    /// don't care about generated titles. Task 4 swaps this out for the
    /// in-memory variant.
    struct NoopTitleGenerator;
    #[async_trait::async_trait]
    impl crate::engine::title_client::TitleGenerator for NoopTitleGenerator {
        async fn generate(
            &self,
            _p: &str,
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, crate::engine::title_client::TitleGeneratorError> {
            Ok(None)
        }
        async fn maybe_retitle(
            &self,
            _c: &str,
            _m: &[(String, String)],
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, crate::engine::title_client::TitleGeneratorError> {
            Ok(None)
        }
    }

    async fn make_engine(src: &PathBuf, overrides: EngineOverrides) -> Engine {
        let dir = tmp();
        std::fs::create_dir_all(src).unwrap();
        let bridge_path = overrides
            .bridge_path
            .clone()
            .unwrap_or_else(|| fixture("fake-sdk-bridge-ok.sh"));
        let cfg = EngineConfig {
            src_root: src.clone(),
            worktrees_root: dir.join("worktrees"),
            log_dir: dir.join("logs"),
            db_path: dir.join("db.sqlite"),
            title_generator: overrides
                .title_generator
                .clone()
                .unwrap_or_else(|| std::sync::Arc::new(NoopTitleGenerator) as std::sync::Arc<dyn crate::engine::title_client::TitleGenerator>),
            retitle_enabled: overrides.retitle_enabled.unwrap_or(true),
            max_concurrent: overrides.max_concurrent,
            git_org: "arcatva".into(),
            claude_config_base: dir.join("claude-config"),
            clone_fn: Some(Arc::new(|_url, _dest| {
                Err(std::io::Error::other(
                    "clone disabled in tests",
                ))
            })),
            sync_fn: overrides.sync_fn,
            // Tests drive turns through the SAME runner production uses (SdkRunner), with a fake
            // bridge script invoked via `bash` instead of `node sdk-bridge.mjs`. The fake bridge
            // appends canned stream-json to $SDK_BRIDGE_LOG — no real claude, no API cost.
            runner: Some(std::sync::Arc::new(
                crate::engine::sdk_runner::SdkRunner::with_node("bash", bridge_path),
            )),
            log_fn: overrides.log_fn,
            now_fn: overrides.now_fn,
            push_fn: overrides.push_fn,
            usage_fn: overrides.usage_fn,
            idle_max_ms: overrides.idle_max_ms,
            wall_max_ms: overrides.wall_max_ms,
            idle_ttl_ms: overrides.idle_ttl_ms,
            memory_max: None,
            memory_high: None,
            cpu_quota: None,
            tasks_max: None,
        };
        Engine::new(cfg).await.expect("engine::new")
    }

    // ── Task 6 test-only helpers ─────────────────────────────

    impl Engine {
        /// Test-only: back-date last_event_at for a session.
        #[cfg(test)]
        pub fn test_set_last_event_at(&self, id: &str, ts: i64) {
            self.0.state.lock().last_event_at.insert(id.to_string(), ts);
        }
        /// Test-only: back-date turn_started_at for a session.
        #[cfg(test)]
        pub fn test_set_turn_started_at(&self, id: &str, ts: i64) {
            self.0.state.lock().turn_started_at.insert(id.to_string(), ts);
        }
        /// Test-only: set awaiting state for a session.
        #[cfg(test)]
        pub fn test_set_awaiting(&self, id: &str, v: bool) {
            self.0.state.lock().awaiting.insert(id.to_string(), v);
        }
    }

    async fn test_engine() -> Engine {
        let dir = tmp();
        make_engine(&dir.join("src"), EngineOverrides::default()).await
    }

    /// Initialize a bare git repo in a temp dir, then copy it into src/<name> as a
    /// real git repo with a commit.
    fn make_temp_git_repo_in(src: &PathBuf, name: &str) {
        let dest = src.join(name);
        std::fs::create_dir_all(&dest).unwrap();
        // Init git repo
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dest)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dest)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dest)
            .output()
            .unwrap();
        // Create a file and commit
        std::fs::write(dest.join("README.md"), "# test\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dest)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dest)
            .output()
            .unwrap();
    }

    /// Wait for a session to reach the given status, polling periodically.
    /// Times out after 30s (panic, not hang).
    async fn wait_status(e: &Engine, id: &str, target: &str) {
        // 240s (not 30s): these tests drive real fake-bridge subprocesses through a 120ms-polled
        // lifecycle; under heavy parallel `cargo test` load — especially 2-vCPU CI runners — the
        // status transitions are correct but slow to get scheduled, and a tight deadline produces
        // load-induced flakes (watchdog_idle_reap hit 90s on GitHub Actions). A genuine hang still
        // fails, just later; the CI job timeout is the backstop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("wait_status timed out waiting for session {id} to reach status={target}; current status={:?}",
                    e.get(id).await.map(|s| s.status));
            }
            if let Some(s) = e.get(id).await {
                if s.status == target {
                    return;
                }
                // If we're waiting for something and the session is already in a terminal state
                // that's different, bail early to avoid hanging.
                if matches!(s.status.as_str(), "done" | "failed" | "killed") && s.status != target {
                    // Only bail if both are terminal (won't transition further).
                    if matches!(target, "done" | "failed" | "killed") {
                        panic!("wait_status: session {id} reached {}, not {target}", s.status);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // ── Task 4 tests (preserved) ─────────────────────────────

    #[test]
    fn has_active_workflow_detects_live_meta_and_ignores_empty() {
        let base = tmp();
        assert!(!has_active_workflow(&base, None), "no workflows dir → false");
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
        assert!(has_active_workflow(&base, None), "a live agent meta file → active");
    }

    #[test]
    fn has_active_workflow_normalises_terminal_status() {
        // A session whose ONLY run has a terminal summary → not active.
        // The status is mixed-case + padded on purpose: the parity fix trims+lowercases
        // before comparing (the old hand-rolled scan compared the raw string, so it would
        // have (wrongly) treated "  Completed " as non-terminal → active).
        let base = tmp();
        let wf = base.join("projects").join("-slug").join("sess").join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(
            wf.join("wf_done.json"),
            r#"{"runId":"wf_done","status":"  Completed "}"#,
        )
        .unwrap();
        assert!(!has_active_workflow(&base, None), "terminal (normalised) summary → not active");
        // Add a non-terminal summary alongside it → now active.
        std::fs::write(
            wf.join("wf_run.json"),
            r#"{"runId":"wf_run","status":"running"}"#,
        )
        .unwrap();
        assert!(has_active_workflow(&base, None), "a running summary → active");
    }

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
        let id = e.submit("demo", "do something", HashMap::new()).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.claude_session_id.as_deref(), Some("fake-sess-123"));
        assert!((s.cost_usd.unwrap_or(0.0) - 0.0042).abs() < 1e-9,
            "cost should be 0.0042, got {:?}", s.cost_usd);
        assert_eq!(s.exit_code, Some(0));
        assert!(!e.get_log(&id).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn logs_an_agentic_prompt_marker() {
        let src = tmp();
        make_temp_git_repo_in(&src, "demo");
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit("demo", "my first prompt", HashMap::new()).await.unwrap();
        wait_status(&e, &id, "done").await;
        let prompts: Vec<serde_json::Value> = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|o: &serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("agentic_prompt"))
            .collect();
        assert!(!prompts.is_empty(), "should have an agentic_prompt entry in log");
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
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-agentresult.sh")),
            ..Default::default()
        }).await;
        let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
        wait_status(&e, &id, "done").await;
        let markers: Vec<serde_json::Value> = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|o: &serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("agent_result"))
            .collect();
        assert_eq!(markers.len(), 1, "exactly one agent_result marker (the spawned agent, not Bash)");
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
        e.on_event(&id, ClaudeEvent::Agent {
            agents: vec![crate::engine::stream::SpawnedAgent {
                id: "tu_agent".into(),
                agent_type: "Explore".into(),
                description: "d".into(),
            }],
            parent_tool_use_id: None,
            raw: serde_json::json!({}),
        }).await;

        // 1) Genuine subagent tool_result (raw type = "user") → persists exactly one marker.
        e.on_event(&id, ClaudeEvent::AgentResult {
            tool_use_id: "tu_agent".into(),
            text: "AGENT OUTPUT".into(),
            raw: serde_json::json!({ "type": "user" }),
        }).await;

        // 2) That marker is re-tailed and decoded back into an AgentResult whose raw IS the marker
        //    (type = "agent_result"). This MUST NOT append a second marker.
        e.on_event(&id, ClaudeEvent::AgentResult {
            tool_use_id: "tu_agent".into(),
            text: "AGENT OUTPUT".into(),
            raw: serde_json::json!({ "type": "agent_result", "toolUseId": "tu_agent", "text": "AGENT OUTPUT" }),
        }).await;

        let markers = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|o: &serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("agent_result"))
            .count();
        assert_eq!(markers, 1, "a re-tailed agent_result marker must not be re-persisted (feedback loop)");
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

        e.on_event(&id, ClaudeEvent::Workflow {
            id: "toolu_D".into(), name: "audit".into(), parent_tool_use_id: None,
            raw: serde_json::json!({}), delegate: true,
        }).await;
        e.on_event(&id, ClaudeEvent::DelegateRequest {
            id: "deleg-1".into(), run_id: "wfdeleg-1-1".into(), tasks: vec![],
            title: Some("audit".into()), raw: serde_json::json!({}),
        }).await;

        let marker = e.get_log(&id).iter()
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

        e.on_event(&id, ClaudeEvent::Workflow {
            id: "toolu_W".into(), name: "review".into(), parent_tool_use_id: None,
            raw: serde_json::json!({}), delegate: false,
        }).await;
        e.on_event(&id, ClaudeEvent::AgentResult {
            tool_use_id: "toolu_W".into(),
            text: "Workflow started — runId: wf_abc123. Watch /workflows.".into(),
            raw: serde_json::json!({ "type": "user" }),
        }).await;

        let parsed: Vec<serde_json::Value> = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let wf = parsed.iter()
            .find(|o| o.get("type").and_then(|t| t.as_str()) == Some("workflowRun"))
            .expect("a workflowRun marker is appended for the native Workflow card");
        assert_eq!(wf["id"], "toolu_W");
        assert_eq!(wf["runId"], "wf_abc123");
        assert!(
            parsed.iter().all(|o| o.get("type").and_then(|t| t.as_str()) != Some("agent_result")),
            "a native Workflow result must not be persisted as an agent_result",
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
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Text { .. }) {
                g.lock().push("text".into());
            }
        }));
        wait_status(&e, &id, "done").await;
        assert!(!got.lock().is_empty(), "saw at least one text event");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn respects_max_concurrent_second_stays_pending() {
        let src = tmp();
        make_temp_git_repo_in(&src, "demo");
        let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), ..Default::default() }).await;
        let mut slow = HashMap::new();
        slow.insert("FAKE_CLAUDE_SLEEP".into(), "2".into());
        let id1 = e.submit("demo", "first", slow).await.unwrap();
        let id2 = e.submit("demo", "second", HashMap::new()).await.unwrap();
        // Wait for id1 to be running
        wait_status(&e, &id1, "running").await;
        // id2 should still be pending
        assert_eq!(e.get(&id2).await.unwrap().status, "pending",
            "second session should be pending while first is running");
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
        let id = e.submit_session(
            vec!["demo".into()],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta::default(),
        ).await.unwrap();
        // Status should be pending immediately (deferred start).
        assert_eq!(e.get(&id).await.unwrap().status, "pending");
        wait_status(&e, &id, "done").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pure_skill_session_no_repo_runs_done() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(
            vec![],
            vec![],
            "just answer".into(),
            HashMap::new(),
            SubmitMeta::default(),
        ).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        assert!(s.repos.is_empty());
        assert_eq!(s.status, "done");
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
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-error.sh")),
            ..Default::default()
        }).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "failed").await;
        let s = e.get(&id).await.unwrap();
        assert!(s.error.unwrap_or_default().to_lowercase().contains("session limit"),
            "error should contain 'session limit'");
        assert_eq!(s.error_kind.as_deref(), Some("usage_limit"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn crashed_turn_is_not_mislabeled_as_usage_limit() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-crash.sh")),
            ..Default::default()
        }).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "failed").await;
        let s = e.get(&id).await.unwrap();
        let err = s.error.unwrap_or_default();
        assert!(!err.is_empty(), "error must not be empty for a crash");
        let limit_re = regex::Regex::new("(?i)limit|resets").unwrap();
        assert!(!limit_re.is_match(&err),
            "crash message must not contain 'limit' or 'resets': got {err:?}");
        assert_eq!(s.error_kind.as_deref(), Some("crashed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rate_limit_tagged_rate_limited() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-ratelimit.sh")),
            ..Default::default()
        }).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "failed").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.error_kind.as_deref(), Some("rate_limited"));
        assert!(s.error.unwrap_or_default().to_lowercase().contains("not your usage limit"),
            "error should contain the rate-limit message");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn emits_structured_turn_lifecycle_logs() {
        let src = tmp();
        make_temp_git_repo_in(&src, "demo");
        let logs = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let l = logs.clone();
        let e = make_engine(&src, EngineOverrides {
            log_fn: Some(Arc::new(move |r| l.lock().push(r))),
            ..Default::default()
        }).await;
        let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
        wait_status(&e, &id, "done").await;
        // turn_end is emitted during finalization, which can land a hair AFTER status flips to "done"
        // under load — wait for it so the assertions below don't race the log_fn.
        wait_until(|| {
            logs.lock().iter().any(|r| r["evt"] == "turn_end" && r["sessionId"] == id.as_str())
        }).await;
        let recs = logs.lock();
        let find = |evt: &str| recs.iter().find(|r| r["evt"] == evt && r["sessionId"] == id.as_str()).cloned();
        let start = find("turn_start").expect("turn_start log record must be present");
        assert!(start.get("queueWaitMs").is_some(), "turn_start must have queueWaitMs");
        assert!(start.get("max").is_some(), "turn_start must have max");
        let result = find("turn_result").expect("turn_result log record must be present");
        assert!(result.as_object().unwrap().contains_key("ttftMs"),
            "turn_result must have ttftMs key");
        let end = find("turn_end").expect("turn_end log record must be present");
        assert_eq!(end["status"], "done", "turn_end status must be 'done'");
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
        e.on_event(&id, ClaudeEvent::Result {
            is_error: true,
            cost_usd: None,
            text: Some(
                "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use".into(),
            ),
            raw: serde_json::json!({}),
        }).await;

        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "killed", "a stopped session stays killed");
        assert!(s.error_kind.is_none(),
            "a deliberate Stop must not be classified as an error, got {:?}", s.error_kind);
        assert!(s.error.is_none(),
            "a deliberate Stop must not carry error text, got {:?}", s.error);
    }

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
        let run = e.0.state.lock().running.get(&id).map(|t| t.run.clone()).expect("running turn");
        assert!(run.is_active(), "precondition: claude is running");
        e.close_with(false);
        assert!(run.is_active(), "graceful close (kill_running=false) must leave the run active");
        run.stop(); // cleanup: don't leak the 120s fake sleep

        // Kill path: close_with(true) stops the in-flight run.
        let e2 = make_engine(&src, EngineOverrides::default()).await;
        let id2 = e2.submit("demo", "slow", slow).await.unwrap();
        wait_status(&e2, &id2, "running").await;
        let run2 = e2.0.state.lock().running.get(&id2).map(|t| t.run.clone()).expect("running turn 2");
        e2.close_with(true);
        // Generous deadline for loaded CI runners (kill + group-reap can take seconds there).
        let mut stopped = false;
        for _ in 0..375 {
            if !run2.is_active() { stopped = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        assert!(stopped, "kill close (kill_running=true) must stop the run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn kill_queued_marks_killed_immediately() {
        let src = tmp();
        make_temp_git_repo_in(&src, "demo");
        // max_concurrent=1 so second session stays queued
        let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), ..Default::default() }).await;
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
        assert!(s.ended_at.is_some(), "killed queued session must have endedAt");
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
        let e = make_engine(&src, EngineOverrides {
            idle_max_ms: Some(500),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "stay forever".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Back-date last_event_at by 60 seconds → idle.
        e.test_set_last_event_at(&id, e.now() - 60_000);
        e.trigger_watchdog().await;
        // Graceful cancel: marked `done` (NOT failed), with no error / error_kind.
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "done", "idle reap must be a graceful done, not failed");
        assert!(s.error.is_none(), "graceful cancel must not set an error: {:?}", s.error);
        assert!(s.error_kind.is_none(), "graceful cancel must not set error_kind: {:?}", s.error_kind);
        // And it must stay resumable — the user can continue the conversation.
        assert!(e.follow_up(&id, "continue please", true, None, None, None).await.is_ok(),
            "session must be resumable after a graceful idle cancel");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watchdog_idle_reap_frees_slot_for_next_session() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            max_concurrent: Some(1),
            idle_max_ms: Some(500),
            wall_max_ms: Some(3_600_000),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "stay forever".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        e.test_set_last_event_at(&id, e.now() - 60_000);
        e.trigger_watchdog().await;
        wait_status(&e, &id, "done").await;   // graceful cancel → done (not failed)
        // Now submit a second session — the slot should be free.
        let id2 = e.submit_session(vec![], vec![], "after".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id2, "done").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watchdog_wall_reap_marks_done_when_cap_set() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // wall_max=500ms (opt-in), idle_max=very high.
        let e = make_engine(&src, EngineOverrides {
            idle_max_ms: Some(3_600_000),
            wall_max_ms: Some(500),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Back-date turn_started_at by 60 seconds (keep last_event_at fresh → not idle).
        e.test_set_turn_started_at(&id, e.now() - 60_000);
        e.trigger_watchdog().await;
        // Graceful cancel → done (resumable), not failed.
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "done");
        assert!(s.error.is_none(), "wall graceful cancel must not set an error: {:?}", s.error);
        assert!(s.error_kind.is_none(), "wall graceful cancel must not set error_kind: {:?}", s.error_kind);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watchdog_wall_unlimited_by_default() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // No wall_max set → wall is unlimited (idle high too). A long-running but recently-active
        // turn must NOT be reaped.
        let e = make_engine(&src, EngineOverrides {
            idle_max_ms: Some(3_600_000),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Back-date turn_started_at by an hour; keep last_event_at fresh.
        e.test_set_turn_started_at(&id, e.now() - 3_600_000);
        e.trigger_watchdog().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(e.get(&id).await.unwrap().status, "running",
            "wall is unlimited by default — a long-running active turn must not be reaped");
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watchdog_does_not_wall_reap_parked_session() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // wall_max=200ms, idle_max=very high
        let e = make_engine(&src, EngineOverrides {
            idle_max_ms: Some(3_600_000),
            wall_max_ms: Some(200),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Mark as parked (awaiting=true) and back-date turn_started_at.
        e.test_set_awaiting(&id, true);
        e.test_set_turn_started_at(&id, e.now() - 60_000);
        // last_event_at is fresh → not idle either
        e.trigger_watchdog().await;
        // Small wait to let watchdog execute; session should NOT have been reaped.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "running", "parked session must not be wall-reaped");
        // Clean up
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watchdog_does_not_idle_cancel_during_pending_delegate() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // idle_max=500ms; a delegate fan-out can run minutes with no events on the main session.
        let e = make_engine(&src, EngineOverrides {
            idle_max_ms: Some(500),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "deleg".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Back-date last_event_at well past the idle cap → would normally be idle-cancelled...
        e.test_set_last_event_at(&id, e.now() - 60_000);
        // ...but a delegate fan-out is in flight, so the watchdog must leave the turn alone.
        e.mark_delegate_pending(&id);
        e.trigger_watchdog().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(e.get(&id).await.unwrap().status, "running",
            "pending delegate must exempt the idle turn from watchdog cancel");
        // Once the fan-out completes the exemption lifts; the still-idle turn is then cancelled → done.
        e.clear_delegate_pending(&id);
        e.trigger_watchdog().await;
        wait_status(&e, &id, "done").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_ttl_reaps_parked_session_as_done() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // idle_ttl_ms=500ms
        let e = make_engine(&src, EngineOverrides {
            idle_ttl_ms: Some(500),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
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
        let e = make_engine(&src, EngineOverrides { idle_ttl_ms: Some(500), ..Default::default() }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        // Give the session a prior error so the reaper takes the 'failed' branch.
        e.0.store.update(&id, SessionPatch {
            error: Some(Some("prior turn error".into())),
            ..Default::default()
        }).await.unwrap();
        e.test_set_awaiting(&id, true);
        e.test_set_last_event_at(&id, e.now() - 60_000);
        e.trigger_watchdog().await;
        // Let the reap + process exit settle, then assert it stuck at 'failed' with the error kept.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "failed", "errored parked session must reap as 'failed', got {:?}", s.status);
        assert_eq!(s.error.as_deref(), Some("prior turn error"), "the prior error must be preserved");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_ttl_reaped_parked_stays_done_after_process_exits() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            idle_ttl_ms: Some(500),
            ..Default::default()
        }).await;
        let mut env = HashMap::new();
        env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
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
            "status should be 'done' after idle-TTL reap, got {:?}", s.status
        );
        assert!(s.error.is_none(), "error should be None after idle-TTL reap: {:?}", s.error);
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
        let e = make_engine(&src, EngineOverrides {
            max_concurrent: Some(1),
            sync_fn: Some(sync_fn),
            ..Default::default()
        }).await;
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
        assert_eq!(s.status, "killed", "session should be killed after kill-during-sync");
        // No claude process should have been spawned (session is not in running map).
        assert!(
            !e.0.state.lock().running.contains_key(&id),
            "no claude process should have been spawned after kill-during-sync"
        );
    }

    // ── Task 7 tests: followUp / streaming awaitingInput ────────

    /// Poll a predicate every 20ms, up to 5 seconds (panic on timeout).
    async fn wait_until<F: Fn() -> bool>(pred: F) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pred() { return; }
            if std::time::Instant::now() > deadline {
                panic!("wait_until: timed out");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Streaming session: one persistent process, each follow_up injects over stdin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streaming_one_process_goes_idle_then_followup_injects_over_stdin() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            max_concurrent: Some(2),
            ..Default::default()
        }).await;

        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "first turn".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();

        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));

        // Wait for first result.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;

        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "running", "streaming session stays running after first result");
        assert_eq!(s.awaiting_input, Some(true), "awaiting_input=true after first result");
        assert_eq!(s.claude_session_id.as_deref(), Some("fake-stream-1"));
        assert!(s.cost_usd.unwrap_or(0.0) > 0.0, "cost captured after first result");

        // Inject second turn.
        let since = e.follow_up(&id, "second turn", true, None, None, None).await.unwrap();
        let _ = since;

        // awaiting_input must flip false immediately after inject.
        let s2 = e.get(&id).await.unwrap();
        assert_eq!(s2.awaiting_input, Some(false), "awaiting_input=false right after follow_up inject");

        // Wait for second result.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;

        let s3 = e.get(&id).await.unwrap();
        assert_eq!(s3.awaiting_input, Some(true), "awaiting_input=true after second result");
        assert_eq!(s3.activity.as_ref().map(|a| a.turns), Some(2), "activity.turns=2");

        // Check both agentic_prompt entries in log.
        let prompts: Vec<String> = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|o| o["type"] == "agentic_prompt")
            .map(|o| o["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(prompts, vec!["first turn", "second turn"], "both prompts in log");

        // Clean up.
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }

    /// follow_up on a live session succeeds (no error).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_does_not_error_on_live_session() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "first".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        assert!(e.follow_up(&id, "second", true, None, None, None).await.is_ok(), "follow_up on live session must succeed");
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
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "first".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        let t1 = e.get(&id).await.unwrap().last_user_message_at;
        // Small delay to ensure clock advances.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        e.follow_up(&id, "second", true, None, None, None).await.unwrap();
        let t2 = e.get(&id).await.unwrap().last_user_message_at;
        assert!(t2 >= t1, "lastUserMessageAt must not go backwards");
        // Cleanup.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }

    /// A later turn crash after a success must finalize as failed / errorKind=crashed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn finalizes_failed_when_later_turn_crashes_after_success() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream-crash2.sh")),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "turn1".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        // Wait for turn 1 result.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        // Inject turn 2 (crash2 fixture exits 1 on second read).
        e.follow_up(&id, "turn2", true, None, None, None).await.unwrap();
        // Session should end as failed.
        wait_status(&e, &id, "failed").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.error_kind.as_deref(), Some("crashed"), "errorKind should be crashed after crash on turn2");
    }

    /// interrupt clears pending_ask but does not change status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interrupt_leaves_session_alive_and_clears_pending_ask() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "t".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
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

    /// follow_up on a finished session re-queues it, accumulates cost, same worktree.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_reruns_finished_session_accumulates_cost() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // Use default fake-sdk-bridge-ok.sh (one-shot, cost 0.0042 each turn).
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec![], vec![], "first".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s1 = e.get(&id).await.unwrap();
        let wt = s1.worktree_path.clone();
        let log_len1 = e.get_log(&id).len();

        // Follow-up: re-queues for a new turn.
        e.follow_up(&id, "second", true, None, None, None).await.unwrap();
        wait_status(&e, &id, "done").await;

        let s2 = e.get(&id).await.unwrap();
        assert_eq!(s2.worktree_path, wt, "same worktree_path after follow-up");
        assert!(s2.cost_usd.unwrap_or(0.0) > s1.cost_usd.unwrap_or(0.0),
            "cost should accumulate across turns");
        assert!(e.get_log(&id).len() > log_len1, "log grew after follow-up turn");
    }

    /// follow_up retitles only when set_title=true.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_retitles_only_when_set_title_true() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec![], vec![], "original".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "done").await;

        // set_title=true → prompt updates.
        e.follow_up(&id, "second", true, None, None, None).await.unwrap();
        wait_status(&e, &id, "done").await;
        assert_eq!(e.get(&id).await.unwrap().prompt, "second", "prompt should update when set_title=true");

        // set_title=false → prompt stays.
        e.follow_up(&id, "third", false, None, None, None).await.unwrap();
        wait_status(&e, &id, "done").await;
        assert_eq!(e.get(&id).await.unwrap().prompt, "second", "prompt should not change when set_title=false");
    }

    /// follow_up with set_title omitted (i.e. None) does NOT retitle — the title
    /// is owned by the first submit and follow-ups leave it alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_default_does_not_retitle() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec![], vec![], "original".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "done").await;

        // After submit, the prompt may have been replaced by generate_title;
        // capture it for comparison.
        let original = e.get(&id).await.unwrap().prompt;

        // set_title omitted → prompt must NOT change to "second".
        e.follow_up(&id, "second", false, None, None, None).await.unwrap();
        wait_status(&e, &id, "done").await;
        assert_eq!(e.get(&id).await.unwrap().prompt, original, "default behaviour must not retitle");
    }

    /// follow_up with no claudeSessionId (noinit fixture) still succeeds and runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_retry_when_no_claude_session_id() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // fake-sdk-bridge-noinit.sh emits no init event → claude_session_id stays None.
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-noinit.sh")),
            ..Default::default()
        }).await;
        let id = e.submit_session(vec![], vec![], "first".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        // noinit results in exit 1 (no result seen) → "failed"
        for _ in 0..250 {
            let st = e.get(&id).await.map(|s| s.status).unwrap_or_default();
            if matches!(st.as_str(), "done" | "failed") { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // claude_session_id should be None (no init emitted).
        let s = e.get(&id).await.unwrap();
        assert!(s.claude_session_id.is_none(), "claude_session_id must be None with noinit fixture");
        // follow_up should still succeed (resume_session_id=None → fresh start).
        assert!(e.follow_up(&id, "retry me", true, None, None, None).await.is_ok(), "follow_up must succeed even without claude_session_id");
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
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-error.sh")),
            ..Default::default()
        }).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
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
        e.follow_up(&id, "continue", true, None, None, None).await.unwrap();
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "pending", "queued branch should mark the session pending");
        assert!(s.error.is_none(), "error must be cleared by the queued branch, got {:?}", s.error);
        assert!(s.error_kind.is_none(), "error_kind must be cleared by the queued branch, got {:?}", s.error_kind);
    }

    /// follow_up on unknown session returns Err.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_unknown_session_errors() {
        let e = test_engine().await;
        assert!(e.follow_up("nope", "x", true, None, None, None).await.is_err(), "follow_up on unknown session must error");
    }

    /// follow_up while another follow_up is already queued → Err("session busy").
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rejects_second_followup_while_first_queued() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // max_concurrent=1 so second follow-up can't start immediately.
        let e = make_engine(&src, EngineOverrides {
            max_concurrent: Some(1),
            ..Default::default()
        }).await;
        // Saturate the slot with a slow first session.
        let mut slow_env = HashMap::new();
        slow_env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let _blocker = e.submit_session(vec![], vec![], "blocker".into(), slow_env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &_blocker, "running").await;

        // Now submit the target session (will be queued, then done after blocker).
        let id = e.submit_session(vec![], vec![], "target".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        // Wait for target to reach pending (it's in queue).
        wait_status(&e, &id, "pending").await;

        // The target is pending/queued. A follow_up queues another turn.
        // But is_busy returns true because the item is in the queue → Err.
        let result = e.follow_up(&id, "extra turn", true, None, None, None).await;
        assert!(result.is_err(), "second follow_up must fail when session is busy/queued");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("busy") || err.contains("pending"),
            "error should mention busy or pending, got: {err}");
    }

    // ── Task 8 helpers ─────────────────────────────────────────

    /// Build an Engine over a fixed work dir (db_path + log_dir deterministic from `work`).
    /// The recover tests open a bare Store, seed rows/logs, drop it, then call this to get
    /// a fresh Engine that runs recover() over the same data.
    async fn engine_from(work: &PathBuf) -> Engine {
        let cfg = EngineConfig {
            src_root: work.clone(),
            worktrees_root: work.join("worktrees"),
            log_dir: work.join("logs"),
            db_path: work.join("db.sqlite"),
            title_generator: std::sync::Arc::new(NoopTitleGenerator),
            retitle_enabled: true,
            max_concurrent: None,
            git_org: "arcatva".into(),
            claude_config_base: work.join("claude-config"),
            clone_fn: Some(Arc::new(|_url, _dest| {
                Err(std::io::Error::other("clone disabled"))
            })),
            sync_fn: None,
            runner: Some(std::sync::Arc::new(
                crate::engine::sdk_runner::SdkRunner::with_node("bash", fixture("fake-sdk-bridge-ok.sh")),
            )),
            log_fn: None,
            now_fn: None,
            push_fn: None,
            usage_fn: None,
            idle_max_ms: None,
            wall_max_ms: None,
            idle_ttl_ms: None,
            memory_max: None,
            memory_high: None,
            cpu_quota: None,
            tasks_max: None,
        };
        Engine::new(cfg).await.expect("engine_from::new")
    }

    // ── Task 8: recover tests ──────────────────────────────────

    /// Recover: a session that was "running" with a successful result → done.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recover_finalizes_running_as_done_when_log_ended_in_result() {
        let work = tmp();
        {
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "s1".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("s1").to_string_lossy().into()),
                ..Default::default()
            }).await.unwrap();
            store.update("s1", SessionPatch {
                status: Some("running".into()),
                ..Default::default()
            }).await.unwrap();
            // Append a successful result event
            store.append_log("s1", r#"{"type":"result","subtype":"success","is_error":false}"#).await.unwrap();
        } // store dropped
        let e = engine_from(&work).await;
        assert_eq!(e.get("s1").await.unwrap().status, "done",
            "running session with success result should recover as done");
    }

    /// Recover: a session that was "running" with no result event → failed/interrupted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recover_finalizes_running_as_failed_interrupted_when_no_result() {
        let work = tmp();
        {
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "s2".into(),
                prompt: "p".into(),
                ..Default::default()
            }).await.unwrap();
            store.update("s2", SessionPatch {
                status: Some("running".into()),
                ..Default::default()
            }).await.unwrap();
            // Append a non-result event (no result)
            store.append_log("s2", r#"{"type":"text","text":"partial output"}"#).await.unwrap();
        }
        let e = engine_from(&work).await;
        let s = e.get("s2").await.unwrap();
        assert_eq!(s.status, "failed", "should be failed");
        assert!(s.error.as_deref().unwrap_or("").contains("interrupted"),
            "error should mention interrupted: {:?}", s.error);
        assert_eq!(s.error_kind.as_deref(), Some("interrupted"));
    }

    /// Recover: a session that was "running" with an error result → failed with classifed errorKind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recover_finalizes_running_as_failed_with_real_reason_on_error_result() {
        let work = tmp();
        {
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "s3".into(),
                prompt: "p".into(),
                ..Default::default()
            }).await.unwrap();
            store.update("s3", SessionPatch {
                status: Some("running".into()),
                ..Default::default()
            }).await.unwrap();
            // Error result with usage-limit text
            store.append_log("s3", r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your session limit"}"#).await.unwrap();
        }
        let e = engine_from(&work).await;
        let s = e.get("s3").await.unwrap();
        assert_eq!(s.status, "failed");
        assert_eq!(s.error_kind.as_deref(), Some("usage_limit"),
            "should classify as usage_limit");
        assert!(s.error.as_deref().unwrap_or("").to_lowercase().contains("session limit"),
            "error text should include 'session limit': {:?}", s.error);
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "sp1".into(),
                prompt: "go".into(),
                worktree_path: Some(session_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            }).await.unwrap();
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "sbad".into(),
                prompt: "go".into(),
                worktree_path: None, // no worktree → start() returns Err
                ..Default::default()
            }).await.unwrap();
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            // bad: no worktree
            store.create(CreateInput {
                id: "sbad2".into(),
                prompt: "go".into(),
                worktree_path: None,
                ..Default::default()
            }).await.unwrap();
            // good: has a worktree dir
            store.create(CreateInput {
                id: "sgood".into(),
                prompt: "go".into(),
                worktree_path: Some(good_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            }).await.unwrap();
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "se1".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("se1").to_string_lossy().into()),
                ..Default::default()
            }).await.unwrap();
            store.update("se1", SessionPatch {
                status: Some("running".into()),
                ..Default::default()
            }).await.unwrap();
            store.append_log("se1", r#"{"type":"agentic_prompt","text":"p","at":1000}"#).await.unwrap();
            store.append_log("se1", r#"{"type":"result","subtype":"success","is_error":false}"#).await.unwrap();
        }
        let e = engine_from(&work).await;
        let s = e.get("se1").await.unwrap();
        assert_eq!(s.status, "done", "success result recovers as done");
        assert_eq!(s.ended_at, Some(1000),
            "recover must restore the real turn time (prompt-at fallback), not boot now()");
    }

    /// When the lifecycle sidecar recorded a TurnEnded, recover restores THAT
    /// authoritative timestamp, in preference to the transcript prompt-at.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recover_prefers_lifecycle_turn_ended_at() {
        let work = tmp();
        {
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "se2".into(),
                prompt: "p".into(),
                worktree_path: Some(work.join("worktrees").join("se2").to_string_lossy().into()),
                ..Default::default()
            }).await.unwrap();
            store.update("se2", SessionPatch {
                status: Some("running".into()),
                ..Default::default()
            }).await.unwrap();
            store.append_log("se2", r#"{"type":"agentic_prompt","text":"p","at":1000}"#).await.unwrap();
            store.append_log("se2", r#"{"type":"result","subtype":"success","is_error":false}"#).await.unwrap();
            store.append_lifecycle("se2", &crate::engine::lifecycle::LifecycleEvent::TurnEnded {
                at: 2000,
                outcome: crate::engine::lifecycle::TurnOutcome::Success,
                cost_usd: None,
                duration_ms: None,
            }).await.unwrap();
        }
        let e = engine_from(&work).await;
        let s = e.get("se2").await.unwrap();
        assert_eq!(s.status, "done");
        assert_eq!(s.ended_at, Some(2000),
            "recover must prefer the authoritative lifecycle TurnEnded.at");
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
        e.0.store.create(CreateInput {
            id: "txb".into(),
            prompt: "p".into(),
            worktree_path: Some(work.join("worktrees").join("txb").to_string_lossy().into()),
            ..Default::default()
        }).await.unwrap();
        e.0.store.update("txb", SessionPatch {
            status: Some("done".into()),
            ended_at: Some(Some(5000)),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(e.get("txb").await.unwrap().ended_at, Some(5000), "stale endedAt seeded");

        // done -> pending (FollowUpQueued) clears endedAt.
        e.transition("txb", SessionStatus::Pending, TransitionReason::FollowUpQueued).await.unwrap();
        assert_eq!(e.get("txb").await.unwrap().ended_at, None,
            "FollowUpQueued must clear endedAt");

        // Re-stamp a stale endedAt while pending, then pending -> running (Start) clears it.
        e.0.store.update("txb", SessionPatch {
            ended_at: Some(Some(7000)),
            ..Default::default()
        }).await.unwrap();
        e.transition("txb", SessionStatus::Running, TransitionReason::Start).await.unwrap();
        assert_eq!(e.get("txb").await.unwrap().ended_at, None,
            "Start must clear endedAt");
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            store.create(CreateInput {
                id: "live-sess".into(),
                prompt: "p".into(),
                worktree_path: Some(live_dir.to_string_lossy().into()),
                ..Default::default()
            }).await.unwrap();
            store.update("live-sess", SessionPatch {
                status: Some("done".into()),
                ..Default::default()
            }).await.unwrap();
        }
        let _e = engine_from(&work).await;
        assert!(live_dir.exists(), "live session dir must be kept");
        assert!(!orphan_dir.exists(), "orphan dir must be removed by reconcile");
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
        assert!(looks_orphan.exists(), "with an empty store reconcile must delete nothing");
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
        assert_eq!(s.worktree_state, "discarded", "worktree_state should be discarded");
        // The worktree dir should be gone.
        if let Some(ref wt) = s.worktree_path {
            assert!(!std::path::Path::new(wt).exists(), "worktree dir should be removed after discard");
        }
    }

    /// discard: rejects a running session (busy); rejects an already-discarded session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn discard_rejects_running_and_already_cleaned() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
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
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        let err = e.discard(&id).await.unwrap_err().to_string();
        assert!(err.contains("busy") || err.contains("running"),
            "discard of running session must fail with busy: {err}");
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
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "done").await;
        let wt_path = e.get(&id).await.unwrap().worktree_path.clone();
        e.delete_session(&id, false).await.unwrap();
        assert!(e.get(&id).await.is_none(), "session row should be gone after delete");
        if let Some(ref wt) = wt_path {
            assert!(!std::path::Path::new(wt).exists(), "worktree dir should be removed after delete");
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
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        let err = e.delete_session(&id, false).await.unwrap_err().to_string();
        assert!(err.contains("busy") || err.contains("running"),
            "delete without force must fail with busy: {err}");
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
        let id = e.submit_session(vec![], vec![], "slow".into(), env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id, "running").await;
        let wt_path = e.get(&id).await.unwrap().worktree_path.clone();
        e.delete_session(&id, true).await.unwrap();
        assert!(e.get(&id).await.is_none(), "session row should be gone after force delete");
        if let Some(ref wt) = wt_path {
            assert!(!std::path::Path::new(wt).exists(), "worktree dir should be removed after force delete");
        }
    }

    /// delete_session with force=true removes a queued/pending session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_session_force_removes_queued_pending() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        // max_concurrent=1: slow first occupies slot; second stays pending.
        let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), ..Default::default() }).await;
        let mut slow_env = HashMap::new();
        slow_env.insert("FAKE_CLAUDE_SLEEP".into(), "120".into());
        let _blocker = e.submit_session(vec![], vec![], "blocker".into(), slow_env, SubmitMeta::default()).await.unwrap();
        wait_status(&e, &_blocker, "running").await;
        let id2 = e.submit_session(vec![], vec![], "target".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        wait_status(&e, &id2, "pending").await;
        let wt_path2 = e.get(&id2).await.unwrap().worktree_path.clone();
        e.delete_session(&id2, true).await.unwrap();
        assert!(e.get(&id2).await.is_none(), "queued session row should be gone after force delete");
        if let Some(ref wt) = wt_path2 {
            assert!(!std::path::Path::new(wt).exists(), "queued session worktree should be removed");
        }
        // Cleanup blocker.
        e.kill(&_blocker).await;
        wait_status(&e, &_blocker, "killed").await;
    }

    // ── Task 8: workflowRunning surfacing ─────────────────────

    /// with_activity surfaces workflowRunning=true for a finished session with a live workflow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reports_workflow_running_true_for_finished_session_with_live_workflow() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
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
        let uuid = s.claude_session_id.clone().expect("the fake bridge sets a claude_session_id");
        let wf_dir = e.0.cfg.claude_config_base
            .join("projects")
            .join("-slug")
            .join(&uuid)
            .join("subagents")
            .join("workflows")
            .join("wf_live");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(wf_dir.join("agent-a1.meta.json"), r#"{"agentType":"workflow-subagent"}"#).unwrap();
        // Now get() should surface workflow_running=true
        let s2 = e.get(&id).await.unwrap();
        assert_eq!(s2.workflow_running, Some(true),
            "workflowRunning should be true when a live agent meta file is present");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fires_push_fn_on_session_exit() {
        use std::sync::Arc;
        use parking_lot::Mutex;
        let src = tmp().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let b = bodies.clone();
        let push_fn: PushFn = Arc::new(move |v| b.lock().push(v));
        let engine = make_engine(&src, EngineOverrides {
            max_concurrent: Some(1),
            push_fn: Some(push_fn),
            ..Default::default()
        }).await;
        // repos empty → pure-skill scratch session, runs immediately under the fake-bridge fixture.
        let id = engine.submit_session(vec![], vec![], "go".into(),
            std::collections::HashMap::new(), SubmitMeta::default()).await.unwrap();
        // Wait for the session to reach "done" (poll the store like the other engine tests).
        for _ in 0..250 {
            if engine.get(&id).await.map(|s| s.status) == Some("done".into()) { break; }
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
        assert_eq!(got.len(), 1, "result+exit must not double-push, got: {:?}", *got);
    }

    /// Push errorText must include the crash-fallback message when a session fails
    /// with no prior error (re-reads final session state after store.update to pick up the fallback).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_error_text_contains_crash_fallback_for_failed_with_no_error() {
        use std::sync::Arc;
        use parking_lot::Mutex;
        let src = tmp().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let b = bodies.clone();
        let push_fn: PushFn = Arc::new(move |v| b.lock().push(v));
        // fake-sdk-bridge-crash.sh exits 1 without a result event → status="failed", error=None
        // before the patch. The exit handler should then set the crash-fallback error and the push
        // body should include that text.
        let engine = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-crash.sh")),
            max_concurrent: Some(1),
            push_fn: Some(push_fn),
            ..Default::default()
        }).await;
        let id = engine.submit_session(vec![], vec![], "go".into(),
            std::collections::HashMap::new(), SubmitMeta::default()).await.unwrap();
        for _ in 0..250 {
            let s = engine.get(&id).await.map(|s| s.status);
            if s == Some("failed".into()) || s == Some("done".into()) { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Give the fire-and-forget push a moment to land.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let got = bodies.lock();
        assert!(!got.is_empty(), "push_fn must fire on exit");
        let payload = &got[0];
        assert_eq!(payload["status"], "failed", "session must be failed, got {:?}", payload);
        let error_text = payload["errorText"].as_str().unwrap_or("");
        assert!(
            error_text.contains("interrupted") || error_text.contains("crashed") || error_text.contains("resume"),
            "errorText must contain crash-fallback message, got: {:?}", error_text
        );
    }

    /// max_concurrent=1: two queued sessions run in FIFO order — the second only starts
    /// after the first finishes (its slot frees). Asserts the second is still pending while
    /// the first runs, and reaches done only after the first does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_concurrent_one_runs_queue_in_fifo_and_starts_next_on_finish() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides { max_concurrent: Some(1), ..Default::default() }).await;
        // First is slow so it holds the single slot while the second waits.
        let mut slow = HashMap::new();
        slow.insert("FAKE_CLAUDE_SLEEP".into(), "2".into());
        let id1 = e.submit_session(vec![], vec![], "first".into(), slow, SubmitMeta::default()).await.unwrap();
        let id2 = e.submit_session(vec![], vec![], "second".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        // First is the head of the queue → it starts; second must wait.
        wait_status(&e, &id1, "running").await;
        assert_eq!(e.get(&id2).await.unwrap().status, "pending",
            "second (FIFO tail) must stay pending while first holds the only slot");
        // Second must not be in the running map while first occupies the slot.
        assert!(!e.0.state.lock().running.contains_key(&id2),
            "second must not be started before first frees its slot");
        // Finishing the first must free the slot and start the second.
        wait_status(&e, &id1, "done").await;
        wait_status(&e, &id2, "done").await;
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
        let e = make_engine(&src, EngineOverrides { sync_fn: Some(sync_fn), ..Default::default() }).await;
        let id = e.submit("demo", "go", HashMap::new()).await.unwrap();
        // Wait until start() is in the sync window: `starting` is populated, `running` is not.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !entered.load(std::sync::atomic::Ordering::SeqCst) {
            if std::time::Instant::now() > deadline { panic!("timed out waiting for start() to enter sync"); }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        {
            let st = e.0.state.lock();
            assert!(st.starting.contains(&id), "precondition: session is in `starting`");
            assert!(!st.running.contains_key(&id), "precondition: not yet in `running`");
        }
        // kill() here must take the not-running branch (run_handle is None).
        e.kill(&id).await;
        // Release start() — it should observe status=killed after sync and NOT spawn.
        let _ = tx.send(());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "killed", "kill while starting must stick as killed");
        assert!(!e.0.state.lock().running.contains_key(&id),
            "no process should be spawned when killed during the starting window");
    }


    /// interrupt() on a parked/awaiting session (after a Result, awaiting_input=true) clears
    /// pending_ask and forwards an interrupt to the live run WITHOUT changing status — the
    /// session stays running+parked, then a follow_up can still inject the next turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interrupt_on_parked_awaiting_session_keeps_it_alive() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "t1".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        // Wait until the session has parked after its first result.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        wait_until(|| {
            let st = e.0.state.lock();
            st.awaiting.get(&id) == Some(&true)
        }).await;
        // Seed a pending_ask to prove interrupt clears it on a parked session.
        e.0.state.lock().pending_ask.insert(id.clone());
        e.interrupt(&id);
        assert!(!e.0.state.lock().pending_ask.contains(&id),
            "interrupt must clear pending_ask even on a parked session");
        // Status unchanged: still running (parked), not killed/failed.
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.status, "running", "interrupt on parked session must not change status");
        assert_eq!(s.awaiting_input, Some(true), "still parked/awaiting after interrupt");
        // The live process survives → a follow_up still injects another turn.
        assert!(e.follow_up(&id, "t2", true, None, None, None).await.is_ok(),
            "session must still be live-injectable after interrupting a parked turn");
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }


    /// Two live follow_up injects in a row over one persistent process produce three total
    /// turns (initial + 2 injects), each logging an agentic_prompt marker in order, and the
    /// activity turn counter reaching 3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_live_inject_twice_in_a_row_streams_three_turns() {
        let src = tmp();
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides {
            bridge_path: Some(fixture("fake-sdk-bridge-stream.sh")),
            max_concurrent: Some(2),
            ..Default::default()
        }).await;
        let results = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let id = e.submit_session(vec![], vec![], "turn one".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        let r = results.clone();
        let _unsub = e.subscribe(&id, Box::new(move |ev| {
            if matches!(ev, ClaudeEvent::Result { .. }) {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        // Turn 1 result.
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        // First inject (turn 2).
        assert!(e.follow_up(&id, "turn two", true, None, None, None).await.is_ok());
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
        // Second inject right after (turn 3) — same live process, no re-queue.
        assert!(e.follow_up(&id, "turn three", true, None, None, None).await.is_ok());
        wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 3).await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.activity.as_ref().map(|a| a.turns), Some(3),
            "three turns total after two consecutive live injects");
        // All three prompts present in log order.
        let prompts: Vec<String> = e.get_log(&id).iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|o| o["type"] == "agentic_prompt")
            .map(|o| o["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(prompts, vec!["turn one", "turn two", "turn three"],
            "all three prompts logged in order");
        e.kill(&id).await;
        wait_status(&e, &id, "killed").await;
    }


mod fork_session {
    use super::*;
    use crate::engine::store::Session;
    use std::collections::HashMap;

    /// Build an engine with a single git-repo session whose HEAD we control, then return
    /// (engine, session_id, repo_path, sha). The repo has one commit ("first") and the
    /// session's worktree is checked out at that commit.
    async fn session_with_one_commit() -> (Engine, String, PathBuf, String) {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // Init a tiny repo and make one commit.
        let repo = dir.join("demo.git");
        std::fs::create_dir_all(&repo).unwrap();
        // Fixed commit dates so the two sibling repos below produce byte-identical commit SHAs
        // regardless of wall-clock — otherwise the two `git commit`s can straddle a 1-second
        // boundary and the `local_sha == sha` setup invariant flakes.
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "first\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first", "-q"]);
        let sha = String::from_utf8(
            std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo).output().unwrap().stdout
        ).unwrap().trim().to_string();

        // The engine expects a workspace under src_root containing the repo as a sibling
        // dir matching the repo name "demo". Copy the repo into place.
        let repo_dir = src.join("demo");
        let run2 = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo_dir)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(&repo_dir).unwrap();
        run2(&["init", "--initial-branch=main", "-q"]);
        run2(&["config", "user.email", "t@t"]);
        run2(&["config", "user.name", "t"]);
        std::fs::write(repo_dir.join("README.md"), "first\n").unwrap();
        run2(&["add", "."]);
        run2(&["commit", "-m", "first", "-q"]);
        let local_sha = String::from_utf8(
            std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo_dir).output().unwrap().stdout
        ).unwrap().trim().to_string();
        assert_eq!(local_sha, sha, "test setup invariant: copy has the same SHA");

        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(vec!["demo".into()], vec![], "first user prompt".into(), HashMap::new(), SubmitMeta::default()).await.unwrap();
        (e, id, repo_dir, local_sha)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_creates_new_session_branched_at_source_head() {
        let (e, src_id, _repo, sha) = session_with_one_commit().await;
        let forked: Session = e.fork_session(&src_id).await.unwrap();
        assert_ne!(forked.id, src_id);
        assert_eq!(forked.parent_session_id.as_deref(), Some(src_id.as_str()));
        assert_eq!(forked.status, "done");
        assert!(forked.prompt.starts_with("Fork of "), "seed prompt must be labelled: {}", forked.prompt);

        // The forked session's worktree exists and its HEAD equals the source HEAD.
        let wt = PathBuf::from(forked.worktree_path.unwrap());
        let repo_wt = wt.join(&forked.repos[0]);  // e.g. session_dir/demo
        let wt_sha = String::from_utf8(
            std::process::Command::new("git").args(["-C", &repo_wt.to_string_lossy(), "rev-parse", "HEAD"]).output().unwrap().stdout
        ).unwrap().trim().to_string();
        assert_eq!(wt_sha, sha, "forked worktree must point at source HEAD");

        // No claude process was spawned — there is no RunningTurn with the new id.
        let list = e.list().await;
        let fork_in_list = list.iter().find(|s| s.id == forked.id).unwrap();
        assert_eq!(fork_in_list.status, "done");
    }

    /// Regression: when the source has a non-empty transcript, the fork seed prompt must wrap
    /// the transcript in a clearly-delimited "# Context" block and tell claude to NOT continue
    /// the prior assistant turn — otherwise claude treats the transcript as its own prior
    /// output, tries to continue an `ASSISTANT: ...` line, hits unresolvable local context
    /// (paths, skill names), and errors with `[ede_diagnostic] stop_reason=tool_use` on the
    /// very first turn of the forked session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_seed_prompt_frames_transcript_as_context_not_continuation() {
        let (e, src_id, _repo, _sha) = session_with_one_commit().await;

        // Prime the source's stream-json log with a transcript so fork_session takes the
        // non-empty branch. We write directly to <log_dir>/<src_id>.jsonl — same path the
        // store appends to at runtime.
        let log_path = e.0.cfg.log_dir.join(format!("{src_id}.jsonl"));
        std::fs::write(&log_path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n"
        ).unwrap();

        let forked: Session = e.fork_session(&src_id).await.unwrap();
        assert!(forked.prompt.starts_with("Fork of "), "seed prompt must be labelled: {}", forked.prompt);
        assert!(forked.prompt.contains("# Context: previous session transcript"),
            "seed prompt must frame transcript as context: {}", forked.prompt);
        assert!(forked.prompt.contains("Do NOT continue the assistant's last turn"),
            "seed prompt must explicitly forbid continuation: {}", forked.prompt);
        assert!(forked.prompt.contains("Awaiting the user's next message"),
            "seed prompt must instruct claude to wait: {}", forked.prompt);
        // The transcript body must still appear, just framed.
        assert!(forked.prompt.contains("USER: hi"),
            "seed prompt must include transcript body: {}", forked.prompt);
        assert!(forked.prompt.contains("ASSISTANT: hello"),
            "seed prompt must include transcript body: {}", forked.prompt);
    }

    /// `@session:<id-prefix>` mentions expand against the store: the delivered text gains the
    /// mentioned session's full id + transcript log path, while text without mentions passes
    /// through unchanged. (The pure resolver is unit-tested in engine::mentions; this covers the
    /// Engine wrapper's store.list + log_path wiring.)
    #[tokio::test]
    async fn expand_session_mentions_resolves_against_store() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let _a = e
            .submit_session(vec![], vec![], "first".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        let b = e
            .submit_session(vec![], vec![], "second".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();

        let text = format!("check @session:{} progress", &b[..8]);
        let out = e.expand_session_mentions(&text).await;
        assert!(out.starts_with(&text), "original text must be kept: {out}");
        assert!(out.contains(&format!("session {b}")), "must resolve to the full id: {out}");
        assert!(
            out.contains(&format!("{b}.jsonl")),
            "must hand claude the transcript log path: {out}"
        );

        // No mention → identity.
        assert_eq!(e.expand_session_mentions("plain text").await, "plain text");
    }

    /// Regression: the seed prompt (source transcript) must actually reach claude on the fork's
    /// FIRST follow-up turn. Before the fix, fork_session stored the seed in the new session's
    /// `prompt` column but the follow-up path enqueued only the user's message — and a fresh fork
    /// has no `claude_session_id`, so `--resume` could not carry it either. The forked claude
    /// therefore started with zero knowledge of what it forked from. We assert the enqueued
    /// QueueItem carries the seed as `context_prefix` (NOT folded into the displayed `prompt`),
    /// and that `compose_turn_text` delivers both the seed and the user's message to claude.
    #[tokio::test]
    async fn fork_first_followup_delivers_seed_context_to_claude() {
        let (e, src_id, _repo, _sha) = session_with_one_commit().await;

        // Prime the source log so the fork's seed contains a real transcript.
        let log_path = e.0.cfg.log_dir.join(format!("{src_id}.jsonl"));
        std::fs::write(&log_path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"build a parser\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"parser done\"}]}}\n"
        ).unwrap();

        let forked: Session = e.fork_session(&src_id).await.unwrap();

        // First follow-up on the idle fork. `set_title=true` exercises the prompt-column overwrite
        // path: the seed must survive because follow_up captures it pre-patch.
        e.follow_up(&forked.id, "now add tests", true, None, None, None).await.unwrap();

        // The deferred pump has not run yet on the current-thread runtime, so the item is still
        // queued (same approach as follow_up_attaches_per_turn_overrides_to_queued_item).
        let pushed = {
            let st = e.0.state.lock();
            st.queue.iter().find(|q| q.id == forked.id).cloned()
                .expect("fork's first follow-up should enqueue a QueueItem")
        };

        // The user-visible/displayed message stays clean — no 50k transcript in the bubble.
        assert_eq!(pushed.prompt, "now add tests");

        // The seed rides along as context_prefix and includes the source transcript + framing.
        let prefix = pushed.context_prefix.as_deref()
            .expect("fork's first turn must carry the seed as context_prefix");
        assert!(prefix.contains("# Context: previous session transcript"),
            "context_prefix must carry the framed seed: {prefix}");
        assert!(prefix.contains("USER: build a parser"),
            "context_prefix must include the source transcript: {prefix}");
        assert!(prefix.contains("ASSISTANT: parser done"),
            "context_prefix must include the source transcript: {prefix}");

        // The text actually written to claude includes BOTH the seed and the user's message.
        let claude_text = compose_turn_text(&pushed);
        assert!(claude_text.contains("USER: build a parser"),
            "claude must receive the forked context: {claude_text}");
        assert!(claude_text.ends_with("now add tests"),
            "claude must receive the user's message after the context: {claude_text}");
    }

    /// A non-fork session's normal follow-up must NOT get a context_prefix (the seed-injection is
    /// fork-only). Guards against the gate accidentally firing for ordinary sessions.
    #[tokio::test]
    async fn non_fork_followup_has_no_context_prefix() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e
            .submit_session(vec![], vec![], "go".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;

        e.follow_up(&id, "second turn", false, None, None, None).await.unwrap();

        let pushed = {
            let st = e.0.state.lock();
            st.queue.iter().find(|q| q.id == id && q.prompt == "second turn").cloned()
                .expect("follow-up should enqueue a QueueItem")
        };
        assert!(pushed.context_prefix.is_none(),
            "non-fork follow-up must not carry a context_prefix");
    }

    /// `@session:` mention expansion is DELIVERY-time only: after a follow-up turn with a mention
    /// runs, the persisted `agentic_prompt` marker (the UI user bubble) must still carry the raw
    /// token and never the server-resolved block. Guards against a "cleanup" that folds the
    /// expanded text into the log marker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mention_expansion_never_leaks_into_the_prompt_marker() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let target = e
            .submit_session(vec![], vec![], "target".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        let asker = e
            .submit_session(vec![], vec![], "asker".into(), HashMap::new(), SubmitMeta::default())
            .await
            .unwrap();
        wait_status(&e, &asker, "done").await;

        let text = format!("look at @session:{}", &target[..8]);
        e.follow_up(&asker, &text, false, None, None, None).await.unwrap();
        wait_status(&e, &asker, "done").await;

        let log = e.0.store.read_log(&asker).join("\n");
        assert!(log.contains(&text), "raw mention marker must be logged: {log}");
        assert!(
            !log.contains("resolved by the server"),
            "expansion must never reach the log/UI bubble: {log}"
        );
    }

    /// Regression: a freshly-forked session must be idle (`status == "done"`) so the user can
    /// open it and send a follow-up. Before the fix, `Store::create` wrote `status = "pending"`,
    /// but the fork path did not enqueue, so `Engine::is_busy` permanently rejected follow-ups
    /// with `EngineError::Busy` ("session busy" 400). The forked session was unusable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_can_accept_follow_up_after_open() {
        let (e, src_id, _repo, _sha) = session_with_one_commit().await;
        let forked: Session = e.fork_session(&src_id).await.unwrap();

        // The forked session should be in `done` (idle) so follow_up can take it.
        // (status="pending" would block follow_up via is_busy.)
        let s = e.get(&forked.id).await.unwrap();
        assert_eq!(s.status, "done",
            "forked session must be idle (done) so follow_up is accepted, got: {}", s.status);

        // follow_up must succeed.
        e.follow_up(&forked.id, "first follow-up", true, None, None, None).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_unknown_src_returns_not_found() {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let err = e.fork_session("does-not-exist").await.unwrap_err();
        assert!(format!("{err}").contains("does-not-exist") || format!("{err}").contains("not found"),
            "expected NotFound, got: {err}");
    }

    /// Regression: `permission_mode` must be copied from the source row when forking. Before the
    /// fix, the fork silently dropped it (None), which could flip a `plan` session into bypass
    /// mode on the first follow-up turn — a permissions hole the user asked us to close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_copies_permission_mode_from_source() {
        let (e, src_id, _repo, _sha) = session_with_one_commit().await;
        // Stamp a non-default permission_mode on the source row directly.
        e.0.store
            .update(
                &src_id,
                crate::engine::store::SessionPatch {
                    permission_mode: Some("plan".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let forked: Session = e.fork_session(&src_id).await.unwrap();
        assert_eq!(
            forked.permission_mode.as_deref(),
            Some("plan"),
            "forked session must inherit the source's permission_mode"
        );
        // And re-reading from the store agrees.
        let persisted = e.get(&forked.id).await.unwrap();
        assert_eq!(persisted.permission_mode.as_deref(), Some("plan"));
    }

    /// Fork must carry the source's plugin blacklist forward (same rationale as the
    /// permission_mode copy above: silently dropping it would re-enable plugins the user
    /// explicitly disabled for the source session).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_session_copies_hidden_plugins_from_source() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        let src_id = e
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
                    hidden_skills: vec!["skill-x".into()],
                    hidden_plugins: vec!["github@claude-plugins-official".into()],
                    claude_md: None,
                    staged_uploads: vec![],
                },
            )
            .await
            .unwrap();
        wait_status(&e, &src_id, "done").await;

        let forked: Session = e.fork_session(&src_id).await.unwrap();
        assert_eq!(forked.hidden_skills, vec!["skill-x".to_string()]);
        assert_eq!(forked.hidden_plugins, vec!["github@claude-plugins-official".to_string()]);
        // And re-reading from the store agrees.
        let persisted = e.get(&forked.id).await.unwrap();
        assert_eq!(persisted.hidden_plugins, vec!["github@claude-plugins-official".to_string()]);
    }
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
            let store = Store::open(work.join("db.sqlite"), work.join("logs")).await.unwrap();
            // running + success result → recover to done
            store.create(CreateInput { id: "rr_ok".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
            store.update("rr_ok", SessionPatch { status: Some("running".into()), ..Default::default() }).await.unwrap();
            store.append_log("rr_ok", r#"{"type":"result","subtype":"success","is_error":false}"#).await.unwrap();
            // running + no result → recover to failed/interrupted
            store.create(CreateInput { id: "rr_int".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
            store.update("rr_int", SessionPatch { status: Some("running".into()), ..Default::default() }).await.unwrap();
            store.append_log("rr_int", r#"{"type":"text","text":"partial"}"#).await.unwrap();
            // already done → must be left untouched (no spurious re-finalize)
            store.create(CreateInput { id: "rr_done".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
            store.update("rr_done", SessionPatch { status: Some("done".into()), ..Default::default() }).await.unwrap();
            // pending with a real worktree dir → re-enqueued and runs to done
            store.create(CreateInput {
                id: "rp".into(),
                prompt: "go".into(),
                worktree_path: Some(pend_dir.to_string_lossy().into()),
                repos: vec![],
                ..Default::default()
            }).await.unwrap();
        }
        let e = engine_from(&work).await;
        assert_eq!(e.get("rr_ok").await.unwrap().status, "done",
            "running+success recovers to done");
        let int = e.get("rr_int").await.unwrap();
        assert_eq!(int.status, "failed", "running+no-result recovers to failed");
        assert_eq!(int.error_kind.as_deref(), Some("interrupted"),
            "running+no-result errorKind is interrupted");
        assert_eq!(e.get("rr_done").await.unwrap().status, "done",
            "already-done row is left untouched");
        // The pending row gets re-enqueued and runs.
        wait_status(&e, "rp", "done").await;
    }


    /// submit_session with multiple repos creates per-repo worktrees, writes the multi-repo
    /// session guide (CLAUDE.md) into the session dir, and runs to done with cwd = session dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_with_multiple_repos_creates_worktrees_and_session_guide() {
        let src = tmp();
        make_temp_git_repo_in(&src, "repoA");
        make_temp_git_repo_in(&src, "repoB");
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(
            vec!["repoA".into(), "repoB".into()],
            vec![],
            "do multi".into(),
            HashMap::new(),
            SubmitMeta::default(),
        ).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        assert_eq!(s.repos, vec!["repoA".to_string(), "repoB".to_string()],
            "both repos recorded on the session");
        let wt = s.worktree_path.as_ref().expect("multi-repo session has a worktree_path");
        let wt_dir = std::path::Path::new(wt);
        // Each repo got its own worktree subdir.
        assert!(wt_dir.join("repoA").exists(), "repoA worktree subdir must exist");
        assert!(wt_dir.join("repoB").exists(), "repoB worktree subdir must exist");
        // Multi-repo orientation guide written into the session dir (gated on wts.len() > 1).
        assert!(wt_dir.join("CLAUDE.md").exists(),
            "multi-repo session must get a CLAUDE.md orientation guide");
        // Per-repo base shas captured.
        assert!(s.base_shas.get("repoA").map(|o| o.is_some()).unwrap_or(false),
            "repoA base sha recorded");
        assert!(s.base_shas.get("repoB").map(|o| o.is_some()).unwrap_or(false),
            "repoB base sha recorded");
    }

    /// submit_session with a custom CLAUDE.md (single repo) writes it into the session dir verbatim.
    /// Single-repo gets no orientation guide, so the file is the user's content alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_with_custom_claude_md_writes_it_into_session_dir() {
        let src = tmp();
        make_temp_git_repo_in(&src, "repoA");
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(
            vec!["repoA".into()],
            vec![],
            "do it".into(),
            HashMap::new(),
            SubmitMeta { claude_md: Some("Run my tests before committing.".into()), ..Default::default() },
        ).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        let wt_dir = std::path::Path::new(s.worktree_path.as_ref().expect("session has a worktree_path"));
        let body = std::fs::read_to_string(wt_dir.join("CLAUDE.md"))
            .expect("custom CLAUDE.md must be written into the session dir");
        assert!(body.contains("Run my tests before committing."), "custom guidance must be present");
        assert!(!body.contains("# agentic-dev multi-repo session"),
            "single-repo session must NOT get the multi-repo orientation guide");
    }

    /// Multi-repo + custom CLAUDE.md combines both into one file, separated by a horizontal rule:
    /// orientation guide first, the user's session-scoped guidance after.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_multi_repo_with_custom_claude_md_combines_guide_and_custom() {
        let src = tmp();
        make_temp_git_repo_in(&src, "repoA");
        make_temp_git_repo_in(&src, "repoB");
        let e = make_engine(&src, EngineOverrides::default()).await;
        let id = e.submit_session(
            vec!["repoA".into(), "repoB".into()],
            vec![],
            "do multi".into(),
            HashMap::new(),
            SubmitMeta { claude_md: Some("No new dependencies.".into()), ..Default::default() },
        ).await.unwrap();
        wait_status(&e, &id, "done").await;
        let s = e.get(&id).await.unwrap();
        let wt_dir = std::path::Path::new(s.worktree_path.as_ref().expect("session has a worktree_path"));
        let body = std::fs::read_to_string(wt_dir.join("CLAUDE.md")).unwrap();
        assert!(body.contains("# agentic-dev multi-repo session"), "orientation guide present");
        assert!(body.contains("No new dependencies."), "custom guidance present");
        assert!(body.contains("\n---\n"), "guide and custom guidance separated by a horizontal rule");
        // Orientation guide comes before the user's custom section.
        assert!(body.find("# agentic-dev multi-repo session").unwrap() < body.find("No new dependencies.").unwrap(),
            "orientation guide must precede the custom guidance");
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

        let id = e.submit_session(
            vec!["repoA".into()],
            vec![],
            "look at [attached: uploads/shot.png]".into(),
            HashMap::new(),
            SubmitMeta {
                staged_uploads: vec![StagedUpload { token: token.clone(), name: "shot.png".into() }],
                ..Default::default()
            },
        ).await.unwrap();
        let s = e.get(&id).await.unwrap();
        let wt_dir = std::path::Path::new(s.worktree_path.as_ref().expect("session has a worktree_path"));
        // Single-repo cwd is session_dir/<repo>, so adopted uploads live there.
        let adopted = wt_dir.join("repoA").join("uploads").join("shot.png");
        let bytes = std::fs::read(&adopted).expect("staged file must be adopted into uploads/");
        assert_eq!(bytes, b"PNGDATA", "adopted file keeps its bytes");
        assert!(!token_dir.exists(), "staging token dir must be removed after adoption");
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

        let id = e.submit_session(
            vec!["repoA".into()],
            vec![],
            "go".into(),
            HashMap::new(),
            SubmitMeta {
                staged_uploads: vec![StagedUpload { token: token.clone(), name: raw_name.into() }],
                ..Default::default()
            },
        ).await.unwrap();
        let s = e.get(&id).await.unwrap();
        let wt_dir = std::path::Path::new(s.worktree_path.as_ref().expect("worktree_path"));
        let uploads = wt_dir.join("repoA").join("uploads");
        assert!(uploads.join(&safe_name).exists(), "file lands under uploads/ with a sanitized name");
        // No directory escape: nothing was written above the uploads dir.
        assert!(!wt_dir.join("escape.png").exists(), "must not escape into the worktree root");
    }

    #[tokio::test]
    async fn perm_event_sets_pending_perm_and_resolved_clears_it() {
        use crate::engine::stream::ClaudeEvent;
        use serde_json::json;
        let e = test_engine().await;
        let id = "perm-sess";
        e.0.store.create(crate::engine::store::CreateInput { id: id.into(), prompt: "p".into(), ..Default::default() })
            .await.unwrap();
        // A parked perm marks the session pending_perm.
        e.on_event(id, ClaudeEvent::Perm { id: "perm-1".into(), tool: "Bash".into(), input: json!({}), raw: json!({}) }).await;
        assert!(e.0.state.lock().pending_perm.contains(id), "perm parks the session");
        // Resolving it clears the flag.
        e.on_event(id, ClaudeEvent::PermResolved { id: "perm-1".into(), decision: "allow".into(), raw: json!({}) }).await;
        assert!(!e.0.state.lock().pending_perm.contains(id), "resolved clears pending_perm");
    }

    #[tokio::test]
    async fn respond_permission_clears_pending_perm_and_parked() {
        use crate::engine::stream::ClaudeEvent;
        use serde_json::json;
        let e = test_engine().await;
        let id = "perm-resp";
        e.0.store.create(crate::engine::store::CreateInput { id: id.into(), prompt: "p".into(), ..Default::default() })
            .await.unwrap();
        // A parked perm arms the watchdog exemption + the pending_prompt payload.
        e.on_event(id, ClaudeEvent::Perm { id: "perm-1".into(), tool: "Bash".into(), input: json!({}), raw: json!({}) }).await;
        assert!(e.0.state.lock().pending_perm.contains(id), "perm parks the session");
        assert!(e.0.state.lock().parked.contains_key(id), "perm records the parked payload");
        // respond_permission must clear both so the watchdog can reap once the turn proceeds, even when
        // the session has no live handle (forwarding is then a no-op — exactly this test's case).
        e.respond_permission(id, "allow", None);
        assert!(!e.0.state.lock().pending_perm.contains(id), "respond clears pending_perm");
        assert!(!e.0.state.lock().parked.contains_key(id), "respond clears parked");
    }

    #[tokio::test]
    async fn ask_event_exposes_pending_prompt_and_resolves_clear_it() {
        use crate::engine::stream::ClaudeEvent;
        use serde_json::json;
        let e = test_engine().await;
        let id = "park-sess";
        e.0.store.create(crate::engine::store::CreateInput { id: id.into(), prompt: "p".into(), ..Default::default() })
            .await.unwrap();
        // An Ask event makes pending_prompt authoritative on the Session (kind "ask").
        e.on_event(id, ClaudeEvent::Ask { questions: vec![json!({"question":"A or B?"})], parent_tool_use_id: None, raw: json!({}) }).await;
        let s = e.get(id).await.unwrap();
        assert_eq!(s.pending_prompt.as_ref().and_then(|v| v.get("kind")).and_then(|k| k.as_str()), Some("ask"));
        // A Result clears it.
        e.on_event(id, ClaudeEvent::Result { is_error: false, cost_usd: None, text: None, raw: json!({}) }).await;
        assert!(e.get(id).await.unwrap().pending_prompt.is_none());
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
                    claude_md: None,
                    staged_uploads: vec![],
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
                    claude_md: None,
                    staged_uploads: vec![],
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
                    claude_md: None,
                    staged_uploads: vec![],
                },
            )
            .await
            .unwrap();

        let s = e.0.store.get(&id).await.unwrap().unwrap();
        // Store keeps the raw blacklist (API/DB model unchanged) …
        assert_eq!(s.hidden_plugins, vec!["superpowers@claude-plugins-official".to_string()]);

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
            worktree_path: Some(e.0.cfg.worktrees_root.join("s1").to_string_lossy().into_owned()),
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
            e.0.cfg.worktrees_root.join("s1").join("agentic-dev").to_string_lossy().to_string(),
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
        let slug = wt.to_string_lossy().replace(|c: char| !c.is_ascii_alphanumeric(), "-");
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
        assert_eq!(opts.resume_session_id.as_deref(), Some("00000000-0000-0000-0000-000000000001"));
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
        assert_eq!(opts.resume_session_id, None, "missing transcript → resume must be dropped");
        assert_eq!(opts.cwd, wt.to_string_lossy().to_string(), "cwd still worktree root");
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
        assert_eq!(opts.cwd, e.0.cfg.worktrees_root.to_string_lossy().to_string());
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
                    claude_md: None,
                    staged_uploads: vec![],
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


mod submit_titles_via_generator {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::engine::title_client::{
        InMemoryTitleGenerator, TitleGenerator,
    };

    /// Integration tests for submit_session title generation and
    /// follow_up periodic retitle. All paths go through the
    /// `TitleGenerator` trait; tests inject an `InMemoryTitleGenerator`
    /// via `EngineOverrides.title_generator`.

    async fn submit_with_generator(
        generator: Arc<dyn TitleGenerator>,
    ) -> (Engine, String) {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(generator),
                ..Default::default()
            },
        )
        .await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        (e, id)
    }

    /// submit_session spawns a fire-and-forget title task that lands
    /// "性能优化阶段" on success. Poll until it does (or timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_uses_generator_title() {
        let (e, id) = submit_with_generator(Arc::new(
            InMemoryTitleGenerator::returning_title("性能优化阶段"),
        ))
        .await;
        let mut landed = false;
        for _ in 0..40 {
            if e.get(&id).await.unwrap().prompt == "性能优化阶段" {
                landed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(landed, "submit-time title never landed");
        assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
    }

    /// If the generator returns `None` (or returns garbage that fails
    /// validation), the original prompt is kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_keeps_prompt_when_generator_returns_none() {
        let (e, id) = submit_with_generator(Arc::new(
            InMemoryTitleGenerator::returning_none_for_retitle(),
        ))
        .await;
        // Even after waiting for the title task to run, the prompt is
        // unchanged because the generator returned None.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(e.get(&id).await.unwrap().prompt, "first prompt");
    }

    /// submit_session returns synchronously — the title task is
    /// fire-and-forget on tokio::spawn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn submit_session_returns_immediately() {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(Arc::new(
                    InMemoryTitleGenerator::returning_title("性能优化阶段"),
                )),
                ..Default::default()
            },
        )
        .await;
        let started = std::time::Instant::now();
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "submit_session must not block on title generation; took {elapsed:?}"
        );
        wait_status(&e, &id, "done").await;
    }

    /// follow_up on the 5th turn spawns a retitle task. The in-memory
    /// generator is configured to return a NEW title (different from the
    /// submit-time title so dedup doesn't kick in) and the test polls
    /// until the prompt flips.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retitle_after_fifth_followup_changes_title() {
        let generator = Arc::new(InMemoryTitleGenerator {
            next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
            next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
        });
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(generator),
                ..Default::default()
            },
        )
        .await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        assert_eq!(e.get(&id).await.unwrap().prompt, "性能优化阶段");
        for i in 0..4 {
            e.follow_up(&id, &format!("f{i}"), false, None, None, None)
                .await
                .unwrap();
            wait_status(&e, &id, "done").await;
        }
        e.follow_up(&id, "fifth", false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        let mut landed = false;
        for _ in 0..40 {
            if e.get(&id).await.unwrap().prompt == "性能优化阶段二号" {
                landed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(landed, "5th-turn retitle never landed");
    }

    /// retitle_enabled=false suppresses the 5th-turn retitle task — the
    /// prompt must not flip even if the generator would have returned
    /// change=true.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retitle_disabled_does_not_change_title() {
        let generator = Arc::new(InMemoryTitleGenerator {
            next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
            next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
        });
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(generator),
                retitle_enabled: Some(false),
                ..Default::default()
            },
        )
        .await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        for i in 0..4 {
            e.follow_up(&id, &format!("f{i}"), false, None, None, None)
                .await
                .unwrap();
            wait_status(&e, &id, "done").await;
        }
        e.follow_up(&id, "fifth", false, None, None, None)
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let prompt = e.get(&id).await.unwrap().prompt;
        assert_ne!(
            prompt, "性能优化阶段二号",
            "retitle must not run when retitle_enabled=false; got {prompt:?}"
        );
    }

    /// A user-pinned title (set via a set_title=true follow_up) must NOT be
    /// overwritten by the periodic retitle, even when the cadence boundary is
    /// hit and the generator would return a change. Guards P0#1 (title_pinned).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pinned_title_survives_periodic_retitle() {
        let generator = Arc::new(InMemoryTitleGenerator {
            next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
            next_retitle: std::sync::Mutex::new(Some(Some("机器改的标题".to_string()))),
        });
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(generator),
                ..Default::default()
            },
        )
        .await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap(); // turn 1
        wait_status(&e, &id, "done").await;
        // User manually renames the session → pins the title.
        e.follow_up(&id, "用户钉的标题", true, None, None, None)
            .await
            .unwrap(); // turn 2
        wait_status(&e, &id, "done").await;
        assert_eq!(e.get(&id).await.unwrap().prompt, "用户钉的标题");
        assert!(
            e.get(&id).await.unwrap().title_pinned,
            "set_title=true must set title_pinned"
        );
        // Drive to the 5-turn cadence boundary (turn 5) so a retitle fires.
        for i in 0..3 {
            e.follow_up(&id, &format!("f{i}"), false, None, None, None)
                .await
                .unwrap();
            wait_status(&e, &id, "done").await;
        }
        // Give any spawned retitle task time to (not) land.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let prompt = e.get(&id).await.unwrap().prompt;
        assert_eq!(
            prompt, "用户钉的标题",
            "pinned title must survive periodic retitle; got {prompt:?}"
        );
    }

    /// follow_up must NOT block on the retitle task even if the
    /// underlying title generator were slow. The in-memory generator
    /// is instant, but the key assertion is that the handler returns
    /// immediately because the retitle is fire-and-forget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn followup_returns_immediately_with_retitle_spawned() {
        let generator = Arc::new(InMemoryTitleGenerator {
            next_generate: std::sync::Mutex::new(Some("性能优化阶段".to_string())),
            next_retitle: std::sync::Mutex::new(Some(Some("性能优化阶段二号".to_string()))),
        });
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let e = make_engine(
            &src,
            EngineOverrides {
                title_generator: Some(generator),
                ..Default::default()
            },
        )
        .await;
        let id = e
            .submit_session(
                vec![],
                vec![],
                "first prompt".into(),
                HashMap::new(),
                SubmitMeta::default(),
            )
            .await
            .unwrap();
        wait_status(&e, &id, "done").await;
        for i in 0..4 {
            e.follow_up(&id, &format!("f{i}"), false, None, None, None)
                .await
                .unwrap();
            wait_status(&e, &id, "done").await;
        }
        let started = std::time::Instant::now();
        e.follow_up(&id, "fifth", false, None, None, None)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "follow_up must not block on the retitle task; took {elapsed:?}"
        );
    }

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
        let e = make_engine(&src, EngineOverrides { now_fn: Some(now_fn), ..Default::default() }).await;
        seed_limited_session(&e, "s1", &format!("Claude AI usage limit reached|{reset_s}")).await;

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
        let e = make_engine(&src, EngineOverrides { now_fn: Some(now_fn), ..Default::default() }).await;
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
        assert_ne!(s.status, "failed", "session re-enqueued (pending/running/done)");
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
            EngineOverrides { now_fn: Some(now_fn), usage_fn: Some(usage), ..Default::default() },
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
            EngineOverrides { now_fn: Some(now_fn), usage_fn: Some(usage), ..Default::default() },
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
        let e = make_engine(&src, EngineOverrides { now_fn: Some(now_fn), ..Default::default() }).await;

        // Toggle OFF → never scheduled.
        seed_limited_session(&e, "off", "usage limit reached|1800003600").await;
        e.0.store
            .update("off", SessionPatch { auto_resume: Some(false), ..Default::default() })
            .await
            .unwrap();
        // Killed → deliberate stop, never scheduled even with the toggle on.
        seed_limited_session(&e, "killed", "usage limit reached|1800003600").await;
        e.0.store
            .update("killed", SessionPatch { status: Some("killed".into()), ..Default::default() })
            .await
            .unwrap();

        e.trigger_auto_resume().await;

        assert_eq!(e.0.store.get("off").await.unwrap().unwrap().auto_resume_at, None);
        assert_eq!(e.0.store.get("killed").await.unwrap().unwrap().auto_resume_at, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn manual_follow_up_supersedes_scheduled_auto_resume() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        seed_limited_session(&e, "s1", "usage limit reached").await;
        e.0.store
            .update(
                "s1",
                SessionPatch { auto_resume_at: Some(Some(9_999_999_999_999)), ..Default::default() },
            )
            .await
            .unwrap();

        e.follow_up("s1", "user takes over", false, None, None, None).await.unwrap();

        let s = e.0.store.get("s1").await.unwrap().unwrap();
        assert_eq!(s.auto_resume_at, None, "manual follow-up cancels the scheduled resume");
    }

    #[tokio::test]
    async fn kill_cancels_scheduled_auto_resume_even_on_terminal_session() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        seed_limited_session(&e, "s1", "usage limit reached").await;
        e.0.store
            .update("s1", SessionPatch { auto_resume_at: Some(Some(9_999_999_999_999)), ..Default::default() })
            .await
            .unwrap();

        // The session is already FAILED (terminal) — kill() must still cancel the schedule,
        // otherwise a user "stop" is ignored and the scheduler resurrects the session later.
        e.kill("s1").await;

        let s = e.0.store.get("s1").await.unwrap().unwrap();
        assert_eq!(s.auto_resume_at, None, "kill must cancel a scheduled auto-resume");
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
        assert_eq!(e.0.store.get("s1").await.unwrap().unwrap().auto_resume_at, Some(5000));

        // A user follow-up cleared the episode → a stale schedule must NOT land.
        e.0.store
            .update(
                "s1",
                SessionPatch { error_kind: Some(None), auto_resume_at: Some(None), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(!e.0.store.schedule_auto_resume("s1", 7000).await.unwrap());
        assert_eq!(e.0.store.get("s1").await.unwrap().unwrap().auto_resume_at, None);
    }

    #[tokio::test]
    async fn claim_auto_resume_wins_exactly_once() {
        let src = tmp();
        let e = make_engine(&src, EngineOverrides::default()).await;
        seed_limited_session(&e, "s1", "usage limit reached").await;
        e.0.store
            .update("s1", SessionPatch { auto_resume_at: Some(Some(1000)), ..Default::default() })
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
            .create(crate::engine::store::CreateInput { id: "s1".into(), prompt: "x".into(), ..Default::default() })
            .await
            .unwrap();
        let s = e.0.store.get("s1").await.unwrap().unwrap();
        assert!(s.auto_resume, "default ON");
        assert_eq!(s.auto_resume_at, None);

        e.0.store
            .update("s1", SessionPatch { auto_resume: Some(false), ..Default::default() })
            .await
            .unwrap();
        let s = e.0.store.get("s1").await.unwrap().unwrap();
        assert!(!s.auto_resume, "toggle persists");

        // Wire format: autoResume always present; autoResumeAt omitted when unscheduled.
        let wire = serde_json::to_value(&s).unwrap();
        assert_eq!(wire["autoResume"], serde_json::json!(false));
        assert!(wire.get("autoResumeAt").is_none());
    }

    // ── Task 4: reconcile_from_native ─────────────────────────
    //
    // Watermark-based import of the native transcript (#2) delta into the
    // rendered log (#1): the first reconcile imports everything past the
    // watermark, a repeat call with no new native lines imports nothing
    // (idempotent), and a later terminal turn is picked up exactly once.
    #[tokio::test]
    async fn reconcile_imports_delta_and_is_idempotent() {
        use crate::engine::native_transcript;
        use std::io::Write;

        let work = tmp();
        let e = engine_from(&work).await; // claude_config_base = work/claude-config
        let id = "sess-recon";

        // Adopted-style row: origin=adopted, worktree_path = a cwd we control.
        // CreateInput has no claude_session_id builder, so we set it via a patch.
        let cwd = work.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy().to_string();
        e.0.store
            .create(CreateInput {
                id: id.into(),
                origin: Some("adopted".into()),
                worktree_path: Some(cwd_s.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        e.0.store
            .update(
                id,
                SessionPatch {
                    claude_session_id: Some(Some("csidR".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Write a native transcript (#2) at the slug path the engine will compute.
        let tp = native_transcript::transcript_path(
            &e.0.cfg.claude_config_base,
            &cwd_s,
            "csidR",
        );
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(
            &tp,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        // First reconcile imports the single user prompt → one agentic_prompt line.
        let n = e.reconcile_from_native(id).await.unwrap();
        assert_eq!(n, 1, "first reconcile imports the one user prompt");
        // Idempotent: no new native lines → nothing appended.
        assert_eq!(
            e.reconcile_from_native(id).await.unwrap(),
            0,
            "second reconcile with no delta is a no-op"
        );

        // A terminal assistant turn arrives while agentic-dev was stopped.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&tp)
            .unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_9\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"yo\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();
        // end_turn assistant → assistant line + result turn-boundary marker = 2.
        assert_eq!(
            e.reconcile_from_native(id).await.unwrap(),
            2,
            "delta reconcile pulls assistant + result for the terminal turn"
        );

        // #1 now renders the imported history.
        let log = e.0.store.read_log(id);
        assert!(
            log.iter().any(|l| l.contains("\"agentic_prompt\"")),
            "log carries the imported user prompt"
        );
        assert!(
            log.iter().any(|l| l.contains("\"assistant\"")),
            "log carries the imported assistant turn"
        );
    }

    // ── Task 5: adopt_session ─────────────────────────────────
    //
    // Adopt an existing external Claude session: create a first-class
    // agentic-dev row pointing at the external `claudeSessionId`, seed the
    // prompt/title from the first native user turn, and import the FULL native
    // history (#2) into the rendered log (#1). Worktree strategy is
    // adopt-in-place — `worktree_path` is the native cwd, no new git worktree.
    #[tokio::test]
    async fn adopt_creates_row_and_imports_full_history() {
        use crate::engine::native_transcript;

        let work = tmp();
        let e = engine_from(&work).await; // claude_config_base = work/claude-config

        // The native session ran in this cwd (a plain, non-git dir).
        let cwd = work.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy().to_string();

        // Write the native transcript (#2) at the slug path the engine computes:
        // one authored user turn + one end_turn assistant turn = 2 native lines.
        let tp = native_transcript::transcript_path(
            &e.0.cfg.claude_config_base,
            &cwd_s,
            "csidX",
        );
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(
            &tp,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();

        let id = e.adopt_session("csidX", &cwd_s).await.unwrap();

        let s = e.0.store.get(&id).await.unwrap().unwrap();
        assert_eq!(s.origin, "adopted", "row is marked adopted provenance");
        assert_eq!(
            s.claude_session_id.as_deref(),
            Some("csidX"),
            "row points at the external Claude session id"
        );
        assert_eq!(
            s.worktree_path.as_deref(),
            Some(cwd_s.as_str()),
            "adopt-in-place: worktree_path is the native cwd"
        );
        assert_eq!(s.status, "pending", "adopted row is pending until opened");
        assert_eq!(
            s.prompt, "first prompt",
            "prompt/title seeded from the first native user turn"
        );
        assert_eq!(
            s.native_watermark_lines, 2,
            "watermark set to the native line count after full import"
        );

        // #1 now renders the imported history (agentic_prompt + assistant).
        let log = e.0.store.read_log(&id);
        assert!(
            log.iter().any(|l| l.contains("\"agentic_prompt\"")),
            "imported log carries the user prompt"
        );
        assert!(
            log.iter().any(|l| l.contains("\"assistant\"")),
            "imported log carries the assistant turn"
        );

        // Double-adopt is rejected.
        assert!(
            e.adopt_session("csidX", &cwd_s).await.is_err(),
            "adopting an already-adopted csid is an error"
        );
    }

    /// `config_base()` is a thin clone of the configured claude config dir — the base the
    /// API layer passes to `scan_adoptable`. Backs the `GET /api/adoptable` handler.
    #[tokio::test]
    async fn config_base_returns_configured_claude_config_base() {
        let work = tmp();
        let e = engine_from(&work).await;
        assert_eq!(e.config_base(), work.join("claude-config"));
    }

    /// `known_claude_session_ids()` is the exclusion set for `scan_adoptable`: it collects
    /// the linked csids across all stored sessions and skips rows with none. Backs the
    /// `GET /api/adoptable` handler's "hide already-adopted" behavior.
    #[tokio::test]
    async fn known_claude_session_ids_collects_only_linked_csids() {
        use crate::engine::store::{CreateInput, SessionPatch};
        let work = tmp();
        let e = engine_from(&work).await;

        // Fresh store → no linked csids.
        assert!(e.known_claude_session_ids().await.is_empty(), "fresh store has no linked csids");

        // Row A: linked to a csid.
        e.0.store.create(CreateInput { id: "a".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
        e.0.store.update("a", SessionPatch {
            claude_session_id: Some(Some("csidA".into())), ..Default::default()
        }).await.unwrap();
        // Row B: no csid.
        e.0.store.create(CreateInput { id: "b".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();

        let known = e.known_claude_session_ids().await;
        assert!(known.contains("csidA"), "linked csid must be present, got {known:?}");
        assert_eq!(known.len(), 1, "rows without a claudeSessionId are excluded, got {known:?}");
    }

    /// If a post-create step fails (here: `reconcile_from_native`'s history import),
    /// the half-created row must be rolled back — otherwise its `claudeSessionId`
    /// permanently locks the csid against re-adoption via the `session_by_csid` guard.
    ///
    /// Trigger: `reconcile_from_native` calls `store.append_log`, which opens
    /// `<log_dir>/<id>.jsonl` with `OpenOptions::create(true)`. Stripping the write bit
    /// from `log_dir` (known up front from `EngineConfig::log_dir`, unlike the
    /// randomly-generated session id) makes that open fail with a real permission-denied
    /// IO error — a realistic failure a step after the row + csid are already persisted,
    /// without needing to mock the store.
    #[tokio::test]
    async fn adopt_rolls_back_on_reconcile_failure() {
        use crate::engine::native_transcript;
        use std::os::unix::fs::PermissionsExt;

        let work = tmp();
        let e = engine_from(&work).await;

        let cwd = work.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy().to_string();

        // Native transcript with at least one line, so reconcile_from_native reaches
        // append_log (an empty transcript would short-circuit at `translate_range`
        // returning nothing to append, never touching the log file).
        let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidY");
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(
            &tp,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n",
        )
        .unwrap();

        let log_dir = e.0.cfg.log_dir.clone();
        std::fs::create_dir_all(&log_dir).unwrap();
        let orig_perms = std::fs::metadata(&log_dir).unwrap().permissions();
        // r-x only: append_log's OpenOptions::create can't create a new file in a
        // directory it can't write to, so the append fails with a permission error.
        std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = e.adopt_session("csidY", &cwd_s).await;

        // Restore permissions immediately so tempdir cleanup (and any assertions below)
        // don't themselves trip over the locked-down directory.
        std::fs::set_permissions(&log_dir, orig_perms).unwrap();

        assert!(
            result.is_err(),
            "adopt_session must surface the reconcile failure, not silently succeed"
        );
        assert!(
            e.0.store
                .session_by_csid("csidY")
                .await
                .unwrap()
                .is_none(),
            "the half-created row must be rolled back so the csid can be re-adopted"
        );
    }

    // ── Task 7: detach_session + reconcile on reopen ──────────
    //
    // Detach hands an adopted session off to a terminal `claude`: hard-stop the
    // live streaming process (single-writer), freeze the watermark at the current
    // native line count, mark `detached`, and hand back a `--resume` command.
    // A later terminal turn is then pulled in by `reconcile_from_native` on reopen.
    #[tokio::test]
    async fn detach_sets_flag_watermark_and_resume_cmd() {
        use crate::engine::native_transcript;
        use std::io::Write;

        let work = tmp();
        let e = engine_from(&work).await; // claude_config_base = work/claude-config

        // Adopt an external session running in `cwd` with one native user turn (#2).
        let cwd = work.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy().to_string();
        let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidD");
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(
            &tp,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();
        let id = e.adopt_session("csidD", &cwd_s).await.unwrap();

        // Detach: returns the csid + a `--resume` command, freezes the watermark,
        // and flips the `detached` flag.
        let info = e.detach_session(&id).await.unwrap();
        assert_eq!(info.claude_session_id, "csidD");
        assert_eq!(info.cwd, cwd_s, "cwd echoes the adopted worktree path");
        assert!(
            info.resume_cmd.contains("claude --resume csidD"),
            "resume_cmd carries the terminal `--resume` invocation"
        );

        let s = e.0.store.get(&id).await.unwrap().unwrap();
        assert!(s.detached, "detach sets the single-writer guard flag");
        assert_eq!(
            s.native_watermark_lines, 1,
            "watermark frozen at the native line count present at detach"
        );

        // The terminal adds a turn while agentic-dev is detached.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&tp)
            .unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();

        // Reconcile on reopen pulls exactly the terminal turn: assistant + result = 2.
        let n = e.reconcile_from_native(&id).await.unwrap();
        assert_eq!(n, 2, "delta reconcile pulls the terminal assistant + result");
        e.0.store.set_detached(&id, false).await.unwrap();
    }
}
