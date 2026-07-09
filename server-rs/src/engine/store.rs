use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;

/// MCP server definition for per-session ad-hoc injection.
/// Supports stdio (`command`+`args`+`env`) and http/sse (`url`+`type`+`headers`) transports.
/// Serde uses the field names verbatim (camelCase where needed via rename).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct McpServerDef {
    pub name: String,
    // stdio transport:
    #[serde(skip_serializing_if = "Option::is_none")] pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub env: Option<std::collections::BTreeMap<String, String>>,
    // http/sse transport:
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")] pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub headers: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("sqlite error: {0}")] Sqlx(#[from] sqlx::Error),
    #[error("io error: {0}")] Io(#[from] std::io::Error),
    #[error("json error: {0}")] Json(#[from] serde_json::Error),
}

/// Runtime-only activity counters attached to a Session by the engine's with_activity().
/// Not persisted to the DB — only lives in-process while the engine is running.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Activity {
    pub turns: u64,
    #[serde(rename = "lastSkill", skip_serializing_if = "Option::is_none")]
    pub last_skill: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Session {
    pub id: String,
    /// Legacy single-repo field = repos[0]. Non-optional string; empty when no repo
    /// (never null/None on the wire).
    pub repo: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    /// Skills the user chose to HIDE from this session (blacklist). Mapped to claude
    /// `skillOverrides:{<name>:"off"}` in the `--settings` flag at spawn time.
    #[serde(rename = "hiddenSkills", default)] pub hidden_skills: Vec<String>,
    /// Plugins (`<plugin>@<marketplace>` ids) the user chose to DISABLE for this session
    /// (blacklist). Mapped to claude `enabledPlugins:{<id>:false}` in the `--settings` flag
    /// at spawn time.
    #[serde(rename = "hiddenPlugins", default)] pub hidden_plugins: Vec<String>,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    /// Permission mode for this session: "plan" / "default" / "acceptEdits" / "bypassPermissions".
    /// `None` → SDK default (equivalent to "default" = prompt on every tool call).
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// True when the user has manually set this session's title (via a
    /// `setTitle=true` follow-up). Pinned titles are NOT overwritten by the
    /// periodic retitle; machine-generated titles (submit-time `generate`)
    /// leave this false so they can still be retitled. Column: `titlePinned`.
    #[serde(rename = "titlePinned", default)]
    pub title_pinned: bool,
    #[serde(rename = "worktreePath")] pub worktree_path: Option<String>,
    pub branch: Option<String>,
    #[serde(rename = "claudeSessionId")] pub claude_session_id: Option<String>,
    pub status: String,
    #[serde(rename = "costUsd")] pub cost_usd: Option<f64>,
    #[serde(rename = "exitCode")] pub exit_code: Option<i64>,
    pub error: Option<String>,
    #[serde(rename = "errorKind")] pub error_kind: Option<String>,
    #[serde(rename = "createdAt")] pub created_at: i64,
    #[serde(rename = "startedAt")] pub started_at: Option<i64>,
    #[serde(rename = "endedAt")] pub ended_at: Option<i64>,
    #[serde(rename = "lastUserMessageAt")] pub last_user_message_at: i64,
    #[serde(rename = "baseSha")] pub base_sha: Option<String>,
    #[serde(rename = "baseShas")] pub base_shas: std::collections::HashMap<String, Option<String>>,
    #[serde(rename = "worktreeState")] pub worktree_state: String,
    // Runtime-only fields set by the engine's with_activity(); not persisted to DB.
    #[serde(skip_serializing_if = "Option::is_none")] pub activity: Option<Activity>,
    #[serde(rename = "awaitingInput", skip_serializing_if = "Option::is_none")] pub awaiting_input: Option<bool>,
    #[serde(rename = "workflowRunning", skip_serializing_if = "Option::is_none")] pub workflow_running: Option<bool>,
    /// Runtime-only: the wire payload of the prompt this session is currently PARKED on (an
    /// AskUserQuestion / perm / plan awaiting the user), or None. Set by the engine's with_activity()
    /// from EngineState.parked; never persisted. Lets the client render the awaiting card from an
    /// authoritative source instead of inferring it from the log.
    #[serde(rename = "pendingPrompt", skip_serializing_if = "Option::is_none")] pub pending_prompt: Option<serde_json::Value>,
    #[serde(rename = "parentSessionId", skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Session group assignment (DB-backed folder). `None` = uncategorized.
    #[serde(rename = "groupId", skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Monotonic counter incremented each time the session reaches a "your turn" state (DONE
    /// or IDLE / awaiting input). Discord-style unread detection: a session is unread when
    /// this id is strictly greater than the client's last-acked id. Persisted in the DB row
    /// so it survives restarts. Not a runtime-only field — the engine writes it on transition.
    #[serde(rename = "unreadEventId")]
    pub unread_event_id: i64,
    /// Server-authoritative: the highest unreadEventId the user has acknowledged.
    /// Set by `PUT /api/sessions/:id/ack`. Discord-style: unread = unreadEventId > ackedEventId.
    /// Both fields are server-side — the client stores nothing locally.
    #[serde(rename = "ackedEventId")]
    pub acked_event_id: i64,
    /// Per-session toggle: when a turn fails with `errorKind == "usage_limit"` (the account's
    /// 5-hour / 7-day token window is exhausted), the auto-resume scheduler re-sends the turn
    /// after the window resets. Default ON. DB column `autoResume` (NULL = on, so rows written
    /// before the column existed keep the default).
    #[serde(rename = "autoResume", default = "default_true")]
    pub auto_resume: bool,
    /// When set: epoch-ms at which the auto-resume scheduler will re-enqueue this session
    /// (the usage-limit reset time + a small buffer). NULL when nothing is scheduled.
    /// Cleared by any accepted follow-up (manual or automatic). Column `autoResumeAt`.
    #[serde(rename = "autoResumeAt", skip_serializing_if = "Option::is_none")]
    pub auto_resume_at: Option<i64>,
    /// Provenance of this session row: `"native"` (default — created normally),
    /// `"fork"` (created by `Engine::fork_session`), or `"adopted"` (imported from
    /// an external `claude` transcript). Immutable source of truth (survives user
    /// regrouping). Column `origin` (TEXT DEFAULT 'native').
    #[serde(default = "default_origin")] pub origin: String,
    /// `true` while this session has been handed off to a terminal `claude` and the
    /// server is paused. Used as a single-writer guard and as the reconcile trigger:
    /// when `detached == true` on a follow-up / reopen path, the engine imports any
    /// native-transcript delta into `#1` and clears the flag. Column `detached`
    /// (INTEGER DEFAULT 0 — SQLite has no native bool, stored as 0/1).
    #[serde(default)] pub detached: bool,
    /// Line-count of the native Claude transcript (#2) already reflected in this
    /// session's rendered log (#1). Updated by adopt (full import) and reconcile
    /// (delta import). Column `nativeWatermarkLines` (INTEGER DEFAULT 0).
    #[serde(rename = "nativeWatermarkLines", default)] pub native_watermark_lines: i64,
}

