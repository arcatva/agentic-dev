# Component CRUD Backend — Implementation Report

**Date:** 2026-07-10
**Branch:** HEAD (agentic/7a5b3fa6)

## Modules Added / Modified

| File | Status | Purpose |
|---|---|---|
| `server-rs/src/engine/user_config.rs` | Created | MCP server add/delete into `~/.claude.json` |
| `server-rs/src/engine/plugin_cli.rs` | Created | Plugin install/uninstall via `claude` CLI with timeout |
| `server-rs/src/engine/skills.rs` | Extended | `add_skill` and `delete_skill` functions added |
| `server-rs/src/engine/mod.rs` | Modified | Registered `user_config` and `plugin_cli` modules |
| `server-rs/src/api/validation.rs` | Created | `valid_component_name` and `valid_plugin_id` validators |
| `server-rs/src/api/mod.rs` | Modified | Added `validation` module + 6 new routes |
| `server-rs/src/api/misc.rs` | Modified | 6 new handler functions + tests |
| `server-rs/src/api/test_support.rs` | Modified | Added `claude_config_base` temp redirect to prevent real `~/.claude.json` writes in tests |

## Routes Added

| Method | Path | Handler | Success Response |
|---|---|---|---|
| POST | `/api/mcp-servers` | `mcp_add_route` | 200 `list_components()` |
| DELETE | `/api/mcp-servers/{name}` | `mcp_delete_route` | 200 `list_components()` or 404 |
| POST | `/api/skills` | `skills_add_route` | 200 `list_components()` |
| DELETE | `/api/skills/{name}` | `skills_delete_route` | 200 `list_components()` or 404 |
| POST | `/api/plugins` | `plugins_add_route` | 200 `list_components()` |
| DELETE | `/api/plugins/{id}` | `plugins_delete_route` | 200 `list_components()` or 500 on CLI failure |

All routes are authenticated by the existing axum auth middleware. Errors return `{"error": "<message>"}` with the appropriate status (400 validation, 404 not-found, 500 server error).

## Security Measures

### Name / ID Validation
- `valid_component_name(s)`: rejects empty strings, the reserved name `"agentic"`, and any character outside `[A-Za-z0-9._-]`. This blocks `/`, `..`, whitespace, `@`, and all shell-special characters in MCP and skill names. Validation fires in the handler BEFORE any filesystem or CLI action.
- `valid_plugin_id(s)`: rejects empty strings and any character outside `[A-Za-z0-9._@/-]`. This allows `<plugin>@<marketplace>` and `plugin/sub@x` style ids while blocking shell injection characters (`;`, `&`, space, etc.).

### Path-Traversal Defense for Skill Delete
`delete_skill` in `engine/skills.rs` does two checks:
1. API layer: `valid_component_name` rejects names containing `/` or `..` before the function is called.
2. Engine layer (defense-in-depth): after joining `skills_dir/name` and calling `canonicalize()` on both, it asserts that `canon_target.parent() == canon_skills_dir`. A name like `../outside` would canonicalize to a path outside `skills_dir` and be refused with `PermissionDenied` — `remove_dir_all` is never called.

### No Shell Injection
Plugin commands use `std::process::Command::new("claude").args([...])` — a vec of string arguments, no shell interpolation. The `CLAUDE_CONFIG_DIR` env var is set per-process, not concatenated into a shell string. `sh -c` is never used.

### Corrupt-Refuse + Backup + Atomic for `.claude.json`
`engine/user_config.rs` mirrors the pattern from `engine/global_settings.rs`:
- Missing file → treat as `{}` (safe to start fresh).
- Valid JSON but not an object → `Err(InvalidData)`, file untouched.
- Invalid JSON → `Err(InvalidData)`, file untouched.
- Before any write: backup to `<config_base>/backups/.claude.json.<millis>.bak` (pruned to 20 backups).
- Write via `crate::engine::atomic_write::write_file_atomic` (write to `.tmp`, fsync, rename, dir-fsync).
- A process-level `Mutex` serializes concurrent writes within the same process.

## Plugin CLI Timeout Approach

