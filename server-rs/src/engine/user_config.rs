use std::io;
use std::path::Path;
use crate::engine::store::McpServerDef;

// TOCTOU note: WRITE_LOCK serializes concurrent writes *within this process only*.
// It does NOT guard against an external `claude` session (or any other process)
// rewriting ~/.claude.json concurrently — a small lost-update race exists there.
// This is an accepted trade-off for a single-user local tool, the same trade-off
// made by `settings.local.json`.  If multi-process safety becomes necessary, an
// advisory file lock (e.g. via the `fs2` crate) would be the appropriate fix.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const MAX_BACKUPS: usize = 20;
const CLAUDE_JSON: &str = ".claude.json";

fn claude_json_path(config_base: &Path) -> std::path::PathBuf {
    config_base.parent().unwrap_or(config_base).join(CLAUDE_JSON)
}

/// Read `.claude.json` for a write:
/// - Missing → Ok(None) (treat as empty object on write)
/// - Valid JSON object → Ok(Some(map))
/// - Anything else → Err(InvalidData) (refuse to clobber)
fn read_claude_json_for_write(path: &Path) -> io::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Object(m)) => Ok(Some(m)),
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, ".claude.json is not a JSON object")),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, format!(".claude.json is corrupt: {e}"))),
        },
    }
}

/// Back up `.claude.json` to `<config_base>/backups/.claude.json.<millis>.bak`, pruning to MAX_BACKUPS.
fn backup_claude_json(config_base: &Path, path: &Path) -> io::Result<()> {
    if !path.exists() { return Ok(()); }
    let backups = config_base.join("backups");
    std::fs::create_dir_all(&backups)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::fs::copy(path, backups.join(format!("{CLAUDE_JSON}.{millis}.bak")))?;
    let mut baks: Vec<std::path::PathBuf> = std::fs::read_dir(&backups)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str())
                .map(|n| n.starts_with(CLAUDE_JSON) && n.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();
    baks.sort();
    if baks.len() > MAX_BACKUPS {
        for p in &baks[..baks.len() - MAX_BACKUPS] { let _ = std::fs::remove_file(p); }
    }
    Ok(())
}

/// Serialize a McpServerDef into the JSON object stored under mcpServers[name].
/// stdio: {command, args?, env?}; http: {type, url, headers?}. Null fields omitted.
///
/// Discriminator: presence of `command` → stdio shape; presence of `url` (without
/// command) → http shape.  This mirrors the logic in sdk-bridge.mjs, which checks
/// `command` to decide the transport, not the `type` field.
///
/// A previous version branched on `transport.is_some()`, which caused a valid stdio
/// server carrying `transport = Some("stdio")` (e.g. from a typed API request) to
/// take the http branch, silently dropping `command`/`args`/`env`.
fn serialize_def(def: &McpServerDef) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    if def.command.is_some() {
        // stdio transport: command drives the shape; type/url/headers are not written.
        if let Some(ref c) = def.command {
            m.insert("command".into(), serde_json::Value::String(c.clone()));
        }
        if let Some(ref a) = def.args {
            m.insert("args".into(), serde_json::to_value(a).unwrap_or_default());
        }
        if let Some(ref e) = def.env {
            m.insert("env".into(), serde_json::to_value(e).unwrap_or_default());
        }
    } else {
        // http/sse transport: type defaults to "http" if transport not specified.
        let type_val = def.transport.clone().unwrap_or_else(|| "http".into());
        m.insert("type".into(), serde_json::Value::String(type_val));
        if let Some(ref u) = def.url {
            m.insert("url".into(), serde_json::Value::String(u.clone()));
        }
        if let Some(ref h) = def.headers {
            m.insert("headers".into(), serde_json::to_value(h).unwrap_or_default());
        }
    }
    m
}