/// Serde default for `Session::origin` — keeps the wire shape `"native"` for
/// legacy rows that predate the column (and any row written before `origin`
/// was set explicitly).
fn default_origin() -> String { "native".into() }

/// Serde default for `Session::auto_resume` — the toggle is opt-OUT.
fn default_true() -> bool {
    true
}

/// A user-created folder that groups sessions together.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

/// Input for creating a new session. Callers that
/// use `..Default::default()` continue to compile — new fields all have `Option`/`Vec` defaults.
#[derive(Default)]
pub struct CreateInput {
    pub id: String,
    pub prompt: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    pub hidden_skills: Vec<String>,
    pub hidden_plugins: Vec<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Raw mode string (e.g. `"ultra"`, `"ultracode"`). Stored as-is; normalization happens
    /// only on read in `row_to_session` (write stores what the caller provided).
    pub mode: Option<String>,
    /// Override for the `repo` column. When `None`, derived from `repos.first()` (empty string when no repos).
    pub repo: Option<String>,
    /// Optional base SHA for the primary repo.
    pub base_sha: Option<String>,
    /// Optional per-repo base SHAs. Defaults to `{}`.
    pub base_shas: std::collections::HashMap<String, Option<String>>,
    /// Optional parent session id (set by `Engine::fork_session`). None for sessions created
    /// from scratch. Always None on write for sessions originating from a normal create.
    pub parent_session_id: Option<String>,
    /// Per-session permission mode override (e.g. `"plan"`, `"acceptEdits"`, `"bypassPermissions"`).
    /// Fork copies it from the source; new sessions default to None (which downstream resolves to
    /// the engine default). The column on disk is `permissionMode`.
    pub permission_mode: Option<String>,
    /// Optional group assignment. `None` = uncategorized.
    pub group_id: Option<String>,
    /// Provenance of the session: `"native"` (default — created normally),
    /// `"fork"` (created by `Engine::fork_session`), or `"adopted"` (imported
    /// from an external `claude` transcript). `None` writes the column
    /// default `"native"`. Column `origin` (TEXT DEFAULT 'native').
    pub origin: Option<String>,
}

impl CreateInput {
    /// Builder: set the provenance marker (e.g. `"adopted"` for `Engine::adopt_session`).
    pub fn origin(mut self, v: impl Into<String>) -> Self {
        self.origin = Some(v.into());
        self
    }
}

/// A partial update. Only `Some` fields are written. Extend as new fields are needed.
#[derive(Default)]
pub struct SessionPatch {
    pub status: Option<String>,
    pub claude_session_id: Option<Option<String>>,
    pub cost_usd: Option<Option<f64>>,
    pub exit_code: Option<Option<i64>>,
    pub error: Option<Option<String>>,
    pub error_kind: Option<Option<String>>,
    pub started_at: Option<Option<i64>>,
    pub ended_at: Option<Option<i64>>,
    pub last_user_message_at: Option<i64>,
    pub prompt: Option<String>,
    pub worktree_state: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub permission_mode: Option<String>,
    pub title_pinned: Option<bool>,
    /// Optional group assignment: `None` = don't touch, `Some(None)` = clear to uncategorized,
    /// `Some(Some(id))` = assign to group.
    pub group_id: Option<Option<String>>,
    pub unread_event_id: Option<i64>,
    /// Auto-resume-on-usage-limit-reset toggle: `None` = don't touch.
    pub auto_resume: Option<bool>,
    /// Scheduled auto-resume time (epoch ms): `None` = don't touch, `Some(None)` = clear,
    /// `Some(Some(t))` = schedule.
    pub auto_resume_at: Option<Option<i64>>,
}

// ───────────────────────────────────────────────────────────────────────────
// SessionUpdate: builder API replacing Option<Option<T>> triple-state.
//
// `Field<T>` makes "don't touch" / "set value" / "clear to NULL" three
// distinct states the compiler can check. `SessionUpdate` is a fluent
// builder; `apply()` converts to the legacy `SessionPatch` and persists
// via the existing `Store::update(&SessionPatch)` path. New engine code
// should prefer `SessionUpdate`; `SessionPatch` is kept for back-compat
// (every existing call site keeps working unchanged).
// ───────────────────────────────────────────────────────────────────────────

/// Tri-state column field. Replaces the `Option<Option<T>>` footgun in
/// `SessionPatch`: outer `None` is no longer ambiguous with `Some(None)`.
#[derive(Debug, Default, Clone, PartialEq)]
pub enum Field<T> {
    #[default]
    Unset,
    Set(T),
    Clear,
}

impl<T> Field<T> {
    pub fn is_unset(&self) -> bool {
        matches!(self, Field::Unset)
    }
    pub fn is_set(&self) -> bool {
        matches!(self, Field::Set(_))
    }
    pub fn is_clear(&self) -> bool {
        matches!(self, Field::Clear)
    }
}

#[derive(Debug, Default)]
pub struct SessionUpdate {
    pub status: Field<crate::engine::status::SessionStatus>,
    pub claude_session_id: Field<String>,
    pub cost_usd: Field<f64>,
    pub exit_code: Field<i64>,
    pub error: Field<String>,
    pub error_kind: Field<String>,
    pub started_at: Field<i64>,
    pub ended_at: Field<i64>,
    pub unread_event_id: Option<i64>,  // None = don't touch; Some = set to this value
    pub last_user_message_at: Option<i64>,
    pub prompt: Option<String>,
    pub worktree_state: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub permission_mode: Option<String>,
    pub title_pinned: Option<bool>,
    pub group_id: Option<String>,
    pub auto_resume: Option<bool>,
    pub auto_resume_at: Field<i64>,
}

