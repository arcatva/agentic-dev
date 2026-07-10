# Component CRUD — add/delete skills, plugins, MCP (backend)

- **Date:** 2026-07-10
- **Status:** Spec (autonomous build)
- **Repo:** agentic-dev (backend). Android UI is a follow-up.

## Goal
Let the app add and delete the three component kinds. Storage differs per kind, which drives the
implementation:
- **MCP** → a JSON edit of `~/.claude.json` `mcpServers` (surgical, backed-up, atomic).
- **Skills** → create/remove a folder under `<skills_dir>` (`~/.claude/skills/<name>/`).
- **Plugins** → shell out to the `claude` CLI (`plugin install` / `plugin uninstall`).

## API (all authed, same as existing routes)
1. **MCP add:** `POST /api/mcp-servers` body = `McpServerDef` (reuse `engine::store::McpServerDef`:
   `{name, command?, args?, env?, type?, url?, headers?}`). Upserts `~/.claude.json`
   `mcpServers[name] = <config>`. → 200 (refreshed component list) or 4xx.
2. **MCP delete:** `DELETE /api/mcp-servers/{name}` → remove `mcpServers[name]`. → 200 or 404.
3. **Skill add:** `POST /api/skills` body `{name, description}` → create
   `<skills_dir>/<name>/SKILL.md` with frontmatter. → 200 or 4xx.
4. **Skill delete:** `DELETE /api/skills/{name}` → recursively remove `<skills_dir>/<name>/`. → 200 or 404.
5. **Plugin add:** `POST /api/plugins` body `{id}` (`<plugin>@<marketplace>`) → run
   `claude plugin install <id>`. → 200 (with output) or 4xx/5xx (with stderr).
6. **Plugin delete:** `DELETE /api/plugins/{id}` → run `claude plugin uninstall <id> -y`. → 200 or 5xx.

Return shape: success routes return the refreshed `list_components(...)` array (so the client
re-renders); errors return `{error}` with the right status.

## Storage details
### MCP (`engine/user_config.rs`, new)
- File: `<claude_config_base>/../.claude.json` (the user file; sibling of `.claude`). Reuse the
  same derivation as `list_user_mcp_servers`.
- `add_mcp_server(config_base, def)`: read `.claude.json` (missing → `{}`, corrupt → **Err**, never
  clobber), back it up to `<config_base>/backups/`, set `mcpServers[def.name] = serialize(def)`
  (stdio → `{command,args,env}`, http → `{type,url,headers}`; omit null fields), atomic write.
  Preserve ALL other keys in the big file.
- `delete_mcp_server(config_base, name)`: remove `mcpServers[name]`; drop the `mcpServers` object if
  it becomes empty; atomic write + backup. 404 if absent.
- Reuse `atomic_write::write_file_atomic` and the backup approach from `global_settings.rs`.

### Skills (`engine/skills.rs`, extend)
- `add_skill(skills_dir, name, description)`: create `<skills_dir>/<name>/` and write `SKILL.md`:
  `---\nname: <name>\ndescription: <description>\n---\n`. Error if the dir already exists.
- `delete_skill(skills_dir, name)`: `remove_dir_all(<skills_dir>/<name>)`. 404 if absent.

### Plugins (`engine/plugin_cli.rs`, new)
- `install_plugin(config_base, id) -> Result<String, String>`: run
  `Command::new("claude").args(["plugin","install", id]).env("CLAUDE_CONFIG_DIR", config_base)`
  with inherited PATH, a stdin of `/dev/null`, a **timeout** (e.g. 180s), capture stdout+stderr.
  Non-zero exit → Err(stderr).
- `uninstall_plugin(config_base, id) -> Result<String, String>`: same with
  `["plugin","uninstall", id, "-y"]`, timeout ~60s.
- Use `std::process::Command` (NO shell) so args can't inject. Run on a blocking thread
  (`tokio::task::spawn_blocking`) with a wall timeout (kill the child on timeout).

## SECURITY (must-haves — these are the adversarial-review focus)
- **Skill/MCP name sanitization:** reject names not matching `^[A-Za-z0-9._-]+$` (400). This blocks
  `/`, `..`, absolute paths, and whitespace. For skill delete, ALSO canonicalize the target and
  assert it is a direct child of `skills_dir` (defense in depth) before `remove_dir_all`.
- **Reserve `agentic`** for MCP name (the delegate server) — 400.
- **Plugin id:** reject ids not matching `^[A-Za-z0-9._@/-]+$` (400) before shelling out. Args are
  passed as a vec (no shell), so even so there is no injection surface, but validate anyway.
- **Never** interpolate any user input into a shell string. No `sh -c`.
- Writes to `~/.claude.json` follow the corrupt-refuse + backup + atomic rules (never wipe the big
  user file).

## Wiring
- `api/misc.rs` (or `api/components.rs`): 6 handlers using `st.config.claude_config_base` /
  `st.config.skills_dir`. Engine stays axum-free; the shell-out + fs live in engine modules.
- Register the 6 routes in `api/mod.rs`.

## Testing (Rust unit + api)
- MCP add/delete round-trips against a temp `.claude.json`; corrupt file refused; name `agentic`
  and bad names → rejected by the validator; other keys preserved.
- Skill add creates the dir+SKILL.md; delete removes it; path-traversal names (`../x`, `a/b`) rejected;
  delete of a non-child path refused.
- Plugin: unit-test the id validator (good/bad ids). The actual CLI shell-out is NOT unit-tested (no
  `claude` in CI) — factor it behind a small seam so the handler logic is testable with a fake
  runner, and mark the live path for manual validation.
- API: validation 400s for each kind.

## Constraints
- Engine axum-free; no new crate deps; `make test` green; `cargo build` compiles.
- Additive; no change to existing behavior.

## Acceptance
- The 6 routes work; validation rejects unsafe names/ids; `~/.claude.json` edits preserve other keys
  and are backed up; skills create/remove real dirs; plugin install/uninstall shells out correctly
  (manual verification). Suite green.
