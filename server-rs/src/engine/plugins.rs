use std::path::Path;

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct PluginInfo {
    /// Full plugin id as claude settings expect it: `<plugin>@<marketplace>`. This is the exact
    /// key used in the `enabledPlugins` settings map, so the client can echo it back verbatim
    /// in `hiddenPlugins` without any re-derivation.
    pub name: String,
}

/// List installed Claude Code plugins from `<claude_config_dir>/plugins/installed_plugins.json`
/// (the CLI-maintained registry, format v2: `{"plugins": {"<name@marketplace>": [ ... ]}}`).
///
/// Mirrors [super::skills::list_skills]: read-only, best-effort (missing/corrupt file → empty
/// list), sorted by name. Entries with an empty install array are still listed — the file only
/// contains plugins the CLI actually installed.
pub fn list_plugins(claude_config_dir: &Path) -> Vec<PluginInfo> {
    let path = claude_config_dir.join("plugins").join("installed_plugins.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![] };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
    let Some(map) = v.get("plugins").and_then(|p| p.as_object()) else { return vec![] };
    let mut out: Vec<PluginInfo> = map
        .keys()
        .filter(|k| !k.trim().is_empty())
        .map(|k| PluginInfo { name: k.clone() })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Resolve the per-session plugin blacklist into an EXPLICIT enable map for the bridge's
/// command-line settings layer: every installed plugin appears with `true` (selected in the
/// New-request Filters) or `false` (deselected → hidden for this session).
///
/// Why explicit instead of the old `{<hidden>:false}` blacklist: `enabledPlugins` values in the
/// command-line settings layer win over the on-disk settings files, so writing `true` for the
/// selected plugins FORCE-ENABLES them for the session even when the plugin is disabled (or was
/// never enabled) in `~/.claude/settings.json`. That makes the app's plugin toggle authoritative —
/// with the blacklist alone, a selected chip was a no-op and could not surface a globally-disabled
/// plugin (verified live: `claude -p --settings '{"enabledPlugins":{"<id>":true}}'` lists the
/// plugin's skills while `claude plugin list` reports it disabled).
///
/// Hidden ids that are no longer installed are still emitted as `false` (harmless, preserves the
/// user's disable intent if the registry read raced an uninstall). If the registry is missing or
/// corrupt the map degrades to exactly the old blacklist (hidden ids → `false`).
pub fn resolve_enabled_plugins(
    claude_config_dir: &Path,
    hidden_plugins: &[String],
) -> std::collections::BTreeMap<String, bool> {
    let hidden: std::collections::BTreeSet<&str> = hidden_plugins
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let mut map = std::collections::BTreeMap::new();
    for p in list_plugins(claude_config_dir) {
        // Trim registry names too: a padded registry key must compare equal to its trimmed hidden
        // counterpart, otherwise a hidden plugin could slip through as `true` under a padded alias.
        let name = p.name.trim();
        if name.is_empty() {
            continue;
        }
        let enabled = !hidden.contains(name);
        map.insert(name.to_string(), enabled);
    }
    for h in hidden {
        map.entry(h.to_string()).or_insert(false);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-plugins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(d.join("plugins")).unwrap();
        d
    }

    #[test]
    fn lists_installed_plugins_sorted() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "zeta@official":[{"scope":"user","version":"1.0.0"}],
                "alpha@official":[{"scope":"user","version":"2.0.0"}]
            }}"#,
        )
        .unwrap();
        let out = list_plugins(&dir);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "alpha@official");
        assert_eq!(out[1].name, "zeta@official");
    }

    #[test]
    fn resolve_marks_installed_enabled_and_hidden_disabled() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "superpowers@official":[{"scope":"user","version":"6.1.1"}],
                "github@official":[{"scope":"user","version":"1.0.0"}],
                "stripe@official":[{"scope":"user","version":"0.2.5"}]
            }}"#,
        )
        .unwrap();
        let map = resolve_enabled_plugins(&dir, &["github@official".into(), " ".into()]);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("superpowers@official"), Some(&true));
        assert_eq!(map.get("stripe@official"), Some(&true));
        assert_eq!(map.get("github@official"), Some(&false));
    }

    #[test]
    fn resolve_empty_blacklist_enables_every_installed_plugin() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "alpha@official":[{"scope":"user","version":"1.0.0"}],
                "beta@official":[{"scope":"user","version":"1.0.0"}]
            }}"#,
        )
        .unwrap();
        let map = resolve_enabled_plugins(&dir, &[]);
        assert_eq!(
            map,
            std::collections::BTreeMap::from([
                ("alpha@official".to_string(), true),
                ("beta@official".to_string(), true),
            ])
        );
    }

    #[test]
    fn resolve_trims_padded_registry_names_so_hidden_still_wins() {
        let dir = tmp();
        // Pathological registry: key carries trailing whitespace. It must still compare equal to
        // the trimmed hidden id — not survive as a force-enabled padded alias.
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"padded@official ":[{"scope":"user","version":"1.0.0"}]}}"#,
        )
        .unwrap();
        let map = resolve_enabled_plugins(&dir, &["padded@official".into()]);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("padded@official"), Some(&false));
    }

    #[test]
    fn resolve_keeps_uninstalled_hidden_ids_disabled() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"alpha@official":[{"scope":"user","version":"1.0.0"}]}}"#,
        )
        .unwrap();
        let map = resolve_enabled_plugins(&dir, &["gone@official".into()]);
        assert_eq!(map.get("alpha@official"), Some(&true));
        assert_eq!(map.get("gone@official"), Some(&false));
    }

    #[test]
    fn resolve_degrades_to_blacklist_without_registry() {
        let dir = tmp();
        // No installed_plugins.json: hidden ids still emitted as false, nothing force-enabled.
        let map = resolve_enabled_plugins(&dir, &["superpowers@official".into()]);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("superpowers@official"), Some(&false));
        // Nothing installed, nothing hidden → empty map → the runner sets no env at all.
        assert!(resolve_enabled_plugins(&dir, &[]).is_empty());
    }

    #[test]
    fn missing_or_corrupt_file_is_empty() {
        let dir = tmp();
        // No installed_plugins.json at all.
        assert!(list_plugins(&dir).is_empty());
        // Corrupt JSON.
        std::fs::write(dir.join("plugins").join("installed_plugins.json"), "{nope").unwrap();
        assert!(list_plugins(&dir).is_empty());
        // Valid JSON, wrong shape.
        std::fs::write(dir.join("plugins").join("installed_plugins.json"), r#"{"plugins": []}"#).unwrap();
        assert!(list_plugins(&dir).is_empty());
    }
}
