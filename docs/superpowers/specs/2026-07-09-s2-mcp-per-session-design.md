# S2 — MCP per-session (toggle existing + in-session ad-hoc add)

- **Date:** 2026-07-09
- **Status:** Spec (autonomous build — architecture pre-approved in the S4 program spec)
- **Builds on:** S4 (`components.rs`, `global_settings.rs`, `/api/global-settings`).

## Goal
Let a session (a) see the configured MCP servers, (b) disable specific ones for itself, and (c) add
an MCP server inline for that session only (not persisted to global config). Mirrors the existing
per-session `hiddenSkills` / `hiddenPlugins` mechanism.

## Scope (in)
1. **MCP enumeration (read).** `engine/components.rs` enumerates user-scope MCP servers from
   `~/.claude.json` → `mcpServers` (object of `{name: config}`). Emit `ComponentInfo{ kind:"mcp",
   id:name, name, description:"", source:"user", global_enabled:true }`. This surfaces MCP servers in
   `GET /api/global-settings`. (No global MCP write-back — see Deferred.)
2. **Per-session inline add — `extraMcpServers`.** New session field: a list of `McpServerDef`.
   Persisted (new DB TEXT column, JSON). At spawn, serialized to env `SDK_BRIDGE_EXTRA_MCP`; the
   bridge injects them into the Agent SDK `mcpServers` option alongside the in-process `agentic`
   delegate server. Supports stdio and http/sse transports.
3. **Per-session disable — `hiddenMcpServers`.** New session field: `Vec<String>` of server names to
   disable for this session (blacklist, exactly like `hiddenPlugins`). At spawn → env
   `SDK_BRIDGE_HIDDEN_MCP` (JSON name array). The bridge (a) writes `settings.disabledMcpjsonServers
   = [names]` (a real Claude Code settings key that disables `.mcp.json`-configured servers) and (b)
   skips injecting any `extraMcpServers` whose name is in the hidden list.

## Scope (out / deferred)
- **Global MCP toggle writing back to `~/.claude.json`** — there is no clean per-server *disable*
  key for user-scope servers (unlike `enabledPlugins`/`skillOverrides`), and editing the big
  `~/.claude.json` to remove/restore server defs is destructive. `global_enabled` stays `true` for
  MCP for now (mirrors the S4 MCP stub). Revisit if needed.
- **Project `.mcp.json` enumeration in the global list** — that file is per-repo/worktree, not a
  global concern; the per-session `disabledMcpjsonServers` path still governs those at runtime.

## Data model
```rust
// engine/store.rs (or a small engine/mcp.rs), serde camelCase on the wire.
pub struct McpServerDef {
    pub name: String,
    // stdio transport:
    #[serde(skip_serializing_if = "Option::is_none")] pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub env: Option<std::collections::BTreeMap<String, String>>,
    // http/sse transport:
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")] pub transport: Option<String>, // "http" | "sse"
    #[serde(skip_serializing_if = "Option::is_none")] pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub headers: Option<std::collections::BTreeMap<String, String>>,
}
```
**Validation** (reject at the API with 400): `name` non-empty; exactly one transport present —
either `command` (stdio) or `url` (http/sse). Unknown/missing → 400.

## Threading (mirror hiddenPlugins exactly)
- `store.rs`: `Session` + `CreateInput` gain `hidden_mcp_servers: Vec<String>` and
  `extra_mcp_servers: Vec<McpServerDef>`; two new DB columns `hiddenMcpServers`, `extraMcpServers`
  (TEXT, JSON) added to the migration list, the INSERT column list/bindings, and the row→Session read
  mapping. Both default to empty (`#[serde(default)]`) for old rows.
- `api/sessions.rs`: `CreateBody` gains `hiddenMcpServers: Option<Vec<String>>` and
  `extraMcpServers: Option<Vec<McpServerDef>>`; validate `extraMcpServers`; pass into `CreateInput`.
- `engine/runner.rs` `RunSpec`: gains `hidden_mcp_servers: Vec<String>` and
  `extra_mcp_servers: Vec<McpServerDef>`.
- `engine/mod.rs` RunSpec build (near the `enabled_plugins`/`hidden_skills` seam): copy the session's
  values through.
- `engine/sdk_runner.rs`: if non-empty, set env `SDK_BRIDGE_HIDDEN_MCP` = JSON name array and
  `SDK_BRIDGE_EXTRA_MCP` = JSON array of the (non-hidden) `McpServerDef`s.
- `sdk-bridge.mjs`: parse both envs. `SDK_BRIDGE_EXTRA_MCP` → for each def build an SDK server config
  (`command` present → `{command, args, env}`; `url` present → `{type: type||"http", url, headers}`)
  and merge into the `mcpServers` option next to `agentic: delegateServer`, skipping any whose name is
  in the hidden list. `SDK_BRIDGE_HIDDEN_MCP` → `settings.disabledMcpjsonServers = names`.

## Testing (Rust suite; bridge JS validated live/manually)
- `components.rs`: enumerates user MCP servers from a fixture `~/.claude.json`; kind/source correct.
- `store.rs`: round-trip a session with `extra_mcp_servers` + `hidden_mcp_servers` (insert → read →
  fields equal); old rows without the columns default to empty.
- `sdk_runner.rs`: env construction — `SDK_BRIDGE_EXTRA_MCP` holds the JSON defs (hidden ones removed),
  `SDK_BRIDGE_HIDDEN_MCP` holds the names; both unset when empty.
- `api/sessions.rs`: `extraMcpServers` validation — missing transport / empty name → 400.
- The bridge `.mjs` change has no suite coverage (the tests use a fake bash bridge). Keep the JS change
  small and defensive; note it needs live validation with a real MCP server.

## Constraints
- Engine stays axum-free; no new crate deps; `make test` green; `cargo build` compiles.
- DB change is additive (new nullable/defaulted columns); old sessions still load.

## Acceptance
- `GET /api/global-settings` lists user-scope MCP servers as `kind:"mcp"`.
- Creating a session with `extraMcpServers` persists them and produces `SDK_BRIDGE_EXTRA_MCP` at spawn;
  `hiddenMcpServers` produces `SDK_BRIDGE_HIDDEN_MCP` and removes hidden servers from the injected set.
- Invalid `extraMcpServers` → 400. Suite green; build compiles.
