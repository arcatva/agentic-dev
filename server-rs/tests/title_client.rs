//! HTTP-level tests for `AnthropicHttpTitleGenerator`.
//!
//! Each test stubs `/v1/messages` via `wiremock` and asserts one aspect
//! of the client (happy path, error mapping, timeout, body shape,
//! dedup-vs-current, etc.). No real network calls are made.

use std::path::PathBuf;
use std::time::Duration;

use agentic_dev_server::engine::title_client::{
    AnthropicHttpTitleGenerator, AuthScheme, TitleGenerator,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(base_url: &str, model: &str) -> AnthropicHttpTitleGenerator {
    client_with_scheme(base_url, model, AuthScheme::ApiKey)
}

fn client_with_scheme(
    base_url: &str,
    model: &str,
    scheme: AuthScheme,
) -> AnthropicHttpTitleGenerator {
    AnthropicHttpTitleGenerator::with(
        base_url.to_string(),
        "test-token".to_string(),
        scheme,
        model.to_string(),
        Duration::from_secs(5),
    )
}

fn cwd() -> PathBuf {
    PathBuf::from("/tmp")
}

#[tokio::test]
async fn generate_returns_some_title_on_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "修复登录 bug"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("修一下登录", &cwd()).await.unwrap();
    assert_eq!(r.as_deref(), Some("修复登录 bug"));
}

#[tokio::test]
async fn generate_returns_none_on_invalid_title_from_model() {
    // "ok" is too short (< 5 chars after trim) → fails is_valid_title.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("...", &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn generate_returns_none_on_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("...", &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn generate_returns_none_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("...", &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn generate_returns_none_on_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(20)))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let started = std::time::Instant::now();
    let r = c.generate("...", &cwd()).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r, None);
    assert!(
        elapsed < Duration::from_secs(10),
        "must time out near 5s, took {elapsed:?}"
    );
}

#[tokio::test]
async fn generate_sends_correct_request_body() {
    let server = MockServer::start().await;
    let mock = Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-token"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_json(json!({
            "model": "haiku-test",
            "max_tokens": 32,
            "system": agentic_dev_server::engine::title_client::TITLE_SYSTEM_PROMPT_FOR_TESTS,
            "messages": [{"role": "user", "content": "hello world"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "示例"}]
        })));
    mock.expect(1).mount(&server).await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("hello world", &cwd()).await.unwrap();
    assert_eq!(r.as_deref(), Some("示例"));
}

#[tokio::test]
async fn maybe_retitle_returns_some_when_change_true() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{\"change\":true,\"title\":\"新标题\"}"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c
        .maybe_retitle("old title", &[("user".into(), "msg".into())], &cwd())
        .await
        .unwrap();
    assert_eq!(r.as_deref(), Some("新标题"));
}

#[tokio::test]
async fn maybe_retitle_returns_none_when_change_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{\"change\":false}"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.maybe_retitle("current", &[], &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn maybe_retitle_dedups_when_new_equals_current() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{\"change\":true,\"title\":\"same\"}"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.maybe_retitle("same", &[], &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn maybe_retitle_returns_none_on_invalid_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "not json"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.maybe_retitle("old", &[], &cwd()).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn auth_error_returns_none_at_surface() {
    // 401 from the upstream → Internal Err(Auth) → public surface
    // returns Ok(None) for silent fallback per the spec.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c.generate("...", &cwd()).await.unwrap();
    assert_eq!(r, None, "401 must collapse to Ok(None)");
}

#[tokio::test]
async fn bearer_scheme_sends_authorization_header_not_x_api_key() {
    // An ANTHROPIC_AUTH_TOKEN / OAuth credential MUST be sent as
    // `Authorization: Bearer`, not `x-api-key`, or the endpoint 401s. The
    // mock only matches when the bearer header is present (and would 404,
    // collapsing to None, otherwise).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "修复登录 bug"}]
        })))
        .mount(&server)
        .await;
    let c = client_with_scheme(&server.uri(), "haiku-test", AuthScheme::Bearer);
    let r = c.generate("修一下登录", &cwd()).await.unwrap();
    assert_eq!(
        r.as_deref(),
        Some("修复登录 bug"),
        "bearer-scheme client must authenticate via Authorization: Bearer"
    );
}

#[tokio::test]
async fn oauth_scheme_sends_bearer_and_beta_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer test-token"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "示例标题"}]
        })))
        .mount(&server)
        .await;
    let c = client_with_scheme(&server.uri(), "haiku-test", AuthScheme::Oauth);
    let r = c.generate("hi", &cwd()).await.unwrap();
    assert_eq!(r.as_deref(), Some("示例标题"));
}

#[tokio::test]
async fn maybe_retitle_tolerates_markdown_fenced_json() {
    // A less prompt-faithful model wraps the JSON in a ```json fence despite
    // the system prompt. maybe_retitle must still extract the change.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "```json\n{\"change\":true,\"title\":\"性能优化阶段\"}\n```"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c
        .maybe_retitle("old title", &[("user".into(), "msg".into())], &cwd())
        .await
        .unwrap();
    assert_eq!(r.as_deref(), Some("性能优化阶段"));
}

#[tokio::test]
async fn maybe_retitle_rejects_latin_only_title() {
    // The retitle path must enforce the same "contains Chinese" gate as
    // submit-time generate. A change=true carrying a Latin-only title (the
    // model ignored the Chinese-only system prompt) must collapse to Ok(None)
    // — NOT replace a good Chinese title with English. Guards P0#2.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{\"change\":true,\"title\":\"perf tuning phase\"}"}]
        })))
        .mount(&server)
        .await;
    let c = client(&server.uri(), "haiku-test");
    let r = c
        .maybe_retitle("旧标题", &[("user".into(), "msg".into())], &cwd())
        .await
        .unwrap();
    assert_eq!(r, None, "Latin-only retitle must be rejected, not stored");
}
