//! Journal writer for "delegate" cheap-worker fan-out runs.
//!
//! When the main (subscription) Claude session calls the `delegate` tool, the backend spawns
//! cheap worker processes (pointed at minimax/deepseek/... Anthropic-compatible endpoints) and
//! transcribes their output into the on-disk workflow-journal format that `engine::workflows`
//! reads — so the cheap workers show up in the *calling session's* workflow tab, reusing the
//! existing reader/API/client with no client changes.
//!
//! Layout, written under the caller session's journal dir
//! (`<configBase>/projects/<slug>/<uuid>/`):
//!   subagents/workflows/<run_id>/agent-<id>.meta.json   live: marks a worker exists
//!   subagents/workflows/<run_id>/journal.jsonl          live: per-worker status lines
//!   subagents/workflows/<run_id>/agent-<id>.jsonl       per-worker transcript (read on click — KEPT)
//!   workflows/wf_<run_id>.json                          summary, written on completion
//!
//! Invariant from `engine::workflows`: a `workflows/wf_<run_id>.json` summary "wins" over the live
//! `subagents/workflows/<run_id>/` dir for the same run_id (the reader dedups, summary first). So we
//! write ONLY the live dir while running, and ADD the summary on completion — but we KEEP the live
//! `agent-<id>.jsonl` transcripts, because `read_workflow_agent` always reads them from there.
//!
//! The writer functions are exercised by the round-trip test below; they are wired into the
//! `/api/sessions/:id/delegate` route + worker tailer in a follow-up slice.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::engine::runner::Runner;
use crate::engine::spawner::{encode_user_message, spawn_claude, SpawnOptions};
use crate::engine::workflows::{read_running_run, WorkflowAgent, WorkflowPhase};

/// One cheap worker's final outcome, used to build the completion summary.
#[derive(Clone, Debug, Default)]
pub struct DelegateAgentResult {
    pub agent_id: String,
    pub label: String,
    pub model: String,
    pub prompt_preview: Option<String>,
    pub result_preview: Option<String>,
    pub failed: bool,
    /// Why the LLM router picked this model (None when pinned/heuristic).
    pub route_reason: Option<String>,
    /// Phase this worker belongs to (groups workers under phase headers, like a native Workflow).
    pub phase_title: Option<String>,
}

fn run_dir(journal_dir: &Path, run_id: &str) -> PathBuf {
    journal_dir.join("subagents").join("workflows").join(run_id)
}

/// Create the live run dir with RICH per-worker + run-level meta (called when the delegate run
/// starts). `engine::workflows::read_running_run` reads these so the run shows its real title,
/// phases, and per-worker label/model/phase WHILE it spins — not a generic "agent N" placeholder.
/// `run.meta.json` carries the run title + phases; each `agent-<id>.meta.json` carries that worker's
/// label/model/phase/prompt.
pub fn start_live_run(
    journal_dir: &Path,
    run_id: &str,
    name: &str,
    phases: &[WorkflowPhase],
    workers: &[WorkerSpec],
) -> std::io::Result<()> {
    let rd = run_dir(journal_dir, run_id);
    std::fs::create_dir_all(&rd)?;
    // Run-level meta: title + phases for the running run (delegate has no workflows/scripts/*.js file
    // for `run_name` to derive a title from, so without this it falls back to the generic "workflow").
    let run_meta = serde_json::json!({
        "name": name,
        "phases": phases.iter()
            .map(|p| serde_json::json!({ "title": p.title, "detail": p.detail }))
            .collect::<Vec<_>>(),
    });
    std::fs::write(rd.join("run.meta.json"), serde_json::to_string(&run_meta)?)?;
    // Per-worker meta so each running agent row shows its label/model/phase/prompt immediately.
    for w in workers {
        let am = serde_json::json!({
            "label": w.label,
            "model": w.model,
            "phaseTitle": w.phase_title,
            "promptPreview": w.prompt.chars().take(200).collect::<String>(),
        });
        std::fs::write(
            rd.join(format!("agent-{}.meta.json", w.agent_id)),
            serde_json::to_string(&am)?,
        )?;
    }
    // Ensure journal.jsonl exists (empty) so the live run is well-formed.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rd.join("journal.jsonl"))?;
    Ok(())
}

/// Append a status line to the live journal.
pub fn append_journal(
    journal_dir: &Path,
    run_id: &str,
    line: &serde_json::Value,
) -> std::io::Result<()> {
    let jf = run_dir(journal_dir, run_id).join("journal.jsonl");
    if let Some(parent) = jf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jf)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Mark a worker done in the live journal (status -> done, with a short result preview).
pub fn mark_agent_done(
    journal_dir: &Path,
    run_id: &str,
    agent_id: &str,
    result: &str,
) -> std::io::Result<()> {
    append_journal(
        journal_dir,
        run_id,
        &serde_json::json!({ "agentId": agent_id, "type": "result", "result": result }),
    )
}

/// Write a worker's transcript (`agent-<id>.jsonl`), read on click. Each entry is the
/// `{type, message:{content}}` shape `read_workflow_agent` expects (type "user"/"assistant").
pub fn write_agent_transcript(
    journal_dir: &Path,
    run_id: &str,
    agent_id: &str,
    lines: &[serde_json::Value],
) -> std::io::Result<()> {
    let rd = run_dir(journal_dir, run_id);
    std::fs::create_dir_all(&rd)?;
    let mut f = std::fs::File::create(rd.join(format!("agent-{agent_id}.jsonl")))?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    Ok(())
}

