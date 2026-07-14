//! ChatGPT-subscription OAuth login endpoints (Codex-compatible Authorization Code + PKCE).
//!
//! The redirect URI is the fixed loopback `http://localhost:1455/auth/callback` baked into the
//! Codex OAuth app, so — exactly like Codex CLI — the server hosts a one-shot listener on :1455 that
//! the browser's redirect hits. The Android client just kicks off the flow, opens the returned
//! authorize URL in a browser, and polls status.
//!
//! Routes (all behind the normal API token auth):
//!   POST /api/providers/oauth/chatgpt/start  → { authorize_url }
//!   GET  /api/providers/oauth/chatgpt/status → login state (email / expiry / needs_reauth)
//!   POST /api/providers/oauth/chatgpt/logout → forget creds + drop the GPT providers

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::engine::oauth;
use crate::engine::providers::{Protocol, Provider};

/// Base URL of the ChatGPT subscription (codex) backend — worker calls reach it via the LiteLLM
/// proxy, which reads it from the provider's `base_url`. Shared with `Provider::oauth_account`,
/// which only hands the subscription bearer to a provider pointed here.
use crate::engine::oauth::CODEX_BASE_URL as CODEX_BASE;

/// GPT models registered on a successful login. Kept small and editable — the user can add/disable
/// more from the normal providers UI; all share the one ChatGPT login via the `oauth:` key ref.
/// (name, model, capability, cost, description)
const GPT_MODELS: &[(&str, &str, f32, f32, &str)] = &[
    (
        "gpt-5",
        "gpt-5",
        0.9,
        0.6,
        "OpenAI GPT-5 — ChatGPT subscription (OAuth)",
    ),
    (
        "gpt-5-codex",
        "gpt-5-codex",
        0.88,
        0.6,
        "OpenAI GPT-5 Codex — coding worker, ChatGPT subscription (OAuth)",
    ),
];

/// The single in-flight callback listener task (only one can hold port 1455). A new /start aborts
/// the previous one before rebinding.
static PENDING: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>> =
    parking_lot::Mutex::new(None);

fn abort_pending() {
    if let Some(h) = PENDING.lock().take() {
        h.abort();
    }
}

/// POST /api/providers/oauth/chatgpt/start
pub async fn start() -> Response {
    let pkce = oauth::gen_pkce();
    let state = oauth::gen_state();

    // Bind the loopback callback listener BEFORE returning the URL, so the browser redirect can
    // never race ahead of a ready listener. Abort any stale in-flight attempt first.
    abort_pending();
    let (primary, secondary) = match bind_callbacks().await {
        Some(pair) => pair,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!(
                    "cannot bind OAuth callback port {} on loopback. Another login may be in progress.",
                    oauth::REDIRECT_PORT
                ) })),
            )
                .into_response()
        }
    };

    let url = oauth::authorize_url(&pkce.challenge, &state);
    let verifier = pkce.verifier;
    let expected_state = state;
    let handle = tokio::spawn(async move {
        run_callback(primary, secondary, verifier, expected_state).await;
    });
    *PENDING.lock() = Some(handle);

    Json(json!({ "authorize_url": url })).into_response()
}

/// GET /api/providers/oauth/chatgpt/status
pub async fn status() -> Response {
    let st = oauth::status(oauth::DEFAULT_ACCOUNT);
    Json(json!({
        "account": st.account,
        "logged_in": st.logged_in,
        "email": st.email,
        "account_id": st.account_id,
        "expires_at": st.expires_at,
        "needs_reauth": st.needs_reauth,
        // login is only useful once the proxy that runs openai providers exists
        "litellm_available": crate::engine::litellm::available(),
    }))
    .into_response()
}

/// POST /api/providers/oauth/chatgpt/logout — forget creds and remove the GPT providers.
pub async fn logout() -> Response {
    abort_pending();
    let _ = oauth::logout(oauth::DEFAULT_ACCOUNT);
    // Drop any provider bound to this account (leave BYOK/other providers untouched).
    let account = oauth::DEFAULT_ACCOUNT;
    for p in crate::engine::providers::load_list() {
        if p.oauth_account() == Some(account) {
            let _ = crate::engine::providers::remove(&p.name);
        }
    }
    crate::engine::litellm::request_reload();
    Json(json!({ "ok": true })).into_response()
}

