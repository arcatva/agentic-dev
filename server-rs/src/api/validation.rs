/// Name validator for skill/MCP names.
/// Accepts `^[A-Za-z0-9._-]+$`, non-empty, not "agentic",
/// not starting with `-` (blocks `--help`, `-y`, `-x`, etc.),
/// and not an all-dots name (`.`, `..`, `...`).
pub fn valid_component_name(s: &str) -> bool {
    if s.is_empty() || s == "agentic" {
        return false;
    }
    // Reject leading dash — would be treated as a CLI flag if passed to claude.
    if s.starts_with('-') {
        return false;
    }
    // Reject all-dots names (., .., ...) — path traversal / ambiguous.
    if s.chars().all(|c| c == '.') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Plugin id validator: accepts `^[A-Za-z0-9._@/-]+$`, non-empty,
/// and not starting with `-` (blocks `--help`, `-y`, `--config=...`, etc.).
pub fn valid_plugin_id(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Reject leading dash — would be treated as a CLI flag when passed to claude.
    if s.starts_with('-') {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '@' || c == '/' || c == '-'
    })
}

/// Transport-level checks for an MCP server definition at add time.
/// Mirrors `user_config::serialize_def`: a present `command` means stdio (type/url are
/// not written); otherwise http/sse with `type` defaulting to "http".
pub fn validate_mcp_def(def: &crate::engine::store::McpServerDef) -> Result<(), String> {
    let transport = def.transport.as_deref();
    if let Some(cmd) = def.command.as_deref() {
        if let Some(t) = transport.filter(|t| *t != "stdio") {
            return Err(format!("{t:?} server does not take a \"command\""));
        }
        if cmd.trim().is_empty() {
            return Err("stdio server requires a non-empty \"command\"".into());
        }
        if def.url.as_deref().is_some_and(|u| !u.trim().is_empty()) {
            return Err("give either \"command\" (stdio) or \"url\" (http/sse), not both".into());
        }
    } else {
        match transport {
            Some("stdio") => return Err("stdio server requires a non-empty \"command\"".into()),
            None | Some("http") | Some("sse") => {}
            Some(other) => {
                return Err(format!(
                    "unknown transport type {other:?} (expected \"stdio\", \"http\" or \"sse\")"
                ))
            }
        }
        let url = def.url.as_deref().unwrap_or("");
        if url.trim().is_empty() {
            return Err("server requires a \"command\" (stdio) or \"url\" (http/sse)".into());
        }
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or("");
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        if host.is_empty() || url.chars().any(char::is_whitespace) {
            return Err(format!("invalid url {url:?} — must be http(s)://host…"));
        }
    }
    Ok(())
}

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
        assert!(!valid_component_name("")); // empty
        assert!(!valid_component_name("agentic")); // reserved
        assert!(!valid_component_name("a/b")); // slash
        assert!(!valid_component_name("../x")); // traversal
        assert!(!valid_component_name("a b")); // space
        assert!(!valid_component_name("/abs")); // absolute
        assert!(!valid_component_name("a\nb")); // newline
        assert!(!valid_component_name("a@b")); // @ not in component names
    }

    #[test]
    fn valid_component_name_rejects_leading_dash_and_dot_only() {
        // leading dash — would be passed as a CLI flag
        assert!(!valid_component_name("-foo"));
        assert!(!valid_component_name("--flag"));
        // all-dots — path traversal
        assert!(!valid_component_name("."));
        assert!(!valid_component_name(".."));
        assert!(!valid_component_name("..."));
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

    #[test]
    fn valid_plugin_id_rejects_leading_dash() {
        // These would be interpreted as CLI flags by claude
        assert!(!valid_plugin_id("-y"));
        assert!(!valid_plugin_id("--help"));
        assert!(!valid_plugin_id("-x"));
    }
}
