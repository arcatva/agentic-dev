/// A stream/connection error caused by the CLIENT going away (app closed/backgrounded or a
/// network blip mid-stream), not a server fault — downgraded to debug so a genuine 500 still
/// stands out. Matches on the code/message text a tungstenite/axum send error surfaces.
pub fn is_client_disconnect(code: Option<&str>, msg: Option<&str>) -> bool {
    matches!(
        code,
        Some("ERR_STREAM_PREMATURE_CLOSE") | Some("ECONNRESET") | Some("EPIPE")
    ) || msg == Some("Premature close")
}

use crate::api::state::AppState;
use crate::engine::stream::parse_line;
use crate::engine::transcript::{filter_rendered, is_stream_event};
use crate::util::now_secs;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::Response;
use futures_util::stream::SplitSink;
use serde::Deserialize;

/// Outbound (send) half of the split WebSocket.
type WsSink = SplitSink<WebSocket, Message>;

/// What the live subscriber callback hands to the WS loop.
enum WsMsg {
    /// New rendered data (or a status change) landed — wake the loop to re-read the cursor.
    Poke,
    /// A live-only frame (agentResult / retry / init) that the rendered cursor never delivers —
    /// send it through as-is. Carries the event's wire JSON.
    Live(serde_json::Value),
}

/// True for events produced ONLY from non-rendered source lines (`system/init`, `system/api_retry`).
/// The rendered cursor never carries these, so they must be forwarded live or the app never sees API
/// retries / the session-id init. Forwarding exactly these introduces NO duplicate with the cursor
/// (their source lines aren't rendered).
///
/// NOTE: `AgentResult` is deliberately NOT here. A genuine subagent result is persisted as a RENDERED
/// `agent_result` marker (engine/mod.rs) and now parses back to a `kind:agentResult` frame via the
/// cursor (engine/stream.rs), so the cursor is its single delivery channel — live AND on reopen.
/// Forwarding it live too (as it was before — a bug from when the marker became rendered) double-
/// delivered it. The transient `user` tool_result line that also yields an AgentResult is a Poke only.
pub(crate) fn is_live_only(ev: &crate::engine::stream::ClaudeEvent) -> bool {
    use crate::engine::stream::ClaudeEvent::{Init, Retry};
    matches!(ev, Init { .. } | Retry { .. })
}

#[derive(Deserialize, Default)]
pub struct StreamQuery {
    pub token: Option<String>,
    pub since: Option<String>,
}

