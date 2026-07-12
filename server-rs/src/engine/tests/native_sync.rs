use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::title_client::{InMemoryTitleGenerator, TitleGenerator};

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
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidR");
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

/// Regression: two concurrent reconcile_from_native calls for the SAME session id must
/// not duplicate the imported delta. Before the fix, the function read
/// `native_watermark_lines`, translated `[from..end)`, appended each line, then advanced
/// the watermark — all unsynchronised. Two callers racing on the same fresh watermark
/// would both see `from=0`, both translate the same delta, both append it, then both
/// stamp the watermark to the same final value — duplicate transcript lines.
///
/// The fix serialises per-id via a `parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>`
/// lookup/insert + an inner async lock on the critical section. Two `tokio::join!` calls
/// against a 3-user-line transcript must together import exactly 3 agentic_prompt lines,
/// sum to 3 in their return values, and leave the watermark at exactly 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_concurrent_calls_same_session_no_duplicate_append() {
    use crate::engine::native_transcript;
    use std::io::Write;

    let work = tmp();
    let e = engine_from(&work).await;
    let id = "sess-recon-conc";

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
                claude_session_id: Some(Some("csidC".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // 3 user-authored native lines.
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidC");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(&tp).unwrap();
    writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-09T00:00:00Z","message":{{"role":"user","content":"u1"}}}}"#
        ).unwrap();
    writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-09T00:00:01Z","message":{{"role":"user","content":"u2"}}}}"#
        ).unwrap();
    writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-09T00:00:02Z","message":{{"role":"user","content":"u3"}}}}"#
        ).unwrap();

    // Two concurrent calls on the same session. They will serialise on the per-id lock;
    // the first imports all 3 user prompts (3 agentic_prompt lines), the second sees
    // an advanced watermark and imports nothing. Sum of return values MUST equal 3.
    let (n1, n2) = tokio::join!(e.reconcile_from_native(id), e.reconcile_from_native(id),);
    let n1 = n1.unwrap();
    let n2 = n2.unwrap();
    assert_eq!(
            n1 + n2,
            3,
            "two concurrent reconciles must together import exactly the 3 native user lines; got n1={n1}, n2={n2}",
        );

    // Rendered log carries exactly one set of 3 agentic_prompt lines — no duplicates.
    let log = e.0.store.read_log(id);
    let prompts: Vec<String> = log
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|o| o["type"] == "agentic_prompt")
        .map(|o| o["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        prompts,
        vec!["u1".to_string(), "u2".to_string(), "u3".to_string()],
        "rendered log must have exactly 3 agentic_prompt lines in order, no dupes; got {prompts:?}",
    );

    // Watermark advanced to the full native line count.
    let s = e.get(id).await.unwrap();
    assert_eq!(
        s.native_watermark_lines, 3,
        "watermark must reach the native line count (3 user lines)",
    );
}

