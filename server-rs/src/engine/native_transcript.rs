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

#[cfg(test)]
mod tests {
    use super::*;

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
}