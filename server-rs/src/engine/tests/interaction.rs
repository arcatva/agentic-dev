use super::*;

#[tokio::test]
async fn perm_event_sets_pending_perm_and_resolved_clears_it() {
    use crate::engine::stream::ClaudeEvent;
    use serde_json::json;
    let e = test_engine().await;
    let id = "perm-sess";
    e.0.store
        .create(crate::engine::store::CreateInput {
            id: id.into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // A parked perm marks the session pending_perm.
    e.on_event(
        id,
        ClaudeEvent::Perm {
            id: "perm-1".into(),
            tool: "Bash".into(),
            input: json!({}),
            raw: json!({}),
        },
    )
    .await;
    assert!(
        e.0.state.lock().pending_perm.contains(id),
        "perm parks the session"
    );
    // Resolving it clears the flag.
    e.on_event(
        id,
        ClaudeEvent::PermResolved {
            id: "perm-1".into(),
            decision: "allow".into(),
            raw: json!({}),
        },
    )
    .await;
    assert!(
        !e.0.state.lock().pending_perm.contains(id),
        "resolved clears pending_perm"
    );
}

#[tokio::test]
async fn respond_permission_clears_pending_perm_and_parked() {
    use crate::engine::stream::ClaudeEvent;
    use serde_json::json;
    let e = test_engine().await;
    let id = "perm-resp";
    e.0.store
        .create(crate::engine::store::CreateInput {
            id: id.into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // A parked perm arms the watchdog exemption + the pending_prompt payload.
    e.on_event(
        id,
        ClaudeEvent::Perm {
            id: "perm-1".into(),
            tool: "Bash".into(),
            input: json!({}),
            raw: json!({}),
        },
    )
    .await;
    assert!(
        e.0.state.lock().pending_perm.contains(id),
        "perm parks the session"
    );
    assert!(
        e.0.state.lock().parked.contains_key(id),
        "perm records the parked payload"
    );
    // respond_permission must clear both so the watchdog can reap once the turn proceeds, even when
    // the session has no live handle (forwarding is then a no-op — exactly this test's case).
    e.respond_permission(id, "allow", None);
    assert!(
        !e.0.state.lock().pending_perm.contains(id),
        "respond clears pending_perm"
    );
    assert!(
        !e.0.state.lock().parked.contains_key(id),
        "respond clears parked"
    );
}

#[tokio::test]
async fn ask_event_exposes_pending_prompt_and_resolves_clear_it() {
    use crate::engine::stream::ClaudeEvent;
    use serde_json::json;
    let e = test_engine().await;
    let id = "park-sess";
    e.0.store
        .create(crate::engine::store::CreateInput {
            id: id.into(),
            prompt: "p".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // An Ask event makes pending_prompt authoritative on the Session (kind "ask").
    e.on_event(
        id,
        ClaudeEvent::Ask {
            questions: vec![json!({"question":"A or B?"})],
            parent_tool_use_id: None,
            raw: json!({}),
        },
    )
    .await;
    let s = e.get(id).await.unwrap();
    assert_eq!(
        s.pending_prompt
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str()),
        Some("ask")
    );
    // A Result clears it.
    e.on_event(
        id,
        ClaudeEvent::Result {
            is_error: false,
            cost_usd: None,
            text: None,
            raw: json!({}),
        },
    )
    .await;
    assert!(e.get(id).await.unwrap().pending_prompt.is_none());
}
