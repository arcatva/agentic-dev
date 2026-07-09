use std::path::{Path, PathBuf};

#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct WorkflowAgent {
    #[serde(rename = "agentId")] pub agent_id: String,
    pub label: String,
    pub state: String,
    pub model: String,
    #[serde(rename = "phaseTitle", skip_serializing_if = "Option::is_none")] pub phase_title: Option<String>,
    #[serde(rename = "promptPreview", skip_serializing_if = "Option::is_none")] pub prompt_preview: Option<String>,
    #[serde(rename = "resultPreview", skip_serializing_if = "Option::is_none")] pub result_preview: Option<String>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct WorkflowPhase {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub detail: Option<String>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct WorkflowRun {
    #[serde(rename = "runId")] pub run_id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub summary: Option<String>,
    #[serde(rename = "agentCount", skip_serializing_if = "Option::is_none")] pub agent_count: Option<i64>,
    #[serde(rename = "createdAt")] pub created_at: i64,
    pub phases: Vec<WorkflowPhase>,
    pub agents: Vec<WorkflowAgent>,
    pub logs: Vec<String>,
}

/// Workflow run statuses that mean the run has finished (normalised: trim + lowercase).
/// Canonical terminal-status set for workflow runs.
pub const TERMINAL_STATUSES: &[&str] = &[
    "done", "complete", "completed", "failed", "error", "killed", "cancelled", "canceled",
];

/// True iff `status` (after trim + lowercase) is a terminal workflow status.
/// Input is trimmed and lowercased before comparison so callers need not normalise.
pub fn is_workflow_terminal(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status.trim().to_lowercase().as_str())
}

/// Project (claude `<configDir>/projects/<slug>/<sessionUuid>`) directories that hold workflow data.
///
/// `session_uuid` scopes the scan to ONE agentic session. This matters because the config dir is now
/// the SHARED `~/.claude` (so all sessions' transcripts live side by side under `projects/`); without
/// scoping, one session's workflow viewer would surface every other session's runs. A session's
/// claude transcript (`<slug>/<uuid>.jsonl`) and its run dirs (`<slug>/<uuid>/…`) live under a single
/// cwd-derived slug, so we only descend into slug dirs that contain that uuid. Encoding-independent:
/// we never reconstruct claude's cwd→slug transform, we just match the uuid claude actually wrote.
/// `None` → unscoped (legacy/per-session-dir callers and tests that pass an already-isolated base).
fn project_dirs(base: &Path, session_uuid: Option<&str>) -> Vec<PathBuf> {
    let projects = base.join("projects");
    let mut out = Vec::new();
    let Ok(slugs) = std::fs::read_dir(&projects) else { return out; };
    for slug in slugs.flatten() {
        let sp = slug.path();
        if let Some(uuid) = session_uuid {
            // Only this session's slug: it must hold the session's transcript file or run dir.
            if !sp.join(format!("{uuid}.jsonl")).exists() && !sp.join(uuid).exists() {
                continue;
            }
        }
        let Ok(sids) = std::fs::read_dir(&sp) else { continue; };
        for sid in sids.flatten() {
            let sd = sid.path();
            if sd.join("workflows").exists() || sd.join("subagents").join("workflows").exists() {
                out.push(sd);
            }
        }
    }
    out
}

fn safe_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c=='_' || c=='.' || c=='-')
}

fn text_of(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() { return s.to_string(); }
    if let Some(arr) = content.as_array() {
        return arr.iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>().join("\n");
    }
    String::new()
}

/// Round a since-epoch duration to the nearest millisecond
/// (subsecond nanos / 1e6, rounded — `as_millis()` would truncate instead).
fn round_ms(d: std::time::Duration) -> i64 {
    d.as_secs() as i64 * 1000 + (d.subsec_nanos() as f64 / 1_000_000.0).round() as i64
}

fn mtime_ms(p: &Path) -> Option<i64> {
    let md = std::fs::metadata(p).ok()?;
    let mt = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(round_ms(mt))
}

