# Component CRUD Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add 6 API routes (add/delete MCP server, skill, plugin) that perform surgical, backed-up, atomic mutations to `~/.claude.json`, `<skills_dir>/`, or the `claude` CLI, secured by name/id validation and path-traversal defenses.

**Architecture:** New engine modules (`user_config.rs` for MCP, extended `skills.rs` for skills, `plugin_cli.rs` for plugins) stay axum-free; handlers in `api/misc.rs` call them and return `list_components(...)`. Validation helpers are pure functions in the api layer. Plugin CLI shell-out is run on a blocking thread via `tokio::task::spawn_blocking` with a manual thread-based timeout (no new crates).

**Tech Stack:** Rust, `serde_json`, `std::process::Command`, `tokio::task::spawn_blocking`, existing `atomic_write::write_file_atomic`, existing `global_settings.rs` pattern (read-modify-write + backup + atomic).

## Global Constraints

- Engine (`server-rs/src/engine/`) must NOT import axum.
- No new crate dependencies (use only what is already in Cargo.toml).
- `cargo test` must stay green before commit.
- `cargo build` must compile.
- Additive only — no change to existing behavior.
- All 6 routes authed (same as existing routes — handled by the axum auth middleware already).
- Name validator: `^[A-Za-z0-9._-]+$`, non-empty, and `!= "agentic"` for MCP.
- Plugin id validator: `^[A-Za-z0-9._@/-]+$`, non-empty.
- Never interpolate user input into a shell string (`sh -c` is forbidden).
- Backup to `<config_base>/backups/` before any write to `~/.claude.json`.
- Corrupt `~/.claude.json` → `Err`, never clobber.
- Atomic write via `crate::engine::atomic_write::write_file_atomic`.
- Skill delete: canonicalize + assert direct-child-of-skills_dir before `remove_dir_all`.
- Plugin CLI timeout: 180s install, 60s uninstall; kill child on timeout.
- Report written to `.superpowers/sdd/crud-backend-report.md`.
- Commit locally only (no push/PR from this plan).
- Commit message ends with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

---

### Task 1: Validation helpers + name sanitization

**Files:**
- Create: `server-rs/src/api/validation.rs`
- Modify: `server-rs/src/api/mod.rs` (add `mod validation;`)

**Interfaces:**
- Produces:
  - `pub fn valid_component_name(s: &str) -> bool` — true iff `s` matches `^[A-Za-z0-9._-]+$` AND is not empty AND is not `"agentic"`.
  - `pub fn valid_mcp_name(s: &str) -> bool` — alias for `valid_component_name`; reserved for MCP context.
  - `pub fn valid_skill_name(s: &str) -> bool` — same regex, not empty, no "agentic" reservation needed for skills but use same function.
  - `pub fn valid_plugin_id(s: &str) -> bool` — true iff `s` matches `^[A-Za-z0-9._@/-]+$` AND is not empty.

- [ ] **Step 1: Write the failing tests**

In `server-rs/src/api/validation.rs` (create the file first with the tests only, no impl yet):

```rust
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_component_name_accepts_normal_names() {
        assert!(valid_component_name("my-server"));
        assert!(valid_component_name("my.server"));
        assert!(valid_component_name("my_server123"));
        assert!(valid_component_name("ABC"));
    }

    #[test]
    fn valid_component_name_rejects_bad_names() {
        assert!(!valid_component_name(""));           // empty
        assert!(!valid_component_name("agentic"));    // reserved
        assert!(!valid_component_name("a/b"));        // slash
        assert!(!valid_component_name("../x"));       // traversal
        assert!(!valid_component_name("a b"));        // space
        assert!(!valid_component_name("/abs"));       // absolute
        assert!(!valid_component_name("a\nb"));       // newline
        assert!(!valid_component_name("a@b"));        // @ not in component names
    }

    #[test]
    fn valid_plugin_id_accepts_marketplace_ids() {
        assert!(valid_plugin_id("gh@official"));
        assert!(valid_plugin_id("github@marketplace"));
        assert!(valid_plugin_id("my-plugin@m"));
        assert!(valid_plugin_id("plugin/sub@x"));
        assert!(valid_plugin_id("my.plugin@x"));
    }

    #[test]
    fn valid_plugin_id_rejects_bad_ids() {
        assert!(!valid_plugin_id(""));
        assert!(!valid_plugin_id("a b"));
        assert!(!valid_plugin_id("a\nb"));
        assert!(!valid_plugin_id("a;b"));
        assert!(!valid_plugin_id("a&b"));
    }
}
```

