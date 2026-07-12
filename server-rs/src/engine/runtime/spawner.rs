use crate::engine::runner::{RunHandle, RunSpec, Runner};
use crate::engine::stream::ClaudeEvent;
use crate::engine::tailer::EventTailer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    pub cwd: String, // the worktree
    pub prompt: String,
    pub env: HashMap<String, String>, // overlay on top of the process env
    pub resume_session_id: Option<String>, // resume an existing claude session when set
    pub claude_config_dir: Option<String>, // per-session CLAUDE_CONFIG_DIR; None = clear it
    pub model: Option<String>,        // SDK model option
    pub effort: Option<String>,       // SDK effort (omitted when ultracode)
    pub mode: Option<String>,         // "ultracode" => settings {"ultracode":true}
    pub hidden_skills: Vec<String>,   // blacklist => settings {"skillOverrides":{<n>:"off"}}
    /// EXPLICIT per-plugin enable map => settings {"enabledPlugins":{<id>:true|false}}. Resolved by
    /// [crate::engine::plugins::resolve_enabled_plugins] from the session's hiddenPlugins blacklist ×
    /// the installed-plugin registry; `true` entries force-enable a plugin for this session even when
    /// it is disabled globally (command-line settings win). Empty map → no env → CLI defaults.
    pub enabled_plugins: std::collections::BTreeMap<String, bool>,
    /// MCP server names to disable for this session (blacklist). Forwarded as SDK_BRIDGE_HIDDEN_MCP.
    pub hidden_mcp_servers: Vec<String>,
    /// Ad-hoc MCP server defs for this session only. Hidden names are removed before injection.
    pub extra_mcp_servers: Vec<crate::engine::store::McpServerDef>,
    /// Plugin ids forced ON for this session. Resolved by resolve_enabled_plugins (forcedOn wins).
    pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session that the global baseline disables.
    pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON (stored; no-op at spawn until global MCP disable exists).
    pub forced_on_mcp_servers: Vec<String>,
    pub log_path: PathBuf, // the bridge appends stream-json here; the tailer reads it
    pub unit: String,      // label (agentic-<id>)
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
    pub permission_mode: Option<String>, // SDK permissionMode
    /// Tier-1 harness rules appended to Claude Code's system prompt (via `--append-system-prompt`).
    /// Set ONLY on the main session turn (see `EngineInner::spawn_opts`); worker spawns leave it
    /// `None` so a delegate worker never carries the orchestrator-only routing / fan-out rules.
    pub append_system_prompt: Option<String>,
}

pub const POLL_MS: u64 = 120;
pub const OUTBOX_NOTE: &str =
    "[Delivering files to the user: this session runs in an app with no terminal. To give the user a \
file — a build artifact (e.g. an APK), a generated document, a report, an exported image — copy or \
write it into ./outbox/ (relative to your working directory; create the dir if needed). Files in \
./outbox/ are shown in the app to preview and download. Do this whenever the user asks you to send, \
give, share, or export a file. Do NOT put ordinary source-code edits in ./outbox/.]";

/// Does `text` begin with a slash command (`/lfg`, `/ce-code-review`, …)?
///
/// Slash-command expansion in the Agent SDK only fires when the message STARTS with `/<cmd>`.
/// The command name is a lowercase-initial run of `[a-z0-9_-:]` ending at the first whitespace
/// (`:` allows an explicitly plugin-namespaced form like `/compound-engineering:lfg`). That shape
/// leaves real filesystem paths (`/home/user/x` — the token holds a `/`) and prose untouched.
/// A false positive is harmless anyway: an unregistered command is delivered as plain text.
pub fn is_slash_command(text: &str) -> bool {
    let Some(rest) = text.trim_start().strip_prefix('/') else {
        return false;
    };
    // split (not split_whitespace): a space right after the slash ("/ foo") yields an empty first
    // token, so it's correctly rejected rather than skipping ahead to "foo".
    let name: &str = rest.split(char::is_whitespace).next().unwrap_or("");
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | ':'))
}

pub fn compose_user_text(text: &str) -> String {
    // A leading slash command must reach the CLI at the very START of the message or the SDK won't
    // expand it — the outbox-note prefix would push it off the front. For a slash-command turn we
    // therefore deliver the text verbatim; the note is a general reminder that reappears on ordinary
    // turns and is irrelevant to a command invocation.
    if is_slash_command(text) {
        // trim_start so the slash sits at offset 0 even if the client sent leading whitespace —
        // the SDK only expands a command that is truly at the message start.
        return text.trim_start().to_string();
    }
    format!("{OUTBOX_NOTE}\n\n---\n\n{text}")
}

pub fn encode_user_message(text: &str) -> String {
    serde_json::json!({ "type": "user", "message": { "role": "user", "content": text } })
        .to_string()
        + "\n"
}

pub fn build_spec(opts: &SpawnOptions) -> RunSpec {
    // Map the turn's structured config onto a RunSpec. SdkRunner forwards
    // model/effort/mode/resume/hidden_skills/permission_mode to the Agent SDK as options — there is
    // no claude CLI argv anymore (the bridge owns all claude-CLI interaction).
    //
    // Merge process env, then opts.env, then set-or-clear CLAUDE_CONFIG_DIR.
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (k, v) in &opts.env {
        env.insert(k.clone(), v.clone());
    }
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        opts.claude_config_dir.clone().unwrap_or_default(),
    );

    RunSpec {
        cwd: opts.cwd.clone(),
        env,
        log_path: opts.log_path.clone(),
        unit: opts.unit.clone(),
        memory_max: opts.memory_max.clone(),
        memory_high: opts.memory_high.clone(),
        cpu_quota: opts.cpu_quota.clone(),
        tasks_max: opts.tasks_max.clone(),
        model: opts.model.clone(),
        effort: opts.effort.clone(),
        mode: opts.mode.clone(),
        resume_session_id: opts.resume_session_id.clone(),
        hidden_skills: opts.hidden_skills.clone(),
        enabled_plugins: opts.enabled_plugins.clone(),
        hidden_mcp_servers: opts.hidden_mcp_servers.clone(),
        extra_mcp_servers: opts.extra_mcp_servers.clone(),
        permission_mode: opts.permission_mode.clone(),
        forced_on_plugins: opts.forced_on_plugins.clone(),
        forced_on_skills: opts.forced_on_skills.clone(),
        forced_on_mcp_servers: opts.forced_on_mcp_servers.clone(),
        append_system_prompt: opts.append_system_prompt.clone(),
    }
}