/// Creation time in ms, falling back to mtime when birthtime is unavailable (some platforms)
/// or 0.
fn birthtime_or_mtime_ms(p: &Path) -> Option<i64> {
    let md = std::fs::metadata(p).ok()?;
    if let Ok(d) = md.created().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(|_| std::io::Error::other("pre-epoch"))) {
        let ms = round_ms(d);
        if ms > 0 { return Some(ms); }
    }
    let mt = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(round_ms(mt))
}

fn run_name(dir: &Path, run_id: &str) -> String {
    let suffix = format!("-{run_id}.js");
    if let Ok(entries) = std::fs::read_dir(dir.join("workflows").join("scripts")) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(&suffix) { return n[..n.len()-suffix.len()].to_string(); }
        }
    }
    "workflow".to_string()
}

fn run_created_at(dir: &Path, run_id: &str, fallback: &Path) -> i64 {
    let suffix = format!("-{run_id}.js");
    if let Ok(entries) = std::fs::read_dir(dir.join("workflows").join("scripts")) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(&suffix) {
                if let Some(ms) = mtime_ms(&e.path()) { return ms; }   // scripts file → mtime
            }
        }
    }
    birthtime_or_mtime_ms(fallback).unwrap_or(0)   // fallback → birthtime, or mtime if birthtime unavailable
}

pub(crate) fn read_running_run(dir: &Path, run_dir: &Path, run_id: &str) -> Option<WorkflowRun> {
    // Parse each agent-<id>.meta.json so the running run shows real per-worker label/model/phase/prompt
    // (delegate writes these in start_live_run; native workflows write at least {"agentType":...}).
    let mut metas: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
    let mut ids: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(stripped) = n.strip_prefix("agent-").and_then(|s| s.strip_suffix(".meta.json")) {
                ids.push(stripped.to_string());
                if let Some(m) = std::fs::read_to_string(e.path()).ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                {
                    metas.insert(stripped.to_string(), m);
                }
            }
        }
    } else { return None; }
    ids.sort();
    let mut done = std::collections::HashSet::new();
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let jf = run_dir.join("journal.jsonl");
    if let Ok(text) = std::fs::read_to_string(&jf) {
        for line in text.split('\n') {
            if line.trim().is_empty() { continue; }
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
            let id = o.get("agentId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id.is_empty() { continue; }
            if !ids.contains(&id) { ids.push(id.clone()); }
            if o.get("type").and_then(|t| t.as_str()) == Some("result") {
                done.insert(id.clone());
                if let Some(r) = o.get("result").and_then(|r| r.as_str()) { results.insert(id, r.to_string()); }
            }
        }
    }
    if ids.is_empty() { return None; }
    let agents = ids.iter().enumerate().map(|(i, id)| {
        let m = metas.get(id);
        let label = m
            .and_then(|m| m.get("label").and_then(|v| v.as_str())
                .or_else(|| m.get("agentType").and_then(|v| v.as_str())))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("agent {}", i + 1));
        WorkflowAgent {
            agent_id: id.clone(),
            label,
            state: if done.contains(id) { "done".into() } else { "running".into() },
            // The live meta's `model` is EMPTY for a native-fallback worker (it ran on the subscription
            // default, not a registered provider, so delegate had no model string to record). Backfill
            // the real tier from the worker's transcript — the SAME source the completion summary uses
            // (delegate::extract_model_from_transcript) — so the RUNNING card shows e.g.
            // "claude-opus-4-8[1m]" instead of a blank model. Bounded read (first ~100 lines); only runs
            // when the meta model is empty, so routed workers (non-empty model) pay nothing.
            model: {
                let meta_model = m
                    .and_then(|m| m.get("model").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if meta_model.is_empty() {
                    crate::engine::delegate::extract_model_from_transcript(
                        &run_dir.join(format!("agent-{id}.jsonl")),
                    )
                    .unwrap_or_default()
                } else {
                    meta_model.to_string()
                }
            },
            phase_title: m.and_then(|m| m.get("phaseTitle").and_then(|v| v.as_str())).map(String::from),
            prompt_preview: m.and_then(|m| m.get("promptPreview").and_then(|v| v.as_str())).map(String::from),
            result_preview: results.get(id).cloned(),
        }
    }).collect::<Vec<_>>();
    // Run-level meta gives the running run its real title + phases; without it (e.g. native workflows)
    // fall back to the script-derived name and no phases — the prior behaviour.
    let run_meta = std::fs::read_to_string(run_dir.join("run.meta.json")).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let name = run_meta.as_ref()
        .and_then(|m| m.get("name").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| run_name(dir, run_id));
    let phases = run_meta.as_ref()
        .and_then(|m| m.get("phases").and_then(|v| v.as_array()))
        .map(|arr| arr.iter().map(|p| WorkflowPhase {
            title: p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            detail: p.get("detail").and_then(|v| v.as_str()).map(String::from),
        }).collect::<Vec<_>>())
        .unwrap_or_default();
    Some(WorkflowRun {
        run_id: run_id.to_string(), name, status: "running".into(),
        summary: None, agent_count: Some(ids.len() as i64),
        created_at: run_created_at(dir, run_id, run_dir),
        phases, agents, logs: vec![],
    })
}

