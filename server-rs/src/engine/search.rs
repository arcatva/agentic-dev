use std::sync::Arc;

use serde::Serialize;

use crate::engine::Engine;
use crate::engine::store::Session;
use crate::engine::transcript::filter_rendered;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SearchField {
    Title, Repo, Branch, SessionId, Status, Error,
    Prompt,
    Notes, Answer,
    ToolName, ToolSummary, ToolDetail,
    SpawnDesc, SpawnResult,
    Skill, Workflow, Ask, Plan, Perm,
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SearchTier { A, B, C }

pub struct ClassifiedLine {
    pub field: SearchField,
    pub tier: SearchTier,
    pub text: String,
}

pub fn derive_tool_summary(name: &str, input: &serde_json::Value) -> String {
    fn s(v: &serde_json::Value) -> String { v.as_str().unwrap_or("").to_string() }
    match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => {
            std::path::Path::new(&s(&input["file_path"])).file_name()
                .and_then(|x| x.to_str()).unwrap_or("").to_string()
        }
        "Bash" => {
            s(&input["command"])
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .chars()
                .take(64)
                .collect()
        }
        "Glob" | "Grep" => s(&input["pattern"]),
        "WebFetch" | "WebSearch" => {
            let url = s(&input["url"]);
            if !url.is_empty() { url } else { s(&input["query"]) }
        }
        "TaskCreate" => s(&input["subject"]),
        "TaskUpdate" => {
            let status = s(&input["status"]);
            if status.is_empty() { s(&input["taskId"]) } else { status }
        }
        "ToolSearch" => s(&input["query"]),
        _ => String::new(),
    }
}

pub fn derive_tool_detail(name: &str, input: &serde_json::Value) -> String {
    fn s(v: &serde_json::Value) -> String { v.as_str().unwrap_or("").to_string() }
    match name {
        "Bash" => s(&input["command"]),
        "Edit" | "MultiEdit" => format!("- {}\n\n+ {}", s(&input["old_string"]), s(&input["new_string"])),
        "Write" => {
            let body = input.get("contents").and_then(|v| v.as_str())
                .or_else(|| input.get("content").and_then(|v| v.as_str()))
                .unwrap_or("");
            body.chars().take(4000).collect()
        }
        _ => {
            if let Some(obj) = input.as_object() {
                let mut parts: Vec<String> = Vec::with_capacity(obj.len());
                for (k, v) in obj {
                    let val_str: String = v.to_string().chars().take(800).collect();
                    parts.push(format!("{}: {}", k, val_str));
                }
                parts.join("\n")
            } else {
                String::new()
            }
        }
    }
}

