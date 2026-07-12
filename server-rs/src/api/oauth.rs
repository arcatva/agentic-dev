//! ChatGPT-subscription OAuth endpoints.
//!
//! `POST /api/oauth/openai/start`    → `{ authorize_url, state }` (opens a browser; PKCE stashed server-side)
//! `POST /api/oauth/openai/complete` → `{ account_id, model, expires_at }` (relays the redirect `code`+`state`)
//!
//! The fixed redirect is `http://localhost:1455/auth/callback`, so the code lands on the BROWSER's own
//! loopback. Two ways it reaches us:
//!   1. a browser on the backend host hits the one-shot listener [`spawn_callback_listener`] here, or
//!   2. the client (Android) captures the redirect and relays it to `/complete`.
//! Both funnel into `engine::oauth::complete_login`.

use crate::api::state::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::engine::oauth;

/// POST /api/oauth/openai/start
pub async fn oauth_start(State(_st): State<AppState>) -> Response {
    let started = oauth::start_login();
    // Best-effort: also listen on the loopback for a browser running on the backend host.
    spawn_callback_listener(_st.oauth_fn.clone());
    Json(json!({ "authorize_url": started.authorize_url, "state": started.state })).into_response()
}

#[derive(serde::Deserialize)]
pub struct CompleteReq {
    pub code: String,
    pub state: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// POST /api/oauth/openai/complete
pub async fn oauth_complete(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let req: CompleteReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid request: {e}") })),
            )
                .into_response()
        }
    };
    match oauth::complete_login(
        &req.code,
        &req.state,
        req.model.as_deref(),
        st.oauth_fn.as_deref(),
    )
    .await
    {
        Ok(done) => Json(json!({
            "account_id": done.account_id,
            "model": done.model,
            "expires_at": done.expires_at,
        }))
        .into_response(),
        Err(oauth::CompleteError::BadState) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unknown or expired state" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Spawn a one-shot loopback listener on 127.0.0.1:1455 that accepts the OAuth redirect for a browser
/// running on the backend host. Non-fatal if the port is taken (the `/complete` relay path still works).
/// Exits after one successful callback or a 10-minute timeout.
fn spawn_callback_listener(oauth_fn: Option<crate::api::state::OauthFn>) {
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", oauth::CALLBACK_PORT)).await
        {
            Ok(l) => l,
            Err(e) => {
                // Already bound (a prior /start, or something else) → rely on the /complete relay.
                tracing::info!("[oauth] callback listener not started: {e}");
                return;
            }
        };
        let deadline = std::time::Duration::from_secs(600);
        let accepted = tokio::time::timeout(deadline, listener.accept()).await;
        let Ok(Ok((mut sock, _))) = accepted else {
            return; // timed out or accept error
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 2048];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req_line = String::from_utf8_lossy(&buf[..n]);
        let (code, state) = parse_callback_query(&req_line);
        let body = if let (Some(code), Some(state)) = (code, state) {
            match oauth::complete_login(&code, &state, None, oauth_fn.as_deref()).await {
                Ok(_) => "ChatGPT connected. You can close this tab and return to the app.",
                Err(_) => "Sign-in failed. Return to the app and try again.",
            }
        } else {
            "Missing authorization code. Return to the app and try again."
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.flush().await;
    });
}

/// Pull `code` and `state` out of the `GET /auth/callback?...` request line. Parses the path via
/// `reqwest::Url` (already a dep) so percent-decoding of the query is handled for us.
fn parse_callback_query(req: &str) -> (Option<String>, Option<String>) {
    let path = req.split_whitespace().nth(1).unwrap_or("");
    // The request line carries only a path; give Url a dummy origin so it parses + decodes the query.
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{path}")) else {
        return (None, None);
    };
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    (code, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state_from_callback() {
        let req = "GET /auth/callback?code=abc123&state=xyz HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
        let (code, state) = parse_callback_query(req);
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn percent_encoded_values_are_decoded() {
        let req = "GET /auth/callback?code=a%2Bb&state=x%20y HTTP/1.1\r\n";
        let (code, state) = parse_callback_query(req);
        assert_eq!(code.as_deref(), Some("a+b"));
        assert_eq!(state.as_deref(), Some("x y"));
    }

    #[test]
    fn missing_query_is_none() {
        let (code, state) = parse_callback_query("GET /auth/callback HTTP/1.1\r\n");
        assert!(code.is_none() && state.is_none());
    }
}
