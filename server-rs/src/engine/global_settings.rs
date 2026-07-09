use std::collections::BTreeMap;
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
