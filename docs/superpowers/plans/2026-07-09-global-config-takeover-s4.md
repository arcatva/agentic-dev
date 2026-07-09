# Global Config Takeover (S4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give agentic-dev a global settings layer that reads the local `~/.claude` install non-destructively and writes plugin/skill on-off toggles back to `settings.local.json` (so they also affect the `claude` CLI), with sessions inheriting that global baseline.

**Architecture:** A new `engine/global_settings.rs` reads/merges the toggle keys (`enabledPlugins`, `skillOverrides`) from `settings.local.json` over `settings.json` and performs surgical, backed-up, atomic writes to `settings.local.json`. A new `engine/components.rs` merges skills + plugins into one `ComponentInfo` list with their effective global state. Two axum routes expose read + toggle. The per-session plugin/skill resolution is reseeded from the global state so a globally-disabled component stays disabled inside sessions.

**Tech Stack:** Rust (axum, serde_json, parking_lot), reusing `engine/atomic_write.rs`. No new dependencies.

## Global Constraints

- Tests must stay green before every commit: `cd server-rs && cargo test`.
- The build must compile: `cd server-rs && cargo build`.
- `server-rs/src/engine/` must NOT import `axum` (keep it unit-testable in isolation).
- No new crate dependencies.
- Write target for global toggles is `~/.claude/settings.local.json` ONLY. Never modify `settings.json`.
- Only ever mutate the keys we own (`enabledPlugins`, `skillOverrides`); preserve all other keys verbatim (e.g. `permissions`).
- Refuse to write over corrupt JSON; never silently reset a config file.
- Every git commit message ends with:
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`

---

## File Structure

- **Create** `server-rs/src/engine/global_settings.rs` — read/merge global toggles, effective-state helpers, surgical backed-up atomic writes, and per-session resolution helpers.
- **Create** `server-rs/src/engine/components.rs` — `ComponentInfo` + `list_components` (skills ∪ plugins, MCP stub).
- **Modify** `server-rs/src/engine/mod.rs` — declare the two new modules; reseed the session skills seam.
- **Modify** `server-rs/src/engine/plugins.rs` — make `resolve_enabled_plugins` global-aware (default from global toggles instead of hardcoded `true`).
- **Modify** `server-rs/src/api/misc.rs` — add `global_settings_route` (GET) + `global_settings_toggle_route` (POST).
- **Modify** `server-rs/src/api/mod.rs` — register the two routes; redirect `claude_config_base` to a temp dir in the test harness.

---

## Task 1: Read + merge global toggles (`global_settings.rs`, read half)

**Files:**
- Create: `server-rs/src/engine/global_settings.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `pub mod global_settings;` near line 2681, alphabetical among the `pub mod` block)
- Test: in-crate `#[cfg(test)]` in `global_settings.rs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `pub struct GlobalToggles { pub enabled_plugins: BTreeMap<String, bool>, pub skill_overrides: BTreeMap<String, String> }`
  - `pub fn read_global_toggles(config_base: &Path) -> GlobalToggles`
  - `pub fn plugin_globally_enabled(t: &GlobalToggles, id: &str) -> bool`
  - `pub fn skill_globally_enabled(t: &GlobalToggles, name: &str) -> bool`

- [ ] **Step 1: Declare the module**

In `server-rs/src/engine/mod.rs`, add this line into the `pub mod` list (keep it alphabetical, right before `pub mod groups;`):

```rust
pub mod global_settings;
```

- [ ] **Step 2: Write the failing test**

Create `server-rs/src/engine/global_settings.rs` with only the tests + a stub, so it fails to compile/pass first:

```rust
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalToggles {
    pub enabled_plugins: BTreeMap<String, bool>,
    pub skill_overrides: BTreeMap<String, String>,
}

pub fn read_global_toggles(_config_base: &Path) -> GlobalToggles {
    unimplemented!()
}

pub fn plugin_globally_enabled(_t: &GlobalToggles, _id: &str) -> bool {
    unimplemented!()
}