/// Write the completion summary (`workflows/wf_<run_id>.json`). "Summary wins" over the live dir,
/// so the run now shows as completed; the per-worker `agent-<id>.jsonl` transcripts stay in the live
/// dir and remain readable on click. Written via a temp file + rename so the reader never sees a
/// half-written summary.
pub fn write_summary(
    journal_dir: &Path,
    run_id: &str,
    name: &str,
    status: &str,
    summary: Option<&str>,
    created_at_ms: i64,
    phases: &[WorkflowPhase],
    agents: &[DelegateAgentResult],
) -> std::io::Result<()> {
    let wf_dir = journal_dir.join("workflows");
    std::fs::create_dir_all(&wf_dir)?;

    let progress: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            let wa = WorkflowAgent {
                agent_id: a.agent_id.clone(),
                label: a.label.clone(),
                state: if a.failed {
                    "failed".into()
                } else {
                    "done".into()
                },
                model: a.model.clone(),
                phase_title: a.phase_title.clone(),
                prompt_preview: a.prompt_preview.clone(),
                result_preview: a.result_preview.clone(),
            };
            // The reader keeps only items whose `type` == "workflow_agent"; WorkflowAgent does not
            // serialize a `type`, so inject it.
            let mut v = serde_json::to_value(&wa).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert("type".into(), serde_json::json!("workflow_agent"));
            }
            v
        })
        .collect();

    let doc = serde_json::json!({
        "runId": run_id,
        "workflowName": name,
        "status": status,
        "summary": summary,
        "agentCount": agents.len() as i64,
        "createdAt": created_at_ms,
        "phases": phases.iter()
            .map(|p| serde_json::json!({ "title": p.title, "detail": p.detail }))
            .collect::<Vec<_>>(),
        "workflowProgress": progress,
        "logs": [],
    });

    let final_path = wf_dir.join(format!("wf_{run_id}.json"));
    let tmp = wf_dir.join(format!(".wf_{run_id}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_string(&doc)?)?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(())
}

// ── worker fan-out orchestration ──────────────────────────────────────────────

/// One subtask the main Claude wants run on a cheap worker.
#[derive(Clone, Debug, Default)]
pub struct DelegateTask {
    pub prompt: String,
    pub role: String,
    /// Optional explicit model/provider hint (e.g. "deepseek-chat", "MiniMax-M3"); resolves the
    /// provider + endpoint. None → the default cheap provider.
    pub model: Option<String>,
    /// Optional phase label grouping this task with others under a phase header (like a native Workflow).
    pub phase: Option<String>,
    /// When true, run this worker in its OWN isolated git worktree (forked from the caller's current
    /// working state) so it can edit files without colliding with peer workers; its resulting diff is
    /// returned to the main session as a unified-diff patch instead of just a text summary.
    pub write: bool,
}

/// A worker resolved to a concrete process spec.
#[derive(Clone, Debug, Default)]
pub struct WorkerSpec {
    pub agent_id: String,
    pub label: String,
    pub model: String,
    /// Env overlay that points this worker at its provider (ANTHROPIC_BASE_URL/ANTHROPIC_AUTH_TOKEN).
    pub env_overlay: HashMap<String, String>,
    /// Per-worker working-dir override. Write workers run in their isolated worktree; `None` → the
    /// shared caller worktree passed to `run_workers` (the read-only default).
    pub cwd_override: Option<String>,
    pub prompt: String,
    /// Why the LLM router picked this model (None when pinned/heuristic) — shown in the run summary.
    pub route_reason: Option<String>,
    /// Phase this worker belongs to (groups workers under phase headers, like a native Workflow).
    pub phase_title: Option<String>,
}

/// A finished worker's distilled outcome (returned to the main Claude as the tool result).
#[derive(Clone, Debug, Default)]
pub struct WorkerSummary {
    pub agent_id: String,
    pub summary: String,
    pub failed: bool,
}

/// Max wall time for a single worker before it is force-stopped.
const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn content_text(content: Option<&serde_json::Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// Distil a worker's answer from its transcript jsonl (= its bridge log): the last non-empty
/// assistant text, falling back to a `result` line's text. Bounded to keep the summary small.
/// Returns (summary, had_error).
fn extract_summary(jsonl: &Path) -> (String, bool) {
    let Ok(file) = std::fs::File::open(jsonl) else {
        return (String::new(), true);
    };
    let mut last_assistant = String::new();
    let mut result_text = String::new();
    let mut had_error = false;
    // Stream line-by-line: a runaway worker could produce a huge log → OOM with read_to_string.
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match o.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let t = content_text(o.get("message").and_then(|m| m.get("content")));
                if !t.is_empty() {
                    last_assistant = t;
                }
            }
            Some("result") => {
                if o.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
                    had_error = true;
                }
                if let Some(r) = o.get("result").and_then(|r| r.as_str()) {
                    result_text = r.to_string();
                }
            }
            _ => {}
        }
    }
    let s = if !last_assistant.is_empty() {
        last_assistant
    } else {
        result_text
    };
    (s.chars().take(2000).collect(), had_error)
}

/// The concrete model a NATIVE Claude worker actually ran on (e.g. "claude-sonnet-4-5-…"), read from
/// its transcript — the system/init line and every assistant message carry a "model" field. Lets us
/// show the real tier instead of the generic "claude (native)". Streams line-by-line (large logs).
pub(crate) fn extract_model_from_transcript(jsonl: &Path) -> Option<String> {
    let file = std::fs::File::open(jsonl).ok()?;
    // The model is in the system/init line (first) or the first assistant message — cap the scan so a
    // huge/corrupt log without a "model" field can't make this walk the whole file.
    for line in BufReader::new(file).lines().map_while(Result::ok).take(100) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // system/init carries "model" at the top level; assistant messages carry message.model.
        let m = o.get("model").and_then(|v| v.as_str()).or_else(|| {
            o.get("message")
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
        });
        if let Some(m) = m.filter(|s| !s.is_empty()) {
            return Some(m.to_string());
        }
    }
    None
}

/// Run a set of cheap workers concurrently, transcribing each into the caller's workflow journal,
/// and return their distilled summaries. Each worker is a one-shot bridge spawn: write the prompt,
/// close stdin, run to completion. The worker's bridge log IS the `agent-<id>.jsonl` transcript the
/// workflow tab reads on click — so no separate transcription pass is needed.
pub async fn run_workers(
    runner: &dyn Runner,
    worktree: &str,
    journal_dir: &Path,
    run_id: &str,
    config_dir: &str,
    name: &str,
    phases: &[WorkflowPhase],
    workers: Vec<WorkerSpec>,
) -> std::io::Result<Vec<WorkerSummary>> {
    start_live_run(journal_dir, run_id, name, phases, &workers)?;
    let rd = run_dir(journal_dir, run_id);

    // Spawn + kick off every worker first so they run concurrently; collect their handles.
    let mut driving = Vec::new();
    for w in workers {
        let log_path = rd.join(format!("agent-{}.jsonl", w.agent_id));
        let mut overlay = w.env_overlay.clone();
        // Mark this as a worker: the bridge denies AskUserQuestion (headless, no user) AND does not
        // mount the `delegate` tool (so a worker cannot recursively fan out).
        overlay.insert("SDK_BRIDGE_WORKER".to_string(), "1".to_string());
        // Write workers run in their own isolated worktree (w.cwd_override); read workers share the
        // caller's worktree (the default), exactly as before.
        let cwd = w
            .cwd_override
            .clone()
            .unwrap_or_else(|| worktree.to_string());
        let opts = SpawnOptions {
            cwd,
            prompt: w.prompt.clone(),
            env: overlay,
            model: if w.model.is_empty() {
                None
            } else {
                Some(w.model.clone())
            },
            permission_mode: Some("bypassPermissions".to_string()),
            log_path: log_path.clone(),
            unit: format!("agentic-worker-{run_id}-{}", w.agent_id),
            // Native Claude workers (empty overlay) read the SUBSCRIPTION credentials from this config
            // dir; without it CLAUDE_CONFIG_DIR is set to "" and the worker reports "Not logged in".
            claude_config_dir: Some(config_dir.to_string()),
            ..Default::default()
        };
        let handle = spawn_claude(opts, runner);
        handle.write(&encode_user_message(&w.prompt));
        handle.end_input();
        driving.push((w.agent_id, handle, log_path));
    }

    // Await each worker (bounded), then distil its transcript. They started together, so the wall
    // time is the slowest worker, not the sum.
    let mut summaries = Vec::new();
    // One global deadline for the whole fan-out: the workers all started together above, so a
    // per-worker fresh timeout would let later workers run far past WORKER_TIMEOUT of wall time.
    let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;
    for (agent_id, mut handle, log_path) in driving {
        let exit_res = tokio::time::timeout_at(deadline, &mut handle.exit).await;
        let timed_out = exit_res.is_err();
        // A worker that exits non-zero without an explicit error line is still a failure.
        let exit_failed = match exit_res {
            Ok(code) => code.unwrap_or(1) != 0,
            Err(_) => false,
        };
        if timed_out {
            handle.kill();
        }
        let (summary, had_error) = extract_summary(&log_path);
        let failed = had_error || timed_out || exit_failed;
        let preview = if summary.is_empty() {
            if failed {
                "(failed)"
            } else {
                "(no output)"
            }
        } else {
            &summary
        };
        let _ = mark_agent_done(journal_dir, run_id, &agent_id, preview);
        summaries.push(WorkerSummary {
            agent_id,
            summary,
            failed,
        });
    }
    Ok(summaries)
}

