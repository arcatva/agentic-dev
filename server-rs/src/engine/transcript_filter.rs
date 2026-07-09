//! Convert a stream-json log into a plain-text transcript suitable for use as the seed prompt
//! of a forked session. See `docs/superpowers/specs/2026-06-23-session-fork-design.md` §2.

const MAX_TRANSCRIPT_CHARS: usize = 50_000;
const TRUNCATION_MARKER: &str = "[... truncated, full log retained on source session ...]";

/// Parse a single stream-json line into (role, text) where role is "USER" or "ASSISTANT",
/// or None if the line should be dropped (synthetic frame, tool-only frame, malformed JSON).
fn extract_role_text(line: &str) -> Option<(&'static str, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type")?.as_str()?;
    // Skip synthetic frames.
    if ty == "agentic_prompt" || ty == "system" { return None; }
    // Top-level user / assistant frames carry the message in `.message.content`.
    let role = match ty {
        "user" => "USER",
        "assistant" => "ASSISTANT",
        _ => return None,
    };
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let mut text = String::new();
    for b in blocks {
        // Only text blocks. tool_use / tool_result / image etc. are skipped.
        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() { text.push('\n'); }
                text.push_str(t);
            }
        }
    }
    if text.is_empty() { return None; }
    Some((role, text))
}

/// Strip ASCII control characters that would break a prompt (BEL, BS, VT, FF, ESC, ...).
/// Keeps `\n` and `\t`.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| {
        let cp = *c as u32;
        cp >= 0x20 || *c == '\n' || *c == '\t'
    }).collect()
}

/// Convert raw stream-json log contents to a plain-text transcript. See module docs.
pub fn filter_log_to_transcript(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.split('\n') {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some((role, text)) = extract_role_text(line) {
            let cleaned = strip_control_chars(&text);
            if !out.is_empty() { out.push_str("\n\n"); }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(&cleaned);
            out.push('\n');
        }
    }
    if out.len() > MAX_TRANSCRIPT_CHARS {
        let mut cut = MAX_TRANSCRIPT_CHARS;
        // Avoid cutting mid-UTF-8-codepoint — back up to the nearest char boundary.
        while cut > 0 && !out.is_char_boundary(cut) { cut -= 1; }
        out.truncate(cut);
        out.push('\n');
        out.push_str(TRUNCATION_MARKER);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_log_returns_empty_string() {
        assert_eq!(filter_log_to_transcript(""), "");
        assert_eq!(filter_log_to_transcript("\n\n   \n"), "");
    }

    #[test]
    fn log_with_only_tool_use_returns_empty() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"Bash","input":{}}]}}"#;
        assert_eq!(filter_log_to_transcript(line), "");
    }

    #[test]
    fn extracts_user_and_assistant_text_in_order() {
        let log = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n{\"type\":\"agentic_prompt\",\"text\":\"internal\",\"at\":1}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello there\"}]}}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"thanks\"}]}}";
        let out = filter_log_to_transcript(log);
        assert_eq!(out, "USER: hi\n\n\nASSISTANT: hello there\n\n\nUSER: thanks\n");
    }

    #[test]
    fn malformed_lines_are_skipped_not_panicked() {
        let log = "not json at all\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"only-good\"}]}}\n{also broken}";
        let out = filter_log_to_transcript(log);
        assert_eq!(out, "USER: only-good\n");
    }

    #[test]
    fn drops_synthetic_agentic_prompt_frames() {
        let log = "{\"type\":\"agentic_prompt\",\"text\":\"meta\",\"at\":1}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"real\"}]}}";
        let out = filter_log_to_transcript(log);
        assert_eq!(out, "USER: real\n");
    }

    #[test]
    fn truncates_with_marker_when_over_limit() {
        // Build a log whose total output is comfortably over 50,000 chars.
        let mut log = String::new();
        for _i in 0..2000 {
            log.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
                "x".repeat(60)
            ));
        }
        let out = filter_log_to_transcript(&log);
        assert!(out.len() <= MAX_TRANSCRIPT_CHARS + TRUNCATION_MARKER.len() + 16, "output too long: {}", out.len());
        assert!(out.contains(TRUNCATION_MARKER), "should contain truncation marker");
    }

    #[test]
    fn strips_ascii_control_chars() {
        let log = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello\u0007\u001bworld"}]}}"#;
        let out = filter_log_to_transcript(log);
        assert_eq!(out, "USER: helloworld\n");
    }
}