pub fn list_workflows(base: &Path, session_uuid: Option<&str>) -> Vec<WorkflowRun> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in project_dirs(base, session_uuid) {
        let wf_dir = dir.join("workflows");
        if let Ok(entries) = std::fs::read_dir(&wf_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".json") { continue; }
                let Ok(text) = std::fs::read_to_string(e.path()) else { continue; };
                let Ok(d) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
                let run_id = d.get("runId").and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.trim_end_matches(".json").to_string());
                if seen.contains(&run_id) { continue; }
                seen.insert(run_id.clone());
                let agents = d.get("workflowProgress").and_then(|v| v.as_array()).map(|arr| arr.iter()
                    .filter(|a| a.get("type").and_then(|t| t.as_str()) == Some("workflow_agent"))
                    .map(|a| WorkflowAgent {
                        agent_id: a.get("agentId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        label: a.get("label").and_then(|v| v.as_str()).unwrap_or("agent").to_string(),
                        state: a.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        model: a.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        phase_title: a.get("phaseTitle").and_then(|v| v.as_str()).map(String::from),
                        prompt_preview: a.get("promptPreview").and_then(|v| v.as_str()).map(String::from),
                        result_preview: a.get("resultPreview").and_then(|v| v.as_str()).map(String::from),
                    }).collect::<Vec<_>>()).unwrap_or_default();
                let phases = d.get("phases").and_then(|v| v.as_array()).map(|arr| arr.iter().map(|p| WorkflowPhase {
                    title: p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    detail: p.get("detail").and_then(|v| v.as_str()).map(String::from),
                }).collect::<Vec<_>>()).unwrap_or_default();
                let created_at = d.get("createdAt").or_else(|| d.get("startedAt"))
                    .and_then(|v| v.as_i64()).filter(|&n| n != 0)
                    .unwrap_or_else(|| run_created_at(&dir, &run_id, &e.path()));
                out.push(WorkflowRun {
                    run_id,
                    name: d.get("workflowName").and_then(|v| v.as_str()).unwrap_or("workflow").to_string(),
                    status: d.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    summary: d.get("summary").and_then(|v| v.as_str()).map(String::from),
                    agent_count: d.get("agentCount").and_then(|v| v.as_i64()),
                    created_at, phases, agents,
                    logs: d.get("logs").and_then(|v| v.as_array()).map(|a| a.iter()
                        .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect())
                        .unwrap_or_default(),
                });
            }
        }
        let sub_dir = dir.join("subagents").join("workflows");
        if let Ok(entries) = std::fs::read_dir(&sub_dir) {
            for e in entries.flatten() {
                let run_id = e.file_name().to_string_lossy().into_owned();
                if seen.contains(&run_id) { continue; }
                if !e.path().is_dir() { continue; }
                if let Some(run) = read_running_run(&dir, &e.path(), &run_id) {
                    seen.insert(run_id);
                    out.push(run);
                }
            }
        }
    }
    out
}