impl SessionUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(mut self, s: crate::engine::status::SessionStatus) -> Self {
        self.status = Field::Set(s);
        self
    }

    /// Set status from a `&str` (e.g. "done", "failed", "killed").
    /// Used by `on_exit` which has a decision-table string variable.
    pub fn status_str(mut self, s: &str) -> Self {
        use std::str::FromStr;
        if let Ok(parsed) = crate::engine::status::SessionStatus::from_str(s) {
            self.status = Field::Set(parsed);
        }
        self
    }

    pub fn claude_session_id(mut self, v: impl Into<String>) -> Self {
        self.claude_session_id = Field::Set(v.into());
        self
    }

    /// `cost()` is additive: it adds `v` to the existing `cost_usd` rather
    /// than overwriting. The accumulator is owned by the engine, not the
    /// update — see `Engine::transition()` (transition.rs, PR3) for the
    /// read-then-write pattern.
    pub fn cost(mut self, v: f64) -> Self {
        self.cost_usd = Field::Set(v);
        self
    }

    pub fn exit_code(mut self, v: i64) -> Self {
        self.exit_code = Field::Set(v);
        self
    }

    pub fn error(mut self, v: impl Into<String>) -> Self {
        self.error = Field::Set(v.into());
        self
    }

    pub fn error_kind(mut self, v: impl Into<String>) -> Self {
        self.error_kind = Field::Set(v.into());
        self
    }

    pub fn started_at(mut self, v: i64) -> Self {
        self.started_at = Field::Set(v);
        self
    }

    pub fn ended_at(mut self, v: i64) -> Self {
        self.ended_at = Field::Set(v);
        self
    }

    /// Set the unread-event counter. Incremented by the engine when the session reaches
    /// a "your turn" state (DONE or IDLE). Discord-style: unread = unreadEventId > lastAcked.
    pub fn unread_event_id(mut self, v: i64) -> Self {
        self.unread_event_id = Some(v);
        self
    }

    pub fn clear_error(mut self) -> Self {
        self.error = Field::Clear;
        self
    }

    pub fn clear_error_kind(mut self) -> Self {
        self.error_kind = Field::Clear;
        self
    }

    pub fn clear_ended_at(mut self) -> Self {
        self.ended_at = Field::Clear;
        self
    }

    pub fn clear_exit_code(mut self) -> Self {
        self.exit_code = Field::Clear;
        self
    }

    pub fn last_user_message_at(mut self, v: i64) -> Self {
        self.last_user_message_at = Some(v);
        self
    }

    pub fn prompt(mut self, v: impl Into<String>) -> Self {
        self.prompt = Some(v.into());
        self
    }

    pub fn title_pinned(mut self, v: bool) -> Self {
        self.title_pinned = Some(v);
        self
    }

    pub fn auto_resume(mut self, v: bool) -> Self {
        self.auto_resume = Some(v);
        self
    }

    /// Schedule the auto-resume scheduler to re-enqueue this session at `v` (epoch ms).
    pub fn auto_resume_at(mut self, v: i64) -> Self {
        self.auto_resume_at = Field::Set(v);
        self
    }

    pub fn clear_auto_resume_at(mut self) -> Self {
        self.auto_resume_at = Field::Clear;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.status.is_unset()
            && self.claude_session_id.is_unset()
            && self.cost_usd.is_unset()
            && self.exit_code.is_unset()
            && self.error.is_unset()
            && self.error_kind.is_unset()
            && self.started_at.is_unset()
            && self.ended_at.is_unset()
            && self.last_user_message_at.is_none()
            && self.prompt.is_none()
            && self.worktree_state.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && self.mode.is_none()
            && self.permission_mode.is_none()
            && self.title_pinned.is_none()
            && self.group_id.is_none()
            && self.auto_resume.is_none()
            && self.auto_resume_at.is_unset()
    }

    /// Convert this builder to a legacy `SessionPatch` (helper for tests +
    /// any future caller that needs the patch form without going through
    /// the engine's transition method). Most callers should use
    /// `Store::apply_update`, which goes through the conversion + persist
    /// path in one call.
    pub fn into_patch(self) -> SessionPatch {
        Store::session_update_to_patch(&self)
    }
}

impl Store {
    /// Apply a `SessionUpdate` to the row for `id`. No-op if `is_empty()`.
    /// Returns the updated `Session` (post-write read-back) so the caller
    /// can inspect the result without a second round-trip.
    pub async fn apply_update(&self, id: &str, update: SessionUpdate) -> Result<Option<Session>, StoreError> {
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
    ("baseSha", "TEXT"), ("worktreeState", "TEXT DEFAULT 'live'"), ("repos", "TEXT"), ("skills", "TEXT"),
    ("baseShas", "TEXT"), ("model", "TEXT"), ("effort", "TEXT"), ("mode", "TEXT"), ("errorKind", "TEXT"),
    ("lastUserMessageAt", "INTEGER"), ("hiddenSkills", "TEXT"), ("permissionMode", "TEXT"),
    ("parentSessionId", "TEXT"), ("titlePinned", "INTEGER"),
    ("groupId", "TEXT"), ("unreadEventId", "INTEGER DEFAULT 0"), ("ackedEventId", "INTEGER DEFAULT 0"),
    // NULL = default ON for autoResume (existing rows keep the default without a backfill).
    ("autoResume", "INTEGER"), ("autoResumeAt", "INTEGER"),
    // NULL = no plugins disabled (rows written before the column existed keep the default).
    ("hiddenPlugins", "TEXT"),
    // Adopt / detach / re-sync provenance & handoff columns. Added for the
    // "Adopt & Re-sync Claude Code Sessions" feature.
    ("origin", "TEXT DEFAULT 'native'"),
    ("detached", "INTEGER DEFAULT 0"),
    ("nativeWatermarkLines", "INTEGER DEFAULT 0"),
];

use crate::util::now_ms;

fn normalize_mode(m: Option<String>) -> Option<String> {
    match m.as_deref() { Some("ultracode") | Some("ultra") => Some("ultracode".into()), _ => None }
}
fn safe_json_vec(raw: Option<String>, fallback: Vec<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or(fallback)
}

pub struct Store {
    pool: SqlitePool,
    log_dir: PathBuf,
    seq: AtomicI64,
}

impl Store {
    pub async fn open(db_path: impl AsRef<Path>, log_dir: impl AsRef<Path>) -> Result<Store, StoreError> {
        let db_path = db_path.as_ref();
        if let Some(parent) = db_path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::create_dir_all(log_dir.as_ref())?;
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await?;
        sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
        sqlx::query("PRAGMA synchronous = NORMAL").execute(&pool).await?;
        sqlx::query("PRAGMA busy_timeout = 5000").execute(&pool).await?;
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS sessions ({COLUMNS_DDL})")).execute(&pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS groups (id TEXT PRIMARY KEY, name TEXT NOT NULL, icon TEXT, sortOrder INTEGER NOT NULL DEFAULT 0, createdAt INTEGER NOT NULL)").execute(&pool).await?;
        // migrate: add any missing ADDED_COLUMNS, then backfill lastUserMessageAt
        let have: Vec<String> = sqlx::query("PRAGMA table_info(sessions)").fetch_all(&pool).await?
            .iter().map(|r| r.get::<String, _>("name")).collect();
        for (name, decl) in ADDED_COLUMNS {
            if !have.iter().any(|h| h == name) {
                sqlx::query(&format!("ALTER TABLE sessions ADD COLUMN {name} {decl}")).execute(&pool).await?;
            }
        }
        sqlx::query("UPDATE sessions SET lastUserMessageAt = createdAt WHERE lastUserMessageAt IS NULL").execute(&pool).await?;
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
        let cols_now: Vec<String> = sqlx::query("PRAGMA table_info(sessions)").fetch_all(&pool).await?
            .iter().map(|r| r.get::<String, _>("name")).collect();
        if cols_now.iter().any(|c| c == "claudeSessionId") {
            sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_claude_session_id ON sessions(claudeSessionId) WHERE claudeSessionId IS NOT NULL").execute(&pool).await?;
        }
        Ok(Store { pool, log_dir: log_dir.as_ref().to_path_buf(), seq: AtomicI64::new(0) })
    }

