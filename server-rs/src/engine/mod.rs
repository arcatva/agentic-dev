use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

// The engine's cross-module working vocabulary, re-exported `pub(crate)` so the method clusters
// split into engine::session::* and the in-crate tests resolve these via `use crate::engine::*` /
// `use super::*` (they were previously reachable because the impl lived in this file).
// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
pub(crate) use crate::engine::classify_error::classify_claude_error;
pub(crate) use crate::engine::repos::ensure_local;
pub(crate) use crate::engine::runner::Runner;
pub(crate) use crate::engine::spawner::{
    compose_user_text, encode_user_message, spawn_claude, SpawnOptions,
};
pub(crate) use crate::engine::status::SessionStatus;
pub(crate) use crate::engine::store::{
    Activity, CreateInput, Field, Session, SessionPatch, SessionUpdate, Store, StoreError,
};
pub(crate) use crate::engine::stream::ClaudeEvent;
pub(crate) use crate::engine::title_client::TitleGenerator;
pub(crate) use crate::engine::transcript::TranscriptCache;
pub(crate) use crate::engine::transition::TransitionReason;
pub(crate) use crate::engine::worktree::{create_session_worktrees, sync_worktree};

// ──────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────

/// Truncate a string to at most `n` Unicode scalar values (characters), not bytes.
/// This avoids a panic from `&s[..n]` when a multi-byte character straddles byte `n`.
fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Build the agentic_prompt JSON event used in three places in the engine.
fn prompt_event_json(text: &str, at: i64) -> serde_json::Value {
    serde_json::json!({"type":"agentic_prompt","text":text,"at":at})
}

/// Number of user turns between periodic retitle attempts. The retitle fires
/// when the persisted `agentic_prompt` count hits a multiple of this.
const RETITLE_EVERY_TURNS: usize = 5;

/// Build the text actually written to claude for a turn: the delivered prompt, with the fork
/// seed context prepended when `item.context_prefix` is set — used by the spawn
/// path to deliver a mention-expanded prompt while keeping the fork seed context (which is a
/// transcript and may itself quote `@session:` tokens) out of the expansion pass.
pub(crate) fn compose_turn_text_with(item: &QueueItem, prompt: &str) -> String {
    match item.context_prefix.as_deref() {
        Some(ctx) if !ctx.is_empty() => format!("{ctx}\n\n---\n\n{prompt}"),
        _ => prompt.to_string(),
    }
}

/// Scan a slice of freshly-appended #1 JSONL lines (the output of
/// `native_transcript::translate_lines`) and return the maximum `at` (epoch ms) across
/// every `agentic_prompt` line. Returns `None` when there are no user-authored lines
/// (e.g. the delta is purely assistant turns) — used by `reconcile_from_native` to
/// decide whether `last_user_message_at` should be bumped.
fn max_agentic_prompt_at(lines: &[String]) -> Option<i64> {
    let mut max_at: Option<i64> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("agentic_prompt") {
            continue;
        }
        if let Some(at) = v.get("at").and_then(|a| a.as_i64()) {
            max_at = Some(max_at.map_or(at, |cur| cur.max(at)));
        }
    }
    max_at
}

// ──────────────────────────────────────────────────────────────
// Timing constants
// ──────────────────────────────────────────────────────────────
pub const WATCHDOG_TICK_MS: u64 = 30_000;
pub const DEFAULT_IDLE_MAX_MS: u64 = 20 * 60 * 1000; // 1_200_000 — "20 min"
                                                     // Wall time has no default cap — it is opt-in via AGENTIC_TURN_WALL_SEC (unlimited otherwise).

// ──────────────────────────────────────────────────────────────
// Injectable function types
// ──────────────────────────────────────────────────────────────
pub type Subscriber = Box<dyn Fn(&ClaudeEvent) + Send + Sync>;
pub type CloneFn = Arc<dyn Fn(&str, &str) -> std::io::Result<()> + Send + Sync>;
pub type SyncFn = Arc<dyn Fn(PathBuf) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
pub type LogFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type PushFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;
/// Injectable account-usage fetch for the auto-resume scheduler (tests). `None` in
/// production → `engine::usage::fetch_usage` against the real OAuth endpoint.
pub type UsageFn = Arc<dyn Fn() -> Result<serde_json::Value, String> + Send + Sync>;