/// Read-modify-write `.claude.json` under the process write lock.
/// The `mutate` closure edits the map and returns whether a write is needed.
/// The backup is taken only when a write will actually happen (after `mutate`, before the
/// write — the on-disk file is still the original at that point), so no-op calls don't
/// churn real history out of the bounded backup rotation.
fn edit_claude_json(config_base: &Path, mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> bool) -> io::Result<bool> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = claude_json_path(config_base);
    let mut map = read_claude_json_for_write(&path)?.unwrap_or_default();
    let changed = mutate(&mut map);
    if changed {
        backup_claude_json(config_base, &path)?;
        let content = serde_json::to_string_pretty(&serde_json::Value::Object(map))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::engine::atomic_write::write_file_atomic(&path, &content)?;
    }
    Ok(changed)
}

/// Our own parking key inside `.claude.json` for globally-DISABLED user-scope MCP servers.
///
/// Rationale: Claude Code has no per-server disable for user-scope `mcpServers` — the
/// `disabledMcpjsonServers` settings key only rejects PROJECT `.mcp.json` servers (verified
/// against the CLI bundle), and `--strict-mcp-config` would also kill plugin-provided servers.
/// So "globally disabled" is implemented by MOVING the server's definition out of `mcpServers`
/// (which every spawned session reads) into this key, which only we read. Claude Code preserves
/// unknown top-level keys on its own read-modify-write cycles. Re-enabling moves it back.
pub const MCP_DISABLED_KEY: &str = "mcpServersDisabled";