pub fn classify_rendered_line(line: &str) -> Option<ClassifiedLine> {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "agentic_prompt" => {
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            Some(ClassifiedLine { field: SearchField::Prompt, tier: SearchTier::A, text })
        }
        "stream_event" => {
            let delta = v.get("event").and_then(|e| e.get("delta"));
            match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                Some("text_delta") => Some(ClassifiedLine {
                    field: SearchField::Notes,
                    tier: SearchTier::C,
                    text: delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string(),
                }),
                Some("thinking_delta") => None, // excluded by spec
                _ => None,
            }
        }
        "assistant" => {
            let content = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
            let Some(content) = content else { return None };
            // Pick the first tool_use block; if none, drop.
            for blk in content {
                if blk.get("type").and_then(|t| t.as_str()) != Some("tool_use") { continue; }
                let name = blk.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = blk.get("input").cloned().unwrap_or(serde_json::Value::Null);
                let (field, text) = match name {
                    "Skill" => (SearchField::Skill, input.get("skill").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                    "Workflow" => (SearchField::Workflow, input.get("name").and_then(|s| s.as_str())
                        .or_else(|| input.get("title").and_then(|s| s.as_str()))
                        .unwrap_or("").to_string()),
                    "AskUserQuestion" => (SearchField::Ask, serde_json::to_string(input.get("questions").unwrap_or(&serde_json::Value::Null)).unwrap_or_default()),
                    "Agent" | "Task" => (SearchField::SpawnDesc, input.get("description").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                    other => (SearchField::ToolName, derive_tool_summary(other, &input)),
                };
                return Some(ClassifiedLine { field, tier: SearchTier::C, text });
            }
            None
        }
        "result" => {
            let text = v.get("result").and_then(|s| s.as_str())
                .or_else(|| v.get("error").and_then(|s| s.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_default();
            if text.is_empty() { None } else { Some(ClassifiedLine { field: SearchField::Answer, tier: SearchTier::C, text }) }
        }
        "agent_result" => Some(ClassifiedLine {
            field: SearchField::SpawnResult,
            tier: SearchTier::C,
            text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        }),
        "agentic_perm" => {
            let kind = v.get("permKind").and_then(|s| s.as_str()).unwrap_or("perm");
            let field = if kind == "plan" { SearchField::Plan } else { SearchField::Perm };
            let text = v.get("plan").and_then(|s| s.as_str())
                .or_else(|| v.get("tool").and_then(|s| s.as_str()))
                .unwrap_or("").to_string();
            Some(ClassifiedLine { field, tier: SearchTier::C, text })
        }
        "agentic_file" => Some(ClassifiedLine {
            field: SearchField::Attachment,
            tier: SearchTier::C,
            text: v.get("path").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        }),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────
// SearchService
// ──────────────────────────────────────────────────────────────

const SNIPPET_MAX: usize = 200;
const PER_SESSION_MATCH_CAP: usize = 3;
const PER_SESSION_SCAN_CAP: usize = 50_000;
const QUERY_MIN_LEN: usize = 2;
const TOTAL_RESULTS_CAP: usize = 50;

#[derive(Debug, Serialize, Clone)]
pub struct SearchMatch {
    pub field: SearchField,
    pub snippet: String,
    #[serde(rename = "lineIndex")] pub line_index: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchHit { pub session: Session, pub score: f32, pub matches: Vec<SearchMatch> }

#[derive(Debug, Serialize)]
pub struct SearchResponse { pub query: String, pub results: Vec<SearchHit> }

pub struct SearchService { pub engine: Arc<Engine> }

impl SearchService {
    pub fn new(engine: Arc<Engine>) -> Self { Self { engine } }
    pub async fn search(&self, q: &str, limit: usize) -> SearchResponse {
        let query = q.trim();
        let mut resp = SearchResponse { query: query.to_string(), results: vec![] };
        if query.chars().count() < QUERY_MIN_LEN { return resp; }
        let needle = query.to_lowercase();
        let limit = limit.clamp(1, TOTAL_RESULTS_CAP);
        let sessions = self.engine.list().await;

        let mut hits: Vec<(u8, f32, SearchHit)> = vec![];
        for session in sessions {
            let mut matches: Vec<SearchMatch> = Vec::new();
            // Tier A
            if let Some(m) = metadata_match(SearchField::Title, &session.prompt, &needle) {
                matches.push(m);
            }
            // Tier B
            for repo in &session.repos {
                if let Some(m) = metadata_match(SearchField::Repo, repo, &needle) { matches.push(m); }
            }
            if let Some(branch) = &session.branch {
                if let Some(m) = metadata_match(SearchField::Branch, branch, &needle) { matches.push(m); }
            }
            if let Some(m) = metadata_match(SearchField::SessionId, &session.id, &needle) { matches.push(m); }
            if let Some(m) = metadata_match(SearchField::Status, &session.status, &needle) { matches.push(m); }
            if let Some(err) = &session.error {
                if let Some(m) = metadata_match(SearchField::Error, err, &needle) { matches.push(m); }
            }

            // Tier C — scan rendered projection
            let raw = self.engine.get_log(&session.id);
            let rendered = filter_rendered(&raw);
            for (i, line) in rendered.iter().take(PER_SESSION_SCAN_CAP).enumerate() {
                if let Some(c) = classify_rendered_line(line) {
                    if !c.text.is_empty() && c.text.to_lowercase().contains(&needle) {
                        // For tool calls, also surface summary and detail fields if they match —
                        // these are derived from the same tool_use input on the line we just
                        // classified. The line itself is the "ToolName" carrier; the JSON input is
                        // not stored on the classified line, so re-parse it from the raw line.
                        matches.push(SearchMatch {
                            field: c.field.clone(),
                            snippet: extract_snippet(&c.text, query),
                            line_index: i,
                        });
                        if let SearchField::ToolName = c.field {
                            if let Some(input) = tool_input_from_line(line) {
                                if let Some(tool_name) = tool_name_from_line(line) {
                                    let tool_matches = match_tool(&tool_name, &input, query, i);
                                    for m in tool_matches {
                                        if matches.len() >= PER_SESSION_MATCH_CAP { break; }
                                        matches.push(m);
                                    }
                                }
                            }
                        }
                        if matches.len() >= PER_SESSION_MATCH_CAP { break; }
                    }
                }
            }

            if matches.is_empty() { continue; }

            // Tier: A=0, B=1, C=2
            let tier = if matches.iter().any(|m| matches!(m.field, SearchField::Prompt | SearchField::Title)) { 0 }
                       else if matches.iter().any(|m| matches!(m.field, SearchField::Repo | SearchField::Branch | SearchField::SessionId | SearchField::Status | SearchField::Error)) { 1 }
                       else { 2 };
            let score = (matches.len() as f32) - (tier as f32) * 0.1;
            hits.push((tier, -score, SearchHit { session, score, matches }));
        }

        hits.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| b.2.session.last_user_message_at
                    .cmp(&a.2.session.last_user_message_at))
        });
        resp.results = hits.into_iter().take(limit).map(|(_, _, h)| h).collect();
        resp
    }
}

/// Case-insensitive metadata match: returns Some(SearchMatch) if `text` contains `needle`.
/// Snippet is the first SNIPPET_MAX chars of `text` (no query-window logic for short metadata
/// fields — they're short by nature).
fn metadata_match(field: SearchField, text: &str, needle: &str) -> Option<SearchMatch> {
    if text.to_lowercase().contains(needle) {
        Some(SearchMatch {
            field,
            snippet: text.chars().take(SNIPPET_MAX).collect(),
            line_index: 0,
        })
    } else {
        None
    }
}

/// Extract the tool name from a rendered `assistant` line (the first tool_use block).
/// Returns an owned String so callers don't outlive the parsed JSON.
fn tool_name_from_line(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let arr = v.get("message")?.get("content")?.as_array()?;
    for blk in arr {
        if blk.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            return blk.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
        }
    }
    None
}

/// Extract the tool input from a rendered `assistant` line.
fn tool_input_from_line(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("message")?.get("content")?.as_array()?
        .iter()
        .find(|blk| blk.get("type").and_then(|t| t.as_str()) == Some("tool_use"))?
        .get("input").cloned()
}

/// 0-2 matches: ToolSummary and/or ToolDetail when their derived strings contain the needle.
/// Caller is responsible for ToolName.
fn match_tool(name: &str, input: &serde_json::Value, query: &str, line_index: usize) -> Vec<SearchMatch> {
    let mut out = Vec::new();
    let needle = query.to_lowercase();
    let summary = derive_tool_summary(name, input);
    let detail = derive_tool_detail(name, input);
    if !summary.is_empty() && summary.to_lowercase().contains(&needle) {
        out.push(SearchMatch {
            field: SearchField::ToolSummary,
            snippet: extract_snippet(&summary, query),
            line_index,
        });
    }
    if !detail.is_empty() && detail.to_lowercase().contains(&needle) {
        out.push(SearchMatch {
            field: SearchField::ToolDetail,
            snippet: extract_snippet(&detail, query),
            line_index,
        });
    }
    out
}

pub fn extract_snippet(text: &str, query: &str) -> String {
    if text.chars().count() <= SNIPPET_MAX { return text.to_string(); }
    let lower = text.to_lowercase();
    let needle = query.to_lowercase();
    // `str::find` returns a BYTE offset, but the windowing below counts in CHARS (and slices `text`
    // via `char_indices`). Convert the match position to a char index — otherwise multi-byte text
    // (CJK is 3 bytes/char, emoji 4) centres the window several chars-per-byte too far to the right,
    // which for a long field drops the matched term out of the snippet entirely (it then has nothing
    // to highlight). For ASCII byte offset == char index, so this is a no-op there.
    let center = lower.find(&needle).map_or(0, |byte| lower[..byte].chars().count());
    // chars() is used for all length math (the snippet cap is 200 *chars*); byte offsets are derived
    // only for the final slice, since `&str[..]` indexes in bytes.
    let half = SNIPPET_MAX / 2;
    let start_char = center.saturating_sub(half);
    let end_char = (start_char + SNIPPET_MAX).min(text.chars().count());
    let start_char = end_char.saturating_sub(SNIPPET_MAX);
    let start_byte = text.char_indices().nth(start_char).map(|(i, _)| i).unwrap_or(text.len());
    let end_byte = text.char_indices().nth(end_char).map(|(i, _)| i).unwrap_or(text.len());
    let window = &text[start_byte..end_byte];
    let mut out = String::with_capacity(SNIPPET_MAX + 6);
    if start_char > 0 { out.push_str("..."); }
    out.push_str(window);
    if end_char < text.chars().count() { out.push_str("..."); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::engine::EngineConfig;
    use crate::engine::title_client::{TitleGenerator, TitleGeneratorError};
    use serde_json::json;
    use std::sync::Arc;

    /// No-op TitleGenerator for search tests. The search tests don't care
    /// about generated titles — they only assert on the search shape.
    struct SearchTestNoopTitleGenerator;
    #[async_trait::async_trait]
    impl TitleGenerator for SearchTestNoopTitleGenerator {
        async fn generate(
            &self,
            _p: &str,
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, TitleGeneratorError> {
            Ok(None)
        }
        async fn maybe_retitle(
            &self,
            _c: &str,
            _m: &[(String, String)],
            _cwd: &std::path::Path,
        ) -> Result<Option<String>, TitleGeneratorError> {
            Ok(None)
        }
    }

    /// Build a minimal Engine for search tests. Mirrors `engine::tests::make_engine` but
    /// doesn't need the rest of that module's helpers; the engine is fully booted (open store,
    /// recover, reconcile) so SearchService::search has a real `Engine` to call.
    async fn make_test_engine() -> Engine {
        static CTR: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir()
            .join(format!("agentic-search-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let cfg = EngineConfig {
            src_root: p.join("src"),
            worktrees_root: p.join("worktrees"),
            log_dir: p.join("logs"),
            db_path: p.join("db.sqlite"),
            max_concurrent: None,
            git_org: "arcatva".into(),
            claude_config_base: p.join("claude-config"),
            clone_fn: Some(Arc::new(|_url, _dest| {
                Err(std::io::Error::other("clone disabled in tests"))
            })),
            sync_fn: None,
            runner: None,
            log_fn: None,
            now_fn: None,
            push_fn: None,
            usage_fn: None,
            idle_max_ms: None,
            wall_max_ms: None,
            idle_ttl_ms: None,
            memory_max: None,
            memory_high: None,
            cpu_quota: None,
            tasks_max: None,
            title_generator: std::sync::Arc::new(SearchTestNoopTitleGenerator),
            retitle_enabled: false,
        };
        Engine::new(cfg).await.expect("engine::new")
    }

    #[tokio::test]
    async fn short_query_returns_empty() {
        let engine = make_test_engine().await;
        let svc = SearchService::new(Arc::new(engine));
        let r = svc.search("a", 50).await;
        assert!(r.results.is_empty());
        assert_eq!(r.query, "a");
    }

    #[tokio::test]
    async fn results_cap_to_limit() {
        let engine = make_test_engine().await;
        let svc = SearchService::new(Arc::new(engine));
        let r = svc.search("hello", 1).await;
        assert!(r.results.len() <= 1);
    }

    #[test]
    fn snippet_caps_at_200_and_ellipsis() {
        // 300-char ASCII text, needle not present: spec is "snippet cap 200 chars, ellipsis on cut".
        // The cut is at the trailing edge (needle is absent → window starts at byte 0), so the
        // truncated snippet must end with "..." and stay under 200 + ellipsis chars.
        let text: String = (0..300).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let s = extract_snippet(&text, "needle");
        assert!(s.len() <= 203, "snippet must be ≤ 200 chars + \"...\"; got len={}", s.len());
        assert!(s.ends_with("..."), "cut at trailing edge must append \"...\"; got: {s}");
    }

    #[test]
    fn snippet_prefers_query_window() {
        // 86-char text with "build failed" near the end. Spec cap is 200, so no truncation
        // needed — the query window IS the whole text. The snippet must contain the query.
        let text = "lorem ipsum dolor sit amet, consectetur adipiscing elit, build failed here somewhere";
        let s = extract_snippet(text, "build failed");
        assert!(s.contains("build failed"), "snippet must contain the query");
    }

    #[test]
    fn snippet_window_includes_a_cjk_match_in_a_long_field() {
        // 312 CJK chars (936 bytes), needle "如果" at char 60. With the old byte-offset-as-char-index
        // bug the window centred on byte 180 → chars [80, 280], which is PAST the match, so the
        // snippet omitted "如果" and there was nothing to highlight. The char-indexed centre keeps the
        // match inside the window.
        let needle = "如果";
        let text = format!("{}{}{}", "啊".repeat(60), needle, "吧".repeat(250));
        assert_eq!(text.chars().count(), 312);
        let s = extract_snippet(&text, needle);
        assert!(s.contains(needle), "CJK snippet must contain the matched needle; got: {s}");
    }

    #[test]
    fn classifies_prompt_and_text_lines() {
        let prompt = r#"{"type":"agentic_prompt","text":"hi","at":1}"#;
        let text = r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello"}}}"#;
        let thinking = r#"{"type":"stream_event","event":{"delta":{"type":"thinking_delta","thinking":"..."}}}"#;
        let tool_bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls","description":"list"}}]}}"#;
        let result_text = r#"{"type":"result","is_error":false,"result":"done","total_cost_usd":0.01}"#;
        let ask = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[{"question":"Q?","options":[]}]}}]}}"#;
        let perm = r#"{"type":"agentic_perm","permKind":"perm","id":"p1","tool":"Bash","input":{}}"#;
        let plan = r#"{"type":"agentic_perm","permKind":"plan","id":"pl1","plan":"x"}"#;
        let att = r#"{"type":"agentic_file","path":"a.png","at":1}"#;
        let sys = r#"{"type":"system","subtype":"init"}"#;
        let user_tr = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        let agent_res = r#"{"type":"agent_result","toolUseId":"tu_1","text":"found it"}"#;

        let cases = vec![
            (prompt, Some(SearchField::Prompt), Some(SearchTier::A)),
            (text, Some(SearchField::Notes), Some(SearchTier::C)),
            (thinking, None, None),
            (tool_bash, Some(SearchField::ToolName), Some(SearchTier::C)),
            (result_text, Some(SearchField::Answer), Some(SearchTier::C)),
            (ask, Some(SearchField::Ask), Some(SearchTier::C)),
            (perm, Some(SearchField::Perm), Some(SearchTier::C)),
            (plan, Some(SearchField::Plan), Some(SearchTier::C)),
            (att, Some(SearchField::Attachment), Some(SearchTier::C)),
            (sys, None, None),
            (user_tr, None, None),
            (agent_res, Some(SearchField::SpawnResult), Some(SearchTier::C)),
        ];
        for (line, expected_field, expected_tier) in cases {
            let got = classify_rendered_line(line);
            assert_eq!(got.as_ref().map(|c| c.field.clone()), expected_field, "line: {line}");
            assert_eq!(got.as_ref().map(|c| c.tier), expected_tier, "line: {line}");
        }
    }

    #[test]
    fn derives_bash_summary_and_detail() {
        let input = json!({"command":"ls -la /tmp","description":"list"});
        assert_eq!(derive_tool_summary("Bash", &input), "ls -la /tmp");
        assert_eq!(derive_tool_detail("Bash", &input), "ls -la /tmp");
    }

    #[test]
    fn derives_read_summary_from_file_path_basename() {
        let input = json!({"file_path":"/tmp/dir/foo.kt"});
        assert_eq!(derive_tool_summary("Read", &input), "foo.kt");
    }
}
