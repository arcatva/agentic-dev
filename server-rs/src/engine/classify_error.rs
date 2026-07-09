use regex::Regex;
use std::sync::OnceLock;

fn rate_limited_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)temporarily limiting|not (?:your|a) usage limit|overloaded|rate[ _-]?limit|\b429\b|\b503\b")
            .expect("valid regex")
    })
}

fn usage_limit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)usage limit|session limit|\b(?:daily|weekly|monthly|\d+-?hour)\b[^.]*limit|resets\b|quota")
            .expect("valid regex")
    })
}

/// Classify claude's result error text into a structured ErrorKind string.
/// Order-sensitive: rate-limited wins over usage-limit (they share the word "limit").
/// Returns one of "rate_limited" | "usage_limit" | "claude_error".
pub fn classify_claude_error(text: &str) -> &'static str {
    if rate_limited_re().is_match(text) {
        return "rate_limited";
    }
    if usage_limit_re().is_match(text) {
        return "usage_limit";
    }
    "claude_error"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_limit_classified() {
        assert_eq!(classify_claude_error("You've hit your session limit · resets 3:30pm (Pacific)"), "usage_limit");
        assert_eq!(classify_claude_error("usage limit reached"), "usage_limit");
        assert_eq!(classify_claude_error("your weekly limit is exhausted"), "usage_limit");
        assert_eq!(classify_claude_error("quota exceeded"), "usage_limit");
    }

    #[test]
    fn rate_limited_wins_over_usage_even_with_limit_word() {
        assert_eq!(classify_claude_error("Server is temporarily limiting requests (not your usage limit) Rate limited"), "rate_limited");
        assert_eq!(classify_claude_error("overloaded, try again"), "rate_limited");
        assert_eq!(classify_claude_error("HTTP 429 Too Many Requests"), "rate_limited");
        assert_eq!(classify_claude_error("503 Service Unavailable"), "rate_limited");
    }

    #[test]
    fn anything_else_is_generic_claude_error() {
        assert_eq!(classify_claude_error("some random tool failure"), "claude_error");
        assert_eq!(classify_claude_error(""), "claude_error");
    }
}
