use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AO};

static CTR: AtomicU64 = AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = CTR.fetch_add(1, AO::SeqCst);
    let p = std::env::temp_dir().join(format!("agentic-store-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn create_get_roundtrip_and_lastusermessageat_eq_createdat() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let s = store
        .create(CreateInput {
            id: "s1".into(),
            repos: vec!["demo".into()],
            prompt: "do x".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.last_user_message_at, s.created_at);
    let got = store.get("s1").await.unwrap().unwrap();
    assert_eq!(got.id, "s1");
    assert_eq!(got.repos, vec!["demo".to_string()]);
    assert_eq!(got.status, "pending");
}

#[tokio::test]
async fn list_orders_by_last_user_message_at_desc() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let a = store
        .create(CreateInput {
            id: "a".into(),
            prompt: "a".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "b".into(),
            prompt: "b".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // default: newest-created first (b before a)
    assert_eq!(
        store
            .list()
            .await
            .unwrap()
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        vec!["b", "a"]
    );
    // bump a past b
    store
        .update(
            "a",
            SessionPatch {
                last_user_message_at: Some(a.created_at + 10_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .list()
            .await
            .unwrap()
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
}

#[tokio::test]
async fn opens_a_legacy_db_and_backfills_last_user_message_at() {
    // Simulate a DB created before lastUserMessageAt was added: sessions table without that column.
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, createdAt INTEGER, seq INTEGER)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq) VALUES ('old','done','p',12345,0)").execute(&pool).await.unwrap();
        pool.close().await;
    }
    let store = Store::open(path, dir.join("logs")).await.unwrap();
    let s = store.get("old").await.unwrap().unwrap();
    assert_eq!(s.created_at, 12345);
    assert_eq!(s.last_user_message_at, 12345); // backfilled
}

#[tokio::test]
async fn migrates_legacy_db_missing_all_added_columns_and_reopen_is_idempotent() {
    // A legacy DB with only the ORIGINAL base columns (none of ADDED_COLUMNS).
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, repo TEXT, prompt TEXT, worktreePath TEXT, \
                 branch TEXT, claudeSessionId TEXT, status TEXT, costUsd REAL, exitCode INTEGER, \
                 error TEXT, createdAt INTEGER, startedAt INTEGER, endedAt INTEGER, seq INTEGER)"
            ).execute(&pool).await.unwrap();
        pool.close().await;
    }
    // First open migrates: all 10 ADDED_COLUMNS get ALTER-added. A full create() round-trip
    // proves repos/skills/baseShas/model/effort/mode/worktreeState columns now exist.
    {
        let store = Store::open(path.clone(), dir.join("logs")).await.unwrap();
        store
            .create(CreateInput {
                id: "s1".into(),
                prompt: "p".into(),
                repos: vec!["demo".into()],
                model: Some("opus".into()),
                mode: Some("ultra".into()),
                base_shas: std::collections::HashMap::from([("demo".into(), Some("abc".into()))]),
                ..Default::default()
            })
            .await
            .unwrap();
        let s = store.get("s1").await.unwrap().unwrap();
        assert_eq!(s.repos, vec!["demo".to_string()]);
        assert_eq!(s.model.as_deref(), Some("opus"));
        assert_eq!(s.base_shas.get("demo"), Some(&Some("abc".to_string())));
    }
    // Re-open the SAME path: migration must be idempotent (re-running the ALTERs is a no-op,
    // not an error) and prior rows survive; a new create still works.
    let store2 = Store::open(path, dir.join("logs")).await.unwrap();
    assert!(
        store2.get("s1").await.unwrap().is_some(),
        "row must survive reopen"
    );
    store2
        .create(CreateInput {
            id: "s2".into(),
            prompt: "q".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(store2.get("s2").await.unwrap().is_some());
}

#[tokio::test]
async fn append_log_writes_one_line_per_call() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store.append_log("s1", "{\"type\":\"x\"}").await.unwrap();
    store.append_log("s1", "{\"type\":\"y\"}").await.unwrap();
    let content = std::fs::read_to_string(store.log_path("s1")).unwrap();
    assert_eq!(content, "{\"type\":\"x\"}\n{\"type\":\"y\"}\n");
}

/// Fix #5: `create()` stores raw mode; `get()` normalizes on read.
#[tokio::test]
async fn create_stores_raw_mode_get_normalizes() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    // "ultra" is stored raw in the returned Session from create(); but the
    // Session returned from create() has mode = raw (no normalization at write time).
    let created = store
        .create(CreateInput {
            id: "m1".into(),
            prompt: "p".into(),
            mode: Some("ultra".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    // create() returns the raw mode.
    assert_eq!(
        created.mode.as_deref(),
        Some("ultra"),
        "create() must return raw mode"
    );
    // get() normalizes on read.
    let got = store.get("m1").await.unwrap().unwrap();
    assert_eq!(
        got.mode.as_deref(),
        Some("ultracode"),
        "get() must normalize mode on read"
    );
}

/// Fix #3/#6: a row with baseShas column NULL but repo+baseSha set reconstructs
/// base_shas = {repo: baseSha} and base_sha = baseSha.
#[tokio::test]
async fn row_to_session_base_shas_fallback_from_repo_and_base_sha() {
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        // Create minimal table (no baseShas column yet, simulating legacy row).
        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, baseSha TEXT, repos TEXT, \
                worktreeState TEXT, lastUserMessageAt INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,baseSha,lastUserMessageAt) \
                VALUES ('r1','pending','p',1000,0,'myrepo','abc123',1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }
    let store = Store::open(path, dir.join("logs")).await.unwrap();
    let s = store.get("r1").await.unwrap().unwrap();
    // baseShas fallback: {repo: baseSha}
    assert_eq!(
        s.base_shas.get("myrepo").and_then(|v| v.as_deref()),
        Some("abc123"),
        "base_shas must be reconstructed from repo+baseSha when baseShas column is null"
    );
    // base_sha fallback: baseSha column value
    assert_eq!(
        s.base_sha.as_deref(),
        Some("abc123"),
        "base_sha must be set from baseSha column"
    );
}

/// Fix #4: repos fallback — empty-string repo → [] not [""]
#[tokio::test]
async fn repos_fallback_empty_repo_yields_empty_vec() {
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, lastUserMessageAt INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // repo = "" (empty string, treated as absent)
        sqlx::query(
            "INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,lastUserMessageAt) \
                VALUES ('r2','pending','p',1000,0,'',1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }
    let store = Store::open(path, dir.join("logs")).await.unwrap();
    let s = store.get("r2").await.unwrap().unwrap();
    assert_eq!(
        s.repos,
        Vec::<String>::new(),
        "empty-string repo must yield empty repos vec (empty string is treated as absent)"
    );
}

/// Multi-repo create round-trip: create({repos:['A','B'], baseShas:{A:'sa',B:'sb'}}) must
/// persist baseSha = baseShas['A'] (repos[0]) and base_shas round-trips intact.
/// Multi-repo create round-trip: stores repos/skills/baseShas and backfills old single-repo rows.
#[tokio::test]
async fn create_multi_repo_baseshas_roundtrip() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let mut base_shas = std::collections::HashMap::new();
    base_shas.insert("A".to_string(), Some("sa".to_string()));
    base_shas.insert("B".to_string(), Some("sb".to_string()));
    let created = store
        .create(CreateInput {
            id: "multi1".into(),
            prompt: "p".into(),
            repos: vec!["A".into(), "B".into()],
            base_shas: base_shas.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    // create() must derive base_sha = baseShas[repos[0]] = baseShas["A"] = "sa"
    assert_eq!(
        created.base_sha.as_deref(),
        Some("sa"),
        "create() must set base_sha = baseShas[repos[0]]"
    );
    assert_eq!(
        created.repo.as_str(),
        "A",
        "create() must set repo = repos[0]"
    );
    assert_eq!(
        created.base_shas.get("A").and_then(|v| v.as_deref()),
        Some("sa"),
        "create() base_shas must contain A=sa"
    );
    assert_eq!(
        created.base_shas.get("B").and_then(|v| v.as_deref()),
        Some("sb"),
        "create() base_shas must contain B=sb"
    );

    // get() must return the same values after persisting.
    let got = store.get("multi1").await.unwrap().unwrap();
    assert_eq!(
        got.base_sha.as_deref(),
        Some("sa"),
        "get() must return persisted base_sha = baseShas[repos[0]]"
    );
    assert_eq!(got.repo.as_str(), "A", "get() must return repo = repos[0]");
    assert_eq!(
        got.repos,
        vec!["A".to_string(), "B".to_string()],
        "get() must return repos round-tripped"
    );
    assert_eq!(
        got.base_shas.get("A").and_then(|v| v.as_deref()),
        Some("sa"),
        "get() base_shas must contain A=sa"
    );
    assert_eq!(
        got.base_shas.get("B").and_then(|v| v.as_deref()),
        Some("sb"),
        "get() base_shas must contain B=sb"
    );
}

/// Legacy single-repo create: create({repo:'r', baseSha:'x'}) must persist
/// baseShas = {"r":"x"} so get() returns base_shas = {r:'x'}.
#[tokio::test]
async fn create_legacy_single_repo_baseshas_roundtrip() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let created = store
        .create(CreateInput {
            id: "legacy1".into(),
            prompt: "p".into(),
            repo: Some("r".into()),
            base_sha: Some("x".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    // create() must synthesize base_shas = {r: x} from legacy form
    assert_eq!(
        created.base_shas.get("r").and_then(|v| v.as_deref()),
        Some("x"),
        "create() must synthesize base_shas = {{repo: baseSha}} from legacy form"
    );
    assert_eq!(
        created.base_sha.as_deref(),
        Some("x"),
        "create() must set base_sha = baseShas[repo] = x"
    );
    assert_eq!(
        created.repo.as_str(),
        "r",
        "create() must set repo = repos[0]"
    );

    // get() must round-trip correctly (not return {} for base_shas)
    let got = store.get("legacy1").await.unwrap().unwrap();
    assert_eq!(
        got.base_shas.get("r").and_then(|v| v.as_deref()),
        Some("x"),
        "get() must return base_shas = {{r: x}} for legacy single-repo create"
    );
    assert_eq!(
        got.base_sha.as_deref(),
        Some("x"),
        "get() must return base_sha = x"
    );
}

/// Corrupt JSON degradation: repos/skills/baseShas with invalid JSON must degrade to
/// [],[],{} respectively instead of panicking. Guards the recover() boot-loop invariant.
/// Corrupt JSON degradation: repos/skills/baseShas with invalid JSON must degrade to empty.
#[tokio::test]
async fn corrupt_json_columns_degrade_gracefully() {
    let dir = tmp();
    let path = dir.join("db.sqlite");
    // Open Store first so it creates + migrates the table (including lastUserMessageAt).
    let store = Store::open(path.clone(), dir.join("logs")).await.unwrap();
    // Now insert rows with corrupt JSON via a raw pool on the already-migrated DB.
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        // Insert a row with invalid JSON in repos, skills, baseShas columns.
        sqlx::query(
                "INSERT INTO sessions (id,repo,prompt,status,createdAt,lastUserMessageAt,worktreeState,repos,skills,baseShas,seq) \
                 VALUES ('corrupt1','','p','pending',1000,1000,'live','NOT JSON','[broken','{bad:json}',0)"
            ).execute(&pool).await.unwrap();
        // Also insert a second row to verify list() doesn't panic either.
        sqlx::query(
                "INSERT INTO sessions (id,repo,prompt,status,createdAt,lastUserMessageAt,worktreeState,repos,skills,baseShas,seq) \
                 VALUES ('corrupt2','','p','pending',1001,1001,'live',NULL,NULL,NULL,1)"
            ).execute(&pool).await.unwrap();
        pool.close().await;
    }
    // get() must not panic and must return degraded values.
    let s = store.get("corrupt1").await.unwrap().unwrap();
    assert_eq!(
        s.repos,
        Vec::<String>::new(),
        "corrupt repos must degrade to []"
    );
    assert_eq!(
        s.skills,
        Vec::<String>::new(),
        "corrupt skills must degrade to []"
    );
    assert!(
        s.base_shas.is_empty(),
        "corrupt baseShas must degrade to {{}}"
    );
    // list() must not panic and must return all rows.
    let list = store.list().await.unwrap();
    assert_eq!(
        list.len(),
        2,
        "list() must return all rows even with corrupt JSON"
    );
}

#[tokio::test]
async fn remove_deletes_row_and_log() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "rm1".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.append_log("rm1", "{\"type\":\"x\"}").await.unwrap();
    assert!(store.get("rm1").await.unwrap().is_some());
    store.remove("rm1").await.unwrap();
    assert!(store.get("rm1").await.unwrap().is_none(), "row deleted");
    assert!(
        store.read_log("rm1").is_empty(),
        "log read returns [] after remove"
    );
    // idempotent
    store.remove("rm1").await.unwrap();
}

#[tokio::test]
async fn read_log_returns_nonempty_lines_in_order() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    assert_eq!(
        store.read_log("missing"),
        Vec::<String>::new(),
        "missing log → []"
    );
    store.append_log("L", "{\"type\":\"a\"}").await.unwrap();
    store.append_log("L", "{\"type\":\"b\"}").await.unwrap();
    assert_eq!(
        store.read_log("L"),
        vec![
            "{\"type\":\"a\"}".to_string(),
            "{\"type\":\"b\"}".to_string()
        ]
    );
}

#[tokio::test]
async fn update_can_set_worktree_state() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "ws".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update(
            "ws",
            SessionPatch {
                worktree_state: Some("discarded".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get("ws").await.unwrap().unwrap().worktree_state,
        "discarded"
    );
}

#[test]
fn session_runtime_fields_skip_when_none() {
    let s = Session {
        id: "x".into(),
        status: "done".into(),
        worktree_state: "live".into(),
        ..Default::default()
    };
    let v = serde_json::to_value(&s).unwrap();
    assert!(
        v.get("activity").is_none()
            && v.get("awaitingInput").is_none()
            && v.get("workflowRunning").is_none(),
        "runtime fields omitted when None (optional props)"
    );
}

/// Session.repo is a non-optional String: create() with no repo yields repo="", not null.
#[tokio::test]
async fn session_repo_is_non_optional_string() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    // No repo: must serialize as "" not null.
    let s = store
        .create(CreateInput {
            id: "norepo".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        s.repo, "",
        "Session.repo must be empty string (not null) when no repo"
    );

    // With repo: must serialize as the repo string.
    let s2 = store
        .create(CreateInput {
            id: "withrepo".into(),
            prompt: "p".into(),
            repos: vec!["demo".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s2.repo, "demo", "Session.repo must equal repos[0]");

    // get() must round-trip the same values.
    let got = store.get("norepo").await.unwrap().unwrap();
    assert_eq!(
        got.repo, "",
        "get() must return repo='' for no-repo session"
    );
    let got2 = store.get("withrepo").await.unwrap().unwrap();
    assert_eq!(got2.repo, "demo", "get() must return repo='demo'");
}

/// update() must touch ONLY the Some() columns and leave every other column intact,
/// including the ability to write SQL NULL via Some(None) (the Option<Option<T>> nullify path).
#[tokio::test]
async fn update_only_some_fields_others_untouched_and_nullify() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "u1".into(),
            prompt: "orig".into(),
            repos: vec!["demo".into()],
            model: Some("opus".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    // Set several optional fields to concrete values in one update.
    store
        .update(
            "u1",
            SessionPatch {
                status: Some("running".into()),
                cost_usd: Some(Some(1.5)),
                exit_code: Some(Some(0)),
                error: Some(Some("boom".into())),
                started_at: Some(Some(7777)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let s = store.get("u1").await.unwrap().unwrap();
    assert_eq!(s.status, "running");
    assert_eq!(s.cost_usd, Some(1.5));
    assert_eq!(s.exit_code, Some(0));
    assert_eq!(s.error.as_deref(), Some("boom"));
    assert_eq!(s.started_at, Some(7777));
    // Untouched columns retain their create-time values.
    assert_eq!(s.prompt, "orig", "prompt untouched by status/cost update");
    assert_eq!(s.model.as_deref(), Some("opus"), "model untouched");
    assert_eq!(s.repos, vec!["demo".to_string()], "repos untouched");

    // Now a partial update of only `prompt` must leave status/cost/error from before intact,
    // and Some(None) on error/cost must write SQL NULL (clearing previously-set values).
    store
        .update(
            "u1",
            SessionPatch {
                prompt: Some("changed".into()),
                error: Some(None),    // nullify
                cost_usd: Some(None), // nullify
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let s2 = store.get("u1").await.unwrap().unwrap();
    assert_eq!(s2.prompt, "changed");
    assert_eq!(s2.error, None, "Some(None) must clear error to SQL NULL");
    assert_eq!(
        s2.cost_usd, None,
        "Some(None) must clear costUsd to SQL NULL"
    );
    // Fields not in the second patch keep their first-update values.
    assert_eq!(
        s2.status, "running",
        "status must survive a prompt-only update"
    );
    assert_eq!(
        s2.exit_code,
        Some(0),
        "exitCode must survive a prompt-only update"
    );
    assert_eq!(
        s2.started_at,
        Some(7777),
        "startedAt must survive a prompt-only update"
    );
}

/// An empty SessionPatch (all None) must be a no-op: it must not error and must not
/// alter any column. Guards the `if sets.is_empty() { return Ok(()) }` early-return.
#[tokio::test]
async fn update_empty_patch_is_noop() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "noop".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let before = store.get("noop").await.unwrap().unwrap();
    store.update("noop", SessionPatch::default()).await.unwrap();
    let after = store.get("noop").await.unwrap().unwrap();
    assert_eq!(after.prompt, before.prompt);
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_user_message_at, before.last_user_message_at);
    // update() of a non-existent id is also a silent no-op (UPDATE matches 0 rows).
    store
        .update(
            "ghost",
            SessionPatch {
                status: Some("x".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(store.get("ghost").await.unwrap().is_none());
}

/// list() ordering tiebreak: when lastUserMessageAt is equal across rows, ordering falls
/// back to `seq DESC` — i.e. the most-recently-created row sorts first. seq is monotonic.
#[tokio::test]
async fn list_tiebreaks_equal_timestamps_by_seq_desc() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    // Create three rows, then force identical lastUserMessageAt so only seq distinguishes them.
    for id in ["t0", "t1", "t2"] {
        store
            .create(CreateInput {
                id: id.into(),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update(
                id,
                SessionPatch {
                    last_user_message_at: Some(5000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    // Equal timestamps → seq DESC → newest-created (t2) first, oldest (t0) last.
    let ids: Vec<String> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(
        ids,
        vec!["t2", "t1", "t0"],
        "equal lastUserMessageAt must tiebreak by seq DESC (newest create first)"
    );
}

/// read_log must skip blank / whitespace-only lines (not just be empty on a missing file),
/// returning only the meaningful JSONL lines in order.
#[tokio::test]
async fn read_log_skips_blank_and_whitespace_lines() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    // Write a file directly containing blank lines and whitespace-only lines between entries.
    std::fs::write(
        store.log_path("blanks"),
        "{\"a\":1}\n\n   \n{\"b\":2}\n\t\n",
    )
    .unwrap();
    assert_eq!(
        store.read_log("blanks"),
        vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()],
        "blank and whitespace-only lines must be filtered out"
    );
    // An empty file (created by append never called, or truncated) reads as [].
    std::fs::write(store.log_path("empty"), "").unwrap();
    assert_eq!(
        store.read_log("empty"),
        Vec::<String>::new(),
        "empty file → []"
    );
}

/// normalize_mode edges (read-side): only "ultra"/"ultracode" map to "ultracode"; any other
/// stored value (including a bogus mode or NULL) reads back as None.
#[tokio::test]
async fn mode_normalization_edges_unknown_becomes_none() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "mw".into(),
            prompt: "p".into(),
            mode: Some("weird".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        store.get("mw").await.unwrap().unwrap().mode,
        None,
        "unknown stored mode must normalize to None on read"
    );
    store
        .create(CreateInput {
            id: "muc".into(),
            prompt: "p".into(),
            mode: Some("ultracode".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        store.get("muc").await.unwrap().unwrap().mode.as_deref(),
        Some("ultracode"),
        "already-normalized 'ultracode' stays 'ultracode'"
    );
    store
        .create(CreateInput {
            id: "mnone".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        store.get("mnone").await.unwrap().unwrap().mode,
        None,
        "absent mode reads back as None"
    );
}

/// Concurrent create() on a shared Arc<Store>: spawned tasks racing on the single-connection
/// pool + AtomicI64 seq must all succeed, produce distinct rows, and yield strictly distinct
/// monotonic seq values (so the list() tiebreak stays well-defined). No panics / lost rows.
#[tokio::test]
async fn concurrent_create_all_rows_persist_with_distinct_seq() {
    let dir = tmp();
    let store = std::sync::Arc::new(
        Store::open(dir.join("db.sqlite"), dir.join("logs"))
            .await
            .unwrap(),
    );
    let mut handles = Vec::new();
    for i in 0..16 {
        let st = store.clone();
        handles.push(tokio::spawn(async move {
            st.create(CreateInput {
                id: format!("c{i}"),
                prompt: "p".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    // All 16 rows present, ids unique.
    let list = store.list().await.unwrap();
    assert_eq!(list.len(), 16, "all concurrent creates must persist");
    let mut ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 16, "no duplicate / lost rows under concurrency");
    // seq values must be strictly distinct (AtomicI64 fetch_add guarantees no collisions),
    // covering the full contiguous 0..16 range.
    let mut seqs: Vec<i64> = sqlx::query("SELECT seq FROM sessions")
        .fetch_all(&store.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<i64, _>("seq"))
        .collect();
    seqs.sort();
    assert_eq!(
        seqs,
        (0..16).collect::<Vec<i64>>(),
        "16 concurrent creates must consume a distinct contiguous seq range 0..16"
    );
}

/// Adopted session round-trip: `origin` is persisted at create time,
/// `nativeWatermarkLines` and `detached` are updated via the dedicated
/// setters and read back through `get()`.
#[tokio::test]
async fn adopted_origin_and_watermark_round_trip() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let id = "sess-adopt-1";
    store
        .create(CreateInput {
            id: id.into(),
            prompt: "p".into(),
            origin: Some("adopted".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = store.get(id).await.unwrap().unwrap();
    assert_eq!(s.origin, "adopted");
    assert_eq!(s.detached, false);
    assert_eq!(s.native_watermark_lines, 0);

    store.set_watermark(id, 42).await.unwrap();
    store.set_detached(id, true).await.unwrap();
    let s2 = store.get(id).await.unwrap().unwrap();
    assert_eq!(s2.native_watermark_lines, 42);
    assert_eq!(s2.detached, true);

    store.set_detached(id, false).await.unwrap();
    let s3 = store.get(id).await.unwrap().unwrap();
    assert_eq!(s3.detached, false);
}

/// `ensure_group("Claude Code Adopted")` must return the stable id
/// `grp-adopted` on the first AND every subsequent call (idempotent),
/// and the table must end up with exactly ONE row for that name even
/// when concurrent calls race the INSERT. Non-adopted names get a
/// fresh `grp-<uuid>` id.
#[tokio::test]
async fn ensure_group_is_idempotent() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    // Adopted name — stable id, idempotent.
    let a = store.ensure_group("Claude Code Adopted").await.unwrap();
    let b = store.ensure_group("Claude Code Adopted").await.unwrap();
    assert_eq!(a, b, "two calls for the same name must return the same id");
    assert_eq!(
        a, "grp-adopted",
        "the 'Claude Code Adopted' group must use the stable id 'grp-adopted'"
    );
    assert_eq!(
        store
            .list_groups()
            .await
            .unwrap()
            .iter()
            .filter(|g| g.name == "Claude Code Adopted")
            .count(),
        1,
        "only one 'Claude Code Adopted' row must exist after repeat calls"
    );

    // Non-adopted name — fresh uuid-prefixed id, also idempotent.
    let c1 = store.ensure_group("Other Group").await.unwrap();
    let c2 = store.ensure_group("Other Group").await.unwrap();
    assert_eq!(c1, c2, "non-adopted name is also idempotent");
    assert!(
        c1.starts_with("grp-") && c1 != "grp-adopted",
        "non-adopted name must get a deterministic grp-<hash> id, got {c1}"
    );
}

/// Non-adopted names must get a stable id derived from the name (not a
/// fresh uuid each call), so two sequential calls return the SAME id and
/// `list_groups` shows exactly one row — this is what makes
/// concurrent inserters race-safe on PRIMARY KEY id instead of
/// accidentally producing duplicate `groups` rows.
#[tokio::test]
async fn ensure_group_same_name_is_stable_id() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let first = store.ensure_group("My Group").await.unwrap();
    let second = store.ensure_group("My Group").await.unwrap();
    assert_eq!(
        first, second,
        "two sequential calls for the same non-adopted name must return the same id"
    );
    assert!(
        first.starts_with("grp-") && first != "grp-adopted",
        "non-adopted name must get a deterministic grp-<hash> id, got {first}"
    );
    let rows: Vec<_> = store
        .list_groups()
        .await
        .unwrap()
        .into_iter()
        .filter(|g| g.name == "My Group")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one row must exist for the non-adopted name, got {} ({:?})",
        rows.len(),
        rows.iter().map(|g| &g.id).collect::<Vec<_>>()
    );
    assert_eq!(
        rows[0].id, first,
        "the single surviving row must carry the deterministic id"
    );
}

#[tokio::test]
async fn parent_session_id_round_trip() {
    let dir = tmp();
    let store = Store::open(dir.join("s.db"), dir.join("logs"))
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "parent".into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(CreateInput {
            id: "child".into(),
            prompt: "c".into(),
            parent_session_id: Some("parent".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    let c = store.get("child").await.unwrap().unwrap();
    assert_eq!(c.parent_session_id.as_deref(), Some("parent"));

    let p = store.get("parent").await.unwrap().unwrap();
    assert_eq!(p.parent_session_id, None);

    let kids = store.list_children("parent").await.unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, "child");
}

#[tokio::test]
async fn roundtrip_mcp_session_fields() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();
    let extra = vec![
        McpServerDef {
            name: "my-mcp".into(),
            command: Some("npx".into()),
            args: Some(vec!["my-server".into()]),
            ..Default::default()
        },
        McpServerDef {
            name: "web-mcp".into(),
            url: Some("https://example.com/mcp".into()),
            transport: Some("http".into()),
            ..Default::default()
        },
    ];
    let hidden = vec!["unwanted-mcp".into()];
    store
        .create(CreateInput {
            id: "mcp1".into(),
            prompt: "test mcp fields".into(),
            extra_mcp_servers: extra.clone(),
            hidden_mcp_servers: hidden.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    let got = store.get("mcp1").await.unwrap().unwrap();
    assert_eq!(got.extra_mcp_servers, extra);
    assert_eq!(got.hidden_mcp_servers, hidden);
}

#[tokio::test]
async fn forced_on_fields_round_trip_and_old_rows_default_empty() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs"))
        .await
        .unwrap();

    let plugins = vec!["superpowers@official".to_string()];
    let skills = vec!["rke2-ops".to_string()];
    let mcp = vec!["my-server".to_string()];

    let created = store
        .create(CreateInput {
            id: "s-forced".into(),
            repos: vec!["r".into()],
            prompt: "x".into(),
            forced_on_plugins: plugins.clone(),
            forced_on_skills: skills.clone(),
            forced_on_mcp_servers: mcp.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(created.forced_on_plugins, plugins);
    assert_eq!(created.forced_on_skills, skills);
    assert_eq!(created.forced_on_mcp_servers, mcp);

    let got = store.get("s-forced").await.unwrap().unwrap();
    assert_eq!(got.forced_on_plugins, plugins);
    assert_eq!(got.forced_on_skills, skills);
    assert_eq!(got.forced_on_mcp_servers, mcp);

    // A session without forced-on fields must default to empty (legacy compat).
    let plain = store
        .create(CreateInput {
            id: "s-plain".into(),
            repos: vec!["r".into()],
            prompt: "y".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(plain.forced_on_plugins.is_empty());
    assert!(plain.forced_on_skills.is_empty());
    assert!(plain.forced_on_mcp_servers.is_empty());
}

#[tokio::test]
async fn old_session_without_mcp_columns_defaults_to_empty() {
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                 createdAt INTEGER, seq INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq) VALUES ('old2','done','p',12345,0)")
                .execute(&pool).await.unwrap();
        pool.close().await;
    }
    let store = Store::open(path, dir.join("logs")).await.unwrap();
    let s = store.get("old2").await.unwrap().unwrap();
    assert!(s.hidden_mcp_servers.is_empty());
    assert!(s.extra_mcp_servers.is_empty());
}
