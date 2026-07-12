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
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "--initial-branch=main", "-q"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "first\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "first", "-q"]);
    let sha = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

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
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(&repo_dir).unwrap();
    run2(&["init", "--initial-branch=main", "-q"]);
    run2(&["config", "user.email", "t@t"]);
    run2(&["config", "user.name", "t"]);
    std::fs::write(repo_dir.join("README.md"), "first\n").unwrap();
    run2(&["add", "."]);
    run2(&["commit", "-m", "first", "-q"]);
    let local_sha = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_dir)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(
        local_sha, sha,
        "test setup invariant: copy has the same SHA"
    );

    let e = make_engine(&src, EngineOverrides::default()).await;
    let id = e
        .submit_session(
            vec!["demo".into()],
            vec![],
            "first user prompt".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    (e, id, repo_dir, local_sha)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_session_creates_new_session_branched_at_source_head() {
    let (e, src_id, _repo, sha) = session_with_one_commit().await;
    let forked: Session = e.fork_session(&src_id).await.unwrap();
    assert_ne!(forked.id, src_id);
    assert_eq!(forked.parent_session_id.as_deref(), Some(src_id.as_str()));
    assert_eq!(forked.status, "done");
    assert!(
        forked.prompt.starts_with("Fork of "),
        "seed prompt must be labelled: {}",
        forked.prompt
    );

    // The forked session's worktree exists and its HEAD equals the source HEAD.
    let wt = PathBuf::from(forked.worktree_path.unwrap());
    let repo_wt = wt.join(&forked.repos[0]); // e.g. session_dir/demo
    let wt_sha = String::from_utf8(
        std::process::Command::new("git")
            .args(["-C", &repo_wt.to_string_lossy(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(wt_sha, sha, "forked worktree must point at source HEAD");

    // Regression guard for the real fork_session assembly site: it injects the build-env guide too.
    let fork_md = std::fs::read_to_string(wt.join("CLAUDE.md"))
        .expect("forked session must get an injected CLAUDE.md");
    assert!(
        fork_md.contains("## Build environment — inherit it from the main checkout"),
        "fork_session must inject the worktree build-env guide"
    );
    // Same Tier-1 exclusion as submit_session: routing/fan-out live in the appended system prompt.
    assert!(
        !fork_md.contains("Model routing") && !fork_md.contains("Fan-out discipline"),
        "routing/fan-out guide must NOT be in the forked session CLAUDE.md"
    );

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
    assert!(
        forked.prompt.starts_with("Fork of "),
        "seed prompt must be labelled: {}",
        forked.prompt
    );
    assert!(
        forked
            .prompt
            .contains("# Context: previous session transcript"),
        "seed prompt must frame transcript as context: {}",
        forked.prompt
    );
    assert!(
        forked
            .prompt
            .contains("Do NOT continue the assistant's last turn"),
        "seed prompt must explicitly forbid continuation: {}",
        forked.prompt
    );
    assert!(
        forked.prompt.contains("Awaiting the user's next message"),
        "seed prompt must instruct claude to wait: {}",
        forked.prompt
    );
    // The transcript body must still appear, just framed.
    assert!(
        forked.prompt.contains("USER: hi"),
        "seed prompt must include transcript body: {}",
        forked.prompt
    );
    assert!(
        forked.prompt.contains("ASSISTANT: hello"),
        "seed prompt must include transcript body: {}",
        forked.prompt
    );
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
        .submit_session(
            vec![],
            vec![],
            "first".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    let b = e
        .submit_session(
            vec![],
            vec![],
            "second".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();

    let text = format!("check @session:{} progress", &b[..8]);
    let out = e.expand_session_mentions(&text).await;
    assert!(out.starts_with(&text), "original text must be kept: {out}");
    assert!(
        out.contains(&format!("session {b}")),
        "must resolve to the full id: {out}"
    );
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
    e.follow_up(&forked.id, "now add tests", true, None, None, None)
        .await
        .unwrap();

    // The deferred pump has not run yet on the current-thread runtime, so the item is still
    // queued (same approach as follow_up_attaches_per_turn_overrides_to_queued_item).
    let pushed = {
        let st = e.0.state.lock();
        st.queue
            .iter()
            .find(|q| q.id == forked.id)
            .cloned()
            .expect("fork's first follow-up should enqueue a QueueItem")
    };

    // The user-visible/displayed message stays clean — no 50k transcript in the bubble.
    assert_eq!(pushed.prompt, "now add tests");

    // The seed rides along as context_prefix and includes the source transcript + framing.
    let prefix = pushed
        .context_prefix
        .as_deref()
        .expect("fork's first turn must carry the seed as context_prefix");
    assert!(
        prefix.contains("# Context: previous session transcript"),
        "context_prefix must carry the framed seed: {prefix}"
    );
    assert!(
        prefix.contains("USER: build a parser"),
        "context_prefix must include the source transcript: {prefix}"
    );
    assert!(
        prefix.contains("ASSISTANT: parser done"),
        "context_prefix must include the source transcript: {prefix}"
    );

    // The text actually written to claude includes BOTH the seed and the user's message.
    let claude_text = compose_turn_text(&pushed);
    assert!(
        claude_text.contains("USER: build a parser"),
        "claude must receive the forked context: {claude_text}"
    );
    assert!(
        claude_text.ends_with("now add tests"),
        "claude must receive the user's message after the context: {claude_text}"
    );
}

/// A non-fork session's normal follow-up must NOT get a context_prefix (the seed-injection is
/// fork-only). Guards against the gate accidentally firing for ordinary sessions.
#[tokio::test]
async fn non_fork_followup_has_no_context_prefix() {
    let src = tmp();
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

    e.follow_up(&id, "second turn", false, None, None, None)
        .await
        .unwrap();

    let pushed = {
        let st = e.0.state.lock();
        st.queue
            .iter()
            .find(|q| q.id == id && q.prompt == "second turn")
            .cloned()
            .expect("follow-up should enqueue a QueueItem")
    };
    assert!(
        pushed.context_prefix.is_none(),
        "non-fork follow-up must not carry a context_prefix"
    );
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
        .submit_session(
            vec![],
            vec![],
            "target".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    let asker = e
        .submit_session(
            vec![],
            vec![],
            "asker".into(),
            HashMap::new(),
            SubmitMeta::default(),
        )
        .await
        .unwrap();
    wait_status(&e, &asker, "done").await;

    let text = format!("look at @session:{}", &target[..8]);
    e.follow_up(&asker, &text, false, None, None, None)
        .await
        .unwrap();
    wait_status(&e, &asker, "done").await;

    let log = e.0.store.read_log(&asker).join("\n");
    assert!(
        log.contains(&text),
        "raw mention marker must be logged: {log}"
    );
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
    assert_eq!(
        s.status, "done",
        "forked session must be idle (done) so follow_up is accepted, got: {}",
        s.status
    );

    // follow_up must succeed.
    e.follow_up(&forked.id, "first follow-up", true, None, None, None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_session_unknown_src_returns_not_found() {
    let dir = tmp();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let e = make_engine(&src, EngineOverrides::default()).await;
    let err = e.fork_session("does-not-exist").await.unwrap_err();
    assert!(
        format!("{err}").contains("does-not-exist") || format!("{err}").contains("not found"),
        "expected NotFound, got: {err}"
    );
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
    wait_status(&e, &src_id, "done").await;

    let forked: Session = e.fork_session(&src_id).await.unwrap();
    assert_eq!(forked.hidden_skills, vec!["skill-x".to_string()]);
    assert_eq!(
        forked.hidden_plugins,
        vec!["github@claude-plugins-official".to_string()]
    );
    // And re-reading from the store agrees.
    let persisted = e.get(&forked.id).await.unwrap();
    assert_eq!(
        persisted.hidden_plugins,
        vec!["github@claude-plugins-official".to_string()]
    );
}

/// Regression: `fork_session` must stamp the new row with `origin="fork"`. Before the fix,
/// the CreateInput built inside fork_session never set `origin`, so forks were persisted
/// with the column default `"native"` — silently indistinguishable from a normal submit.
/// The migration backfill (store.rs) upgrades any legacy `origin='native'` row that ALSO
/// has a `parentSessionId` to `"fork"`, but new writes must carry the marker from the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_session_stamps_origin_fork() {
    let (e, src_id, _repo, _sha) = session_with_one_commit().await;
    let forked: Session = e.fork_session(&src_id).await.unwrap();
    assert_eq!(
        forked.origin, "fork",
        "fork_session must persist origin='fork' on the new row, got: {}",
        forked.origin
    );
    // And re-reading from the store agrees — protects against a future refactor that
    // returns the in-memory Session from the create call but forgets the INSERT.
    let persisted = e.get(&forked.id).await.unwrap();
    assert_eq!(persisted.origin, "fork");
    // Sanity: the source row stays 'native' (fork must not mutate the source).
    let src_again = e.get(&src_id).await.unwrap();
    assert_eq!(src_again.origin, "native");
}
