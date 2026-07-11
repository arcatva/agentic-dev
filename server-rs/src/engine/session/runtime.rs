use std::path::PathBuf;

use crate::engine::classify_error::classify_claude_error;
// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
use crate::engine::runner::Runner;
use crate::engine::spawner::{compose_user_text, encode_user_message, spawn_claude, SpawnOptions};
use crate::engine::status::SessionStatus;
use crate::engine::store::{Field, Session, SessionPatch, SessionUpdate};
use crate::engine::stream::ClaudeEvent;
use crate::engine::transition::TransitionReason;
use crate::engine::worktree::sync_worktree;

use crate::engine::*;

impl Engine {
    /// Defer a pump() call to the next async tick.
    pub(crate) fn defer_pump(&self) {
        let inner = self.0.clone();
        tokio::spawn(async move { Engine(inner).pump() });
    }

    /// Pump: start queued items up to max_concurrent.
    pub(crate) fn pump(&self) {
        loop {
            // Short critical section: check closed, active_count, pop from queue.
            let item = {
                let mut state = self.0.state.lock();
                if state.closed {
                    break;
                }
                let max = self.0.cfg.max_concurrent.unwrap_or(u64::MAX);
                if Engine::active_count(&state) >= max {
                    break;
                }
                if state.queue.is_empty() {
                    break;
                }
                let item = state
                    .queue
                    .pop_front()
                    .expect("queue non-empty (checked above)");
                state.starting.insert(item.id.clone());
                item
            };

            // Spawn the start task.
            let engine = self.clone();
            let id = item.id.clone();
            tokio::spawn(async move {
                let result = engine.start(item).await;
                // catch: on error, mark failed
                if let Err(msg) = result {
                    let closed = engine.0.state.lock().closed;
                    if !closed {
                        // PR6: route through transition() with the
                        // StartFailed reason. transition() owns the
                        // status=Failed + error+errorKind+ended_at patch.
                        if let Err(e) = engine
                            .transition(
                                &id,
                                SessionStatus::Failed,
                                TransitionReason::StartFailed(msg),
                            )
                            .await
                        {
                            tracing::error!("[engine] transition start-failed: {e}");
                        }
                        engine.forget_session(&id);
                    }
                }
                // finally: remove from starting, re-pump
                {
                    let mut state = engine.0.state.lock();
                    state.starting.remove(&id);
                }
                let closed = engine.0.state.lock().closed;
                if !closed {
                    engine.pump();
                }
            });
        }
    }

