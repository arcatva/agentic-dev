use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::repos::ensure_local;
// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
use crate::engine::spawner::{compose_user_text, encode_user_message};
use crate::engine::status::SessionStatus;
use crate::engine::store::{CreateInput, SessionPatch, SessionUpdate};
use crate::engine::stream::ClaudeEvent;
use crate::engine::title_client::TitleGenerator;
use crate::engine::transition::TransitionReason;
use crate::engine::worktree::create_session_worktrees;

use crate::engine::*;

impl Engine {
    // ── submit_session / submit ───────────────────────────────

    /// Create a new session, enqueue it, and defer a pump. Returns the session id.
    pub async fn submit_session(
        &self,
        repos: Vec<String>,
        skills: Vec<String>,
        prompt: String,
        env: HashMap<String, String>,
        meta: SubmitMeta,
    ) -> Result<String, String> {
        let now = self.now();

        // Resolve repos to local paths (clone if absent).
        let default_clone: Arc<dyn Fn(&str, &str) -> std::io::Result<()> + Send + Sync> =
            Arc::new(crate::engine::repos::default_clone);
        let clone_fn: &dyn Fn(&str, &str) -> std::io::Result<()> = self
            .0
            .cfg
            .clone_fn
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str, &str) -> std::io::Result<()>)
            .unwrap_or_else(|| default_clone.as_ref());

        let repo_specs: Vec<(String, PathBuf)> = repos
            .iter()
            .map(|r| {
                ensure_local(r, &self.0.cfg.src_root, &self.0.cfg.git_org, clone_fn)
                    .map(|p| (r.clone(), p))
                    .map_err(|e| format!("repo {r}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let id = new_session_id();

        // Create worktrees (or use session dir directly for no-repo sessions).
        let (session_dir, base_shas, wts) = if repo_specs.is_empty() {
            // No repos: session dir is in worktrees_root/<id>, no worktrees to create.
            let session_dir = self.0.cfg.worktrees_root.join(&id);
            std::fs::create_dir_all(&session_dir)
                .map_err(|e| format!("create session dir: {e}"))?;
            (session_dir, std::collections::HashMap::new(), vec![])
        } else {
            let wts = create_session_worktrees(&repo_specs, &self.0.cfg.worktrees_root, &id)
                .map_err(|e| format!("create worktrees: {e}"))?;
            let session_dir = self.0.cfg.worktrees_root.join(&id);
            let base_shas: std::collections::HashMap<String, Option<String>> = wts
                .iter()
                .map(|w| (w.repo.clone(), Some(w.base_sha.clone())))
                .collect();
            (session_dir, base_shas, wts)
        };

        // Session-dir CLAUDE.md (Tier-2 project memory). Claude Code loads it for the session —
        // directly (multi-repo / no-repo cwd IS the session dir) or via parent-dir traversal
        // (single-repo cwd is `session_dir/<repo>`). It layers ON TOP of each repo's own committed
        // CLAUDE.md. Composed in order:
        //   1. the build-env guide (always),
        //   2. the multi-repo orientation guide (only when there's more than one worktree), and
        //   3. the user's session-scoped custom guidance from the New-request form.
        // The routing / fan-out rules are Tier-1 — appended to the system prompt on each main turn
        // (see `spawn_opts`), NOT written here, so delegate workers / the router never load them.
        // Best-effort — a write failure must never abort the session.
        {
            let mut sections: Vec<String> =
                vec![crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string()];
            if wts.len() > 1 {
                let repo_summaries: Vec<(String, Option<String>)> = wts
                    .iter()
                    .map(|w| {
                        (
                            w.repo.clone(),
                            crate::engine::session_guide::summarize_repo(&w.worktree_path),
                        )
                    })
                    .collect();
                sections.push(crate::engine::session_guide::build_session_guide(
                    &repo_summaries,
                    &skills,
                ));
            }
            if let Some(custom) = meta
                .claude_md
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                sections.push(custom.to_string());
            }
            crate::engine::session_guide::write_session_claude_md(&session_dir, &sections);
        }

        // Adopt any files staged before this session existed (the New-request form attaches files
        // while there is no session id yet). Move each from `<worktrees_root>/.staging/<token>/<name>`
        // into this session's `uploads/` dir NOW — synchronously, before the prompt is enqueued and
        // the agent spawns — so the file is on disk before the agent reads the prompt's
        // `[attached: uploads/<name>]` path. The uploads dir mirrors the per-session upload route's
        // `session_cwd`: a single-repo session's cwd is `session_dir/<repo>`, everything else is
        // `session_dir`. Best-effort per file — a missing/unreadable staged file is skipped (its
        // marker path just won't resolve) rather than aborting the whole session.
        if !meta.staged_uploads.is_empty() {
            let uploads_dir = if repos.len() == 1 {
                session_dir.join(&repos[0]).join("uploads")
            } else {
                session_dir.join("uploads")
            };
            if let Err(e) = std::fs::create_dir_all(&uploads_dir) {
                tracing::warn!("[engine] create uploads dir failed: {e}");
            }
            let staging = crate::engine::staging_root(&self.0.cfg.worktrees_root);
            for su in &meta.staged_uploads {
                // Never trust client-supplied path components: sanitize both to a single safe leaf so
                // a crafted token/name can't traverse out of the staging tree or the uploads dir.
                let token = crate::engine::sanitize_upload_name(&su.token);
                let name = crate::engine::sanitize_upload_name(&su.name);
                let src = staging.join(&token).join(&name);
                let dst = uploads_dir.join(&name);
                // Same filesystem (staging lives under worktrees_root) → rename is a cheap move; fall
                // back to a read+write copy if rename fails for any reason.
                if std::fs::rename(&src, &dst).is_err() {
                    match std::fs::read(&src) {
                        Ok(bytes) => {
                            let _ = std::fs::write(&dst, &bytes);
                        }
                        Err(e) => {
                            tracing::warn!("[engine] staged upload {token}/{name} unreadable: {e}")
                        }
                    }
                }
                // Best-effort cleanup of the (now empty) staging token dir.
                let _ = std::fs::remove_dir_all(staging.join(&token));
            }
        }

        let branch = format!("agentic/{id}");

        // Build base_sha from first repo's sha.
        let base_sha: Option<String> = wts.first().map(|w| w.base_sha.clone());

        // Store the session.
        let store = self.0.store.clone();
        let create_input = CreateInput {
            id: id.clone(),
            repos: repos.clone(),
            skills: skills.clone(),
            hidden_skills: meta.hidden_skills,
            hidden_plugins: meta.hidden_plugins,
            hidden_mcp_servers: meta.hidden_mcp_servers,
            extra_mcp_servers: meta.extra_mcp_servers,
            forced_on_plugins: meta.forced_on_plugins,
            forced_on_skills: meta.forced_on_skills,
            forced_on_mcp_servers: meta.forced_on_mcp_servers,
            prompt: prompt.clone(),
            worktree_path: Some(session_dir.to_string_lossy().into_owned()),
            branch: Some(branch),
            model: meta.model,
            effort: meta.effort,
            mode: meta.mode,
            permission_mode: meta.permission_mode,
            base_shas,
            base_sha,
            ..Default::default()
        };

        store
            .create(create_input)
            .await
            .map_err(|e| format!("store.create: {e}"))?;

        // Generate a stable title for the session via the Anthropic HTTP
        // title generator. Fire-and-forget: submit_session returns immediately
        // and the title lands asynchronously. On any failure (timeout, non-2xx,
        // invalid output) the title is left as the user's original prompt —
        // silent fallback per spec.
        {
            let engine = self.clone();
            let id_for_title = id.clone();
            let prompt_for_title = prompt.clone();
            let session_dir_for_title = session_dir.clone();
            tokio::spawn(async move {
                let res = engine
                    .0
                    .cfg
                    .title_generator
                    .generate(&prompt_for_title, &session_dir_for_title)
                    .await;
                match res {
                    Ok(Some(t)) => {
                        if let Err(e) = engine
                            .0
                            .store
                            .apply_update(&id_for_title, SessionUpdate::new().prompt(t))
                            .await
                        {
                            tracing::warn!("[engine] title store.update failed: {e}");
                        }
                    }
                    Ok(None) => { /* invalid output, keep prompt */ }
                    Err(e) => tracing::warn!("[engine] title generator error: {e:?}"),
                }
            });
        }

        // Enqueue and defer pump.
        {
            let mut state = self.0.state.lock();
            state.queue.push_back(QueueItem {
                id: id.clone(),
                prompt: prompt.clone(),
                env,
                resume_session_id: None,
                enqueued_at: Some(now),
                model: None,
                effort: None,
                permission_mode: None,
                context_prefix: None,
            });
        }
        self.defer_pump();

        Ok(id)
    }

    /// Convenience: submit a single-repo session.
    pub async fn submit(
        &self,
        repo: &str,
        prompt: &str,
        env: HashMap<String, String>,
    ) -> Result<String, String> {
        self.submit_session(
            vec![repo.to_string()],
            vec![],
            prompt.to_string(),
            env,
            SubmitMeta::default(),
        )
        .await
    }

    /// Read the session's log, build the recent-message list, and call
    /// `title_generator.maybe_retitle`. On success, write the new title
    /// to `sessions.prompt`. On every failure path, leave the title
    /// unchanged. This is fire-and-forget — `follow_up` calls it from a
    /// `tokio::spawn` so the API handler does not block.
    pub async fn maybe_retitle_session(&self, id: &str) -> Option<String> {
        let session = self.0.store.get(id).await.ok().flatten()?;
        // Never overwrite a title the user pinned by manually renaming the
        // session (a set_title=true follow-up sets title_pinned). Machine titles
        // from submit-time generate leave it false, so they can still be retitled.
        if session.title_pinned {
            return None;
        }
        let current_title = session.prompt;
        let lines = self.0.store.read_log(id);
        let recent = crate::engine::title::parse_recent_messages(&lines);
        let cwd = self.0.cfg.worktrees_root.join(id);
        let new_title = self
            .0
            .cfg
            .title_generator
            .maybe_retitle(&current_title, &recent, &cwd)
            .await
            .ok()
            .flatten()?;
        if let Err(e) = self
            .0
            .store
            .update(
                id,
                crate::engine::store::SessionPatch {
                    prompt: Some(new_title.clone()),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!("[engine] retitle store.update failed: {e}");
        }
        Some(new_title)
    }

    /// Fire-and-forget a periodic retitle if enabled and this turn lands on the
    /// cadence boundary. Cadence is driven by the count of `agentic_prompt`
    /// markers persisted in the session log (restart-stable), NOT the in-memory
    /// `act.turns` counter — that resets to 0 on a server restart and would
    /// otherwise shift the every-Nth-turn phase. MUST be called AFTER this
    /// turn's prompt marker has been appended to the log. No subscriber event is
    /// emitted — title changes are metadata, not conversation.
    pub(crate) fn maybe_spawn_retitle(&self, id: &str) {
        if !self.0.cfg.retitle_enabled {
            return;
        }
        let turns = crate::engine::title::count_user_turns(&self.0.store.read_log(id));
        if turns > 0 && turns % RETITLE_EVERY_TURNS == 0 {
            let engine = self.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                engine.maybe_retitle_session(&id).await;
            });
        }
    }

    /// Returns true if the session is busy in a non-injectable state.
    pub(crate) fn is_busy(&self, id: &str, status: &str) -> bool {
        let state = self.0.state.lock();
        state.running.contains_key(id)
            || state.starting.contains(id)
            || state.queue.iter().any(|q| q.id == id)
            || status == "pending"
            || status == "running"
    }

    /// Expand `@session:<id-prefix>` mentions in an outgoing prompt (see [mentions]) so the
    /// receiving claude gets the mentioned session's identity + on-disk paths. Called on the
    /// DELIVERED text only — logged prompt markers keep the raw token. Best-effort: on a store
    /// error the text passes through unchanged (a mention must never fail a turn).
    pub(crate) async fn expand_session_mentions(&self, text: &str) -> String {
        if !text.contains("@session:") {
            return text.to_string();
        }
        match self.0.store.list().await {
            Ok(sessions) => {
                let store = &self.0.store;
                mentions::expand_session_mentions(text, &sessions, &|id| store.log_path(id))
            }
            Err(e) => {
                tracing::warn!(
                    "[engine] @session mention expansion skipped — store.list failed: {e}"
                );
                text.to_string()
            }
        }
    }

    /// Follow up on an existing session: inject a message if live, or re-queue if idle.
    pub async fn follow_up(
        &self,
        id: &str,
        prompt: &str,
        set_title: bool,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
    ) -> Result<i64, EngineError> {
        let now = self.now();

        // Get the session.
        let s = self
            .0
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

        // Check if live (running and awaiting input → injectable).
        let live_run = {
            let state = self.0.state.lock();
            if state.running.contains_key(id) {
                // Extract the run handle and saw_result for write
                state
                    .running
                    .get(id)
                    .map(|rt| (rt.run.clone(), rt.saw_result.clone()))
            } else {
                None
            }
        };

        if let Some((run_handle, saw_result)) = live_run {
            // Live branch: inject over stdin.
            let since = self.0.store.read_log(id).len() as i64;

            // Build patch (retitle + clear error + stamp lastUserMessageAt).
            // auto_resume_at is dropped too: any accepted follow-up (manual or the auto-resume
            // scheduler's own) supersedes a pending scheduled resume.
            let mut patch = SessionPatch {
                error: Some(None),
                error_kind: Some(None),
                exit_code: Some(None),
                last_user_message_at: Some(now),
                auto_resume_at: Some(None),
                ..Default::default()
            };
            if set_title {
                patch.prompt = Some(prompt.to_string());
                // User manually renamed the session → pin it so periodic
                // retitle won't silently overwrite the user's choice.
                patch.title_pinned = Some(true);
            }

            if let Err(e) = self.0.store.update(id, patch).await {
                tracing::error!("[engine] store.update live-inject patch failed: {e}");
            }

            // Append prompt marker to log.
            let log_line = prompt_event_json(prompt, now).to_string();
            if let Err(e) = self.0.store.append_log(id, &log_line).await {
                tracing::error!("[engine] store.append_log prompt marker failed: {e}");
            }

            // Emit prompt event to subscribers.
            // Use ClaudeEvent::Prompt (not Other) so to_wire produces kind:"prompt",
            // which the Android client renders as the user message bubble in the live stream.
            let prompt_raw = prompt_event_json(prompt, now);
            let prompt_ev = ClaudeEvent::Prompt {
                text: prompt.to_string(),
                at: now,
                raw: prompt_raw,
            };
            self.emit(id, &prompt_ev);

            // Update runtime state.
            {
                let mut state = self.0.state.lock();
                let act = state.activity.entry(id.to_string()).or_default();
                act.turns += 1;
                state.awaiting.insert(id.to_string(), false);
                state.pending_ask.remove(id);
                state.pending_perm.remove(id);
                state.parked.remove(id);
                state.last_event_at.insert(id.to_string(), now);
                state.turn_started_at.insert(id.to_string(), now);
            }

            // Periodic retitle on the cadence boundary. The prompt marker was
            // appended above, so the persisted agentic_prompt count includes
            // this turn. (Cadence is restart-stable — see maybe_spawn_retitle.)
            self.maybe_spawn_retitle(id);

            // Write the user message — reset saw_result first (mirrors SpawnHandle::write).
            // `@session:<id>` mentions are expanded on the DELIVERED text only — the prompt
            // marker logged above stays the user's raw text, so the UI bubble is untouched.
            let delivered = self.expand_session_mentions(prompt).await;
            saw_result.store(false, std::sync::atomic::Ordering::SeqCst);
            run_handle.write(&encode_user_message(&compose_user_text(&delivered)));

            return Ok(since);
        }

        // Not live: check if busy in a non-injectable state.
        if self.is_busy(id, &s.status) {
            return Err(EngineError::Busy);
        }

        // FIX (reclaim cursor): snapshot the log-position cursor BEFORE the reclaim reconcile
        // below. The reclaim appends the terminal-added turns to #1; if the returned stream
        // cursor were computed AFTER that append, the client would start its stream past the
        // reclaimed lines and silently skip the terminal turns. Capturing `since` here — at the
        // pre-reclaim log length — makes the returned cursor PRECEDE the reconciled lines so the
        // client stream includes them.
        let since = self.0.store.read_log(id).len() as i64;

        // Reclaim-on-reopen: if this session was detached to a terminal `claude`, the
        // terminal may have appended turns to #2 while we were stopped. Pull that delta
        // into #1 and clear the flag BEFORE preparing the resume turn, so `--resume`
        // continues from the reconciled history and turn counts self-correct.
        if s.detached {
            // Clear `detached` (reclaim ownership) ONLY when the native transcript file actually
            // EXISTS and the import succeeded. `reconcile_from_native` returns Ok(0) when the
            // transcript file is ABSENT (nothing to import) — but a transiently-missing file
            // (moved/not yet synced) is exactly the case we must retry, not abandon. Clearing the
            // flag on that Ok(0) would drop the retry (the next reopen no longer sees
            // `detached == true`). So: only a present-file + successful reconcile clears detached;
            // an absent file or a failed reconcile leaves detached=true for the next reopen. The
            // turn itself still proceeds either way — this is best-effort bookkeeping, not a gate.
            let transcript_exists = s
                .claude_session_id
                .as_deref()
                .map(|csid| {
                    crate::engine::native_transcript::transcript_path(
                        &self.0.cfg.claude_config_base,
                        s.worktree_path.as_deref().unwrap_or_default(),
                        csid,
                    )
                    .is_file()
                })
                .unwrap_or(false);
            match self.reconcile_from_native(id).await {
                Ok(_) if transcript_exists => {
                    let _ = self.0.store.set_detached(id, false).await; // agentic-dev reclaims ownership
                }
                Ok(_) => {
                    tracing::warn!(
                        "[engine] follow_up reclaim: native transcript missing for {id}, leaving detached=true for retry on next reopen"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "[engine] follow_up reclaim: reconcile_from_native failed for {id}, leaving detached=true for retry on next reopen: {e}"
                    );
                }
            }
        }

        // Read the (possibly reclaim-grown) log for the snapshot turn index. NOTE: the returned
        // `since` cursor was captured ABOVE, before the reclaim, on purpose (see the FIX comment);
        // do NOT recompute it from this post-reclaim read or the client will skip reclaimed lines.
        let log = self.0.store.read_log(id);

        // Rewind support: snapshot the (stable, idle) working tree BEFORE this resumed turn runs, so
        // the user can later rewind to "just before this prompt". Labeled with the about-to-start
        // turn's index = number of prompts already in the log. Best-effort — never fails the turn.
        let turn_index = crate::engine::title::count_user_turns(&log);
        self.snapshot_worktrees(&s, turn_index);

        // Fork's first turn: deliver the source session's transcript (the "seed prompt") as
        // context for this turn. fork_session stored it in the new session's `prompt` column but
        // it is never otherwise sent to claude — a fresh fork has no `claude_session_id`, so the
        // `--resume` carry-over below is a no-op (`resume_session_id` is `None`). Without this the
        // forked claude starts with zero knowledge of what it forked from. We read the seed from
        // `s` (loaded at the top of follow_up, BEFORE the retitle patch a few lines down), so a
        // `set_title=true` follow-up that overwrites the prompt column cannot destroy it. Gate on
        // an empty log (`since == 0`) so the seed is injected only on the very first turn, never
        // again. Carried on the QueueItem rather than folded into `prompt` so the logged/displayed
        // user message stays the user's text, not the (up to 50k char) transcript.
        let context_prefix = if s.parent_session_id.is_some() && since == 0 && !s.prompt.is_empty()
        {
            Some(s.prompt.clone())
        } else {
            None
        };

        // Update store: clear error + retitle + status=pending + stamp lastUserMessageAt.
        // The error/errorKind/exitCode clear mirrors the live-inject branch above (line ~552): a session
        // the user has just resumed is no longer in an "errored terminal" state from the client's view, so
        // we drop those fields the moment the follow-up lands in the queue. Without this, the Android
        // client's session-row banner (driven by errorKind != null in StopReason.hasError) lingers
        // throughout the `pending` window (sync, queue wait, max_concurrent backpressure) — sometimes
        // several seconds — looking like the recovery didn't take. Mirroring the live branch keeps the
        // invariant uniform: any accepted follow-up clears the prior turn's error fields.
        // PR6: route through transition() so the status + clears are
        // centralized. last_user_message_at + prompt still go through
        // a follow-up SessionUpdate (transition() doesn't touch them).
        let _ = self
            .transition(id, SessionStatus::Pending, TransitionReason::FollowUpQueued)
            .await
            .map_err(|e| tracing::error!("[engine] transition follow_up→pending failed: {e}"));
        let mut follow_up = SessionUpdate::new()
            .last_user_message_at(now)
            .clear_error()
            .clear_error_kind()
            .clear_exit_code()
            // A pending scheduled auto-resume is superseded by this follow-up.
            .clear_auto_resume_at();
        if set_title {
            // User manually renamed the session → pin it (see live branch above).
            follow_up = follow_up.prompt(prompt.to_string()).title_pinned(true);
        }
        if let Err(e) = self.0.store.apply_update(id, follow_up).await {
            tracing::error!("[engine] store.update follow-up patch failed: {e}");
        }

        // Enqueue with resume_session_id so the resumed turn uses --resume.
        {
            let mut state = self.0.state.lock();
            state.queue.push_back(QueueItem {
                id: id.to_string(),
                prompt: prompt.to_string(),
                env: HashMap::new(),
                resume_session_id: s.claude_session_id.clone(),
                enqueued_at: Some(now),
                model,
                effort,
                permission_mode,
                context_prefix,
            });
        }

        // NOTE: the periodic retitle for queued/resumed turns is triggered in
        // start() — AFTER the new prompt is appended to the log and act.turns
        // is incremented — so the retitle reads a log that already contains the
        // triggering message. (The live-inject branch above triggers inline
        // because it appends the prompt and increments turns itself.)
        self.defer_pump();

        Ok(since)
    }

    /// Create a new session that is a fork of `src_id`:
    ///   - the new session's per-repo worktree branches off `src_id`'s HEAD (snapshot).
    ///   - the new session's `prompt` is the source's transcript filtered into plain text,
    ///     framed inside a "# Context: previous session transcript" block so claude treats it
    ///     as background reference (not as its own prior output) and waits for a fresh user
    ///     message before responding.
    ///   - the new session's `parentSessionId` is `src_id`.
    ///   - the new session is NOT enqueued — it sits idle with `status == "done"` until the
    ///     user opens it and sends a real follow-up prompt (the normal follow-up path spawns).
    ///     ("done" = idle; "pending" is blocked by `is_busy` which is what the follow-up path
    ///     checks before accepting a turn.)
    ///
    /// Returns the new session row on success. On any failure after partial work (some
    /// worktrees created) the worktrees are removed and the row is deleted before returning
    /// the error.
    pub async fn fork_session(
        &self,
        src_id: &str,
    ) -> Result<crate::engine::store::Session, EngineError> {
        use crate::engine::store::{CreateInput, SessionPatch, StoreError};
        use crate::engine::transcript_filter::filter_log_to_transcript;

        let Some(src) = self.get(src_id).await else {
            return Err(EngineError::NotFound(src_id.into()));
        };

        // Resolve repos to local paths (same as submit_session).
        let default_clone: Arc<dyn Fn(&str, &str) -> std::io::Result<()> + Send + Sync> =
            Arc::new(crate::engine::repos::default_clone);
        let clone_fn: &dyn Fn(&str, &str) -> std::io::Result<()> = self
            .0
            .cfg
            .clone_fn
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str, &str) -> std::io::Result<()>)
            .unwrap_or_else(|| default_clone.as_ref());

        let repo_specs: Vec<(String, std::path::PathBuf)> = if src.repos.is_empty() {
            Vec::new()
        } else {
            src.repos
                .iter()
                .map(|r| {
                    crate::engine::repos::ensure_local(
                        r,
                        &self.0.cfg.src_root,
                        &self.0.cfg.git_org,
                        clone_fn,
                    )
                    .map(|p| (r.clone(), p))
                    .map_err(|e| EngineError::Internal(format!("repo {r}: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?
        };

        // Read each source worktree's HEAD (snapshot point).
        let mut base_shas: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (repo, _repo_path) in &repo_specs {
            let wt = self.0.cfg.worktrees_root.join(src_id).join(repo);
            if !wt.exists() {
                return Err(EngineError::SourceUnhealthy(format!(
                    "source worktree missing for repo {repo}"
                )));
            }
            let sha = std::process::Command::new("git")
                .args(["-C", &wt.to_string_lossy(), "rev-parse", "HEAD"])
                .output()
                .map_err(|e| EngineError::Internal(format!("rev-parse: {e}")))?;
            if !sha.status.success() {
                return Err(EngineError::SourceUnhealthy(format!(
                    "source HEAD unreadable for repo {repo}: {}",
                    truncate_chars(&String::from_utf8_lossy(&sha.stderr), 200)
                )));
            }
            base_shas.insert(
                repo.clone(),
                String::from_utf8_lossy(&sha.stdout).trim().to_string(),
            );
        }

        // Create the new session id (same generator as submit_session uses internally).
        let id = new_session_id();

        // Create the new worktrees, branched off the source HEAD. On failure, no rollback is
        // needed yet (nothing has been persisted).
        let wts = if repo_specs.is_empty() {
            let session_dir = self.0.cfg.worktrees_root.join(&id);
            std::fs::create_dir_all(&session_dir)
                .map_err(|e| EngineError::Internal(format!("create session dir: {e}")))?;
            Vec::new()
        } else {
            crate::engine::worktree::create_fork_worktrees(
                &repo_specs,
                &self.0.cfg.worktrees_root,
                &id,
                &base_shas,
            )
            .map_err(|e| EngineError::Internal(truncate_chars(&format!("{e}"), 200).to_string()))?
        };

        let session_dir = self.0.cfg.worktrees_root.join(&id);
        let base_shas_db: std::collections::HashMap<String, Option<String>> = wts
            .iter()
            .map(|w| (w.repo.clone(), Some(w.base_sha.clone())))
            .collect();

        // Session CLAUDE.md (Tier-2): build-env guide (always) + multi-repo orientation (when >1
        // worktree) + custom guidance. Routing / fan-out are Tier-1 (system prompt), not written here.
        {
            let mut sections = vec![crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string()];
            if wts.len() > 1 {
                let repo_summaries: Vec<(String, Option<String>)> = wts
                    .iter()
                    .map(|w| {
                        (
                            w.repo.clone(),
                            crate::engine::session_guide::summarize_repo(&w.worktree_path),
                        )
                    })
                    .collect();
                sections.push(crate::engine::session_guide::build_session_guide(
                    &repo_summaries,
                    &src.skills,
                ));
            }
            crate::engine::session_guide::write_session_claude_md(&session_dir, &sections);
        }

        let branch = format!("agentic/{id}");
        let base_sha_first = wts.first().map(|w| w.base_sha.clone());

        // Read the source log and build the seed prompt.
        let log_raw = std::fs::read_to_string(self.0.store.log_path(src_id)).unwrap_or_default();
        let transcript = filter_log_to_transcript(&log_raw);
        let mut visible_label: String = src.prompt.chars().take(50).collect();
        if src.prompt.chars().count() > 50 {
            visible_label.push('…');
        }
        let seed_prompt = if transcript.is_empty() {
            format!("Fork of {}:", visible_label)
        } else {
            format!(
                "Fork of {}:\n\n# Context: previous session transcript (for reference only)\n\n\
The following is a transcript of a previous session. Treat it as background context, NOT as your own prior output. \
Do NOT continue the assistant's last turn — wait for the user's next message in THIS session before responding.\n\n\
{}\n\n\
# Continue\n\n\
The new session is now active. Awaiting the user's next message.",
                visible_label,
                transcript.trim_end()
            )
        };

        // Insert the new row. parent_session_id is set here.
        let create_input = CreateInput {
            id: id.clone(),
            prompt: seed_prompt,
            repos: src.repos.clone(),
            skills: src.skills.clone(),
            hidden_skills: src.hidden_skills.clone(),
            hidden_plugins: src.hidden_plugins.clone(),
            hidden_mcp_servers: src.hidden_mcp_servers.clone(),
            extra_mcp_servers: src.extra_mcp_servers.clone(),
            forced_on_plugins: src.forced_on_plugins.clone(),
            forced_on_skills: src.forced_on_skills.clone(),
            forced_on_mcp_servers: src.forced_on_mcp_servers.clone(),
            worktree_path: Some(session_dir.to_string_lossy().into_owned()),
            branch: Some(branch),
            model: src.model.clone(),
            effort: src.effort.clone(),
            mode: src.mode.clone(),
            permission_mode: src.permission_mode.clone(),
            base_shas: base_shas_db,
            base_sha: base_sha_first,
            parent_session_id: Some(src_id.into()),
            // Fork provenance: this row is the child of `fork_session`, not a normal
            // native submission. Mirrors `adopt_session`'s `origin: Some("adopted")`.
            origin: Some("fork".into()),
            ..Default::default()
        };

        let inserted = match self.0.store.create(create_input).await {
            Ok(s) => s,
            Err(e @ StoreError::Sqlx(_)) => {
                // Roll back the worktrees we just made before propagating the error.
                self.remove_worktrees_best_effort(&repo_specs, &id);
                return Err(EngineError::Store(e));
            }
            Err(e @ StoreError::Io(_)) => {
                self.remove_worktrees_best_effort(&repo_specs, &id);
                return Err(EngineError::Store(e));
            }
            Err(e @ StoreError::Json(_)) => {
                self.remove_worktrees_best_effort(&repo_specs, &id);
                return Err(EngineError::Store(e));
            }
        };

        // Flip the freshly-created row from "pending" (Store::create default) to "done" so the
        // session sits idle and `follow_up` is accepted when the user opens it. We do NOT enqueue
        // here — the fork only runs when the user opens it and sends a follow-up.
        if let Err(e) = self
            .0
            .store
            .update(
                &inserted.id,
                SessionPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
        {
            // Roll back fully: the row was created with status "pending" and the flip to "done"
            // failed, so leaving it would strand an unusable session — a follow-up would reject it
            // forever via is_busy("pending"), and nothing else ever cleans up an orphan row.
            // Delete the row AND its worktrees so fork stays atomic: success or nothing.
            let _ = self.0.store.remove(&id).await;
            self.remove_worktrees_best_effort(&repo_specs, &id);
            return Err(EngineError::Store(e));
        }
        let mut inserted = inserted;
        inserted.status = "done".into();
        Ok(inserted)
    }

    /// Best-effort cleanup helper used by `fork_session` rollback. Mirrors
    /// `remove_session_worktrees` but takes `(repo, repo_path)` pairs instead of a separate
    /// session dir. Logs failures at debug level and never returns.
    fn remove_worktrees_best_effort(&self, repo_specs: &[(String, std::path::PathBuf)], id: &str) {
        let session_dir = self.0.cfg.worktrees_root.join(id);
        for (repo, repo_path) in repo_specs {
            let wt = session_dir.join(repo);
            let rp = repo_path.to_string_lossy();
            let wp = wt.to_string_lossy();
            if let Err(e) = crate::engine::worktree::git_sync(&[
                "-C", &rp, "worktree", "remove", "--force", &wp,
            ]) {
                tracing::debug!(repo = %repo, worktree = %wp, "fork rollback: worktree remove failed: {e}");
            }
            if let Err(e) = crate::engine::worktree::git_sync(&["-C", &rp, "worktree", "prune"]) {
                tracing::debug!(repo = %repo, "fork rollback: worktree prune failed: {e}");
            }
        }
        // Remove the session dir itself — mirrors delete_session/discard. After the per-repo
        // worktrees are removed the dir can still hold the multi-repo session guide and empty repo
        // dirs; without this, repeated fork failures leak session directories under worktrees_root.
        if let Err(e) = std::fs::remove_dir_all(&session_dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(session_dir = %session_dir.to_string_lossy(), "fork rollback: session dir remove failed: {e}");
            }
        }
    }

    /// Kill a session. If running: mark killed, stop process. If queued: drop from queue.
    pub async fn kill(&self, id: &str) {
        // A kill is a deliberate stop in EVERY state: cancel any scheduled auto-resume
        // unconditionally. Without this, killing an already-FAILED usage-limited session
        // (DELETE /api/sessions/{id}) would leave the schedule intact and the scheduler
        // would resurrect the session later.
        let _ = self
            .0
            .store
            .apply_update(
                id,
                crate::engine::store::SessionUpdate::new().clear_auto_resume_at(),
            )
            .await
            .map_err(|e| tracing::warn!("[engine] kill: auto-resume cancel failed for {id}: {e}"));

        let run_handle = {
            let state = self.0.state.lock();
            state.running.get(id).map(|r| r.run.clone())
        };

        if let Some(run) = run_handle {
            // Running: mark killed before stopping so on_exit honors it.
            // PR6: route through transition() so the killed-backstop
            // (clear error/error_kind) is centralized.
            if let Err(e) = self
                .transition(id, SessionStatus::Killed, TransitionReason::Kill)
                .await
            {
                tracing::error!("[engine] transition kill failed: {e}");
            }
            run.stop();
        } else {
            // Not running: drop from queue.
            {
                let mut state = self.0.state.lock();
                state.queue.retain(|q| q.id != id);
            }
            // If status was pending → mark killed with endedAt.
            // PR6: transition() with KillQueued reason does this.
            if let Ok(Some(s)) = self.0.store.get(id).await {
                if s.status == "pending" {
                    if let Err(e) = self
                        .transition(id, SessionStatus::Killed, TransitionReason::KillQueued)
                        .await
                    {
                        tracing::error!("[engine] transition pending→killed failed: {e}");
                    }
                }
            }
        }
    }

    /// Interrupt the running session (clears pending_ask; sends interrupt signal).
    pub fn interrupt(&self, id: &str) {
        let run_handle = {
            let mut state = self.0.state.lock();
            state.pending_ask.remove(id);
            state.pending_perm.remove(id);
            state.parked.remove(id);
            state.running.get(id).map(|r| r.run.clone())
        };
        if let Some(run) = run_handle {
            run.interrupt();
        }
    }

    /// Answer a parked perm/plan permission prompt for a live session: clear the watchdog exemption
    /// (`pending_perm`/`parked`) and forward the allow/deny to the running turn's handle, which relays
    /// it to the bridge's parked `canUseTool`. A later `perm`/`plan` event re-arms the exemption. The
    /// bridge writes the `agentic_perm_resolved` marker, so the resolution flows back through the tailer
    /// (not synthesized here). No-op if the session isn't running.
    pub fn respond_permission(&self, id: &str, decision: &str, feedback: Option<&str>) {
        let run_handle = {
            let mut state = self.0.state.lock();
            state.pending_perm.remove(id);
            state.parked.remove(id);
            state.running.get(id).map(|r| r.run.clone())
        };
        if let Some(run) = run_handle {
            run.respond_permission(decision, feedback);
        }
    }
}
