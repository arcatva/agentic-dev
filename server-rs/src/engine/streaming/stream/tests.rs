use super::*;
use serde_json::json;

#[test]
fn to_wire_emits_kind_tagged_camelcase() {
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
    let line = json!({ "type": "system", "subtype": "init", "session_id": "abc123" }).to_string();
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
    let line =
        json!({ "type": "agentic_perm_resolved", "id": "perm-1", "decision": "allow" }).to_string();
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
        detect_created_pr_urls("Opened https://github.com/arcatva/a/pull/1 for review").is_empty()
    );
    assert!(detect_created_pr_urls("url:\thttps://github.com/arcatva/a/pull/1").is_empty());
    assert!(
        detect_created_pr_urls("https://github.com/arcatva/a/pull/1#issuecomment-9").is_empty()
    );
    assert!(detect_created_pr_urls("https://api.github.com/repos/arcatva/a/pulls/1").is_empty());
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
    let line = json!({ "type": "workflowRun", "id": "toolu_W", "runId": "wf_abc123" }).to_string();
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