/// Locate the caller session's on-disk journal dir (`<config_base>/projects/<slug>/<csid>/`) by
/// matching the claude session uuid, the same way `engine::workflows` scopes its scan.
pub(crate) fn resolve_journal_dir(config_base: &Path, csid: &str) -> Option<PathBuf> {
    // Defense-in-depth: csid comes from the session Init event; never let it escape projects/.
    if csid.contains('/') || csid.contains('\\') || csid.contains("..") {
        return None;
    }
    let projects = config_base.join("projects");
    for slug in std::fs::read_dir(&projects).ok()?.flatten() {
        let sp = slug.path();
        if sp.join(format!("{csid}.jsonl")).exists() || sp.join(csid).exists() {
            return Some(sp.join(csid));
        }
    }
    None
}

/// Boot-time recovery for delegate runs interrupted by a restart.
///
/// A delegate fan-out writes its completion summary (`workflows/wf_<run>.json`) only when
/// `run_delegate` finishes (`run_workers` returns). If the server dies mid-fan-out, that future is
/// dropped: the live `subagents/workflows/<run>/` dir survives with NO summary, so the workflow
/// reader (`read_running_run`) shows the run — and every still-going worker — as "running" forever.
/// `recover()` finalizes the SESSION but never touches these journals, so the phantom card lingers.
///
/// This scans one session's journal dir and, for every live run lacking a summary, writes a terminal
/// summary so the run shows as finished. Workers that already produced a result line stay "done"; the
/// rest are marked "failed" (interrupted). The run's status is "done" only if every worker finished,
/// else "failed". Returns the number of runs finalized.
///
/// MUST be called only at boot, when no fan-out is in flight — a live dir without a summary then
/// unambiguously means "interrupted", never "still legitimately running".
pub fn finalize_orphaned_runs(journal_dir: &Path, now_ms: i64) -> usize {
    let live_root = journal_dir.join("subagents").join("workflows");
    let Ok(entries) = std::fs::read_dir(&live_root) else {
        return 0;
    };
    let mut finalized = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let run_id = entry.file_name().to_string_lossy().into_owned();
        // Defense-in-depth: run_id flows into the summary file path below.
        if run_id.is_empty()
            || run_id.contains('/')
            || run_id.contains('\\')
            || run_id.contains("..")
        {
            continue;
        }
        // A completion summary already exists → the run finished (summary wins over the live dir).
        if journal_dir
            .join("workflows")
            .join(format!("wf_{run_id}.json"))
            .exists()
        {
            continue;
        }
        // Reconstruct the run exactly as the live workflow tab renders it (name, phases, per-worker
        // label/model/phase + which workers reached their result line).
        let Some(run) = read_running_run(journal_dir, &entry.path(), &run_id) else {
            continue; // empty/garbage run dir — the reader drops it, so do we
        };
        let agents: Vec<DelegateAgentResult> = run
            .agents
            .iter()
            .map(|a| {
                // A worker that never wrote a result line was cut off mid-run → failed.
                let failed = a.state != "done";
                // Native-fallback workers have an empty meta model; read the real tier from the
                // worker's transcript so the recovered row matches a normally-completed one.
                let model = if a.model.is_empty() {
                    extract_model_from_transcript(
                        &entry.path().join(format!("agent-{}.jsonl", a.agent_id)),
                    )
                    .unwrap_or_default()
                } else {
                    a.model.clone()
                };
                DelegateAgentResult {
                    agent_id: a.agent_id.clone(),
                    label: a.label.clone(),
                    model,
                    prompt_preview: a.prompt_preview.clone(),
                    result_preview: a.result_preview.clone(),
                    failed,
                    route_reason: None,
                    phase_title: a.phase_title.clone(),
                }
            })
            .collect();
        // "done" only if every worker had finished before the crash; otherwise the run was interrupted.
        let all_done = !agents.is_empty() && agents.iter().all(|a| !a.failed);
        let status = if all_done { "done" } else { "failed" };
        let summary = if all_done {
            "recovered after restart"
        } else {
            "interrupted by server restart"
        };
        if let Err(e) = write_summary(
            journal_dir,
            &run_id,
            &run.name,
            status,
            Some(summary),
            now_ms,
            &run.phases,
            &agents,
        ) {
            tracing::warn!("[engine] finalize orphaned delegate run {run_id}: {e}");
            continue;
        }
        finalized += 1;
    }
    finalized
}

/// Ask NATIVE Claude (the subscription main model) to make the per-task routing decision, used when
/// NO third-party provider is flagged as the router. We can't HTTP-call the subscription (no raw key),
/// so spawn a ONE-SHOT headless native Claude (empty overlay) with the route prompt, read its reply,
/// and parse it like a normal router reply (plus the deterministic priority layer). Returns an empty
/// map on any failure → the caller then runs the un-pinned tasks on native Claude directly. The spawn
/// uses a THROWAWAY temp transcript (not the run journal), so the routing call never shows as a workflow.
async fn route_via_native_claude(
    runner: &dyn Runner,
    worktree: &str,
    config_dir: &str,
    tasks: &[DelegateTask],
    candidates: &[&crate::engine::providers::Provider],
    t: f32,
) -> HashMap<usize, crate::engine::router::RouteChoice> {
    use crate::engine::router;
    let route_idxs: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.model.as_deref().unwrap_or("").trim().is_empty())
        .map(|(i, _)| i)
        .collect();
    // Nothing to route: no un-pinned tasks.
    if route_idxs.is_empty() {
        return HashMap::new();
    }
    // A single candidate means there is no routing DECISION to make — assign it to every un-pinned
    // task directly (mirrors router::route_batch's single-candidate handling). Without this, an
    // un-pinned task would get no pick and fall back to the subscription DEFAULT model — so the sole
    // candidate (e.g. a single-family native catalog collapsed to one newest model, or one registered
    // provider) and any override attached to it would never run. Returns before spawning the router.
    if candidates.len() == 1 {
        let only = candidates[0];
        return route_idxs
            .into_iter()
            .map(|i| {
                (
                    i,
                    crate::engine::router::RouteChoice {
                        model: only.model.clone(),
                        reason: "only candidate".into(),
                    },
                )
            })
            .collect();
    }
    let prompt = router::build_route_prompt(tasks, &route_idxs, candidates);
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let log_path =
        std::env::temp_dir().join(format!("agentic-route-{}-{uniq}.jsonl", std::process::id()));
    let mut overlay = HashMap::new();
    // Headless: deny AskUserQuestion + don't mount delegate (no nested fan-out from the router call).
    overlay.insert("SDK_BRIDGE_WORKER".to_string(), "1".to_string());
    let opts = SpawnOptions {
        cwd: worktree.to_string(),
        prompt: prompt.clone(),
        env: overlay,
        model: None, // subscription default model
        permission_mode: Some("bypassPermissions".to_string()),
        log_path: log_path.clone(),
        unit: "agentic-native-router".to_string(),
        claude_config_dir: Some(config_dir.to_string()),
        ..Default::default()
    };
    let mut handle = spawn_claude(opts, runner);
    handle.write(&encode_user_message(&prompt));
    handle.end_input();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    if tokio::time::timeout_at(deadline, &mut handle.exit)
        .await
        .is_err()
    {
        handle.kill();
        // Wait for the killed process to ACTUALLY exit before reading/removing its log (bounded, so a
        // wedged kill can't hang the fan-out). Otherwise we could read/delete the log mid-write.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle.exit).await;
    }
    let (text, _had_error) = extract_summary(&log_path);
    let _ = std::fs::remove_file(&log_path);
    if text.trim().is_empty() {
        return HashMap::new();
    }
    router::select_from_picks(
        router::parse_route_response(&text, &route_idxs, candidates),
        candidates,
        t,
    )
}

