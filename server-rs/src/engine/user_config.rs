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
fn edit_claude_json(config_base: &Path, mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> bool) -> io::Result<bool> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = claude_json_path(config_base);
    let mut map = read_claude_json_for_write(&path)?.unwrap_or_default();
    backup_claude_json(config_base, &path)?;
    let changed = mutate(&mut map);
    if changed {
        let content = serde_json::to_string_pretty(&serde_json::Value::Object(map))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::engine::atomic_write::write_file_atomic(&path, &content)?;
    }
    Ok(changed)
}

/// Upsert an MCP server entry into `~/.claude.json` `mcpServers[def.name]`.
/// Missing file is treated as `{}`; corrupt file returns Err (never clobbers).
/// Backs up before writing; writes atomically.
pub fn add_mcp_server(config_base: &Path, def: &McpServerDef) -> io::Result<()> {
    edit_claude_json(config_base, |map| {
        let servers = map.entry("mcpServers".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let serde_json::Value::Object(ref mut obj) = servers {
            obj.insert(def.name.clone(), serde_json::Value::Object(serialize_def(def)));
        }
        true
    })?;
    Ok(())
}

/// Remove `mcpServers[name]` from `~/.claude.json`.
/// Returns `true` if found and removed; `false` if absent (→ 404).
/// Drops the `mcpServers` object if it becomes empty.
/// Backs up before writing; writes atomically.
pub fn delete_mcp_server(config_base: &Path, name: &str) -> io::Result<bool> {
    let mut found = false;
    edit_claude_json(config_base, |map| {
        let mut drop_parent = false;
        if let Some(serde_json::Value::Object(ref mut obj)) = map.get_mut("mcpServers") {
            if obj.remove(name).is_some() {
                found = true;
                drop_parent = obj.is_empty();
            }
        }
        if drop_parent { map.remove("mcpServers"); }
        found
    })?;
    Ok(found)
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
}
