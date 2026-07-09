//! Decide whether a session's claude transcript can actually be resumed via `--resume`.
//!
//! The Anthropic API requires the `previous_message_id` the CLI sends on resume to be a real server
//! message id (`msg_...`). The SDK bridge has written some transcripts with only SYNTHETIC ids (no
//! `msg_` anywhere), so `--resume` on those 400s with
//! `previous_message_id: must be the id from a prior /v1/messages response (starts with msg_)` and can
//! NEVER succeed — the engine cannot synthesize server ids, and trimming the tail does not help (both
//! were verified live). For those sessions the engine drops `--resume` and runs the turn FRESH: prior
//! chat context is not carried into the model, but the git worktree + files are untouched and the
//! session becomes usable again. (A fresh turn that gets real `msg_` ids makes future resumes work, so
//! this self-heals once the bad pre-fix transcript is left behind.)
//!
//! However, when the model is a NON-Anthropic provider (e.g. DeepSeek proxied through the Anthropic
//! protocol, configured via ANTHROPIC_BASE_URL / ANTHROPIC_MODEL in settings.json), the message ids
//! are the provider's own format (UUIDs, not `msg_`). Those providers accept their own id format in
//! `previous_message_id`, so the `msg_` check would wrongly block resume. The gate therefore inspects
//! the inner `message.model` field: if the model is an Anthropic one (contains "claude"), the `msg_`
//! requirement applies; otherwise we trust the SDK's provider-specific resume mechanism.
//!
//! Algorithm:
//!   - Find the last assistant message in the transcript.
//!   - If its inner model contains "claude" → require at least one `msg_` id (Anthropic rule).
//!   - If its inner model is non-Anthropic (or unreadable) → resumable as long as there IS at least
//!     one assistant message (the SDK knows how to resume with that provider's id format).
//!   - If the transcript is empty or unreadable → not resumable.

use std::io::{BufRead, BufReader};
use std::path::Path;

/// True if [path] is resumable. Scans the transcript for the last assistant message, checks its
/// model, and applies the appropriate id-format rule. A read error returns `false`.
pub fn transcript_is_resumable(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // State tracked across the scan: last assistant model name, and whether we've seen at least
    // one msg_ id. We scan the whole transcript (once) because we need BOTH pieces of data.
    let mut has_msg_id = false;
    let mut last_assistant_model: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        // Check for a real Anthropic server message id — cheap substring scan.
        if !has_msg_id && line.contains("\"id\":\"msg_") {
            has_msg_id = true;
        }
        // Extract the inner model from assistant messages. We want the LAST one, so we always
        // overwrite. The line looks like:
        //   ...,"message":{"model":"deepseek-v4-pro","id":"...","type":"message","role":"assistant",...},...
        // We look for "role":"assistant" to identify assistant messages, then extract "model".
        if line.contains("\"role\":\"assistant\"") {
            if let Some(model) = extract_model(&line) {
                last_assistant_model = Some(model);
            }
        }
    }
    // If we found at least one msg_ id, it's definitely resumable (Anthropic model, real ids).
    if has_msg_id {
        return true;
    }
    // No msg_ ids. Determine the model and apply the appropriate rule.
    match last_assistant_model.as_deref() {
        // Anthropic model without msg_ ids → the known "synthetic ids" bug → not resumable.
        Some(m) if m.contains("claude") => false,
        // Non-Anthropic model (e.g. deepseek, minimax, gpt-*) → trust the SDK/provider resume mech.
        Some(_) => true,
        // No assistant messages at all → nothing to resume from.
        None => false,
    }
}

/// Extract the inner `message.model` field from a JSON line. Returns `None` on any parse failure
/// (the line might not be valid JSON, or might not have the expected structure).
fn extract_model(line: &str) -> Option<String> {
    // We do a targeted substring extraction rather than full JSON parsing to keep this cheap
    // (transcripts can be hundreds of MB). Find "model":" then read until the next unescaped ".
    let model_key = "\"model\":\"";
    let start = line.find(model_key)? + model_key.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(lines: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentic-resgate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(&p, format!("{}\n", lines.join("\n"))).unwrap();
        p
    }

    #[test]
    fn resumable_when_a_real_server_id_is_present() {
        let p = write(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","id":"msg_01ABC","stop_reason":"end_turn"}}"#,
        ]);
        assert!(transcript_is_resumable(&p));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn not_resumable_with_only_synthetic_ids() {
        // Synthetic ids (32-char hex, no dashes) with a Claude model → the known bug:
        // the SDK produced UUIDs instead of msg_ ids for an Anthropic model → resume will 400.
        let p = write(&[
            r#"{"type":"assistant","message":{"role":"assistant","id":"068a1f1cb34f9753729c184b73b6eb80","model":"claude-sonnet-4-5"}}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","id":"b93bfe4d-ca34-48e4"}}"#,
        ]);
        assert!(!transcript_is_resumable(&p));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn resumable_with_non_anthropic_model_even_without_msg_ids() {
        // DeepSeek proxied through Anthropic protocol — produces its own UUID ids, not msg_.
        // The SDK/provider resume mechanism handles these; the gate should NOT block them.
        let p = write(&[
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"assistant","message":{"model":"deepseek-v4-pro","id":"d41de33f-0967-42f0-9d8e-d2b3a46ec75b","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        ]);
        assert!(transcript_is_resumable(&p));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn not_resumable_when_no_assistant_messages() {
        // Transcript with only user/system messages → nothing to resume from.
        let p = write(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"system","message":{"role":"system","content":"sys"}}"#,
        ]);
        assert!(!transcript_is_resumable(&p));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn msg_ids_always_win_regardless_of_model() {
        // If there are msg_ ids, it's resumable regardless of what model produced them.
        let p = write(&[
            r#"{"type":"assistant","message":{"model":"gpt-4o","id":"chatcmpl-123","type":"message","role":"assistant"}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_01DEF","type":"message","role":"assistant"}}"#,
        ]);
        assert!(transcript_is_resumable(&p));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn missing_file_is_not_resumable() {
        assert!(!transcript_is_resumable(Path::new("/no/such/transcript.jsonl")));
    }
}