`run_plugin_command` in `engine/plugin_cli.rs`:
1. Spawns `Command::new("claude").args(args)` with `stdin=null`, `stdout=piped`, `stderr=piped`, `CLAUDE_CONFIG_DIR` env set.
2. Captures the child's pid before moving it.
3. Sends the child into a `std::thread::spawn` closure that calls `child.wait_with_output()`.
4. The main call site waits on an `mpsc::Receiver::recv_timeout(duration)`.
5. On timeout: calls `kill -9 <pid>` via another `Command::new("kill")` (best-effort, no new crates, no `libc`).
6. `install_plugin` uses 180s timeout; `uninstall_plugin` uses 60s.
7. Handlers call these functions via `tokio::task::spawn_blocking` so they don't block the async runtime.

## Test Evidence

### engine::user_config (8 tests)
- `add_stdio_server_round_trips` — stdio def written correctly; `type`/`url`/`headers` absent
- `add_http_server_round_trips` — http def written correctly; `command` absent
- `add_preserves_other_top_level_keys` — `otherKey` and existing `mcpServers` entries preserved
- `corrupt_file_refused` — `{corrupt` input → `InvalidData`, file byte-for-byte unchanged
- `delete_returns_true_and_removes_entry` — target removed, sibling preserved
- `delete_absent_returns_false` — returns `false` when key not present
- `delete_drops_empty_mcp_servers_map` — `mcpServers` key removed when last entry deleted
- `add_creates_backup` — one backup file in `backups/` after write

### engine::skills (5 new tests, 1 existing)
- `add_skill_creates_dir_and_skill_md` — correct SKILL.md content with frontmatter
- `add_skill_errors_if_already_exists` — `AlreadyExists` on duplicate
- `delete_skill_removes_dir_and_returns_true` — dir gone after delete
- `delete_skill_absent_returns_false` — returns `false` when dir not present
- `delete_skill_rejects_path_traversal` — `../outside-<pid>` refused with `PermissionDenied`; outside dir untouched

### engine::plugin_cli (1 test)
- `run_plugin_command_returns_quickly_when_binary_missing` — function returns in < 5s whether `claude` is installed or not (NotFound is handled cleanly)

### api::validation (4 tests)
- `valid_component_name_accepts_normal_names` — `my-server`, `my.server`, `ABC` all pass
- `valid_component_name_rejects_bad_names` — empty, `agentic`, `/`, `..`, space, `@` all fail
- `valid_plugin_id_accepts_marketplace_ids` — `gh@official`, `plugin/sub@x` etc. pass
- `valid_plugin_id_rejects_bad_ids` — empty, space, newline, `;`, `&` fail

### api::misc CRUD tests (7 new tests)
- `mcp_add_rejects_bad_name` — `../evil` → 400
- `mcp_add_rejects_reserved_agentic_name` — `agentic` → 400
- `mcp_add_and_delete_round_trip` — add → verify in `GET /api/global-settings` → delete → verify gone
- `mcp_delete_absent_is_404` — DELETE on unknown name → 404
- `skills_add_rejects_bad_name` — `a/b` → 400
- `skills_add_and_delete_round_trip` — add → verify in component list → delete
- `plugins_add_rejects_bad_id` — `a b` (space) → 400 (validation fires before any CLI call)

**Total test suite: 574 library tests + 22 integration tests — all passing.**

## What Is Manual-Only (No Unit Test)

- `install_plugin` / `uninstall_plugin` live paths: require `claude` CLI to be present. Not available in CI. The id validator (`valid_plugin_id`) and the 400 rejection path are covered by automated tests. The actual spawn + timeout + kill path is covered by the `run_plugin_command_returns_quickly_when_binary_missing` test only for the error case.
- Manual validation steps:
  1. `curl -X POST /api/plugins -H "Authorization: Bearer $TOKEN" -d '{"id":"gh@official"}' ...` — should call `claude plugin install gh@official` and return the updated component list.
  2. `curl -X DELETE /api/plugins/gh@official ...` — should call `claude plugin uninstall gh@official -y`.
  3. Confirm timeout kills: test with a slow plugin install and a deliberately short timeout.