    pub fn log_path(&self, id: &str) -> PathBuf { self.log_dir.join(format!("{id}.jsonl")) }

    /// Append a single line (with trailing `\n`) to the session's log file.
    /// The blocking file write runs in `spawn_blocking` so it never blocks the async executor.
    pub async fn append_log(&self, id: &str, line: &str) -> Result<(), StoreError> {
        let path = self.log_path(id);
        let line = line.to_string();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(f, "{line}")?;
            Ok(())
        }).await.map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync (blocking) append — used from non-async contexts like `with_activity`.
    pub fn append_log_blocking(&self, id: &str, line: &str) {
        let path = self.log_path(id);
        let _ = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
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
            input.repo.as_ref().filter(|r| !r.is_empty()).map(|r| vec![r.clone()]).unwrap_or_default()
        };
        // repo: first element of repos, or empty string when no repos (non-optional string on the wire).
        let repo: String = repos.first().cloned().unwrap_or_default();

        // baseShas: if explicitly provided (non-empty), use as-is; otherwise derive.
        // Since CreateInput.base_shas defaults to empty map, "not provided" is approximated as empty
        // with no legacy single-repo field. If base_shas is empty AND input.repo was set,
        // synthesize {repo: base_sha} — e.g. create({repo:'r', baseSha:'x'}) → baseShas={r:x}.
        let base_shas: std::collections::HashMap<String, Option<String>> = if !input.base_shas.is_empty() {
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
        };
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        // Bind repo column as the raw string ("" not NULL for no-repo sessions).
        sqlx::query("INSERT INTO sessions (id,repo,repos,skills,hiddenSkills,hiddenPlugins,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,permissionMode,titlePinned,parentSessionId,seq,groupId,unreadEventId,ackedEventId,origin) \
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&s.id).bind(&s.repo).bind(serde_json::to_string(&s.repos)?).bind(serde_json::to_string(&s.skills)?).bind(serde_json::to_string(&s.hidden_skills)?).bind(serde_json::to_string(&s.hidden_plugins)?)
            .bind(&s.prompt).bind(&s.worktree_path).bind(&s.branch).bind(&s.claude_session_id).bind(&s.status)
            .bind(s.cost_usd).bind(s.exit_code).bind(&s.error).bind(&s.error_kind).bind(s.created_at)
            .bind(s.started_at).bind(s.ended_at).bind(s.last_user_message_at).bind(&base_sha)
            .bind(serde_json::to_string(&s.base_shas)?).bind(&s.worktree_state)
            .bind(&s.model).bind(&s.effort).bind(&s.mode).bind(&input.permission_mode).bind(s.title_pinned).bind(&input.parent_session_id).bind(seq)
            .bind(&input.group_id).bind(s.unread_event_id).bind(s.acked_event_id).bind(&s.origin)
            .execute(&self.pool).await?;
        Ok(s)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
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
        let rows = sqlx::query("SELECT * FROM sessions ORDER BY COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC")
            .fetch_all(&self.pool).await?;
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
            group_id: u.group_id.clone().map(|gid| if gid.is_empty() { Some(None) } else { Some(Some(gid)) }).unwrap_or(None),
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
        macro_rules! col { ($opt:expr, $sql:literal) => { if $opt.is_some() { sets.push($sql); } } }
        col!(patch.status, "status = ?"); col!(patch.claude_session_id, "claudeSessionId = ?");
        col!(patch.cost_usd, "costUsd = ?"); col!(patch.exit_code, "exitCode = ?");
        col!(patch.error, "error = ?"); col!(patch.error_kind, "errorKind = ?");
        col!(patch.started_at, "startedAt = ?"); col!(patch.ended_at, "endedAt = ?");
        col!(patch.last_user_message_at, "lastUserMessageAt = ?"); col!(patch.prompt, "prompt = ?");
        col!(patch.worktree_state, "worktreeState = ?");
        col!(patch.model, "model = ?"); col!(patch.effort, "effort = ?");
        col!(patch.mode, "mode = ?"); col!(patch.permission_mode, "permissionMode = ?");
        col!(patch.title_pinned, "titlePinned = ?");
        col!(patch.group_id, "groupId = ?");
        col!(patch.unread_event_id, "unreadEventId = ?");
        col!(patch.auto_resume, "autoResume = ?");
        col!(patch.auto_resume_at, "autoResumeAt = ?");
        if sets.is_empty() { return Ok(()); }
        let sql = format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = &patch.status { q = q.bind(v); }
        if let Some(v) = &patch.claude_session_id { q = q.bind(v); }
        if let Some(v) = &patch.cost_usd { q = q.bind(v); }
        if let Some(v) = &patch.exit_code { q = q.bind(v); }
        if let Some(v) = &patch.error { q = q.bind(v); }
        if let Some(v) = &patch.error_kind { q = q.bind(v); }
        if let Some(v) = &patch.started_at { q = q.bind(v); }
        if let Some(v) = &patch.ended_at { q = q.bind(v); }
        if let Some(v) = &patch.last_user_message_at { q = q.bind(v); }
        if let Some(v) = &patch.prompt { q = q.bind(v); }
        if let Some(v) = &patch.worktree_state { q = q.bind(v); }
        if let Some(v) = &patch.model { q = q.bind(v); }
        if let Some(v) = &patch.effort { q = q.bind(v); }
        if let Some(v) = &patch.mode { q = q.bind(v); }
        if let Some(v) = &patch.permission_mode { q = q.bind(v); }
        if let Some(v) = &patch.title_pinned { q = q.bind(v); }
        if let Some(v) = &patch.group_id { q = q.bind(v); }
        if let Some(v) = patch.unread_event_id { q = q.bind(v); }
        if let Some(v) = patch.auto_resume { q = q.bind(v); }
        if let Some(v) = &patch.auto_resume_at { q = q.bind(v); }
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
        sqlx::query("UPDATE sessions SET unreadEventId = COALESCE(unreadEventId, 0) + 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Discord-style: ack session event. Sets ackedEventId = MAX(ackedEventId, eid).
    pub async fn ack_event(&self, id: &str, eid: i64) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET ackedEventId = MAX(COALESCE(ackedEventId, 0), ?) WHERE id = ?")
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
        sqlx::query("DELETE FROM sessions WHERE id = ?").bind(id).execute(&self.pool).await?;
        let _ = std::fs::remove_file(self.log_path(id)); // best-effort
        Ok(())
    }

    /// Read the session log file → Vec of non-empty lines ([] if missing).
    pub fn read_log(&self, id: &str) -> Vec<String> {
        match std::fs::read_to_string(self.log_path(id)) {
            Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ── Groups table CRUD ──────────────────────────────────────────────────

    pub async fn list_groups(&self) -> Result<Vec<Group>, StoreError> {
        let rows = sqlx::query("SELECT * FROM groups ORDER BY sortOrder ASC, createdAt ASC")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Group {
            id: r.try_get("id").unwrap_or_default(),
            name: r.try_get("name").unwrap_or_default(),
            icon: r.try_get("icon").ok().flatten(),
            sort_order: r.try_get("sortOrder").unwrap_or(0),
            created_at: r.try_get("createdAt").unwrap_or(0),
        }).collect())
    }

    pub async fn get_group(&self, id: &str) -> Result<Option<Group>, StoreError> {
        let row = sqlx::query("SELECT * FROM groups WHERE id = ?").bind(id)
            .fetch_optional(&self.pool).await?;
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
            .bind(&id).bind(name).bind(icon).bind(sort_order).bind(now)
            .execute(&self.pool).await?;
        Ok(Group { id, name: name.to_string(), icon: icon.map(|s| s.to_string()), sort_order, created_at: now })
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

    pub async fn update_group(&self, id: &str, name: Option<&str>, icon: Option<&str>) -> Result<Option<Group>, StoreError> {
        let existing = self.get_group(id).await?;
        let Some(_) = existing else { return Ok(None); };
        let mut sets: Vec<&str> = Vec::new();
        if name.is_some() { sets.push("name = ?"); }
        if icon.is_some() { sets.push("icon = ?"); }
        if sets.is_empty() { return self.get_group(id).await; }
        let sql = format!("UPDATE groups SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = name { q = q.bind(v); }
        if let Some(v) = icon { q = q.bind(v); }
        q.bind(id).execute(&self.pool).await?;
        self.get_group(id).await
    }

    /// Delete a group and set all sessions referencing it to NULL (uncategorized).
    pub async fn delete_group(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET groupId = NULL WHERE groupId = ?").bind(id)
            .execute(&self.pool).await?;
        sqlx::query("DELETE FROM groups WHERE id = ?").bind(id)
            .execute(&self.pool).await?;
        Ok(())
    }
}

/// Convert a SQLite row to a `Session`, applying all column fallbacks.
fn row_to_session(r: &SqliteRow) -> Session {
    // repo: raw column value — exposed as a non-optional String (empty string when no repo).
    let repo_raw: String = r.try_get::<Option<String>, _>("repo").ok().flatten().unwrap_or_default();
    // repo_truthy: non-empty repo for fallback computations (empty string treated as absent).
    let repo_truthy: Option<&str> = if repo_raw.is_empty() { None } else { Some(&repo_raw) };
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
        safe_json_vec(raw, repo_truthy.map(|r| vec![r.to_string()]).unwrap_or_default())
    };

    // baseSha: prefer column value; fall back to baseShas[repos[0]] when column is NULL.
    let base_sha: Option<String> = base_sha_col.clone().or_else(|| {
        repos.first().and_then(|r0| base_shas.get(r0).cloned().flatten())
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
        unread_event_id: r.try_get::<Option<i64>, _>("unreadEventId").ok().flatten().unwrap_or(0),
            acked_event_id: r.try_get::<Option<i64>, _>("ackedEventId").ok().flatten().unwrap_or(0),
        last_user_message_at: r.try_get::<Option<i64>, _>("lastUserMessageAt").ok().flatten().unwrap_or(created_at),
        base_sha,
        base_shas,
        worktree_state: r.try_get::<Option<String>, _>("worktreeState").ok().flatten().unwrap_or_else(|| "live".into()),
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
        origin: r.try_get::<Option<String>, _>("origin").ok().flatten().unwrap_or_else(|| "native".into()),
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
    }
}

#[cfg(test)]
mod tests {
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
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let s = store.create(CreateInput { id: "s1".into(), repos: vec!["demo".into()], prompt: "do x".into(), ..Default::default() }).await.unwrap();
        assert_eq!(s.last_user_message_at, s.created_at);
        let got = store.get("s1").await.unwrap().unwrap();
        assert_eq!(got.id, "s1");
        assert_eq!(got.repos, vec!["demo".to_string()]);
        assert_eq!(got.status, "pending");
    }

    #[tokio::test]
    async fn list_orders_by_last_user_message_at_desc() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let a = store.create(CreateInput { id: "a".into(), prompt: "a".into(), ..Default::default() }).await.unwrap();
        store.create(CreateInput { id: "b".into(), prompt: "b".into(), ..Default::default() }).await.unwrap();
        // default: newest-created first (b before a)
        assert_eq!(store.list().await.unwrap().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["b", "a"]);
        // bump a past b
        store.update("a", SessionPatch { last_user_message_at: Some(a.created_at + 10_000), ..Default::default() }).await.unwrap();
        assert_eq!(store.list().await.unwrap().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn opens_a_legacy_db_and_backfills_last_user_message_at() {
        // Simulate a DB created before lastUserMessageAt was added: sessions table without that column.
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
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
                .connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
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
            store.create(CreateInput {
                id: "s1".into(), prompt: "p".into(), repos: vec!["demo".into()],
                model: Some("opus".into()), mode: Some("ultra".into()),
                base_shas: std::collections::HashMap::from([("demo".into(), Some("abc".into()))]),
                ..Default::default()
            }).await.unwrap();
            let s = store.get("s1").await.unwrap().unwrap();
            assert_eq!(s.repos, vec!["demo".to_string()]);
            assert_eq!(s.model.as_deref(), Some("opus"));
            assert_eq!(s.base_shas.get("demo"), Some(&Some("abc".to_string())));
        }
        // Re-open the SAME path: migration must be idempotent (re-running the ALTERs is a no-op,
        // not an error) and prior rows survive; a new create still works.
        let store2 = Store::open(path, dir.join("logs")).await.unwrap();
        assert!(store2.get("s1").await.unwrap().is_some(), "row must survive reopen");
        store2.create(CreateInput { id: "s2".into(), prompt: "q".into(), ..Default::default() })
            .await.unwrap();
        assert!(store2.get("s2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn append_log_writes_one_line_per_call() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.append_log("s1", "{\"type\":\"x\"}").await.unwrap();
        store.append_log("s1", "{\"type\":\"y\"}").await.unwrap();
        let content = std::fs::read_to_string(store.log_path("s1")).unwrap();
        assert_eq!(content, "{\"type\":\"x\"}\n{\"type\":\"y\"}\n");
    }

    /// Fix #5: `create()` stores raw mode; `get()` normalizes on read.
    #[tokio::test]
    async fn create_stores_raw_mode_get_normalizes() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        // "ultra" is stored raw in the returned Session from create(); but the
        // Session returned from create() has mode = raw (no normalization at write time).
        let created = store.create(CreateInput {
            id: "m1".into(), prompt: "p".into(), mode: Some("ultra".into()), ..Default::default()
        }).await.unwrap();
        // create() returns the raw mode.
        assert_eq!(created.mode.as_deref(), Some("ultra"), "create() must return raw mode");
        // get() normalizes on read.
        let got = store.get("m1").await.unwrap().unwrap();
        assert_eq!(got.mode.as_deref(), Some("ultracode"), "get() must normalize mode on read");
    }

    /// Fix #3/#6: a row with baseShas column NULL but repo+baseSha set reconstructs
    /// base_shas = {repo: baseSha} and base_sha = baseSha.
    #[tokio::test]
    async fn row_to_session_base_shas_fallback_from_repo_and_base_sha() {
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
            // Create minimal table (no baseShas column yet, simulating legacy row).
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, baseSha TEXT, repos TEXT, \
                worktreeState TEXT, lastUserMessageAt INTEGER)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,baseSha,lastUserMessageAt) \
                VALUES ('r1','pending','p',1000,0,'myrepo','abc123',1000)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let store = Store::open(path, dir.join("logs")).await.unwrap();
        let s = store.get("r1").await.unwrap().unwrap();
        // baseShas fallback: {repo: baseSha}
        assert_eq!(s.base_shas.get("myrepo").and_then(|v| v.as_deref()), Some("abc123"),
            "base_shas must be reconstructed from repo+baseSha when baseShas column is null");
        // base_sha fallback: baseSha column value
        assert_eq!(s.base_sha.as_deref(), Some("abc123"),
            "base_sha must be set from baseSha column");
    }

    /// Fix #4: repos fallback — empty-string repo → [] not [""]
    #[tokio::test]
    async fn repos_fallback_empty_repo_yields_empty_vec() {
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, lastUserMessageAt INTEGER)").execute(&pool).await.unwrap();
            // repo = "" (empty string, treated as absent)
            sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,lastUserMessageAt) \
                VALUES ('r2','pending','p',1000,0,'',1000)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let store = Store::open(path, dir.join("logs")).await.unwrap();
        let s = store.get("r2").await.unwrap().unwrap();
        assert_eq!(s.repos, Vec::<String>::new(),
            "empty-string repo must yield empty repos vec (empty string is treated as absent)");
    }

    /// Multi-repo create round-trip: create({repos:['A','B'], baseShas:{A:'sa',B:'sb'}}) must
    /// persist baseSha = baseShas['A'] (repos[0]) and base_shas round-trips intact.
    /// Multi-repo create round-trip: stores repos/skills/baseShas and backfills old single-repo rows.
    #[tokio::test]
    async fn create_multi_repo_baseshas_roundtrip() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let mut base_shas = std::collections::HashMap::new();
        base_shas.insert("A".to_string(), Some("sa".to_string()));
        base_shas.insert("B".to_string(), Some("sb".to_string()));
        let created = store.create(CreateInput {
            id: "multi1".into(),
            prompt: "p".into(),
            repos: vec!["A".into(), "B".into()],
            base_shas: base_shas.clone(),
            ..Default::default()
        }).await.unwrap();
        // create() must derive base_sha = baseShas[repos[0]] = baseShas["A"] = "sa"
        assert_eq!(created.base_sha.as_deref(), Some("sa"),
            "create() must set base_sha = baseShas[repos[0]]");
        assert_eq!(created.repo.as_str(), "A",
            "create() must set repo = repos[0]");
        assert_eq!(created.base_shas.get("A").and_then(|v| v.as_deref()), Some("sa"),
            "create() base_shas must contain A=sa");
        assert_eq!(created.base_shas.get("B").and_then(|v| v.as_deref()), Some("sb"),
            "create() base_shas must contain B=sb");

        // get() must return the same values after persisting.
        let got = store.get("multi1").await.unwrap().unwrap();
        assert_eq!(got.base_sha.as_deref(), Some("sa"),
            "get() must return persisted base_sha = baseShas[repos[0]]");
        assert_eq!(got.repo.as_str(), "A",
            "get() must return repo = repos[0]");
        assert_eq!(got.repos, vec!["A".to_string(), "B".to_string()],
            "get() must return repos round-tripped");
        assert_eq!(got.base_shas.get("A").and_then(|v| v.as_deref()), Some("sa"),
            "get() base_shas must contain A=sa");
        assert_eq!(got.base_shas.get("B").and_then(|v| v.as_deref()), Some("sb"),
            "get() base_shas must contain B=sb");
    }

    /// Legacy single-repo create: create({repo:'r', baseSha:'x'}) must persist
    /// baseShas = {"r":"x"} so get() returns base_shas = {r:'x'}.
    #[tokio::test]
    async fn create_legacy_single_repo_baseshas_roundtrip() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let created = store.create(CreateInput {
            id: "legacy1".into(),
            prompt: "p".into(),
            repo: Some("r".into()),
            base_sha: Some("x".into()),
            ..Default::default()
        }).await.unwrap();
        // create() must synthesize base_shas = {r: x} from legacy form
        assert_eq!(created.base_shas.get("r").and_then(|v| v.as_deref()), Some("x"),
            "create() must synthesize base_shas = {{repo: baseSha}} from legacy form");
        assert_eq!(created.base_sha.as_deref(), Some("x"),
            "create() must set base_sha = baseShas[repo] = x");
        assert_eq!(created.repo.as_str(), "r",
            "create() must set repo = repos[0]");

        // get() must round-trip correctly (not return {} for base_shas)
        let got = store.get("legacy1").await.unwrap().unwrap();
        assert_eq!(got.base_shas.get("r").and_then(|v| v.as_deref()), Some("x"),
            "get() must return base_shas = {{r: x}} for legacy single-repo create");
        assert_eq!(got.base_sha.as_deref(), Some("x"),
            "get() must return base_sha = x");
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
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
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
        assert_eq!(s.repos, Vec::<String>::new(), "corrupt repos must degrade to []");
        assert_eq!(s.skills, Vec::<String>::new(), "corrupt skills must degrade to []");
        assert!(s.base_shas.is_empty(), "corrupt baseShas must degrade to {{}}");
        // list() must not panic and must return all rows.
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2, "list() must return all rows even with corrupt JSON");
    }

    #[tokio::test]
    async fn remove_deletes_row_and_log() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.create(CreateInput { id: "rm1".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
        store.append_log("rm1", "{\"type\":\"x\"}").await.unwrap();
        assert!(store.get("rm1").await.unwrap().is_some());
        store.remove("rm1").await.unwrap();
        assert!(store.get("rm1").await.unwrap().is_none(), "row deleted");
        assert!(store.read_log("rm1").is_empty(), "log read returns [] after remove");
        // idempotent
        store.remove("rm1").await.unwrap();
    }

    #[tokio::test]
    async fn read_log_returns_nonempty_lines_in_order() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        assert_eq!(store.read_log("missing"), Vec::<String>::new(), "missing log → []");
        store.append_log("L", "{\"type\":\"a\"}").await.unwrap();
        store.append_log("L", "{\"type\":\"b\"}").await.unwrap();
        assert_eq!(store.read_log("L"), vec!["{\"type\":\"a\"}".to_string(), "{\"type\":\"b\"}".to_string()]);
    }

    #[tokio::test]
    async fn update_can_set_worktree_state() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.create(CreateInput { id: "ws".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
        store.update("ws", SessionPatch { worktree_state: Some("discarded".into()), ..Default::default() }).await.unwrap();
        assert_eq!(store.get("ws").await.unwrap().unwrap().worktree_state, "discarded");
    }

    #[test]
    fn session_runtime_fields_skip_when_none() {
        let s = Session { id: "x".into(), status: "done".into(), worktree_state: "live".into(), ..Default::default() };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("activity").is_none() && v.get("awaitingInput").is_none() && v.get("workflowRunning").is_none(),
            "runtime fields omitted when None (optional props)");
    }

    /// Session.repo is a non-optional String: create() with no repo yields repo="", not null.
    #[tokio::test]
    async fn session_repo_is_non_optional_string() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        // No repo: must serialize as "" not null.
        let s = store.create(CreateInput {
            id: "norepo".into(), prompt: "p".into(), ..Default::default()
        }).await.unwrap();
        assert_eq!(s.repo, "", "Session.repo must be empty string (not null) when no repo");

        // With repo: must serialize as the repo string.
        let s2 = store.create(CreateInput {
            id: "withrepo".into(), prompt: "p".into(),
            repos: vec!["demo".into()], ..Default::default()
        }).await.unwrap();
        assert_eq!(s2.repo, "demo", "Session.repo must equal repos[0]");

        // get() must round-trip the same values.
        let got = store.get("norepo").await.unwrap().unwrap();
        assert_eq!(got.repo, "", "get() must return repo='' for no-repo session");
        let got2 = store.get("withrepo").await.unwrap().unwrap();
        assert_eq!(got2.repo, "demo", "get() must return repo='demo'");
    }

    /// update() must touch ONLY the Some() columns and leave every other column intact,
    /// including the ability to write SQL NULL via Some(None) (the Option<Option<T>> nullify path).
    #[tokio::test]
    async fn update_only_some_fields_others_untouched_and_nullify() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.create(CreateInput {
            id: "u1".into(), prompt: "orig".into(), repos: vec!["demo".into()],
            model: Some("opus".into()), ..Default::default()
        }).await.unwrap();
        // Set several optional fields to concrete values in one update.
        store.update("u1", SessionPatch {
            status: Some("running".into()),
            cost_usd: Some(Some(1.5)),
            exit_code: Some(Some(0)),
            error: Some(Some("boom".into())),
            started_at: Some(Some(7777)),
            ..Default::default()
        }).await.unwrap();
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
        store.update("u1", SessionPatch {
            prompt: Some("changed".into()),
            error: Some(None),       // nullify
            cost_usd: Some(None),    // nullify
            ..Default::default()
        }).await.unwrap();
        let s2 = store.get("u1").await.unwrap().unwrap();
        assert_eq!(s2.prompt, "changed");
        assert_eq!(s2.error, None, "Some(None) must clear error to SQL NULL");
        assert_eq!(s2.cost_usd, None, "Some(None) must clear costUsd to SQL NULL");
        // Fields not in the second patch keep their first-update values.
        assert_eq!(s2.status, "running", "status must survive a prompt-only update");
        assert_eq!(s2.exit_code, Some(0), "exitCode must survive a prompt-only update");
        assert_eq!(s2.started_at, Some(7777), "startedAt must survive a prompt-only update");
    }


    /// An empty SessionPatch (all None) must be a no-op: it must not error and must not
    /// alter any column. Guards the `if sets.is_empty() { return Ok(()) }` early-return.
    #[tokio::test]
    async fn update_empty_patch_is_noop() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.create(CreateInput { id: "noop".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
        let before = store.get("noop").await.unwrap().unwrap();
        store.update("noop", SessionPatch::default()).await.unwrap();
        let after = store.get("noop").await.unwrap().unwrap();
        assert_eq!(after.prompt, before.prompt);
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_user_message_at, before.last_user_message_at);
        // update() of a non-existent id is also a silent no-op (UPDATE matches 0 rows).
        store.update("ghost", SessionPatch { status: Some("x".into()), ..Default::default() }).await.unwrap();
        assert!(store.get("ghost").await.unwrap().is_none());
    }


    /// list() ordering tiebreak: when lastUserMessageAt is equal across rows, ordering falls
    /// back to `seq DESC` — i.e. the most-recently-created row sorts first. seq is monotonic.
    #[tokio::test]
    async fn list_tiebreaks_equal_timestamps_by_seq_desc() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        // Create three rows, then force identical lastUserMessageAt so only seq distinguishes them.
        for id in ["t0", "t1", "t2"] {
            store.create(CreateInput { id: id.into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
            store.update(id, SessionPatch { last_user_message_at: Some(5000), ..Default::default() }).await.unwrap();
        }
        // Equal timestamps → seq DESC → newest-created (t2) first, oldest (t0) last.
        let ids: Vec<String> = store.list().await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["t2", "t1", "t0"],
            "equal lastUserMessageAt must tiebreak by seq DESC (newest create first)");
    }


    /// read_log must skip blank / whitespace-only lines (not just be empty on a missing file),
    /// returning only the meaningful JSONL lines in order.
    #[tokio::test]
    async fn read_log_skips_blank_and_whitespace_lines() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        // Write a file directly containing blank lines and whitespace-only lines between entries.
        std::fs::write(store.log_path("blanks"), "{\"a\":1}\n\n   \n{\"b\":2}\n\t\n").unwrap();
        assert_eq!(store.read_log("blanks"),
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()],
            "blank and whitespace-only lines must be filtered out");
        // An empty file (created by append never called, or truncated) reads as [].
        std::fs::write(store.log_path("empty"), "").unwrap();
        assert_eq!(store.read_log("empty"), Vec::<String>::new(), "empty file → []");
    }


    /// normalize_mode edges (read-side): only "ultra"/"ultracode" map to "ultracode"; any other
    /// stored value (including a bogus mode or NULL) reads back as None.
    #[tokio::test]
    async fn mode_normalization_edges_unknown_becomes_none() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.create(CreateInput { id: "mw".into(), prompt: "p".into(), mode: Some("weird".into()), ..Default::default() }).await.unwrap();
        assert_eq!(store.get("mw").await.unwrap().unwrap().mode, None,
            "unknown stored mode must normalize to None on read");
        store.create(CreateInput { id: "muc".into(), prompt: "p".into(), mode: Some("ultracode".into()), ..Default::default() }).await.unwrap();
        assert_eq!(store.get("muc").await.unwrap().unwrap().mode.as_deref(), Some("ultracode"),
            "already-normalized 'ultracode' stays 'ultracode'");
        store.create(CreateInput { id: "mnone".into(), prompt: "p".into(), ..Default::default() }).await.unwrap();
        assert_eq!(store.get("mnone").await.unwrap().unwrap().mode, None,
            "absent mode reads back as None");
    }


    /// Concurrent create() on a shared Arc<Store>: spawned tasks racing on the single-connection
    /// pool + AtomicI64 seq must all succeed, produce distinct rows, and yield strictly distinct
    /// monotonic seq values (so the list() tiebreak stays well-defined). No panics / lost rows.
    #[tokio::test]
    async fn concurrent_create_all_rows_persist_with_distinct_seq() {
        let dir = tmp();
        let store = std::sync::Arc::new(Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap());
        let mut handles = Vec::new();
        for i in 0..16 {
            let st = store.clone();
            handles.push(tokio::spawn(async move {
                st.create(CreateInput { id: format!("c{i}"), prompt: "p".into(), ..Default::default() }).await.unwrap();
            }));
        }
        for h in handles { h.await.unwrap(); }
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
            .fetch_all(&store.pool).await.unwrap()
            .iter().map(|r| r.get::<i64, _>("seq")).collect();
        seqs.sort();
        assert_eq!(seqs, (0..16).collect::<Vec<i64>>(),
            "16 concurrent creates must consume a distinct contiguous seq range 0..16");
    }

    /// Adopted session round-trip: `origin` is persisted at create time,
    /// `nativeWatermarkLines` and `detached` are updated via the dedicated
    /// setters and read back through `get()`.
    #[tokio::test]
    async fn adopted_origin_and_watermark_round_trip() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let id = "sess-adopt-1";
        store.create(CreateInput {
            id: id.into(),
            prompt: "p".into(),
            origin: Some("adopted".into()),
            ..Default::default()
        }).await.unwrap();
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
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        // Adopted name — stable id, idempotent.
        let a = store.ensure_group("Claude Code Adopted").await.unwrap();
        let b = store.ensure_group("Claude Code Adopted").await.unwrap();
        assert_eq!(a, b, "two calls for the same name must return the same id");
        assert_eq!(a, "grp-adopted",
            "the 'Claude Code Adopted' group must use the stable id 'grp-adopted'");
        assert_eq!(
            store.list_groups().await.unwrap()
                .iter().filter(|g| g.name == "Claude Code Adopted").count(),
            1,
            "only one 'Claude Code Adopted' row must exist after repeat calls"
        );

        // Non-adopted name — fresh uuid-prefixed id, also idempotent.
        let c1 = store.ensure_group("Other Group").await.unwrap();
        let c2 = store.ensure_group("Other Group").await.unwrap();
        assert_eq!(c1, c2, "non-adopted name is also idempotent");
        assert!(c1.starts_with("grp-") && c1 != "grp-adopted",
            "non-adopted name must get a deterministic grp-<hash> id, got {c1}");
    }

    /// Non-adopted names must get a stable id derived from the name (not a
    /// fresh uuid each call), so two sequential calls return the SAME id and
    /// `list_groups` shows exactly one row — this is what makes
    /// concurrent inserters race-safe on PRIMARY KEY id instead of
    /// accidentally producing duplicate `groups` rows.
    #[tokio::test]
    async fn ensure_group_same_name_is_stable_id() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let first = store.ensure_group("My Group").await.unwrap();
        let second = store.ensure_group("My Group").await.unwrap();
        assert_eq!(first, second,
            "two sequential calls for the same non-adopted name must return the same id");
        assert!(first.starts_with("grp-") && first != "grp-adopted",
            "non-adopted name must get a deterministic grp-<hash> id, got {first}");
        let rows: Vec<_> = store.list_groups().await.unwrap()
            .into_iter().filter(|g| g.name == "My Group").collect();
        assert_eq!(rows.len(), 1,
            "exactly one row must exist for the non-adopted name, got {} ({:?})",
            rows.len(), rows.iter().map(|g| &g.id).collect::<Vec<_>>());
        assert_eq!(rows[0].id, first,
            "the single surviving row must carry the deterministic id");
    }

    #[tokio::test]
    async fn parent_session_id_round_trip() {
        let dir = tmp();
        let store = Store::open(dir.join("s.db"), dir.join("logs")).await.unwrap();
        store.create(CreateInput {
            id: "parent".into(),
            prompt: "p".into(),
            ..Default::default()
        }).await.unwrap();
        store.create(CreateInput {
            id: "child".into(),
            prompt: "c".into(),
            parent_session_id: Some("parent".into()),
            ..Default::default()
        }).await.unwrap();

        let c = store.get("child").await.unwrap().unwrap();
        assert_eq!(c.parent_session_id.as_deref(), Some("parent"));

        let p = store.get("parent").await.unwrap().unwrap();
        assert_eq!(p.parent_session_id, None);

        let kids = store.list_children("parent").await.unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, "child");
    }
}
