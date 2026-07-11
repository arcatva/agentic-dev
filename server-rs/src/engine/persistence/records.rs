use serde::{Deserialize, Serialize};

/// MCP server definition for per-session ad-hoc injection.
/// Supports stdio (`command`+`args`+`env`) and http/sse (`url`+`type`+`headers`) transports.
/// Serde uses the field names verbatim (camelCase where needed via rename).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct McpServerDef {
    pub name: String,
    // stdio transport:
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    // http/sse transport:
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
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
    #[serde(rename = "hiddenSkills", default)]
    pub hidden_skills: Vec<String>,
    /// Plugins (`<plugin>@<marketplace>` ids) the user chose to DISABLE for this session
    /// (blacklist). Mapped to claude `enabledPlugins:{<id>:false}` in the `--settings` flag
    /// at spawn time.
    #[serde(rename = "hiddenPlugins", default)]
    pub hidden_plugins: Vec<String>,
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
    #[serde(rename = "worktreePath")]
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    #[serde(rename = "claudeSessionId")]
    pub claude_session_id: Option<String>,
    pub status: String,
    #[serde(rename = "costUsd")]
    pub cost_usd: Option<f64>,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    #[serde(rename = "errorKind")]
    pub error_kind: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "startedAt")]
    pub started_at: Option<i64>,
    #[serde(rename = "endedAt")]
    pub ended_at: Option<i64>,
    #[serde(rename = "lastUserMessageAt")]
    pub last_user_message_at: i64,
    #[serde(rename = "baseSha")]
    pub base_sha: Option<String>,
    #[serde(rename = "baseShas")]
    pub base_shas: std::collections::HashMap<String, Option<String>>,
    #[serde(rename = "worktreeState")]
    pub worktree_state: String,
    // Runtime-only fields set by the engine's with_activity(); not persisted to DB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
    #[serde(rename = "awaitingInput", skip_serializing_if = "Option::is_none")]
    pub awaiting_input: Option<bool>,
    #[serde(rename = "workflowRunning", skip_serializing_if = "Option::is_none")]
    pub workflow_running: Option<bool>,
    /// Runtime-only: the wire payload of the prompt this session is currently PARKED on (an
    /// AskUserQuestion / perm / plan awaiting the user), or None. Set by the engine's with_activity()
    /// from EngineState.parked; never persisted. Lets the client render the awaiting card from an
    /// authoritative source instead of inferring it from the log.
    #[serde(rename = "pendingPrompt", skip_serializing_if = "Option::is_none")]
    pub pending_prompt: Option<serde_json::Value>,
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
    #[serde(default = "default_origin")]
    pub origin: String,
    /// `true` while this session has been handed off to a terminal `claude` and the
    /// server is paused. Used as a single-writer guard and as the reconcile trigger:
    /// when `detached == true` on a follow-up / reopen path, the engine imports any
    /// native-transcript delta into `#1` and clears the flag. Column `detached`
    /// (INTEGER DEFAULT 0 — SQLite has no native bool, stored as 0/1).
    #[serde(default)]
    pub detached: bool,
    /// Line-count of the native Claude transcript (#2) already reflected in this
    /// session's rendered log (#1). Updated by adopt (full import) and reconcile
    /// (delta import). Column `nativeWatermarkLines` (INTEGER DEFAULT 0).
    #[serde(rename = "nativeWatermarkLines", default)]
    pub native_watermark_lines: i64,
    /// MCP server names to hide for this session (blacklist). Bridge writes them to
    /// `settings.disabledMcpjsonServers` and skips injecting any matching `extra_mcp_servers`.
    #[serde(rename = "hiddenMcpServers", default)]
    pub hidden_mcp_servers: Vec<String>,
    /// Extra MCP servers to inject for this session only (not persisted globally).
    /// Serialized as JSON array to `extraMcpServers` DB column; forwarded to bridge as
    /// `SDK_BRIDGE_EXTRA_MCP` env (hidden names removed).
    #[serde(rename = "extraMcpServers", default)]
    pub extra_mcp_servers: Vec<McpServerDef>,
    /// Plugin ids forced ON for this session (overrides a global-off). Precedence:
    /// forcedOn > hidden > global inherit.
    #[serde(rename = "forcedOnPlugins", default)]
    pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session (overrides a global-off).
    #[serde(rename = "forcedOnSkills", default)]
    pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON for this session (overrides a global-off). A globally
    /// DISABLED server (parked in .claude.json's mcpServersDisabled) named here gets its
    /// definition injected back via the extra-defs channel at spawn. Precedence:
    /// forcedOn > hidden > global inherit (same as plugins/skills).
    #[serde(rename = "forcedOnMcpServers", default)]
    pub forced_on_mcp_servers: Vec<String>,
}

/// Serde default for `Session::origin` — keeps the wire shape `"native"` for
/// legacy rows that predate the column (and any row written before `origin`
/// was set explicitly).
fn default_origin() -> String {
    "native".into()
}

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
    pub hidden_mcp_servers: Vec<String>,
    pub extra_mcp_servers: Vec<McpServerDef>,
    pub forced_on_plugins: Vec<String>,
    pub forced_on_skills: Vec<String>,
    pub forced_on_mcp_servers: Vec<String>,
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
    pub unread_event_id: Option<i64>, // None = don't touch; Some = set to this value
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

    /// Convert a builder-style `SessionUpdate` into the legacy `SessionPatch`
    /// so the existing `Store::update(&SessionPatch)` path can persist it.
    /// Unset fields are dropped; Set fields become `Some(v)`; Clear fields
    /// become `Some(None)` (the SQL-NULL write sentinel).
    pub fn to_patch(&self) -> SessionPatch {
        SessionPatch {
            status: match &self.status {
                Field::Set(s) => Some(s.as_str().to_string()),
                _ => None,
            },
            claude_session_id: match &self.claude_session_id {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            cost_usd: match &self.cost_usd {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            exit_code: match &self.exit_code {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            error: match &self.error {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            error_kind: match &self.error_kind {
                Field::Set(v) => Some(Some(v.clone())),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            started_at: match &self.started_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            ended_at: match &self.ended_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
            last_user_message_at: self.last_user_message_at,
            prompt: self.prompt.clone(),
            worktree_state: self.worktree_state.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            mode: self.mode.clone(),
            permission_mode: self.permission_mode.clone(),
            title_pinned: self.title_pinned,
            group_id: self
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
            unread_event_id: self.unread_event_id,
            auto_resume: self.auto_resume,
            auto_resume_at: match &self.auto_resume_at {
                Field::Set(v) => Some(Some(*v)),
                Field::Clear => Some(None),
                Field::Unset => None,
            },
        }
    }

    /// Convert this builder to a legacy `SessionPatch` (helper for tests +
    /// any future caller that needs the patch form without going through
    /// the engine's transition method). Most callers should use
    /// `Store::apply_update`, which goes through the conversion + persist
    /// path in one call.
    pub fn into_patch(self) -> SessionPatch {
        self.to_patch()
    }
}
