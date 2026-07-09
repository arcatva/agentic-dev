//! Anthropic HTTP client for session-title generation.
//!
//! The `TitleGenerator` trait is the abstraction that replaces the old
//! `TitleBridge` (which spawned `node sdk-bridge.mjs`). The production
//! implementation, `AnthropicHttpTitleGenerator`, posts directly to
//! `${ANTHROPIC_BASE_URL}/v1/messages` via `reqwest` — no Node runtime.
//!
//! All failure paths collapse to `Ok(None)` at the trait surface so the
//! caller falls back to the original user prompt (silent fallback per the
//! spec).

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::engine::title::is_accepted_generated_title;

/// Errors that the `TitleGenerator` trait can surface. Every variant is
/// collapsed to `Ok(None)` by `submit_session` / `maybe_retitle_session`
/// — callers log and continue, the title stays as the original prompt.
#[derive(Debug)]
pub enum TitleGeneratorError {
    /// Network-level error (DNS, TCP, TLS, non-2xx response, etc.).
    Http(String),
    /// The request exceeded the configured timeout.
    Timeout,
    /// The response was 2xx but the body shape was unparseable / missing content.
    InvalidResponse,
    /// 401 from the upstream endpoint — auth token rejected.
    Auth,
}

/// Generate session titles (and judge retitle) via an HTTP backend.
///
/// `generate` runs at submit time to produce a 5–12 character Chinese title
/// from the user's first prompt. `maybe_retitle` runs at the 5th turn to
/// decide whether the existing title still fits the conversation. Both
/// return `Ok(None)` for silent fallback (invalid output, timeout, error).
#[async_trait]
pub trait TitleGenerator: Send + Sync {
    async fn generate(
        &self,
        prompt: &str,
        cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError>;

    async fn maybe_retitle(
        &self,
        current_title: &str,
        recent_messages: &[(String, String)],
        cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError>;
}

/// System prompt for `generate` — produces a 5–12 character Chinese title
/// for a fresh session. Carried over from `sdk-bridge.mjs`.
pub const TITLE_SYSTEM_PROMPT: &str = "你的任务是根据用户给出的请求,生成一个会话标题。要求:\n\
- 用中文,5 到 12 个字\n\
- 只输出标题本身,不要任何解释、不要标点包裹、不要 markdown\n\
- 标题要能反映\"用户在做什么\",而不是\"用户最后一句话\"\n\
- 如果用户输入很短(比如 \"ok\"),用会话的整体意图来概括\n";

/// Public mirror of `TITLE_SYSTEM_PROMPT` so the wiremock tests can
/// assert the exact body shape without re-declaring the string.
pub const TITLE_SYSTEM_PROMPT_FOR_TESTS: &str = TITLE_SYSTEM_PROMPT;

/// System prompt for `maybe_retitle` — returns `{"change": bool, ...}` JSON
/// describing whether to update the existing title.
const RETITLE_SYSTEM_PROMPT: &str = "你的任务是判断这个 session 的标题是否需要更新。\n\n\
输入包含:\n\
- 当前标题\n\
- 最近 10 条消息 (按时间顺序)\n\n\
输出必须是以下 JSON 之一,不要其他内容,不要 markdown 代码块:\n\
- 不需要改: {\"change\": false}\n\
- 需要改:   {\"change\": true, \"title\": \"<5-12 个汉字的新标题>\"}\n\n\
判断标准:\n\
- 标题要反映\"session 当前在做什么\",而不是第一句话或最后一句话\n\
- 如果当前标题仍然准确,输出 {\"change\": false}\n\
- 如果话题已经明显改变,输出新标题\n\
- 新标题跟旧标题不能完全相同\n";

/// Anthropic API version header value (per Anthropic's docs).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `ANTHROPIC_BASE_URL` when no env var is set. Production uses
/// whatever ccswitch exports (e.g. `https://api.minimaxi.com/anthropic`).
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Default model when `ANTHROPIC_DEFAULT_HAIKU_MODEL` is unset.
const DEFAULT_MODEL: &str = "claude-haiku-4-5";

/// Default timeout in seconds for a single title call.
const DEFAULT_TIMEOUT_SECS: u64 = 20;

/// How the auth token is presented to the upstream `/v1/messages` endpoint.
///
/// The header MUST match the token kind or the endpoint replies 401:
///   - a first-party API key (`ANTHROPIC_API_KEY`) goes in `x-api-key`;
///   - an auth/bearer token (`ANTHROPIC_AUTH_TOKEN`, e.g. a ccswitch gateway
///     token) goes in `Authorization: Bearer`;
///   - a claude.ai OAuth access token (`~/.claude/.credentials.json`) goes in
///     `Authorization: Bearer` AND needs `anthropic-beta: oauth-2025-04-20`
///     (mirrors `engine::usage::fetch_usage`, the working path for the same
///     credential).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    ApiKey,
    Bearer,
    Oauth,
}

/// Production `TitleGenerator` that posts `/v1/messages` directly via reqwest.
pub struct AnthropicHttpTitleGenerator {
    client: reqwest::Client,
    base_url: String,
    auth_token: String,
    auth_scheme: AuthScheme,
    /// When true (production OAuth path), re-read the token from
    /// `~/.claude/.credentials.json` on every request so a token the claude
    /// CLI has rotated in place does not silently start returning 401. `with`
    /// (test path) leaves this false and uses the fixed `auth_token`.
    oauth_reread: bool,
    model: String,
    #[allow(dead_code)]
    timeout: Duration,
}

impl AnthropicHttpTitleGenerator {
    /// Construct with explicit values. Production uses `from_env`; tests
    /// use this to inject a `wiremock` URI.
    pub fn with(
        base_url: String,
        auth_token: String,
        auth_scheme: AuthScheme,
        model: String,
        timeout: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest::Client::build cannot fail without a custom connector");
        Self {
            client,
            base_url,
            auth_token,
            auth_scheme,
            oauth_reread: false,
            model,
            timeout,
        }
    }

