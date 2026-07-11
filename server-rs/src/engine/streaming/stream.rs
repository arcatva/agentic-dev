use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct SpawnedAgent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeEvent {
    Init {
        session_id: String,
        raw: Value,
    },
    Prompt {
        text: String,
        at: i64,
        raw: Value,
    },
    Text {
        text: String,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    Skill {
        names: Vec<String>,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    Ask {
        questions: Vec<Value>,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    Agent {
        agents: Vec<SpawnedAgent>,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    Workflow {
        id: String,
        name: String,
        parent_tool_use_id: Option<String>,
        raw: Value,
        delegate: bool,
    },
    Thinking {
        text: String,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    Tool {
        name: String,
        input: Value,
        parent_tool_use_id: Option<String>,
        raw: Value,
    },
    AgentResult {
        tool_use_id: String,
        text: String,
        raw: Value,
    },
    Retry {
        attempt: u32,
        max_retries: u32,
        category: String,
        raw: Value,
    },
    Result {
        is_error: bool,
        cost_usd: Option<f64>,
        text: Option<String>,
        raw: Value,
    },
    Perm {
        id: String,
        tool: String,
        input: Value,
        raw: Value,
    },
    Plan {
        id: String,
        plan: String,
        raw: Value,
    },
    PermResolved {
        id: String,
        decision: String,
        raw: Value,
    },
    /// A pull request the session created. Engine-synthesized (NOT raw model output): detected from a
    /// `gh pr create` tool result, enriched with title/body/state fetched via `gh pr view`, persisted
    /// as a rendered `{"type":"pr",...}` marker so it replays on reconnect. The client renders one card.
    Pr {
        url: String,
        number: i64,
        repo: String,
        title: String,
        body: String,
        state: String,
        raw: Value,
    },
    /// In-band request from the main session's `delegate` MCP tool to fan out cheap workers. The
    /// engine runs `delegate::run_delegate` and replies via the bridge's stdin control channel.
    DelegateRequest {
        id: String,
        run_id: String,
        tasks: Vec<Value>,
        title: Option<String>,
        raw: Value,
    },
    /// Engine-synthesized link from a workflow card's tool_use id (`id`) to its run id (`run_id`), so
    /// the client opens the EXACT run a card maps to instead of guessing by name.
    WorkflowRun {
        id: String,
        run_id: String,
        raw: Value,
    },
    /// Engine-synthesized marker for a file delivered to the session outbox. The engine writes an
    /// `agentic_file` marker at the moment the file is first noticed, so it appears inline in the
    /// transcript at its real delivery position — no client-side heuristic guessing needed.
    File {
        path: String,
        at: i64,
        raw: Value,
    },
    Other {
        raw: Value,
    },
}

/// Flatten a tool_result's content (string, or array of text blocks) to plain text.
fn result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .map(|b| b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// A dynamic ("ultracode") workflow's real name lives only inside its inline script's
/// `export const meta = { name: '…' }`. Extract it so the UI chip shows the real name.
/// The script is multiline, so `.` must match newlines (`(?s)`).
fn meta_name(script: &Value) -> Option<String> {
    let s = script.as_str()?;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"meta\s*=\s*\{(?s:.*?)name\s*:\s*['"]([^'"]+)['"]"#).expect("valid regex")
    });
    re.captures(s).map(|c| c[1].to_string())
}

/// The `result` event's error/transcript text lives in a different field per result shape:
/// `result` (string), else `error` (string), else `errors` (string[] joined by "\n"; empty → None).
/// Extract the result/error text from a `result` event object.
fn result_text_field(obj: &Value) -> Option<String> {
    if let Some(s) = obj.get("result").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = obj.get("error").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = obj.get("errors").and_then(|v| v.as_array()) {
        let joined = arr
            .iter()
            .filter_map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() {
            return None;
        }
        return Some(joined);
    }
    None
}

/// PR web URLs that stand ALONE on a line of a tool result — the exact shape `gh pr create` prints on
/// success. Returns each distinct URL in order. This is intentionally specific to creation: `gh pr
/// view` (formatted, `url:\t…`), `gh pr list` (a table), `gh pr comment` (a `#issuecomment-…` suffix),
/// `gh pr merge` (`✓ Merged…`), and `gh api` (JSON) never print a bare PR-URL line, so they don't fire.
pub fn detect_created_pr_urls(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/\d+$")
            .expect("valid regex")
    });
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if re.is_match(t) && !out.iter().any(|u| u == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// "owner/repo" extracted from a GitHub PR web URL, or None if it doesn't match.
pub fn pr_repo_from_url(url: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/pull/\d+")
            .expect("valid regex")
    });
    re.captures(url).map(|c| c[1].to_string())
}

pub fn parse_line(line: &str) -> Vec<ClaudeEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let obj: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let parent: Option<String> = obj
        .get("parent_tool_use_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let ty = obj.get("type").and_then(|v| v.as_str());
    let subtype = obj.get("subtype").and_then(|v| v.as_str());

    // 1. system/init
    if ty == Some("system") && subtype == Some("init") {
        if let Some(session_id) = obj.get("session_id").and_then(|v| v.as_str()) {
            return vec![ClaudeEvent::Init {
                session_id: session_id.to_string(),
                raw: obj,
            }];
        }
    }
    // 2. agentic_prompt
    if ty == Some("agentic_prompt") {
        let text = obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // `at` is epoch-ms; accept a JSON integer OR a float-encoded number (serde_json's
        // as_i64() is None for floats); defaults to 0 when absent or non-numeric.
        let at = obj
            .get("at")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0);
        return vec![ClaudeEvent::Prompt { text, at, raw: obj }];
    }
    // 2b. agent_result — the engine-synthesized RENDERED marker carrying a genuine subagent's
    // returned text (written in mod.rs after a spawn's tool_result comes back). Parsing it here —
    // instead of letting it fall through to `Other` (kind:other, which the client ignores) — makes
    // the WS cursor deliver a proper `kind:agentResult` EXACTLY ONCE, identically live and on reopen.
    // The original non-rendered `user` tool_result line still yields an AgentResult event too, but
    // that one is no longer forwarded live (see `is_live_only`, which excludes AgentResult); the
    // rendered marker is the single delivery channel.
    if ty == Some("agent_result") {
        let tool_use_id = obj
            .get("toolUseId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let text = obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return vec![ClaudeEvent::AgentResult {
            tool_use_id,
            text,
            raw: obj,
        }];
    }
    // 2c. agentic_perm / agentic_perm_resolved — the bridge's perm/plan approval markers.
    if ty == Some("agentic_perm") {
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if obj.get("permKind").and_then(|v| v.as_str()) == Some("plan") {
            let plan = obj
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return vec![ClaudeEvent::Plan { id, plan, raw: obj }];
        }
        let tool = obj
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("tool")
            .to_string();
        let input = obj
            .get("input")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        return vec![ClaudeEvent::Perm {
            id,
            tool,
            input,
            raw: obj,
        }];
    }
    if ty == Some("agentic_perm_resolved") {
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let decision = obj
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("deny")
            .to_string();
        return vec![ClaudeEvent::PermResolved {
            id,
            decision,
            raw: obj,
        }];
    }
    // 2c-bis. pr — the engine-synthesized PR-card marker (written after `gh pr create` + `gh pr view`).
    // Decoding it here (instead of letting it fall to `Other`) makes the WS cursor deliver a proper
    // `kind:pr` frame exactly once, identically live and on reopen. Detection (which writes it) lives in
    // mod.rs; this only DECODES the marker, so re-tailing it never re-triggers a fetch (no feedback loop).
    if ty == Some("pr") {
        let url = obj
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let number = obj.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
        let repo = obj
            .get("repo")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = obj
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state = obj
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("OPEN")
            .to_string();
        return vec![ClaudeEvent::Pr {
            url,
            number,
            repo,
            title,
            body,
            state,
            raw: obj,
        }];
    }
    // 2c-bis. agentic_file — engine-synthesized marker for a file delivered to the session outbox.
    // The engine writes this at the moment the file is first noticed (in with_activity), so the file
    // card appears inline in the transcript at its real delivery position.
    if ty == Some("agentic_file") {
        let path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let at = obj
            .get("at")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0);
        return vec![ClaudeEvent::File { path, at, raw: obj }];
    }
    // 2c-ter. workflowRun — the engine-synthesized marker linking a workflow card's tool_use id to its
    // run id (written in mod.rs once the run id is known). Decoding it here (instead of letting it fall
    // to `Other`) makes the WS cursor deliver a `kind:workflowRun` frame exactly once, live and on
    // reopen. This only DECODES the marker, so re-tailing it never re-triggers detection (no loop).
    if ty == Some("workflowRun") {
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let run_id = obj
            .get("runId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return vec![ClaudeEvent::WorkflowRun {
            id,
            run_id,
            raw: obj,
        }];
    }
    // 2d. agentic_delegate_request — the bridge's `delegate` tool asking the engine to fan out cheap
    // workers. The engine answers in-band via the bridge stdin control line {"__bridge":"delegate"}.
    if ty == Some("agentic_delegate_request") {
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let run_id = obj
            .get("runId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tasks = obj
            .get("tasks")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return vec![ClaudeEvent::DelegateRequest {
            id,
            run_id,
            tasks,
            title,
            raw: obj,
        }];
    }
    // 3. system/api_retry
    if ty == Some("system") && subtype == Some("api_retry") {
        let attempt = obj.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let max_retries = obj.get("max_retries").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let category = obj
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        return vec![ClaudeEvent::Retry {
            attempt,
            max_retries,
            category,
            raw: obj,
        }];
    }
    // 4 & 5. stream_event deltas
    if ty == Some("stream_event") {
        let delta_type = obj.pointer("/event/delta/type").and_then(|v| v.as_str());
        if delta_type == Some("text_delta") {
            let text = obj
                .pointer("/event/delta/text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return vec![ClaudeEvent::Text {
                text,
                parent_tool_use_id: parent,
                raw: obj,
            }];
        }
        if delta_type == Some("thinking_delta") {
            let text = obj
                .pointer("/event/delta/thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return vec![ClaudeEvent::Thinking {
                text,
                parent_tool_use_id: parent,
                raw: obj,
            }];
        }
    }
    // 6. result
    if ty == Some("result") {
        let cost_usd = obj.get("total_cost_usd").and_then(|v| v.as_f64());
        let text = result_text_field(&obj);
        let is_error = obj
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return vec![ClaudeEvent::Result {
            is_error,
            cost_usd,
            text,
            raw: obj,
        }];
    }
    // 7. user message — subagent input (text+parent) and results (tool_result)
    if ty == Some("user") {
        if let Some(content) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
            let mut out = Vec::new();
            for b in content {
                let bt = b.get("type").and_then(|v| v.as_str());
                if bt == Some("text") && parent.is_some() {
                    let text = b
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(ClaudeEvent::Text {
                        text,
                        parent_tool_use_id: parent.clone(),
                        raw: obj.clone(),
                    });
                } else if bt == Some("tool_result") {
                    if let Some(tool_use_id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                        let text = result_text(b.get("content").unwrap_or(&Value::Null));
                        out.push(ClaudeEvent::AgentResult {
                            tool_use_id: tool_use_id.to_string(),
                            text,
                            raw: obj.clone(),
                        });
                    }
                }
            }
            return out;
        }
    }
    // 8. assistant message — full tool_use inputs (Skill/Agent/Workflow/Ask + ordinary tools)
    if ty == Some("assistant") {
        if let Some(content) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
            let mut out = Vec::new();
            // tool_use blocks only
            let tools: Vec<&Value> = content
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                .collect();

            // Prose + reasoning blocks. With includePartialMessages OFF (the bridge's default) the
            // model's text/thinking arrives ONLY in this final assistant message — there are no
            // stream_event deltas — so emit one event per COMPLETE block here (per-message
            // granularity, not per-token). Emitted before the tool events below so narration renders
            // ahead of the tool chips it precedes. (If partials are ever re-enabled these would
            // double the streamed text_delta/thinking_delta events; the bridge keeps partials off
            // precisely to avoid that fragmentation.)
            for b in content {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("thinking") => {
                        if let Some(text) = b
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .or_else(|| b.get("text").and_then(|v| v.as_str()))
                            .filter(|s| !s.is_empty())
                        {
                            out.push(ClaudeEvent::Thinking {
                                text: text.to_string(),
                                parent_tool_use_id: parent.clone(),
                                raw: obj.clone(),
                            });
                        }
                    }
                    Some("text") => {
                        if let Some(text) = b
                            .get("text")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            out.push(ClaudeEvent::Text {
                                text: text.to_string(),
                                parent_tool_use_id: parent.clone(),
                                raw: obj.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }

            // Skill names
            let names: Vec<String> = tools
                .iter()
                .filter(|b| b.get("name").and_then(|v| v.as_str()) == Some("Skill"))
                .filter_map(|b| {
                    b.pointer("/input/skill")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect();
            if !names.is_empty() {
                out.push(ClaudeEvent::Skill {
                    names,
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }

            // Spawned subagents (Agent or Task with input)
            let agents: Vec<SpawnedAgent> = tools
                .iter()
                .filter(|b| {
                    let n = b.get("name").and_then(|v| v.as_str());
                    (n == Some("Agent") || n == Some("Task")) && b.get("input").is_some()
                })
                .map(|b| SpawnedAgent {
                    id: b
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    agent_type: b
                        .pointer("/input/subagent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent")
                        .to_string(),
                    description: b
                        .pointer("/input/description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect();
            if !agents.is_empty() {
                out.push(ClaudeEvent::Agent {
                    agents,
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }

            // Workflows (+ the in-process `delegate` fan-out tool) — one card per block, in order.
            // The delegate tool renders as a workflow card (not a generic tool chip), matching the
            // native Workflow tool; its workers show in the same session's workflow tab.
            for w in tools.iter().filter(|b| {
                let n = b.get("name").and_then(|v| v.as_str());
                n == Some("Workflow") || n == Some("mcp__agentic__delegate")
            }) {
                let is_delegate =
                    w.get("name").and_then(|v| v.as_str()) == Some("mcp__agentic__delegate");
                let wf_name = if is_delegate {
                    // The caller-supplied delegate title (== the run name), like the native Workflow
                    // tool — falling back to "delegate" only when no title was passed.
                    w.pointer("/input/title")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| "delegate".to_string())
                } else {
                    w.pointer("/input/name")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .or_else(|| {
                            w.pointer("/input/title")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .or_else(|| meta_name(w.pointer("/input/script").unwrap_or(&Value::Null)))
                        .unwrap_or_else(|| "workflow".to_string())
                };
                out.push(ClaudeEvent::Workflow {
                    id: w
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: wf_name,
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                    delegate: is_delegate,
                });
            }

            // AskUserQuestion — first match only, questions must be an array
            if let Some(ask) = tools.iter().find(|b| {
                b.get("name").and_then(|v| v.as_str()) == Some("AskUserQuestion")
                    && b.pointer("/input/questions")
                        .map(|q| q.is_array())
                        .unwrap_or(false)
            }) {
                let questions = ask
                    .pointer("/input/questions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                out.push(ClaudeEvent::Ask {
                    questions,
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }

            // Every other tool call (Read/Edit/Bash/Write/…)
            const SPECIAL: [&str; 6] = [
                "Skill",
                "Agent",
                "Task",
                "Workflow",
                "AskUserQuestion",
                "mcp__agentic__delegate",
            ];
            for t in tools.iter().filter(|b| {
                let n = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                !SPECIAL.contains(&n)
            }) {
                out.push(ClaudeEvent::Tool {
                    name: t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string(),
                    input: t
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Default::default())),
                    parent_tool_use_id: parent.clone(),
                    raw: obj.clone(),
                });
            }
            return out;
        }
    }

    vec![ClaudeEvent::Other { raw: obj }]
}

/// Extract a native `Workflow` tool's run id (`wf_…`) from its tool-result text. The Workflow tool
/// reports a `runId` like `wf_ab12cd` in its result; we link the card to that run. Delegate run ids
/// (`wfdeleg-…`) deliberately don't match — there's no underscore after `wf` — so this fires only for
/// native Workflow results. Returns the first match, or None (caller then leaves the card unlinked and
/// the client falls back to name matching).
pub fn parse_workflow_run_id(text: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"wf_[A-Za-z0-9-]{4,}").expect("valid regex"));
    re.find(text).map(|m| m.as_str().to_string())
}

impl ClaudeEvent {
    /// Serialize to the kind-tagged, camelCase wire JSON the Android client consumes.
    /// `parentToolUseId`/`costUsd` are emitted as JSON `null` when absent.
    pub fn to_wire(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            ClaudeEvent::Init { session_id, raw } => {
                json!({ "kind": "init", "sessionId": session_id, "raw": raw })
            }
            ClaudeEvent::Prompt { text, at, raw } => {
                json!({ "kind": "prompt", "text": text, "at": at, "raw": raw })
            }
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                raw,
            } => {
                json!({ "kind": "text", "text": text, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Thinking {
                text,
                parent_tool_use_id,
                raw,
            } => {
                json!({ "kind": "thinking", "text": text, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Skill {
                names,
                parent_tool_use_id,
                raw,
            } => {
                json!({ "kind": "skill", "names": names, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Ask {
                questions,
                parent_tool_use_id,
                raw,
            } => {
                json!({ "kind": "ask", "questions": questions, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Agent {
                agents,
                parent_tool_use_id,
                raw,
            } => {
                let agents: Vec<serde_json::Value> = agents
                    .iter()
                    .map(|a| {
                        json!({
                            "id": a.id, "agentType": a.agent_type, "description": a.description,
                        })
                    })
                    .collect();
                json!({ "kind": "agent", "agents": agents, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::Workflow {
                id,
                name,
                parent_tool_use_id,
                raw,
                delegate,
            } => {
                json!({ "kind": "workflow", "id": id, "name": name, "parentToolUseId": parent_tool_use_id, "raw": raw, "delegate": delegate })
            }
            ClaudeEvent::Tool {
                name,
                input,
                parent_tool_use_id,
                raw,
            } => {
                json!({ "kind": "tool", "name": name, "input": input, "parentToolUseId": parent_tool_use_id, "raw": raw })
            }
            ClaudeEvent::AgentResult {
                tool_use_id,
                text,
                raw,
            } => {
                json!({ "kind": "agentResult", "toolUseId": tool_use_id, "text": text, "raw": raw })
            }
            ClaudeEvent::Retry {
                attempt,
                max_retries,
                category,
                raw,
            } => {
                json!({ "kind": "retry", "attempt": attempt, "maxRetries": max_retries, "category": category, "raw": raw })
            }
            ClaudeEvent::Result {
                is_error,
                cost_usd,
                text,
                raw,
            } => {
                json!({ "kind": "result", "isError": is_error, "costUsd": cost_usd, "text": text, "raw": raw })
            }
            ClaudeEvent::Perm {
                id,
                tool,
                input,
                raw,
            } => json!({ "kind": "perm", "id": id, "tool": tool, "input": input, "raw": raw }),
            ClaudeEvent::Plan { id, plan, raw } => {
                json!({ "kind": "plan", "id": id, "plan": plan, "raw": raw })
            }
            ClaudeEvent::PermResolved { id, decision, raw } => {
                json!({ "kind": "permResolved", "id": id, "decision": decision, "raw": raw })
            }
            ClaudeEvent::Pr {
                url,
                number,
                repo,
                title,
                body,
                state,
                raw,
            } => {
                json!({ "kind": "pr", "url": url, "number": number, "repo": repo, "title": title, "body": body, "state": state, "raw": raw })
            }
            ClaudeEvent::DelegateRequest {
                id,
                run_id,
                tasks,
                title,
                raw,
            } => {
                json!({ "kind": "delegateRequest", "id": id, "runId": run_id, "tasks": tasks, "title": title, "raw": raw })
            }
            ClaudeEvent::WorkflowRun { id, run_id, raw } => {
                json!({ "kind": "workflowRun", "id": id, "runId": run_id, "raw": raw })
            }
            ClaudeEvent::File { path, at, raw } => {
                json!({ "kind": "file", "path": path, "at": at, "raw": raw })
            }
            ClaudeEvent::Other { raw } => json!({ "kind": "other", "raw": raw }),
        }
    }
}

#[cfg(test)]
mod tests;
