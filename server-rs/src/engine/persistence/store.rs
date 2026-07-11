use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

// Record/data types live in `records.rs`; re-exported so `crate::engine::store::<T>` resolves.
pub use crate::engine::persistence::records::*;

impl Store {
    /// Apply a `SessionUpdate` to the row for `id`. No-op if `is_empty()`.
    /// Returns the updated `Session` (post-write read-back) so the caller
    /// can inspect the result without a second round-trip.
    pub async fn apply_update(
        &self,
        id: &str,
        update: SessionUpdate,
    ) -> Result<Option<Session>, StoreError> {
        if update.is_empty() {
            return Ok(None);
        }
        let patch = Store::session_update_to_patch(&update);
        self.update(id, patch).await?;
        Ok(self.get(id).await?)
    }
}

const COLUMNS_DDL: &str = "id TEXT PRIMARY KEY, repo TEXT, prompt TEXT, worktreePath TEXT, branch TEXT, \
  claudeSessionId TEXT, status TEXT, costUsd REAL, exitCode INTEGER, error TEXT, errorKind TEXT, \
  createdAt INTEGER, startedAt INTEGER, endedAt INTEGER, baseSha TEXT, worktreeState TEXT, repos TEXT, \
  skills TEXT, hiddenSkills TEXT, baseShas TEXT, model TEXT, effort TEXT, mode TEXT, permissionMode TEXT, \
  titlePinned INTEGER, seq INTEGER, unreadEventId INTEGER DEFAULT 0, ackedEventId INTEGER DEFAULT 0";

// (name, decl) — columns added after the initial schema; each ALTER is applied idempotently.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("baseSha", "TEXT"),
    ("worktreeState", "TEXT DEFAULT 'live'"),
    ("repos", "TEXT"),
    ("skills", "TEXT"),
    ("baseShas", "TEXT"),
    ("model", "TEXT"),
    ("effort", "TEXT"),
    ("mode", "TEXT"),
    ("errorKind", "TEXT"),
    ("lastUserMessageAt", "INTEGER"),
    ("hiddenSkills", "TEXT"),
    ("permissionMode", "TEXT"),
    ("parentSessionId", "TEXT"),
    ("titlePinned", "INTEGER"),
    ("groupId", "TEXT"),
    ("unreadEventId", "INTEGER DEFAULT 0"),
    ("ackedEventId", "INTEGER DEFAULT 0"),
    // NULL = default ON for autoResume (existing rows keep the default without a backfill).
    ("autoResume", "INTEGER"),
    ("autoResumeAt", "INTEGER"),
    // NULL = no plugins disabled (rows written before the column existed keep the default).
    ("hiddenPlugins", "TEXT"),
    // Adopt / detach / re-sync provenance & handoff columns. Added for the
    // "Adopt & Re-sync Claude Code Sessions" feature.
    ("origin", "TEXT DEFAULT 'native'"),
    ("detached", "INTEGER DEFAULT 0"),
    ("nativeWatermarkLines", "INTEGER DEFAULT 0"),
    // NULL = no MCP servers hidden/added (rows written before the column existed keep the default).
    ("hiddenMcpServers", "TEXT"),
    ("extraMcpServers", "TEXT"),
    // NULL = no forced-on overrides (rows written before the column existed keep the default).
    ("forcedOnPlugins", "TEXT"),
    ("forcedOnSkills", "TEXT"),
    ("forcedOnMcpServers", "TEXT"),
];

use crate::util::now_ms;

fn normalize_mode(m: Option<String>) -> Option<String> {
    match m.as_deref() {
        Some("ultracode") | Some("ultra") => Some("ultracode".into()),
        _ => None,
    }
}
fn safe_json_vec(raw: Option<String>, fallback: Vec<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or(fallback)
}

pub struct Store {
    pool: SqlitePool,
    log_dir: PathBuf,
    seq: AtomicI64,
}