// ── write-mode worktree isolation ──────────────────────────────────────────────
//
// A WRITE worker must NOT share the caller's worktree — concurrent writers would clobber each other
// and the main session couldn't review what changed. So each write worker gets its OWN git worktree
// per repo, forked from a snapshot of the caller's CURRENT working state (tracked + untracked WIP).
// The worker edits freely in there; on completion we `git diff` it against that snapshot and hand the
// patch back to the main session, which stays the single writer (it reviews + `git apply`s itself).

/// One repo checkout provisioned for a write worker, plus what's needed to diff and tear it down.
/// `anchor` is the caller's own worktree for this repo — `git -C <anchor>` shares the repo's object/
/// ref store, so sibling worktrees are added/removed through it (no access to the origin clone needed).
struct WriteRepoWt {
    repo: String,
    anchor: PathBuf,
    worker_wt: PathBuf,
    base_sha: String,
    branch: String,
}

/// Everything provisioned for one write worker: its isolated session-style cwd plus the per-repo
/// checkouts inside it (mirroring the session's `<dir>/<repo>` layout so the worker sees a normal tree).
struct WriteProvision {
    worker_dir: PathBuf,
    repos: Vec<WriteRepoWt>,
}

/// Per-repo ref under which the batch's WIP snapshot commit is parked so it stays reachable for the
/// worktree forks (worktrees share their repo's ref store). Deleted in `cleanup_write_batch`.
fn snapshot_ref(run_id: &str) -> String {
    format!("refs/agentic/snapshots/deleg-{run_id}")
}

/// Snapshot every repo's CURRENT working state ONCE for the whole batch → `repo -> commit sha`. All
/// the batch's write workers fork from the same snapshot, so each worker's diff is a clean delta over
/// one shared base (and includes the orchestrator's uncommitted WIP, not just the last commit).
fn snapshot_session_repos(
    session_dir: &Path,
    repos: &[String],
    run_id: &str,
) -> Result<HashMap<String, String>, crate::engine::worktree::WorktreeError> {
    let sref = snapshot_ref(run_id);
    let mut bases = HashMap::new();
    for repo in repos {
        let sha = crate::engine::worktree::snapshot_worktree(&session_dir.join(repo), &sref)?;
        bases.insert(repo.clone(), sha);
    }
    Ok(bases)
}

/// Fork an isolated worktree per repo for ONE write worker, from the batch snapshot bases. The
/// worker's cwd is `<session>/.agentic-delegate/<run>/<agent>/`, holding a `<repo>/` checkout each.
fn provision_write_worktree(
    session_dir: &Path,
    repos: &[String],
    run_id: &str,
    agent_id: &str,
    bases: &HashMap<String, String>,
) -> Result<WriteProvision, crate::engine::worktree::WorktreeError> {
    use crate::engine::worktree::WorktreeError;
    let worker_dir = session_dir
        .join(".agentic-delegate")
        .join(run_id)
        .join(agent_id);
    std::fs::create_dir_all(&worker_dir)?;
    let mut out = Vec::new();
    for repo in repos {
        let anchor = session_dir.join(repo);
        let base = bases
            .get(repo)
            .ok_or_else(|| WorktreeError::Git(format!("no snapshot base for repo {repo}")))?;
        let worker_wt = worker_dir.join(repo);
        let branch = format!("agentic-deleg/{run_id}/{agent_id}/{repo}");
        let rp = anchor.to_string_lossy();
        let wp = worker_wt.to_string_lossy();
        crate::engine::worktree::git_sync(&[
            "-C", &rp, "worktree", "add", &wp, "-b", &branch, base,
        ])?;
        out.push(WriteRepoWt {
            repo: repo.clone(),
            anchor,
            worker_wt,
            base_sha: base.clone(),
            branch,
        });
    }
    Ok(WriteProvision {
        worker_dir,
        repos: out,
    })
}

/// Diff a write worker's isolated worktrees against their snapshot bases and format the combined
/// per-repo patch the main session applies. Empty string when the worker changed nothing.
async fn collect_write_patch(prov: &WriteProvision) -> String {
    let mut patch = String::new();
    for r in &prov.repos {
        match crate::engine::worktree::diff_worktree(&r.worker_wt, &r.base_sha).await {
            Ok(d) if !d.trim().is_empty() => {
                patch.push_str(&format!(
                    "\n===== BEGIN PATCH · repo `{0}` · review, then `git apply` inside the {0}/ subdir =====\n",
                    r.repo
                ));
                patch.push_str(&d);
                if !d.ends_with('\n') {
                    patch.push('\n');
                }
                patch.push_str(&format!("===== END PATCH · repo `{}` =====\n", r.repo));
            }
            Ok(_) => {}
            Err(e) => patch.push_str(&format!(
                "\n[delegate] could not diff repo `{}`: {e}\n",
                r.repo
            )),
        }
    }
    patch
}

/// Remove every write worker's isolated worktrees + their throwaway branches (best-effort).
fn teardown_write_worktrees(provs: &HashMap<String, WriteProvision>) {
    for prov in provs.values() {
        for r in &prov.repos {
            let _ = crate::engine::worktree::discard_worktree(&r.anchor, &r.worker_wt, &r.branch);
        }
    }
}

/// Delete the per-repo batch snapshot refs and the scratch `.agentic-delegate/<run_id>` dir.
fn cleanup_write_batch(session_dir: &Path, repos: &[String], run_id: &str) {
    let snap_id = format!("deleg-{run_id}");
    for repo in repos {
        crate::engine::worktree::delete_snapshot_refs(&session_dir.join(repo), &snap_id);
    }
    let _ = std::fs::remove_dir_all(session_dir.join(".agentic-delegate").join(run_id));
}