    /// Build from env vars. Used at startup.
    ///
    /// Resolution order:
    ///   - `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`)
    ///   - token + auth scheme, first match wins:
    ///       1. `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer` (the ccswitch
    ///          gateway path; primary in production)
    ///       2. `ANTHROPIC_API_KEY`    → `x-api-key`
    ///       3. `~/.claude/.credentials.json` (`claudeAiOauth.accessToken`)
    ///          → `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`
    ///   - `ANTHROPIC_DEFAULT_HAIKU_MODEL` (default `claude-haiku-4-5`)
    ///   - `AGENTIC_TITLE_TIMEOUT` seconds (default 20)
    ///
    /// Returns `TitleGeneratorError::Auth` if no token source is available.
    pub fn from_env() -> Result<Self, TitleGeneratorError> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let (auth_token, auth_scheme) = resolve_auth_from_env().ok_or(TitleGeneratorError::Auth)?;
        let model = std::env::var("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let timeout_secs = std::env::var("AGENTIC_TITLE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let mut gen = Self::with(
            base_url,
            auth_token,
            auth_scheme,
            model,
            Duration::from_secs(timeout_secs),
        );
        // The OAuth access token rotates in place; re-read it per request so a
        // long-running server doesn't start 401ing once the startup token expires.
        gen.oauth_reread = auth_scheme == AuthScheme::Oauth;
        Ok(gen)
    }
}

/// Resolve the Anthropic auth token + scheme from env, first match wins:
/// `ANTHROPIC_AUTH_TOKEN` (Bearer) → `ANTHROPIC_API_KEY` (x-api-key) →
/// `~/.claude/.credentials.json` OAuth token (Bearer + oauth beta). `None` if no source is
/// available.
pub(crate) fn resolve_auth_from_env() -> Option<(String, AuthScheme)> {
    let non_empty = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    non_empty("ANTHROPIC_AUTH_TOKEN")
        .map(|t| (t, AuthScheme::Bearer))
        .or_else(|| non_empty("ANTHROPIC_API_KEY").map(|k| (k, AuthScheme::ApiKey)))
        .or_else(|| read_oauth_token().map(|t| (t, AuthScheme::Oauth)))
}

/// Read the OAuth access token from `~/.claude/.credentials.json`.
/// Returns `None` if the file is missing or the token field is absent.
pub(crate) fn read_oauth_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home)
        .join(".claude")
        .join(".credentials.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(|s| s.to_string())
}

