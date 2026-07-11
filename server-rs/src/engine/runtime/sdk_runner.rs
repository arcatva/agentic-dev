//! Production turn runner: drives claude through the official Claude Agent SDK via a thin Node bridge
//! (`server-rs/sdk-bridge.mjs`). This is the only turn runner; there is no raw-`claude`-CLI runner.
//! Tests inject an SdkRunner pointed at a fake bridge script (`tests/fixtures/fake-sdk-bridge-*.sh`).
//!
//! Why a Node bridge instead of a pure-Rust port: the SDK is the harness side of claude's stream-json
//! control protocol (the `initialize` handshake + `can_use_tool` requests). That protocol is what makes
//! **AskUserQuestion pause in-turn** instead of erroring. Reimplementing it in Rust would mean
//! reverse-engineering a 1.1 MB minified bundle; reusing the SDK via a tiny per-turn Node child is
//! reliable and keeps every latency-sensitive path (HTTP/WS/engine/transcript) in Rust. The bridge
//! appends each SDK message to the session log file itself — the same shape the Rust tailer reads — so
//! the streamParser, transcript model, and Android client are all unchanged.
//!
//! Stderr capture: the bridge's stderr (which transitively includes the SDK's child `claude`
//! subprocess stderr) is now routed to `<log_path>.stderr` instead of /dev/null. On a thrown
//! `query()` failure the bridge reads the tail of that file and embeds it in the synthetic
//! error result, so a turn that fails with "Claude Code process exited with code 1" no longer
//! hides WHY the claude child exited — we see auth/oom/cli-mismatch/cwd-missing/etc. directly
//! in the session transcript. Off by default? No — on. The file is tiny (a few KB at most) and
//! it gets the next-failure root cause back into the transcript without a service redeploy.

use parking_lot::Mutex;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::engine::runner::{RunHandle, RunSpec, Runner};

pub struct SdkRunner {
    node_bin: String,
    bridge_path: String,
}

impl SdkRunner {
    pub fn new(bridge_path: impl Into<String>) -> Self {
        SdkRunner {
            node_bin: std::env::var("AGENTIC_NODE_BIN").unwrap_or_else(|_| "node".into()),
            bridge_path: bridge_path.into(),
        }
    }
    /// Constructor with an explicit node binary (tests inject a fake; diagnostics override node).
    pub fn with_node(node_bin: impl Into<String>, bridge_path: impl Into<String>) -> Self {
        SdkRunner {
            node_bin: node_bin.into(),
            bridge_path: bridge_path.into(),
        }
    }
}

struct SdkHandle {
    state: Arc<Mutex<(bool, Option<i32>)>>, // (active, exit_code)
    pid: Option<u32>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
}

impl SdkHandle {
    fn send(&self, line: &str) {
        let line = if line.ends_with('\n') {
            line.to_string()
        } else {
            format!("{line}\n")
        };
        if let Some(w) = self.stdin.lock().as_mut() {
            if let Err(e) = w.write_all(line.as_bytes()) {
                tracing::warn!("[sdk_runner] stdin write failed: {e}");
            } else if let Err(e) = w.flush() {
                tracing::warn!("[sdk_runner] stdin flush failed: {e}");
            }
        }
    }
}

