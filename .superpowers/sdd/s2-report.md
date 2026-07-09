# S2 — MCP Per-Session: Implementation Report

**Date:** 2026-07-09
**Branch:** `agentic/7a5b3fa6-51d9-4deb-a442-c30944eee3ab`
**Spec:** `docs/superpowers/specs/2026-07-09-s2-mcp-per-session-design.md`

---

## Commits

| SHA | Subject |
|-----|---------|
| `09abb7a` | feat(s2): add McpServerDef struct and list_user_mcp_servers enumeration |
| `226ddbe` | feat(s2): thread hiddenMcpServers + extraMcpServers through store and SubmitMeta |
| `34fa4ed` | feat(s2): inject SDK_BRIDGE_HIDDEN_MCP and SDK_BRIDGE_EXTRA_MCP at spawn |
| `8e03908` | feat(s2): API CreateBody accepts hiddenMcpServers + extraMcpServers with validation |
| `e469760` | feat(s2): sdk-bridge parses SDK_BRIDGE_HIDDEN_MCP + SDK_BRIDGE_EXTRA_MCP |

---

## Files Changed

| File | Change |
|------|--------|
| `server-rs/src/engine/store.rs` | Added `McpServerDef` struct; added `hidden_mcp_servers` + `extra_mcp_servers` to `Session`, `CreateInput`; added `hiddenMcpServers` + `extraMcpServers` to `ADDED_COLUMNS`; updated INSERT statement (+2 columns, +2 binds); updated `row_to_session` to read both columns; 2 new tests |
| `server-rs/src/engine/components.rs` | Added `list_user_mcp_servers(config_base)` function; called it in `list_components`; replaced the MCP stub comment; 3 new tests |
| `server-rs/src/engine/runner.rs` | Added `hidden_mcp_servers: Vec<String>` and `extra_mcp_servers: Vec<McpServerDef>` to `RunSpec` |
| `server-rs/src/engine/spawner.rs` | Added same two fields to `SpawnOptions`; passed them through `build_spec` |
| `server-rs/src/engine/sdk_runner.rs` | Added env injection for `SDK_BRIDGE_HIDDEN_MCP` and `SDK_BRIDGE_EXTRA_MCP` (hidden names removed from extra set); 2 new tests |
| `server-rs/src/engine/mod.rs` | Added `hidden_mcp_servers` + `extra_mcp_servers` to `SubmitMeta`; wired through `submit_session` CreateInput build and `fork_session` CreateInput build; copied session values into RunSpec |
| `server-rs/src/api/sessions.rs` | Added `hidden_mcp_servers` + `extra_mcp_servers` to `CreateBody`; added validation (empty name → 400, missing transport → 400, both transports → 400); wired into `SubmitMeta`; 4 new tests |
| `server-rs/src/api/misc.rs` | Added the two new `SubmitMeta` fields to the explicit template-based construction |
| `server-rs/src/engine/tests.rs` | Updated 5 explicit `SubmitMeta` constructions to include the two new fields |
| `server-rs/sdk-bridge.mjs` | Added `SDK_BRIDGE_HIDDEN_MCP` parsing → `settings.disabledMcpjsonServers`; `SDK_BRIDGE_EXTRA_MCP` parsing → `extraMcpServers` object; merged into `mcpServers` option alongside `agentic: delegateServer`; updated boot log line |

---

## DB Migration Approach

Two new nullable TEXT columns added to `ADDED_COLUMNS` (applied idempotently at startup via `ALTER TABLE IF NOT EXISTS`):

```
("hiddenMcpServers", "TEXT"),
("extraMcpServers", "TEXT"),
```

Both default to NULL when absent (i.e., rows from before this migration). `row_to_session` reads them with `try_get(...).ok().flatten()` and falls back to `vec![]` on NULL or parse failure — identical to how `hiddenPlugins` works. The INSERT always writes both columns (serialized JSON arrays).

---

## Store Round-Trip Validation

Test: `engine::store::tests::roundtrip_mcp_session_fields`
- Creates a session with `extra_mcp_servers` (one stdio, one http) and `hidden_mcp_servers`
- Reads it back from the DB via `store.get()`
- Asserts both fields match exactly (including the `transport: Some("http")` field)