/// Pull the first `text` block out of an Anthropic `/v1/messages` response.
pub(crate) fn extract_text(body: &serde_json::Value) -> Option<String> {
    body.get("content")?
        .as_array()?
        .iter()
        .find_map(|c| {
            if c.get("type")?.as_str()? == "text" {
                c.get("text")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
}

#[async_trait]
impl TitleGenerator for AnthropicHttpTitleGenerator {
    async fn generate(
        &self,
        prompt: &str,
        _cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError> {
        let body = json!({
            "model": self.model,
            "max_tokens": 32,
            "system": TITLE_SYSTEM_PROMPT,
            "messages": [{"role": "user", "content": prompt}],
        });
        let raw = match self.post(&body).await {
            Ok(s) => s,
            // Silent fallback: collapse every error to Ok(None) per the
            // spec. The caller (engine) keeps the user's original prompt
            // as the title.
            Err(_) => return Ok(None),
        };
        // Accept only a structurally valid title that actually contains Han —
        // the system prompt requires Chinese, so Latin-only output means the
        // model ignored it. Shared with maybe_retitle so both paths agree.
        if !is_accepted_generated_title(&raw) {
            return Ok(None);
        }
        Ok(Some(raw))
    }

    async fn maybe_retitle(
        &self,
        current_title: &str,
        recent_messages: &[(String, String)],
        _cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError> {
        let msgs_json: Vec<serde_json::Value> = recent_messages
            .iter()
            .map(|(r, t)| json!([r, t]))
            .collect();
        let user_content = json!({
            "currentTitle": current_title,
            "messages": msgs_json,
        });
        let body = json!({
            "model": self.model,
            "max_tokens": 64,
            "system": RETITLE_SYSTEM_PROMPT,
            "messages": [{"role": "user", "content": user_content.to_string()}],
        });
        let raw = match self.post(&body).await {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        // The model may wrap the JSON in a markdown ```json fence or add prose
        // despite the system prompt (especially a less prompt-faithful gateway
        // model). Isolate the outermost {...} object span before parsing.
        // Reject change=false (no change) and dedup against the current title.
        let json_span = match (raw.find('{'), raw.rfind('}')) {
            (Some(a), Some(b)) if b > a => &raw[a..=b],
            _ => return Ok(None),
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_span) else {
            return Ok(None);
        };
        let change = v.get("change").and_then(|c| c.as_bool()).unwrap_or(false);
        if !change {
            return Ok(None);
        }
        let Some(new_title) = v.get("title").and_then(|t| t.as_str()) else {
            return Ok(None);
        };
        if new_title == current_title {
            return Ok(None);
        }
        if !is_accepted_generated_title(new_title) {
            return Ok(None);
        }
        Ok(Some(new_title.to_string()))
    }
}

impl AnthropicHttpTitleGenerator {
    /// POST a body to `${base_url}/v1/messages` and return the first text block. Thin wrapper over
    /// the shared [`anthropic_messages`] transport.
    async fn post(&self, body: &serde_json::Value) -> Result<String, TitleGeneratorError> {
        anthropic_messages(
            &self.client,
            &self.base_url,
            &self.auth_token,
            self.auth_scheme,
            self.oauth_reread,
            body,
        )
        .await
    }
}

/// Low-level POST to `${base_url}/v1/messages`, returning the first text block from the response.
/// Maps reqwest errors and HTTP failures to the appropriate [`TitleGeneratorError`] variant; logs
/// non-2xx with the upstream error body.
///
/// `auth_token` is the startup/env token; when `oauth_reread` is true (the OAuth path) the freshest
/// token on disk is preferred per call, since the CLI rotates it in place.
pub(crate) async fn anthropic_messages(
    client: &reqwest::Client,
    base_url: &str,
    auth_token: &str,
    auth_scheme: AuthScheme,
    oauth_reread: bool,
    body: &serde_json::Value,
) -> Result<String, TitleGeneratorError> {
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let fresh = if oauth_reread { read_oauth_token() } else { None };
    let token: &str = fresh.as_deref().unwrap_or(auth_token);
    let mut builder = client
        .post(&url)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json");
    builder = match auth_scheme {
        AuthScheme::ApiKey => builder.header("x-api-key", token),
        AuthScheme::Bearer => builder.header("authorization", format!("Bearer {token}")),
        AuthScheme::Oauth => builder
            .header("authorization", format!("Bearer {token}"))
            .header("anthropic-beta", "oauth-2025-04-20"),
    };
    let req = builder
        .json(body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                TitleGeneratorError::Timeout
            } else {
                TitleGeneratorError::Http(e.to_string())
            }
        })?;

    let status = req.status();
    if !status.is_success() {
        let code = status.as_u16();
        // Consume + log the error body so a failed call has a WHY, not just a bare code.
        let body_snippet: String = req.text().await.unwrap_or_default().chars().take(512).collect();
        tracing::warn!(
            "[anthropic] HTTP {code} from {base_url} (scheme={auth_scheme:?}): {body_snippet}"
        );
        if code == 401 || code == 403 {
            return Err(TitleGeneratorError::Auth);
        }
        return Err(TitleGeneratorError::Http(format!("status {code}")));
    }
    let body: serde_json::Value = req.json().await.map_err(|_| TitleGeneratorError::InvalidResponse)?;
    extract_text(&body).ok_or(TitleGeneratorError::InvalidResponse)
}

/// Test-only `TitleGenerator` that returns a fixed response configured
/// at construction. Both fields are non-one-shot by default — they keep
/// returning their configured value — so a single generator can drive
/// both the submit-time title and the 5th-turn retitle in the same test.
pub struct InMemoryTitleGenerator {
    /// `generate` response. `None` means "no title for this call".
    pub next_generate: std::sync::Mutex<Option<String>>,
    /// `maybe_retitle` response. Outer `None` = "generator says no
    /// opinion yet" (returns Ok(None)). Outer `Some(None)` = "model
    /// returned `{\"change\":false}`" (also Ok(None)). Outer
    /// `Some(Some(s))` = "model returned change=true with title s".
    pub next_retitle: std::sync::Mutex<Option<Option<String>>>,
}

impl InMemoryTitleGenerator {
    /// Build a generator that returns `title` on every `generate` call.
    /// `maybe_retitle` always returns `Ok(None)`.
    pub fn returning_title(title: impl Into<String>) -> Self {
        Self {
            next_generate: std::sync::Mutex::new(Some(title.into())),
            next_retitle: std::sync::Mutex::new(None),
        }
    }

    /// Build a generator that returns `Some(new_title)` on every
    /// `maybe_retitle` call (None of the optional means the model
    /// declined the change). `generate` always returns `Ok(None)`.
    pub fn returning_retitle(new_title: Option<impl Into<String>>) -> Self {
        Self {
            next_generate: std::sync::Mutex::new(None),
            next_retitle: std::sync::Mutex::new(Some(new_title.map(Into::into))),
        }
    }

    /// Convenience: `returning_retitle(None)`.
    pub fn returning_none_for_retitle() -> Self {
        Self::returning_retitle(None::<String>)
    }

    /// Override the generate response in place (used after construction
    /// for two-stage tests that need to swap the response mid-run).
    pub fn set_generate(&self, title: Option<String>) {
        *self.next_generate.lock().unwrap() = title;
    }

    /// Override the retitle response in place.
    pub fn set_retitle(&self, new_title: Option<Option<String>>) {
        *self.next_retitle.lock().unwrap() = new_title;
    }
}

#[async_trait]
impl TitleGenerator for InMemoryTitleGenerator {
    async fn generate(
        &self,
        _prompt: &str,
        _cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError> {
        Ok(self.next_generate.lock().unwrap().take())
    }

    async fn maybe_retitle(
        &self,
        _current: &str,
        _recent: &[(String, String)],
        _cwd: &Path,
    ) -> Result<Option<String>, TitleGeneratorError> {
        Ok(self.next_retitle.lock().unwrap().take().flatten())
    }
}