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

/// Read user-scope MCP servers from `<config_base>/../.claude.json` → `mcpServers`.
/// Returns one `ComponentInfo{kind:"mcp"}` per named server. Missing file → empty vec.
pub fn list_user_mcp_servers(config_base: &Path) -> Vec<ComponentInfo> {
    let claude_json = config_base.parent().unwrap_or(config_base).join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&claude_json) else { return vec![]; };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };
    let Some(obj) = val.get("mcpServers").and_then(|v| v.as_object()) else { return vec![]; };
    obj.keys()
        .filter(|name| !name.is_empty())
        .map(|name| ComponentInfo {
            kind: "mcp".into(),
            id: name.clone(),
            name: name.clone(),
            description: String::new(),
            source: "user".into(),
            global_enabled: true,
        })
        .collect()
}

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

    for c in list_user_mcp_servers(config_base) {
        out.push(c);
    }

    out.sort_by(|a, b| (a.kind.as_str(), a.id.as_str()).cmp(&(b.kind.as_str(), b.id.as_str())));
    out
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
    fn lists_user_mcp_servers_from_claude_json() {
        let base = tmp();
        // Use base/.claude as config_base → list_user_mcp_servers reads base/.claude/../.claude.json = base/.claude.json
        let config_base = base.join(".claude");
        std::fs::create_dir_all(&config_base).unwrap();
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"my-server":{"command":"npx","args":["my-mcp"]},"web-server":{"type":"http","url":"https://example.com/mcp"}}}"#
        ).unwrap();

        let out = list_user_mcp_servers(&config_base);
        assert_eq!(out.len(), 2);
        let s1 = out.iter().find(|c| c.id == "my-server").unwrap();
        assert_eq!(s1.kind, "mcp");
        assert_eq!(s1.source, "user");
        assert!(s1.global_enabled);
        let s2 = out.iter().find(|c| c.id == "web-server").unwrap();
        assert_eq!(s2.kind, "mcp");
    }

    #[test]
    fn list_user_mcp_servers_missing_file_returns_empty() {
        let base = tmp();
        let config_base = base.join(".claude");
        std::fs::create_dir_all(&config_base).unwrap();
        // No .claude.json file written
        let out = list_user_mcp_servers(&config_base);
        assert!(out.is_empty());
    }

    #[test]
    fn list_components_includes_mcp_servers() {
        let base = tmp();
        let config_base = base.join(".claude");
        std::fs::create_dir_all(&config_base).unwrap();
        let skills = base.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(base.join(".claude.json"),
            r#"{"mcpServers":{"test-mcp":{"command":"node","args":["server.js"]}}}"#
        ).unwrap();
        let out = list_components(&config_base, &skills);
        let mcp = out.iter().find(|c| c.kind == "mcp" && c.id == "test-mcp").unwrap();
        assert_eq!(mcp.source, "user");
        assert!(mcp.global_enabled);
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
