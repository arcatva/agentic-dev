use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use crate::engine::classify_error::classify_claude_error;
use crate::engine::repos::ensure_local;
// The per-turn transport is the SDK bridge (SdkRunner), in both production and tests. Production
// injects SdkRunner (main.rs); tests inject an SdkRunner pointed at a fake bridge script. There is
// no raw-`claude`-CLI runner anymore.
use crate::engine::runner::Runner;
use crate::engine::title_client::TitleGenerator;
use crate::engine::spawner::{compose_user_text, encode_user_message, spawn_claude, SpawnOptions};
use crate::engine::status::SessionStatus;
use crate::engine::store::{
    Activity, CreateInput, Field, Session, SessionPatch, SessionUpdate, Store, StoreError,
};
use crate::engine::transition::TransitionReason;
use crate::engine::stream::ClaudeEvent;
use crate::engine::transcript::TranscriptCache;
use crate::engine::worktree::{create_session_worktrees, sync_worktree};

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

/// Build the text actually written to claude for a turn. Normally this is just `item.prompt`.
/// When `item.context_prefix` is set (a fork's first turn), the seed context is prepended ahead
/// of the user's message so claude sees the forked-from conversation, while the logged/displayed
/// user message stays `item.prompt` alone.
pub(crate) fn compose_turn_text(item: &QueueItem) -> String {
    compose_turn_text_with(item, &item.prompt)
}

