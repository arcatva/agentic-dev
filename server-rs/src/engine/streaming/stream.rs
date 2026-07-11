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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_wire_emits_kind_tagged_camelcase() {
        use serde_json::json;
        // text with no parent → parentToolUseId must be JSON null (present, not omitted).
        let ev = ClaudeEvent::Text {
            text: "hi".into(),
            parent_tool_use_id: None,
            raw: json!({"type":"x"}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"text","text":"hi","parentToolUseId":null,"raw":{"type":"x"}})
        );

        // text with a parent → camelCase parentToolUseId carries the id.
        let ev = ClaudeEvent::Text {
            text: "yo".into(),
            parent_tool_use_id: Some("t1".into()),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"text","text":"yo","parentToolUseId":"t1","raw":{}})
        );

        // init → sessionId.
        let ev = ClaudeEvent::Init {
            session_id: "sess-1".into(),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"init","sessionId":"sess-1","raw":{}})
        );

        // prompt → text + at.
        let ev = ClaudeEvent::Prompt {
            text: "go".into(),
            at: 42,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"prompt","text":"go","at":42,"raw":{}})
        );

        // result with a cost → isError + costUsd. Absent cost → costUsd null.
        let ev = ClaudeEvent::Result {
            is_error: false,
            cost_usd: Some(0.0042),
            text: Some("ok".into()),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"result","isError":false,"costUsd":0.0042,"text":"ok","raw":{}})
        );
        let ev = ClaudeEvent::Result {
            is_error: true,
            cost_usd: None,
            text: None,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"result","isError":true,"costUsd":null,"text":null,"raw":{}})
        );

        // agentResult → toolUseId.
        let ev = ClaudeEvent::AgentResult {
            tool_use_id: "tu".into(),
            text: "done".into(),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"agentResult","toolUseId":"tu","text":"done","raw":{}})
        );

        // retry → attempt + maxRetries + category.
        let ev = ClaudeEvent::Retry {
            attempt: 1,
            max_retries: 3,
            category: "overloaded".into(),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"retry","attempt":1,"maxRetries":3,"category":"overloaded","raw":{}})
        );

        // agent → agents array of {id, agentType, description}.
        let ev = ClaudeEvent::Agent {
            agents: vec![SpawnedAgent {
                id: "a1".into(),
                agent_type: "coder".into(),
                description: "d".into(),
            }],
            parent_tool_use_id: Some("p".into()),
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({
                "kind":"agent",
                "agents":[{"id":"a1","agentType":"coder","description":"d"}],
                "parentToolUseId":"p","raw":{}
            })
        );

        // workflow / skill / ask / tool / thinking / other shapes.
        let ev = ClaudeEvent::Workflow {
            id: "w".into(),
            name: "n".into(),
            parent_tool_use_id: None,
            raw: json!({}),
            delegate: false,
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"workflow","id":"w","name":"n","parentToolUseId":null,"raw":{},"delegate":false})
        );
        let evd = ClaudeEvent::Workflow {
            id: "w".into(),
            name: "n".into(),
            parent_tool_use_id: None,
            raw: json!({}),
            delegate: true,
        };
        assert_eq!(evd.to_wire()["delegate"], json!(true));
        let ev = ClaudeEvent::Skill {
            names: vec!["s1".into()],
            parent_tool_use_id: None,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"skill","names":["s1"],"parentToolUseId":null,"raw":{}})
        );
        let ev = ClaudeEvent::Ask {
            questions: vec![json!({"q":1})],
            parent_tool_use_id: None,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"ask","questions":[{"q":1}],"parentToolUseId":null,"raw":{}})
        );
        let ev = ClaudeEvent::Tool {
            name: "bash".into(),
            input: json!({"cmd":"ls"}),
            parent_tool_use_id: None,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"tool","name":"bash","input":{"cmd":"ls"},"parentToolUseId":null,"raw":{}})
        );
        let ev = ClaudeEvent::Thinking {
            text: "hmm".into(),
            parent_tool_use_id: None,
            raw: json!({}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"thinking","text":"hmm","parentToolUseId":null,"raw":{}})
        );
        let ev = ClaudeEvent::Other {
            raw: json!({"engineExit":{"code":null,"status":"done"}}),
        };
        assert_eq!(
            ev.to_wire(),
            json!({"kind":"other","raw":{"engineExit":{"code":null,"status":"done"}}})
        );
    }

    #[test]
    fn blank_or_non_json_lines_yield_no_events() {
        assert_eq!(parse_line(""), vec![]);
        assert_eq!(parse_line("   "), vec![]);
        assert_eq!(parse_line("not json"), vec![]);
    }

    #[test]
    fn parses_init_and_extracts_session_id() {
        let line =
            json!({ "type": "system", "subtype": "init", "session_id": "abc123" }).to_string();
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ClaudeEvent::Init { session_id, .. } => assert_eq!(session_id, "abc123"),
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn parses_text_delta_with_parent_tool_use_id() {
        let line = json!({
            "type": "stream_event",
            "parent_tool_use_id": "toolu_1",
            "event": { "delta": { "type": "text_delta", "text": "hello" } }
        })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(text, "hello");
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_from_main_agent_has_none_parent() {
        let line = json!({
            "type": "stream_event",
            "event": { "delta": { "type": "text_delta", "text": "hi" } }
        })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(text, "hi");
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parses_api_retry() {
        let line = json!({ "type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 5, "error": "overloaded" })
            .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Retry {
                attempt,
                max_retries,
                category,
                ..
            } => {
                assert_eq!((*attempt, *max_retries), (2, 5));
                assert_eq!(category, "overloaded");
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn parses_result_with_cost() {
        let line = json!({ "type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.0123 })
            .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result {
                is_error, cost_usd, ..
            } => {
                assert!(!(*is_error));
                assert_eq!(*cost_usd, Some(0.0123));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn captures_error_result_from_errors_array() {
        let line = json!({ "type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["You've hit your session limit · resets 3:30pm"] })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result { is_error, text, .. } => {
                assert!(*is_error);
                assert_eq!(
                    text.as_deref(),
                    Some("You've hit your session limit · resets 3:30pm")
                );
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn reads_legacy_error_result_text_field() {
        let line = json!({ "type": "result", "subtype": "error_during_execution", "is_error": true,
            "result": "You've hit your session limit · resets 3:30pm" })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Result { is_error, text, .. } => {
                assert!(*is_error);
                assert_eq!(
                    text.as_deref(),
                    Some("You've hit your session limit · resets 3:30pm")
                );
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parses_agentic_prompt_marker() {
        let line = json!({ "type": "agentic_prompt", "text": "do x" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Prompt { text, at, .. } => {
                assert_eq!(text, "do x");
                assert_eq!(*at, 0);
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn parses_agent_result_marker() {
        // The engine-synthesized rendered marker must parse back to AgentResult (not fall through to
        // Other), so the WS cursor delivers it as kind:agentResult exactly once — live AND on reopen.
        let line =
            json!({ "type": "agent_result", "toolUseId": "toolu_42", "text": "subagent said hi" })
                .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::AgentResult {
                tool_use_id, text, ..
            } => {
                assert_eq!(tool_use_id, "toolu_42");
                assert_eq!(text, "subagent said hi");
            }
            other => panic!("expected AgentResult, got {other:?}"),
        }
        // And its wire shape is the kind:agentResult the Android client attaches to the agent card.
        assert_eq!(parse_line(&line)[0].to_wire()["kind"], "agentResult");
    }

    #[test]
    fn emits_thinking_for_thinking_delta() {
        let line = json!({ "type": "stream_event", "event": { "delta": { "type": "thinking_delta", "thinking": "hmm" } } })
            .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Thinking { text, .. } => assert_eq!(text, "hmm"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn user_text_block_with_parent_is_text_event() {
        let line = json!({ "type": "user", "parent_tool_use_id": "toolu_A",
            "message": { "content": [{ "type": "text", "text": "your job: reply pong" }] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(text, "your job: reply pong");
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_A"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn user_tool_result_block_is_agent_result() {
        let line = json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "toolu_A", "content": [{ "type": "text", "text": "pong" }] }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::AgentResult {
                tool_use_id, text, ..
            } => {
                assert_eq!(tool_use_id, "toolu_A");
                assert_eq!(text, "pong");
            }
            other => panic!("expected AgentResult, got {other:?}"),
        }
    }

    #[test]
    fn assistant_with_no_content_is_other() {
        let line = json!({ "type": "assistant" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Other { .. } => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn extracts_skill_names_then_ordinary_tool_in_order() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "thinking" },
            { "type": "tool_use", "name": "Skill", "input": { "skill": "superpowers:writing-plans" } },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ] } })
        .to_string();
        let evs = parse_line(&line);
        // Leading prose block is now surfaced as a Text event (partials are off), then Skill, then Bash.
        assert_eq!(evs.len(), 3);
        match &evs[0] {
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(text, "thinking");
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Text first, got {other:?}"),
        }
        match &evs[1] {
            ClaudeEvent::Skill {
                names,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(names, &vec!["superpowers:writing-plans".to_string()]);
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Skill second, got {other:?}"),
        }
        match &evs[2] {
            ClaudeEvent::Tool { name, .. } => assert_eq!(name, "Bash"),
            other => panic!("expected Tool third, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_ordinary_tool_call_with_input() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Tool { name, input, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls"));
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn extracts_ask_user_question() {
        let questions = json!([{ "question": "A or B?", "header": "Pick",
            "options": [{ "label": "A", "description": "" }], "multiSelect": false }]);
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "AskUserQuestion", "input": { "questions": questions } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Ask { questions: q, .. } => {
                assert_eq!(q.len(), 1);
                assert_eq!(
                    q[0].get("question").and_then(|v| v.as_str()),
                    Some("A or B?")
                );
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn extracts_spawned_subagent_with_type_and_description() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_A", "name": "Agent",
              "input": { "subagent_type": "Explore", "description": "search repos" } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Agent {
                agents,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(
                    agents[0],
                    SpawnedAgent {
                        id: "toolu_A".into(),
                        agent_type: "Explore".into(),
                        description: "search repos".into(),
                    }
                );
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn extracts_workflow_tool_use_by_name() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_W", "name": "Workflow", "input": { "name": "review-changes" } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "toolu_W");
                assert_eq!(name, "review-changes");
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn yields_one_agent_event_with_multiple_subagents() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "a1", "name": "Agent", "input": { "subagent_type": "Explore", "description": "x" } },
            { "type": "tool_use", "id": "a2", "name": "Agent", "input": { "subagent_type": "Plan", "description": "y" } }
        ] } })
        .to_string();
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ClaudeEvent::Agent { agents, .. } => assert_eq!(agents.len(), 2),
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn extracts_dynamic_workflow_name_from_inline_script_meta() {
        let script = "export const meta = {\n  name: 'fix-bugs',\n  description: 'x',\n}\n// body";
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_w", "name": "Workflow", "input": { "script": script } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "toolu_w");
                assert_eq!(name, "fix-bugs");
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn agent_default_type_is_agent_when_subagent_type_missing() {
        // Parity guard: SpawnedAgent.agent_type default is the literal "agent" (not "Task").
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "t1", "name": "Task", "input": { "description": "d" } }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Agent { agents, .. } => {
                assert_eq!(agents[0].agent_type, "agent");
                assert_eq!(agents[0].id, "t1");
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_and_control_lines_become_single_other_event() {
        // A control_request line (SDK control protocol — parser knows nothing about it).
        let req = json!({
            "type": "control_request",
            "request_id": "req_1",
            "request": { "subtype": "interrupt" }
        })
        .to_string();
        let evs = parse_line(&req);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ClaudeEvent::Other { raw } => {
                assert_eq!(
                    raw.get("type").and_then(|v| v.as_str()),
                    Some("control_request")
                );
                // raw is passed through verbatim, and to_wire stays stable.
                assert_eq!(evs[0].to_wire(), json!({ "kind": "other", "raw": raw }));
            }
            other => panic!("expected Other, got {other:?}"),
        }

        // A control_response line.
        let resp = json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": "req_1" }
        })
        .to_string();
        match &parse_line(&resp)[0] {
            ClaudeEvent::Other { .. } => {}
            other => panic!("expected Other, got {other:?}"),
        }

        // A brand-new/unknown type the parser has never heard of.
        let novel = json!({ "type": "quantum_flux", "payload": { "a": [1, 2, 3] } }).to_string();
        let evs = parse_line(&novel);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ClaudeEvent::Other { .. }));

        // JSON with no "type" key at all is still valid JSON → Other (not a panic, not empty).
        let no_type = json!({ "hello": "world" }).to_string();
        assert!(matches!(parse_line(&no_type)[0], ClaudeEvent::Other { .. }));
    }

    #[test]
    fn tool_use_blocks_with_missing_fields_use_defaults_and_dont_panic() {
        // Ordinary tool block with NO name and NO input.
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use" }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Tool {
                name,
                input,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(name, "tool");
                assert_eq!(*input, json!({}));
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Tool, got {other:?}"),
        }

        // Agent block with no id and no subagent_type → defaults id "" / type "agent".
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "Agent", "input": {} }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Agent { agents, .. } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, "");
                assert_eq!(agents[0].agent_type, "agent");
                assert_eq!(agents[0].description, "");
            }
            other => panic!("expected Agent, got {other:?}"),
        }

        // Workflow with no name/title/script and no id → name "workflow", id "".
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "Workflow", "input": {} }
        ] } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "");
                assert_eq!(name, "workflow");
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn multi_block_assistant_emits_events_in_array_order_per_category() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "let me work" },
            { "type": "tool_use", "name": "Skill", "input": { "skill": "superpowers:writing-plans" } },
            { "type": "tool_use", "id": "ag1", "name": "Agent", "input": { "subagent_type": "Explore", "description": "d1" } },
            { "type": "tool_use", "id": "wf1", "name": "Workflow", "input": { "name": "first" } },
            { "type": "tool_use", "id": "ag2", "name": "Agent", "input": { "subagent_type": "Plan", "description": "d2" } },
            { "type": "tool_use", "name": "Read", "input": { "file_path": "/a" } },
            { "type": "tool_use", "name": "AskUserQuestion", "input": { "questions": [{ "question": "q?" }] } },
            { "type": "tool_use", "id": "wf2", "name": "Workflow", "input": { "title": "second" } },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ] } })
        .to_string();
        let evs = parse_line(&line);
        // 1 Text + 1 Skill + 1 Agent(2 subagents) + 2 Workflow + 1 Ask + 2 Tool = 8 events.
        assert_eq!(evs.len(), 8);

        // Leading prose first, then category order: Text, Skill, Agent, Workflow(s), Ask, Tool(s).
        match &evs[0] {
            ClaudeEvent::Text { text, .. } => assert_eq!(text, "let me work"),
            other => panic!("expected Text, got {other:?}"),
        }
        match &evs[1] {
            ClaudeEvent::Skill { names, .. } => {
                assert_eq!(names, &vec!["superpowers:writing-plans".to_string()])
            }
            other => panic!("expected Skill, got {other:?}"),
        }
        match &evs[2] {
            ClaudeEvent::Agent { agents, .. } => {
                assert_eq!(agents.len(), 2);
                assert_eq!(agents[0].id, "ag1");
                assert_eq!(agents[1].id, "ag2");
            }
            other => panic!("expected Agent, got {other:?}"),
        }
        // Workflows appear in source-array order: "first" (name) before "second" (title).
        match (&evs[3], &evs[4]) {
            (
                ClaudeEvent::Workflow {
                    name: n1, id: id1, ..
                },
                ClaudeEvent::Workflow {
                    name: n2, id: id2, ..
                },
            ) => {
                assert_eq!((n1.as_str(), id1.as_str()), ("first", "wf1"));
                assert_eq!((n2.as_str(), id2.as_str()), ("second", "wf2"));
            }
            other => panic!("expected two Workflow events, got {other:?}"),
        }
        match &evs[5] {
            ClaudeEvent::Ask { questions, .. } => assert_eq!(questions.len(), 1),
            other => panic!("expected Ask, got {other:?}"),
        }
        // Ordinary tools last, in array order: Read then Bash.
        match (&evs[6], &evs[7]) {
            (ClaudeEvent::Tool { name: t1, .. }, ClaudeEvent::Tool { name: t2, .. }) => {
                assert_eq!((t1.as_str(), t2.as_str()), ("Read", "Bash"));
            }
            other => panic!("expected two Tool events, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_with_non_array_questions_yields_no_ask() {
        // questions is a string, not an array → no Ask, and no other tool blocks → empty vec.
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "AskUserQuestion", "input": { "questions": "oops not an array" } }
        ] } })
        .to_string();
        let evs = parse_line(&line);
        assert!(
            evs.is_empty(),
            "non-array questions must not yield an Ask, got {evs:?}"
        );

        // questions is an object → still skipped.
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "AskUserQuestion", "input": { "questions": { "q": 1 } } }
        ] } })
        .to_string();
        assert!(parse_line(&line).is_empty());

        // Missing input entirely → also no Ask, no panic.
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "AskUserQuestion" }
        ] } })
        .to_string();
        assert!(parse_line(&line).is_empty());
    }

    #[test]
    fn final_assistant_text_and_thinking_are_surfaced() {
        // A streamed delta (when partials are ON) still yields one Text event with the chunk.
        let partial = json!({
            "type": "stream_event",
            "event": { "delta": { "type": "text_delta", "text": "Hel" } }
        })
        .to_string();
        match &parse_line(&partial)[0] {
            ClaudeEvent::Text {
                text,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(text, "Hel");
                assert_eq!(*parent_tool_use_id, None);
            }
            other => panic!("expected Text, got {other:?}"),
        }

        // With partials OFF (the bridge default) the model's prose + reasoning arrive only in the
        // final assistant message, so the parser surfaces them as one Text / Thinking event per
        // complete block (in array order) — otherwise the live transcript would show tool chips but
        // no narration. Regression guard for the includePartialMessages=false streaming path.
        let final_msg = json!({ "type": "assistant", "message": { "content": [
            { "type": "thinking", "thinking": "let me reason" },
            { "type": "text", "text": "Hello world" }
        ] } })
        .to_string();
        let evs = parse_line(&final_msg);
        assert_eq!(
            evs.len(),
            2,
            "thinking + text must each surface, got {evs:?}"
        );
        match &evs[0] {
            ClaudeEvent::Thinking { text, .. } => assert_eq!(text, "let me reason"),
            other => panic!("expected Thinking first, got {other:?}"),
        }
        match &evs[1] {
            ClaudeEvent::Text { text, .. } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text second, got {other:?}"),
        }
    }

    #[test]
    fn parses_agentic_perm_perm_and_plan_and_resolved() {
        // perm (a tool awaiting approval)
        let line = json!({ "type": "agentic_perm", "permKind": "perm", "id": "perm-1",
            "tool": "Bash", "input": { "command": "rm -rf x" } })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Perm {
                id, tool, input, ..
            } => {
                assert_eq!(id, "perm-1");
                assert_eq!(tool, "Bash");
                assert_eq!(
                    input.get("command").and_then(|v| v.as_str()),
                    Some("rm -rf x")
                );
            }
            other => panic!("expected Perm, got {other:?}"),
        }
        assert_eq!(parse_line(&line)[0].to_wire()["kind"], "perm");

        // plan (ExitPlanMode awaiting approval)
        let line =
            json!({ "type": "agentic_perm", "permKind": "plan", "id": "perm-2", "plan": "do X" })
                .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Plan { id, plan, .. } => {
                assert_eq!(id, "perm-2");
                assert_eq!(plan, "do X");
            }
            other => panic!("expected Plan, got {other:?}"),
        }
        assert_eq!(parse_line(&line)[0].to_wire()["kind"], "plan");

        // resolved
        let line = json!({ "type": "agentic_perm_resolved", "id": "perm-1", "decision": "allow" })
            .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::PermResolved { id, decision, .. } => {
                assert_eq!(id, "perm-1");
                assert_eq!(decision, "allow");
            }
            other => panic!("expected PermResolved, got {other:?}"),
        }
        let w = parse_line(&line)[0].to_wire();
        assert_eq!(w["kind"], "permResolved");
        assert_eq!(w["decision"], "allow");
    }

    #[test]
    fn parses_pr_marker_and_wires_kind_pr() {
        let line = json!({ "type": "pr", "url": "https://github.com/arcatva/agentic-dev/pull/26",
            "number": 26, "repo": "arcatva/agentic-dev", "title": "Add PR cards",
            "body": "Backend-driven\n\n- detail", "state": "OPEN" })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::Pr {
                url,
                number,
                repo,
                title,
                body,
                state,
                ..
            } => {
                assert_eq!(url, "https://github.com/arcatva/agentic-dev/pull/26");
                assert_eq!(*number, 26);
                assert_eq!(repo, "arcatva/agentic-dev");
                assert_eq!(title, "Add PR cards");
                assert_eq!(body, "Backend-driven\n\n- detail");
                assert_eq!(state, "OPEN");
            }
            other => panic!("expected Pr, got {other:?}"),
        }
        let w = parse_line(&line)[0].to_wire();
        assert_eq!(w["kind"], "pr");
        assert_eq!(w["number"], 26);
        assert_eq!(w["repo"], "arcatva/agentic-dev");
        assert_eq!(w["title"], "Add PR cards");
    }

    #[test]
    fn detect_created_pr_urls_matches_only_bare_url_lines() {
        // gh pr create: a bare URL alone on its line → detected.
        assert_eq!(
            detect_created_pr_urls("https://github.com/arcatva/agentic-dev/pull/26\n"),
            vec!["https://github.com/arcatva/agentic-dev/pull/26"],
        );
        // Two distinct creations in one result → both, in order, deduped.
        assert_eq!(
            detect_created_pr_urls(
                "https://github.com/arcatva/a/pull/1\nhttps://github.com/arcatva/a/pull/1\nhttps://github.com/arcatva/b/pull/2"
            ),
            vec!["https://github.com/arcatva/a/pull/1", "https://github.com/arcatva/b/pull/2"],
        );
        // Not a bare-URL line: inline prose, the `url:` view label, the `#issuecomment` comment suffix,
        // the `/pulls/` REST path, and an issue link must all NOT fire.
        assert!(
            detect_created_pr_urls("Opened https://github.com/arcatva/a/pull/1 for review")
                .is_empty()
        );
        assert!(detect_created_pr_urls("url:\thttps://github.com/arcatva/a/pull/1").is_empty());
        assert!(
            detect_created_pr_urls("https://github.com/arcatva/a/pull/1#issuecomment-9").is_empty()
        );
        assert!(
            detect_created_pr_urls("https://api.github.com/repos/arcatva/a/pulls/1").is_empty()
        );
        assert!(detect_created_pr_urls("https://github.com/arcatva/a/issues/1").is_empty());
    }

    #[test]
    fn pr_repo_from_url_extracts_owner_repo() {
        assert_eq!(
            pr_repo_from_url("https://github.com/arcatva/agentic-dev/pull/26").as_deref(),
            Some("arcatva/agentic-dev"),
        );
        assert_eq!(
            pr_repo_from_url("https://github.com/BerriAI/litellm/pull/14821").as_deref(),
            Some("BerriAI/litellm")
        );
        assert_eq!(pr_repo_from_url("not a url"), None);
    }

    #[test]
    fn parses_agentic_delegate_request() {
        let line = json!({ "type": "agentic_delegate_request", "id": "deleg-1", "runId": "wf_d1",
            "tasks": [{ "prompt": "explore X", "role": "explorer" }] })
        .to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::DelegateRequest {
                id, run_id, tasks, ..
            } => {
                assert_eq!(id, "deleg-1");
                assert_eq!(run_id, "wf_d1");
                assert_eq!(tasks.len(), 1);
                assert_eq!(
                    tasks[0].get("prompt").and_then(|v| v.as_str()),
                    Some("explore X")
                );
            }
            other => panic!("expected DelegateRequest, got {other:?}"),
        }
        assert_eq!(parse_line(&line)[0].to_wire()["kind"], "delegateRequest");
    }

    #[test]
    fn parses_workflow_run_marker_roundtrip() {
        let line =
            json!({ "type": "workflowRun", "id": "toolu_W", "runId": "wf_abc123" }).to_string();
        match &parse_line(&line)[0] {
            ClaudeEvent::WorkflowRun { id, run_id, .. } => {
                assert_eq!(id, "toolu_W");
                assert_eq!(run_id, "wf_abc123");
            }
            other => panic!("expected WorkflowRun, got {other:?}"),
        }
        let wire = parse_line(&line)[0].to_wire();
        assert_eq!(wire["kind"], "workflowRun");
        assert_eq!(wire["id"], "toolu_W");
        assert_eq!(wire["runId"], "wf_abc123");
    }

    #[test]
    fn parse_workflow_run_id_extracts_native_wf_id_not_delegate() {
        // Native Workflow result text mentions its run id.
        assert_eq!(
            parse_workflow_run_id("Workflow started. runId: wf_abc123 — see /workflows"),
            Some("wf_abc123".to_string())
        );
        // Hyphenated ids are captured whole.
        assert_eq!(
            parse_workflow_run_id("runId wf_a1b2-c3d4 path=/x/y-wf_a1b2-c3d4.js"),
            Some("wf_a1b2-c3d4".to_string())
        );
        // Delegate run ids (`wfdeleg-…`, no underscore after `wf`) must NOT match.
        assert_eq!(parse_workflow_run_id("runId: wfdeleg-12345-7"), None);
        // No run id present.
        assert_eq!(parse_workflow_run_id("just some summary text"), None);
    }

    #[test]
    fn delegate_tool_use_renders_as_a_workflow_card() {
        let line = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "toolu_d", "name": "mcp__agentic__delegate",
              "input": { "tasks": [{"prompt":"a"},{"prompt":"b"},{"prompt":"c"}] } }
        ] } })
        .to_string();
        // delegate is surfaced as a Workflow card (not a generic Tool chip), like the native Workflow tool.
        let evs = parse_line(&line);
        assert_eq!(
            evs.len(),
            1,
            "delegate yields one workflow card, not also a tool chip"
        );
        match &evs[0] {
            ClaudeEvent::Workflow { id, name, .. } => {
                assert_eq!(id, "toolu_d");
                assert_eq!(name, "delegate");
            }
            other => panic!("expected Workflow card for delegate, got {other:?}"),
        }
        assert_eq!(evs[0].to_wire()["kind"], "workflow");
    }

    #[test]
    fn unicode_and_huge_text_roundtrip_without_panic() {
        // ~50k chars of mixed multibyte content: emoji, CJK, combining marks, a control char.
        let chunk = "héllo 🌍 世界 \u{0301}\u{200d} \t";
        let big: String = chunk.repeat(2_000);
        let line = json!({
            "type": "stream_event",
            "event": { "delta": { "type": "text_delta", "text": big } }
        })
        .to_string();
        let evs = parse_line(&line);
        match &evs[0] {
            ClaudeEvent::Text { text, .. } => {
                assert_eq!(text.as_str(), big.as_str());
                // to_wire must preserve the exact unicode payload.
                let wire = evs[0].to_wire();
                assert_eq!(
                    wire.get("text").and_then(|v| v.as_str()),
                    Some(big.as_str())
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }

        // Garbage with surrounding whitespace / embedded NUL must never panic; trims to non-JSON.
        assert_eq!(parse_line("\n\t  {bad json \0 \n"), vec![]);
        assert_eq!(parse_line("   \u{feff}   "), vec![]); // BOM + spaces, no JSON
                                                          // A bare JSON scalar (valid JSON, not an object) → has no "type" → Other, no panic.
        assert!(matches!(parse_line("42")[0], ClaudeEvent::Other { .. }));
        assert!(matches!(
            parse_line("\"just a string\"")[0],
            ClaudeEvent::Other { .. }
        ));
    }
}