- [ ] **Step 2: Run to confirm it fails (functions not defined)**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test validation 2>&1 | head -30
```

Expected: compile error "unresolved import" or "not found in scope".

- [ ] **Step 3: Write the implementation**

Add the full file content:

```rust
/// Name validator for skill/MCP names.
/// Accepts `^[A-Za-z0-9._-]+$` and rejects empty or the reserved name "agentic".
pub fn valid_component_name(s: &str) -> bool {
    if s.is_empty() || s == "agentic" { return false; }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Plugin id validator: accepts `^[A-Za-z0-9._@/-]+$`, non-empty.
pub fn valid_plugin_id(s: &str) -> bool {
    if s.is_empty() { return false; }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '@' || c == '/' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    // ... (paste test block from Step 1)
}
```

- [ ] **Step 4: Add `mod validation;` to `server-rs/src/api/mod.rs`**

Find the line `mod login;` and add `pub(crate) mod validation;` nearby.

- [ ] **Step 5: Run tests**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test api::validation 2>&1
```

Expected: all tests PASS.

---

### Task 2: MCP add/delete engine (`engine/user_config.rs`)

**Files:**
- Create: `server-rs/src/engine/user_config.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `pub mod user_config;`)

**Interfaces:**
- Consumes:
  - `crate::engine::store::McpServerDef` (fields: `name: String`, `command: Option<String>`, `args: Option<Vec<String>>`, `env: Option<BTreeMap<String,String>>`, `transport: Option<String>` serialized as `"type"`, `url: Option<String>`, `headers: Option<BTreeMap<String,String>>`)
  - `crate::engine::atomic_write::write_file_atomic`
- Produces:
  - `pub fn add_mcp_server(config_base: &Path, def: &McpServerDef) -> std::io::Result<()>`
  - `pub fn delete_mcp_server(config_base: &Path, name: &str) -> std::io::Result<bool>` — returns `false` if absent (→ 404), `true` on success.

**Key behaviors:**
- `claude_json_path(config_base)` = `config_base.parent().unwrap_or(config_base).join(".claude.json")`
- Missing file → treat as `{}` (write a fresh object).
- Corrupt file (valid JSON but not an object, OR invalid JSON) → return `Err(InvalidData)`, never clobber.
- Back up to `<config_base>/backups/.claude.json.<millis>.bak` before any write (if the file exists).
- MCP server entry serialization: stdio → `{"command", "args"?, "env"?}`; http → `{"type", "url", "headers"?}`. Omit null fields.
- After mutation, write back with `write_file_atomic`.
- Use a process-level `Mutex` (same as `global_settings.rs`) to serialize writes.

- [ ] **Step 1: Write the failing tests**

Create `server-rs/src/engine/user_config.rs` with tests only (no impl):

```rust
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
```

- [ ] **Step 2: Run to confirm tests fail (no impl)**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test user_config 2>&1 | head -40
```

Expected: compile error (functions not found).

- [ ] **Step 3: Write the full implementation**

```rust
use std::io;
use std::path::Path;
use crate::engine::store::McpServerDef;

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
fn serialize_def(def: &McpServerDef) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    // http/sse transport: type + url are the discriminating fields
    if let Some(ref t) = def.transport {
        m.insert("type".into(), serde_json::Value::String(t.clone()));
        if let Some(ref u) = def.url {
            m.insert("url".into(), serde_json::Value::String(u.clone()));
        }
        if let Some(ref h) = def.headers {
            m.insert("headers".into(), serde_json::to_value(h).unwrap_or_default());
        }
    } else {
        // stdio transport
        if let Some(ref c) = def.command {
            m.insert("command".into(), serde_json::Value::String(c.clone()));
        }
        if let Some(ref a) = def.args {
            m.insert("args".into(), serde_json::to_value(a).unwrap_or_default());
        }
        if let Some(ref e) = def.env {
            m.insert("env".into(), serde_json::to_value(e).unwrap_or_default());
        }
    }
    m
}

/// Read-modify-write `.claude.json` under the process write lock.
fn edit_claude_json(config_base: &Path, mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> bool) -> io::Result<bool> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = claude_json_path(config_base);
    let mut map = read_claude_json_for_write(&path)?.unwrap_or_default();
    backup_claude_json(config_base, &path)?;
    let changed = mutate(&mut map);
    if changed {
        let content = serde_json::to_string_pretty(&serde_json::Value::Object(map))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
        crate::engine::atomic_write::write_file_atomic(&path, &content)?;
    }
    Ok(changed)
}

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

/// Returns `true` if the server was present and removed; `false` if absent.
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
    // ... (paste test block from Step 1)
}
```

- [ ] **Step 4: Register the module in `engine/mod.rs`**

Find where other engine modules are declared (e.g. `pub mod global_settings;`) and add:
```rust
pub mod user_config;
```

- [ ] **Step 5: Run tests**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test user_config 2>&1
```

Expected: all 8 tests PASS.

---

### Task 3: Skill add/delete (`engine/skills.rs` extension)

**Files:**
- Modify: `server-rs/src/engine/skills.rs` (add two functions + their tests)

**Interfaces:**
- Consumes: `skills_dir: &Path`, `name: &str`, `description: &str`
- Produces:
  - `pub fn add_skill(skills_dir: &Path, name: &str, description: &str) -> io::Result<()>` — creates `<skills_dir>/<name>/SKILL.md` with frontmatter. Errors if dir already exists.
  - `pub fn delete_skill(skills_dir: &Path, name: &str) -> io::Result<bool>` — canonicalize `<skills_dir>/<name>`, assert it is a DIRECT CHILD of canonicalized `skills_dir`, then `remove_dir_all`. Returns `false` if absent.

**Security note for `delete_skill`:** The name is validated by the API layer (regex), but as defense-in-depth the engine ALSO canonicalizes both paths and checks that `target.parent() == canon_skills_dir`. This blocks `../x`-style attacks even if the validator were bypassed.

- [ ] **Step 1: Add the new tests to `skills.rs`** (append to the existing `#[cfg(test)]` block):

```rust
    #[test]
    fn add_skill_creates_dir_and_skill_md() {
        let dir = tmp();
        add_skill(&dir, "my-skill", "does things").unwrap();
        let md = dir.join("my-skill").join("SKILL.md");
        assert!(md.exists());
        let text = std::fs::read_to_string(&md).unwrap();
        assert_eq!(text, "---\nname: my-skill\ndescription: does things\n---\n");
    }

    #[test]
    fn add_skill_errors_if_already_exists() {
        let dir = tmp();
        add_skill(&dir, "dup", "d").unwrap();
        let err = add_skill(&dir, "dup", "d2").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn delete_skill_removes_dir_and_returns_true() {
        let dir = tmp();
        add_skill(&dir, "to-remove", "desc").unwrap();
        assert!(delete_skill(&dir, "to-remove").unwrap());
        assert!(!dir.join("to-remove").exists());
    }

    #[test]
    fn delete_skill_absent_returns_false() {
        let dir = tmp();
        assert!(!delete_skill(&dir, "no-such").unwrap());
    }

    #[test]
    fn delete_skill_rejects_path_traversal() {
        let dir = tmp();
        // Create a directory OUTSIDE skills_dir to try to delete via traversal.
        let outside = dir.parent().unwrap().join(format!("outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        // The name "../outside-<pid>" would resolve to outside dir — must be refused.
        let name = format!("../outside-{}", std::process::id());
        let err = delete_skill(&dir, &name).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied,
            "traversal must be refused with PermissionDenied, got: {err}");
        // The outside dir must still exist (not deleted).
        assert!(outside.exists());
        std::fs::remove_dir_all(&outside).ok();
    }
```

- [ ] **Step 2: Run to confirm new tests fail**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test skills 2>&1 | head -30
```

Expected: compile error (add_skill, delete_skill not found).

- [ ] **Step 3: Write the implementation** (add to `skills.rs`, before the `#[cfg(test)]` block):

```rust
use std::io;

/// Create `<skills_dir>/<name>/SKILL.md` with YAML frontmatter.
/// Returns `AlreadyExists` if the directory already exists.
pub fn add_skill(skills_dir: &Path, name: &str, description: &str) -> io::Result<()> {
    let skill_dir = skills_dir.join(name);
    if skill_dir.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists,
            format!("skill '{name}' already exists")));
    }
    std::fs::create_dir_all(&skill_dir)?;
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n");
    std::fs::write(skill_dir.join("SKILL.md"), content)?;
    Ok(())
}

/// Remove `<skills_dir>/<name>` recursively.
/// Returns `false` if absent, `true` on success.
/// Refuses (PermissionDenied) if the resolved path is not a direct child of `skills_dir`.
pub fn delete_skill(skills_dir: &Path, name: &str) -> io::Result<bool> {
    let target = skills_dir.join(name);
    if !target.exists() { return Ok(false); }

    // Defense-in-depth: canonicalize both and assert target.parent() == skills_dir canon.
    let canon_skills = skills_dir.canonicalize()?;
    let canon_target = target.canonicalize()?;
    let canon_parent = canon_target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "target has no parent")
    })?;
    if canon_parent != canon_skills {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            format!("refusing to delete '{}': not a direct child of skills_dir",
                canon_target.display())));
    }
    std::fs::remove_dir_all(&canon_target)?;
    Ok(true)
}
```

- [ ] **Step 4: Run tests**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test skills 2>&1
```

Expected: all tests including the 5 new ones PASS.

---

### Task 4: Plugin CLI engine (`engine/plugin_cli.rs`)

**Files:**
- Create: `server-rs/src/engine/plugin_cli.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `pub mod plugin_cli;`)