// ──────────────────────────────────────────────────────────────
// EngineConfig
// ──────────────────────────────────────────────────────────────
pub struct EngineConfig {
    pub src_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub log_dir: PathBuf,
    pub db_path: PathBuf,
    pub title_generator: std::sync::Arc<dyn TitleGenerator>, // Anthropic HTTP title/retitle client
    pub retitle_enabled: bool, // gate for periodic retitle of submit_session titles
    pub max_concurrent: Option<u64>, // None = unlimited
    pub git_org: String,
    pub claude_config_base: PathBuf,
    pub clone_fn: Option<CloneFn>,
    pub sync_fn: Option<SyncFn>,
    pub runner: Option<Arc<dyn Runner>>,
    pub log_fn: Option<LogFn>,
    pub now_fn: Option<NowFn>,
    pub push_fn: Option<PushFn>, // Phase 6 fills the FCM body; default no-op
    pub usage_fn: Option<UsageFn>, // auto-resume scheduler's usage probe; None = real endpoint
    pub idle_max_ms: Option<i64>,
    pub wall_max_ms: Option<i64>,
    pub idle_ttl_ms: Option<i64>,
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
}

// ──────────────────────────────────────────────────────────────
// Queue / running types
// ──────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct QueueItem {
    pub id: String,
    pub prompt: String,
    pub env: HashMap<String, String>,
    pub resume_session_id: Option<String>,
    pub enqueued_at: Option<i64>,
    /// Per-turn model override; `None` means "use session-level value".
    pub model: Option<String>,
    /// Per-turn effort override; `None` means "use session-level value".
    pub effort: Option<String>,
    /// Per-turn permission mode override; `None` means "use session-level value".
    pub permission_mode: Option<String>,
    /// Context to prepend to the user's message when this turn is written to claude, WITHOUT
    /// polluting the logged/displayed user message (which stays `prompt`). Used by fork: a fresh
    /// fork has no `claude_session_id`, so `--resume` cannot carry the source conversation; the
    /// source transcript (the "seed prompt") rides here on the fork's first turn so claude actually
    /// sees what it forked from. `None` for every normal turn.
    pub context_prefix: Option<String>,
}