Test: `engine::store::tests::old_session_without_mcp_columns_defaults_to_empty`
- Creates a legacy DB with only minimal columns (no `hiddenMcpServers` or `extraMcpServers`)
- Opens it with the new Store (migration adds the columns)
- Reads an existing row and asserts both MCP fields are empty `vec![]`

---

## Test Evidence

Command: `cd server-rs && cargo test`

Result:
```
test result: ok. 494 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.49s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.02s
```

New tests added (11 total):
- `engine::components::tests::lists_user_mcp_servers_from_claude_json` — MCP enumeration from fixture
- `engine::components::tests::list_user_mcp_servers_missing_file_returns_empty` — missing file → empty
- `engine::components::tests::list_components_includes_mcp_servers` — MCP appears in unified list
- `engine::store::tests::roundtrip_mcp_session_fields` — store insert+read round-trip
- `engine::store::tests::old_session_without_mcp_columns_defaults_to_empty` — legacy DB migration
- `engine::sdk_runner::tests::start_passes_mcp_envs_to_bridge` — env injection, hidden excluded
- `engine::sdk_runner::tests::start_omits_mcp_envs_when_empty` — empty fields → no env vars
- `api::sessions::tests::create_session_rejects_extra_mcp_with_no_transport` — missing transport → 400
- `api::sessions::tests::create_session_rejects_extra_mcp_with_empty_name` — empty name → 400
- `api::sessions::tests::create_session_rejects_extra_mcp_with_both_transports` — both transports → 400
- `api::sessions::tests::create_session_accepts_valid_extra_mcp_servers` — valid defs → 200

Build: `cargo build` — `Finished dev profile [unoptimized + debuginfo]` — 0 errors, 1 pre-existing dead_code warning.

---

## Bridge JS — Untested by Suite

The `sdk-bridge.mjs` changes (parsing `SDK_BRIDGE_HIDDEN_MCP` / `SDK_BRIDGE_EXTRA_MCP`, building `extraMcpServers`, merging into `mcpServers`) are NOT covered by the Rust test suite. The test suite uses fake shell bridge scripts (`fake-sdk-bridge-ok.sh`) which never execute the JS. The bridge JS needs live validation:

- Verify `disabledMcpjsonServers` is respected by Claude Code CLI when a `.mcp.json` server is hidden
- Verify `extraMcpServers` entries (stdio + http) are connected and callable by the agent
- Verify that a `hidden-one` entry in `hiddenMcpServers` is excluded from the `extraMcpServers` merged object

Syntax is validated with `node --check sdk-bridge.mjs` — passes.

---

## Concerns / Deviations

1. **`SpawnOptions` also needed the new fields.** The plan only mentioned `RunSpec` in `runner.rs` and the `mod.rs` build site, but `SpawnOptions` in `spawner.rs` is a parallel struct that also maps to `RunSpec` via `build_spec`. Added the fields there too — no deviation from spec intent, just a missed file in the plan.

2. **`tests.rs` and `misc.rs` needed patching.** Five explicit `SubmitMeta { ... }` constructions in `tests.rs` (not using `..Default::default()`) and one in `misc.rs` required the two new fields to compile. Fixed by adding `hidden_mcp_servers: vec![], extra_mcp_servers: vec![]` to each.

3. **Test assertion for `start_passes_mcp_envs_to_bridge` needed refinement.** The initial plan assertion used `split("SDK_BRIDGE_EXTRA_MCP=").nth(1)` which included the trailing `SDK_BRIDGE_HIDDEN_MCP=["hidden-one"]` text, causing a false-positive. Fixed by using `.lines().find(|l| l.starts_with("SDK_BRIDGE_EXTRA_MCP="))` to isolate the specific env line.

4. **`components.rs` test pollution concern.** Using `base.join(".claude")` as `config_base` avoids writing `.claude.json` to the system temp dir root. The test creates `<tmp>/agentic-comp-<pid>-<nanos>/.claude.json` — contained within the test-specific temp dir.

5. **No global write-back for MCP.** Per spec, `global_enabled` stays `true` for all MCP servers and there is no write-back to `~/.claude.json`. This is by design.