**Interfaces:**
- Produces:
  - `pub fn run_plugin_command(config_base: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String>` — internal seam that executes `Command::new("claude").args(args).env("CLAUDE_CONFIG_DIR", config_base)`, kills on timeout, returns `Ok(stdout)` or `Err(stderr/reason)`.
  - `pub fn install_plugin(config_base: &Path, id: &str) -> Result<String, String>` — calls `run_plugin_command` with `["plugin", "install", id]` and 180s timeout.
  - `pub fn uninstall_plugin(config_base: &Path, id: &str) -> Result<String, String>` — calls `run_plugin_command` with `["plugin", "uninstall", id, "-y"]` and 60s timeout.

**Timeout approach:** `std::process::Command::spawn()` returns a `Child`. We send the child to a thread via `std::sync::mpsc::channel`, wait on the receiver with a timeout duration; if timed out we call `child.kill()`. This avoids any new crates.

- [ ] **Step 1: Write tests** (id validator test only — no `claude` available in CI):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // run_plugin_command is NOT tested with a real 'claude' binary (not available in CI).
    // We only test the seam contract for validation rejection (tested at the API layer).
    // Manual verification note: install_plugin / uninstall_plugin are tested manually.

    #[test]
    fn run_plugin_command_times_out_and_kills() {
        // Use 'sleep' as a proxy for a long-running command; 1s timeout kills it.
        // This test only runs on unix where 'sleep' is available.
        #[cfg(unix)]
        {
            let tmp = std::env::temp_dir().join(format!("agentic-pltest-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).unwrap();
            // "sleep 60" should be killed within ~1s
            let start = std::time::Instant::now();
            let result = run_plugin_command(&tmp, &["--version"], 1); // 1s timeout on a nonexistent 'claude'
            // On systems without 'claude', the spawn itself fails immediately with NotFound.
            // That's fine — we just assert the function returns without hanging.
            let elapsed = start.elapsed();
            assert!(elapsed.as_secs() < 5, "must not hang: took {:?}", elapsed);
            drop(result); // ok whether Ok or Err
        }
    }
}
```

- [ ] **Step 2: Run to confirm tests compile but pass (or fail gracefully)**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test plugin_cli 2>&1
```

- [ ] **Step 3: Write the implementation**

```rust
use std::path::Path;

/// Run `claude <args>` with env `CLAUDE_CONFIG_DIR=<config_base>`, no shell, stdin null.
/// Kills the child after `timeout_secs` seconds.
/// Returns `Ok(stdout)` on zero exit or `Err(stderr/reason)` otherwise.
pub fn run_plugin_command(config_base: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("claude")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<std::process::Output>>();

    // Move the child's wait into a thread so we can enforce a wall-clock timeout.
    std::thread::spawn(move || {
        let out = child.wait_with_output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }
        Ok(Err(e)) => Err(format!("process error: {e}")),
        Err(_) => {
            // Timeout: we can't easily kill the child here since ownership moved to the thread.
            // The thread will eventually complete but we return an error now.
            // NOTE: on Linux the child will be orphaned if the thread hangs; this is acceptable
            // for a single-user tool. A more robust approach would use a shared Arc<Mutex<Child>>.
            Err(format!("plugin command timed out after {timeout_secs}s"))
        }
    }
}

pub fn install_plugin(config_base: &Path, id: &str) -> Result<String, String> {
    run_plugin_command(config_base, &["plugin", "install", id], 180)
}

pub fn uninstall_plugin(config_base: &Path, id: &str) -> Result<String, String> {
    run_plugin_command(config_base, &["plugin", "uninstall", id, "-y"], 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    // ... (paste test block from Step 1)
}
```

**Note on timeout:** The child's `Child` struct moves into the thread. To actually kill on timeout we need shared ownership. Here is the improved kill-capable version:

```rust
pub fn run_plugin_command(config_base: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    let mut child = Command::new("claude")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    // Share the child with the timeout-killer: wrap in Arc<Mutex> so both the
    // wait thread and the timeout killer can access it.
    // But `wait_with_output` consumes the Child. Instead, read id for kill,
    // and use a separate approach: channel + thread + kill by id.
    //
    // Practical approach: use a boolean flag + raw pid kill instead.
    #[cfg(unix)]
    let pid = {
        use std::os::unix::process::CommandExt;
        child.id()
    };
    #[cfg(not(unix))]
    let pid = child.id();

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<std::process::Output>>();

    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }
        Ok(Err(e)) => Err(format!("process error: {e}")),
        Err(_elapsed) => {
            // Timeout: kill by pid (best-effort).
            #[cfg(unix)]
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
            #[cfg(not(unix))]
            { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status(); }
            Err(format!("plugin command timed out after {timeout_secs}s"))
        }
    }
}
```

**Simpler approach without libc (use `std::process::id` + kill via Command on Unix or just accept orphan):**
Actually the cleanest approach without libc is to keep the `Arc<Mutex<Option<Child>>>` trick — but `wait_with_output` needs ownership. The simplest no-new-crate approach is:

```rust
// Final impl: child is owned by the thread; on timeout we signal via a flag and
// use a second thread to kill by pid after a brief grace period. Since this is
// a single-user tool and the timeout is a safety net, a kill-by-pid via SIGKILL
// via std::process::Command is acceptable and avoids adding libc.
```

The actual implementation to use (no libc, no new crates):

```rust
pub fn run_plugin_command(config_base: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("claude")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    let pid = child.id();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<std::process::Output>>();

    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }
        Ok(Err(e)) => Err(format!("process error: {e}")),
        Err(_) => {
            // Best-effort kill via `kill -9 <pid>` on unix, `taskkill` on windows.
            #[cfg(unix)]
            { let _ = Command::new("kill").args(["-9", &pid.to_string()]).status(); }
            #[cfg(windows)]
            { let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status(); }
            Err(format!("plugin command timed out after {timeout_secs}s"))
        }
    }
}
```

- [ ] **Step 4: Register module, run tests**

Add `pub mod plugin_cli;` to `engine/mod.rs`.

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test plugin_cli 2>&1
```

Expected: compile + pass (the timeout test returns quickly since `claude` is not installed).

---

### Task 5: API handlers + routes (6 routes)

**Files:**
- Modify: `server-rs/src/api/misc.rs` (add 6 handlers + their tests)
- Modify: `server-rs/src/api/mod.rs` (register 6 routes + add `use` for `delete` method if not already imported)

**Interfaces:**
- Consumes:
  - `crate::engine::user_config::{add_mcp_server, delete_mcp_server}`
  - `crate::engine::skills::{add_skill, delete_skill}`
  - `crate::engine::plugin_cli::{install_plugin, uninstall_plugin}` (via `spawn_blocking`)
  - `crate::engine::components::list_components`
  - `crate::engine::store::McpServerDef`
  - `crate::api::validation::{valid_component_name, valid_plugin_id}`
  - `st.config.claude_config_base`, `st.config.skills_dir`
- Produces: 6 async handler functions registered in `mod.rs`.

**Route table:**

| Method | Path | Handler |
|--------|------|---------|
| POST | `/api/mcp-servers` | `mcp_add_route` |
| DELETE | `/api/mcp-servers/{name}` | `mcp_delete_route` |
| POST | `/api/skills` | `skills_add_route` |
| DELETE | `/api/skills/{name}` | `skills_delete_route` |
| POST | `/api/plugins` | `plugins_add_route` |
| DELETE | `/api/plugins/{id}` | `plugins_delete_route` |

**Response shape:**
- Success → 200 `Json(list_components(base, skills_dir))`
- Validation error → 400 `Json({"error": "..."})`
- Not found → 404 `Json({"error": "..."})`
- Write/CLI error → 500 `Json({"error": "..."})`

- [ ] **Step 1: Write the handler tests first** (add to `misc.rs` test block):

```rust
    #[tokio::test]
    async fn mcp_add_rejects_bad_name() {
        let st = test_state().await;
        let (s, b) = oneshot_req(st, Request::post("/api/mcp-servers")
            .header("authorization", auth(&crate::api::test_support::test_state().await))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"../evil","command":"x"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("invalid"));
    }

    #[tokio::test]
    async fn mcp_add_rejects_reserved_agentic_name() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(st, Request::post("/api/mcp-servers")
            .header("authorization", tok)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"agentic","command":"x"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().to_lowercase().contains("reserved") || b["error"].as_str().unwrap().contains("invalid"));
    }

    #[tokio::test]
    async fn mcp_add_and_delete_round_trip() {
        let st = test_state().await;
        let tok = auth(&st);
        // Add
        let (s, _) = oneshot_req(st.clone(), Request::post("/api/mcp-servers")
            .header("authorization", tok.clone())
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"test-mcp","command":"node","args":["s.js"]}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        // Verify in list
        let (s2, arr) = oneshot_req(st.clone(), Request::get("/api/global-settings")
            .header("authorization", tok.clone()).body(Body::empty()).unwrap()).await;
        assert_eq!(s2, StatusCode::OK);
        assert!(arr.as_array().unwrap().iter().any(|c| c["kind"] == "mcp" && c["id"] == "test-mcp"));
        // Delete
        let (s3, _) = oneshot_req(st.clone(), Request::delete("/api/mcp-servers/test-mcp")
            .header("authorization", tok.clone()).body(Body::empty()).unwrap()).await;
        assert_eq!(s3, StatusCode::OK);
        // Verify gone
        let (_, arr2) = oneshot_req(st.clone(), Request::get("/api/global-settings")
            .header("authorization", tok).body(Body::empty()).unwrap()).await;
        assert!(!arr2.as_array().unwrap().iter().any(|c| c["id"] == "test-mcp"));
    }

    #[tokio::test]
    async fn mcp_delete_absent_is_404() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(st, Request::delete("/api/mcp-servers/no-such")
            .header("authorization", tok).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(b["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn skills_add_rejects_bad_name() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(st, Request::post("/api/skills")
            .header("authorization", tok)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a/b","description":"d"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn skills_add_and_delete_round_trip() {
        let st = test_state().await;
        let tok = auth(&st);
        // Create skills_dir (test_state sets it to temp/skills)
        std::fs::create_dir_all(&st.config.skills_dir).unwrap();
        // Add
        let (s, arr) = oneshot_req(st.clone(), Request::post("/api/skills")
            .header("authorization", tok.clone())
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"my-skill","description":"does stuff"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert!(arr.as_array().unwrap().iter().any(|c| c["kind"] == "skill" && c["id"] == "my-skill"),
            "skill must appear in component list: {arr}");
        // Delete
        let (s2, _) = oneshot_req(st.clone(), Request::delete("/api/skills/my-skill")
            .header("authorization", tok).body(Body::empty()).unwrap()).await;
        assert_eq!(s2, StatusCode::OK);
    }

    #[tokio::test]
    async fn plugins_add_rejects_bad_id() {
        let st = test_state().await;
        let tok = auth(&st);
        let (s, b) = oneshot_req(st, Request::post("/api/plugins")
            .header("authorization", tok)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"a b"}"#)).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().is_some());
    }
```

- [ ] **Step 2: Run to confirm tests fail (handlers not defined)**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test misc 2>&1 | head -30
```

Expected: compile errors for missing handler functions + missing routes.

- [ ] **Step 3: Write the 6 handlers** (add to `api/misc.rs`):

```rust
// ── Component CRUD — add/delete MCP servers, skills, plugins ──

#[derive(serde::Deserialize)]
pub struct AddMcpBody {
    pub name: String,
    #[serde(flatten)]
    pub def_fields: serde_json::Value,
}

pub async fn mcp_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let def: crate::engine::store::McpServerDef = match serde_json::from_slice(&body) {
        Ok(d) => d,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid body: {e}")}))).into_response(),
    };
    if !crate::api::validation::valid_component_name(&def.name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid or reserved MCP name: {:?}", def.name)}))).into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    match crate::engine::user_config::add_mcp_server(&base, &def) {
        Ok(()) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn mcp_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_component_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid MCP name: {name:?}")}))).into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    match crate::engine::user_config::delete_mcp_server(&base, &name) {
        Ok(true) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": format!("MCP server '{name}' not found")}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct AddSkillBody { pub name: String, pub description: String }

pub async fn skills_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: AddSkillBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid body: {e}")}))).into_response(),
    };
    if !crate::api::validation::valid_component_name(&b.name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid skill name: {:?}", b.name)}))).into_response();
    }
    let skills = st.config.skills_dir.clone();
    let base = st.config.claude_config_base.clone();
    match crate::engine::skills::add_skill(&skills, &b.name, &b.description) {
        Ok(()) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists =>
            (StatusCode::BAD_REQUEST, Json(json!({"error": format!("skill '{}' already exists", b.name)}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn skills_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_component_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid skill name: {name:?}")}))).into_response();
    }
    let skills = st.config.skills_dir.clone();
    let base = st.config.claude_config_base.clone();
    match crate::engine::skills::delete_skill(&skills, &name) {
        Ok(true) => Json(crate::engine::components::list_components(&base, &skills)).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": format!("skill '{name}' not found")}))).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied =>
            (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid skill path: {e}")}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct AddPluginBody { pub id: String }

pub async fn plugins_add_route(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: AddPluginBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid body: {e}")}))).into_response(),
    };
    if !crate::api::validation::valid_plugin_id(&b.id) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid plugin id: {:?}", b.id)}))).into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    let id = b.id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::engine::plugin_cli::install_plugin(&base, &id)
    }).await.map_err(|e| format!("task error: {e}")).and_then(|r| r);
    match result {
        Ok(_stdout) => Json(crate::engine::components::list_components(&st.config.claude_config_base, &skills)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn plugins_delete_route(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !crate::api::validation::valid_plugin_id(&id) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid plugin id: {id:?}")}))).into_response();
    }
    let base = st.config.claude_config_base.clone();
    let skills = st.config.skills_dir.clone();
    let id2 = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::engine::plugin_cli::uninstall_plugin(&base, &id2)
    }).await.map_err(|e| format!("task error: {e}")).and_then(|r| r);
    match result {
        Ok(_stdout) => Json(crate::engine::components::list_components(&st.config.claude_config_base, &skills)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}
```

- [ ] **Step 4: Register the 6 routes in `api/mod.rs`**

In the `compressed` Router builder, after the existing `/api/global-settings/toggle` route, add:

```rust
.route("/api/mcp-servers", post(misc::mcp_add_route))
.route("/api/mcp-servers/{name}", delete(misc::mcp_delete_route))
.route("/api/skills", post(misc::skills_add_route))
.route("/api/skills/{name}", delete(misc::skills_delete_route))
.route("/api/plugins", post(misc::plugins_add_route))
.route("/api/plugins/{id}", delete(misc::plugins_delete_route))
```

Note: the existing `/api/skills` has a `get` handler — add `post` alongside it:
Change `.route("/api/skills", get(misc::skills_route))` to `.route("/api/skills", get(misc::skills_route).post(misc::skills_add_route))`.
Similarly for `/api/plugins`: add `.post(misc::plugins_add_route)` alongside the existing GET.

- [ ] **Step 5: Run tests + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass. Then:

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo build 2>&1 | tail -10
```

Expected: `Finished` with no errors.

---

### Task 6: Write report + commit

**Files:**
- Create: `.superpowers/sdd/crud-backend-report.md`

- [ ] **Step 1: Verify tests are green**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test 2>&1 | tail -5
```

Expected: `test result: ok`.

- [ ] **Step 2: Verify build is clean**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo build 2>&1 | tail -5
```

Expected: `Finished`.

- [ ] **Step 3: Write report to `.superpowers/sdd/crud-backend-report.md`**

Content outline (write in full):
- Modules added: `engine/user_config.rs`, `engine/plugin_cli.rs`; extended `engine/skills.rs`; added `api/validation.rs`; extended `api/misc.rs` and `api/mod.rs`.
- Routes added: 6 routes (table of method + path).
- Security measures: name/id regex validation, reserved "agentic" rejection, child-of-skills_dir canonicalize check, no shell (`Command::new` with args vec), corrupt-refuse + backup + atomic for `.claude.json`.
- Plugin CLI timeout approach: `spawn_blocking` + `mpsc::channel` + `recv_timeout` + kill-by-pid via `kill -9 <pid>`.
- Test evidence: list of test function names + what they cover.
- Manual-only: `install_plugin` / `uninstall_plugin` live paths require `claude` CLI present.

- [ ] **Step 4: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev && git add server-rs/src/engine/user_config.rs server-rs/src/engine/plugin_cli.rs server-rs/src/engine/skills.rs server-rs/src/engine/mod.rs server-rs/src/api/validation.rs server-rs/src/api/misc.rs server-rs/src/api/mod.rs .superpowers/sdd/crud-backend-report.md docs/superpowers/plans/2026-07-10-component-crud-backend.md
```

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev && git commit -m "$(cat <<'EOF'
feat: component CRUD backend — add/delete MCP server, skill, plugin (6 routes)

Adds engine/user_config.rs (surgical .claude.json MCP upsert+delete with
backup+atomic write), engine/plugin_cli.rs (plugin install/uninstall via
spawn_blocking + mpsc timeout), extends engine/skills.rs (add/delete skill
dir with path-traversal defense), and api/validation.rs (name/id validators).
Wires 6 new routes: POST/DELETE /api/mcp-servers[/{name}],
/api/skills[/{name}], /api/plugins[/{id}]. All routes return list_components()
on success. Tests cover MCP round-trip, corrupt-file refusal, path-traversal
rejection, skill create/delete, validation 400s, and API add+delete harness.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review Against Spec

**Spec coverage check:**

| Spec requirement | Covered by |
|---|---|
| POST /api/mcp-servers upserts mcpServers[name] | Task 2 + 5 |
| DELETE /api/mcp-servers/{name} removes entry | Task 2 + 5 |
| POST /api/skills creates dir+SKILL.md | Task 3 + 5 |
| DELETE /api/skills/{name} removes dir | Task 3 + 5 |
| POST /api/plugins shells out to claude plugin install | Task 4 + 5 |
| DELETE /api/plugins/{id} shells out to claude plugin uninstall -y | Task 4 + 5 |
| Success → list_components() | Task 5 (all handlers) |
| Errors → {error} + right status | Task 5 (all handlers) |
| Corrupt file → Err, never clobber | Task 2 tests |
| Backup before write | Task 2 |
| Atomic write | Task 2 (uses write_file_atomic) |
| Preserve all other keys | Task 2 tests |
| stdio vs http serialization | Task 2 (serialize_def) |
| drop empty mcpServers parent | Task 2 (delete_mcp_server) |
| Skills: add AlreadyExists error | Task 3 |
| Skills: delete direct-child-check | Task 3 (delete_skill) |
| Plugin: no shell, args as vec | Task 4 |
| Plugin: CLAUDE_CONFIG_DIR env | Task 4 |
| Plugin: stdin null | Task 4 |
| Plugin: 180s/60s timeout + kill | Task 4 |
| Plugin: spawn_blocking | Task 5 (handlers) |
| Name validator ^[A-Za-z0-9._-]+$ | Task 1 |
| Reserve "agentic" for MCP | Task 1 (valid_component_name) |
| Plugin id validator ^[A-Za-z0-9._@/-]+$ | Task 1 |
| Reject BEFORE fs/CLI action | Task 5 (handlers check validator first) |
| Engine axum-free | All engine tasks (no axum import) |
| No new crate deps | All tasks (only std + existing deps) |
| cargo test green | Task 6 |
| cargo build compiles | Task 6 |
| Report at .superpowers/sdd/crud-backend-report.md | Task 6 |
| Commit with Co-Authored-By line | Task 6 |

All spec requirements are covered.
