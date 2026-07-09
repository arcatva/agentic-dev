use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalToggles {
    pub enabled_plugins: BTreeMap<String, bool>,
    pub skill_overrides: BTreeMap<String, String>,
}

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

/// True if the BASE file (`settings.json` only, ignoring `settings.local.json`) explicitly disables
/// this plugin. When enabling, this decides whether deleting our local key is enough (base allows
/// it) or whether we must write an explicit local override that wins over the base disable.
fn base_disables_plugin(config_base: &Path, id: &str) -> bool {
    read_object_lossy(&config_base.join("settings.json"))
        .get("enabledPlugins")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(id.trim()))
        .and_then(|v| v.as_bool())
        == Some(false)
}

/// True if the BASE file (`settings.json` only) force-disables this skill (`"off"`).
fn base_disables_skill(config_base: &Path, name: &str) -> bool {
    read_object_lossy(&config_base.join("settings.json"))
        .get("skillOverrides")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(name.trim()))
        .and_then(|v| v.as_str())
        == Some("off")
}

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
    // Enabling: if the base settings.json force-disables this plugin (`false`), deleting our local
    // key would leave it disabled (base wins in read_global_toggles), so write an explicit local
    // `true` override that wins over the base. Otherwise delete our key (installed ⇒ on by default).
    let value = if enabled {
        if base_disables_plugin(config_base, &id) {
            Some(serde_json::Value::Bool(true))
        } else {
            None
        }
    } else {
        Some(serde_json::Value::Bool(false))
    };
    edit_local(config_base, |m| set_nested(m, "enabledPlugins", &id, value))
}

pub fn set_skill_enabled(config_base: &Path, name: &str, enabled: bool) -> std::io::Result<()> {
    let name = name.trim().to_string();
    // Enabling: if the base settings.json force-disables this skill ("off"), deleting our local key
    // would leave it disabled (base wins in read_global_toggles), so write an explicit local "on"
    // override that wins over the base. Otherwise delete our key (skills are on by default).
    let value = if enabled {
        if base_disables_skill(config_base, &name) {
            Some(serde_json::Value::String("on".into()))
        } else {
            None
        }
    } else {
        Some(serde_json::Value::String("off".into()))
    };
    edit_local(config_base, |m| set_nested(m, "skillOverrides", &name, value))
}

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

    #[test]
    fn enable_over_base_disabled_plugin_writes_true_override() {
        let dir = tmp();
        // Base file force-disables the plugin; no local file yet.
        std::fs::write(dir.join("settings.json"), r#"{"enabledPlugins":{"gh@m":false}}"#).unwrap();
        set_plugin_enabled(&dir, "gh@m", true).unwrap();
        // Local override wins: an explicit `true` is written (not a delete).
        let local = read_object_lossy(&dir.join("settings.local.json"));
        assert_eq!(local["enabledPlugins"]["gh@m"], serde_json::json!(true));
        // Effective merged state is now enabled.
        assert!(plugin_globally_enabled(&read_global_toggles(&dir), "gh@m"));
        // Base file is never modified.
        let base = read_object_lossy(&dir.join("settings.json"));
        assert_eq!(base["enabledPlugins"]["gh@m"], serde_json::json!(false));
    }

    #[test]
    fn enable_over_base_disabled_skill_writes_on_override() {
        let dir = tmp();
        std::fs::write(dir.join("settings.json"), r#"{"skillOverrides":{"rke2-ops":"off"}}"#).unwrap();
        set_skill_enabled(&dir, "rke2-ops", true).unwrap();
        let local = read_object_lossy(&dir.join("settings.local.json"));
        assert_eq!(local["skillOverrides"]["rke2-ops"], serde_json::json!("on"));
        assert!(skill_globally_enabled(&read_global_toggles(&dir), "rke2-ops"));
    }

    #[test]
    fn enable_without_base_disable_deletes_local_key() {
        let dir = tmp();
        // No base settings.json; local currently disables the plugin.
        std::fs::write(dir.join("settings.local.json"), r#"{"enabledPlugins":{"gh@m":false}}"#).unwrap();
        set_plugin_enabled(&dir, "gh@m", true).unwrap();
        // Base allows it → minimal behavior: local key removed (empty parent collapsed).
        let local = read_object_lossy(&dir.join("settings.local.json"));
        assert!(local.get("enabledPlugins").and_then(|m| m.get("gh@m")).is_none());
    }
}