pub fn read_workflow_agent(base: &Path, session_uuid: Option<&str>, run_id: &str, agent_id: &str) -> String {
    if !safe_segment(run_id) || !safe_segment(agent_id) { return String::new(); }
    let mut found: Option<PathBuf> = None;
    for dir in project_dirs(base, session_uuid) {
        let cand = dir.join("subagents").join("workflows").join(run_id).join(format!("agent-{agent_id}.jsonl"));
        if cand.exists() { found = Some(cand); break; }
    }
    let Some(f) = found else { return String::new(); };
    let Ok(text) = std::fs::read_to_string(&f) else { return String::new(); };
    let (mut input, mut output): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for line in text.split('\n') {
        if line.trim().is_empty() { continue; }
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
        match o.get("type").and_then(|t| t.as_str()) {
            Some("user") => { let t = text_of(o.get("message").and_then(|m| m.get("content")).unwrap_or(&serde_json::Value::Null)); if !t.is_empty() { input.push(t); } }
            Some("assistant") => { let t = text_of(o.get("message").and_then(|m| m.get("content")).unwrap_or(&serde_json::Value::Null)); if !t.is_empty() { output.push(t); } }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if !input.is_empty() { parts.push(format!("**Task**\n\n{}", input.join("\n\n"))); }
    if !output.is_empty() { parts.push(format!("**Output**\n\n{}", output.join("\n\n"))); }
    parts.join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-wf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn sid_dir(base: &PathBuf) -> PathBuf {
        let d = base.join("projects").join("slug").join("sid");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_a_completed_summary_run() {
        let b = base();
        let sd = sid_dir(&b);
        let wf = sd.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("wf_1.json"), serde_json::json!({
            "runId":"wf_1","workflowName":"build","status":"done","createdAt":1234,
            "phases":[{"title":"plan"}],
            "workflowProgress":[{"type":"workflow_agent","agentId":"a1","label":"A","state":"done","model":"opus"}]
        }).to_string()).unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "wf_1");
        assert_eq!(runs[0].name, "build");
        assert_eq!(runs[0].status, "done");
        assert_eq!(runs[0].created_at, 1234);
        assert_eq!(runs[0].agents.len(), 1);
        assert_eq!(runs[0].agents[0].agent_id, "a1");
        assert_eq!(runs[0].phases[0].title, "plan");
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn synthesizes_a_running_run_from_meta_and_journal() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_2");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("agent-a1.meta.json"), "{}").unwrap();
        std::fs::write(run.join("agent-a2.meta.json"), "{}").unwrap();
        std::fs::write(run.join("journal.jsonl"),
            "{\"agentId\":\"a1\",\"type\":\"result\",\"result\":\"ok\"}\n").unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.run_id, "wf_2");
        assert_eq!(r.status, "running");
        assert_eq!(r.agent_count, Some(2));
        let a1 = r.agents.iter().find(|a| a.agent_id == "a1").unwrap();
        assert_eq!(a1.state, "done");
        assert_eq!(a1.result_preview.as_deref(), Some("ok"));
        let a2 = r.agents.iter().find(|a| a.agent_id == "a2").unwrap();
        assert_eq!(a2.state, "running");
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn running_run_backfills_native_worker_model_from_transcript() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_bf");
        std::fs::create_dir_all(&run).unwrap();
        // w1: native-fallback worker — its spawn model is EMPTY in the meta (it ran on the subscription
        // default), and the real tier lives only in the transcript's system/init line.
        std::fs::write(run.join("agent-w1.meta.json"),
            serde_json::json!({"label":"w","model":""}).to_string()).unwrap();
        std::fs::write(run.join("agent-w1.jsonl"),
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-opus-4-8[1m]\"}\n").unwrap();
        // w2: routed worker — its meta already carries the provider model; the transcript must NOT
        // override it (the backfill only fires when the meta model is empty).
        std::fs::write(run.join("agent-w2.meta.json"),
            serde_json::json!({"label":"w","model":"MiniMax-M3"}).to_string()).unwrap();
        std::fs::write(run.join("agent-w2.jsonl"),
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-opus-4-8[1m]\"}\n").unwrap();
        std::fs::write(run.join("journal.jsonl"), "").unwrap(); // live (no summary) → read as running

        let runs = list_workflows(&b, None);
        let r = runs.iter().find(|r| r.run_id == "wf_bf").unwrap();
        let w1 = r.agents.iter().find(|a| a.agent_id == "w1").unwrap();
        assert_eq!(w1.model, "claude-opus-4-8[1m]"); // backfilled from the transcript
        let w2 = r.agents.iter().find(|a| a.agent_id == "w2").unwrap();
        assert_eq!(w2.model, "MiniMax-M3"); // routed model preserved, not overridden
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn summary_wins_over_live_journal_for_same_runid() {
        let b = base();
        let sd = sid_dir(&b);
        std::fs::create_dir_all(sd.join("workflows")).unwrap();
        std::fs::write(sd.join("workflows").join("dup.json"),
            serde_json::json!({"runId":"dup","status":"done"}).to_string()).unwrap();
        let live = sd.join("subagents").join("workflows").join("dup");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("agent-x.meta.json"), "{}").unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.iter().filter(|r| r.run_id == "dup").count(), 1);
        assert_eq!(runs[0].status, "done");   // summary, not "running"
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn read_agent_formats_task_and_output_and_rejects_traversal() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_9");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("agent-a.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"do it\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n").unwrap();
        let t = read_workflow_agent(&b, None, "wf_9", "a");
        assert!(t.contains("**Task**"));
        assert!(t.contains("do it"));
        assert!(t.contains("**Output**"));
        assert!(t.contains("done"));
        assert!(t.contains("---"));
        // traversal rejected
        assert_eq!(read_workflow_agent(&b, None, "../etc", "a"), "");
        assert_eq!(read_workflow_agent(&b, None, "wf_9", "a/b"), "");
        // missing → empty
        assert_eq!(read_workflow_agent(&b, None, "nope", "a"), "");
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn terminal_status_check_normalises_trim_and_case() {
        // trim + lowercase normalisation before comparison
        assert!(is_workflow_terminal("  DONE  "));
        assert!(is_workflow_terminal("Completed"));
        assert!(is_workflow_terminal("CANCELLED"));
        assert!(is_workflow_terminal("canceled")); // both US + UK spellings are terminal
        assert!(is_workflow_terminal("Error"));
        // non-terminal / unknown statuses
        assert!(!is_workflow_terminal("running"));
        assert!(!is_workflow_terminal("queued"));
        assert!(!is_workflow_terminal("")); // empty (unset status) is active, not terminal
        assert!(!is_workflow_terminal("done x")); // substring must not match the whole token
    }


    #[test]
    fn summary_runid_falls_back_to_filename_and_skips_malformed_sibling() {
        let b = base();
        let sd = sid_dir(&b);
        let wf = sd.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        // no "runId" key → run_id derived from the filename (sans ".json")
        std::fs::write(wf.join("noid.json"),
            serde_json::json!({"workflowName":"derived","status":"failed"}).to_string()).unwrap();
        // a malformed JSON file must be skipped, not abort the whole scan
        std::fs::write(wf.join("broken.json"), "{ this is not json").unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "noid");
        assert_eq!(runs[0].name, "derived");
        assert_eq!(runs[0].status, "failed");
        std::fs::remove_dir_all(&b).ok();
    }


    #[test]
    fn zero_created_at_falls_back_to_file_time_not_zero() {
        let b = base();
        let sd = sid_dir(&b);
        let wf = sd.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        // createdAt:0 and startedAt:0 must both be rejected by the `n != 0` filter,
        // falling back to the json file's birthtime/mtime (a real, nonzero timestamp).
        std::fs::write(wf.join("wf_z.json"), serde_json::json!({
            "runId":"wf_z","workflowName":"zerots","status":"done",
            "createdAt":0,"startedAt":0
        }).to_string()).unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].created_at > 0, "createdAt should fall back to file time, got {}", runs[0].created_at);
        std::fs::remove_dir_all(&b).ok();
    }


    #[test]
    fn running_run_picks_up_agents_present_only_in_journal() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_j");
        std::fs::create_dir_all(&run).unwrap();
        // only one meta file on disk...
        std::fs::write(run.join("agent-a1.meta.json"), "{}").unwrap();
        // ...but the journal references a second agent that has no meta file yet.
        std::fs::write(run.join("journal.jsonl"),
            "{\"agentId\":\"a1\",\"type\":\"result\",\"result\":\"first\"}\n\
             {\"agentId\":\"a2\",\"type\":\"start\"}\n\
             \n\
             not-json-line\n").unwrap();
        let runs = list_workflows(&b, None);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.status, "running");
        // a2 was added from the journal even though it had no meta file
        assert_eq!(r.agent_count, Some(2));
        let a1 = r.agents.iter().find(|a| a.agent_id == "a1").unwrap();
        assert_eq!(a1.state, "done");
        assert_eq!(a1.result_preview.as_deref(), Some("first"));
        let a2 = r.agents.iter().find(|a| a.agent_id == "a2").unwrap();
        assert_eq!(a2.state, "running"); // no result row → still running
        assert!(a2.result_preview.is_none());
        std::fs::remove_dir_all(&b).ok();
    }


    #[test]
    fn empty_run_dir_with_no_agents_is_dropped() {
        let b = base();
        let sd = sid_dir(&b);
        // a live workflow dir that has no agent meta files and no journal → read_running_run
        // returns None (ids empty), so the run must not surface at all.
        let run = sd.join("subagents").join("workflows").join("wf_empty");
        std::fs::create_dir_all(&run).unwrap();
        let runs = list_workflows(&b, None);
        assert!(runs.iter().all(|r| r.run_id != "wf_empty"));
        assert!(runs.is_empty());
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn scopes_to_session_uuid_ignoring_other_sessions() {
        // With the shared ~/.claude config dir, projects/ holds EVERY session's transcripts.
        // session_uuid must scope list/read to one session's slug, or the workflow viewer leaks
        // other sessions' runs.
        let b = base();
        // session A: its own cwd-slug, transcript uuid "uuidA", one live run.
        let a_run = b.join("projects").join("-slug-a").join("uuidA")
            .join("subagents").join("workflows").join("wf_a");
        std::fs::create_dir_all(&a_run).unwrap();
        std::fs::write(a_run.join("agent-x.meta.json"), "{}").unwrap();
        // session B: different slug + uuid, different run.
        let b_run = b.join("projects").join("-slug-b").join("uuidB")
            .join("subagents").join("workflows").join("wf_b");
        std::fs::create_dir_all(&b_run).unwrap();
        std::fs::write(b_run.join("agent-y.meta.json"), "{}").unwrap();

        // scoped to A → only A's run
        let only_a = list_workflows(&b, Some("uuidA"));
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].run_id, "wf_a");
        // scoped to B → only B's run
        let only_b = list_workflows(&b, Some("uuidB"));
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].run_id, "wf_b");
        // an agent read scoped to A cannot reach B's run dir
        assert_eq!(read_workflow_agent(&b, Some("uuidA"), "wf_b", "y"), "");
        // unscoped (None) still sees both — preserves the per-session-dir/test callers
        assert_eq!(list_workflows(&b, None).len(), 2);
        std::fs::remove_dir_all(&b).ok();
    }

}