    /// Build SpawnOptions for a session.
    pub(crate) fn spawn_opts(&self, s: &Session, item: &QueueItem) -> SpawnOptions {
        // Resolve cwd. Claude CLI's `--resume` is cwd-scoped: it computes the project
        // slug from `pwd()` (a path like `/home/.../agentic-worktrees/<id>` becomes
        // `...-<id>` under ~/.claude/projects/), so `--resume <sessionId>` only finds
        // the original transcript jsonl when cwd matches what was used at session-create
        // time.
        //
        // For RESUME turns (claudeSessionId is set on the session): use the worktree
        // ROOT — that's where every original session's transcript lives, because the
        // engine originally spawned from worktree root. For FIRST turns of new sessions,
        // keep the `<worktree>/<repo>` cwd so tooling (Bash/Edit/etc.) finds repo files
        // at the right relative paths; the new transcript will be written under
        // `<worktree>-<repo>/` which is consistent for future resume turns of THIS session.
        //
        // Why we don't use `<worktree>/<repo>` for resume too: any session whose
        // transcript was written before `spawn_opts` started appending `<repo>` to cwd
        // (every session currently in production as of this change) lives under
        // `<worktree>/`, NOT `<worktree>-<repo>/`. Switching to the repo-subdir cwd for
        // resume would push CLI's slug-search into `<worktree>-<repo>/` and miss the
        // existing transcript. See outbox/REAL-ROOT-CAUSE.md for the reproduction.
        let has_resume_target = !s.claude_session_id.as_deref().unwrap_or("").is_empty();
        let cwd = if has_resume_target {
            // Resume turn: spawn from worktree root so the CLI's slug matches where the
            // transcript was originally written. Tooling inside the turn will resolve
            // paths relative to the repo (the assistant knows the layout); this only
            // affects how `pwd()` is computed at spawn time.
            s.worktree_path
                .clone()
                .unwrap_or_else(|| self.0.cfg.worktrees_root.to_string_lossy().into_owned())
        } else if s.repos.len() == 1 {
            // Single-repo first turn: cwd is the repo's worktree subdirectory so
            // tooling sees files at their natural relative paths.
            if let Some(ref wt) = s.worktree_path {
                format!("{}/{}", wt, s.repos[0])
            } else {
                self.0.cfg.worktrees_root.to_string_lossy().into_owned()
            }
        } else {
            // Multi-repo or no-repo first turn: cwd is the session dir.
            s.worktree_path
                .clone()
                .unwrap_or_else(|| self.0.cfg.worktrees_root.to_string_lossy().into_owned())
        };

        // All agentic sessions share the real ~/.claude config dir so they read/write the SAME OAuth
        // credential file: one shared, self-refreshing token — never a per-session copy that could
        // rotation-invalidate the other sessions or the user's own login. Per-session isolation of
        // transcripts/workflows is preserved by claude's own cwd-derived slug under projects/.
        let claude_config_dir = Some(self.0.cfg.claude_config_base.to_string_lossy().into_owned());

        // Structured spawn trace — key=value fields journald can index. Pairs with the runner's
        // `evt=bridge_spawning` and the bridge's `[sdk-bridge] boot` line to form a self-
        // contained timeline of what the engine decided, what the runner handed to node, and
        // what the bridge did with it. Goal: any "Claude Code process exited with code 1"
        // failure must be diagnosable from journald alone with a single
        //   journalctl --since "10 min ago" session_id=<id>
        // query that surfaces cwd + resume id + claude_session_id + bin in one slice.
        tracing::info!(
            evt = "spawn_opts_resolved",
            session_id = %s.id,
            repos = ?s.repos,
            worktree_path = ?s.worktree_path,
            claude_session_id = ?s.claude_session_id,
            resume_session_id = ?item.resume_session_id,
            cwd = %cwd,
            log_path = %self.0.store.log_path(&s.id).display(),
        );

        // Resume sanity check: if the session has a stored claudeSessionId but its
        // backing transcript jsonl is gone (a common case after `git worktree prune`,
        // a manual `rm ~/.claude/projects/.../<id>.jsonl`, or a CLI version bump that
        // re-keys the projects tree), Claude CLI's `--resume <id>` exits with code 1
        // and stderr `No conversation found with session ID: <id>` — which surfaces
        // to the engine as `Claude Code process exited with code 1` and to Android
        // as a red "Claude error" banner. Detect this BEFORE spawning by checking
        // whether the jsonl exists at the expected cwd-derived path, and silently
        // drop the resume id when it doesn't (fresh start, with the same cwd
        // alignment as the spawn_opts cwd branch above).
        let resume_session_id = if let Some(ref csid) = s.claude_session_id {
            if !csid.is_empty() {
                let cwd_slug = cwd.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
                let transcript_path = self
                    .0
                    .cfg
                    .claude_config_base
                    .join("projects")
                    .join(&cwd_slug)
                    .join(format!("{csid}.jsonl"));
                let exists = transcript_path.is_file();
                tracing::info!(
                    evt = "resume_sanity_check",
                    session_id = %s.id,
                    claude_session_id = %csid,
                    transcript_path = %transcript_path.display(),
                    exists = exists,
                    decision = if exists { "pass_through" } else { "drop_resume_session_id_fresh_start" },
                );
                if exists {
                    // `--resume` makes the CLI send `previous_message_id` from the transcript, which the
                    // API requires to be a real server id (`msg_...`). Some SDK-written transcripts hold
                    // only synthetic ids (no `msg_`), so resume 400s and can NEVER succeed — and the
                    // engine cannot synthesize server ids (trimming the tail does not help; verified).
                    // Drop `--resume` and run the turn FRESH in that case: prior chat context is not
                    // carried into the model, but the worktree/files are untouched and the session works
                    // again. Transcripts WITH server ids resume normally.
                    if crate::engine::resume_gate::transcript_is_resumable(&transcript_path) {
                        item.resume_session_id.clone()
                    } else {
                        tracing::info!(
                            evt = "resume_gate",
                            session_id = %s.id,
                            transcript_path = %transcript_path.display(),
                            decision = "no_server_msg_ids_fresh_start",
                        );
                        None
                    }
                } else {
                    None
                }
            } else {
                item.resume_session_id.clone()
            }
        } else {
            item.resume_session_id.clone()
        };

        SpawnOptions {
            cwd,
            prompt: item.prompt.clone(),
            env: item.env.clone(),
            resume_session_id,
            claude_config_dir,
            // model: per-turn wins; otherwise fall back to session; empty-string → None
            model: item
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| s.model.clone()),
            // effort: same rule as model
            effort: item
                .effort
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| s.effort.clone()),
            // permission_mode: per-turn wins; otherwise fall back to session; empty-string → None
            permission_mode: item
                .permission_mode
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| s.permission_mode.clone()),
            mode: s.mode.clone(),
            // Reseed from global: session inherits globally-off skills + applies its own hides,
            // minus any skills the session forces on (forced-on wins over global-off).
            hidden_skills: crate::engine::global_settings::resolve_session_hidden_skills(
                &self.0.cfg.claude_config_base,
                &s.hidden_skills,
                &s.forced_on_skills,
            ),
            // Globally-off skills the session forces on → bridge needs explicit "on" overrides.
            forced_on_skills: crate::engine::global_settings::resolve_session_forced_on_skills(
                &self.0.cfg.claude_config_base,
                &s.forced_on_skills,
            ),
            // Resolve the session's hiddenPlugins blacklist × forced-on × the installed-plugin
            // registry into an EXPLICIT enable map at spawn time (re-read each turn, so
            // mid-session installs are picked up). Forced-on plugins emit `true` even when
            // globally disabled; hidden plugins emit `false`.
            enabled_plugins: crate::engine::plugins::resolve_enabled_plugins(
                &self.0.cfg.claude_config_base,
                &s.hidden_plugins,
                &s.forced_on_plugins,
            ),
            forced_on_plugins: s.forced_on_plugins.clone(),
            // Forced-on wins over hidden for MCP too (same precedence as skills/plugins) —
            // sdk_runner drops hidden names from the extra-defs injection, so a name in both
            // lists must leave the hidden set or the forced-on injection would be filtered out.
            hidden_mcp_servers: s
                .hidden_mcp_servers
                .iter()
                .filter(|h| !s.forced_on_mcp_servers.contains(h))
                .cloned()
                .collect(),
            forced_on_mcp_servers: s.forced_on_mcp_servers.clone(),
            // Forced-on MCP servers that are globally DISABLED (parked in .claude.json under
            // mcpServersDisabled) are invisible to the session's normal config loading, so they
            // are injected back through the extra-defs channel (SDK_BRIDGE_EXTRA_MCP → the SDK
            // mcpServers option). Session-defined extras keep priority on a name clash.
            extra_mcp_servers: {
                let mut extras = s.extra_mcp_servers.clone();
                for def in crate::engine::user_config::parked_mcp_defs(
                    &self.0.cfg.claude_config_base,
                    &s.forced_on_mcp_servers,
                ) {
                    if !extras.iter().any(|e| e.name == def.name) {
                        extras.push(def);
                    }
                }
                extras
            },
            log_path: self.0.store.log_path(&s.id),
            unit: format!("agentic-{}", s.id),
            memory_max: self.0.cfg.memory_max.clone(),
            memory_high: self.0.cfg.memory_high.clone(),
            cpu_quota: self.0.cfg.cpu_quota.clone(),
            tasks_max: self.0.cfg.tasks_max.clone(),
            // Tier-1 harness rules (routing + fan-out discipline) appended to the system prompt on
            // EVERY main turn. NOT in the session CLAUDE.md — so delegate workers / the router (which
            // build their own SpawnOptions with `..Default::default()` → None) never carry them.
            append_system_prompt: Some(crate::engine::session_guide::harness_rules()),
        }
    }

    /// Start a queued item: sync worktree, build config dir, spawn claude, attach.
    async fn start(&self, item: QueueItem) -> Result<(), String> {
        let id = &item.id;

        // Get the session.
        let s = match self.0.store.get(id).await.map_err(|e| e.to_string())? {
            Some(s) => s,
            None => return Ok(()), // deleted before start
        };

        // Per plan: a session with no worktree_path is not startable.
        if s.worktree_path.is_none() {
            return Err("not startable: session has no worktree path".into());
        }

        // Sync worktrees (if session has repos).
        for repo in &s.repos {
            if let Some(ref wt_path) = s.worktree_path {
                let repo_wt = PathBuf::from(wt_path).join(repo);
                if let Some(ref sync_fn) = self.0.cfg.sync_fn {
                    (sync_fn)(repo_wt).await;
                } else {
                    sync_worktree(&repo_wt).await;
                }
            }
        }

        // Check if closed or killed during sync (honor kill-during-sync).
        {
            let closed = self.0.state.lock().closed;
            if closed {
                return Ok(());
            }
        }

        // Re-read session after sync to check for kill.
        let cur = match self.0.store.get(id).await.map_err(|e| e.to_string())? {
            Some(c) => c,
            None => return Ok(()), // deleted during sync
        };
        if cur.status == "killed" {
            return Ok(()); // honor kill issued during sync window — no spawn
        }

        // No per-session config dir: sessions use the shared ~/.claude directly (set as
        // CLAUDE_CONFIG_DIR in spawn_opts), so all sessions share one self-refreshing credential.
        // Skills/plugins/settings resolve from ~/.claude; this session's transcript and workflow
        // data are namespaced by claude under projects/<cwd-slug>/.

        let now = self.now();

        // Log turn_start lifecycle event.
        let active = Engine::active_count(&self.0.state.lock());
        let max_concurrent = self.0.cfg.max_concurrent;
        let queue_wait_ms = item.enqueued_at.map(|e| std::cmp::max(0, now - e));
        self.log(serde_json::json!({
            "evt": "turn_start",
            "sessionId": id,
            "queueWaitMs": queue_wait_ms,
            "active": active,
            "max": max_concurrent,
        }));

        // Set runtime state.
        {
            let mut state = self.0.state.lock();
            state.last_event_at.insert(id.to_string(), now);
            state.turn_started_at.insert(id.to_string(), now);
            let act = state.activity.entry(id.to_string()).or_default();
            act.turns += 1;
        }

        // Append agentic_prompt marker to log BEFORE spawning.
        self.0
            .store
            .append_log(id, &prompt_event_json(&item.prompt, now).to_string())
            .await
            .map_err(|e| e.to_string())?;

        // Periodic retitle for queued/resumed turns — the prompt marker was
        // appended just above, so the persisted agentic_prompt count includes
        // this turn. The initial submit is turn 1, so it never triggers.
        self.maybe_spawn_retitle(id);

        // Resolve `@session:<id>` mentions BEFORE spawning: expansion awaits Store::list(), and
        // any await between spawn_claude and attach() widens the spawn→attach race window (a kill
        // landing mid-await finds nothing in state.running and no-ops, leaving the just-spawned
        // handle running). Mentions are expanded on the user's prompt only (never on the fork
        // seed context — a transcript may quote mention tokens from earlier turns) and only on
        // the delivered text — the log marker appended above stays `item.prompt`.
        let delivered = self.expand_session_mentions(&item.prompt).await;

        // Spawn claude.
        let opts = self.spawn_opts(&s, &item);
        tracing::info!(
            evt = "turn_spawning",
            session_id = %id,
            cwd = %opts.cwd,
            resume_session_id = ?opts.resume_session_id,
            claude_config_dir = ?opts.claude_config_dir,
            log_path = %opts.log_path.display(),
            unit = %opts.unit,
        );
        let handle = spawn_claude(opts, self.0.runner.as_ref());

        // Set awaiting=false.
        {
            let mut state = self.0.state.lock();
            state.awaiting.insert(id.to_string(), false);
        }

        // Write the first user message. For a fork's first turn this prepends the seed context
        // (the source transcript) ahead of the user's message; for every normal turn it is just
        // the (mention-expanded) prompt. The displayed user bubble stays the user's text, not the
        // prepended transcript.
        handle.write(&encode_user_message(&compose_user_text(
            &compose_turn_text_with(&item, &delivered),
        )));

        // Attach: spawn the pump task AND register the RunningTurn in state.running. This MUST
        // precede publishing status="running": kill()/watchdog-reap/discard/delete all look the
        // session up in state.running. If status="running" were published first, a concurrent caller
        // (or a test's wait_status("running")) could observe "running" during the window before
        // attach() registers the turn, find nothing in state.running, and silently no-op — losing a
        // kill or skipping a reap. Registering first closes that race.
        self.attach(id.to_string(), handle);

        // Now publish status="running" — the turn is registered and killable. Guard against a turn
        // that already finalized in the attach→publish window (a fast on_exit on an unspawnable
        // binary, or a kill that just landed): never resurrect a terminal status back to "running".
        let already_terminal = matches!(
            self.0
                .store
                .get(id)
                .await
                .ok()
                .flatten()
                .map(|c| c.status)
                .as_deref(),
            Some("killed") | Some("done") | Some("failed")
        );
        if !already_terminal {
            // PR6: route through transition() with Start reason.
            // transition() owns status=running, started_at=now, and
            // the clear_error / clear_error_kind / clear_exit_code
            // side-effect patch — so the start-time guarantees are
            // in one place. Idempotent self-transition (Running →
            // Running) is a no-op.
            self.transition(id, SessionStatus::Running, TransitionReason::Start)
                .await
                .map_err(|e| e.to_string())?;

            // PR4 (finally wired): record the turn-start wall-clock in the
            // lifecycle sidecar. recover()'s anti-stale-429 scan uses this as
            // its anchor, and it is the coarse end-time fallback when a turn
            // never records a TurnEnded (crash mid-turn). Best-effort: a failed
            // append must not abort the turn that is already running.
            let _ = self
                .0
                .store
                .append_lifecycle(
                    id,
                    &crate::engine::lifecycle::LifecycleEvent::TurnStarted {
                        at: now,
                        prompt_len: item.prompt.chars().count(),
                    },
                )
                .await
                .map_err(|e| tracing::warn!("[engine] TurnStarted append failed for {id}: {e}"));
        }

        Ok(())
    }

    /// Wire up the event/exit pump task for a started claude process.
    fn attach(&self, id: String, mut handle: crate::engine::spawner::SpawnHandle) {
        // Extract the kill/interrupt surface and saw_result BEFORE moving handle into the pump task.
        // saw_result is the SAME Arc the SpawnHandle's polling loop uses, so resetting it on
        // follow_up live-inject (via RunningTurn.saw_result) actually clears the flag the poller
        // reads — ensuring a crash on turn 2 yields exit code 1 (not 0 from turn 1's success).
        let run = handle.run.clone();
        let saw_result = handle.saw_result.clone();

        let engine = self.clone();
        let id_for_task = id.clone();
        let pump_task = tokio::spawn(async move {
            let id = id_for_task;
            // Drive the select loop: events and exit.
            loop {
                tokio::select! {
                    Some(ev) = handle.events.recv() => {
                        engine.on_event(&id, ev).await;
                    }
                    code = &mut handle.exit => {
                        let code = code.unwrap_or(1);
                        // Drain any remaining events before calling on_exit.
                        while let Ok(ev) = handle.events.try_recv() {
                            engine.on_event(&id, ev).await;
                        }
                        engine.on_exit(&id, code).await;
                        break;
                    }
                }
            }
        });

        // Register the running turn.
        {
            let mut state = self.0.state.lock();
            state.running.insert(
                id,
                RunningTurn {
                    run,
                    pump: pump_task,
                    saw_result,
                },
            );
        }
    }

    /// Handle one ClaudeEvent from the pump task.
    pub(crate) async fn on_event(&self, id: &str, ev: ClaudeEvent) {
        // Check closed.
        if self.0.state.lock().closed {
            return;
        }

        // Update last_event_at.
        let now = self.now();
        {
            let mut state = self.0.state.lock();
            state.last_event_at.insert(id.to_string(), now);
        }

        match &ev {
            ClaudeEvent::Init { session_id, .. } => {
                if let Err(e) = self
                    .0
                    .store
                    .update(
                        id,
                        SessionPatch {
                            claude_session_id: Some(Some(session_id.clone())),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    tracing::error!("[engine] store.update Init session_id failed: {e}");
                }
                let mut state = self.0.state.lock();
                state.awaiting.insert(id.to_string(), false);
            }

            ClaudeEvent::Skill { names, .. } if !names.is_empty() => {
                let mut state = self.0.state.lock();
                let act = state.activity.entry(id.to_string()).or_default();
                act.last_skill = names.last().cloned();
            }

            ClaudeEvent::Ask { .. } => {
                let mut state = self.0.state.lock();
                state.pending_ask.insert(id.to_string());
                state.parked.insert(id.to_string(), ev.to_wire());
            }

            ClaudeEvent::Perm { .. } | ClaudeEvent::Plan { .. } => {
                let mut state = self.0.state.lock();
                state.pending_perm.insert(id.to_string());
                state.parked.insert(id.to_string(), ev.to_wire());
            }

            ClaudeEvent::PermResolved { .. } => {
                let mut state = self.0.state.lock();
                state.pending_perm.remove(id);
                state.parked.remove(id);
            }

            ClaudeEvent::DelegateRequest {
                id: req_id,
                run_id,
                tasks,
                title,
                ..
            } => {
                // The main session's `delegate` tool is parked waiting for cheap workers. Run the
                // fan-out OFF the event loop (it can take minutes — run_delegate also marks the
                // watchdog exemption), then reply via the bridge's stdin control channel so the tool
                // resolves and the turn continues.
                // Link this run to the exact delegate card that started it: cards and requests are 1:1
                // and in order within a turn, so the front of the FIFO is this request's card. Lets the
                // client open the right run on click instead of guessing by (often-duplicated) name.
                let card_tool_use = {
                    let mut state = self.0.state.lock();
                    state
                        .workflow_delegate_pending
                        .get_mut(id)
                        .and_then(|q| q.pop_front())
                };
                if let (Some(card), false) = (card_tool_use, run_id.is_empty()) {
                    self.log_workflow_run(id, &card, run_id).await;
                }

                let engine = Engine(self.0.clone());
                let caller = id.to_string();
                let req_id = req_id.clone();
                let run_id = run_id.clone();
                let title = title.clone();
                let dtasks: Vec<crate::engine::delegate::DelegateTask> = tasks
                    .iter()
                    .map(|t| crate::engine::delegate::DelegateTask {
                        prompt: t
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        role: t
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("explorer")
                            .to_string(),
                        model: t.get("model").and_then(|v| v.as_str()).map(String::from),
                        phase: t.get("phase").and_then(|v| v.as_str()).map(String::from),
                        write: t.get("write").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                    .collect();
                tokio::spawn(async move {
                    let summaries = match engine.run_delegate(&caller, &run_id, dtasks, title).await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("[engine] delegate run failed for {caller}: {e}");
                            // Surface the failure to the main session (e.g. write-mode worktree setup
                            // failed) instead of returning an empty result it can't interpret.
                            vec![crate::engine::delegate::WorkerSummary {
                                agent_id: "w1".to_string(),
                                summary: format!("delegate run failed: {e}"),
                                failed: true,
                            }]
                        }
                    };
                    let wire: Vec<serde_json::Value> = summaries
                        .iter()
                        .map(|s| serde_json::json!({ "agentId": s.agent_id, "summary": s.summary, "failed": s.failed }))
                        .collect();
                    let line = serde_json::json!({ "__bridge": "delegate", "id": req_id, "summaries": wire }).to_string();
                    let run = {
                        engine
                            .0
                            .state
                            .lock()
                            .running
                            .get(&caller)
                            .map(|r| r.run.clone())
                    };
                    if let Some(run) = run {
                        run.write(&line);
                    } else {
                        tracing::warn!(
                            "[engine] delegate reply: caller {caller} no longer running"
                        );
                    }
                });
            }

            ClaudeEvent::Result {
                is_error,
                cost_usd,
                text,
                raw,
            } => {
                // Log turn_result lifecycle event.
                let ttft_ms = raw.get("ttft_ms").cloned();
                let duration_ms = raw.get("duration_ms").cloned();
                self.log(serde_json::json!({
                    "evt": "turn_result",
                    "sessionId": id,
                    "ttftMs": ttft_ms,
                    "durationMs": duration_ms,
                    "isError": is_error,
                    "costUsd": cost_usd,
                }));

                // Remove pending_ask and pending_perm.
                {
                    let mut state = self.0.state.lock();
                    state.pending_ask.remove(id);
                    state.pending_perm.remove(id);
                    state.parked.remove(id);
                }

                // Read the session once: accumulate cost, and learn whether the user already stopped
                // this turn. kill() flips status to "killed" BEFORE it aborts the run, so the abort's
                // synthetic is_error result line — or the live build's "[ede_diagnostic] …" line —
                // reaches here AFTER the kill. A deliberate Stop is not an error, so never classify or
                // record one for an already-killed session; otherwise the Android client paints a red
                // dot + "⚠ Claude error" banner on a turn the user chose to stop.
                let cur = self.0.store.get(id).await.ok().flatten();
                let cur_cost = cur.as_ref().and_then(|s| s.cost_usd).unwrap_or(0.0);
                let killed = cur.as_ref().map(|s| s.status.as_str()) == Some("killed");
                let new_cost = cur_cost + cost_usd.unwrap_or(0.0);

                let mut patch = SessionPatch {
                    cost_usd: Some(Some(new_cost)),
                    ..Default::default()
                };

                if *is_error && !killed {
                    if let Some(ref t) = text {
                        let truncated = truncate_chars(t, 500);
                        patch.error = Some(Some(truncated.to_string()));
                        patch.error_kind = Some(Some(classify_claude_error(t).to_string()));
                    }
                }

                if let Err(e) = self.0.store.update(id, patch).await {
                    tracing::error!("[engine] store.update Result cost/error patch failed: {e}");
                }

                // Set awaiting=true (session is idle, waiting for next input or done).
                {
                    let mut state = self.0.state.lock();
                    state.awaiting.insert(id.to_string(), true);
                }
                // Discord-style unread tracking: increment the monotonic counter so the client's
                // comparison `unreadEventId > lastAckedEventId` detects this as a new "your turn" point.
                let _ = self.0.store.incr_unread_event_id(id).await.map_err(|e| {
                    tracing::warn!("[engine] unreadEventId incr failed for {id}: {e}")
                });

                // PR4 (finally wired): record the turn-end wall-clock in the
                // lifecycle sidecar. This is the AUTHORITATIVE end time recover()
                // restores into `endedAt` after a restart — a turn that finished
                // here (process stays alive, session parks as awaiting) leaves no
                // other durable end timestamp, so without this a restart would
                // re-stamp endedAt=now and resurrect the client's unread dot.
                let outcome = if killed {
                    crate::engine::lifecycle::TurnOutcome::Killed
                } else if *is_error {
                    crate::engine::lifecycle::TurnOutcome::Error
                } else {
                    crate::engine::lifecycle::TurnOutcome::Success
                };
                let _ = self
                    .0
                    .store
                    .append_lifecycle(
                        id,
                        &crate::engine::lifecycle::LifecycleEvent::TurnEnded {
                            at: now,
                            outcome,
                            cost_usd: *cost_usd,
                            duration_ms: raw.get("duration_ms").and_then(|v| v.as_i64()),
                        },
                    )
                    .await
                    .map_err(|e| tracing::warn!("[engine] TurnEnded append failed for {id}: {e}"));

                // Finish-line push at TURN END. In the streaming architecture this is the NORMAL
                // completion: the persistent process stays alive and the session parks as awaiting,
                // so on_exit (the only place the push used to fire) never runs — a finished turn
                // never notified the user. Fire here instead; on_exit skips its push entirely for
                // sessions that were already parked (was_parked) so the two hooks never double-
                // notify. A deliberate Stop is excluded — the user ended the turn themselves.
                if !killed {
                    if let Some(ref push_fn) = self.0.cfg.push_fn {
                        // Built from values already in memory (`cur` + the patch inputs above) —
                        // no second store.get: the re-read only added SQLite lock contention
                        // (review feedback on #58).
                        if let Some(ref s) = cur {
                            let push_status = if *is_error { "failed" } else { "done" };
                            // Mirrors the error patch above: the truncated result text for an
                            // error turn; a success push carries no error.
                            let error_text = if *is_error {
                                text.as_ref()
                                    .map(|t| truncate_chars(t, 500).to_string())
                                    .or_else(|| s.error.clone())
                            } else {
                                None
                            };
                            let payload = serde_json::json!({
                                "sessionId": id,
                                "status": push_status,
                                "isError": *is_error,
                                "errorText": error_text,
                                "costUsd": new_cost,
                                "title": s.prompt,
                            });
                            push_fn(payload);
                        }
                    }
                }

                // Re-pump (a slot freed because this session is now parked/awaiting).
                self.pump();
            }

            ClaudeEvent::Agent { agents, .. } => {
                // Remember the spawned subagents' tool_use ids so we can tell their results apart from
                // ordinary tool results when they come back as `tool_result`s.
                let mut state = self.0.state.lock();
                let set = state.spawn_ids.entry(id.to_string()).or_default();
                for a in agents {
                    if !a.id.is_empty() {
                        set.insert(a.id.clone());
                    }
                }
            }

            ClaudeEvent::AgentResult {
                tool_use_id,
                text,
                raw,
            } => {
                // agent ≠ tool: a `tool_result` is a genuine subagent result ONLY if its tool_use_id is
                // one we recorded as a spawn. Anything else is a plain tool's output.
                let is_agent = {
                    let state = self.0.state.lock();
                    state
                        .spawn_ids
                        .get(id)
                        .is_some_and(|s| s.contains(tool_use_id.as_str()))
                };
                if !is_agent {
                    // A native `Workflow` tool's result carries its run id (`wf_…`). Link the card to
                    // that run so a click opens the exact run, then stop — its tool chip already
                    // represents the call (don't also treat it as a PR or an agent result).
                    let is_native_workflow = {
                        let mut state = self.0.state.lock();
                        state
                            .workflow_native_ids
                            .get_mut(id)
                            .is_some_and(|s| s.remove(tool_use_id.as_str()))
                    };
                    if is_native_workflow {
                        if let Some(run_id) = crate::engine::stream::parse_workflow_run_id(text) {
                            self.log_workflow_run(id, tool_use_id, &run_id).await;
                        }
                        return;
                    }
                    // Plain tool result (Bash/Read/…): the tool chip already represents the call. Don't
                    // surface it as an agent (no orphan agent card) and don't persist it.
                    //
                    // BUT — a `gh pr create` prints the new PR's URL alone on a line of its output. Turn
                    // each freshly-seen one into a PR card: fetch its title/description via `gh pr view`
                    // OFF the hot path (a fire-and-forget task) and append a rendered `pr` marker, which
                    // re-tails into a `kind:pr` frame (live + on reconnect). pr_seen dedups per session.
                    for url in crate::engine::stream::detect_created_pr_urls(text) {
                        let fresh = {
                            let mut state = self.0.state.lock();
                            state
                                .pr_seen
                                .entry(id.to_string())
                                .or_default()
                                .insert(url.clone())
                        };
                        if fresh {
                            let engine = self.clone();
                            let id = id.to_string();
                            tokio::spawn(async move { engine.fetch_pr_and_log(&id, &url).await });
                        }
                    }
                    return;
                }
                // Genuine subagent result (a `user` tool_result): persist a compact, rendered marker so
                // the agent card's body survives a reopen/reconnect (the raw `user` tool_result line is
                // filtered out of the rendered log). Fall through to emit so the LIVE view still attaches
                // it to the card.
                //
                // CRITICAL — break the feedback loop: that marker is written to the SAME log the
                // EventTailer reads, and `parse_line` decodes an `{"type":"agent_result"}` line straight
                // back into THIS event (stream.rs). So our own marker re-enters here. Re-persisting it
                // would append another marker, which is re-tailed, … an unbounded loop (logs grew to
                // hundreds of MB — a single result echoed tens of thousands of times — even with no
                // claude process running). Persist ONLY when the source is a genuine tool_result, never
                // when this event was decoded from an already-persisted marker. The re-tailed marker
                // still falls through to emit below, which is the single live-delivery channel.
                let from_marker = raw.get("type").and_then(|v| v.as_str()) == Some("agent_result");
                if !from_marker {
                    let marker = serde_json::json!({
                        "type": "agent_result",
                        "toolUseId": tool_use_id,
                        "text": text,
                    })
                    .to_string();
                    if let Err(e) = self.0.store.append_log(id, &marker).await {
                        tracing::error!(
                            "[engine] store.append_log agent_result marker failed: {e}"
                        );
                    }
                }
            }

            ClaudeEvent::Workflow {
                id: card_id,
                delegate,
                ..
            } => {
                // Record each workflow card's tool_use id so we can link it to its run id once known
                // (delegate: popped on its DelegateRequest; native Workflow: read from its tool result).
                // Workflow events come only from assistant tool_use blocks (never a re-tailed marker),
                // so no loop guard is needed. Falls through to emit below.
                if !card_id.is_empty() {
                    let mut state = self.0.state.lock();
                    if *delegate {
                        state
                            .workflow_delegate_pending
                            .entry(id.to_string())
                            .or_default()
                            .push_back(card_id.clone());
                    } else {
                        state
                            .workflow_native_ids
                            .entry(id.to_string())
                            .or_default()
                            .insert(card_id.clone());
                    }
                }
            }

            _ => {} // Other events: just emit below
        }

        // Always emit to subscribers.
        self.emit(id, &ev);
    }

    /// Persist + announce the link from a workflow card's tool_use id to its run id. Appends a rendered
    /// `{"type":"workflowRun",…}` marker (so it replays on reconnect via the cursor) and emits the event
    /// to poke any live WS loop to re-read the cursor. The cursor is the single delivery channel, so the
    /// frame reaches the client exactly once — live and on reopen (mirrors the `pr` marker path). The
    /// re-tailed marker decodes back to a `WorkflowRun` event that only emits (never re-appends), so
    /// there is no feedback loop.
    async fn log_workflow_run(&self, id: &str, tool_use_id: &str, run_id: &str) {
        let raw = serde_json::json!({ "type": "workflowRun", "id": tool_use_id, "runId": run_id });
        if let Err(e) = self.0.store.append_log(id, &raw.to_string()).await {
            tracing::error!("[engine] store.append_log workflowRun marker failed for {id}: {e}");
            return;
        }
        self.emit(
            id,
            &crate::engine::stream::ClaudeEvent::WorkflowRun {
                id: tool_use_id.to_string(),
                run_id: run_id.to_string(),
                raw,
            },
        );
    }

    /// Fetch a created PR's metadata via `gh pr view` and append a rendered `{"type":"pr",…}` marker to
    /// the session log. The tailer re-reads that marker, decodes it to a `ClaudeEvent::Pr`, and the WS
    /// cursor delivers a `kind:pr` frame — live and on reconnect. Best-effort: any failure (gh missing,
    /// network, timeout, non-zero, unparseable) is logged and dropped, never blocking or failing a turn.
    /// Runs in its own task (see the call site), so the `gh` subprocess never sits on the event loop.
    async fn fetch_pr_and_log(&self, id: &str, url: &str) {
        use tokio::process::Command;
        // kill_on_drop: if the 10 s timeout fires and this future is dropped, tokio SIGKILLs the child
        // so a network-stalled `gh` can't leak as an orphan.
        let fetched = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Command::new("gh")
                .args(["pr", "view", url, "--json", "number,title,body,state"])
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let out = match fetched {
            Ok(Ok(o)) if o.status.success() => o,
            Ok(Ok(o)) => {
                tracing::warn!(
                    "[engine] gh pr view {url} failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!("[engine] gh pr view {url} spawn error: {e}");
                return;
            }
            Err(_) => {
                tracing::warn!("[engine] gh pr view {url} timed out");
                return;
            }
        };
        let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            tracing::warn!("[engine] gh pr view {url}: unparseable JSON");
            return;
        };
        let number = meta.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
        let repo = crate::engine::stream::pr_repo_from_url(url).unwrap_or_default();
        let title = meta
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = meta
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state = meta
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("OPEN")
            .to_string();
        let raw = serde_json::json!({
            "type": "pr", "url": url, "number": number, "repo": repo,
            "title": title, "body": body, "state": state,
        });
        if let Err(e) = self.0.store.append_log(id, &raw.to_string()).await {
            tracing::error!("[engine] store.append_log pr marker failed for {id}: {e}");
            return;
        }
        // Also emit directly: a `gh pr create` is often the LAST thing a turn does, so by the time this
        // fetch finishes the pump task that re-tails the appended marker may have stopped (turn ended) —
        // and only that re-tail would otherwise poke the WS loop. Emitting here pokes a still-connected
        // client to re-read the rendered cursor. Harmless if the pump is alive too: both paths only poke,
        // and the cursor delivers the one persisted `pr` line exactly once.
        self.emit(
            id,
            &crate::engine::stream::ClaudeEvent::Pr {
                url: url.to_string(),
                number,
                repo,
                title,
                body,
                state,
                raw,
            },
        );
    }

    /// Handle process exit.
    async fn on_exit(&self, id: &str, code: i32) {
        // Check closed.
        if self.0.state.lock().closed {
            return;
        }

        let now = self.now();

        // Remove from running and clear per-session runtime state (except activity — kept for withActivity).
        // was_parked (captured before the wipe): the session had already finished a turn and parked as
        // awaiting — its completion was announced by the turn-end push in the Result branch. The push
        // block below uses it to skip a duplicate/late "done" notification (e.g. a benign idle/wall cap
        // reaping a long-parked session hours after the user read the result).
        let was_parked = {
            let mut state = self.0.state.lock();
            let was_parked = state.awaiting.get(id) == Some(&true);
            state.running.remove(id);
            state.last_event_at.remove(id);
            state.turn_started_at.remove(id);
            state.awaiting.remove(id);
            state.pending_ask.remove(id);
            state.pending_perm.remove(id);
            state.pending_delegate.remove(id);
            state.parked.remove(id);
            was_parked
        };

        // Read current session state.
        let cur = match self.0.store.get(id).await.ok().flatten() {
            Some(s) => s,
            None => {
                // Session was deleted; just re-pump.
                self.pump();
                return;
            }
        };

        // Determine final status based on exit code and error state.
        let status = match cur.status.as_str() {
            "killed" => "killed",
            "failed" => "failed",
            "done" => "done",
            _ => {
                if code == 0 && cur.error.is_none() {
                    "done"
                } else {
                    "failed"
                }
            }
        };

        let started_at = cur.started_at;
        let ended_at = now;

        // PR6: use the SessionUpdate builder (clear_error / clear_error_kind
        // instead of the Some(None) sentinel). The on_exit decision
        // table is preserved verbatim — PR6 is a mechanical
        // readability improvement, not a behavior change.
        let mut u = SessionUpdate::new()
            .status_str(status)
            .exit_code(code as i64)
            .ended_at(ended_at);

        // Generic crash fallback: failed with no error → set neutral crash message.
        if status == "failed" && cur.error.is_none() {
            u = u
                .error("turn ended without completing — interrupted, crashed, or killed (resume to retry)")
                .error_kind("crashed");
        }

        // Backstop: a deliberate Stop (status "killed") is never an error.
        // Clear any error / errorKind a racing abort result line set
        // before the kill landed. Pairs with the already-killed guard
        // in on_event's Result branch.
        if status == "killed" {
            u = u.clear_error().clear_error_kind();
        }

        // error_kind_for_log is whatever we just decided (Set or None);
        // fall back to cur.error_kind only if we didn't touch it.
        let error_kind_for_log = match &u.error_kind {
            Field::Set(v) => Some(v.clone()),
            Field::Clear => None,
            Field::Unset => cur.error_kind.clone(),
        };

        if let Err(e) = self.0.store.apply_update(id, u).await {
            tracing::error!("[engine] store.update on_exit final patch failed: {e}");
        }

        // Discord-style unread tracking: increment the counter for DONE sessions
        // (reaching terminal state is a "your turn" point).
        if status == "done" {
            let _ = self.0.store.incr_unread_event_id(id).await.map_err(|e| {
                tracing::warn!("[engine] unreadEventId incr on_exit failed for {id}: {e}")
            });
        }

        // Emit engineExit event.
        self.emit(
            id,
            &ClaudeEvent::Other {
                raw: serde_json::json!({
                    "engineExit": {
                        "code": code,
                        "status": status,
                        "errorKind": error_kind_for_log,
                    }
                }),
            },
        );

        // Log turn_end lifecycle event.
        let duration_ms = started_at.map(|st| ended_at - st);
        self.log(serde_json::json!({
            "evt": "turn_end",
            "sessionId": id,
            "status": status,
            "errorKind": error_kind_for_log,
            "exitCode": code,
            "durationMs": duration_ms,
        }));

        // Phase 6: fire the finish-line push hook. The closure (installed in main.rs) loads the
        // device token + creds and sends FCM; tests inject a recorder. Fire-and-forget — must not
        // block re-pump. Re-fetch the session AFTER store.update so that crash-fallback error
        // messages written by the patch above are included in errorText.
        if let Some(ref push_fn) = self.0.cfg.push_fn {
            let final_session = self.0.store.get(id).await.ok().flatten();
            // Only fire the finish-line push on a genuine terminal state with a still-present
            // session row (guards a delete/race between the update and this read). An exit of an
            // already-parked session is skipped ENTIRELY: the last turn's outcome (done OR failed)
            // was already pushed at turn end (Result branch), and the exit of a parked process is
            // housekeeping — a benign idle/wall cap reap, a platform restart, or the user stopping
            // an idle session — none of which is a new "your turn" moment. Invariant: the
            // finish-line push fires exactly once per turn, at turn end.
            if final_session.is_some()
                && matches!(status, "done" | "failed" | "killed")
                && !was_parked
            {
                let cost = final_session
                    .as_ref()
                    .and_then(|s| s.cost_usd)
                    .or(cur.cost_usd);
                let error_text = final_session.as_ref().and_then(|s| s.error.clone());
                let payload = serde_json::json!({
                    "sessionId": id,
                    "status": status,
                    "isError": status != "done",
                    "errorText": error_text,
                    "costUsd": cost,
                    "title": final_session.as_ref().map(|s| s.prompt.clone()),
                });
                push_fn(payload);
            }
        }

        // Re-pump.
        self.pump();
    }
}