/// Regression: when reconcile_from_native imports a delta that includes a user-authored
/// turn, it must bump `last_user_message_at` to the ISO-3339 epoch ms of the newest
/// imported `agentic_prompt` — but NEVER rewind if a newer value is already recorded
/// (e.g. an out-of-band live turn ran ahead of the reconcile). Before the fix, a fresh
/// adopt's `last_user_message_at` stayed at `createdAt`, leaving the session looking
/// "old" against more recent activity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_bumps_last_user_message_at_to_imported_user_prompt() {
    use crate::engine::native_transcript;

    let work = tmp();
    let e = engine_from(&work).await;
    let id = "sess-recon-luma";

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
                claude_session_id: Some(Some("csidL".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // A user turn at a known ISO timestamp. The native_transcript test asserts
    // 2026-07-09T00:00:00Z == 1783555200000 ms; we assert the same here so a future
    // parser change fails loudly in both tests.
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidL");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
            &tp,
            r#"{"type":"user","timestamp":"2026-07-09T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
        )
        .unwrap();

    let expected_ms: i64 = native_transcript::iso_to_ms("2026-07-09T00:00:00Z");

    // Force the baseline low so the imported prompt's `at` is GUARANTEED to bump it.
    // (Without this, `Store::create` stamps `last_user_message_at = now_ms()` at
    // creation time, which is naturally newer than any past native user turn and
    // would never trigger the bump — masking the test of the recency-bump branch.)
    e.0.store
        .update(
            id,
            SessionPatch {
                last_user_message_at: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // First reconcile imports the user prompt and bumps last_user_message_at.
    let n = e.reconcile_from_native(id).await.unwrap();
    assert_eq!(n, 1);

    let s = e.get(id).await.unwrap();
    assert_eq!(
            s.last_user_message_at, expected_ms,
            "reconcile must bump last_user_message_at to the ISO-ts epoch-ms of the imported user prompt",
        );

    // A second no-op reconcile (no new native lines) must NOT rewind. We force a
    // high-water baseline FIRST so the no-op would naively zero it back out: bump to a
    // value higher than expected_ms, then reconcile again and assert no change.
    e.0.store
        .update(
            id,
            SessionPatch {
                last_user_message_at: Some(expected_ms + 10_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let n2 = e.reconcile_from_native(id).await.unwrap();
    assert_eq!(n2, 0, "second reconcile is a no-op with no delta appended");
    let s2 = e.get(id).await.unwrap();
    assert_eq!(
        s2.last_user_message_at,
        expected_ms + 10_000,
        "no-op reconcile must never rewind last_user_message_at below the current value",
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
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidX");
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
    assert_eq!(
            s.status, "done",
            "adopted row is an idle/finished resumable session (accepts follow-up, not re-enqueued on restart)"
        );
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

/// SECURITY (path traversal): `claudeSessionId` arrives raw from the HTTP body and is
/// interpolated into `native_transcript::transcript_path` -> `format!("{csid}.jsonl")`.
/// A relative traversal (`../../../../etc/passwd`) or an absolute path must be rejected
/// at the top of `adopt_session`, before any filesystem access or row creation — and must
/// leave no trace (no row keyed by that "csid" via `session_by_csid`).
#[tokio::test]
async fn adopt_rejects_path_traversal_and_absolute_csid() {
    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();

    let traversal = "../../../../etc/passwd";
    let result = e.adopt_session(traversal, &cwd_s).await;
    assert!(
        result.is_err(),
        "adopt_session must reject a path-traversal claudeSessionId"
    );
    assert!(
        e.0.store
            .session_by_csid(traversal)
            .await
            .unwrap()
            .is_none(),
        "no session row must be created for a rejected traversal csid"
    );

    let absolute = "/etc/passwd";
    let result2 = e.adopt_session(absolute, &cwd_s).await;
    assert!(
        result2.is_err(),
        "adopt_session must reject an absolute-path claudeSessionId"
    );
    assert!(
        e.0.store.session_by_csid(absolute).await.unwrap().is_none(),
        "no session row must be created for a rejected absolute-path csid"
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
    assert!(
        e.known_claude_session_ids().await.is_empty(),
        "fresh store has no linked csids"
    );

    // Row A: linked to a csid.
    e.0.store
        .create(CreateInput {
            id: "a".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    e.0.store
        .update(
            "a",
            SessionPatch {
                claude_session_id: Some(Some("csidA".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Row B: no csid.
    e.0.store
        .create(CreateInput {
            id: "b".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let known = e.known_claude_session_ids().await;
    assert!(
        known.contains("csidA"),
        "linked csid must be present, got {known:?}"
    );
    assert_eq!(
        known.len(),
        1,
        "rows without a claudeSessionId are excluded, got {known:?}"
    );
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
        e.0.store.session_by_csid("csidY").await.unwrap().is_none(),
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
    assert_eq!(
        n, 2,
        "delta reconcile pulls the terminal assistant + result"
    );
    e.0.store.set_detached(&id, false).await.unwrap();
}

/// `read_lines` returns an empty vec when the native transcript file is missing or
/// unreadable. Detach must not trust that empty count at face value — naively setting
/// the watermark to 0 would make a later reopen re-translate the ENTIRE native history
/// back into #1, duplicating every line already imported at adopt time. The frozen
/// watermark must never regress below what was already imported.
#[tokio::test]
async fn detach_never_lowers_watermark_when_transcript_file_missing() {
    use crate::engine::native_transcript;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidM");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
            &tp,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();

    let id = e.adopt_session("csidM", &cwd_s).await.unwrap();
    let before = e.0.store.get(&id).await.unwrap().unwrap();
    assert!(
        before.native_watermark_lines > 0,
        "sanity: adopt seeded a nonzero watermark, got {}",
        before.native_watermark_lines
    );

    // Simulate the transcript file going missing (moved/deleted) before detach.
    std::fs::remove_file(&tp).unwrap();

    let _ = e.detach_session(&id).await.unwrap();
    let after = e.0.store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        after.native_watermark_lines, before.native_watermark_lines,
        "watermark must not regress to 0 when the transcript file is missing at detach"
    );
}

/// `kill()` only *sends* SIGTERM (`run.stop()`) and returns immediately — it does not
/// wait for the pump task to actually exit. `detach_session` must await the engine's
/// real "not running" signal (`wait_for_exit`, which polls `state.running` — the same
/// map `is_busy`/`kill` consult) BEFORE reading the native line count, or an in-flight
/// turn's last transcript lines might not be flushed yet, freezing the watermark too
/// low. Drives a genuinely RUNNING streaming session into detach and asserts that, by
/// the time `detach_session` returns, the pump task has already exited — proving the
/// wait actually happened rather than detach racing ahead of the fire-and-forget kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_awaits_process_exit_before_reading_watermark() {
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
    wait_until(|| results.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;

    // Sanity: the streaming process is genuinely registered as running before detach.
    assert!(
        e.0.state.lock().running.contains_key(&id),
        "sanity: session must be running before detach"
    );

    let info = e.detach_session(&id).await.unwrap();
    assert_eq!(info.claude_session_id, "fake-stream-1");

    // No regression: detach must still succeed and set the flag on a session that was
    // genuinely running (not just idle) at the moment of detach.
    let s = e.0.store.get(&id).await.unwrap().unwrap();
    assert!(
        s.detached,
        "detach sets the single-writer guard flag even on a running session"
    );

    // By the time detach_session returns, the pump task must already be gone — proving
    // detach awaited real process exit rather than racing ahead of `kill`'s fire-and-forget
    // SIGTERM (before the fix, this could still observe the session as "running").
    assert!(
        !e.0.state.lock().running.contains_key(&id),
        "detach_session must not return while the killed session is still in the running map"
    );
}

// ── Fix 4: reclaim-on-reopen must not clear `detached` on reconcile failure ──
//
// The reclaim hook in `follow_up` used to clear `detached` unconditionally, even when
// `reconcile_from_native` failed — permanently losing whatever terminal-added delta
// failed to import (nothing would ever retry it, since the next reopen no longer sees
// `detached == true`). It must clear the flag ONLY on a successful reconcile; on
// failure it should log a warning and leave `detached == true` so the next reopen
// retries. The turn itself must still proceed either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_reclaim_leaves_detached_true_when_reconcile_fails() {
    use crate::engine::native_transcript;
    use crate::engine::store::SessionPatch;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidF");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();

    // Adopt (imports the one native line, so the log file already exists on disk).
    let id = e.adopt_session("csidF", &cwd_s).await.unwrap();

    // Simulate: the row already had a real completed turn (so follow_up's `is_busy` gate
    // — which treats a fresh "pending" row as busy — doesn't reject the follow-up), then
    // got detached, then the terminal appended a turn while detached.
    e.0.store
        .update(
            &id,
            SessionPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.0.store.set_detached(&id, true).await.unwrap();
    std::fs::OpenOptions::new().append(true).open(&tp).unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();

    // Force reconcile_from_native to fail: it calls store.append_log, which opens the
    // session's EXISTING log file with OpenOptions::append(true) — strip the write bit
    // from the log file itself (directory perms don't gate writes to an existing file;
    // only creating/renaming/unlinking entries needs directory write access) so that
    // open fails with a real permission-denied IO error.
    let log_path = e.0.store.log_path(&id);
    assert!(
        log_path.is_file(),
        "sanity: adopt must have already created the log file"
    );
    let orig_perms = std::fs::metadata(&log_path).unwrap().permissions();
    std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o400)).unwrap();

    let result = e
        .follow_up(&id, "reopen after terminal turn", false, None, None, None)
        .await;

    // Restore permissions immediately so cleanup and later assertions aren't themselves
    // tripped up by the locked-down file.
    std::fs::set_permissions(&log_path, orig_perms).unwrap();

    assert!(
        result.is_ok(),
        "the follow-up turn itself must still proceed even though reconcile failed, got {result:?}"
    );

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    assert!(
        s.detached,
        "detached must stay true when reconcile_from_native failed, so the next reopen retries"
    );

    // Clean up the now-queued/running turn so the test process doesn't leak a subprocess.
    e.kill(&id).await;
}

/// FIX 5 — the reclaim-on-reopen hook has no test through the REAL `follow_up` path.
/// This drives a genuine detached, adopted session through `follow_up` (the same entry
/// point the API's `POST /api/sessions/:id/follow-up` route calls) and asserts BOTH
/// halves of the reclaim contract: the terminal-added native lines get reconciled into
/// #1 (the rendered log grows with the reconciled content) AND `detached` is cleared —
/// using the crate's real engine + fake-sdk-bridge harness the same way the other
/// follow_up/turn tests do (see `streaming_one_process_goes_idle_then_followup_injects_over_stdin`
/// above). Also doubles as the happy-path mirror of the reconcile-failure test above,
/// guarding against a fix that's overly conservative (e.g. never clearing `detached`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_reclaim_reconciles_native_delta_and_clears_detached() {
    use crate::engine::native_transcript;
    use crate::engine::store::SessionPatch;
    use std::io::Write;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidG");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();

    // Adopt, then simulate: a real completed turn (so `is_busy` doesn't reject the
    // follow-up on a fresh "pending" row) → detach → the terminal appends a turn while
    // agentic-dev is detached, exactly the "handed off, terminal-added, then reopened"
    // scenario the reclaim hook exists for.
    let id = e.adopt_session("csidG", &cwd_s).await.unwrap();
    e.0.store
        .update(
            &id,
            SessionPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.0.store.set_detached(&id, true).await.unwrap();
    std::fs::OpenOptions::new().append(true).open(&tp).unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();

    let log_before = e.0.store.read_log(&id);

    // Drive the real reopen through follow_up (not a direct reconcile_from_native call).
    let result = e
        .follow_up(&id, "reopen after terminal turn", false, None, None, None)
        .await;
    assert!(result.is_ok(), "follow-up must succeed, got {result:?}");

    // Half 1: the terminal-added native lines were reconciled into #1 — the log grew
    // and now carries the reconciled assistant turn (end_turn → assistant + result).
    let log_after = e.0.store.read_log(&id);
    assert!(
        log_after.len() >= log_before.len() + 2,
        "reconcile must append the terminal assistant+result lines to #1: before={}, after={}",
        log_before.len(),
        log_after.len()
    );
    assert!(
        log_after.iter().any(|l| l.contains("\"back\"")),
        "reconciled log must carry the terminal-added assistant text, got: {log_after:?}"
    );

    // Half 2: detached was cleared — agentic-dev reclaimed ownership.
    let s = e.0.store.get(&id).await.unwrap().unwrap();
    assert!(
        !s.detached,
        "detached must clear once reconcile_from_native succeeds"
    );

    // Clean up the now-queued/running turn so the test process doesn't leak a subprocess.
    e.kill(&id).await;
}

/// FIX 2 (DATA LOSS): adopt is in-place, so `worktree_path` is the user's REAL project cwd,
/// which lives OUTSIDE the managed `worktrees_root`. The delete path must NOT `remove_dir_all`
/// that directory — doing so would wipe the user's actual project. Adopt an in-place session
/// whose cwd is outside `worktrees_root`, delete it, and assert the cwd (and a file in it)
/// still exist on disk.
#[tokio::test]
async fn delete_does_not_remove_adopted_in_place_cwd_outside_worktrees_root() {
    use crate::engine::native_transcript;

    let work = tmp();
    let e = engine_from(&work).await;

    // Adopt-in-place: cwd is the user's real project dir, OUTSIDE worktrees_root.
    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    std::fs::write(cwd.join("important.txt"), "user data").unwrap();
    assert!(
        !cwd.starts_with(&e.0.cfg.worktrees_root),
        "sanity: adopted cwd must be outside worktrees_root"
    );

    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidDEL");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let id = e.adopt_session("csidDEL", &cwd_s).await.unwrap();

    // Delete the session — the out-of-root cwd removal must be SKIPPED.
    e.delete_session(&id, false).await.unwrap();

    assert!(
        cwd.exists(),
        "adopt-in-place cwd (user's real project dir) must NOT be deleted"
    );
    assert!(
        cwd.join("important.txt").exists(),
        "user's files under the adopted cwd must survive delete"
    );
    assert!(
        e.0.store.get(&id).await.unwrap().is_none(),
        "the session row itself is removed"
    );
}

/// FIX 3 (atomic adopt): a PARTIAL unique index on `claudeSessionId` makes a concurrent
/// double-adopt of the same csid fail the second csid-setting UPDATE with a constraint error,
/// which adopt's existing rollback cleans up. Two concurrent `adopt_session` for the same csid
/// → exactly one Ok + one Err, and exactly one row carries the csid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_double_adopt_same_csid_exactly_one_succeeds() {
    use crate::engine::native_transcript;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidRACE");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();

    let (r1, r2) = tokio::join!(
        e.adopt_session("csidRACE", &cwd_s),
        e.adopt_session("csidRACE", &cwd_s),
    );

    let oks = (r1.is_ok() as usize) + (r2.is_ok() as usize);
    assert_eq!(
        oks, 1,
        "exactly one concurrent adopt of the same csid may succeed: r1={r1:?} r2={r2:?}"
    );

    assert!(
        e.0.store
            .session_by_csid("csidRACE")
            .await
            .unwrap()
            .is_some(),
        "the winning row is findable by csid"
    );
    let rows = e.0.store.list().await.unwrap();
    let with_csid = rows
        .iter()
        .filter(|s| s.claude_session_id.as_deref() == Some("csidRACE"))
        .count();
    assert_eq!(
        with_csid, 1,
        "exactly one row may carry the csid (partial unique index)"
    );
}

/// FIX 4 (shell-escape): `detach_session`'s `resume_cmd` interpolates `cwd` into a shell
/// command; a cwd with a space (or metacharacters) would break/inject when pasted. It must be
/// single-quoted. Detach a session whose cwd contains a space and assert the path is quoted.
#[tokio::test]
async fn detach_shell_quotes_cwd_with_spaces() {
    use crate::engine::native_transcript;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("my project dir");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidSP");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let id = e.adopt_session("csidSP", &cwd_s).await.unwrap();

    let info = e.detach_session(&id).await.unwrap();
    assert_eq!(
        info.resume_cmd,
        format!("cd '{cwd_s}' && claude --resume csidSP"),
        "cwd with spaces must be single-quoted so the pasted command is safe"
    );
}

/// FIX 5 (keep detached when transcript missing): `reconcile_from_native` returns Ok(0) when
/// the native transcript file is ABSENT. The reclaim hook must NOT treat that as "reclaimed" —
/// a transiently-missing file must be retried on the next reopen, so `detached` stays true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_reclaim_leaves_detached_true_when_transcript_missing() {
    use crate::engine::native_transcript;
    use crate::engine::store::SessionPatch;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidMISS");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let id = e.adopt_session("csidMISS", &cwd_s).await.unwrap();
    e.0.store
        .update(
            &id,
            SessionPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.0.store.set_detached(&id, true).await.unwrap();

    // Transcript goes missing (moved / not yet synced) before reopen.
    std::fs::remove_file(&tp).unwrap();

    let result = e.follow_up(&id, "reopen", false, None, None, None).await;
    assert!(
        result.is_ok(),
        "the follow-up turn must still proceed, got {result:?}"
    );

    let s = e.0.store.get(&id).await.unwrap().unwrap();
    assert!(
        s.detached,
        "detached must stay true when the transcript file is absent, so the next reopen retries"
    );

    e.kill(&id).await;
}

/// FIX 6 (cursor before reclaim): the reclaim reconcile appends the terminal-added turns to #1
/// BEFORE `follow_up` returns; the returned stream cursor must PRECEDE those lines or the client
/// skips the reclaimed terminal turns. Assert `follow_up` returns the pre-reclaim log length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followup_reclaim_cursor_precedes_reconciled_lines() {
    use crate::engine::native_transcript;
    use crate::engine::store::SessionPatch;
    use std::io::Write;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidCUR");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let id = e.adopt_session("csidCUR", &cwd_s).await.unwrap();
    e.0.store
        .update(
            &id,
            SessionPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    e.0.store.set_detached(&id, true).await.unwrap();
    std::fs::OpenOptions::new().append(true).open(&tp).unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();

    let log_before = e.0.store.read_log(&id);
    let since = e
        .follow_up(&id, "reopen", false, None, None, None)
        .await
        .unwrap();

    assert_eq!(
            since,
            log_before.len() as i64,
            "follow_up must return the PRE-reclaim log cursor so the client stream re-includes the reconciled terminal turns"
        );
    let log_after = e.0.store.read_log(&id);
    assert!(
        (log_after.len() as i64) > since,
        "reclaim must have appended lines AFTER the returned cursor: since={since}, after={}",
        log_after.len()
    );

    e.kill(&id).await;
}

/// FIX 7 (no watermark advance on repeated detach): a second detach (double-click/retry) while
/// the row is ALREADY detached must NOT recompute the watermark — terminal lines added since the
/// first handoff would otherwise be marked already-imported and permanently skipped. Detach
/// twice with a terminal line appended in between; the stored watermark must stay at the
/// first-detach value.
#[tokio::test]
async fn second_detach_does_not_advance_watermark() {
    use crate::engine::native_transcript;
    use std::io::Write;

    let work = tmp();
    let e = engine_from(&work).await;

    let cwd = work.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let tp = native_transcript::transcript_path(&e.0.cfg.claude_config_base, &cwd_s, "csidDBL");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let id = e.adopt_session("csidDBL", &cwd_s).await.unwrap();

    // First detach freezes the watermark at the current native line count (1).
    let _ = e.detach_session(&id).await.unwrap();
    let first =
        e.0.store
            .get(&id)
            .await
            .unwrap()
            .unwrap()
            .native_watermark_lines;
    assert_eq!(
        first, 1,
        "sanity: first detach freezes the watermark at the native line count"
    );

    // A terminal turn is appended AFTER the first handoff.
    std::fs::OpenOptions::new().append(true).open(&tp).unwrap()
            .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();

    // Second detach while ALREADY detached must not advance the watermark.
    let info = e.detach_session(&id).await.unwrap();
    assert!(
        info.resume_cmd.contains("claude --resume csidDBL"),
        "a repeat detach still returns the resume command"
    );
    let second =
        e.0.store
            .get(&id)
            .await
            .unwrap()
            .unwrap()
            .native_watermark_lines;
    assert_eq!(
            second, first,
            "a repeat detach must leave the first-detach watermark untouched (else the interim terminal line is skipped)"
        );
}
