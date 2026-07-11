//! Pure-data helpers for session-title generation and validation.
//!
//! The HTTP title-call logic lives in `engine::title_client`. The
//! helpers in this file do not make any HTTP calls; they are unit-tested
//! in isolation and are cheap to call from anywhere in the engine.

const TITLE_MAX_CHARS: usize = 24;
const RECENT_MESSAGES_MAX: usize = 10;
const RECENT_MESSAGES_MAX_BYTES: usize = 4 * 1024;

/// Return true if `s` is acceptable as a session title.
///
/// Rules (from the spec):
///   - non-empty after trim
///   - ≤ 24 characters (Unicode scalar values)
///   - no leading markdown marker: `#`, `>`, `` ` ``
///   - no newlines
pub(crate) fn is_valid_title(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().count() > TITLE_MAX_CHARS {
        return false;
    }
    if let Some(first) = trimmed.chars().next() {
        if matches!(first, '#' | '>' | '`') {
            return false;
        }
    }
    if trimmed.contains('\n') {
        return false;
    }
    true
}

/// True if `s` contains at least one CJK Unified Ideograph (BMP block,
/// U+4E00..=U+9FFF). The title prompts require Chinese; a Latin-only reply
/// means the model ignored the prompt (e.g. a less prompt-faithful gateway
/// model), so we reject it.
fn has_han(s: &str) -> bool {
    s.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// Acceptance gate for a *generated* title. BOTH title-producing paths use it:
/// submit-time `generate` and the periodic `maybe_retitle`. A title is accepted
/// only when it is structurally valid AND actually contains Chinese.
///
/// Centralizing this closes a real gap: previously only `generate` enforced the
/// Han check, so `maybe_retitle` could silently replace a Chinese title with
/// Latin-only text (the retitle model is no more prompt-faithful than the
/// generate model). Now both paths share one contract.
pub(crate) fn is_accepted_generated_title(s: &str) -> bool {
    is_valid_title(s) && has_han(s)
}

/// Parse a session's JSONL log lines into (role, text) pairs — ONE entry per
/// conversation message (not per token chunk) — for the most recent
/// user/assistant turns. Capped at `RECENT_MESSAGES_MAX` entries and
/// `RECENT_MESSAGES_MAX_BYTES` of joined body text. Older entries are dropped
/// first when the cap is hit, so the returned list always reflects the tail of
/// the conversation.
///
/// On-disk log shapes (written by `sdk-bridge.mjs`, see `engine::stream` /
/// `engine::transcript_filter`):
///   - user prompt:  `{"type":"agentic_prompt","text":"...","at":N}`
///   - assistant turn (full, one per turn):
///       `{"type":"assistant","message":{"content":[{"type":"text","text":"..."},{"type":"tool_use",...}]}}`
///
/// We read the FULL assistant frame and join its `text` blocks into one entry.
/// The per-token `{"type":"stream_event",...,"text_delta",...}` lines that the
/// SDK also writes are intentionally ignored: the final frame already carries
/// the assembled text, and counting each delta as its own entry would let a
/// single reply's token fragments fill the whole window and evict every user
/// prompt under the cap below.
pub(crate) fn parse_recent_messages(lines: &[String]) -> Vec<(String, String)> {
    use serde_json::Value;
    let mut parsed: Vec<(String, String)> = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        match ty {
            "agentic_prompt" => {
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    parsed.push(("user".to_string(), text.to_string()));
                }
            }
            "assistant" => {
                // Join all `text` content blocks of the full assistant frame
                // into one coherent message (mirrors `transcript_filter`).
                // Frames carrying only tool_use blocks yield no text → skipped.
                let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
                    continue;
                };
                let mut text = String::new();
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                }
                if !text.is_empty() {
                    parsed.push(("assistant".to_string(), text));
                }
            }
            _ => {}
        }
    }
    if parsed.len() > RECENT_MESSAGES_MAX {
        let drop = parsed.len() - RECENT_MESSAGES_MAX;
        parsed.drain(..drop);
    }
    while parsed.len() > 1 {
        let total: usize = parsed.iter().map(|(_, t)| t.len()).sum();
        if total <= RECENT_MESSAGES_MAX_BYTES {
            break;
        }
        parsed.remove(0);
    }
    parsed
}