/// RAII guard that runs the engine unsubscribe closure on drop, so the subscription is removed on
/// EVERY exit of [handle_socket] — normal return, an error path, OR future cancellation (the socket
/// task being dropped at an `.await`). A manual `unsub()` call is skipped on cancellation, which would
/// leak the subscription in `engine.state.subs`; the guard cannot be.
struct UnsubGuard(Option<Box<dyn FnOnce() + Send>>);
impl Drop for UnsubGuard {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

pub async fn stream_session(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Response {
    // The shared auth_gate already accepts ?token=; this is the WS-specific re-check so we can
    // close with the exact 1008 code the client expects on a bad/expired token.
    let token = q.token.unwrap_or_default();
    let now = now_secs();
    let authed = crate::api::auth::verify_token(&st.config.auth_secret, &token, now);
    ws.on_upgrade(move |socket| handle_socket(socket, st, id, q.since, authed))
}

async fn handle_socket(
    socket: WebSocket,
    st: AppState,
    id: String,
    since_raw: Option<String>,
    authed: bool,
) {
    use futures_util::{SinkExt, StreamExt};
    // Split so we can poll the INBOUND half concurrently with sending. Polling inbound is what lets
    // the WS layer answer the client's Ping with a Pong; without it OkHttp/Ktor tears the socket
    // down on its keepalive timeout (~20s) and the app reconnects in a loop.
    let (mut sink, mut stream) = socket.split();

    if !authed {
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1008,
                reason: "unauthorized".into(),
            })))
            .await;
        return;
    }
    if st.engine.get(&id).await.is_none() {
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1008,
                reason: "not found".into(),
            })))
            .await;
        return;
    }

    // since = max(0, parseInt || 0). Absent → 0.
    let since: usize = match since_raw {
        Some(s) => s.trim().parse::<i64>().unwrap_or(0).max(0) as usize,
        None => 0,
    };

    // ── Cursor-based, gap-free AND dup-free live protocol ────────────────────────────────────
    // The bytes we send are ALWAYS rendered lines read by a MONOTONIC cursor over the
    // TranscriptCache projection — never re-forwarded event payloads. The subscriber is used
    // only as a "new data" POKE: it tells the loop when to re-read the projection.
    //
    // Why this is gap-free: we subscribe BEFORE the backfill read, so any line appended after
    // the backfill snapshot triggers a poke; the steady-state loop then reads everything at
    // [cursor..], which always includes lines appended after the snapshot.
    //
    // Why this is dup-free: `cursor` only moves forward and we only ever send lines at index
    // >= the number already sent. A line that lands in the window between "subscribe" and
    // "backfill read" is in the snapshot exactly once and is at index < cursor afterward, so
    // the live loop never re-sends it. (The old code forwarded buffered event payloads on top
    // of the snapshot, double-delivering exactly those boundary lines.)
    //
    // Turn end is detected by RE-READING session status in the loop (not by forwarding a
    // synthetic engineExit from the callback): when status becomes done|failed|killed we flush
    // any remaining [cursor..] lines, synthesize the engineExit frame once, and close.
    // ────────────────────────────────────────────────────────────────────────────────────────

    let log_path = st.store.log_path(&id);

    // 1. Subscribe FIRST (before the backfill read). The callback forwards live-only frames
    //    (agentResult/retry/init — never carried by the rendered cursor) and pokes the loop on
    //    every other event so it re-reads the cursor. Each event sends exactly one message, so the
    //    loop processes pokes (rendered flush) and live frames in emission order.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsMsg>();
    // Held for the lifetime of the socket; its Drop unsubscribes on every exit (incl. cancellation).
    let _unsub = UnsubGuard(Some(st.engine.subscribe(
        &id,
        Box::new(move |ev| {
            let _ = if is_live_only(ev) {
                tx.send(WsMsg::Live(ev.to_wire()))
            } else {
                tx.send(WsMsg::Poke)
            };
        }),
    )));

    // Read rendered lines [from..) from the projection, returning (lines, total_count). On a
    // projection read error, fall back to the whole-file filtered read (still cursor-correct).
    async fn read_from(
        st: &AppState,
        id: &str,
        log_path: &std::path::Path,
        from: usize,
    ) -> (Vec<String>, usize) {
        match st
            .transcript
            .with(id, log_path, |p| (p.slice_from(from).to_vec(), p.count()))
            .await
        {
            Ok(out) => out,
            Err(_) => {
                let lines = filter_rendered(&st.engine.get_log(id));
                let total = lines.len();
                let slice = lines.into_iter().skip(from).collect();
                (slice, total)
            }
        }
    }

    // ── Discord-style gateway additions ──────────────────────────────────────────────────────
    // Every rendered frame carries a monotonic `seq` (the post-line rendered cursor) so the client
    // RESUMEs after a drop with `?since=last_seq` — gap-free and dup-free. A HELLO frame announces
    // the heartbeat cadence; a periodic HEARTBEAT carries the authoritative {seq, status} so the
    // client's liveness watchdog has a signal AND status self-corrects even with no new content.
    // The socket is disposable: if the client goes silent (no inbound at all) we idle-close so a
    // half-open zombie can't linger (the leaked-ESTABLISHED-connection case).
    const HEARTBEAT_MS: u64 = 10_000;
    const IDLE_CLOSE_MS: u64 = 30_000;

    let status0 = st
        .engine
        .get(&id)
        .await
        .map(|s| s.status)
        .unwrap_or_default();
    let hello = serde_json::json!({
        "kind": "hello", "heartbeatMs": HEARTBEAT_MS, "seq": since, "status": status0,
    });
    if send_json(&mut sink, &hello).await.is_err() {
        return;
    }

    // 2. Backfill: read rendered lines [since, total), stamping each with its post-line cursor.
    let (backfill, mut cursor) = read_from(&st, &id, &log_path, since).await;
    {
        let mut seq = since;
        for line in &backfill {
            seq += 1;
            if send_rendered(&mut sink, line, seq).await.is_err() {
                return;
            }
        }
    }

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_millis(HEARTBEAT_MS));
    heartbeat.tick().await; // the first interval tick is immediate — consume it
    let mut last_inbound = tokio::time::Instant::now();

    // 3. Steady-state: flush new rendered lines at [cursor..] (seq-stamped), check for turn end,
    //    then wait for a subscriber message (poke/live), an inbound frame (resets the idle clock),
    //    a heartbeat tick, or the idle deadline.
    loop {
        let (new_lines, total) = read_from(&st, &id, &log_path, cursor).await;
        {
            let mut seq = cursor;
            for line in &new_lines {
                seq += 1;
                if send_rendered(&mut sink, line, seq).await.is_err() {
                    return;
                }
            }
        }
        cursor = total.max(cursor);

        let status = match st.engine.get(&id).await {
            Some(s) => s.status,
            None => {
                return;
            }
        };
        if matches!(status.as_str(), "done" | "failed" | "killed") {
            let (tail, total) = read_from(&st, &id, &log_path, cursor).await;
            {
                let mut seq = cursor;
                for line in &tail {
                    seq += 1;
                    if send_rendered(&mut sink, line, seq).await.is_err() {
                        return;
                    }
                }
            }
            cursor = total.max(cursor);
            let exit = with_seq(
                serde_json::json!({"kind":"other","raw":{"engineExit":{"code":serde_json::Value::Null,"status":status}}}),
                cursor,
            );
            let _ = send_json(&mut sink, &exit).await;
            let _ = sink
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1000,
                    reason: "ended".into(),
                })))
                .await;
            return;
        }

        let idle_deadline = last_inbound + std::time::Duration::from_millis(IDLE_CLOSE_MS);
        tokio::select! {
            msg = rx.recv() => match msg {
                None => break,
                Some(WsMsg::Poke) => {}
                Some(WsMsg::Live(frame)) => {
                    if send_json(&mut sink, &with_seq(frame, cursor)).await.is_err() { return; }
                }
            },
            inbound = stream.next() => match inbound {
                // A Ping is auto-answered with a Pong by the WS layer once we poll the inbound half.
                // Any inbound frame (incl. the client's keepalive ping) proves the peer is alive →
                // reset the idle clock. A Close or read error means the client is gone.
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => { return; }
                Some(Ok(_)) => { last_inbound = tokio::time::Instant::now(); }
            },
            _ = heartbeat.tick() => {
                let status = st.engine.get(&id).await.map(|s| s.status).unwrap_or_default();
                let hb = serde_json::json!({"kind":"heartbeat","seq":cursor,"status":status});
                if send_json(&mut sink, &hb).await.is_err() { return; }
            },
            _ = tokio::time::sleep_until(idle_deadline) => {
                // No inbound for IDLE_CLOSE_MS (not even the client's ~20s keepalive ping) → the peer
                // is gone but never sent a FIN. Reap the half-open socket.
                let _ = sink.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 1000, reason: "idle".into() }))).await;
                return;
            },
        }
    }

    let _ = sink
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "ended".into(),
        })))
        .await;
}