/// Bind the callback on BOTH loopback families (`127.0.0.1` and `::1`) so a browser that resolves
/// `localhost` to IPv6-first still reaches us even if it doesn't fall back to IPv4. Returns the
/// primary listener + an optional secondary; `None` only when neither family binds (port busy).
async fn bind_callbacks() -> Option<(tokio::net::TcpListener, Option<tokio::net::TcpListener>)> {
    let v4 = bind_one("127.0.0.1").await;
    let v6 = bind_one("::1").await;
    match (v4, v6) {
        (Some(a), b) => Some((a, b)),
        (None, Some(b)) => Some((b, None)),
        (None, None) => None,
    }
}

async fn bind_one(ip: &str) -> Option<tokio::net::TcpListener> {
    // A previous aborted attempt may not have released the socket yet; one short retry covers it.
    for attempt in 0..2 {
        if let Ok(l) = tokio::net::TcpListener::bind((ip, oauth::REDIRECT_PORT)).await {
            return Some(l);
        }
        if attempt == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    None
}

/// Accept a connection from either loopback listener, or `pending` forever when there's no secondary
/// (so the `select!` arm is inert rather than busy).
async fn accept_either(
    l: &Option<tokio::net::TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match l {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Accept connections until the callback (with a valid state) arrives or 5 minutes pass.
async fn run_callback(
    primary: tokio::net::TcpListener,
    secondary: Option<tokio::net::TcpListener>,
    verifier: String,
    expected_state: String,
) {
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    loop {
        let accepted = tokio::select! {
            _ = &mut deadline => {
                tracing::warn!("[oauth] login timed out with no callback");
                return;
            }
            a = primary.accept() => a,
            a = accept_either(&secondary) => a,
        };
        {
                let mut stream = match accepted {
                    Ok((s, _)) => s,
                    Err(e) => { tracing::warn!("[oauth] callback accept failed: {e}"); continue; }
                };
                let Some(target) = read_request_target(&mut stream).await else {
                    respond(&mut stream, 400, "bad request").await;
                    continue;
                };
                let Some((path, query)) = split_target(&target) else {
                    respond(&mut stream, 404, "not found").await;
                    continue;
                };
                if path != "/auth/callback" {
                    respond(&mut stream, 404, "not found").await; // favicon etc. — keep waiting
                    continue;
                }
                match handle_callback(&query, &verifier, &expected_state).await {
                    Ok(()) => {
                        respond(&mut stream, 200,
                            "Signed in to ChatGPT. You can close this tab and return to agentic-dev.").await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("[oauth] callback error: {e}");
                        respond(&mut stream, 400, &format!("login failed: {e}")).await;
                        return;
                    }
                }
        }
    }
}

/// Validate state, exchange the code, persist creds, register the GPT providers, reload LiteLLM.
async fn handle_callback(
    query: &str,
    verifier: &str,
    expected_state: &str,
) -> Result<(), String> {
    let mut code = None;
    let mut got_state = None;
    let mut err = None;
    for (k, v) in query_pairs(query) {
        match k.as_str() {
            "code" => code = Some(v),
            "state" => got_state = Some(v),
            "error" => err = Some(v),
            _ => {}
        }
    }
    if let Some(e) = err {
        return Err(format!("authorization denied: {e}"));
    }
    if got_state.as_deref() != Some(expected_state) {
        return Err("state mismatch (possible CSRF)".into());
    }
    let code = code.ok_or("no authorization code in callback")?;

    // Token exchange is blocking reqwest — run off the async runtime.
    let verifier = verifier.to_string();
    let creds = tokio::task::spawn_blocking(move || oauth::exchange_code(&code, &verifier))
        .await
        .map_err(|e| format!("exchange task panicked: {e}"))??;

    oauth::save(oauth::DEFAULT_ACCOUNT, &creds).map_err(|e| format!("saving creds: {e}"))?;
    register_gpt_providers(oauth::DEFAULT_ACCOUNT)?;
    // New rotating bearer + new openai providers → regenerate LiteLLM config and (re)start the proxy.
    crate::engine::litellm::request_reload();
    // Keep the rotated-token/reload loop running for this account.
    oauth::spawn_refresh_task();
    tracing::info!("[oauth] ChatGPT login complete; {} GPT model(s) registered", GPT_MODELS.len());
    Ok(())
}

fn register_gpt_providers(account: &str) -> Result<(), String> {
    // Don't clobber a provider the user already tuned (disabled it, or changed capability/priority)
    // — a re-login (e.g. after needs_reauth) must not silently reset those. The rotating bearer is
    // read live from the store regardless, so an existing entry keeps working with the new token.
    let existing: std::collections::HashSet<String> = crate::engine::providers::load_list()
        .iter()
        .map(|p| p.name.to_lowercase())
        .collect();
    for (name, model, capability, cost, desc) in GPT_MODELS {
        if existing.contains(&name.to_lowercase()) {
            continue;
        }
        let p = Provider {
            name: (*name).to_string(),
            base_url: CODEX_BASE.to_string(),
            api_key: String::new(),
            // The bearer is the rotating OAuth token — referenced, never stored in providers.json.
            api_key_env: Some(oauth::key_ref(account)),
            model: (*model).to_string(),
            protocol: Protocol::Openai,
            capability: *capability,
            description: Some((*desc).to_string()),
            priority: 0.5,
            cost: *cost,
            router: false,
            enabled: true,
        };
        crate::engine::providers::upsert(p).map_err(|e| format!("registering {name}: {e}"))?;
    }
    Ok(())
}

// ── tiny HTTP/1.1 request reader (callback only; no framework on :1455) ──

/// Read just enough of the request to get the request-line target (`GET <target> HTTP/1.1`).
/// Reads until the first newline (the request line is all we need) rather than assuming one `read`
/// delivers it — TCP may fragment even a short line, which would otherwise truncate the callback URL.
async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut acc = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            break; // peer closed before a full line
        }
        acc.extend_from_slice(&buf[..n]);
        if acc.contains(&b'\n') || acc.len() >= 8192 {
            break; // have the request line (or a runaway request — bail)
        }
    }
    let head = String::from_utf8_lossy(&acc);
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    Some(parts.next()?.to_string())
}