pub fn skill_globally_enabled(_t: &GlobalToggles, _name: &str) -> bool {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-gs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn local_overrides_base_and_defaults_are_on() {
        let dir = tmp();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"enabledPlugins":{"a@m":false,"b@m":true},"skillOverrides":{"s1":"off"}}"#,
        ).unwrap();
        std::fs::write(
            dir.join("settings.local.json"),
            r#"{"enabledPlugins":{"a@m":true},"skillOverrides":{"s2":"off"},"permissions":{"x":1}}"#,
        ).unwrap();

        let t = read_global_toggles(&dir);
        // local wins: a@m flips back to true; base b@m stays; local s2 off added.
        assert_eq!(t.enabled_plugins.get("a@m"), Some(&true));
        assert_eq!(t.enabled_plugins.get("b@m"), Some(&true));
        assert_eq!(t.skill_overrides.get("s1").map(String::as_str), Some("off"));
        assert_eq!(t.skill_overrides.get("s2").map(String::as_str), Some("off"));

        // Effective helpers: unknown id/name default to enabled (installed ⇒ on).
        assert!(plugin_globally_enabled(&t, "a@m"));
        assert!(plugin_globally_enabled(&t, "absent@m")); // absent ⇒ true
        assert!(!skill_globally_enabled(&t, "s1"));
        assert!(skill_globally_enabled(&t, "never-mentioned"));
    }

    #[test]
    fn missing_or_corrupt_files_degrade_to_empty() {
        let dir = tmp();
        // No files at all.
        assert_eq!(read_global_toggles(&dir), GlobalToggles::default());
        // Corrupt local file → treated as empty (read is best-effort).
        std::fs::write(dir.join("settings.local.json"), "{nope").unwrap();
        assert_eq!(read_global_toggles(&dir), GlobalToggles::default());
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd server-rs && cargo test --lib global_settings::tests -- --nocapture`
Expected: FAIL / panic `not implemented` (or compile error until the file is wired).

- [ ] **Step 4: Write the implementation**

Replace the three stub bodies in `global_settings.rs`:

```rust
/// Read a JSON object from `path`, returning an empty map on missing OR corrupt file
/// (best-effort — used for reads that must never fail).
fn read_object_lossy(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Default::default() };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => Default::default(),
    }
}