impl RunHandle for SdkHandle {
    fn is_active(&self) -> bool {
        self.state.lock().0
    }
    fn exit_code(&self) -> Option<i32> {
        self.state.lock().1
    }
    fn stop(&self) {
        // SIGTERM the bridge's process group → the bridge abort()s the SDK query and exits; the kill
        // also reaps the claude grandchild.
        //
        // Signal via the kill(2) SYSCALL, not a kill command: the external binary is not portable
        // for negative (process-group) pids (procps on Debian/Ubuntu parses `-<pid>` as an option),
        // and shell-builtin syntax varies (dash vs bash `--` handling) — both left bridges alive on
        // GitHub runners while util-linux kill on the dev host masked the bug. The syscall has no
        // parser to disagree with.
        //
        // Then ESCALATE to SIGKILL after a short grace: a SIG_IGN disposition inherited across exec
        // (seen under some CI/job supervisors) makes TERM a silent no-op, and a wedged bridge might
        // never honor it — stop() must be a guarantee, not a request. SIGKILL cannot be ignored.
        let Some(pid) = self.pid else { return };
        let pgid = -(pid as i32);
        // SAFETY: kill(2) with a signal number takes no pointers; failure returns -1 harmlessly.
        if unsafe { libc::kill(pgid, libc::SIGTERM) } != 0 {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        let state = self.state.clone();
        std::thread::spawn(move || {
            for _ in 0..20 {
                if !state.lock().0 {
                    return; // exited within the 2s grace — no escalation
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // SAFETY: as above; ESRCH after a natural exit is fine.
            unsafe {
                libc::kill(pgid, libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        });
    }
    fn write(&self, line: &str) {
        // A user turn (first + follow-ups). The bridge feeds it to the live SDK query, or — if a
        // question is parked — treats it as the AskUserQuestion answer.
        self.send(line);
    }
    fn end_input(&self) {
        self.send("{\"__bridge\":\"end\"}");
    }
    fn interrupt(&self) {
        self.send("{\"__bridge\":\"interrupt\"}");
    }
    fn respond_permission(&self, decision: &str, feedback: Option<&str>) {
        // Relay the app's allow/deny to the bridge's parked canUseTool. The bridge resolves the single
        // live perm/plan request, writes the agentic_perm_resolved marker, and (on a plan approve)
        // switches the live query to `default`. Only one request is ever parked, so no id is needed.
        let line = serde_json::json!({
            "__bridge": "perm",
            "decision": decision,
            "feedback": feedback,
        })
        .to_string();
        self.send(&line);
    }
}

impl Runner for SdkRunner {
    fn start(&self, spec: RunSpec) -> Box<dyn RunHandle> {
        // Structured trace: every bridge spawn logs the exact argv/env we'll pass. Pairs
        // with the engine's `evt=spawn_opts_resolved` and bridge's `[sdk-bridge] boot`
        // line — together they form a self-contained timeline of "what did the engine
        // decide, what did the runner hand to node, what did the bridge do with it".
        let stderr_capture_path = format!("{}.stderr", spec.log_path.to_string_lossy());
        tracing::info!(
            evt = "bridge_spawning",
            session_id = %spec.unit,
            cwd = %spec.cwd,
            bridge_path = %self.bridge_path,
            log_path = %spec.log_path.display(),
            stderr_capture_path = %stderr_capture_path,
            resume_session_id = ?spec.resume_session_id,
            model = ?spec.model,
            permission_mode = ?spec.permission_mode,
            hidden_skills = ?spec.hidden_skills,
            enabled_plugins = ?spec.enabled_plugins,
        );
        let mut cmd = Command::new(&self.node_bin);
        cmd.arg(&self.bridge_path)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env) // spec.env already merges the process env + per-session CLAUDE_CONFIG_DIR
            .env(
                "SDK_BRIDGE_LOG",
                spec.log_path.to_string_lossy().to_string(),
            )
            // SDK_BRIDGE_STDERR is set unconditionally so the bridge knows where to write
            // (and later read back) the captured stderr. It's empty when the runner falls
            // back to Stdio::null (file-open failed), in which case the bridge skips capture.
            .env("SDK_BRIDGE_STDERR", {
                if std::env::var("AGENTIC_SDK_BRIDGE_DEBUG").is_ok() {
                    String::new() // inherit mode: bridge writes to its own stderr (visible in journald)
                } else {
                    format!("{}.stderr", spec.log_path.to_string_lossy())
                }
            })
            .env("SDK_BRIDGE_CWD", &spec.cwd)
            .stdin(Stdio::piped())
            // The bridge appends SDK messages to SDK_BRIDGE_LOG itself; its own stdout is just
            // diagnostics → drop it (set AGENTIC_SDK_BRIDGE_DEBUG to inherit for debugging).
            //
            // Stderr: route to `<log_path>.stderr` so the bridge can read its tail on a thrown
            // query() and embed it in the synthetic error result. This is how we recover the
            // "Claude Code process exited with code 1" root cause (auth/oom/cli-version/etc.) —
            // previously this was sent to /dev/null and we lost all signal. Override with
            // AGENTIC_SDK_BRIDGE_DEBUG=1 to inherit stderr to the parent process for live debugging.
            .stdout(Stdio::null())
            .stderr({
                if std::env::var("AGENTIC_SDK_BRIDGE_DEBUG").is_ok() {
                    tracing::info!(
                        evt = "stderr_pipe_inherit",
                        session_id = %spec.unit,
                        reason = "AGENTIC_SDK_BRIDGE_DEBUG is set",
                    );
                    Stdio::inherit()
                } else {
                    let stderr_path = format!("{}.stderr", spec.log_path.to_string_lossy());
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&stderr_path)
                    {
                        Ok(_f) => {
                            tracing::info!(
                                evt = "stderr_pipe_opened",
                                session_id = %spec.unit,
                                path = %stderr_path,
                            );
                            Stdio::from(_f)
                        }
                        Err(e) => {
                            // Graceful degradation: drop stderr. The bridge will see
                            // SDK_BRIDGE_STDERR="" and skip capture. We log loudly so the
                            // operator notices the missing capture (the next failure will
                            // be opaque again).
                            tracing::error!(
                                evt = "stderr_pipe_open_failed",
                                session_id = %spec.unit,
                                path = %stderr_path,
                                error = %e,
                                effect = "stderr capture disabled for this turn",
                            );
                            Stdio::null()
                        }
                    }
                }
            });
        if let Some(m) = spec.model.as_deref().filter(|s| !s.is_empty()) {
            cmd.env("SDK_BRIDGE_MODEL", m);
        }
        if let Some(r) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
            cmd.env("SDK_BRIDGE_RESUME", r);
        }
        if let Some(md) = spec.mode.as_deref().filter(|s| !s.is_empty()) {
            cmd.env("SDK_BRIDGE_MODE", md);
        }
        if let Some(e) = spec.effort.as_deref().filter(|s| !s.is_empty()) {
            cmd.env("SDK_BRIDGE_EFFORT", e);
        }
        if let Some(p) = spec.permission_mode.as_deref().filter(|s| !s.is_empty()) {
            cmd.env("SDK_BRIDGE_PERMISSION_MODE", p);
        }
        // Tier-1 harness rules (routing + fan-out discipline) → appended to Claude Code's system prompt
        // in the bridge. Set only on main session turns; worker specs leave this None, so a delegate
        // worker never carries the orchestrator-only rules.
        if let Some(sp) = spec
            .append_system_prompt
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            cmd.env("SDK_BRIDGE_APPEND_SYSTEM_PROMPT", sp);
        }
        let hidden_skills: Vec<&str> = spec
            .hidden_skills
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !hidden_skills.is_empty() {
            if let Ok(json) = serde_json::to_string(&hidden_skills) {
                cmd.env("SDK_BRIDGE_HIDDEN_SKILLS", json);
            }
        }
        // Forced-on skills: globally-off skills the session forces back ON.
        // The bridge applies these as skillOverrides[name]="on" AFTER the "off" entries so
        // forced-on wins even when a name appears in both (belt-and-suspenders; the API
        // rejects that combination, but we stay safe here too).
        let forced_on_skills: Vec<&str> = spec
            .forced_on_skills
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !forced_on_skills.is_empty() {
            if let Ok(json) = serde_json::to_string(&forced_on_skills) {
                cmd.env("SDK_BRIDGE_FORCED_ON_SKILLS", json);
            }
        }
        // Explicit per-plugin enable map (see RunSpec::enabled_plugins). BTreeMap → compact JSON
        // object with sorted keys; blank ids are dropped defensively (mirrors the skills filtering).
        let enabled_plugins: std::collections::BTreeMap<&str, bool> = spec
            .enabled_plugins
            .iter()
            .map(|(k, v)| (k.trim(), *v))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        if !enabled_plugins.is_empty() {
            if let Ok(json) = serde_json::to_string(&enabled_plugins) {
                cmd.env("SDK_BRIDGE_ENABLED_PLUGINS", json);
            }
        }
        // Per-session MCP hidden list → SDK_BRIDGE_HIDDEN_MCP (JSON name array).
        let hidden_mcp: Vec<&str> = spec
            .hidden_mcp_servers
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !hidden_mcp.is_empty() {
            if let Ok(json) = serde_json::to_string(&hidden_mcp) {
                cmd.env("SDK_BRIDGE_HIDDEN_MCP", json);
            }
        }
        // Per-session extra MCP defs → SDK_BRIDGE_EXTRA_MCP (JSON array, hidden names removed).
        let hidden_set: std::collections::HashSet<&str> = hidden_mcp.iter().copied().collect();
        let extra_mcp: Vec<&crate::engine::store::McpServerDef> = spec
            .extra_mcp_servers
            .iter()
            .filter(|d| !d.name.trim().is_empty() && !hidden_set.contains(d.name.trim()))
            .collect();
        if !extra_mcp.is_empty() {
            if let Ok(json) = serde_json::to_string(&extra_mcp) {
                cmd.env("SDK_BRIDGE_EXTRA_MCP", json);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // new group so stop() can SIGTERM the bridge + its claude child
        }

        let state = Arc::new(Mutex::new((true, None::<i32>)));
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // Graceful degradation: missing node / bridge → inactive handle, exit code 127.
                tracing::error!(
                    evt = "bridge_spawn_failed",
                    session_id = %spec.unit,
                    node_bin = %self.node_bin,
                    bridge_path = %self.bridge_path,
                    cwd = %spec.cwd,
                    error = %e,
                    action = "returning inactive handle with exit code 127",
                );
                return Box::new(SdkHandle {
                    state: Arc::new(Mutex::new((false, Some(127)))),
                    pid: None,
                    stdin: Mutex::new(None),
                });
            }
        };
        let pid = child.id();
        let stdin = child.stdin.take();
        let st = state.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let mut g = st.lock();
            g.0 = false;
            g.1 = code;
        });
        Box::new(SdkHandle {
            state,
            pid: Some(pid),
            stdin: Mutex::new(stdin),
        })
    }
}