impl crate::engine::Engine {
    /// Run a `delegate` cheap-worker fan-out on behalf of `caller_id`: resolve the caller's worktree
    /// + journal dir, exempt the turn from the watchdog, run the workers, write the completion
    /// summary, and return each worker's distilled result. The workers appear in the caller
    /// session's workflow tab via the journal we write.
    pub(crate) async fn run_delegate(
        &self,
        caller_id: &str,
        run_id: &str,
        tasks: Vec<DelegateTask>,
        title: Option<String>,
    ) -> Result<Vec<WorkerSummary>, String> {
        // Defense-in-depth: run_id flows into file paths; never let it escape the journal dir.
        if run_id.is_empty()
            || run_id.contains('/')
            || run_id.contains('\\')
            || run_id.contains("..")
        {
            return Err("invalid run_id".to_string());
        }

        // Run title (the workflow card title) + phases (distinct non-empty task.phase labels, in order).
        let name = title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("delegate")
            .to_string();
        // Distinct phases in first-appearance order. Dedup is case-insensitive (keyed by lowercase),
        // but we store the canonical FIRST-appearance casing — and normalize each worker's phase_title
        // to it below — so "Explore"/"explore" collapse to one phase header AND one Android agent group.
        let mut canonical_phase: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut phases: Vec<WorkflowPhase> = Vec::new();
        for t in &tasks {
            if let Some(ph) = t.phase.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                if !canonical_phase.contains_key(&ph.to_lowercase()) {
                    canonical_phase.insert(ph.to_lowercase(), ph.to_string());
                    phases.push(WorkflowPhase {
                        title: ph.to_string(),
                        detail: None,
                    });
                }
            }
        }
        let s = self
            .0
            .store
            .get(caller_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no such session".to_string())?;
        let worktree = s
            .worktree_path
            .clone()
            .ok_or_else(|| "session has no worktree".to_string())?;
        // For write-mode isolation: the session dir holds one `<repo>/` worktree per repo; write
        // workers fork their own checkouts from these.
        let session_dir = std::path::PathBuf::from(&worktree);
        let repos = s.repos.clone();
        let has_write = tasks.iter().any(|t| t.write);
        let csid = s
            .claude_session_id
            .clone()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "session not yet initialized (no claudeSessionId)".to_string())?;
        let journal_dir = resolve_journal_dir(&self.0.cfg.claude_config_base, &csid)
            .ok_or_else(|| "could not locate caller journal dir".to_string())?;

        // Load the provider registry ONCE for the whole fan-out (not per task).
        let registry = crate::engine::providers::ProviderRegistry::load();

        // Exempt the session from the idle watchdog for the WHOLE fan-out, including the (possibly
        // slow) router HTTP call below — not just the worker run.
        self.mark_delegate_pending(caller_id);

        // ── provision isolated worktrees for any WRITE workers (forked from current WIP) ──
        // Up front so a setup failure aborts the batch cleanly before any worker spawns. There are no
        // `?`/early-returns between here and `run_workers`, so a provisioned batch always reaches its
        // teardown below.
        let mut write_provisions: HashMap<String, WriteProvision> = HashMap::new();
        if has_write {
            if repos.is_empty() {
                self.clear_delegate_pending(caller_id);
                return Err(
                    "delegate write mode needs a session with at least one repo".to_string()
                );
            }
            let bases = match snapshot_session_repos(&session_dir, &repos, run_id) {
                Ok(b) => b,
                Err(e) => {
                    cleanup_write_batch(&session_dir, &repos, run_id);
                    self.clear_delegate_pending(caller_id);
                    return Err(format!("delegate write-mode snapshot failed: {e}"));
                }
            };
            for (i, t) in tasks.iter().enumerate() {
                if !t.write {
                    continue;
                }
                let agent_id = format!("w{}", i + 1);
                match provision_write_worktree(&session_dir, &repos, run_id, &agent_id, &bases) {
                    Ok(p) => {
                        write_provisions.insert(agent_id, p);
                    }
                    Err(e) => {
                        teardown_write_worktrees(&write_provisions);
                        cleanup_write_batch(&session_dir, &repos, run_id);
                        self.clear_delegate_pending(caller_id);
                        return Err(format!("delegate write-mode worktree setup failed: {e}"));
                    }
                }
            }
        }

        // Candidate catalog the router chooses from = registered keyed anthropic providers PLUS the
        // built-in native Claude models (opus/sonnet/haiku). The Claude tiers ALWAYS compete, so even
        // with a single registered cheap model the router weighs it against Claude — a hard task can go
        // to a strong Claude model and an easy one to the cheap model. Native picks run on the
        // subscription (no overlay); registered picks run cheap via their endpoint overlay.
        // Native candidates live in a local owned Vec; registered providers stay as references (no
        // clone). `candidates` borrows both.
        let native_overrides = crate::engine::native_overrides::load_map();
        let native_candidates =
            crate::engine::providers::native_claude_candidates(&native_overrides);
        let candidates: Vec<&crate::engine::providers::Provider> = registry
            .providers
            .iter()
            .filter(|p| {
                // Disabled models never participate in routing (the Enabled toggle).
                p.enabled
                    // Anthropic providers run directly; openai providers run via the LiteLLM proxy,
                    // so only offer them as candidates when that proxy is actually available.
                    && !p.resolved_key().is_empty()
                    && (matches!(p.protocol, crate::engine::providers::Protocol::Anthropic)
                        || (matches!(p.protocol, crate::engine::providers::Protocol::Openai)
                            && crate::engine::litellm::available()))
            })
            .chain(native_candidates.iter().filter(|p| p.enabled))
            .collect();

        // Did ANY candidate exist BEFORE the enabled filter? This distinguishes two very different
        // empty-pool causes: (a) the user deliberately DISABLED everything — a real config error we
        // should surface, not paper over by silently running on the subscription default; versus
        // (b) there was nothing to route to at all — no registered providers AND native discovery
        // empty (the startup race before `init_claude_models` populates the OnceLock, or a permanent
        // auth/network discovery failure). Case (b) must keep the pre-feature behavior: fall through
        // to the subscription-default path, NOT error. (Erroring in (b) would break every fan-out for
        // a user who simply hasn't hit the Providers screen yet.)
        let existed_before_disable = !native_candidates.is_empty()
            || registry.providers.iter().any(|p| {
                !p.resolved_key().is_empty()
                    && (matches!(p.protocol, crate::engine::providers::Protocol::Anthropic)
                        || (matches!(p.protocol, crate::engine::providers::Protocol::Openai)
                            && crate::engine::litellm::available()))
            });
        if candidates.is_empty() && existed_before_disable {
            // Everything that existed was turned off. Fail BEFORE the router match (spec §11): the
            // native-router path has no empty guard and the worker-spec `None =>` arm would otherwise
            // route every task to the default model, lying to a user who disabled everything.
            teardown_write_worktrees(&write_provisions);
            cleanup_write_batch(&session_dir, &repos, run_id);
            self.clear_delegate_pending(caller_id);
            return Err(
                "all routing models are disabled — enable at least one provider or native tier"
                    .into(),
            );
        }