/// Count the user turns persisted in a session log — one per `agentic_prompt`
/// marker. Unlike `parse_recent_messages` (which keeps only the tail under a
/// cap), this counts the FULL history, so it is a restart-stable basis for the
/// periodic retitle cadence: the engine's in-memory turn counter resets to 0
/// on a server restart, which would otherwise shift the every-Nth-turn phase.
pub(crate) fn count_user_turns(lines: &[String]) -> usize {
    // Deserialize each line into a borrowed, zero-allocation view that extracts
    // only `type` — avoids building a full `serde_json::Value` (which heap-
    // allocates the whole object) per line. This runs on the turn path for the
    // full log, so the saving grows with conversation length. A line missing
    // `type`, with a non-string `type`, or that isn't a JSON object fails to
    // deserialize and is correctly counted as not-a-prompt.
    #[derive(serde::Deserialize)]
    struct TypeOnly<'a> {
        #[serde(rename = "type")]
        ty: &'a str,
    }
    lines
        .iter()
        .filter(|line| {
            serde_json::from_str::<TypeOnly>(line)
                .map(|v| v.ty == "agentic_prompt")
                .unwrap_or(false)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_valid_title --

    #[test]
    fn is_valid_title_accepts_short_chinese_string() {
        assert!(is_valid_title("修复登录 bug"));
    }

    #[test]
    fn is_valid_title_rejects_empty() {
        assert!(!is_valid_title(""));
        assert!(!is_valid_title("   "));
    }

    #[test]
    fn is_valid_title_rejects_too_long() {
        let s: String = std::iter::repeat('a').take(25).collect();
        assert!(!is_valid_title(&s));
    }

    #[test]
    fn is_valid_title_rejects_markdown_flavour() {
        assert!(!is_valid_title("# heading"));
        assert!(!is_valid_title("> quote"));
        assert!(!is_valid_title("`code`"));
    }

    #[test]
    fn is_valid_title_rejects_newlines() {
        assert!(!is_valid_title("line one\nline two"));
    }

    // -- parse_recent_messages --

    #[test]
    fn parse_recent_messages_extracts_user_prompts_in_order() {
        let lines = vec![
            r#"{"type":"system","subtype":"init"}"#.to_string(),
            r#"{"type":"agentic_prompt","text":"hello","at":1}"#.to_string(),
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}"#.to_string(),
            r#"{"type":"agentic_prompt","text":"again","at":2}"#.to_string(),
        ];
        let got = parse_recent_messages(&lines);
        assert_eq!(
            got,
            vec![
                ("user".to_string(), "hello".to_string()),
                ("assistant".to_string(), "world".to_string()),
                ("user".to_string(), "again".to_string()),
            ]
        );
    }

    #[test]
    fn parse_recent_messages_coalesces_assistant_frame_and_keeps_user_turn() {
        // Realistic single turn: one user prompt, MANY stream_event token
        // deltas, then one full assistant frame. The deltas must be ignored
        // and the assistant turn must become exactly ONE coherent entry.
        let mut lines =
            vec![r#"{"type":"agentic_prompt","text":"add retry logic","at":1}"#.to_string()];
        for i in 0..30 {
            lines.push(format!(
                r#"{{"type":"stream_event","event":{{"delta":{{"type":"text_delta","text":"tok{i} "}}}}}}"#
            ));
        }
        lines.push(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Here is the full reply"}]}}"#.to_string(),
        );
        let got = parse_recent_messages(&lines);
        assert_eq!(
            got,
            vec![
                ("user".to_string(), "add retry logic".to_string()),
                (
                    "assistant".to_string(),
                    "Here is the full reply".to_string()
                ),
            ]
        );
    }

    #[test]
    fn parse_recent_messages_keeps_user_and_assistant_after_5_turns() {
        // Five full turns, each = 1 user prompt + 50 token deltas + 1 final
        // assistant frame. The 10-entry cap must keep all 5 user prompts and
        // all 5 joined assistant replies — NOT trailing token fragments.
        let mut lines: Vec<String> = Vec::new();
        for turn in 0..5 {
            lines.push(format!(
                r#"{{"type":"agentic_prompt","text":"u{turn}","at":{turn}}}"#
            ));
            for d in 0..50 {
                lines.push(format!(
                    r#"{{"type":"stream_event","event":{{"delta":{{"type":"text_delta","text":"frag{turn}_{d}"}}}}}}"#
                ));
            }
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"reply {turn}"}}]}}}}"#
            ));
        }
        let got = parse_recent_messages(&lines);
        assert_eq!(
            got.len(),
            10,
            "expected 5 user + 5 assistant whole messages"
        );
        for (i, turn) in (0..5).enumerate() {
            assert_eq!(got[i * 2], ("user".to_string(), format!("u{turn}")));
            assert_eq!(
                got[i * 2 + 1],
                ("assistant".to_string(), format!("reply {turn}"))
            );
        }
        // No entry should be a leaked token fragment.
        assert!(
            got.iter().all(|(_, t)| !t.starts_with("frag")),
            "token fragments must never reach the parsed window: {got:?}"
        );
    }

    #[test]
    fn parse_recent_messages_skips_tool_only_assistant_frame() {
        let lines = vec![
            r#"{"type":"agentic_prompt","text":"run it","at":1}"#.to_string(),
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"Bash","input":{}}]}}"#.to_string(),
        ];
        let got = parse_recent_messages(&lines);
        assert_eq!(got, vec![("user".to_string(), "run it".to_string())]);
    }

    #[test]
    fn parse_recent_messages_caps_at_10_entries() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..30 {
            lines.push(format!(
                r#"{{"type":"agentic_prompt","text":"u{i}","at":{i}}}"#
            ));
        }
        let got = parse_recent_messages(&lines);
        assert_eq!(got.len(), 10);
        assert_eq!(got[0].1, "u20");
        assert_eq!(got[9].1, "u29");
    }

    #[test]
    fn parse_recent_messages_caps_joined_text_at_4kib() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..20 {
            let body = "x".repeat(500);
            lines.push(format!(
                r#"{{"type":"agentic_prompt","text":"u{i}-{body}","at":{i}}}"#
            ));
        }
        let got = parse_recent_messages(&lines);
        let total: usize = got.iter().map(|(_, t)| t.len()).sum();
        assert!(total <= 4096, "joined text {total} bytes exceeds 4 KiB");
        assert!(got.last().unwrap().1.starts_with("u19-"));
    }

    #[test]
    fn parse_recent_messages_ignores_unrelated_event_types() {
        let lines = vec![
            r#"{"type":"system","subtype":"init"}"#.to_string(),
            r#"{"type":"stream_event","event":{"delta":{"type":"message_start"}}}"#.to_string(),
            r#"{"type":"tool_use","name":"Bash"}"#.to_string(),
            r#"{"type":"agentic_prompt","text":"only user","at":1}"#.to_string(),
        ];
        let got = parse_recent_messages(&lines);
        assert_eq!(got, vec![("user".to_string(), "only user".to_string())]);
    }

    // -- is_accepted_generated_title --

    #[test]
    fn accepted_title_requires_han_character() {
        assert!(is_accepted_generated_title("修复登录"));
        assert!(is_accepted_generated_title("修复 OAuth 登录")); // mixed CJK + Latin is fine
                                                                 // Latin-only output (model ignored the Chinese-only prompt) is rejected
                                                                 // even though it is otherwise a structurally valid title.
        assert!(is_valid_title("perf tuning phase"));
        assert!(!is_accepted_generated_title("perf tuning phase"));
        assert!(!is_accepted_generated_title("")); // empty fails is_valid_title first
    }

    // -- count_user_turns --

    #[test]
    fn count_user_turns_counts_full_history_not_just_tail() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..23 {
            lines.push(format!(
                r#"{{"type":"agentic_prompt","text":"u{i}","at":{i}}}"#
            ));
            // interleave noise the parser-cap would otherwise truncate
            lines.push(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"r"}]}}"#
                    .to_string(),
            );
            lines.push(
                r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"x"}}}"#
                    .to_string(),
            );
        }
        // 23 agentic_prompt markers — count is the FULL history, not the 10-cap tail.
        assert_eq!(count_user_turns(&lines), 23);
    }

    #[test]
    fn count_user_turns_ignores_non_prompt_lines_and_garbage() {
        let lines = vec![
            r#"{"type":"system","subtype":"init"}"#.to_string(),
            r#"{"type":"agentic_prompt","text":"a","at":1}"#.to_string(),
            "not json at all".to_string(),
            r#"{"type":"assistant","message":{"content":[]}}"#.to_string(),
            r#"{"type":"agentic_prompt","text":"b","at":2}"#.to_string(),
        ];
        assert_eq!(count_user_turns(&lines), 2);
        assert_eq!(count_user_turns(&[]), 0);
    }
}