/// Send one rendered line as the client expects: parse_line→to_wire, with the raw fallback for an
/// unparseable line. Each frame is stamped with [seq] (the post-line rendered cursor) so the client
/// can RESUME from exactly this point. Returns Err(()) if the socket is gone.
async fn send_rendered(sink: &mut WsSink, line: &str, seq: usize) -> Result<(), ()> {
    let evs = parse_line(line);
    if evs.is_empty() {
        let raw: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|_| serde_json::Value::String(line.to_string()));
        send_json(
            sink,
            &with_seq(serde_json::json!({"kind":"backfill","raw": raw}), seq),
        )
        .await
    } else {
        // A `stream_event` source line is a STREAMING DELTA (text_delta/thinking_delta); mark its
        // frames `delta:true`. The complete `assistant` message that restates the same blocks is a
        // SEALED block (no marker). The client reducer accumulates deltas into the growing node and
        // lets the sealed block finalize that run (replace, not append) — reconciling the duplicate by
        // block identity instead of double-rendering. Old logs (no stream_events) are all sealed.
        let is_delta = is_stream_event(line);
        for ev in &evs {
            let frame = with_seq(ev.to_wire(), seq);
            send_json(sink, &with_delta(frame, is_delta)).await?;
        }
        Ok(())
    }
}