pub struct RunningTurn {
    pub run: Arc<dyn crate::engine::runner::RunHandle>, // kill/interrupt surface (Arc-shared)
    pub pump: tokio::task::JoinHandle<()>,
    pub saw_result: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
pub(crate) struct SubEntry {
    next: u64,
    subs: HashMap<u64, Subscriber>,
}

// ──────────────────────────────────────────────────────────────
// Engine state (the Mutex-guarded bag — never held across .await)
// ──────────────────────────────────────────────────────────────
#[derive(Default)]
pub struct EngineState {
    pub(crate) subs: HashMap<String, SubEntry>,
    pub(crate) running: HashMap<String, RunningTurn>,
    pub(crate) queue: VecDeque<QueueItem>,
    pub(crate) activity: HashMap<String, Activity>,
    pub(crate) awaiting: HashMap<String, bool>,
    pub(crate) pending_ask: HashSet<String>,
    pub(crate) pending_perm: HashSet<String>,
    /// Sessions with an in-flight `delegate` cheap-worker fan-out. Like pending_ask, this parks the
    /// watchdog so the main turn is not idle/wall-cancelled while workers run (minutes, no main events).
    pub(crate) pending_delegate: HashSet<String>,
    /// sessionId -> the wire JSON (to_wire) of the prompt the session is parked on (ask/perm/plan).
    /// Parallel to pending_ask/pending_perm; the payload lets with_activity expose it on the Session.
    pub(crate) parked: std::collections::HashMap<String, serde_json::Value>,
    // Per-session set of tool_use ids that are SPAWNED SUBAGENTS (Agent/Task). A `tool_result` whose
    // tool_use_id is in here is a genuine agent result (persist + show on the agent card); any other
    // tool_result is just a plain tool's output (its tool chip already represents it) — keeps agent ≠ tool.
    pub(crate) spawn_ids: HashMap<String, HashSet<String>>,
    /// Per-session set of PR web URLs already turned into a card, so a URL echoed across several tool
    /// results (e.g. `gh pr create` then a later `gh pr view`) fetches + cards exactly once per session.
    pub(crate) pr_seen: HashMap<String, HashSet<String>>,
    /// Per-session FIFO of `delegate` workflow-card tool_use ids awaiting their `DelegateRequest`. A
    /// card and its request are emitted 1:1 and in order within a turn, so popping front on each request
    /// links the run to the exact card that started it (used to open the right run on click).
    pub(crate) workflow_delegate_pending: HashMap<String, VecDeque<String>>,
    /// Per-session set of native `Workflow` tool_use ids awaiting their tool_result, from which the run
    /// id is read to link the card. An id is removed once its result arrives.
    pub(crate) workflow_native_ids: HashMap<String, HashSet<String>>,
    pub(crate) starting: HashSet<String>,
    pub(crate) last_event_at: HashMap<String, i64>,
    pub(crate) turn_started_at: HashMap<String, i64>,
    pub(crate) closed: bool,
    /// Per-session set of outbox file paths already logged as `agentic_file` markers.
    pub(crate) logged_outbox: HashMap<String, HashSet<String>>,
}

// ──────────────────────────────────────────────────────────────
// EngineInner / Engine
// ──────────────────────────────────────────────────────────────
pub struct EngineInner {
    pub(crate) cfg: EngineConfig,
    pub(crate) store: Arc<Store>,
    pub(crate) runner: Arc<dyn Runner>,
    pub(crate) transcript: Option<Arc<TranscriptCache>>,
    pub(crate) state: Mutex<EngineState>,
    watchdog: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Per-session async lock used by `reconcile_from_native` to serialise concurrent
    /// calls for the same session id. Without it, two calls can both read the same
    /// `native_watermark_lines`, both translate the same delta, and both append the
    /// same lines to the rendered log — duplicate transcript. Keyed by session id; an
    /// inner `Arc<Mutex<()>>` is inserted on first observation and reused thereafter.
    /// Guarded by a fast synchronous `parking_lot::Mutex<HashMap<_, _>>` only for the
    /// lookup/insert (NEVER held across `.await`).
    pub(crate) reconcile_locks:
        parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
pub struct Engine(pub Arc<EngineInner>);

/// Result of `Engine::detach_session`: the terminal `claude --resume` command to
/// hand off an adopted session, plus the pieces it's built from.
pub struct DetachInfo {
    pub cwd: String,
    pub claude_session_id: String,
    pub resume_cmd: String,
}

// ──────────────────────────────────────────────────────────────
// SubmitMeta
// ──────────────────────────────────────────────────────────────
#[derive(Default)]
pub struct SubmitMeta {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    /// Permission mode for this session (e.g. "plan" / "acceptEdits" / "bypassPermissions").
    /// None → engine default; empty string treated the same as None downstream.
    pub permission_mode: Option<String>,
    /// Skills to hide from this session (blacklist) → claude `skillOverrides:{<name>:"off"}`.
    pub hidden_skills: Vec<String>,
    /// Plugins (`<plugin>@<marketplace>` ids) to disable for this session (blacklist) →
    /// claude `enabledPlugins:{<id>:false}` in the per-session `--settings`.
    pub hidden_plugins: Vec<String>,
    /// Optional session-scoped CLAUDE.md content from the New-request form. Written into the session
    /// dir so Claude Code loads it as project memory for this session, layered ON TOP of each repo's
    /// own committed CLAUDE.md. None / blank = no extra guidance.
    pub claude_md: Option<String>,
    /// Files uploaded to the staging area (`POST /api/uploads`) BEFORE this session existed — the
    /// New-request form attaches files while there is no session id yet. [Engine::submit_session]
    /// moves each into the new session's `uploads/` dir before the first prompt runs, so the agent
    /// can read the `[attached: uploads/<name>]` paths in that very first prompt. Empty = none.
    pub staged_uploads: Vec<StagedUpload>,
    /// MCP server names to disable for this session (blacklist).
    pub hidden_mcp_servers: Vec<String>,
    /// Ad-hoc MCP servers to inject for this session only (not persisted globally).
    pub extra_mcp_servers: Vec<crate::engine::store::McpServerDef>,
    /// Plugin ids to force ON for this session (overrides global-off). Disjoint from hidden_plugins.
    pub forced_on_plugins: Vec<String>,
    /// Skill names to force ON for this session (overrides global-off). Disjoint from hidden_skills.
    pub forced_on_skills: Vec<String>,
    /// MCP server names to force ON (stored; no-op at spawn until global MCP disable exists).
    pub forced_on_mcp_servers: Vec<String>,
}

/// One file staged before a session exists (see [SubmitMeta::staged_uploads]). Lives at
/// `<worktrees_root>/.staging/<token>/<name>` until [Engine::submit_session] adopts it into the new
/// session's `uploads/` dir. Deserialized straight from the create request's `stagedUploads` array.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct StagedUpload {
    pub token: String,
    pub name: String,
}

/// Root dir for pre-session staged uploads. Kept under `worktrees_root` (same filesystem as the
/// session worktrees) so a staged file can be adopted with a cheap `rename` instead of a copy. The
/// leading dot keeps it clear of session-id worktree dirs; session listing is db-driven (never a
/// directory scan), so this sibling is never mistaken for a session.
pub fn staging_root(worktrees_root: &std::path::Path) -> std::path::PathBuf {
    worktrees_root.join(".staging")
}

/// A fresh, collision-free token naming one staged upload's directory.
pub fn new_staging_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Best-effort sweep of staging token dirs older than `max_age` — orphans left by New-request flows
/// the user abandoned before creating a session (a created session's adoption removes its own token
/// dir). Cheap: one `read_dir` over `.staging`; every error is ignored so this never disrupts the
/// upload it precedes.
pub fn sweep_stale_staging(worktrees_root: &std::path::Path, max_age: std::time::Duration) {
    let Ok(rd) = std::fs::read_dir(staging_root(worktrees_root)) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age > max_age)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Sanitize a user-supplied filename to a single safe path component: drop any directory part, then
/// replace every char outside `[\w.\-]` with `_`, falling back to "upload" when nothing is left.
/// Single source of truth for both the per-session upload route and the staging route (and reused
/// when adopting staged files, so a malicious `name`/`token` can never escape its directory).
pub fn sanitize_upload_name(raw: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[^\w.\-]+").expect("valid regex"));
    let stem = std::path::Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload");
    let safe = RE.replace_all(stem, "_").into_owned();
    if safe.is_empty() {
        "upload".into()
    } else {
        safe
    }
}

// ──────────────────────────────────────────────────────────────
// new_session_id — UUID v4 via the uuid crate
// ──────────────────────────────────────────────────────────────
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ──────────────────────────────────────────────────────────────
// read_git_head — branch + HEAD sha of a directory (empty if non-git)
// ──────────────────────────────────────────────────────────────
/// Read `(branch, sha)` for `cwd` via `git -C <cwd> rev-parse`. Returns
/// `("", "")` when `cwd` is not a git repo (or `git` is unavailable) — adopt
/// treats a non-git cwd as having no branch/base. Mirrors the `git -C … rev-parse`
/// pattern `fork_session` uses to snapshot a source worktree's HEAD.
fn read_git_head(cwd: &str) -> (String, String) {
    let run = |args: &[&str]| -> String {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = run(&["rev-parse", "HEAD"]);
    (branch, sha)
}

/// Single-quote `s` for safe interpolation into a POSIX shell command line. Wraps the whole
/// string in single quotes and rewrites any embedded single quote as `'\''` (close-quote,
/// backslash-escaped literal quote, reopen-quote) — the standard shell-quoting idiom. Used by
/// `detach_session` so a `cwd` containing spaces or shell metacharacters can't break out of the
/// generated `cd <cwd> && claude --resume …` command when pasted into a terminal.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ──────────────────────────────────────────────────────────────
// has_active_workflow
// ──────────────────────────────────────────────────────────────
/// Returns true iff any workflow run under `base` is not in a terminal state.
///
/// Delegates to `workflows::list_workflows` (the Phase-6 canonical reader: completed
/// summaries win over a lingering live journal) and checks each run's status against the
/// shared terminal-state set with trim+lowercase normalisation. Cheap when there are no workflows
/// (`list_workflows` returns []). Previously a hand-rolled scan (Phase-4 minimum) that
/// only looked at `subagents/workflows` and compared status without normalising.
pub fn has_active_workflow(base: &std::path::Path, session_uuid: Option<&str>) -> bool {
    crate::engine::workflows::list_workflows(base, session_uuid)
        .iter()
        .any(|r| !crate::engine::workflows::is_workflow_terminal(&r.status))
}

// ──────────────────────────────────────────────────────────────
// Engine impl
// ──────────────────────────────────────────────────────────────
impl Engine {
    /// Construct: open the store, run recover() + reconcile_worktrees(), start the watchdog.
    pub async fn new(cfg: EngineConfig) -> Result<Engine, StoreError> {
        let store = Arc::new(Store::open(cfg.db_path.clone(), cfg.log_dir.clone()).await?);
        let engine = Engine::with_store(cfg, store, None);
        // Task 8: recover sessions left running/pending from a prior restart,
        // then reconcile orphan worktree dirs and orphaned delegate fan-outs.
        engine.recover().await;
        engine.reconcile_worktrees().await;
        engine.reconcile_delegate_runs().await;
        Ok(engine)
    }

    /// Variant that reuses an already-open Store + optional shared TranscriptCache.
    /// Does NOT run recover()/reconcile_worktrees() — callers that need recovery
    /// (production main.rs) should call Engine::new instead. AppState (Task 9) can
    /// call Engine::new so the full boot sequence runs there too.
    pub fn with_store(
        cfg: EngineConfig,
        store: Arc<Store>,
        transcript: Option<Arc<TranscriptCache>>,
    ) -> Engine {
        // Production always injects SdkRunner (main.rs) → the SDK bridge is the sole transport. Tests
        // inject an SdkRunner pointed at a fake bridge script. If somehow unset, fall back to a real
        // SdkRunner (production-shaped) — never a raw claude-CLI runner.
        let runner: Arc<dyn Runner> = cfg.runner.clone().unwrap_or_else(|| {
            Arc::new(crate::engine::sdk_runner::SdkRunner::new(
                crate::engine::sdk_runner::default_bridge_path(),
            ))
        });
        let inner = Arc::new(EngineInner {
            cfg,
            store,
            runner,
            transcript,
            state: Mutex::new(EngineState::default()),
            watchdog: Mutex::new(None),
            reconcile_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        });
        let engine = Engine(inner);
        // Start the watchdog (Task 6 body is already live).
        engine.start_watchdog();
        engine
    }

    // ── Internal helpers ─────────────────────────────────────

    /// Current time in ms. Uses the injectable now_fn or SystemTime.
    pub(crate) fn now(&self) -> i64 {
        if let Some(ref f) = self.0.cfg.now_fn {
            f()
        } else {
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        }
    }

    /// Emit a structured log record via log_fn (or no-op).
    pub(crate) fn log(&self, mut rec: serde_json::Value) {
        if let Some(ref f) = self.0.cfg.log_fn {
            if let Some(obj) = rec.as_object_mut() {
                obj.insert("comp".into(), serde_json::json!("engine"));
                obj.insert("t".into(), serde_json::json!(self.now()));
            }
            f(rec);
        }
    }

    /// Emit an event to all subscribers for this session id.
    /// Subscribers are called while holding the state lock; they must not re-lock state
    /// (they only forward to channels).
    pub(crate) fn emit(&self, id: &str, ev: &ClaudeEvent) {
        let state = self.0.state.lock();
        if let Some(entry) = state.subs.get(id) {
            for f in entry.subs.values() {
                f(ev);
            }
        }
    }

    /// Subscribe to events for a session. Returns a closure that removes the subscription.
    pub fn subscribe(&self, id: &str, f: Subscriber) -> Box<dyn FnOnce() + Send> {
        let mut state = self.0.state.lock();
        let entry = state.subs.entry(id.to_string()).or_default();
        let token = entry.next;
        entry.next += 1;
        entry.subs.insert(token, f);
        let inner = self.0.clone();
        let id_owned = id.to_string();
        Box::new(move || {
            let mut state = inner.state.lock();
            if let Some(entry) = state.subs.get_mut(&id_owned) {
                entry.subs.remove(&token);
                if entry.subs.is_empty() {
                    state.subs.remove(&id_owned);
                }
            }
        })
    }

    /// Remove all per-session runtime state (but not subs — only deleteSession clears subs).
    pub(crate) fn forget_session(&self, id: &str) {
        let mut state = self.0.state.lock();
        state.activity.remove(id);
        state.last_event_at.remove(id);
        state.turn_started_at.remove(id);
        state.awaiting.remove(id);
        state.pending_ask.remove(id);
        state.pending_perm.remove(id);
        state.pending_delegate.remove(id);
        state.parked.remove(id);
        state.spawn_ids.remove(id);
        state.pr_seen.remove(id);
        state.workflow_delegate_pending.remove(id);
        state.workflow_native_ids.remove(id);
        state.starting.remove(id);
        // Drop the state guard before taking reconcile_locks so the two locks are never held
        // together. Otherwise this per-session lock entry leaks as sessions are created/deleted.
        drop(state);
        self.0.reconcile_locks.lock().remove(id);
    }

    /// Count active concurrency slots (starting + running-non-parked).
    pub(crate) fn active_count(state: &EngineState) -> u64 {
        let starting = state.starting.len() as u64;
        let running = state
            .running
            .keys()
            .filter(|id| state.awaiting.get(*id) != Some(&true))
            .count() as u64;
        starting + running
    }

    /// Enrich a session with runtime fields from EngineState.
    pub(crate) fn with_activity(&self, mut s: Session) -> Session {
        // ── Outbox file markers: log new files as `agentic_file` events in the transcript stream ──
        // This makes delivered files first-class events — positioned at their real delivery time by the
        // server, not guessed by client-side heuristics. `interleaveShared` deduplicates them.
        if let Some(wt) = s.worktree_path.as_deref().filter(|p| !p.is_empty()) {
            let session_dir = std::path::Path::new(wt);
            let outbox_dir = if s.repos.len() == 1 {
                session_dir.join(&s.repos[0]).join("outbox")
            } else {
                session_dir.join("outbox")
            };
            if outbox_dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&outbox_dir) {
                    let files: Vec<std::fs::DirEntry> =
                        entries.flatten().filter(|e| e.path().is_file()).collect();
                    // The in-memory `logged_outbox` set dies with the process. On the FIRST touch of a
                    // session after a restart (or after the entry was purged), seed it from the markers
                    // already persisted in the log — otherwise every existing outbox file would be
                    // "new" again and get a duplicate `agentic_file` marker appended at the log TAIL,
                    // piling stale file cards at the bottom of the reopened transcript. Read the log
                    // OUTSIDE the state lock (it can be large); the later extend is idempotent, so a
                    // racing touch seeding the same session twice is harmless. Cost note: the read is
                    // once per session per process life, and ONLY for sessions that actually have
                    // outbox files. An EMPTY outbox skips everything — including creating the
                    // logged_outbox entry — so a later first touch that actually sees files still seeds.
                    if !files.is_empty() {
                        let needs_seed = !self.0.state.lock().logged_outbox.contains_key(&s.id);
                        let seeded: Option<HashSet<String>> = if needs_seed {
                            Some(
                                self.0
                                    .store
                                    .read_log(&s.id)
                                    .iter()
                                    .filter(|l| l.contains("\"type\":\"agentic_file\""))
                                    .filter_map(|l| {
                                        serde_json::from_str::<serde_json::Value>(l).ok()
                                    })
                                    .filter(|v| {
                                        v.get("type").and_then(|t| t.as_str())
                                            == Some("agentic_file")
                                    })
                                    .filter_map(|v| {
                                        v.get("path").and_then(|p| p.as_str()).map(str::to_string)
                                    })
                                    .collect(),
                            )
                        } else {
                            None
                        };
                        let mut state_guard = self.0.state.lock();
                        let logged = state_guard.logged_outbox.entry(s.id.clone()).or_default();
                        if let Some(seed) = seeded {
                            logged.extend(seed);
                        }
                        for entry in files {
                            let path = entry.path();
                            let rel = path.strip_prefix(&outbox_dir).unwrap_or(&path);
                            let rel_str = format!("outbox/{}", rel.display());
                            if !logged.insert(rel_str.clone()) {
                                continue;
                            }
                            let at = entry
                                .metadata()
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            let marker = serde_json::json!({
                                "type": "agentic_file",
                                "path": rel_str,
                                "at": at,
                            })
                            .to_string();
                            self.0.store.append_log_blocking(&s.id, &marker);
                        }
                    }
                }
            }
        }

        let state = self.0.state.lock();
        let id = &s.id;
        if let Some(act) = state.activity.get(id) {
            s.activity = Some(act.clone());
        }
        if let Some(&v) = state.awaiting.get(id) {
            s.awaiting_input = Some(v);
        }
        s.pending_prompt = state.parked.get(id).cloned();
        let finished_or_idle = matches!(s.status.as_str(), "done" | "failed" | "killed")
            || s.awaiting_input == Some(true);
        if finished_or_idle {
            // Workflow data now lives in the SHARED config dir; scope to THIS session's claude
            // transcript uuid so we don't read another session's runs. No uuid → never ran claude
            // → no workflows.
            if let Some(uuid) = s.claude_session_id.as_deref() {
                if has_active_workflow(&self.0.cfg.claude_config_base, Some(uuid)) {
                    s.workflow_running = Some(true);
                }
            }
        }
        s
    }