impl Store {
    pub async fn open(
        db_path: impl AsRef<Path>,
        log_dir: impl AsRef<Path>,
    ) -> Result<Store, StoreError> {
        let db_path = db_path.as_ref();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(log_dir.as_ref())?;
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await?;
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&pool)
            .await?;
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&pool)
            .await?;
        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS sessions ({COLUMNS_DDL})"
        ))
        .execute(&pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS groups (id TEXT PRIMARY KEY, name TEXT NOT NULL, icon TEXT, sortOrder INTEGER NOT NULL DEFAULT 0, createdAt INTEGER NOT NULL)").execute(&pool).await?;
        // migrate: add any missing ADDED_COLUMNS, then backfill lastUserMessageAt
        let have: Vec<String> = sqlx::query("PRAGMA table_info(sessions)")
            .fetch_all(&pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        for (name, decl) in ADDED_COLUMNS {
            if !have.iter().any(|h| h == name) {
                sqlx::query(&format!("ALTER TABLE sessions ADD COLUMN {name} {decl}"))
                    .execute(&pool)
                    .await?;
            }
        }
        sqlx::query(
            "UPDATE sessions SET lastUserMessageAt = createdAt WHERE lastUserMessageAt IS NULL",
        )
        .execute(&pool)
        .await?;
        // Idempotent backfill: any session that has a parent (created by fork_session) but
        // still has the legacy `origin='native'` (or NULL on rows that predate the column)
        // is upgraded in place to 'fork'. Re-running this on already-migrated data is a
        // no-op because `origin='fork'` is excluded by the WHERE.
        sqlx::query("UPDATE sessions SET origin='fork' WHERE parentSessionId IS NOT NULL AND (origin IS NULL OR origin='native')").execute(&pool).await?;
        // Atomic adopt: enforce that a non-null external Claude session id maps to at most one
        // row. A PARTIAL unique index (WHERE claudeSessionId IS NOT NULL) lets the many rows with
        // NULL csid coexist while making a concurrent double-adopt of the same csid fail the
        // second csid-setting UPDATE with a constraint error — which adopt_session's existing
        // rollback then cleans up. Guarded on the column actually existing: a very old/minimal
        // legacy table (claudeSessionId is a base column, not in ADDED_COLUMNS, so it isn't
        // back-migrated) would otherwise make the index DDL fail on a missing column.
        let cols_now: Vec<String> = sqlx::query("PRAGMA table_info(sessions)")
            .fetch_all(&pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        if cols_now.iter().any(|c| c == "claudeSessionId") {
            sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_claude_session_id ON sessions(claudeSessionId) WHERE claudeSessionId IS NOT NULL").execute(&pool).await?;
        }
        Ok(Store {
            pool,
            log_dir: log_dir.as_ref().to_path_buf(),
            seq: AtomicI64::new(0),
        })
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.log_dir.join(format!("{id}.jsonl"))
    }

    /// Append a single line (with trailing `\n`) to the session's log file.
    /// The blocking file write runs in `spawn_blocking` so it never blocks the async executor.
    pub async fn append_log(&self, id: &str, line: &str) -> Result<(), StoreError> {
        let path = self.log_path(id);
        let line = line.to_string();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(f, "{line}")?;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync (blocking) append — used from non-async contexts like `with_activity`.
    pub fn append_log_blocking(&self, id: &str, line: &str) {
        let path = self.log_path(id);
        let _ = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(f, "{line}")?;
            Ok(())
        })();
    }

    pub async fn create(&self, input: CreateInput) -> Result<Session, StoreError> {
        let now = now_ms();
        // repos: use provided list; if empty, derive from single-repo field (or empty vec).
        let repos: Vec<String> = if !input.repos.is_empty() {
            input.repos.clone()
        } else {
            input
                .repo
                .as_ref()
                .filter(|r| !r.is_empty())
                .map(|r| vec![r.clone()])
                .unwrap_or_default()
        };
        // repo: first element of repos, or empty string when no repos (non-optional string on the wire).
        let repo: String = repos.first().cloned().unwrap_or_default();

        // baseShas: if explicitly provided (non-empty), use as-is; otherwise derive.
        // Since CreateInput.base_shas defaults to empty map, "not provided" is approximated as empty
        // with no legacy single-repo field. If base_shas is empty AND input.repo was set,
        // synthesize {repo: base_sha} — e.g. create({repo:'r', baseSha:'x'}) → baseShas={r:x}.
        let base_shas: std::collections::HashMap<String, Option<String>> =
            if !input.base_shas.is_empty() {
                // Explicitly provided — use as-is (covers multi-repo: create({repos, baseShas})).
                input.base_shas.clone()
            } else if let Some(ref r) = input.repo {
                if !r.is_empty() {
                    // Legacy single-repo form: create({repo:'r', baseSha:'x'}) → {r: Some("x") or None}
                    let mut m = std::collections::HashMap::new();
                    m.insert(r.clone(), input.base_sha.clone());
                    m
                } else {
                    std::collections::HashMap::new()
                }
            } else {
                // No repo and no base_shas → {}
                std::collections::HashMap::new()
            };

        // baseSha: prefer the value from baseShas[repo] if present, otherwise fall back to input.baseSha.
        let base_sha: Option<String> = if base_shas.contains_key(&repo) {
            base_shas.get(&repo).cloned().flatten()
        } else {
            input.base_sha.clone()
        };

        let s = Session {
            id: input.id,
            // repo is non-optional String (empty = no repo).
            repo: repo.clone(),
            repos: repos.clone(),
            skills: input.skills.clone(),
            hidden_skills: input.hidden_skills.clone(),
            hidden_plugins: input.hidden_plugins.clone(),
            prompt: input.prompt,
            model: input.model,
            effort: input.effort,
            // Fix #5: store raw mode, do NOT normalize on create. Normalization happens on read.
            mode: input.mode,
            permission_mode: input.permission_mode.clone(),
            // New sessions are never pinned; a machine title from generate must
            // remain retitle-able until the user manually renames the session.
            title_pinned: false,
            worktree_path: input.worktree_path,
            branch: input.branch,
            claude_session_id: None,
            status: "pending".into(),
            cost_usd: None,
            exit_code: None,
            error: None,
            error_kind: None,
            created_at: now,
            started_at: None,
            ended_at: None,
            last_user_message_at: now,
            base_sha: base_sha.clone(),
            base_shas: base_shas.clone(),
            worktree_state: "live".into(),
            activity: None,
            awaiting_input: None,
            workflow_running: None,
            pending_prompt: None,
            parent_session_id: input.parent_session_id.clone(),
            group_id: input.group_id.clone(),
            unread_event_id: 0,
            acked_event_id: 0,
            // Default ON; the INSERT leaves the column NULL, which reads back as true.
            auto_resume: true,
            auto_resume_at: None,
            // Adopt / detach / watermark defaults; origin can be overridden via CreateInput::origin().
            origin: input.origin.clone().unwrap_or_else(|| "native".into()),
            detached: false,
            native_watermark_lines: 0,
            hidden_mcp_servers: input.hidden_mcp_servers.clone(),
            extra_mcp_servers: input.extra_mcp_servers.clone(),
            forced_on_plugins: input.forced_on_plugins.clone(),
            forced_on_skills: input.forced_on_skills.clone(),
            forced_on_mcp_servers: input.forced_on_mcp_servers.clone(),
        };
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        // Bind repo column as the raw string ("" not NULL for no-repo sessions).
        sqlx::query("INSERT INTO sessions (id,repo,repos,skills,hiddenSkills,hiddenPlugins,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,permissionMode,titlePinned,parentSessionId,seq,groupId,unreadEventId,ackedEventId,origin,hiddenMcpServers,extraMcpServers,forcedOnPlugins,forcedOnSkills,forcedOnMcpServers) \
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&s.id).bind(&s.repo).bind(serde_json::to_string(&s.repos)?).bind(serde_json::to_string(&s.skills)?).bind(serde_json::to_string(&s.hidden_skills)?).bind(serde_json::to_string(&s.hidden_plugins)?)
            .bind(&s.prompt).bind(&s.worktree_path).bind(&s.branch).bind(&s.claude_session_id).bind(&s.status)
            .bind(s.cost_usd).bind(s.exit_code).bind(&s.error).bind(&s.error_kind).bind(s.created_at)
            .bind(s.started_at).bind(s.ended_at).bind(s.last_user_message_at).bind(&base_sha)
            .bind(serde_json::to_string(&s.base_shas)?).bind(&s.worktree_state)
            .bind(&s.model).bind(&s.effort).bind(&s.mode).bind(&input.permission_mode).bind(s.title_pinned).bind(&input.parent_session_id).bind(seq)
            .bind(&input.group_id).bind(s.unread_event_id).bind(s.acked_event_id).bind(&s.origin)
            .bind(serde_json::to_string(&s.hidden_mcp_servers)?)
            .bind(serde_json::to_string(&s.extra_mcp_servers)?)
            .bind(serde_json::to_string(&s.forced_on_plugins)?)
            .bind(serde_json::to_string(&s.forced_on_skills)?)
            .bind(serde_json::to_string(&s.forced_on_mcp_servers)?)
            .execute(&self.pool).await?;
        Ok(s)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| row_to_session(&r)))
    }

    /// Look up a session by its external Claude session id (`claudeSessionId`).
    /// Used by `Engine::adopt_session` to reject a double-adopt: an external
    /// csid maps to at most one adopted agentic-dev row. Returns the first match.
    pub async fn session_by_csid(&self, csid: &str) -> Result<Option<Session>, StoreError> {
        let row = sqlx::query("SELECT * FROM sessions WHERE claudeSessionId = ? LIMIT 1")
            .bind(csid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| row_to_session(&r)))
    }

    /// Return every session whose `parentSessionId` equals `parent_id`. The list is in
    /// creation-order (seq ASC). Used by callers that want to render "forked to N sessions"
    /// on a parent session; not exposed via the API in v1.
    pub async fn list_children(&self, parent_id: &str) -> Result<Vec<Session>, StoreError> {
        let rows = sqlx::query("SELECT * FROM sessions WHERE parentSessionId = ? ORDER BY seq ASC")
            .bind(parent_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| row_to_session(r)).collect())
    }

    pub async fn list(&self) -> Result<Vec<Session>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM sessions ORDER BY COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_session).collect())
    }

    /// Convert a builder-style `SessionUpdate` into the legacy `SessionPatch`
    /// so the existing `Store::update(&SessionPatch)` path can persist it.
    /// Unset fields are dropped; Set fields become `Some(v)`; Clear fields
    /// become `Some(None)` (the SQL-NULL write sentinel).
    pub fn session_update_to_patch(u: &SessionUpdate) -> SessionPatch {
        SessionPatch {
            status: match &u.status {
                Field::Set(s) => Some(s.as_str().to_string()),
                _ => None,
            },
            claude_session_id: match &u.claude_session_id {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            cost_usd: match &u.cost_usd {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            exit_code: match &u.exit_code {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            error: match &u.error {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            error_kind: match &u.error_kind {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            started_at: match &u.started_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            ended_at: match &u.ended_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            last_user_message_at: u.last_user_message_at,
            prompt: u.prompt.clone(),
            worktree_state: u.worktree_state.clone(),
            model: u.model.clone(),
            effort: u.effort.clone(),
            mode: u.mode.clone(),
            permission_mode: u.permission_mode.clone(),
            title_pinned: u.title_pinned,
            group_id: u
                .group_id
                .clone()
                .map(|gid| {
                    if gid.is_empty() {
                        Some(None)
                    } else {
                        Some(Some(gid))
                    }
                })
                .unwrap_or(None),
            unread_event_id: u.unread_event_id,
            auto_resume: u.auto_resume,
            auto_resume_at: match &u.auto_resume_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
        }
    }

    pub async fn update(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError> {
        let mut sets: Vec<&str> = Vec::new();
        // Build the SET list + bind in the same order.
        macro_rules! col {
            ($opt:expr, $sql:literal) => {
                if $opt.is_some() {
                    sets.push($sql);
                }
            };
        }
        col!(patch.status, "status = ?");
        col!(patch.claude_session_id, "claudeSessionId = ?");
        col!(patch.cost_usd, "costUsd = ?");
        col!(patch.exit_code, "exitCode = ?");
        col!(patch.error, "error = ?");
        col!(patch.error_kind, "errorKind = ?");
        col!(patch.started_at, "startedAt = ?");
        col!(patch.ended_at, "endedAt = ?");
        col!(patch.last_user_message_at, "lastUserMessageAt = ?");
        col!(patch.prompt, "prompt = ?");
        col!(patch.worktree_state, "worktreeState = ?");
        col!(patch.model, "model = ?");
        col!(patch.effort, "effort = ?");
        col!(patch.mode, "mode = ?");
        col!(patch.permission_mode, "permissionMode = ?");
        col!(patch.title_pinned, "titlePinned = ?");
        col!(patch.group_id, "groupId = ?");
        col!(patch.unread_event_id, "unreadEventId = ?");
        col!(patch.auto_resume, "autoResume = ?");
        col!(patch.auto_resume_at, "autoResumeAt = ?");
        if sets.is_empty() {
            return Ok(());
        }
        let sql = format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = &patch.status {
            q = q.bind(v);
        }
        if let Some(v) = &patch.claude_session_id {
            q = q.bind(v);
        }
        if let Some(v) = &patch.cost_usd {
            q = q.bind(v);
        }
        if let Some(v) = &patch.exit_code {
            q = q.bind(v);
        }
        if let Some(v) = &patch.error {
            q = q.bind(v);
        }
        if let Some(v) = &patch.error_kind {
            q = q.bind(v);
        }
        if let Some(v) = &patch.started_at {
            q = q.bind(v);
        }
        if let Some(v) = &patch.ended_at {
            q = q.bind(v);
        }
        if let Some(v) = &patch.last_user_message_at {
            q = q.bind(v);
        }
        if let Some(v) = &patch.prompt {
            q = q.bind(v);
        }
        if let Some(v) = &patch.worktree_state {
            q = q.bind(v);
        }
        if let Some(v) = &patch.model {
            q = q.bind(v);
        }
        if let Some(v) = &patch.effort {
            q = q.bind(v);
        }
        if let Some(v) = &patch.mode {
            q = q.bind(v);
        }
        if let Some(v) = &patch.permission_mode {
            q = q.bind(v);
        }
        if let Some(v) = &patch.title_pinned {
            q = q.bind(v);
        }
        if let Some(v) = &patch.group_id {
            q = q.bind(v);
        }
        if let Some(v) = patch.unread_event_id {
            q = q.bind(v);
        }
        if let Some(v) = patch.auto_resume {
            q = q.bind(v);
        }
        if let Some(v) = &patch.auto_resume_at {
            q = q.bind(v);
        }
        q.bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// Persist a computed auto-resume schedule, but only while the row is STILL in the
    /// usage-limit-errored state with no schedule. Returns false when a concurrent user
    /// follow-up (which clears `errorKind`/`autoResumeAt`) superseded this episode between
    /// the scheduler's snapshot and this write — the stale schedule must not land on the
    /// now-active session.
    pub async fn schedule_auto_resume(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE sessions SET autoResumeAt = ? \
             WHERE id = ? AND autoResumeAt IS NULL AND errorKind = 'usage_limit'",
        )
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Atomically claim a due auto-resume: clear `autoResumeAt` only while it still equals `at`
    /// AND the session is still in the usage-limit-errored state. Exactly one concurrent claimer
    /// can win (`rows_affected == 1`); a loser must NOT fire — a user follow-up or another tick
    /// already superseded this schedule.
    pub async fn claim_auto_resume(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE sessions SET autoResumeAt = NULL \
             WHERE id = ? AND autoResumeAt = ? AND errorKind = 'usage_limit'",
        )
        .bind(id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Atomically increment `unreadEventId` for a session. Called by the engine when the session
    /// reaches a "your turn" state — IDLE (awaiting input) or DONE. The client compares this
    /// monotonically-increasing id against its stored `lastAckedEventId` to determine unread status
    /// (Discord-style: unread = unreadEventId > lastAckedEventId, requiring no timestamp comparison
    /// or client-side idle detection loops).
    pub async fn incr_unread_event_id(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE sessions SET unreadEventId = COALESCE(unreadEventId, 0) + 1 WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Discord-style: ack session event. Sets ackedEventId = MAX(ackedEventId, eid).
    pub async fn ack_event(&self, id: &str, eid: i64) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE sessions SET ackedEventId = MAX(COALESCE(ackedEventId, 0), ?) WHERE id = ?",
        )
        .bind(eid)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set the `detached` flag for an adopted session. `true` = handed off to a
    /// terminal `claude` (single-writer guard / reconcile trigger); `false` =
    /// agentic-dev reclaims ownership after a successful reconcile on reopen.
    /// SQLite has no native bool — stored as INTEGER 0/1.
    pub async fn set_detached(&self, id: &str, v: bool) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET detached = ? WHERE id = ?")
            .bind(v as i64)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set `nativeWatermarkLines` for a session — the line-count of the native
    /// Claude transcript (#2) already reflected in this session's rendered log (#1).
    /// Updated by `Engine::adopt_session` (full import) and
    /// `Engine::reconcile_from_native` (delta import).
    pub async fn set_watermark(&self, id: &str, n: i64) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET nativeWatermarkLines = ? WHERE id = ?")
            .bind(n)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete the row + best-effort remove the log file. Idempotent.
    pub async fn remove(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        let _ = std::fs::remove_file(self.log_path(id)); // best-effort
        Ok(())
    }

    /// Read the session log file → Vec of non-empty lines ([] if missing).
    pub fn read_log(&self, id: &str) -> Vec<String> {
        match std::fs::read_to_string(self.log_path(id)) {
            Ok(s) => s
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // ── Groups table CRUD ──────────────────────────────────────────────────

    pub async fn list_groups(&self) -> Result<Vec<Group>, StoreError> {
        let rows = sqlx::query("SELECT * FROM groups ORDER BY sortOrder ASC, createdAt ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| Group {
                id: r.try_get("id").unwrap_or_default(),
                name: r.try_get("name").unwrap_or_default(),
                icon: r.try_get("icon").ok().flatten(),
                sort_order: r.try_get("sortOrder").unwrap_or(0),
                created_at: r.try_get("createdAt").unwrap_or(0),
            })
            .collect())
    }

    pub async fn get_group(&self, id: &str) -> Result<Option<Group>, StoreError> {
        let row = sqlx::query("SELECT * FROM groups WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| Group {
            id: r.try_get("id").unwrap_or_default(),
            name: r.try_get("name").unwrap_or_default(),
            icon: r.try_get("icon").ok().flatten(),
            sort_order: r.try_get("sortOrder").unwrap_or(0),
            created_at: r.try_get("createdAt").unwrap_or(0),
        }))
    }

    pub async fn create_group(&self, name: &str, icon: Option<&str>) -> Result<Group, StoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let sort_order = now; // default: created order
        sqlx::query("INSERT INTO groups (id, name, icon, sortOrder, createdAt) VALUES (?,?,?,?,?)")
            .bind(&id)
            .bind(name)
            .bind(icon)
            .bind(sort_order)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(Group {
            id,
            name: name.to_string(),
            icon: icon.map(|s| s.to_string()),
            sort_order,
            created_at: now,
        })
    }

    /// Return the id of the group whose `name` matches; create it (with a
    /// stable id for `"Claude Code Adopted"`, or a deterministic
    /// `grp-<hash>` derived from `name` for any other name) if absent.
    /// Idempotent and safe under concurrent inserters: the generated id is
    /// a pure function of `name`, so two racing callers compute the SAME id
    /// and the loser's INSERT collides on the PRIMARY KEY, which is
    /// swallowed by `INSERT OR IGNORE`. We then re-`SELECT` by name so a
    /// caller that raced still resolves to the winner's id (a pre-existing
    /// group of any id for `name` is also respected). The `sortOrder` is
    /// set to `1000` (same default as the rest of the engine so the group
    /// renders in a predictable place in the UI).
    pub async fn ensure_group(&self, name: &str) -> Result<String, StoreError> {
        if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM groups WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(id);
        }
        let id = if name == "Claude Code Adopted" {
            "grp-adopted".to_string()
        } else {
            // Deterministic id from the name (no new dep): two racing
            // callers produce the same `grp-<hash>`, so their INSERT
            // statements collide on PRIMARY KEY id and INSERT OR IGNORE
            // resolves the race without creating duplicate rows.
            use std::hash::{DefaultHasher, Hash, Hasher};
            let mut h = DefaultHasher::new();
            Hash::hash(name, &mut h);
            format!("grp-{:016x}", Hasher::finish(&h))
        };
        let now = now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO groups (id, name, icon, sortOrder, createdAt) VALUES (?,?,?,?,?)",
        )
        .bind(&id)
        .bind(name)
        .bind(Option::<String>::None)
        .bind(1000i64)
        .bind(now)
        .execute(&self.pool)
        .await?;
        // Resolve the winner's id in case a concurrent caller already
        // inserted a row for `name` between our SELECT and INSERT.
        let winner = sqlx::query_scalar::<_, String>("SELECT id FROM groups WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await?;
        Ok(winner)
    }

    pub async fn update_group(
        &self,
        id: &str,
        name: Option<&str>,
        icon: Option<&str>,
    ) -> Result<Option<Group>, StoreError> {
        let existing = self.get_group(id).await?;
        let Some(_) = existing else {
            return Ok(None);
        };
        let mut sets: Vec<&str> = Vec::new();
        if name.is_some() {
            sets.push("name = ?");
        }
        if icon.is_some() {
            sets.push("icon = ?");
        }
        if sets.is_empty() {
            return self.get_group(id).await;
        }
        let sql = format!("UPDATE groups SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = name {
            q = q.bind(v);
        }
        if let Some(v) = icon {
            q = q.bind(v);
        }
        q.bind(id).execute(&self.pool).await?;
        self.get_group(id).await
    }

    /// Delete a group and set all sessions referencing it to NULL (uncategorized).
    pub async fn delete_group(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET groupId = NULL WHERE groupId = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM groups WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Convert a SQLite row to a `Session`, applying all column fallbacks.
fn row_to_session(r: &SqliteRow) -> Session {
    // repo: raw column value — exposed as a non-optional String (empty string when no repo).
    let repo_raw: String = r
        .try_get::<Option<String>, _>("repo")
        .ok()
        .flatten()
        .unwrap_or_default();
    // repo_truthy: non-empty repo for fallback computations (empty string treated as absent).
    let repo_truthy: Option<&str> = if repo_raw.is_empty() {
        None
    } else {
        Some(&repo_raw)
    };
    let base_sha_col: Option<String> = r.try_get("baseSha").ok().flatten();

    // baseShas: parse the JSON column, falling back to {repo: baseSha} when the column is NULL
    // and a single-repo row has both repo and baseSha set.
    let base_shas: std::collections::HashMap<String, Option<String>> = {
        let raw: Option<String> = r.try_get("baseShas").ok().flatten();
        let parsed = raw.and_then(|s| serde_json::from_str(&s).ok());
        parsed.unwrap_or_else(|| {
            // Fallback: if repo truthy AND baseSha non-null → {repo: baseSha}, else {}
            match (repo_truthy, &base_sha_col) {
                (Some(rep), Some(sha)) => {
                    let mut m = std::collections::HashMap::new();
                    m.insert(rep.to_string(), Some(sha.clone()));
                    m
                }
                _ => std::collections::HashMap::new(),
            }
        })
    };

    // repos: parse JSON column, falling back to [repo] when set (empty string → [] not [""])
    let repos: Vec<String> = {
        let raw: Option<String> = r.try_get("repos").ok().flatten();
        safe_json_vec(
            raw,
            repo_truthy.map(|r| vec![r.to_string()]).unwrap_or_default(),
        )
    };

    // baseSha: prefer column value; fall back to baseShas[repos[0]] when column is NULL.
    let base_sha: Option<String> = base_sha_col.clone().or_else(|| {
        repos
            .first()
            .and_then(|r0| base_shas.get(r0).cloned().flatten())
    });

    let created_at: i64 = r.try_get("createdAt").unwrap_or(0);
    Session {
        id: r.try_get("id").unwrap_or_default(),
        // repo: raw column value, "" when empty (non-optional string on the wire).
        repo: repo_raw,
        repos,
        skills: safe_json_vec(r.try_get("skills").ok().flatten(), vec![]),
        hidden_skills: safe_json_vec(r.try_get("hiddenSkills").ok().flatten(), vec![]),
        hidden_plugins: safe_json_vec(r.try_get("hiddenPlugins").ok().flatten(), vec![]),
        prompt: r.try_get("prompt").unwrap_or_default(),
        model: r.try_get("model").ok().flatten(),
        effort: r.try_get("effort").ok().flatten(),
        // Normalization happens on READ (not on create/write).
        mode: normalize_mode(r.try_get("mode").ok().flatten()),
        permission_mode: r.try_get("permissionMode").ok().flatten(),
        // SQLite stores bool as INTEGER 0/1; NULL (legacy/migrated rows, and new
        // rows where create() omits the column) reads as false = unpinned.
        title_pinned: r
            .try_get::<Option<i64>, _>("titlePinned")
            .ok()
            .flatten()
            .map(|n| n != 0)
            .unwrap_or(false),
        worktree_path: r.try_get("worktreePath").ok().flatten(),
        branch: r.try_get("branch").ok().flatten(),
        claude_session_id: r.try_get("claudeSessionId").ok().flatten(),
        status: r.try_get("status").unwrap_or_default(),
        cost_usd: r.try_get("costUsd").ok().flatten(),
        exit_code: r.try_get("exitCode").ok().flatten(),
        error: r.try_get("error").ok().flatten(),
        error_kind: r.try_get("errorKind").ok().flatten(),
        created_at,
        started_at: r.try_get("startedAt").ok().flatten(),
        ended_at: r.try_get("endedAt").ok().flatten(),
        unread_event_id: r
            .try_get::<Option<i64>, _>("unreadEventId")
            .ok()
            .flatten()
            .unwrap_or(0),
        acked_event_id: r
            .try_get::<Option<i64>, _>("ackedEventId")
            .ok()
            .flatten()
            .unwrap_or(0),
        last_user_message_at: r
            .try_get::<Option<i64>, _>("lastUserMessageAt")
            .ok()
            .flatten()
            .unwrap_or(created_at),
        base_sha,
        base_shas,
        worktree_state: r
            .try_get::<Option<String>, _>("worktreeState")
            .ok()
            .flatten()
            .unwrap_or_else(|| "live".into()),
        // Runtime-only fields — always None when reading from DB; the engine sets them via with_activity().
        activity: None,
        awaiting_input: None,
        workflow_running: None,
        pending_prompt: None,
        parent_session_id: r.try_get("parentSessionId").ok().flatten(),
        group_id: r.try_get("groupId").ok().flatten(),
        // NULL (legacy rows + fresh create()) = default ON.
        auto_resume: r
            .try_get::<Option<i64>, _>("autoResume")
            .ok()
            .flatten()
            .map(|n| n != 0)
            .unwrap_or(true),
        auto_resume_at: r.try_get::<Option<i64>, _>("autoResumeAt").ok().flatten(),
        // Adopt / detach / watermark — legacy rows (pre-ALTER) read as defaults via the column DEFAULT.
        origin: r
            .try_get::<Option<String>, _>("origin")
            .ok()
            .flatten()
            .unwrap_or_else(|| "native".into()),
        detached: r
            .try_get::<Option<i64>, _>("detached")
            .ok()
            .flatten()
            .map(|n| n != 0)
            .unwrap_or(false),
        native_watermark_lines: r
            .try_get::<Option<i64>, _>("nativeWatermarkLines")
            .ok()
            .flatten()
            .unwrap_or(0),
        hidden_mcp_servers: safe_json_vec(r.try_get("hiddenMcpServers").ok().flatten(), vec![]),
        extra_mcp_servers: r
            .try_get::<Option<String>, _>("extraMcpServers")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Vec<McpServerDef>>(&s).ok())
            .unwrap_or_default(),
        forced_on_plugins: safe_json_vec(r.try_get("forcedOnPlugins").ok().flatten(), vec![]),
        forced_on_skills: safe_json_vec(r.try_get("forcedOnSkills").ok().flatten(), vec![]),
        forced_on_mcp_servers: safe_json_vec(
            r.try_get("forcedOnMcpServers").ok().flatten(),
            vec![],
        ),
    }
}

#[cfg(test)]
mod tests;