/// [compose_turn_text] with the delivered prompt supplied by the caller — used by the spawn
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
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
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
    let Ok(rd) = std::fs::read_dir(staging_root(worktrees_root)) else { return };
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
    if safe.is_empty() { "upload".into() } else { safe }
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
                            self.0.store.read_log(&s.id).iter()
                                .filter(|l| l.contains("\"type\":\"agentic_file\""))
                                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("agentic_file"))
                                .filter_map(|v| v.get("path").and_then(|p| p.as_str()).map(str::to_string))
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
                        if !logged.insert(rel_str.clone()) { continue; }
                        let at = entry.metadata().ok().and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        let marker = serde_json::json!({
                            "type": "agentic_file",
                            "path": rel_str,
                            "at": at,
                        }).to_string();
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

    // ── Public read surface ───────────────────────────────────

    /// Best-effort session list — degrades a store failure to an EMPTY list. Kept for internal callers
    /// (search, recovery) that prefer a degraded result over surfacing an error. The HTTP handler must
    /// use [Engine::try_list] instead: a 200-with-empty laundered from a store error makes the Android
    /// client REPLACE its good list with nothing, briefly "losing" sessions (most visibly the
    /// recently-active ones) until the next poll.
    pub async fn list(&self) -> Vec<Session> {
        self.try_list().await.unwrap_or_default()
    }

    /// Fallible session list. A store error (e.g. transient sqlite lock contention from concurrently
    /// running sessions writing their status/activity) propagates as [EngineError::Store] so the API
    /// can answer 5xx — the Android client then keeps its last-good list (blip tolerance) instead of
    /// blanking it on a successful-but-empty response.
    pub async fn try_list(&self) -> Result<Vec<Session>, EngineError> {
        let sessions = self.0.store.list().await?;
        Ok(sessions.into_iter().map(|s| self.with_activity(s)).collect())
    }

    pub async fn get(&self, id: &str) -> Option<Session> {
        self.0
            .store
            .get(id)
            .await
            .ok()
            .flatten()
            .map(|s| self.with_activity(s))
    }

    /// Update session metadata (model/effort/mode/permission_mode) and return the refreshed session.
    /// Returns Err(StoreError) on DB failure; returns Err(EngineError::NotFound) if the session
    /// does not exist after the update (session was deleted concurrently).
    pub async fn patch_session_meta(&self, id: &str, patch: SessionPatch) -> Result<Session, EngineError> {
        self.0.store.update(id, patch).await.map_err(EngineError::Store)?;
        self.0.store.get(id).await
            .map_err(EngineError::Store)?
            .map(|s| self.with_activity(s))
            .ok_or_else(|| EngineError::NotFound(id.to_string()))
    }

    pub fn get_log(&self, id: &str) -> Vec<String> {
        self.0.store.read_log(id)
    }

    /// The Claude config base dir (`~/.claude` by default) this engine reads native
    /// transcripts from. Thin accessor so the API layer can call
    /// [crate::engine::native_transcript::scan_adoptable] without reaching into engine
    /// internals — keeps the engine axum-free.
    pub fn config_base(&self) -> std::path::PathBuf {
        self.0.cfg.claude_config_base.clone()
    }

    /// The set of `claudeSessionId`s already linked to a stored session — the exclusion
    /// set for [crate::engine::native_transcript::scan_adoptable] so an already-adopted (or
    /// natively-linked) transcript is hidden from the adoptable list. Rows without a linked
    /// csid contribute nothing; a store error degrades to an empty set (scan then offers
    /// everything, which the adopt guard still rejects on a double-adopt).
    pub async fn known_claude_session_ids(&self) -> std::collections::HashSet<String> {
        self.0
            .store
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| s.claude_session_id)
            .collect()
    }

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
        let (lines, total) =
            crate::engine::native_transcript::translate_range(&path, from);
        for line in &lines {
            self.0
                .store
                .append_log(id, line)
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
        let resume_cmd = format!("cd {} && claude --resume {}", shell_single_quote(&cwd), csid);
        Ok(DetachInfo {
            cwd,
            claude_session_id: csid,
            resume_cmd,
        })
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
            let mut sections: Vec<String> = vec![
                crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string(),
            ];
            if wts.len() > 1 {
                let repo_summaries: Vec<(String, Option<String>)> = wts
                    .iter()
                    .map(|w| (w.repo.clone(), crate::engine::session_guide::summarize_repo(&w.worktree_path)))
                    .collect();
                sections.push(crate::engine::session_guide::build_session_guide(&repo_summaries, &skills));
            }
            if let Some(custom) = meta.claude_md.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
                        Ok(bytes) => { let _ = std::fs::write(&dst, &bytes); }
                        Err(e) => tracing::warn!("[engine] staged upload {token}/{name} unreadable: {e}"),
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
        if let Err(e) = self.0.store.update(id, crate::engine::store::SessionPatch {
            prompt: Some(new_title.clone()),
            ..Default::default()
        }).await {
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
    fn maybe_spawn_retitle(&self, id: &str) {
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
    fn is_busy(&self, id: &str, status: &str) -> bool {
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
    async fn expand_session_mentions(&self, text: &str) -> String {
        if !text.contains("@session:") {
            return text.to_string();
        }
        match self.0.store.list().await {
            Ok(sessions) => {
                let store = &self.0.store;
                mentions::expand_session_mentions(text, &sessions, &|id| store.log_path(id))
            }
            Err(e) => {
                tracing::warn!("[engine] @session mention expansion skipped — store.list failed: {e}");
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
        let context_prefix = if s.parent_session_id.is_some() && since == 0 && !s.prompt.is_empty() {
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
        if let Err(e) = self
            .0
            .store
            .apply_update(id, follow_up)
            .await
        {
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
    pub async fn fork_session(&self, src_id: &str) -> Result<crate::engine::store::Session, EngineError> {
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
            src.repos.iter().map(|r| {
                crate::engine::repos::ensure_local(r, &self.0.cfg.src_root, &self.0.cfg.git_org, clone_fn)
                    .map(|p| (r.clone(), p))
                    .map_err(|e| EngineError::Internal(format!("repo {r}: {e}")))
            }).collect::<Result<Vec<_>, _>>()?
        };

        // Read each source worktree's HEAD (snapshot point).
        let mut base_shas: std::collections::HashMap<String, String> = std::collections::HashMap::new();
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
            base_shas.insert(repo.clone(), String::from_utf8_lossy(&sha.stdout).trim().to_string());
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
            ).map_err(|e| EngineError::Internal(truncate_chars(&format!("{e}"), 200).to_string()))?
        };

        let session_dir = self.0.cfg.worktrees_root.join(&id);
        let base_shas_db: std::collections::HashMap<String, Option<String>> = wts
            .iter().map(|w| (w.repo.clone(), Some(w.base_sha.clone()))).collect();

        // Session CLAUDE.md (Tier-2): build-env guide (always) + multi-repo orientation (when >1
        // worktree) + custom guidance. Routing / fan-out are Tier-1 (system prompt), not written here.
        {
            let mut sections = vec![
                crate::engine::session_guide::WORKTREE_SETUP_GUIDE.to_string(),
            ];
            if wts.len() > 1 {
                let repo_summaries: Vec<(String, Option<String>)> = wts.iter()
                    .map(|w| (w.repo.clone(), crate::engine::session_guide::summarize_repo(&w.worktree_path)))
                    .collect();
                sections.push(crate::engine::session_guide::build_session_guide(&repo_summaries, &src.skills));
            }
            crate::engine::session_guide::write_session_claude_md(&session_dir, &sections);
        }

        let branch = format!("agentic/{id}");
        let base_sha_first = wts.first().map(|w| w.base_sha.clone());

        // Read the source log and build the seed prompt.
        let log_raw = std::fs::read_to_string(self.0.store.log_path(src_id)).unwrap_or_default();
        let transcript = filter_log_to_transcript(&log_raw);
        let mut visible_label: String = src.prompt.chars().take(50).collect();
        if src.prompt.chars().count() > 50 { visible_label.push('…'); }
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
    fn remove_worktrees_best_effort(
        &self,
        repo_specs: &[(String, std::path::PathBuf)],
        id: &str,
    ) {
        let session_dir = self.0.cfg.worktrees_root.join(id);
        for (repo, repo_path) in repo_specs {
            let wt = session_dir.join(repo);
            let rp = repo_path.to_string_lossy();
            let wp = wt.to_string_lossy();
            if let Err(e) = crate::engine::worktree::git_sync(&["-C", &rp, "worktree", "remove", "--force", &wp]) {
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
            .apply_update(id, crate::engine::store::SessionUpdate::new().clear_auto_resume_at())
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

    // ── Task 8: discard / delete_session ─────────────────────

    /// Return the session if it exists, busy-check, and live-worktree-check.
    /// Used by discard().
    async fn live_session(&self, id: &str) -> Result<Session, EngineError> {
        let s = self
            .0
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        if self.is_busy(id, &s.status) {
            return Err(EngineError::Busy);
        }
        if s.worktree_state != "live" {
            return Err(EngineError::WorktreeCleaned);
        }
        Ok(s)
    }

    /// True when `path` lives INSIDE the managed worktrees root (`cfg.worktrees_root`).
    ///
    /// DATA-LOSS guard for adopt-in-place: an adopted session's `worktree_path` is the user's
    /// REAL project cwd, which is OUTSIDE `worktrees_root`. Every `remove_dir_all` on a session's
    /// worktree dir must first pass this check so `delete`/`discard` never blow away the user's
    /// actual project directory — they may only remove the managed `<worktrees_root>/<id>` dirs.
    /// Canonicalizes both sides where possible (resolving symlinks/`..`); when a path can't be
    /// canonicalized (e.g. already gone) it falls back to raw-prefix comparison against both the
    /// canonicalized and raw root, so a genuinely-managed dir is never mis-skipped.
    fn within_worktrees_root(&self, path: &std::path::Path) -> bool {
        crate::engine::native_transcript::path_within(path, &self.0.cfg.worktrees_root)
    }

    /// Discard the worktree for a done/idle session.
    pub async fn discard(&self, id: &str) -> Result<(), EngineError> {
        let s = self.live_session(id).await?;
        let wt_path = match s.worktree_path.as_ref() {
            Some(p) => PathBuf::from(p),
            None => return Err(EngineError::BadInput("session has no worktree path".into())),
        };
        let branch = s.branch.as_deref().unwrap_or("");
        // For each repo: call discard_worktree(src_root/repo, wt_path/repo, &branch)
        for repo in &s.repos {
            let repo_path = self.0.cfg.src_root.join(repo);
            let repo_wt = wt_path.join(repo);
            // Best-effort: ignore errors per individual repo
            if let Err(e) = crate::engine::worktree::discard_worktree(&repo_path, &repo_wt, branch)
            {
                tracing::warn!("[engine] discard_worktree {repo} failed: {e}");
            }
        }
        // Remove the session worktree dir itself — but ONLY if it is inside the managed
        // worktrees root. An adopt-in-place session's worktree_path is the user's real project
        // cwd (outside the root); removing it would destroy the user's actual directory.
        if self.within_worktrees_root(&wt_path) {
            if let Err(e) = std::fs::remove_dir_all(&wt_path) {
                tracing::warn!("[engine] remove worktree dir failed: {e}");
            }
        } else {
            tracing::warn!(
                "[engine] discard: skipping remove_dir_all of {} — outside worktrees_root (adopt-in-place cwd)",
                wt_path.display()
            );
        }
        // Drop any per-turn rewind snapshot refs for this session (best-effort).
        for repo in &s.repos {
            crate::engine::worktree::delete_snapshot_refs(&self.0.cfg.src_root.join(repo), id);
        }
        // Mark as discarded in the store
        self.0
            .store
            .update(
                id,
                SessionPatch {
                    worktree_state: Some("discarded".into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// Best-effort: snapshot every repo's working tree before a turn, labeled with `turn_index`, so
    /// the user can later rewind to it. Failures are logged, never propagated — a snapshot problem
    /// must never break the turn that triggered it.
    fn snapshot_worktrees(&self, s: &Session, turn_index: usize) {
        let Some(wt_root) = s.worktree_path.as_deref() else { return };
        for repo in &s.repos {
            let wt = std::path::Path::new(wt_root).join(repo);
            let snapshot_ref = format!("refs/agentic/snapshots/{}/{}", s.id, turn_index);
            if let Err(e) = crate::engine::worktree::snapshot_worktree(&wt, &snapshot_ref) {
                tracing::warn!("[engine] snapshot {repo}@turn{turn_index} failed: {e}");
            }
        }
    }

    /// Rewind the working tree to the snapshot taken just before `turn_index` ran — restoring tracked
    /// files (and recreating tracked deletions) while KEEPING untracked files created since (no git
    /// clean). `turn_index == 0` restores the per-repo base SHA (state before the first prompt). The
    /// branch/HEAD is not moved; chat history is unchanged (code-only rewind). Requires an idle
    /// session (a live turn is mid-write — restoring under it would corrupt its work).
    pub async fn rewind(&self, id: &str, turn_index: usize) -> Result<(), EngineError> {
        let s = self.live_session(id).await?;
        let wt_root = s.worktree_path.clone().ok_or(EngineError::NoWorktree)?;
        for repo in &s.repos {
            let wt = std::path::Path::new(&wt_root).join(repo);
            let target = if turn_index == 0 {
                s.base_shas
                    .get(repo)
                    .cloned()
                    .flatten()
                    .ok_or_else(|| EngineError::BadInput(format!("no base snapshot for repo {repo}")))?
            } else {
                let snapshot_ref = format!("refs/agentic/snapshots/{id}/{turn_index}");
                let wt_str = wt.to_string_lossy();
                // Verify the snapshot exists so the caller gets a clean 400, not a raw git error.
                if crate::engine::worktree::git_sync(&[
                    "-C", &wt_str, "rev-parse", "--verify", &format!("{snapshot_ref}^{{commit}}"),
                ])
                .is_err()
                {
                    return Err(EngineError::BadInput(format!(
                        "no snapshot for turn {turn_index} in repo {repo}"
                    )));
                }
                snapshot_ref
            };
            crate::engine::worktree::restore_worktree(&wt, &target)
                .map_err(|e| EngineError::Internal(format!("rewind restore {repo} failed: {e}")))?;
        }
        Ok(())
    }

    /// Wait until the pump task for `id` has finished (i.e., on_exit removed it from running).
    /// Polls every 10ms up to 5s for the pump task to complete.
    async fn wait_for_exit(&self, id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let state = self.0.state.lock();
                if !state.running.contains_key(id) {
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return; // timed out — best effort
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Delete a session. If force=true, stops any running turn first.
    pub async fn delete_session(&self, id: &str, force: bool) -> Result<(), EngineError> {
        // Get the session — if missing, return Ok (already gone).
        let s = match self.0.store.get(id).await? {
            Some(s) => s,
            None => return Ok(()),
        };
        let busy = self.is_busy(id, &s.status);
        if busy && !force {
            return Err(EngineError::Busy);
        }
        if busy {
            // Force: kill and wait.
            // Drop from queue first.
            {
                let mut state = self.0.state.lock();
                state.queue.retain(|q| q.id != id);
            }
            let run_handle = {
                let state = self.0.state.lock();
                state.running.get(id).map(|r| r.run.clone())
            };
            if let Some(run) = run_handle {
                // PR6: route through transition() with DeleteForce
                // reason (same effect as Kill but distinct for tracing).
                if let Err(e) = self
                    .transition(id, SessionStatus::Killed, TransitionReason::DeleteForce)
                    .await
                {
                    tracing::error!("[engine] transition delete kill-running failed: {e}");
                }
                run.stop();
                // Wait for on_exit to run (removes from running map).
                self.wait_for_exit(id).await;
            } else {
                // Was in starting or just queued (already removed) — mark killed+ended.
                // PR6: route through transition() with DeleteForce
                // reason. transition()'s DeleteForce branch (like KillQueued)
                // sets ended_at = now.
                if let Err(e) = self
                    .transition(id, SessionStatus::Killed, TransitionReason::DeleteForce)
                    .await
                {
                    tracing::error!("[engine] transition delete kill-queued failed: {e}");
                }
            }
        }
        // Re-read current session to get latest worktree_state after potential kill.
        let cur = match self.0.store.get(id).await? {
            Some(c) => c,
            None => return Ok(()), // deleted by on_exit race — fine
        };
        // Discard worktrees if still live.
        if cur.worktree_state == "live" {
            if let Some(ref wt_path_str) = cur.worktree_path {
                let wt_path = PathBuf::from(wt_path_str);
                let branch = cur.branch.as_deref().unwrap_or("");
                for repo in &cur.repos {
                    let repo_path = self.0.cfg.src_root.join(repo);
                    let repo_wt = wt_path.join(repo);
                    if let Err(e) =
                        crate::engine::worktree::discard_worktree(&repo_path, &repo_wt, branch)
                    {
                        tracing::warn!("[engine] delete discard_worktree {repo} failed: {e}");
                    }
                }
                // Remove the whole session dir (worktrees + any per-session scratch). Credentials
                // and transcripts live in the shared ~/.claude, so there is no per-session config
                // dir to clean up here anymore. Guard with within_worktrees_root: an adopt-in-place
                // session's worktree_path is the user's real project cwd (outside the root) —
                // remove_dir_all'ing it would destroy the user's actual directory (DATA LOSS).
                if self.within_worktrees_root(&wt_path) {
                    if let Err(e) = std::fs::remove_dir_all(&wt_path) {
                        tracing::warn!("[engine] remove session dir failed: {e}");
                    }
                } else {
                    tracing::warn!(
                        "[engine] delete: skipping remove_dir_all of {} — outside worktrees_root (adopt-in-place cwd)",
                        wt_path.display()
                    );
                }
            }
        }
        // Remove the store row.
        self.0.store.remove(id).await?;
        // Clean up all runtime state including subs.
        self.forget_session(id);
        {
            let mut state = self.0.state.lock();
            state.subs.remove(id);
        }
        // Drop cached transcript projection if we have a TranscriptCache.
        if let Some(ref tc) = self.0.transcript {
            tc.drop_session(id);
        }
        Ok(())
    }

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
                            .transition(&id, SessionStatus::Failed, TransitionReason::StartFailed(msg))
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
    fn spawn_opts(&self, s: &Session, item: &QueueItem) -> SpawnOptions {
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
        handle.write(&encode_user_message(&compose_user_text(&compose_turn_text_with(
            &item, &delivered,
        ))));

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
    async fn on_event(&self, id: &str, ev: ClaudeEvent) {
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

            ClaudeEvent::DelegateRequest { id: req_id, run_id, tasks, title, .. } => {
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
                        prompt: t.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        role: t.get("role").and_then(|v| v.as_str()).unwrap_or("explorer").to_string(),
                        model: t.get("model").and_then(|v| v.as_str()).map(String::from),
                        phase: t.get("phase").and_then(|v| v.as_str()).map(String::from),
                        write: t.get("write").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                    .collect();
                tokio::spawn(async move {
                    let summaries = match engine.run_delegate(&caller, &run_id, dtasks, title).await {
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
                    let run = { engine.0.state.lock().running.get(&caller).map(|r| r.run.clone()) };
                    if let Some(run) = run {
                        run.write(&line);
                    } else {
                        tracing::warn!("[engine] delegate reply: caller {caller} no longer running");
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
                let _ = self.0.store.incr_unread_event_id(id).await
                    .map_err(|e| tracing::warn!("[engine] unreadEventId incr failed for {id}: {e}"));

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
                            state.pr_seen.entry(id.to_string()).or_default().insert(url.clone())
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

            ClaudeEvent::Workflow { id: card_id, delegate, .. } => {
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
                tracing::warn!("[engine] gh pr view {url} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
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
        let title = meta.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let body = meta.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = meta.get("state").and_then(|v| v.as_str()).unwrap_or("OPEN").to_string();
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
            &crate::engine::stream::ClaudeEvent::Pr { url: url.to_string(), number, repo, title, body, state, raw },
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
            let _ = self.0.store.incr_unread_event_id(id).await
                .map_err(|e| tracing::warn!("[engine] unreadEventId incr on_exit failed for {id}: {e}"));
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

// Engine method clusters split across sibling files (multi-file inherent impls).
// HTTP-independent core modules.
pub mod atomic_write;
pub mod auto_resume;
pub mod classify_error;
pub mod components;
pub mod delegate;
pub mod global_settings;
pub mod groups;
pub mod plugins;
pub mod providers;
pub mod push;
pub mod repos;
pub mod router;
pub mod runner;
pub mod sdk_runner;
pub mod search;
pub mod session_guide;
pub mod skill_install;
pub mod skills;
pub mod spawner;
pub mod status;
pub mod store;
pub mod stream;
pub mod transition;
pub mod lifecycle;
pub mod litellm;
pub mod mentions;
pub mod structured_diff;
pub mod tailer;
pub mod templates;
pub mod title;
pub mod title_client;
pub mod transcript;
pub mod transcript_filter;
pub mod usage;
pub mod user_config;
pub mod plugin_cli;
pub mod workflows;
pub mod worktree;

// Engine method clusters split across sibling files (multi-file inherent impls).
mod diff;
mod error;
// `pub(crate)` so the API layer (`api::sessions`) can call `scan_adoptable` /
// `transcript_path` for the adopt / adoptable HTTP routes. The engine stays axum-free.
pub(crate) mod native_transcript;
mod recover;
mod resume_gate;
mod watchdog;
pub use error::EngineError;

pub use search::{
    classify_rendered_line, derive_tool_detail, derive_tool_summary, extract_snippet, ClassifiedLine,
    SearchField, SearchHit, SearchMatch, SearchResponse, SearchService, SearchTier,
};

#[cfg(test)]
mod tests;