/// Default bridge path: `AGENTIC_SDK_BRIDGE` env, else `<dir-of-exe>/sdk-bridge.mjs` (packaged
/// install: binary and bridge side by side), else `<dir-of-exe>/../../sdk-bridge.mjs`
/// (repo layout: target/release/<bin> → server-rs/sdk-bridge.mjs).
///
/// When nothing exists, returns the packaged-layout path anyway so the boot preflight prints an
/// accurate "SDK bridge script missing: <path>" error (there is no machine-specific fallback).
pub fn default_bridge_path() -> String {
    if let Ok(p) = std::env::var("AGENTIC_SDK_BRIDGE") {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        // Packaged layout: <install-dir>/agentic-dev-server + <install-dir>/sdk-bridge.mjs
        if let Some(p) = exe.parent().map(|d| d.join("sdk-bridge.mjs")) {
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
        // Repo layout: target/release/agentic-dev-server → up to server-rs/
        if let Some(p) = exe.ancestors().nth(3).map(|d| d.join("sdk-bridge.mjs")) {
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
        // Nothing found: report the packaged-layout path so the preflight error is actionable.
        if let Some(p) = exe.parent().map(|d| d.join("sdk-bridge.mjs")) {
            return p.to_string_lossy().into_owned();
        }
    }
    "sdk-bridge.mjs".into()
}

/// Startup preflight for the Node SDK bridge. Returns human-readable problems (empty = all good).
/// Run at boot so a missing `npm install` / `node` surfaces immediately in the logs instead of
/// silently failing every turn at runtime (the bridge is the SOLE production claude transport).
pub fn preflight(node_bin: &str, bridge_path: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let bridge = std::path::Path::new(bridge_path);
    if !bridge.exists() {
        problems.push(format!(
            "SDK bridge script missing: {bridge_path} (set AGENTIC_SDK_BRIDGE to its path)"
        ));
    }
    if !sdk_installed(bridge) {
        let dir = bridge
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".into());
        problems.push(format!(
            "@anthropic-ai/claude-agent-sdk not installed — every turn will fail. Fix: (cd {dir} && npm install)"
        ));
    }
    if !node_on_path(node_bin) {
        problems.push(format!(
            "node binary '{node_bin}' not found on PATH (set AGENTIC_NODE_BIN to an absolute path)"
        ));
    }
    problems
}

/// Mirror node's module resolution for the bridge: walk up from the bridge's directory looking for
/// `node_modules/@anthropic-ai/claude-agent-sdk`.
fn sdk_installed(bridge: &std::path::Path) -> bool {
    let mut dir = bridge.parent();
    while let Some(d) = dir {
        if d.join("node_modules/@anthropic-ai/claude-agent-sdk/package.json")
            .exists()
        {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// Is `node_bin` runnable? An explicit path must exist; a bare name is searched on PATH.
fn node_on_path(node_bin: &str) -> bool {
    if node_bin.contains('/') {
        return std::path::Path::new(node_bin).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(node_bin).exists()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::{RunSpec, Runner};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmpdir() -> std::path::PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "sdkr-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A fake "node": ignores the bridge-path arg, records the SDK_BRIDGE_* env + every stdin line to
    /// `rec`, writes one fake turn (init + success result) to SDK_BRIDGE_LOG, then stays alive reading
    /// stdin (a persistent session) until EOF or SIGTERM — exercising the SdkRunner contract without
    /// the real SDK/claude.
    fn fake_node(dir: &std::path::Path, rec: &std::path::Path) -> String {
        let script = format!(
            "#!/bin/sh\nenv | grep '^SDK_BRIDGE' >> '{rec}'\nprintf '{{\"type\":\"system\",\"subtype\":\"init\"}}\\n{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}\\n' >> \"$SDK_BRIDGE_LOG\"\nwhile IFS= read -r line; do echo \"STDIN:$line\" >> '{rec}'; done\n",
            rec = rec.display()
        );
        let p = dir.join("fakenode.sh");
        std::fs::write(&p, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn preflight_ok_when_bridge_sdk_and_node_present() {
        let dir = tmpdir();
        let bridge = dir.join("sdk-bridge.mjs");
        std::fs::write(&bridge, "// bridge").unwrap();
        let sdk = dir.join("node_modules/@anthropic-ai/claude-agent-sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(sdk.join("package.json"), "{}").unwrap();
        // "/bin/sh" is an explicit path that exists → stands in for a real node binary.
        let problems = preflight("/bin/sh", &bridge.to_string_lossy());
        assert!(
            problems.is_empty(),
            "expected no problems, got {problems:?}"
        );
    }

    #[test]
    fn preflight_flags_missing_sdk_with_fix_command() {
        let dir = tmpdir();
        let bridge = dir.join("sdk-bridge.mjs");
        std::fs::write(&bridge, "// bridge").unwrap();
        // no node_modules next to the bridge
        let problems = preflight("/bin/sh", &bridge.to_string_lossy());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("claude-agent-sdk") && p.contains("npm install")),
            "must flag the missing SDK with the npm-install fix: {problems:?}"
        );
    }

    #[test]
    fn preflight_flags_missing_bridge_and_node() {
        let dir = tmpdir();
        let bridge = dir.join("does-not-exist.mjs");
        let problems = preflight("/no/such/node", &bridge.to_string_lossy());
        assert!(
            problems.iter().any(|p| p.contains("bridge")),
            "flags missing bridge: {problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("node")),
            "flags missing node: {problems:?}"
        );
    }

    #[test]
    fn sdk_runner_wires_env_writes_log_persists_and_stops() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("turn.jsonl");
        let runner = SdkRunner::with_node(fake_node(&dir, &rec), "/ignored/sdk-bridge.mjs");
        let mut env = HashMap::new();
        env.insert(
            "HOME".to_string(),
            std::env::var("HOME").unwrap_or_default(),
        );
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );
        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log.clone(),
            model: Some("opus".into()),
            append_system_prompt: Some("HARNESS_MARKER".into()),
            ..Default::default()
        };
        let h = runner.start(spec);
        h.write("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}");
        // Poll for the fake node to write the turn log + record stdin instead of a fixed sleep —
        // under parallel-test load 400ms was not always enough, which made this flake.
        let (mut logged, mut recorded) = (String::new(), String::new());
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            logged = std::fs::read_to_string(&log).unwrap_or_default();
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            // Break on the full STDIN payload, not just the "STDIN:" prefix — the recorder appends
            // the line in one write, but polling can still observe it before the value is flushed.
            if logged.contains("\"subtype\":\"success\"") && recorded.contains("\"content\":\"hi\"")
            {
                break;
            }
        }
        assert!(
            h.is_active(),
            "the bridge stays alive after a turn (persistent session)"
        );
        assert!(
            logged.contains("\"subtype\":\"init\"") && logged.contains("\"subtype\":\"success\""),
            "bridge wrote the turn to the log: {logged}"
        );
        assert!(
            recorded.contains("SDK_BRIDGE_LOG="),
            "log env wired: {recorded}"
        );
        assert!(
            recorded.contains("SDK_BRIDGE_MODEL=opus"),
            "model wired: {recorded}"
        );
        assert!(
            recorded.contains("SDK_BRIDGE_APPEND_SYSTEM_PROMPT=HARNESS_MARKER"),
            "append-system-prompt wired: {recorded}"
        );
        assert!(
            recorded.contains("STDIN:") && recorded.contains("\"content\":\"hi\""),
            "the user turn reached the bridge over stdin: {recorded}"
        );
        h.stop();
        // Generous deadline: on loaded CI runners the process-group kill + reaper-thread update
        // can take several seconds; a genuine failure still fails, just later.
        for _ in 0..300 {
            if !h.is_active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!h.is_active(), "stop() terminates the bridge");
    }

    #[test]
    fn respond_permission_sends_bridge_control_line() {
        // The runner must relay an allow/deny to the bridge as a {"__bridge":"perm",...} stdin line —
        // this is the wire the new POST /permission round-trip rides. (Whether the real SDK then routes
        // tools through canUseTool is a separate, SDK-internal concern proven only by the manual smoke
        // test; here we prove our half: the control line reaches the bridge with the right shape.)
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("turn.jsonl");
        let runner = SdkRunner::with_node(fake_node(&dir, &rec), "/ignored/sdk-bridge.mjs");
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );
        let h = runner.start(RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            ..Default::default()
        });
        h.respond_permission("allow", Some("looks good"));
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            if recorded.contains("\"__bridge\":\"perm\"") && recorded.contains("looks good") {
                break;
            }
        }
        h.stop();
        assert!(
            recorded.contains("\"__bridge\":\"perm\"")
                && recorded.contains("\"decision\":\"allow\"")
                && recorded.contains("\"feedback\":\"looks good\""),
            "respond_permission must send a perm control line with decision+feedback; recorded={recorded}"
        );
    }

    #[test]
    fn start_passes_hidden_skills_to_bridge_env() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            hidden_skills: vec!["rke2-ops".into(), "".into(), "cloudstack-ops".into()],
            ..Default::default()
        };

        let handle = runner.start(spec);
        // Poll for the fake node to record its env instead of a fixed sleep — under parallel-test
        // load the child can take >100ms to write `rec`, which made this flake.
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            // Break on the FULL expected value, not the prefix — `env | grep >> rec` is a multi-line
            // write the poll can otherwise catch mid-line (prefix present, value not yet flushed).
            if recorded.contains("SDK_BRIDGE_HIDDEN_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]") {
                break;
            }
        }
        handle.stop();

        assert!(
            recorded.contains("SDK_BRIDGE_HIDDEN_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]"),
            "hidden skills env must be compact JSON without blank entries; recorded={recorded}"
        );
    }

    #[test]
    fn start_passes_forced_on_skills_to_bridge_env() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            forced_on_skills: vec!["rke2-ops".into(), "".into(), "cloudstack-ops".into()],
            ..Default::default()
        };

        let handle = runner.start(spec);
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            if recorded.contains("SDK_BRIDGE_FORCED_ON_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]") {
                break;
            }
        }
        handle.stop();

        assert!(
            recorded.contains("SDK_BRIDGE_FORCED_ON_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]"),
            "forced-on skills env must be compact JSON without blank entries; recorded={recorded}"
        );
    }

    #[test]
    fn start_passes_enabled_plugins_to_bridge_env() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            enabled_plugins: std::collections::BTreeMap::from([
                ("superpowers@official".to_string(), true),
                ("".to_string(), true), // blank id must be dropped
                ("github@official".to_string(), false),
            ]),
            ..Default::default()
        };

        let handle = runner.start(spec);
        // Poll for the fake node to record its env (same anti-flake loop as the skills test).
        let expected =
            "SDK_BRIDGE_ENABLED_PLUGINS={\"github@official\":false,\"superpowers@official\":true}";
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            if recorded.contains(expected) {
                break;
            }
        }
        handle.stop();

        assert!(
            recorded.contains(expected),
            "enabled plugins env must be a compact, sorted JSON object without blank ids; recorded={recorded}"
        );
    }

    #[test]
    fn missing_node_degrades_to_inactive_127_not_panic() {
        let runner = SdkRunner::with_node("/no/such/node/binary", "/x.mjs");
        let h = runner.start(RunSpec {
            log_path: tmpdir().join("l"),
            ..Default::default()
        });
        assert!(!h.is_active());
        assert_eq!(h.exit_code(), Some(127));
    }

    /// A fake "node" that prints to stderr — verifies the runner routes the bridge's stderr to
    /// `<log_path>.stderr` so the bridge can later read it back on a query() throw and embed it
    /// in the synthetic error result. Without this, "Claude Code process exited with code 1"
    /// failures have no actionable stderr to look at.
    fn fake_node_with_stderr(dir: &std::path::Path) -> String {
        let script =
            "#!/bin/sh\necho 'fake stderr line 1' 1>&2\necho 'fake stderr line 2' 1>&2\nsleep 60\n";
        let p = dir.join("fakenode-stderr.sh");
        std::fs::write(&p, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn bridge_stderr_is_routed_to_dot_stderr_file() {
        let dir = tmpdir();
        let log = dir.join("turn.jsonl");
        let stderr_expected = std::path::Path::new(&log).with_extension("jsonl.stderr");
        // sanity: file does not pre-exist
        assert!(!stderr_expected.exists());
        let runner = SdkRunner::with_node(fake_node_with_stderr(&dir), "/ignored/sdk-bridge.mjs");
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );
        let h = runner.start(RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log.clone(),
            ..Default::default()
        });
        // Poll for the fake node's stderr to reach <log_path>.stderr instead of a fixed sleep
        // (flaky under parallel-test load).
        let mut captured = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            captured = std::fs::read_to_string(&stderr_expected).unwrap_or_default();
            if captured.contains("fake stderr line 1") && captured.contains("fake stderr line 2") {
                break;
            }
        }
        h.stop();
        for _ in 0..60 {
            if !h.is_active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            captured.contains("fake stderr line 1") && captured.contains("fake stderr line 2"),
            "bridge stderr must be captured to <log_path>.stderr: {captured}"
        );
    }

    #[test]
    fn start_passes_mcp_envs_to_bridge() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            hidden_mcp_servers: vec!["hidden-one".into()],
            extra_mcp_servers: vec![
                crate::engine::store::McpServerDef {
                    name: "extra-mcp".into(),
                    command: Some("npx".into()),
                    args: Some(vec!["my-server".into()]),
                    ..Default::default()
                },
                crate::engine::store::McpServerDef {
                    name: "hidden-one".into(), // must be excluded from EXTRA_MCP
                    command: Some("npx".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let handle = runner.start(spec);
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            if recorded.contains("SDK_BRIDGE_HIDDEN_MCP")
                && recorded.contains("SDK_BRIDGE_EXTRA_MCP")
            {
                break;
            }
        }
        handle.stop();

        assert!(
            recorded.contains("SDK_BRIDGE_HIDDEN_MCP=[\"hidden-one\"]"),
            "hidden MCP names must be a JSON array; recorded={recorded}"
        );
        // Extra MCP must contain "extra-mcp" and must NOT contain "hidden-one" in the EXTRA var.
        // Parse just the line that starts with SDK_BRIDGE_EXTRA_MCP= to avoid false-positives from
        // SDK_BRIDGE_HIDDEN_MCP appearing later in the recorded env dump.
        let extra_mcp_line = recorded
            .lines()
            .find(|l| l.starts_with("SDK_BRIDGE_EXTRA_MCP="))
            .unwrap_or("");
        assert!(
            extra_mcp_line.contains("\"extra-mcp\""),
            "extra MCP must be set and contain extra-mcp; recorded={recorded}"
        );
        assert!(
            !extra_mcp_line.contains("hidden-one"),
            "hidden-one must be excluded from extra MCP; extra_mcp_line={extra_mcp_line}"
        );
    }

    #[test]
    fn start_omits_mcp_envs_when_empty() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            ..Default::default()
        };
        let handle = runner.start(spec);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let recorded = std::fs::read_to_string(&rec).unwrap_or_default();
        handle.stop();

        assert!(
            !recorded.contains("SDK_BRIDGE_HIDDEN_MCP"),
            "must not set SDK_BRIDGE_HIDDEN_MCP when empty; recorded={recorded}"
        );
        assert!(
            !recorded.contains("SDK_BRIDGE_EXTRA_MCP"),
            "must not set SDK_BRIDGE_EXTRA_MCP when empty; recorded={recorded}"
        );
    }
}
