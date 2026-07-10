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
