use std::collections::HashMap;
use std::path::PathBuf;

/// What to run for one turn. The bridge appends stream-json to `log_path` (the session log file);
/// the engine's tailer reads it.
#[derive(Clone, Debug, Default)]
pub struct RunSpec {
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub log_path: PathBuf,
    pub unit: String, // diagnostic label
    // cgroup caps — carried for compatibility (not enforced since the systemd runner was removed).
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub cpu_quota: Option<String>,
    pub tasks_max: Option<String>,
    // Structured turn config — SdkRunner forwards these to the Agent SDK as options.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub resume_session_id: Option<String>,
    /// Per-session skill blacklist — SdkRunner forwards it to sdk-bridge as `skillOverrides`.
    pub hidden_skills: Vec<String>,
    /// Explicit per-plugin enable map (`<plugin>@<marketplace>` → enabled) — SdkRunner forwards it
    /// to sdk-bridge, which writes it verbatim as settings `enabledPlugins`. `true` force-enables
    /// (command-line settings layer wins over on-disk settings); `false` disables for this session.
    pub enabled_plugins: std::collections::BTreeMap<String, bool>,
    /// Per-session MCP server name blacklist. Bridge writes them to `settings.disabledMcpjsonServers`.
    pub hidden_mcp_servers: Vec<String>,
    /// Per-session ad-hoc MCP server definitions. Hidden names are removed by sdk_runner before injection.
    pub extra_mcp_servers: Vec<crate::engine::store::McpServerDef>,
    pub permission_mode: Option<String>,
    /// Plugin ids forced ON for this session (forcedOn > hidden > global).
    pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session that the global baseline disables → bridge sets "on".
    pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON (stored; no-op at spawn until global MCP disable exists).
    pub forced_on_mcp_servers: Vec<String>,
}

/// A started turn. Liveness + exit are polled. Optional methods default to no-ops.
pub trait RunHandle: Send + Sync {
    fn is_active(&self) -> bool;
    fn exit_code(&self) -> Option<i32>; // meaningful once is_active() is false
    fn stop(&self);
    fn interrupt(&self) {}
    fn write(&self, _line: &str) {}
    fn end_input(&self) {}
    /// Answer a parked perm/plan permission request (allow/deny + optional feedback). Implemented by
    /// the SDK runner (relays it to the bridge's parked `canUseTool`); a no-op on any other runner.
    fn respond_permission(&self, _decision: &str, _feedback: Option<&str>) {}
}

pub trait Runner: Send + Sync {
    fn start(&self, spec: RunSpec) -> Box<dyn RunHandle>;
}