/// Upsert an MCP server entry into `~/.claude.json` `mcpServers[def.name]`.
/// Also drops any same-named entry from the disabled parking key, so re-adding a
/// globally-disabled name yields a single, enabled definition.
/// Missing file is treated as `{}`; corrupt file returns Err (never clobbers).
/// Backs up before writing; writes atomically.
pub fn add_mcp_server(config_base: &Path, def: &McpServerDef) -> io::Result<()> {
    edit_claude_json(config_base, |map| {
        remove_from_key(map, MCP_DISABLED_KEY, &def.name);
        let servers = map.entry("mcpServers".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let serde_json::Value::Object(ref mut obj) = servers {
            obj.insert(def.name.clone(), serde_json::Value::Object(serialize_def(def)));
        }
        true
    })?;
    Ok(())
}

/// Remove `name` from a top-level object key, dropping the parent object if it becomes empty.
/// Returns the removed value (None if absent).
fn remove_from_key(map: &mut serde_json::Map<String, serde_json::Value>, key: &str, name: &str) -> Option<serde_json::Value> {
    let mut removed = None;
    let mut drop_parent = false;
    if let Some(serde_json::Value::Object(ref mut obj)) = map.get_mut(key) {
        removed = obj.remove(name);
        drop_parent = removed.is_some() && obj.is_empty();
    }
    if drop_parent { map.remove(key); }
    removed
}

/// Remove `mcpServers[name]` (and any parked `mcpServersDisabled[name]`) from `~/.claude.json`.
/// Returns `true` if found in either place and removed; `false` if absent (→ 404).
/// Backs up before writing; writes atomically.
pub fn delete_mcp_server(config_base: &Path, name: &str) -> io::Result<bool> {
    let mut found = false;
    edit_claude_json(config_base, |map| {
        let a = remove_from_key(map, "mcpServers", name).is_some();
        let b = remove_from_key(map, MCP_DISABLED_KEY, name).is_some();
        found = a || b;
        found
    })?;
    Ok(found)
}

/// Globally enable/disable a user-scope MCP server by moving its definition between
/// `mcpServers` (enabled — sessions see it) and `mcpServersDisabled` (parked — invisible
/// to sessions). Returns `false` if the name exists in neither place. Idempotent: moving
/// to the side it is already on is a no-op success.
///
/// Duplicate handling (name on BOTH sides — e.g. the server was parked here, then the user
/// ran `claude mcp add <name>` in a terminal, which writes `mcpServers` blind to our parking
/// key): the LIVE `mcpServers` definition is authoritative. The source side is ALWAYS scrubbed
/// — disabling parks the live def over any stale parked copy; enabling keeps the live def and
/// drops the stale parked one. (A naive already-on-target early-return would leave the live
/// copy in place and turn "disable" into a permanent silent no-op.)
pub fn set_mcp_server_enabled(config_base: &Path, name: &str, enabled: bool) -> io::Result<bool> {
    let (from, to) = if enabled {
        (MCP_DISABLED_KEY, "mcpServers")
    } else {
        ("mcpServers", MCP_DISABLED_KEY)
    };
    let mut found = false;
    edit_claude_json(config_base, |map| {
        let target_has = map.get(to).and_then(|v| v.as_object()).map(|o| o.contains_key(name)).unwrap_or(false);
        let Some(def) = remove_from_key(map, from, name) else {
            // Nothing on the source side: success iff already on the target side; no write.
            found = target_has;
            return false;
        };
        found = true;
        // Conflict rule: the def coming FROM `mcpServers` (live) is authoritative.
        //  - disabling (def == live): overwrite any stale parked copy with it.
        //  - enabling with a live def already on target: keep the live one, drop the stale
        //    parked def we just removed.
        if !(enabled && target_has) {
            let target = map.entry(to.to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(ref mut obj) = target {
                obj.insert(name.to_string(), def);
            }
        }
        true // the source-side removal must persist
    })?;
    Ok(found)
}

/// Deserialize the parked (globally-disabled) definitions for the given names back into
/// [McpServerDef]s — used at spawn to inject a session's forced-on MCP servers via the
/// extra-defs channel (the SDK `mcpServers` option), since the parked entry is invisible
/// to the session's normal config loading. Names not parked are skipped (a forced-on,
/// globally-ENABLED server needs no injection — sessions see it anyway).
pub fn parked_mcp_defs(config_base: &Path, names: &[String]) -> Vec<McpServerDef> {
    let path = claude_json_path(config_base);
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![] };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
    let Some(parked) = map.get(MCP_DISABLED_KEY).and_then(|v| v.as_object()) else { return vec![] };
    names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .filter_map(|n| {
            let mut obj = parked.get(n)?.clone();
            // The parked value is exactly serialize_def's output; McpServerDef's serde attrs
            // (`type` → transport) deserialize it back. `name` is the map KEY, not a field of
            // the stored value — inject it before deserializing (McpServerDef requires it).
            if let serde_json::Value::Object(ref mut m) = obj {
                m.insert("name".into(), serde_json::Value::String(n.to_string()));
            }
            serde_json::from_value::<McpServerDef>(obj).ok()
        })
        // A def with neither transport is unspawnable — don't forward garbage to the bridge.
        .filter(|d| d.command.is_some() || d.url.is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::store::McpServerDef;
    use std::collections::BTreeMap;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-uc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // config_base = base/.claude; .claude.json = base/.claude.json
    fn setup(base: &std::path::Path) -> std::path::PathBuf {
        let cb = base.join(".claude");
        std::fs::create_dir_all(&cb).unwrap();
        cb
    }

    #[test]
    fn add_stdio_server_round_trips() {
        let base = tmp();
        let cb = setup(&base);
        let def = McpServerDef {
            name: "my-mcp".into(),
            command: Some("node".into()),
            args: Some(vec!["server.js".into()]),
            env: None,
            transport: None,
            url: None,
            headers: None,
        };
        add_mcp_server(&cb, &def).unwrap();
        let text = std::fs::read_to_string(base.join(".claude.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["my-mcp"]["command"], "node");
        assert_eq!(v["mcpServers"]["my-mcp"]["args"][0], "server.js");
        // type/url/headers must NOT appear for stdio
        assert!(v["mcpServers"]["my-mcp"].get("type").is_none());
    }

    #[test]
    fn add_http_server_round_trips() {
        let base = tmp();
        let cb = setup(&base);
        let mut hdrs = BTreeMap::new();
        hdrs.insert("X-Token".into(), "abc".into());
        let def = McpServerDef {
            name: "web-mcp".into(),
            command: None,
            args: None,
            env: None,
            transport: Some("http".into()),
            url: Some("https://example.com/mcp".into()),
            headers: Some(hdrs),
        };
        add_mcp_server(&cb, &def).unwrap();
        let text = std::fs::read_to_string(base.join(".claude.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["web-mcp"]["type"], "http");
        assert_eq!(v["mcpServers"]["web-mcp"]["url"], "https://example.com/mcp");
        // command must NOT appear for http
        assert!(v["mcpServers"]["web-mcp"].get("command").is_none());
    }

    #[test]
    fn add_preserves_other_top_level_keys() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"otherKey":"preserved","mcpServers":{"existing":{"command":"x"}}}"#
        ).unwrap();
        let def = McpServerDef { name: "new".into(), command: Some("y".into()), ..Default::default() };
        add_mcp_server(&cb, &def).unwrap();
        let text = std::fs::read_to_string(base.join(".claude.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["otherKey"], "preserved");
        assert!(v["mcpServers"]["existing"]["command"] == "x");
        assert!(v["mcpServers"]["new"]["command"] == "y");
    }

    #[test]
    fn corrupt_file_refused() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"), "{corrupt").unwrap();
        let err = add_mcp_server(&cb, &McpServerDef { name: "x".into(), command: Some("c".into()), ..Default::default() }).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // file must be untouched
        assert_eq!(std::fs::read_to_string(base.join(".claude.json")).unwrap(), "{corrupt");
    }

    #[test]
    fn delete_returns_true_and_removes_entry() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"to-delete":{"command":"x"},"keep":{"command":"y"}}}"#
        ).unwrap();
        let removed = delete_mcp_server(&cb, "to-delete").unwrap();
        assert!(removed);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(base.join(".claude.json")).unwrap()
        ).unwrap();
        assert!(v["mcpServers"].get("to-delete").is_none());
        assert_eq!(v["mcpServers"]["keep"]["command"], "y");
    }

    #[test]
    fn delete_absent_returns_false() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"), r#"{"mcpServers":{}}"#).unwrap();
        assert!(!delete_mcp_server(&cb, "no-such").unwrap());
    }

    #[test]
    fn delete_drops_empty_mcp_servers_map() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"only":{"command":"x"}}}"#
        ).unwrap();
        delete_mcp_server(&cb, "only").unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(base.join(".claude.json")).unwrap()
        ).unwrap();
        // mcpServers key removed when empty
        assert!(v.get("mcpServers").is_none());
    }

    #[test]
    fn stdio_server_with_explicit_transport_field_retains_command() {
        // Regression: when a caller sets transport=Some("stdio") AND command=Some("npx"),
        // the old discriminator (transport.is_some()) wrongly took the http branch and
        // dropped command/args/env entirely.  The new discriminator (command.is_some())
        // must keep command in the output and must NOT emit a "type" field.
        let base = tmp();
        let cb = setup(&base);
        let def = McpServerDef {
            name: "stdio-explicit".into(),
            command: Some("npx".into()),
            args: Some(vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into()]),
            env: None,
            transport: Some("stdio".into()), // explicit transport field — must NOT flip to http branch
            url: None,
            headers: None,
        };
        add_mcp_server(&cb, &def).unwrap();
        let text = std::fs::read_to_string(base.join(".claude.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        // command and args must be present
        assert_eq!(v["mcpServers"]["stdio-explicit"]["command"], "npx",
            "command must survive even when transport=Some(\"stdio\") is set");
        assert_eq!(v["mcpServers"]["stdio-explicit"]["args"][0], "-y");
        // type/url/headers must NOT appear for stdio
        assert!(v["mcpServers"]["stdio-explicit"].get("type").is_none(),
            "type must not appear for a stdio server");
        assert!(v["mcpServers"]["stdio-explicit"].get("url").is_none());
    }

    #[test]
    fn add_creates_backup() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"), r#"{"mcpServers":{}}"#).unwrap();
        let def = McpServerDef { name: "x".into(), command: Some("c".into()), ..Default::default() };
        add_mcp_server(&cb, &def).unwrap();
        let backups: Vec<_> = std::fs::read_dir(cb.join("backups")).unwrap().flatten().collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn disable_moves_def_to_parking_key_and_enable_moves_back() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"srv":{"command":"node","args":["s.js"]},"other":{"type":"http","url":"https://x/mcp"}},"projects":{"keep":1}}"#,
        ).unwrap();

        // Disable: def moves verbatim to the parking key; other keys untouched.
        assert!(set_mcp_server_enabled(&cb, "srv", false).unwrap());
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert!(v["mcpServers"].get("srv").is_none());
        assert_eq!(v[MCP_DISABLED_KEY]["srv"]["command"], "node");
        assert_eq!(v["mcpServers"]["other"]["url"], "https://x/mcp");
        assert_eq!(v["projects"]["keep"], 1);

        // Idempotent: disabling again is a found no-op.
        assert!(set_mcp_server_enabled(&cb, "srv", false).unwrap());

        // Re-enable: moves back, parking key collapses away.
        assert!(set_mcp_server_enabled(&cb, "srv", true).unwrap());
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["srv"]["args"][0], "s.js");
        assert!(v.get(MCP_DISABLED_KEY).is_none());

        // Unknown name → Ok(false).
        assert!(!set_mcp_server_enabled(&cb, "nope", false).unwrap());
    }

    #[test]
    fn duplicate_on_both_sides_disable_scrubs_live_copy() {
        // The server was parked, then an external `claude mcp add` re-created it live —
        // name now on BOTH sides. Disable must park the LIVE def (authoritative) and leave
        // no live copy behind; a naive already-on-target no-op would keep the server active.
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"srv":{"command":"new"}},"mcpServersDisabled":{"srv":{"command":"stale"}}}"#,
        ).unwrap();

        assert!(set_mcp_server_enabled(&cb, "srv", false).unwrap());
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert!(v.get("mcpServers").and_then(|m| m.get("srv")).is_none(), "live copy must be scrubbed");
        assert_eq!(v[MCP_DISABLED_KEY]["srv"]["command"], "new", "live def wins over stale parked copy");
    }

    #[test]
    fn duplicate_on_both_sides_enable_keeps_live_def() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"srv":{"command":"new"}},"mcpServersDisabled":{"srv":{"command":"stale"}}}"#,
        ).unwrap();

        assert!(set_mcp_server_enabled(&cb, "srv", true).unwrap());
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["srv"]["command"], "new", "live def is authoritative");
        assert!(v.get(MCP_DISABLED_KEY).is_none(), "stale parked copy must be dropped");
    }

    #[test]
    fn parked_defs_deserialize_for_forced_on_injection() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"live":{"command":"a"}},"mcpServersDisabled":{"parked-stdio":{"command":"node","args":["s.js"],"env":{"K":"V"}},"parked-http":{"type":"sse","url":"https://x/mcp"}}}"#,
        ).unwrap();

        let defs = parked_mcp_defs(&cb, &["parked-stdio".into(), "parked-http".into(), "live".into(), "absent".into()]);
        // Only parked names come back — a live (enabled) server needs no injection.
        assert_eq!(defs.len(), 2);
        let s = defs.iter().find(|d| d.name == "parked-stdio").unwrap();
        assert_eq!(s.command.as_deref(), Some("node"));
        assert_eq!(s.env.as_ref().unwrap().get("K").map(String::as_str), Some("V"));
        let h = defs.iter().find(|d| d.name == "parked-http").unwrap();
        assert_eq!(h.transport.as_deref(), Some("sse"));
        assert_eq!(h.url.as_deref(), Some("https://x/mcp"));
    }

    #[test]
    fn delete_removes_parked_entry_too() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServersDisabled":{"parked":{"command":"c"}}}"#).unwrap();
        assert!(delete_mcp_server(&cb, "parked").unwrap());
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert!(v.get(MCP_DISABLED_KEY).is_none());
        assert!(!delete_mcp_server(&cb, "parked").unwrap());
    }

    #[test]
    fn re_adding_a_parked_name_drops_the_parked_copy() {
        let base = tmp();
        let cb = setup(&base);
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServersDisabled":{"srv":{"command":"old"}}}"#).unwrap();
        let def = McpServerDef { name: "srv".into(), command: Some("new".into()), ..Default::default() };
        add_mcp_server(&cb, &def).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(base.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["srv"]["command"], "new");
        assert!(v.get(MCP_DISABLED_KEY).is_none(), "stale parked copy must be dropped");
    }
}
