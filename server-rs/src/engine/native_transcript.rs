// Native transcript reader + discovery scan — Task 2 of the adopt & re-sync plan.
//
// Reads #2 (`~/.claude/projects/<slug>/<csid>.jsonl`) and exposes a discovery scan
// (`scan_adoptable`) used by `GET /api/adoptable`. Translation to rendered #1 lines
// lives in Task 3; this file only knows how to read #2 and decide "is this adoptable?".

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The verbatim `cwd -> slug` rule. Must stay byte-identical to `engine/mod.rs:1745`.
pub fn slug_for_cwd(cwd: &str) -> String {
    cwd.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
}

/// `<claude_config_base>/projects/<slug>/<csid>.jsonl`.
pub fn transcript_path(config_base: &Path, cwd: &str, csid: &str) -> PathBuf {
    config_base.join("projects").join(slug_for_cwd(cwd)).join(format!("{csid}.jsonl"))
}

/// Validate a `claudeSessionId` supplied over the HTTP API before it is ever interpolated
/// into a filesystem path (`transcript_path` above does `format!("{csid}.jsonl")` with no
/// further sanitization). A csid must be a non-empty run of `[A-Za-z0-9._-]` with no path
/// separator, no `..` traversal segment, and must not start with `.` (blocks dotfiles and
/// the `..` / `.` special segments alike). Anything else — including an absolute path or a
/// relative traversal like `../../../../etc/passwd` — is rejected.
pub fn is_valid_csid(csid: &str) -> bool {
    if csid.is_empty() {
        return false;
    }
    if csid.starts_with('.') {
        return false;
    }
    if csid.contains('/') || csid.contains('\\') || csid.contains("..") {
        return false;
    }
    csid
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[derive(serde::Serialize, Debug)]
pub struct Adoptable {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    pub slug: String,
    #[serde(rename = "firstPrompt")]
    pub first_prompt: String,
    #[serde(rename = "mtimeMs")]
    pub mtime_ms: i64,
    pub resumable: bool,
    #[serde(rename = "lineCount")]
    pub line_count: i64,
}

/// RFC 3339 → epoch ms. Reuses the minimal parser in `auto_resume` (no chrono dep).
fn iso_to_ms(ts: &str) -> i64 {
    super::auto_resume::rfc3339_to_epoch_ms(ts).unwrap_or(0)
}

/// Extract plain user text from a native `user` line's `message.content`.
/// Returns `None` for tool-result-only / meta / sidechain / non-user lines.
pub fn user_prompt_text(line: &serde_json::Value) -> Option<String> {
    if line.get("type").and_then(|v| v.as_str()) != Some("user") { return None; }
    for flag in ["isMeta", "isSidechain", "isCompactSummary"] {
        if line.get(flag).and_then(|v| v.as_bool()) == Some(true) { return None; }
    }
    let content = line.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return if s.trim().is_empty() { None } else { Some(s.to_string()) };
    }
    let arr = content.as_array()?;
    let text: String = arr.iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() { None } else { Some(text) }
}

/// Read a JSONL transcript into a Vec<Value>; bad lines are skipped, missing file = empty.
pub(crate) fn read_lines(path: &Path) -> Vec<serde_json::Value> {
    let Ok(raw) = std::fs::read_to_string(path) else { return vec![]; };
    raw.lines().filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Walk `<config_base>/projects/*/*.jsonl`, skip any `session_id` already in `known_csids`,
/// return newest-first.
pub fn scan_adoptable(config_base: &Path, known_csids: &HashSet<String>) -> Vec<Adoptable> {
    let root = config_base.join("projects");
    let mut out = vec![];
    let Ok(projects) = std::fs::read_dir(&root) else { return out; };
    for proj in projects.flatten() {
        let Ok(files) = std::fs::read_dir(proj.path()) else { continue; };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            let Some(csid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue; };
            if known_csids.contains(&csid) { continue; }
            let lines = read_lines(&p);
            if lines.is_empty() { continue; }
            let cwd = lines.iter().find_map(|l| l.get("cwd").and_then(|v| v.as_str())).unwrap_or("").to_string();
            let first_prompt = lines.iter().find_map(|l| user_prompt_text(l)).unwrap_or_default();
            let resumable = super::resume_gate::transcript_is_resumable(&p);
            let mtime_ms = f.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            out.push(Adoptable {
                session_id: csid,
                slug: p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string(),
                cwd, first_prompt, mtime_ms, resumable, line_count: lines.len() as i64,
            });
        }
    }
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    out
}