pub fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Drives a started turn: polls the tailer every POLL_MS, forwarding ClaudeEvents over `events`, and
/// emits the exit code over `exit` once the run goes inactive (0 iff a clean `result` was seen).
pub struct SpawnHandle {
    pub(crate) run: Arc<dyn RunHandle>, // pub(crate) so the kill test can clone it
    pub events: mpsc::UnboundedReceiver<ClaudeEvent>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    poll_task: tokio::task::JoinHandle<()>,
    pub(crate) saw_result: Arc<std::sync::atomic::AtomicBool>, // cleared by write(); set in the poll loop
}

impl SpawnHandle {
    fn start(run: Arc<dyn RunHandle>, mut tailer: EventTailer) -> SpawnHandle {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let saw_result = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loop_run = run.clone();
        let loop_saw = saw_result.clone();
        let poll_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(POLL_MS));
            let mut exit_tx = Some(exit_tx);
            loop {
                tick.tick().await;
                for ev in tailer.poll() {
                    if let ClaudeEvent::Result {
                        is_error: false, ..
                    } = ev
                    {
                        loop_saw.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    let _ = events_tx.send(ev);
                }
                if !loop_run.is_active() {
                    let mut tail: Vec<ClaudeEvent> = tailer.poll();
                    tail.extend(tailer.flush());
                    for ev in tail {
                        if let ClaudeEvent::Result {
                            is_error: false, ..
                        } = ev
                        {
                            loop_saw.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        let _ = events_tx.send(ev);
                    }
                    let code = if loop_saw.load(std::sync::atomic::Ordering::SeqCst) {
                        0
                    } else {
                        1
                    };
                    if let Some(tx) = exit_tx.take() {
                        let _ = tx.send(code);
                    }
                    break;
                }
            }
        });
        SpawnHandle {
            run,
            events: events_rx,
            exit: exit_rx,
            poll_task,
            saw_result,
        }
    }

    pub fn kill(&self) {
        self.run.stop();
    }

    pub fn write(&self, line: &str) {
        self.saw_result
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.run.write(line);
    }

    pub fn end_input(&self) {
        self.run.end_input();
    }

    pub fn interrupt(&self) {
        // Only the SDK runner remains; it relays the interrupt to the bridge's query.interrupt()
        // (a real in-turn interrupt). The old stdin control_request fallback (for the raw-CLI
        // LocalRunner) is gone.
        self.run.interrupt();
    }

    pub fn detach(self) {
        self.poll_task.abort(); // stop polling, don't kill the run
    }
}

pub fn spawn_claude(opts: SpawnOptions, runner: &dyn Runner) -> SpawnHandle {
    // Capture the start offset BEFORE spawning. The child redirects stdout straight to the log file
    // and can begin writing immediately, so reading file_size AFTER runner.start() races: under load
    // the child may already have written the init/first deltas, and the tailer would skip them
    // (dropping the turn's session_id / early output). Pre-spawn EOF is 0 for a fresh turn, or the end
    // of the prior turn's log for a follow-up — exactly the offset we want.
    let start_offset = file_size(&opts.log_path);
    let run: Arc<dyn RunHandle> = Arc::from(runner.start(build_spec(&opts)));
    let tailer = EventTailer::new(&opts.log_path, start_offset);
    SpawnHandle::start(run, tailer)
}

#[cfg(test)]
mod slash_tests {
    use super::{compose_user_text, is_slash_command, OUTBOX_NOTE};

    #[test]
    fn slash_command_turn_drops_outbox_prefix_so_the_slash_leads() {
        // /lfg must reach the CLI at the very start, or the SDK won't expand it.
        let out = compose_user_text("/lfg add a CSV export to the orders page");
        assert!(out.starts_with("/lfg "), "slash must lead: {out:?}");
        assert!(!out.contains(OUTBOX_NOTE), "no outbox prefix on a command turn");
        // namespaced form too
        assert!(compose_user_text("/compound-engineering:lfg do it").starts_with("/compound"));
        // leading whitespace is trimmed so the slash still lands at offset 0
        assert!(compose_user_text("  /lfg go").starts_with("/lfg go"));
    }

    #[test]
    fn ordinary_turn_keeps_outbox_prefix() {
        let out = compose_user_text("add a CSV export, please");
        assert!(out.starts_with(OUTBOX_NOTE));
        assert!(out.ends_with("add a CSV export, please"));
    }

    #[test]
    fn paths_and_prose_are_not_treated_as_commands() {
        for t in [
            "/home/user/project/main.rs please review", // path: slash inside the token
            "/etc/hosts is the file",
            "no leading slash here",
            "/Uppercase is not a command",
            "/ spaced slash",
        ] {
            assert!(!is_slash_command(t), "should not be a command: {t:?}");
            assert!(compose_user_text(t).starts_with(OUTBOX_NOTE), "keeps prefix: {t:?}");
        }
    }
}