        // Pick the routing model: a flagged/keyed third-party router runs over HTTP; if NONE is flagged,
        // NATIVE Claude (the subscription) makes the decision via a one-shot headless call so tasks still
        // route across the catalog (incl. the registered models) instead of all running unrouted.
        // Global cost⇄quality tradeoff (0=cheapest .. 1=strongest) feeds the deterministic scorer.
        // Named `knob` (not `t`) so it isn't shadowed by the per-task closure param `t` below.
        let knob = crate::engine::routing_config::load().tradeoff;
        let picks = match crate::engine::router::router_provider(&registry) {
            Some(rp) => {
                crate::engine::router::route_batch(&tasks, &candidates, rp, knob, None).await
            }
            None => {
                let config_dir = self.0.cfg.claude_config_base.to_string_lossy().into_owned();
                route_via_native_claude(
                    self.0.runner.as_ref(),
                    &worktree,
                    &config_dir,
                    &tasks,
                    &candidates,
                    knob,
                )
                .await
            }
        };

        let workers: Vec<WorkerSpec> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let explicit = t.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
                // Resolve the explicit hint or the router's pick against the WHOLE catalog
                // (registered providers + native Claude tiers).
                // Exact-match-first (resolve_candidate) so a bare native tier name ("sonnet"/"opus"/
                // "haiku") binds to the native subscription candidate even when a registered provider's
                // model id (e.g. "claude-3-5-haiku-latest") substring-contains it and appears earlier.
                let resolved: Option<(&crate::engine::providers::Provider, Option<String>)> =
                    if let Some(m) = explicit {
                        crate::engine::providers::resolve_candidate(&candidates, m)
                            .map(|p| (p, None))
                    } else {
                        // The LLM pick, already run through select_model upstream. If routing
                        // failed / produced no pick for this un-pinned task, DEGRADE to a mid-floor
                        // (d=0.5) SCORED pick so it still respects the tradeoff/cost knob instead of
                        // silently running on the raw subscription default model.
                        let choice = picks.get(&i).cloned().unwrap_or_else(|| {
                            crate::engine::router::select_with_floor(0.5, &candidates, knob)
                        });
                        crate::engine::providers::resolve_candidate(&candidates, &choice.model)
                            .map(|p| (p, Some(choice.reason.clone())))
                    };
                let (model, overlay, route_reason) = match resolved {
                    // A native Claude candidate runs on the subscription: model override, NO overlay.
                    Some((p, reason)) if crate::engine::providers::is_native(p) => {
                        (p.model.clone(), HashMap::new(), reason)
                    }
                    // A registered provider runs cheap via its endpoint overlay.
                    Some((p, reason)) => (
                        p.model.clone(),
                        crate::engine::providers::env_overlay(p),
                        reason,
                    ),
                    // No catalog match (explicit unknown model, or router unavailable/failed) → native
                    // Claude: an explicit id becomes a native override, otherwise the default model.
                    None => (explicit.unwrap_or("").to_string(), HashMap::new(), None),
                };
                WorkerSpec {
                    agent_id: format!("w{}", i + 1),
                    label: format!(
                        "{} {}",
                        if t.role.is_empty() { "worker" } else { &t.role },
                        i + 1
                    ),
                    model,
                    env_overlay: overlay,
                    // Write workers run in their isolated worktree (provisioned above); read workers
                    // leave this None and share the caller worktree.
                    cwd_override: write_provisions
                        .get(&format!("w{}", i + 1))
                        .map(|p| p.worker_dir.to_string_lossy().into_owned()),
                    prompt: t.prompt.clone(),
                    route_reason,
                    phase_title: t
                        .phase
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|ph| {
                            canonical_phase
                                .get(&ph.to_lowercase())
                                .cloned()
                                .unwrap_or_else(|| ph.to_string())
                        }),
                }
            })
            .collect();

        // Remember each worker's model + label + routing reason by agent id BEFORE `workers` is moved
        // into run_workers, so the completion summary shows them in the workflow tab.
        let meta: HashMap<String, (String, String, Option<String>, Option<String>)> = workers
            .iter()
            .map(|w| {
                (
                    w.agent_id.clone(),
                    (
                        w.model.clone(),
                        w.label.clone(),
                        w.route_reason.clone(),
                        w.phase_title.clone(),
                    ),
                )
            })
            .collect();

        let config_dir = self.0.cfg.claude_config_base.to_string_lossy().into_owned();
        let mut result = run_workers(
            self.0.runner.as_ref(),
            &worktree,
            &journal_dir,
            run_id,
            &config_dir,
            &name,
            &phases,
            workers,
        )
        .await;

        // ── WRITE-mode post-processing ──
        // For each write worker, diff its isolated worktree against the snapshot base and append the
        // unified patch to its summary (the main session reviews + applies it). Then tear down ALL
        // isolated worktrees, branches, snapshot refs, and the scratch dir — always, even on failure.
        if !write_provisions.is_empty() {
            if let Ok(ref mut sums) = result {
                for sum in sums.iter_mut() {
                    if let Some(prov) = write_provisions.get(&sum.agent_id) {
                        let patch = collect_write_patch(prov).await;
                        sum.summary = if patch.trim().is_empty() {
                            format!(
                                "{}\n\n[delegate] write worker produced no file changes.",
                                sum.summary
                            )
                        } else {
                            format!("{}\n{}", sum.summary, patch)
                        };
                    }
                }
            }
            teardown_write_worktrees(&write_provisions);
            cleanup_write_batch(&session_dir, &repos, run_id);
        }

        if let Ok(ref sums) = result {
            let agents: Vec<DelegateAgentResult> = sums
                .iter()
                .map(|s| {
                    let (model, label, route_reason, phase_title) =
                        meta.get(&s.agent_id).cloned().unwrap_or_default();
                    DelegateAgentResult {
                        agent_id: s.agent_id.clone(),
                        label: if label.is_empty() {
                            s.agent_id.clone()
                        } else {
                            label
                        },
                        // An empty spawn model means the native Claude fallback ran the worker — read the
                        // ACTUAL tier from its transcript (e.g. "claude-sonnet-4-5-…") so the row shows a
                        // real model name, not a generic placeholder.
                        model: if model.is_empty() {
                            extract_model_from_transcript(
                                &run_dir(&journal_dir, run_id)
                                    .join(format!("agent-{}.jsonl", s.agent_id)),
                            )
                            .unwrap_or_else(|| "claude (native)".into())
                        } else {
                            model
                        },
                        prompt_preview: None,
                        result_preview: Some(s.summary.chars().take(200).collect()),
                        failed: s.failed,
                        route_reason,
                        phase_title,
                    }
                })
                .collect();
            // Surface the auto-routing decisions (which model each worker got + why) via the existing
            // `summary` field — no client change needed. Falls back to a generic line when nothing was
            // auto-routed (all pinned, or routing disabled/failed).
            let routed: Vec<String> = agents
                .iter()
                .filter_map(|a| {
                    a.route_reason.as_ref().map(|r| {
                        if r.is_empty() {
                            format!("{}→{}", a.agent_id, a.model)
                        } else {
                            format!("{}→{} ({})", a.agent_id, a.model, r)
                        }
                    })
                })
                .collect();
            let summary = if routed.is_empty() {
                "delegate fan-out".to_string()
            } else {
                format!("auto-routed · {}", routed.join(" · "))
            };
            // The run shows under its caller-supplied title (or "delegate"); per-worker model + phase on each row.
            if let Err(e) = write_summary(
                &journal_dir,
                run_id,
                &name,
                "done",
                Some(&summary),
                self.now(),
                &phases,
                &agents,
            ) {
                tracing::error!("[engine] failed to write delegate completion summary: {e}");
            }
        }
        self.clear_delegate_pending(caller_id);
        result.map_err(|e| e.to_string())
    }
}