    // ── Close ────────────────────────────────────────────────

    /// Shut the engine down. `kill_running = true` stops every in-flight claude run;
    /// `false` *detaches* them — the child keeps running and writing to its log fd, to be
    /// finalized by the next boot's `recover()`.
    pub fn close_with(&self, kill_running: bool) {
        let mut state = self.0.state.lock();
        state.closed = true;
        // Abort the watchdog task
        if let Some(h) = self.0.watchdog.lock().take() {
            h.abort();
        }
        // Stop (kill) or detach all running turns.
        for (_, turn) in state.running.drain() {
            if kill_running {
                turn.run.stop();
            }
            // Always drop the pump: on detach the child keeps writing to its log fd directly,
            // so aborting the pump only stops *us* from consuming it (mirror of
            // SpawnHandle::detach's poll_task.abort()).
            turn.pump.abort();
        }
        drop(state);
    }

    /// Shut the engine down, killing all in-flight runs.
    pub fn close(&self) {
        self.close_with(true);
    }
}

// HTTP-independent core, grouped by responsibility. Each group is a submodule; every file is
// re-exported at the engine level below so existing `crate::engine::<module>` paths keep
// resolving — this was a behavior-preserving regroup of a previously-flat directory. The API
// layer (`api::sessions`) reaches `native_transcript` etc. through these re-exports; the engine
// itself stays axum-free.
// The grouping subdirectories are `pub(crate)` — only the flat re-exports below are the engine's
// public surface, so the internal directory layout is not exposed as public API.
pub(crate) mod config;
pub(crate) mod extensions;
pub(crate) mod misc;
pub(crate) mod model;
pub(crate) mod persistence;
pub(crate) mod runtime;
pub(crate) mod session;
pub(crate) mod streaming;
pub(crate) mod titling;
pub(crate) mod transcripts;
pub(crate) mod vcs;
pub(crate) mod workflow;

pub use config::{global_settings, session_guide, templates, user_config};
pub use extensions::{components, plugin_cli, plugins, skill_install, skills};
pub use misc::{classify_error, mentions, push, search};
pub use model::{chatgpt_oauth, litellm, native_overrides, providers, router, routing_config};
pub use persistence::{atomic_write, store};
pub use runtime::{runner, sdk_runner, spawner};
pub use session::{auto_resume, groups, lifecycle, status, transition};
pub use streaming::stream;
pub use titling::{title, title_client};
pub use transcripts::{tailer, transcript, transcript_filter, usage};
pub use vcs::{repos, structured_diff, worktree};
pub use workflow::{delegate, workflows};

// These were private / `pub(crate)` before the regroup — keep them crate-only (don't widen to the
// public API just because they moved into a subdir).
pub(crate) use misc::error;
pub(crate) use session::resume_gate;
pub(crate) use transcripts::native_transcript;

pub use misc::error::EngineError;

pub use misc::search::{
    classify_rendered_line, derive_tool_detail, derive_tool_summary, extract_snippet,
    ClassifiedLine, SearchField, SearchHit, SearchMatch, SearchResponse, SearchService, SearchTier,
};

#[cfg(test)]
mod tests;
