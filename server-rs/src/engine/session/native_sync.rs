use std::sync::Arc;

// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.

use crate::engine::*;

impl Engine {
    // ── Native re-sync (adopt / detach round-trip) ────────────

    /// Import the native transcript (#2) delta into this session's rendered log (#1),
    /// advance the watermark, and return the count of #1 lines appended.
    ///
    /// The native transcript at `~/.claude/projects/<slug(cwd)>/<csid>.jsonl` is the
    /// complete record (every turn, from either agentic-dev or a terminal `claude`);
    /// #1 is what the client renders. `nativeWatermarkLines` marks how many #2 lines
    /// are already reflected in #1, so translating only `[watermark .. end)` imports
    /// exactly the new turns without per-line dedup.
    ///
    /// Idempotent: a second call with no new native lines translates an empty slice and
    /// appends nothing (returns 0). Returns `Ok(0)` when the transcript file is absent
    /// (e.g. an adopted csid whose file was moved) — nothing to import, not an error.
    /// Engine stays axum-free: this is pure store + filesystem work.
    pub async fn reconcile_from_native(&self, id: &str) -> Result<usize, String> {
        // Cheap pre-checks OUTSIDE the per-session lock: the early-return paths
        // (no session / no csid / file missing) do not contend with each other, so we
        // only serialise the actually-mutating critical section. Doing the early
        // returns first keeps the lock window minimal and avoids taking the lock on
        // fail-fast paths.
        let (csid, cwd) = match self.0.store.get(id).await.map_err(|e| e.to_string())? {
            None => return Err("no such session".into()),
            Some(s) => match s.claude_session_id.clone() {
                None => return Err("session has no claudeSessionId".into()),
                Some(c) => (c, s.worktree_path.clone().unwrap_or_default()),
            },
        };
        let path = crate::engine::native_transcript::transcript_path(
            &self.0.cfg.claude_config_base,
            &cwd,
            &csid,
        );
        if !path.is_file() {
            return Ok(0);
        }

        // Per-session async lock. Get-or-insert the inner `Arc<Mutex<()>>` while
        // holding the fast synchronous map mutex (cheap, non-blocking on the only
        // contention surface), then drop the map mutex BEFORE awaiting the inner
        // mutex — holding `parking_lot::Mutex` across an `.await` would block the
        // executor thread and dead-lock under load.
        let inner_lock: Arc<tokio::sync::Mutex<()>> = {
            let mut map = self.0.reconcile_locks.lock();
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        let _guard = inner_lock.lock().await;

        // CRITICAL SECTION. Re-read the session here: the pre-lock `get` may be
        // stale (a sibling task could have advanced the watermark between read
        // and lock acquisition). Re-reading inside the guard ensures we translate
        // only the lines that are STILL new.
        let s = self
            .0
            .store
            .get(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no such session")?;

        let from = s.native_watermark_lines.max(0) as usize;
        let (lines, total) = crate::engine::native_transcript::translate_range(&path, from);
        // Batch the delta into ONE append: per-line appends spawned a blocking task and
        // opened/closed the file once per line. `append_log` writes `{arg}\n`, so joining with
        // '\n' produces byte-identical file content to N single-line calls — and fewer partial
        // states if interrupted mid-import.
        if !lines.is_empty() {
            self.0
                .store
                .append_log(id, &lines.join("\n"))
                .await
                .map_err(|e| e.to_string())?;
        }
        self.0
            .store
            .set_watermark(id, total as i64)
            .await
            .map_err(|e| e.to_string())?;

        // Recency bump (Fix C): when the imported delta contains user-authored text,
        // advance `last_user_message_at` to the newest imported agentic_prompt's `at`
        // (epoch ms), but never rewind a row that has already been touched by a newer
        // turn (e.g. a subsequent prompt in a live turn wrote a higher timestamp out of
        // band). `lines` carries the freshly-appended #1 JSONL — each `agentic_prompt`
        // line has an `at` field set by `translate_lines` to the ISO-3339 epoch ms of
        // the native user turn.
        if let Some(max_at) = max_agentic_prompt_at(&lines) {
            if max_at > s.last_user_message_at {
                self.0
                    .store
                    .update(
                        id,
                        crate::engine::store::SessionPatch {
                            last_user_message_at: Some(max_at),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(lines.len())
    }

    /// Adopt an existing external Claude Code CLI session as a first-class
    /// agentic-dev session. The native session ran in `cwd` and wrote its
    /// transcript (#2) to `~/.claude/projects/<slug(cwd)>/<csid>.jsonl`; this
    /// creates an agentic-dev row pointing at that `csid` and imports the full
    /// native history into the rendered log (#1). Returns the new session id.
    ///
    /// Worktree strategy is **adopt-in-place** (v1): `worktree_path = cwd` and
    /// no new git worktree is created, so a later resume turn computes the same
    /// cwd → slug and `--resume` finds #2. `branch`/`baseSha` are read from
    /// `cwd` if it is a git repo, else left empty.
    ///
    /// Errors on a double-adopt (a row already carries this csid) or when no
    /// native transcript exists at the computed path. Engine stays axum-free:
    /// this is pure store + filesystem + `git` work.
    pub async fn adopt_session(&self, csid: &str, cwd: &str) -> Result<String, String> {
        use crate::engine::store::{CreateInput, SessionPatch};

        // Security: `csid` comes straight from the HTTP request body and is interpolated
        // into a filesystem path below (`native_transcript::transcript_path`). Reject
        // anything that isn't a bare filename component BEFORE it touches the filesystem —
        // otherwise a csid like `../../../../etc/passwd` or an absolute path escapes
        // `claude_config_base/projects/<slug>` entirely.
        if !crate::engine::native_transcript::is_valid_csid(csid) {
            return Err("invalid claudeSessionId".to_string());
        }

        // Guard: an external csid maps to at most one adopted row.
        if self
            .0
            .store
            .session_by_csid(csid)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err(format!("session {csid} already adopted"));
        }

        // Require the native transcript to exist at the slug path.
        let path = crate::engine::native_transcript::transcript_path(
            &self.0.cfg.claude_config_base,
            cwd,
            csid,
        );
        if !path.is_file() {
            return Err(format!("no transcript at {}", path.display()));
        }

        // Seed the prompt/title from the first authored native user turn.
        let native = crate::engine::native_transcript::read_lines(&path);
        let first_prompt = native
            .iter()
            .find_map(crate::engine::native_transcript::user_prompt_text)
            .unwrap_or_default();

        let id = new_session_id();
        let group_id = self
            .0
            .store
            .ensure_group("Claude Code Adopted")
            .await
            .map_err(|e| e.to_string())?;
        let (branch, base_sha) = read_git_head(cwd);

        // `Store::create` forces `claude_session_id = None` and `status = "pending"`,
        // so build the row with the fields CreateInput carries, then set the csid
        // via an update patch (mirrors the Init handler / reconcile test).
        self.0
            .store
            .create(CreateInput {
                id: id.clone(),
                prompt: first_prompt,
                worktree_path: Some(cwd.to_string()),
                branch: (!branch.is_empty()).then(|| branch.clone()),
                base_sha: (!base_sha.is_empty()).then(|| base_sha.clone()),
                group_id: Some(group_id),
                origin: Some("adopted".into()),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;

        if let Err(e) = self
            .0
            .store
            .update(
                &id,
                SessionPatch {
                    claude_session_id: Some(Some(csid.to_string())),
                    ..Default::default()
                },
            )
            .await
        {
            // Roll back the row we just created — leaving a half-created row behind
            // would permanently lock this csid against re-adoption via the
            // session_by_csid guard above (mirrors fork_session's rollback).
            let _ = self.0.store.remove(&id).await;
            return Err(e.to_string());
        }

        // Full history import + watermark (#2 → #1).
        if let Err(e) = self.reconcile_from_native(&id).await {
            let _ = self.0.store.remove(&id).await;
            return Err(e);
        }

        // An adopted row is a FINISHED/idle resumable session: it carries real history but
        // nothing is enqueued. `Store::create` leaves it 'pending', but `pending` reads as BUSY
        // to `follow_up`'s `is_busy` gate — so the user's next message would be rejected — and
        // restart recovery treats a `pending` row as a queued turn and re-enqueues the seeded
        // prompt. Flip it to the same status a normally-FINISHED turn lands on ('done', mirrors
        // fork_session's post-create flip) so the session sits idle and accepts a follow-up.
        if let Err(e) = self
            .0
            .store
            .update(
                &id,
                SessionPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
        {
            let _ = self.0.store.remove(&id).await;
            return Err(e.to_string());
        }
        Ok(id)
    }

    /// Hand an adopted session off to a terminal `claude --resume`: hard-stop the
    /// live streaming process so the terminal is the single writer, freeze the
    /// watermark at the current native line count (`#2` will only grow from the
    /// terminal after this point), mark the row `detached`, and return the
    /// `--resume` command to run. On reopen, `follow_up` sees `detached` and pulls
    /// the terminal-added delta back into `#1` via `reconcile_from_native`.
    ///
    /// Engine stays axum-free: pure store + filesystem work.
    pub async fn detach_session(&self, id: &str) -> Result<DetachInfo, String> {
        let s = self
            .0
            .store
            .get(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no such session")?;
        let csid = s
            .claude_session_id
            .clone()
            .ok_or("session has no claudeSessionId")?;
        let cwd = s.worktree_path.clone().unwrap_or_default();

        // Recompute the watermark ONLY on the FIRST detach. A repeat detach (double-click /
        // retry) while the row is ALREADY detached must not touch the watermark: between the
        // first handoff and now the terminal may have appended turns, and re-reading the
        // transcript length here would mark those new terminal lines as already-imported,
        // permanently skipping them on the next reclaim. Leave the first-detach watermark
        // frozen and just hand back the resume command again.
        if !s.detached {
            // Best-effort hard-stop the live streaming process so the terminal becomes
            // the single writer. `kill` is the existing SIGTERM path; it only *sends* the
            // stop signal (via `run.stop()`) and returns immediately — it does not wait for
            // the pump task to actually exit. Without waiting, an in-flight turn's last
            // lines may not be flushed to the native transcript (#2) yet, and the line
            // count read below would freeze the watermark too low. `wait_for_exit` is the
            // engine's real "is this session still running" signal — it polls
            // `state.running`, the same map `is_busy`/`kill` consult — bounded at 5s so a
            // stuck process can't hang detach forever; a no-op when already idle.
            self.kill(id).await;
            self.wait_for_exit(id).await;

            // Freeze the watermark at the current native line count — but never let it
            // regress below what's already been imported (`s.native_watermark_lines`,
            // read before the kill above). `read_lines` returns an empty vec when the
            // transcript file is missing or unreadable (e.g. moved/deleted out from under
            // us), and naively trusting that count would zero out an already-nonzero
            // watermark; a later reopen would then re-translate the ENTIRE native history
            // back into #1, duplicating every line already imported.
            let path = crate::engine::native_transcript::transcript_path(
                &self.0.cfg.claude_config_base,
                &cwd,
                &csid,
            );
            let read_count = crate::engine::native_transcript::read_lines(&path).len() as i64;
            let total = read_count.max(s.native_watermark_lines);
            self.0
                .store
                .set_watermark(id, total)
                .await
                .map_err(|e| e.to_string())?;
            self.0
                .store
                .set_detached(id, true)
                .await
                .map_err(|e| e.to_string())?;
        }

        // Shell-single-quote `cwd`: it may contain spaces or shell metacharacters, which would
        // otherwise break (or inject into) the `cd` when the user pastes this into a terminal.
        // `csid` is already validated (`is_valid_csid`) at adopt time, so it needs no quoting.
        let resume_cmd = format!(
            "cd {} && claude --resume {}",
            shell_single_quote(&cwd),
            csid
        );
        Ok(DetachInfo {
            cwd,
            claude_session_id: csid,
            resume_cmd,
        })
    }
}