pub fn read_global_toggles(config_base: &Path) -> GlobalToggles {
    let mut out = GlobalToggles::default();
    // Base first, then local overrides base key-by-key (local wins).
    for file in ["settings.json", "settings.local.json"] {
        let obj = read_object_lossy(&config_base.join(file));
        if let Some(serde_json::Value::Object(ep)) = obj.get("enabledPlugins") {
            for (k, v) in ep {
                if let Some(b) = v.as_bool() {
                    out.enabled_plugins.insert(k.clone(), b);
                }
            }
        }
        if let Some(serde_json::Value::Object(so)) = obj.get("skillOverrides") {
            for (k, v) in so {
                if let Some(s) = v.as_str() {
                    out.skill_overrides.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    out
}

pub fn plugin_globally_enabled(t: &GlobalToggles, id: &str) -> bool {
    t.enabled_plugins.get(id.trim()).copied().unwrap_or(true)
}

pub fn skill_globally_enabled(t: &GlobalToggles, name: &str) -> bool {
    t.skill_overrides.get(name.trim()).map(|v| v != "off").unwrap_or(true)
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd server-rs && cargo test --lib global_settings::tests`
Expected: PASS (both tests).

- [ ] **Step 6: Commit**

```bash
cd server-rs && cargo test --lib global_settings::tests && cd ..
git add server-rs/src/engine/global_settings.rs server-rs/src/engine/mod.rs
git commit -m "feat(global-settings): read+merge enabledPlugins/skillOverrides from ~/.claude

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Surgical backed-up writes (`global_settings.rs`, write half)

**Files:**
- Modify: `server-rs/src/engine/global_settings.rs`
- Test: in-crate `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::engine::atomic_write::write_file_atomic(path: &Path, content: &str) -> std::io::Result<()>` (existing).
- Produces:
  - `pub fn set_plugin_enabled(config_base: &Path, id: &str, enabled: bool) -> std::io::Result<()>`
  - `pub fn set_skill_enabled(config_base: &Path, name: &str, enabled: bool) -> std::io::Result<()>`

  Semantics: `enabled=false` writes `enabledPlugins[id]=false` / `skillOverrides[name]="off"`; `enabled=true` deletes that key (revert to default). Both back up `settings.local.json` to `<config_base>/backups/` first, preserve all other keys, refuse to write over corrupt JSON, and create the file/dirs if absent.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `global_settings.rs`:

```rust
    #[test]
    fn disable_then_reenable_plugin_preserves_other_keys() {
        let dir = tmp();
        std::fs::write(
            dir.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash"]}}"#,
        ).unwrap();

        set_plugin_enabled(&dir, "gh@m", false).unwrap();
        let after = read_object_lossy(&dir.join("settings.local.json"));
        assert_eq!(after["enabledPlugins"]["gh@m"], serde_json::json!(false));
        // untouched key preserved
        assert_eq!(after["permissions"]["allow"][0], serde_json::json!("Bash"));
        // settings.json must be untouched (never created by us)
        assert!(!dir.join("settings.json").exists());
        // a backup was produced
        let backups: Vec<_> = std::fs::read_dir(dir.join("backups")).unwrap().flatten().collect();
        assert_eq!(backups.len(), 1);

        // Re-enable ⇒ key removed; empty enabledPlugins map collapses away.
        set_plugin_enabled(&dir, "gh@m", true).unwrap();
        let after2 = read_object_lossy(&dir.join("settings.local.json"));
        assert!(after2.get("enabledPlugins").and_then(|m| m.get("gh@m")).is_none());
        assert!(after2.get("permissions").is_some());
    }

    #[test]
    fn set_skill_off_and_missing_file_is_created() {
        let dir = tmp(); // no settings.local.json yet
        set_skill_enabled(&dir, "rke2-ops", false).unwrap();
        let after = read_object_lossy(&dir.join("settings.local.json"));
        assert_eq!(after["skillOverrides"]["rke2-ops"], serde_json::json!("off"));
    }

    #[test]
    fn corrupt_local_file_refuses_write() {
        let dir = tmp();
        std::fs::write(dir.join("settings.local.json"), "{not json").unwrap();
        let err = set_plugin_enabled(&dir, "gh@m", false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // file left untouched
        assert_eq!(std::fs::read_to_string(dir.join("settings.local.json")).unwrap(), "{not json");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test --lib global_settings::tests`
Expected: FAIL — `set_plugin_enabled` / `set_skill_enabled` not found.

- [ ] **Step 3: Write the implementation**

Add to `global_settings.rs` (above the `#[cfg(test)]` module). Note the top-of-file `use` needs `BTreeMap` (already imported) and `std::sync::Mutex`:

```rust
/// Serializes all global-settings writes within this process. Cross-process races
/// (the CLI editing the same file) are an accepted small risk for a single-user tool.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const LOCAL_FILE: &str = "settings.local.json";
const MAX_BACKUPS: usize = 20;

/// Read `settings.local.json` for a WRITE: `Ok(None)` if missing, `Err(InvalidData)` if corrupt,
/// `Ok(Some(map))` if valid. Distinguishing missing from corrupt is what lets us refuse to
/// clobber a file we cannot parse.
fn read_local_for_write(path: &Path) -> std::io::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Object(m)) => Ok(Some(m)),
            Ok(_) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "settings.local.json is not a JSON object")),
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("settings.local.json is corrupt: {e}"))),
        },
    }
}

/// Copy the current file into `<config_base>/backups/settings.local.json.<millis>.bak`, then
/// prune to the most recent MAX_BACKUPS. Best-effort: a backup failure aborts the write (we do
/// not overwrite without a backup).
fn backup_local(config_base: &Path) -> std::io::Result<()> {
    let src = config_base.join(LOCAL_FILE);
    if !src.exists() {
        return Ok(()); // nothing to back up on first write
    }
    let backups = config_base.join("backups");
    std::fs::create_dir_all(&backups)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::fs::copy(&src, backups.join(format!("{LOCAL_FILE}.{millis}.bak")))?;
    // Prune oldest.
    let mut baks: Vec<std::path::PathBuf> = std::fs::read_dir(&backups)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&format!("{LOCAL_FILE}.")) && n.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();
    baks.sort();
    if baks.len() > MAX_BACKUPS {
        for p in &baks[..baks.len() - MAX_BACKUPS] {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(())
}

/// Read-modify-write `settings.local.json` under the process write lock, backing up first and
/// writing atomically. `mutate` edits only the keys we own.
fn edit_local(config_base: &Path, mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) -> std::io::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = config_base.join(LOCAL_FILE);
    let mut map = read_local_for_write(&path)?.unwrap_or_default();
    backup_local(config_base)?;
    mutate(&mut map);
    std::fs::create_dir_all(config_base)?;
    let content = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::engine::atomic_write::write_file_atomic(&path, &content)
}

/// Set/clear a boolean entry in a nested object key. `Some(v)` inserts; `None` removes and drops
/// the parent object if it becomes empty.
fn set_nested(map: &mut serde_json::Map<String, serde_json::Value>, parent: &str, key: &str, value: Option<serde_json::Value>) {
    match value {
        Some(v) => {
            let entry = map.entry(parent.to_string()).or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(obj) = entry {
                obj.insert(key.to_string(), v);
            } else {
                // parent existed with a non-object value — replace with a fresh object we own
                let mut obj = serde_json::Map::new();
                obj.insert(key.to_string(), v);
                map.insert(parent.to_string(), serde_json::Value::Object(obj));
            }
        }
        None => {
            let mut drop_parent = false;
            if let Some(serde_json::Value::Object(obj)) = map.get_mut(parent) {
                obj.remove(key);
                drop_parent = obj.is_empty();
            }
            if drop_parent {
                map.remove(parent);
            }
        }
    }
}

pub fn set_plugin_enabled(config_base: &Path, id: &str, enabled: bool) -> std::io::Result<()> {
    let id = id.trim().to_string();
    edit_local(config_base, |m| {
        let v = if enabled { None } else { Some(serde_json::Value::Bool(false)) };
        set_nested(m, "enabledPlugins", &id, v);
    })
}

pub fn set_skill_enabled(config_base: &Path, name: &str, enabled: bool) -> std::io::Result<()> {
    let name = name.trim().to_string();
    edit_local(config_base, |m| {
        let v = if enabled { None } else { Some(serde_json::Value::String("off".into())) };
        set_nested(m, "skillOverrides", &name, v);
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test --lib global_settings::tests`
Expected: PASS (all Task 1 + Task 2 tests).

- [ ] **Step 5: Commit**

```bash
cd server-rs && cargo test --lib global_settings && cd ..
git add server-rs/src/engine/global_settings.rs
git commit -m "feat(global-settings): surgical backed-up atomic writes to settings.local.json

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Unified component enumeration (`components.rs`)

**Files:**
- Create: `server-rs/src/engine/components.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `pub mod components;` alphabetically — right after `pub mod classify_error;`)
- Test: in-crate `#[cfg(test)]`

**Interfaces:**
- Consumes:
  - `crate::engine::skills::list_skills(skills_dir: &Path) -> Vec<SkillInfo{name, description}>`
  - `crate::engine::plugins::list_plugins(config_base: &Path) -> Vec<PluginInfo{name}>`
  - `crate::engine::global_settings::{read_global_toggles, plugin_globally_enabled, skill_globally_enabled}` (Task 1)
- Produces:
  - `pub struct ComponentInfo { kind, id, name, description, source, global_enabled }` (serde: `global_enabled` → `globalEnabled`)
  - `pub fn list_components(config_base: &Path, skills_dir: &Path) -> Vec<ComponentInfo>`

- [ ] **Step 1: Declare the module**

In `server-rs/src/engine/mod.rs`, add (alphabetical, right after `pub mod classify_error;`):

```rust
pub mod components;
```

- [ ] **Step 2: Write the failing test**

Create `server-rs/src/engine/components.rs`:

```rust
use std::path::Path;

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct ComponentInfo {
    pub kind: String,   // "skill" | "plugin" | "mcp"
    pub id: String,     // stable id: skill name, or "<plugin>@<marketplace>"
    pub name: String,   // display name
    pub description: String,
    pub source: String, // "user" | "project" | "plugin"
    #[serde(rename = "globalEnabled")]
    pub global_enabled: bool,
}

pub fn list_components(_config_base: &Path, _skills_dir: &Path) -> Vec<ComponentInfo> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-comp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn merges_skills_and_plugins_with_global_enabled() {
        let base = tmp();
        let skills = base.join("skills");
        std::fs::create_dir_all(skills.join("rke2-ops")).unwrap();
        std::fs::write(skills.join("rke2-ops").join("SKILL.md"),
            "---\nname: rke2-ops\ndescription: cluster ops\n---\nbody").unwrap();

        std::fs::create_dir_all(base.join("plugins")).unwrap();
        std::fs::write(base.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"github@mkt":[{"scope":"user"}]}}"#).unwrap();

        // Global: disable the skill, leave plugin default-on.
        std::fs::write(base.join("settings.local.json"),
            r#"{"skillOverrides":{"rke2-ops":"off"}}"#).unwrap();

        let out = list_components(&base, &skills);
        let skill = out.iter().find(|c| c.kind == "skill" && c.id == "rke2-ops").unwrap();
        assert_eq!(skill.source, "user");
        assert_eq!(skill.global_enabled, false);

        let plugin = out.iter().find(|c| c.kind == "plugin" && c.id == "github@mkt").unwrap();
        assert_eq!(plugin.source, "plugin");
        assert_eq!(plugin.name, "github"); // display name = id before '@'
        assert_eq!(plugin.global_enabled, true);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd server-rs && cargo test --lib components::tests`
Expected: FAIL — `not implemented`.

- [ ] **Step 4: Write the implementation**

Replace the `list_components` stub:

```rust
pub fn list_components(config_base: &Path, skills_dir: &Path) -> Vec<ComponentInfo> {
    let toggles = crate::engine::global_settings::read_global_toggles(config_base);
    let mut out = Vec::new();

    for s in crate::engine::skills::list_skills(skills_dir) {
        let global_enabled = crate::engine::global_settings::skill_globally_enabled(&toggles, &s.name);
        out.push(ComponentInfo {
            kind: "skill".into(),
            id: s.name.clone(),
            name: s.name,
            description: s.description,
            source: "user".into(),
            global_enabled,
        });
    }

    for p in crate::engine::plugins::list_plugins(config_base) {
        let display = p.name.split('@').next().unwrap_or(&p.name).to_string();
        let global_enabled = crate::engine::global_settings::plugin_globally_enabled(&toggles, &p.name);
        out.push(ComponentInfo {
            kind: "plugin".into(),
            id: p.name.clone(),
            name: display,
            description: String::new(), // installed_plugins.json carries no description
            source: "plugin".into(),
            global_enabled,
        });
    }

    // MCP: no user/project mcpServers today (all come from plugins). Stub: nothing to add.

    out.sort_by(|a, b| (a.kind.as_str(), a.id.as_str()).cmp(&(b.kind.as_str(), b.id.as_str())));
    out
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd server-rs && cargo test --lib components::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd server-rs && cargo test --lib components && cd ..
git add server-rs/src/engine/components.rs server-rs/src/engine/mod.rs
git commit -m "feat(components): unified skill+plugin enumeration with global enabled state

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Session inheritance seam (plugins default from global; skills reseeded)

**Files:**
- Modify: `server-rs/src/engine/plugins.rs` (`resolve_enabled_plugins` body + new tests)
- Modify: `server-rs/src/engine/global_settings.rs` (add `resolve_session_hidden_skills`)
- Modify: `server-rs/src/engine/mod.rs` (line ~1819, wrap `hidden_skills`)
- Test: in-crate `#[cfg(test)]` in both files

**Interfaces:**
- Consumes: `global_settings::{read_global_toggles, plugin_globally_enabled}` (Task 1).
- Produces: `pub fn resolve_session_hidden_skills(config_base: &Path, hidden_skills: &[String]) -> Vec<String>` in `global_settings.rs`.
- Changes behavior (same signature) of `plugins::resolve_enabled_plugins(config_base, hidden_plugins)`: a plugin's per-session default is now its **global** state, not hardcoded `true`.

- [ ] **Step 1: Write the failing test (plugins global-off inheritance)**

Add to the `tests` module in `server-rs/src/engine/plugins.rs`:

```rust
    #[test]
    fn session_inherits_global_disable_when_not_hidden() {
        let dir = tmp(); // tmp() already creates <dir>/plugins/
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"gh@m":[{"scope":"user"}],"cf@m":[{"scope":"user"}]}}"#,
        ).unwrap();
        // Global disables gh@m via settings.local.json.
        std::fs::write(dir.join("settings.local.json"), r#"{"enabledPlugins":{"gh@m":false}}"#).unwrap();

        // Session hides nothing → gh@m must stay false (inherited), cf@m true.
        let map = resolve_enabled_plugins(&dir, &[]);
        assert_eq!(map.get("gh@m"), Some(&false));
        assert_eq!(map.get("cf@m"), Some(&true));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test --lib plugins::tests::session_inherits_global_disable_when_not_hidden`
Expected: FAIL — current code returns `true` for `gh@m` (force-enable-everything).

- [ ] **Step 3: Make `resolve_enabled_plugins` global-aware**

In `server-rs/src/engine/plugins.rs`, inside `resolve_enabled_plugins`, replace the loop body that inserts `enabled`. Current:

```rust
    let mut map = std::collections::BTreeMap::new();
    for p in list_plugins(claude_config_dir) {
        let name = p.name.trim();
        if name.is_empty() {
            continue;
        }
        let enabled = !hidden.contains(name);
        map.insert(name.to_string(), enabled);
    }
```

Replace with (seed the default from the global toggle state instead of unconditional `true`):

```rust
    let toggles = crate::engine::global_settings::read_global_toggles(claude_config_dir);
    let mut map = std::collections::BTreeMap::new();
    for p in list_plugins(claude_config_dir) {
        let name = p.name.trim();
        if name.is_empty() {
            continue;
        }
        // Per-session default = the global state; a session hide forces it off.
        let enabled = crate::engine::global_settings::plugin_globally_enabled(&toggles, name)
            && !hidden.contains(name);
        map.insert(name.to_string(), enabled);
    }
```

Also update the doc comment on `resolve_enabled_plugins` (the paragraph starting "Why explicit...") — append one sentence:

```rust
/// As of S4 the per-session default is seeded from the global toggle state
/// (`settings.local.json`/`settings.json`) rather than unconditional `true`, so a
/// globally-disabled plugin stays disabled inside sessions unless the session re-enables it.
```

- [ ] **Step 4: Run plugin tests to verify pass (incl. the unchanged existing ones)**

Run: `cd server-rs && cargo test --lib plugins::tests`
Expected: PASS — the new test passes; the existing tests still pass because their temp dirs contain no `settings*.json` (global toggles empty ⇒ default `true`, unchanged behavior).

- [ ] **Step 5: Write the failing test (skills reseed helper)**

Add to the `tests` module in `server-rs/src/engine/global_settings.rs`:

```rust
    #[test]
    fn resolve_session_hidden_skills_unions_global_off_and_session_hidden() {
        let dir = tmp();
        std::fs::write(dir.join("settings.local.json"),
            r#"{"skillOverrides":{"g-off":"off","on-one":"on"}}"#).unwrap();
        // global off: g-off ; session hides: sess-hide
        let mut out = resolve_session_hidden_skills(&dir, &["sess-hide".into(), " ".into()]);
        out.sort();
        assert_eq!(out, vec!["g-off".to_string(), "sess-hide".to_string()]);
    }
```

- [ ] **Step 6: Run to verify it fails**

Run: `cd server-rs && cargo test --lib global_settings::tests::resolve_session_hidden_skills_unions_global_off_and_session_hidden`
Expected: FAIL — function not found.

- [ ] **Step 7: Implement `resolve_session_hidden_skills`**

Add to `server-rs/src/engine/global_settings.rs` (above the tests module). It needs `BTreeSet`; add `use std::collections::BTreeSet;` to the file's imports if not present:

```rust
/// The set of skills to turn OFF for a session: the union of globally-off skills
/// (`skillOverrides == "off"`) and the session's own hidden-skill list. This makes a session
/// inherit the global skill state while still applying its own hides on top.
pub fn resolve_session_hidden_skills(config_base: &Path, hidden_skills: &[String]) -> Vec<String> {
    let toggles = read_global_toggles(config_base);
    let mut set: BTreeSet<String> = toggles
        .skill_overrides
        .iter()
        .filter(|(_, v)| v.as_str() == "off")
        .map(|(k, _)| k.clone())
        .collect();
    for h in hidden_skills {
        let h = h.trim();
        if !h.is_empty() {
            set.insert(h.to_string());
        }
    }
    set.into_iter().collect()
}
```

- [ ] **Step 8: Wire the skills seam in `engine/mod.rs`**

In `server-rs/src/engine/mod.rs`, at the RunSpec build (~line 1819), replace:

```rust
            hidden_skills: s.hidden_skills.clone(),
```

with:

```rust
            // Reseed from global: session inherits globally-off skills + applies its own hides.
            hidden_skills: crate::engine::global_settings::resolve_session_hidden_skills(
                &self.0.cfg.claude_config_base,
                &s.hidden_skills,
            ),
```

(The `enabled_plugins` field just below already calls `resolve_enabled_plugins`, which is now global-aware — no change needed there.)

- [ ] **Step 9: Run the full engine test suite**

Run: `cd server-rs && cargo test --lib global_settings plugins components && cargo build`
Expected: PASS + compiles.

- [ ] **Step 10: Commit**

```bash
cd server-rs && cargo test --lib && cargo build && cd ..
git add server-rs/src/engine/plugins.rs server-rs/src/engine/global_settings.rs server-rs/src/engine/mod.rs
git commit -m "feat(sessions): inherit global plugin/skill toggles instead of force-enabling all

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: API routes (`GET /api/global-settings`, `POST /api/global-settings/toggle`)

**Files:**
- Modify: `server-rs/src/api/misc.rs` (two handlers)
- Modify: `server-rs/src/api/mod.rs` (register routes; test-harness redirect of `claude_config_base`)
- Test: in-crate `#[cfg(test)]` in `api/mod.rs`

**Interfaces:**
- Consumes: `engine::components::list_components`, `engine::global_settings::{set_plugin_enabled, set_skill_enabled}`, `st.config.claude_config_base`, `st.config.skills_dir`.
- Produces: routes `GET /api/global-settings` (array of `ComponentInfo`) and `POST /api/global-settings/toggle` (body `{kind, id, enabled}` → refreshed array, or 4xx/5xx).

- [ ] **Step 1: Add the handlers**

In `server-rs/src/api/misc.rs`, add after `plugins_route` (the file already imports `State`, `StatusCode`, `Json`, `json`, `Response`, `IntoResponse`):

```rust
/// GET /api/global-settings — unified skill+plugin components with their global on/off state.
pub async fn global_settings_route(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    Json(crate::engine::components::list_components(
        &st.config.claude_config_base,
        &st.config.skills_dir,
    ))
}

#[derive(serde::Deserialize)]
pub struct ToggleReq {
    pub kind: String,
    pub id: String,
    pub enabled: bool,
}

/// POST /api/global-settings/toggle — flip one component globally (writes settings.local.json).
pub async fn global_settings_toggle_route(
    State(st): State<AppState>,
    Json(req): Json<ToggleReq>,
) -> Response {
    let base = &st.config.claude_config_base;
    let res = match req.kind.as_str() {
        "plugin" => crate::engine::global_settings::set_plugin_enabled(base, &req.id, req.enabled),
        "skill" => crate::engine::global_settings::set_skill_enabled(base, &req.id, req.enabled),
        other => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("unknown kind: {other}")}))).into_response();
        }
    };
    match res {
        Ok(()) => Json(crate::engine::components::list_components(base, &st.config.skills_dir)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}
```

- [ ] **Step 2: Register the routes**

In `server-rs/src/api/mod.rs`, add after the `/api/plugins` route (line ~74):

```rust
        .route("/api/global-settings", get(misc::global_settings_route))
        .route("/api/global-settings/toggle", post(misc::global_settings_toggle_route))
```

(`get` and `post` are already imported and used by neighboring routes.)

- [ ] **Step 3: Redirect `claude_config_base` in the test harness**

In `server-rs/src/api/mod.rs`, in `test_state()`, next to the existing `c.skills_dir = dir.join("skills");` line, add:

```rust
        c.claude_config_base = dir.join("claude");
        let _ = std::fs::create_dir_all(&c.claude_config_base);
```

- [ ] **Step 4: Write the API test**

Add a test in the `tests` module of `server-rs/src/api/mod.rs`. Reuse the existing router + auth helpers already present in that module (`app(...)`/`issue_token`/`oneshot` pattern used by other tests). If a helper to build the router + an authed request already exists in this module, use it; otherwise this self-contained version drives the router directly:

```rust
    #[tokio::test]
    async fn global_settings_get_and_toggle() {
        let st = test_state().await;
        let base = st.config.claude_config_base.clone();
        let skills = st.config.skills_dir.clone();

        // Seed one installed plugin and one user skill.
        std::fs::create_dir_all(base.join("plugins")).unwrap();
        std::fs::write(base.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"gh@m":[{"scope":"user"}]}}"#).unwrap();
        std::fs::create_dir_all(skills.join("rke2-ops")).unwrap();
        std::fs::write(skills.join("rke2-ops").join("SKILL.md"),
            "---\nname: rke2-ops\ndescription: d\n---\nb").unwrap();

        let token = issue_token(&st.config.auth_secret, now_secs());
        let router = build_router(st.clone()); // use this module's existing router builder

        // GET → both components present, default enabled.
        let resp = router.clone()
            .oneshot(Request::builder()
                .uri("/api/global-settings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let arr: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(arr.as_array().unwrap().iter().any(|c| c["id"] == "gh@m" && c["globalEnabled"] == true));

        // POST toggle → disable the plugin globally.
        let resp = router.clone()
            .oneshot(Request::builder()
                .method("POST")
                .uri("/api/global-settings/toggle")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"kind":"plugin","id":"gh@m","enabled":false}"#)).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // settings.local.json now records the disable; settings.json untouched.
        let local: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(base.join("settings.local.json")).unwrap()).unwrap();
        assert_eq!(local["enabledPlugins"]["gh@m"], serde_json::json!(false));
        assert!(!base.join("settings.json").exists());
    }
```

> Note: replace `build_router(st.clone())` and `issue_token(...)`/`now_secs()` with the exact router-builder and auth helpers already used by the other `#[tokio::test]`s in this module (grep the module for how existing route tests construct the router and authed requests, and copy that pattern). Do not introduce a new router constructor.

- [ ] **Step 5: Run the API test**

Run: `cd server-rs && cargo test --lib api::tests::global_settings_get_and_toggle`
Expected: PASS.

- [ ] **Step 6: Full suite + build**

Run: `cd server-rs && cargo test && cargo build`
Expected: all green, compiles.

- [ ] **Step 7: Commit**

```bash
cd server-rs && cargo test && cargo build && cd ..
git add server-rs/src/api/misc.rs server-rs/src/api/mod.rs
git commit -m "feat(api): GET /api/global-settings + POST /api/global-settings/toggle

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Manual verification (after Task 5)

1. Start the server against the real `~/.claude`.
2. `curl -s $BASE/api/global-settings` (with auth) → lists 13 plugins + 5 skills, all `globalEnabled:true`.
3. `POST /api/global-settings/toggle {"kind":"plugin","id":"github@claude-plugins-official","enabled":false}`.
4. Confirm `~/.claude/settings.local.json` gained `enabledPlugins["github@claude-plugins-official"]=false`, that `permissions` is still present, that `~/.claude/settings.json` is unchanged, and that a backup appeared under `~/.claude/backups/`.
5. Confirm `claude plugin list` (standalone CLI) now reports github disabled — i.e. the global toggle reached the CLI.
6. Start a new agentic-dev session that does not hide github → it must NOT force github back on (seam behavior).
7. Re-enable via toggle `enabled:true` → key removed from `settings.local.json`.

---

## Self-Review (author checklist — completed)

**Spec coverage:**
- Adoption/non-destructive read → Task 1 (`read_global_toggles`, best-effort). ✅
- Unified read layer (S1 subset) → Task 3 (`components.rs`). ✅
- Global settings API read+write → Task 5. ✅
- Write to `settings.local.json`, preserve unknown keys, delete-on-reenable → Task 2. ✅
- Write-safety (backup, atomic, corrupt-refuse, dir create, prune) → Task 2. ✅
- Session inheritance seam (plugins default-from-global; skills reseed) → Task 4. ✅
- MCP stub, deferred → Task 3 (comment, no rows). ✅
- Testing per component → each task has unit tests; Task 5 has an API test. ✅

**Placeholder scan:** No TBD/TODO; every code step shows complete code. The only "adapt to existing helper" note is Task 5 Step 4's router/auth builder, which is explicitly a "copy the existing pattern in this module" instruction, not a hidden implementation gap. ✅

**Type consistency:** `GlobalToggles`, `read_global_toggles`, `plugin_globally_enabled`, `skill_globally_enabled` (Task 1) are consumed with identical names/signatures in Tasks 3 & 4. `set_plugin_enabled`/`set_skill_enabled` (Task 2) consumed verbatim in Task 5. `ComponentInfo`/`list_components` (Task 3) consumed verbatim in Task 5. `resolve_session_hidden_skills` (Task 4) matches its `engine/mod.rs` call. `resolve_enabled_plugins` keeps its existing signature. ✅