// ── watchdog exemption hooks (used by the /api/sessions/:id/delegate route, follow-up slice) ──
impl crate::engine::Engine {
    /// Park the watchdog for `id` while a `delegate` fan-out is in flight: cheap workers can run for
    /// minutes with no events on the main session, which would otherwise trip the idle/wall cancel.
    /// Mirrors `pending_ask`; cleared by `clear_delegate_pending` when the call returns.
    pub(crate) fn mark_delegate_pending(&self, id: &str) {
        self.0.state.lock().pending_delegate.insert(id.to_string());
    }
    /// Lift the delegate watchdog exemption for `id` (call in a finally around the fan-out).
    pub(crate) fn clear_delegate_pending(&self, id: &str) {
        self.0.state.lock().pending_delegate.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::workflows::{list_workflows, read_workflow_agent};

    fn journal_base() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "agentic-deleg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let jd = base.join("projects").join("slug").join("uuid");
        std::fs::create_dir_all(&jd).unwrap();
        (base, jd)
    }

    #[test]
    fn live_then_summary_roundtrips_through_the_real_reader() {
        let (base, jd) = journal_base();
        let run = "wf_deleg1";

        // ── live phase: two workers, one finished ──
        let live_workers = vec![
            WorkerSpec {
                agent_id: "w1".into(),
                label: "explorer 1".into(),
                model: "MiniMax-M3".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore X".into(),
                route_reason: None,
                phase_title: Some("Explore".into()),
            },
            WorkerSpec {
                agent_id: "w2".into(),
                label: "explorer 2".into(),
                model: "deepseek-chat".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore Y".into(),
                route_reason: None,
                phase_title: Some("Explore".into()),
            },
        ];
        start_live_run(
            &jd,
            run,
            "search batch",
            &[WorkflowPhase {
                title: "Explore".into(),
                detail: None,
            }],
            &live_workers,
        )
        .unwrap();
        write_agent_transcript(
            &jd,
            run,
            "w1",
            &[
                serde_json::json!({"type":"user","message":{"content":"explore X"}}),
                serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"found 3 callsites"}]}}),
            ],
        )
        .unwrap();
        mark_agent_done(&jd, run, "w1", "found 3 callsites").unwrap();

        // reader synthesizes a RUNNING run (no summary yet), scoped to this session uuid
        let runs = list_workflows(&base, Some("uuid"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, run);
        assert_eq!(runs[0].status, "running");
        assert_eq!(runs[0].agent_count, Some(2));
        let w1 = runs[0].agents.iter().find(|a| a.agent_id == "w1").unwrap();
        assert_eq!(w1.state, "done");
        // P + N + O: the RUNNING run shows its real title, phases, and per-worker label/model/phase
        // (read from run.meta.json + agent-<id>.meta.json) — not the generic "workflow"/"agent N".
        assert_eq!(runs[0].name, "search batch");
        assert_eq!(runs[0].phases.len(), 1);
        assert_eq!(runs[0].phases[0].title, "Explore");
        assert_eq!(w1.label, "explorer 1");
        assert_eq!(w1.model, "MiniMax-M3");
        assert_eq!(w1.phase_title.as_deref(), Some("Explore"));
        // transcript readable while live
        let t = read_workflow_agent(&base, Some("uuid"), run, "w1");
        assert!(t.contains("explore X"), "task missing: {t}");
        assert!(t.contains("found 3 callsites"), "output missing: {t}");

        // ── completion: write the summary ──
        write_summary(
            &jd,
            run,
            "search batch",
            "done",
            Some("2 workers done"),
            1000,
            &[WorkflowPhase {
                title: "Explore".into(),
                detail: None,
            }],
            &[
                DelegateAgentResult {
                    agent_id: "w1".into(),
                    label: "explorer 1".into(),
                    model: "MiniMax-M3".into(),
                    prompt_preview: Some("explore X".into()),
                    result_preview: Some("found 3 callsites".into()),
                    failed: false,
                    route_reason: None,
                    phase_title: Some("Explore".into()),
                },
                DelegateAgentResult {
                    agent_id: "w2".into(),
                    label: "explorer 2".into(),
                    model: "deepseek-chat".into(),
                    prompt_preview: None,
                    result_preview: Some("ok".into()),
                    failed: false,
                    route_reason: None,
                    phase_title: Some("Explore".into()),
                },
            ],
        )
        .unwrap();

        // reader now shows DONE (summary wins over the live dir), with worker models preserved
        let runs = list_workflows(&base, Some("uuid"));
        assert_eq!(runs.len(), 1, "summary must dedup the live dir to one run");
        assert_eq!(runs[0].status, "done");
        assert_eq!(runs[0].summary.as_deref(), Some("2 workers done"));
        assert_eq!(runs[0].agents.len(), 2);
        assert_eq!(runs[0].name, "search batch");
        assert_eq!(runs[0].phases.len(), 1, "summary carries phases");
        let w1 = runs[0].agents.iter().find(|a| a.agent_id == "w1").unwrap();
        assert_eq!(w1.model, "MiniMax-M3");
        assert_eq!(w1.state, "done");
        assert_eq!(w1.phase_title.as_deref(), Some("Explore"));

        // per-worker transcript STILL readable after the summary was written
        let t = read_workflow_agent(&base, Some("uuid"), run, "w1");
        assert!(
            t.contains("found 3 callsites"),
            "transcript lost after summary: {t}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn finalize_orphaned_runs_writes_terminal_summary_for_interrupted_run() {
        use crate::engine::workflows::{is_workflow_terminal, list_workflows};
        let (base, jd) = journal_base();
        let run = "wfdeleg-123-1";

        // A live run: two workers started, but only w1 reached its result line before the crash.
        let workers = vec![
            WorkerSpec {
                agent_id: "w1".into(),
                label: "explorer 1".into(),
                model: "MiniMax-M3".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore X".into(),
                route_reason: None,
                phase_title: Some("Explore".into()),
            },
            WorkerSpec {
                agent_id: "w2".into(),
                label: "explorer 2".into(),
                model: "deepseek-chat".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore Y".into(),
                route_reason: None,
                phase_title: Some("Explore".into()),
            },
        ];
        start_live_run(
            &jd,
            run,
            "search batch",
            &[WorkflowPhase {
                title: "Explore".into(),
                detail: None,
            }],
            &workers,
        )
        .unwrap();
        mark_agent_done(&jd, run, "w1", "found 3 callsites").unwrap();

        // Before recovery the reader shows it RUNNING (live dir, no summary) — the phantom card.
        let pre = list_workflows(&base, Some("uuid"));
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].status, "running");
        assert!(!is_workflow_terminal(&pre[0].status));

        // Recover: one orphaned run finalized, a summary file is written.
        assert_eq!(
            finalize_orphaned_runs(&jd, 4242),
            1,
            "one orphaned run finalized"
        );
        assert!(
            jd.join("workflows").join(format!("wf_{run}.json")).exists(),
            "summary written"
        );

        // After recovery the summary WINS over the live dir → terminal "failed" (w2 never finished);
        // w1 stays done, w2 is marked failed, and the title/phases/timestamp are preserved.
        let post = list_workflows(&base, Some("uuid"));
        assert_eq!(post.len(), 1, "summary dedups the live dir to one run");
        assert_eq!(post[0].run_id, run);
        assert_eq!(post[0].status, "failed");
        assert!(is_workflow_terminal(&post[0].status));
        assert_eq!(post[0].name, "search batch");
        assert_eq!(post[0].phases.len(), 1);
        assert_eq!(post[0].created_at, 4242);
        let w1 = post[0].agents.iter().find(|a| a.agent_id == "w1").unwrap();
        assert_eq!(w1.state, "done");
        assert_eq!(w1.model, "MiniMax-M3");
        let w2 = post[0].agents.iter().find(|a| a.agent_id == "w2").unwrap();
        assert_eq!(w2.state, "failed");

        // Idempotent: a second pass sees the summary and does nothing.
        assert_eq!(
            finalize_orphaned_runs(&jd, 9999),
            0,
            "already-finalized run is skipped"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn finalize_marks_run_done_when_every_worker_finished() {
        use crate::engine::workflows::list_workflows;
        let (base, jd) = journal_base();
        let run = "wfdeleg-777-1";
        let workers = vec![WorkerSpec {
            agent_id: "w1".into(),
            label: "w1".into(),
            model: "m".into(),
            env_overlay: HashMap::new(),
            cwd_override: None,
            prompt: "a".into(),
            route_reason: None,
            phase_title: None,
        }];
        start_live_run(&jd, run, "batch", &[], &workers).unwrap();
        mark_agent_done(&jd, run, "w1", "ok").unwrap(); // the only worker finished before the crash

        assert_eq!(finalize_orphaned_runs(&jd, 1), 1);
        let runs = list_workflows(&base, Some("uuid"));
        assert_eq!(
            runs[0].status, "done",
            "all workers done → recovered as done, not failed"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_workers_drives_fake_bridge_and_writes_journal() {
        use crate::engine::sdk_runner::SdkRunner;
        let (base, jd) = journal_base();
        let worktree = base.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // Drive the worker bridges through the SAME runner production uses, pointed at the fake
        // bridge script (canned stream-json, no node, no API cost) — exactly like the engine tests.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-sdk-bridge-ok.sh");
        let runner = SdkRunner::with_node("bash", fixture.to_string_lossy().into_owned());
        let workers = vec![
            WorkerSpec {
                agent_id: "w1".into(),
                label: "explorer 1".into(),
                model: "MiniMax-M3".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore A".into(),
                route_reason: None,
                phase_title: None,
            },
            WorkerSpec {
                agent_id: "w2".into(),
                label: "explorer 2".into(),
                model: "MiniMax-M3".into(),
                env_overlay: HashMap::new(),
                cwd_override: None,
                prompt: "explore B".into(),
                route_reason: None,
                phase_title: None,
            },
        ];
        let sums = run_workers(
            &runner,
            worktree.to_str().unwrap(),
            &jd,
            "wf_d",
            "/tmp",
            "delegate",
            &[],
            workers,
        )
        .await
        .unwrap();
        assert_eq!(sums.len(), 2, "both workers returned a summary");
        // the fan-out shows up through the real workflow reader, scoped to this session uuid
        let runs = list_workflows(&base, Some("uuid"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "wf_d");
        assert_eq!(runs[0].agent_count, Some(2));
        std::fs::remove_dir_all(&base).ok();
    }

    /// End-to-end of the WRITE-mode worktree isolation helpers (no claude): a write worker forks an
    /// ISOLATED checkout from the session's CURRENT working state (uncommitted WIP included), its edits
    /// stay out of the session worktree, and `collect_write_patch` returns only the worker's delta over
    /// that WIP. Teardown removes the worktrees + scratch dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_mode_isolates_worktree_and_returns_patch() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed in {dir:?}");
        }
        let base = std::env::temp_dir().join(format!(
            "agentic-deleg-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // A repo with one commit.
        let origin = base.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "master"]);
        git(&origin, &["config", "user.email", "t@t"]);
        git(&origin, &["config", "user.name", "t"]);
        std::fs::write(origin.join("file.txt"), "line1\nline2\n").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-q", "-m", "init"]);

        // Session layout: `<session_dir>/<repo>` is a worktree of the repo (as a real session is).
        let repo = "myrepo";
        let session_dir = base.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let session_wt = session_dir.join(repo);
        git(
            &origin,
            &[
                "worktree",
                "add",
                session_wt.to_str().unwrap(),
                "-b",
                "agentic/sess",
            ],
        );
        // Orchestrator WIP: an UNCOMMITTED edit in the session worktree.
        std::fs::write(session_wt.join("file.txt"), "line1\nWIP\nline2\n").unwrap();

        let repos = vec![repo.to_string()];
        let run_id = "wfdeleg-test-1";

        // Snapshot the WIP, then fork an isolated worktree for write worker "w1".
        let bases = snapshot_session_repos(&session_dir, &repos, run_id).unwrap();
        let prov = provision_write_worktree(&session_dir, &repos, run_id, "w1", &bases).unwrap();
        let worker_wt = prov.repos[0].worker_wt.clone();

        // The worker STARTS from the WIP snapshot (sees the uncommitted "WIP" line).
        assert_eq!(
            std::fs::read_to_string(worker_wt.join("file.txt")).unwrap(),
            "line1\nWIP\nline2\n"
        );

        // Simulate the worker editing a file and adding a new one.
        std::fs::write(worker_wt.join("file.txt"), "line1\nWIP\nline2\nADDED\n").unwrap();
        std::fs::write(worker_wt.join("new.txt"), "brand new\n").unwrap();

        // The patch is the worker's delta over the WIP snapshot.
        let patch = collect_write_patch(&prov).await;
        assert!(
            patch.contains("BEGIN PATCH"),
            "patch must carry the per-repo header: {patch}"
        );
        assert!(
            patch.contains("+ADDED"),
            "patch must contain the worker's added line: {patch}"
        );
        assert!(
            patch.contains("new.txt"),
            "patch must contain the new file: {patch}"
        );
        assert!(
            !patch.contains("+WIP"),
            "WIP is in the base snapshot, not an added line: {patch}"
        );

        // Isolation: the worker's edits never touched the session worktree.
        assert_eq!(
            std::fs::read_to_string(session_wt.join("file.txt")).unwrap(),
            "line1\nWIP\nline2\n"
        );
        assert!(
            !session_wt.join("new.txt").exists(),
            "worker file must not leak into the session worktree"
        );

        // Teardown removes the worktrees + scratch dir.
        let mut provs = HashMap::new();
        provs.insert("w1".to_string(), prov);
        teardown_write_worktrees(&provs);
        cleanup_write_batch(&session_dir, &repos, run_id);
        assert!(!worker_wt.exists(), "worker worktree must be removed");
        assert!(
            !session_dir.join(".agentic-delegate").join(run_id).exists(),
            "scratch dir must be cleaned up"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
