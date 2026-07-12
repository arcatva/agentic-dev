//! ChatGPT subscription OAuth endpoints. Login runs a one-shot loopback listener on
//! 127.0.0.1:1455 (the fixed OAuth redirect target); the OAuth consent must be completed in a
//! browser that can reach the server host (documented co-location assumption — see the spec).
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::engine::model::openai_oauth as oauth;
use crate::engine::providers::{Protocol, Provider};

/// The provider row written to providers.json on a successful login (NO key — the token lives in the
/// 0600 store). base_url points the LiteLLM proxy at the codex backend.
fn subscription_provider() -> Provider {
    Provider {
        name: oauth::PROVIDER_NAME.into(),
        base_url: oauth::CODEX_BASE_URL.into(),
        api_key: String::new(),
        api_key_env: None,
        model: "gpt-5".into(),
        protocol: Protocol::Openai,
        capability: 0.9,
        description: Some("ChatGPT subscription (OAuth) — GPT worker".into()),
        priority: 0.5,
        cost: 0.9,
        router: false,
        enabled: true,
    }
}

fn callback_port() -> u16 {
    std::env::var("AGENTIC_OPENAI_CALLBACK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1455)
}

#[derive(Clone)]
struct CbState {
    verifier: Arc<String>,
    expected_state: Arc<String>,
    done: Arc<tokio::sync::Notify>,
}

#[derive(serde::Deserialize)]
struct CbQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

const SUCCESS_HTML: &str = "<html><body style=\"font-family:sans-serif\"><h2>ChatGPT connected ✓</h2>\
     <p>You can close this tab and return to agentic-dev.</p></body></html>";

async fn callback(State(st): State<CbState>, Query(q): Query<CbQuery>) -> Html<String> {
    if let Some(err) = q.error {
        st.done.notify_one();
        return Html(format!("<h2>Login failed: {err}</h2>"));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Html("<h2>Missing code/state</h2>".to_string());
    };
    if state != *st.expected_state {
        // Ignore a bad-state hit; keep the listener up for the real redirect.
        return Html("<h2>State mismatch — ignore this tab.</h2>".to_string());
    }
    let verifier = st.verifier.clone();
    let exchanged =
        tokio::task::spawn_blocking(move || oauth::exchange_code(&code, &verifier)).await;
    st.done.notify_one();
    match exchanged {
        Ok(Ok(tok)) => {
            if let Err(e) = oauth::save(&tok) {
                return Html(format!("<h2>Could not persist token: {e}</h2>"));
            }
            if let Err(e) = crate::engine::providers::upsert(subscription_provider()) {
                return Html(format!("<h2>Could not register provider: {e}</h2>"));
            }
            crate::engine::litellm::request_reload();
            Html(SUCCESS_HTML.to_string())
        }
        Ok(Err(e)) => Html(format!("<h2>Token exchange failed: {e}</h2>")),
        Err(e) => Html(format!("<h2>Token exchange task failed: {e}</h2>")),
    }
}

/// POST /api/providers/openai-subscription/login — start OAuth, return the authorize URL.
pub async fn login_start() -> Response {
    let (verifier, challenge) = oauth::gen_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::authorize_url(&challenge, &state);
    let st = CbState {
        verifier: Arc::new(verifier),
        expected_state: Arc::new(state),
        done: Arc::new(tokio::sync::Notify::new()),
    };
    // Bind BEFORE returning so the port is ready when the user's browser hits the redirect.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", callback_port())).await {
        Ok(l) => l,
        Err(e) => {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "callback port {} busy (login already in progress?): {e}",
                        callback_port()
                    )
                })),
            )
                .into_response()
        }
    };
    let done = st.done.clone();
    let app = axum::Router::new()
        .route("/auth/callback", axum::routing::get(callback))
        .with_state(st);
    tokio::spawn(async move {
        let shutdown = async move {
            tokio::select! {
                _ = done.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
            }
        };
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
    });
    Json(json!({ "authorize_url": url })).into_response()
}

/// GET /api/providers/openai-subscription/status — connection state for the client's status chip.
pub async fn status() -> Response {
    let tok = oauth::load();
    let connected = tok
        .as_ref()
        .map(|t| !t.access_token.is_empty() && !t.needs_reauth)
        .unwrap_or(false);
    Json(json!({
        "connected": connected,
        "account_id": tok.as_ref().map(|t| t.account_id.clone()).unwrap_or_default(),
        "expires_at": tok.as_ref().map(|t| t.expires_at).unwrap_or(0),
        "needs_reauth": tok.as_ref().map(|t| t.needs_reauth).unwrap_or(false),
    }))
    .into_response()
}

/// POST /api/providers/openai-subscription/logout — clear the store + remove the provider.
pub async fn logout() -> Response {
    oauth::clear();
    let _ = crate::engine::providers::remove(oauth::PROVIDER_NAME);
    crate::engine::litellm::request_reload();
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_provider_shape() {
        let p = subscription_provider();
        assert!(p.is_subscription());
        assert!(matches!(p.protocol, Protocol::Openai));
        assert_eq!(p.base_url, oauth::CODEX_BASE_URL);
        assert!(p.api_key.is_empty(), "token never in providers.json");
        assert!(p.enabled);
    }
}