fn split_target(target: &str) -> Option<(String, String)> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    match target.split_once('?') {
        Some((p, q)) => Some((p.to_string(), q.to_string())),
        None => Some((target.to_string(), String::new())),
    }
}

/// Percent-decode application/x-www-form-urlencoded query pairs (reqwest's Url is overkill here and
/// needs a base). Handles `%XX` and `+`.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn respond(stream: &mut tokio::net::TcpStream, code: u16, message: &str) {
    let reason = if code == 200 { "OK" } else { "Error" };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>agentic-dev</title>\
         <body style=\"font:16px system-ui;margin:3rem\"><p>{}</p></body>",
        html_escape(message)
    );
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_target_parses_path_and_query() {
        assert_eq!(
            split_target("/auth/callback?code=abc&state=xyz"),
            Some(("/auth/callback".into(), "code=abc&state=xyz".into()))
        );
        assert_eq!(
            split_target("/favicon.ico"),
            Some(("/favicon.ico".into(), String::new()))
        );
        assert_eq!(split_target("  "), None);
    }

    #[test]
    fn query_pairs_percent_decode() {
        let pairs = query_pairs("code=a%2Bb%2Fc&state=x+y&error=");
        assert_eq!(pairs[0], ("code".to_string(), "a+b/c".to_string()));
        assert_eq!(pairs[1], ("state".to_string(), "x y".to_string()));
        assert_eq!(pairs[2], ("error".to_string(), String::new()));
    }

    #[test]
    fn percent_decode_leaves_bad_escapes() {
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn html_escape_blocks_injection() {
        assert_eq!(html_escape("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
    }

    #[test]
    fn gpt_models_use_oauth_key_ref() {
        // The registered providers must reference the token, never embed it.
        for (name, model, cap, cost, _) in GPT_MODELS {
            assert!(!name.is_empty() && !model.is_empty());
            assert!((0.0..=1.0).contains(cap) && (0.0..=1.0).contains(cost));
        }
        assert_eq!(oauth::key_ref("chatgpt"), "oauth:chatgpt");
        assert_eq!(oauth::account_from_key_ref("oauth:chatgpt"), Some("chatgpt"));
        assert_eq!(oauth::account_from_key_ref("MINIMAX_API_KEY"), None);
    }
}