/// Mark a wire frame as a streaming delta (`delta:true`) vs a sealed complete block. Top-level
/// sibling like `seq`; never touches `raw`. Only set when true to keep sealed frames byte-identical
/// to the pre-streaming format (so old clients and the dedup test are unaffected).
fn with_delta(mut v: serde_json::Value, is_delta: bool) -> serde_json::Value {
    if is_delta {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("delta".into(), serde_json::Value::Bool(true));
        }
    }
    v
}

/// Stamp the monotonic rendered-line cursor onto a wire frame as a top-level `seq` sibling. It never
/// touches `raw`, so a frame's payload is unchanged — only the resume cursor is added.
fn with_seq(mut v: serde_json::Value, seq: usize) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("seq".into(), serde_json::json!(seq));
    }
    v
}

/// Send a JSON value as a text frame. On a send error, log benign client-disconnects at debug
/// and genuine (non-disconnect) send failures at warn.
async fn send_json(sink: &mut WsSink, v: &serde_json::Value) -> Result<(), ()> {
    use futures_util::SinkExt;
    match sink.send(Message::Text(v.to_string().into())).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if is_client_disconnect(None, Some(msg.as_str())) {
                tracing::debug!("ws client disconnect: {msg}");
            } else {
                tracing::warn!("ws send error: {msg}");
            }
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_benign_client_disconnects() {
        assert!(is_client_disconnect(
            Some("ERR_STREAM_PREMATURE_CLOSE"),
            None
        ));
        assert!(is_client_disconnect(None, Some("Premature close")));
        assert!(is_client_disconnect(Some("ECONNRESET"), None));
        assert!(is_client_disconnect(Some("EPIPE"), None));
    }

    #[test]
    fn does_not_flag_real_server_errors() {
        assert!(!is_client_disconnect(Some("ERR_SOMETHING_ELSE"), None));
        assert!(!is_client_disconnect(None, Some("internal error")));
        assert!(!is_client_disconnect(None, None));
    }

    use crate::api::auth::issue_token;
    use crate::api::test_support::test_state_with_fixture;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use std::time::Duration;

    async fn serve(st: crate::api::state::AppState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = crate::api::app(st).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    async fn wait_status(st: &crate::api::state::AppState, id: &str, want: &str) {
        for _ in 0..250 {
            if st.engine.get(id).await.map(|s| s.status) == Some(want.to_string()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for status={want}");
    }

    async fn spawn_live_session(st: &crate::api::state::AppState) -> (u16, String) {
        use std::collections::HashMap;
        use std::process::Command;
        // Create a temp src repo "demo".
        let dir = st.config.src_root.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&dir)
            .status()
            .unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&dir)
            .status()
            .unwrap();
        let port = serve(st.clone()).await;
        let id = st
            .engine
            .submit("demo", "go", HashMap::new())
            .await
            .unwrap();
        (port, id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_rejects_a_bad_token() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let id = "s-ws-bad";
        st.store
            .create(crate::engine::store::CreateInput {
                id: id.into(),
                repos: vec![],
                skills: vec![],
                prompt: "p".into(),
                worktree_path: Some("/tmp/x".into()),
                branch: Some("b".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let port = serve(st).await;
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token=bad");
        let res = tokio_tungstenite::connect_async(url).await;
        // The auth gate rejects the upgrade (401) → connect fails OR the socket closes immediately.
        assert!(res.is_err(), "bad token must not establish a WS");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_streams_a_session_to_completion() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let (port, id) = spawn_live_session(&st).await;
        let token = issue_token(&st.config.auth_secret, 3600, super::now_secs());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}");
        let (mut sock, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

        let mut kinds = Vec::new();
        // Read until the socket closes (server closes after engineExit).
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                kinds.push(v["kind"].as_str().unwrap_or("").to_string());
            }
        }
        assert!(
            kinds.iter().any(|k| k == "text" || k == "result"),
            "must deliver at least one text or result event, got {kinds:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_since_at_log_length_sends_only_engine_exit() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let (port, id) = spawn_live_session(&st).await;
        wait_status(&st, &id, "done").await;
        // Use the FILTERED log length as `since` — matches what the client sees.
        let raw_log = st.engine.get_log(&id);
        let since = filter_rendered(&raw_log).len();
        let token = issue_token(&st.config.auth_secret, 3600, super::now_secs());
        let url =
            format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}&since={since}");
        let (mut sock, _r) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut msgs = Vec::new();
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                msgs.push(serde_json::from_str::<Value>(&t).unwrap());
            }
        }
        // No backfill lines (since skipped them all); only the synthetic terminal engineExit.
        assert!(
            !msgs.iter().any(|m| m["kind"] == "text"),
            "no text events expected when since covers all lines, got {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m["kind"] == "hello"),
            "a HELLO frame must open the stream, got {msgs:?}"
        );
        // Ignore the gateway HELLO/HEARTBEAT frames; the rest must be only the terminal engineExit.
        assert!(
            msgs.iter()
                .filter(|m| m["kind"] != "hello" && m["kind"] != "heartbeat")
                .all(|m| m["kind"] == "other" && m["raw"].get("engineExit").is_some()),
            "only engineExit frames expected, got {msgs:?}"
        );
    }

    /// Block until the FILTERED (rendered) log of `id` has at least `n` lines, so we can connect
    /// to an ACTIVE session with a guaranteed-non-empty backfill — i.e. the connection straddles
    /// the backfill→live boundary (some lines already on disk, more arriving live).
    async fn wait_rendered_at_least(st: &crate::api::state::AppState, id: &str, n: usize) {
        for _ in 0..250 {
            if filter_rendered(&st.engine.get_log(id)).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timeout waiting for >= {n} rendered lines");
    }

    /// Regression for the double-delivery review finding: when a client (re)connects to an ACTIVE
    /// session, a rendered line that lands in the window between "subscribe" and "backfill read"
    /// used to be delivered TWICE — once in the backfill snapshot, once again from the forwarded
    /// live buffer. The cursor-based handler must deliver each rendered line EXACTLY ONCE and IN
    /// ORDER across the backfill→live boundary (no dup, no gap).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_no_duplicate_frames_across_backfill_live_boundary() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let (port, id) = spawn_live_session(&st).await;
        // Connect while the turn is still producing: at least one rendered line is already on
        // disk (so backfill is non-empty), but the result line is still to come (arrives live).
        wait_rendered_at_least(&st, &id, 1).await;
        let token = issue_token(&st.config.auth_secret, 3600, super::now_secs());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}&since=0");
        let (mut sock, _r) = tokio_tungstenite::connect_async(url).await.unwrap();

        // Collect every frame until the server closes after engineExit.
        let mut frames = Vec::new();
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                frames.push(serde_json::from_str::<Value>(&t).unwrap());
            }
        }

        // Exactly one terminal engineExit frame.
        let exits = frames
            .iter()
            .filter(|m| m["raw"].get("engineExit").is_some())
            .count();
        assert_eq!(exits, 1, "exactly one engineExit expected, got {frames:?}");

        // Split delivered frames: RENDERED (cursor-delivered, incl. agentResult) vs live-only
        // (init/retry, forwarded from the subscriber). The rendered stream must equal the rendered log
        // EXACTLY — same lines, same order, NO duplicate (the old bug) and NO gap. Live-only frames
        // are extra and never duplicate a rendered line (they come from non-rendered source lines).
        // Exclude live-only (init/retry) AND the gateway HELLO/HEARTBEAT frames — none of these are
        // rendered-cursor lines, so they're additive and must not be in the rendered comparison.
        let gateway = |k: Option<&str>| {
            matches!(
                k,
                Some("init") | Some("retry") | Some("hello") | Some("heartbeat")
            )
        };
        let delivered: Vec<Value> = frames
            .iter()
            .filter(|m| m["raw"].get("engineExit").is_none() && !gateway(m["kind"].as_str()))
            .map(|m| m["raw"].clone())
            .collect();
        let expected: Vec<Value> = filter_rendered(&st.engine.get_log(&id))
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect();
        assert_eq!(
            delivered, expected,
            "rendered frames must equal the rendered log exactly (no dup, no gap, in order)\n\
             delivered={delivered:?}\nexpected={expected:?}"
        );

        // Belt-and-suspenders: no rendered raw line appears more than once.
        for line in &expected {
            let count = delivered.iter().filter(|d| *d == line).count();
            assert_eq!(
                count, 1,
                "rendered line delivered {count} times (expected 1): {line:?}"
            );
        }
        // NOTE: any live-only frames (init/retry) that arrive after subscribe are
        // additive and correctly excluded above; whether `init` lands live here is tailer-timing
        // dependent, so the deterministic proof of forwarding is `is_live_only_*` + the live loop.
    }

    #[test]
    fn is_live_only_classifies_non_rendered_event_kinds() {
        use crate::engine::stream::ClaudeEvent;
        use serde_json::json;
        // Live-only: produced solely from non-rendered source lines (system/init, api_retry).
        assert!(is_live_only(&ClaudeEvent::Init {
            session_id: "s".into(),
            raw: json!({})
        }));
        assert!(is_live_only(&ClaudeEvent::Retry {
            attempt: 1,
            max_retries: 3,
            category: "x".into(),
            raw: json!({})
        }));
        // Rendered (delivered via the cursor) → NOT live-only (forwarding would duplicate).
        // AgentResult is rendered now: the engine persists a `agent_result` marker that the cursor
        // carries as kind:agentResult, so forwarding the live event too would double-deliver it.
        assert!(!is_live_only(&ClaudeEvent::AgentResult {
            tool_use_id: "t".into(),
            text: "o".into(),
            raw: json!({})
        }));
        assert!(!is_live_only(&ClaudeEvent::Text {
            text: "hi".into(),
            parent_tool_use_id: None,
            raw: json!({})
        }));
        assert!(!is_live_only(&ClaudeEvent::Result {
            is_error: false,
            cost_usd: None,
            text: None,
            raw: json!({})
        }));
        assert!(!is_live_only(&ClaudeEvent::Prompt {
            text: "p".into(),
            at: 0,
            raw: json!({})
        }));
        assert!(!is_live_only(&ClaudeEvent::Other { raw: json!({}) }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_answers_client_ping_and_streams_to_completion() {
        use tokio_tungstenite::tungstenite::Message as TMsg;
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let (port, id) = spawn_live_session(&st).await;
        let token = issue_token(&st.config.auth_secret, 3600, super::now_secs());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}");
        let (mut sock, _r) = tokio_tungstenite::connect_async(url).await.unwrap();
        // Send a client Ping: the server must keep the connection healthy (poll inbound → pong) and
        // still stream to completion rather than ignoring inbound and stalling.
        sock.send(TMsg::Ping(vec![1, 2, 3].into())).await.unwrap();
        let mut saw_exit = false;
        while let Some(Ok(msg)) = sock.next().await {
            if let TMsg::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["raw"].get("engineExit").is_some() {
                    saw_exit = true;
                }
            }
        }
        assert!(
            saw_exit,
            "stream must run to engineExit even after the client sent a Ping"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_opens_with_hello_and_stamps_monotonic_seq() {
        let st = test_state_with_fixture("fake-sdk-bridge-ok.sh").await;
        let (port, id) = spawn_live_session(&st).await;
        let token = issue_token(&st.config.auth_secret, 3600, super::now_secs());
        let url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/stream?token={token}");
        let (mut sock, _r) = tokio_tungstenite::connect_async(url).await.unwrap();

        let mut frames = Vec::new();
        while let Some(Ok(msg)) = sock.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                frames.push(serde_json::from_str::<Value>(&t).unwrap());
            }
        }

        // HELLO opens the stream and announces a numeric heartbeat cadence (the gateway contract).
        let hello = frames.first().expect("at least one frame");
        assert_eq!(
            hello["kind"], "hello",
            "first frame must be HELLO, got {frames:?}"
        );
        assert!(
            hello["heartbeatMs"].as_u64().is_some(),
            "HELLO must carry heartbeatMs, got {hello:?}"
        );

        // Every frame carries a numeric `seq` (the RESUME cursor), monotonically non-decreasing.
        let mut last = 0u64;
        for f in &frames {
            let seq = f["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("frame missing seq: {f:?}"));
            assert!(
                seq >= last,
                "seq must be non-decreasing: {seq} < {last} in {f:?}"
            );
            last = seq;
        }
        assert!(
            last > 0,
            "the terminal frame's seq must advance past 0, got {frames:?}"
        );
    }
}