/// Translate a slice of native #2 lines into ready-to-append #1 JSONL strings
/// (no trailing newline). User lines with real authored text become
/// `agentic_prompt` (with `at` = ms of the native timestamp); assistant lines
/// become `assistant` (with the native message object verbatim), plus an extra
/// `result` line on `stop_reason == "end_turn"` as the turn-boundary marker.
pub fn translate_lines(native: &[serde_json::Value]) -> Vec<String> {
    let mut out = vec![];
    for line in native {
        match line.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                if let Some(text) = user_prompt_text(line) {
                    let at = line.get("timestamp").and_then(|v| v.as_str()).map(iso_to_ms).unwrap_or(0);
                    out.push(serde_json::json!({"type":"agentic_prompt","text":text,"at":at}).to_string());
                }
            }
            Some("assistant") => {
                if let Some(msg) = line.get("message") {
                    out.push(serde_json::json!({"type":"assistant","message":msg}).to_string());
                    if msg.get("stop_reason").and_then(|v| v.as_str()) == Some("end_turn") {
                        out.push(serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":""}).to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Translate native #2 lines `[from_line .. end)` into ready-to-append #1
/// strings. Returns `(lines, new_total_line_count)` — `total` is the line count
/// of the whole file at read time, used by the watermark bookkeeping.
pub fn translate_range(path: &Path, from_line: usize) -> (Vec<String>, usize) {
    let all = read_lines(path);
    let total = all.len();
    let slice = if from_line >= total { &[][..] } else { &all[from_line..] };
    (translate_lines(slice), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_csid_accepts_plain_ids_rejects_traversal_and_absolute_paths() {
        // Plausible real csids: accepted.
        assert!(is_valid_csid("csid-A"));
        assert!(is_valid_csid("a1b2c3_d4.e5"));
        assert!(is_valid_csid("00000000-0000-0000-0000-000000000000"));

        // Rejected: empty, traversal, absolute, separators, leading dot.
        assert!(!is_valid_csid(""));
        assert!(!is_valid_csid("../../../../etc/passwd"));
        assert!(!is_valid_csid("/etc/passwd"));
        assert!(!is_valid_csid(".."));
        assert!(!is_valid_csid("."));
        assert!(!is_valid_csid(".hidden"));
        assert!(!is_valid_csid("a/b"));
        assert!(!is_valid_csid("a\\b"));
        assert!(!is_valid_csid("a..b"));
        assert!(!is_valid_csid("csid with spaces"));
    }

    #[test]
    fn slug_matches_engine_rule() {
        assert_eq!(slug_for_cwd("/home/me/proj"), "-home-me-proj");
        assert_eq!(slug_for_cwd("/a/b_c"), "-a-b-c");
    }

    #[test]
    fn scan_lists_resumable_and_skips_known() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects").join("-home-me-proj");
        std::fs::create_dir_all(&projects).unwrap();
        // resumable: has a msg_ id and a real user prompt
        std::fs::write(projects.join("csid-A.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-07-09T00:00:00Z\",\"cwd\":\"/home/me/proj\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"stop_reason\":\"end_turn\"}}\n").unwrap();
        // already adopted -> must be skipped
        std::fs::write(projects.join("csid-known.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n").unwrap();

        let known: std::collections::HashSet<String> = ["csid-known".to_string()].into_iter().collect();
        let got = scan_adoptable(tmp.path(), &known);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, "csid-A");
        assert_eq!(got[0].cwd, "/home/me/proj");
        assert_eq!(got[0].first_prompt, "hello");
        assert!(got[0].resumable);
    }

    #[test]
    fn translate_maps_user_and_assistant() {
        let native: Vec<serde_json::Value> = [
            r#"{"type":"user","timestamp":"2026-07-09T00:00:00Z","message":{"role":"user","content":"do a thing"}}"#,
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","model":"claude-x","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta noise"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#,
        ].iter().map(|s| serde_json::from_str(s).unwrap()).collect();

        let out = translate_lines(&native);
        let types: Vec<String> = out.iter()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["type"].as_str().unwrap().to_string())
            .collect();
        // user prompt -> agentic_prompt ; assistant -> assistant + result(end_turn) ; meta + tool_result-only user -> skipped
        assert_eq!(types, vec!["agentic_prompt", "assistant", "result"]);

        let first: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(first["text"], "do a thing");
        // ms of 2026-07-09T00:00:00Z. (The plan's 1783641600000 is one day off — that
        // is the epoch-ms of 2026-07-10T00:00:00Z. The correct value is below.)
        assert_eq!(first["at"], 1783555200000i64);
    }

    #[test]
    fn translate_range_slices_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("range.jsonl");
        // 2 native lines: a real user prompt + an assistant end_turn (the watermark
        // reconcile path depends on translate_range returning the right slice and
        // a stable total even at the boundaries).
        std::fs::write(&path,
            "{\"type\":\"user\",\"timestamp\":\"2026-07-09T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();
        let native_total = read_lines(&path).len();
        assert_eq!(native_total, 2);

        // (a) from_line=0 -> whole file's translated lines, total = native line count.
        let (all, total) = translate_range(&path, 0);
        assert_eq!(total, native_total);
        // 2 native lines => agentic_prompt + assistant + result(end_turn) = 3 lines.
        assert_eq!(all.len(), 3);

        // (b) from_line=total -> empty vec, total unchanged, no panic.
        let (empty, total_eq) = translate_range(&path, total);
        assert!(empty.is_empty(), "from_line==total should yield empty vec");
        assert_eq!(total_eq, total, "total must be unchanged when slice is empty");

        // (c) from_line=total+5 -> empty vec, no panic, total still unchanged.
        let (empty2, total_far) = translate_range(&path, total + 5);
        assert!(empty2.is_empty(), "from_line>total should yield empty vec");
        assert_eq!(total_far, total, "total must be unchanged for out-of-range from_line");
    }
}