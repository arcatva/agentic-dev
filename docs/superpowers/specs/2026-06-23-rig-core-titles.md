# Title/Retitle via rig-core — design

Date: 2026-06-23
Status: draft (awaiting user review)
Repo: `agentic-dev` (server-rs)
Supersedes: the Node+`sdk-bridge.mjs`+`title_bin` path on master; this design
replaces it end-to-end so title/retitle no longer requires a Node runtime
or a separate `node_modules/` directory. Uses the `rig-core` crate's
Anthropic provider for the HTTP call, sharing the Anthropic request
shape with rig's existing provider abstractions.

## Problem

`Engine::submit_session` and `Engine::follow_up` both want to call Anthropic
haiku to summarise the user's first prompt into a 5–12 character Chinese
title (submit) or to decide whether the current title still fits
(5th-turn retitle). The existing implementation:

- Spawns `node sdk-bridge.mjs` from Rust via `tokio::process::Command`.
- Requires the SDK bridge to be deployed **next to** the binary and
  with a working `node_modules/@anthropic-ai/claude-agent-sdk/`.
- Reads the Anthropic auth from `~/.claude/.credentials.json` via
  the SDK (OAuth refresh handled there).

For this single-site use (a one-shot haiku call with no tool use, no
streaming, no `canUseTool` protocol) the entire Node toolchain is
over-engineered. In a ccswitch environment (custom
`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`) the OAuth path is
also wrong: we want the same ccswitch-routed endpoint the rest of
the system uses, not the OAuth refresh that the SDK performs
internally.

## Goal

Replace the Node bridge with a Rust-only HTTP client that posts
`/v1/messages` to whatever `ANTHROPIC_BASE_URL` is configured
(preserving the ccswitch routing), authenticates with
`ANTHROPIC_AUTH_TOKEN` (or falls back to the OAuth token from
`~/.claude/.credentials.json` if the env var is unset), and reads
the model name from `ANTHROPIC_DEFAULT_HAIKU_MODEL` (already set
by ccswitch). All title/retitle traffic leaves the binary as a
single file; `node` and `npm` are no longer required to deploy.

## Non-goals

- Switching the main turn runner (Claude Code SDK) to a Rust client.
  The main loop's `canUseTool` + AskUserQuestion handling is a
  different problem and stays on the Node SDK.
- Streaming responses. Title and retitle are both single-shot text.
- A "rename from Android" UI. The existing `setTitle=true` API
  escape hatch is preserved unchanged.
- A separate `summary` field for session-list previews. Title alone
  is the spec.

## Design

### High level

Replace the `title_bridge.rs` Node-spawning module with a new
`title_client.rs` that calls Anthropic's `/v1/messages` HTTP API
via the `rig-core` crate's Anthropic provider. Wrap it in a
`TitleGenerator` trait so the rest of the engine talks to one
interface regardless of the underlying transport; production wires
the real client, tests wire a fake.

**Note on rig-core's exact Anthropic API**: rig-core's Anthropic
client signature may have changed between versions. The plan assumes
the modern form (e.g. `rig::providers::anthropic::Client::new(api_key,
base_url)` or `Client::from_env()`); if the actual API differs,
Task 2 of the plan has an early **compile check** step that pins
the right call. We pin to `rig-core = "0.x"` (latest) and document
the version in `Cargo.toml`.

### `TitleGenerator` trait

```rust
#[async_trait::async_trait]
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
```

`generate` returns `Some(title)` only on a valid 5–12 char Chinese
title that passes `is_valid_title`. `maybe_retitle` returns `Some(new)`
only when the model says `change=true` AND the new title differs
from the current one AND passes `is_valid_title`. Every other
path returns `Ok(None)` (silent fallback per the spec).

### `AnthropicHttpTitleGenerator` impl

```rust
pub struct AnthropicHttpTitleGenerator {
    client: reqwest::Client,
    base_url: String,   // e.g. "https://api.minimaxi.com/anthropic"
    auth_token: String, // ANTHROPIC_AUTH_TOKEN
    model: String,      // ANTHROPIC_DEFAULT_HAIKU_MODEL
    timeout: Duration,
}
```

Constructor reads env in this order:

1. `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`)
2. `ANTHROPIC_AUTH_TOKEN` (preferred)
3. else read `~/.claude/.credentials.json`, take
   `claudeAiOauth.accessToken`
4. else error at startup (the existing preflight pattern)
5. `ANTHROPIC_DEFAULT_HAIKU_MODEL` (default `claude-haiku-4-5`)
6. `AGENTIC_TITLE_TIMEOUT` seconds (default 20)

The constructor runs once at boot. The OAuth token from step 3
is **not** refreshed — title calls are short-lived enough that the
default access-token lifetime (~hours) is plenty for the lifetime
of one agentic-dev-server process. If a title call returns 401
once, we return `Ok(None)` and log a warning. Adding a refresh
loop is out of scope for v1.

### HTTP shape

`generate` posts a `/v1/messages` body of:

```json
{
  "model": "<haiku model>",
  "max_tokens": 32,
  "system": "<fixed Chinese system prompt>",
  "messages": [{"role": "user", "content": "<the user prompt>"}]
}
```

The system prompt (carried over from `sdk-bridge.mjs`):

> 你的任务是根据用户给出的请求,生成一个会话标题。要求:
> - 用中文,5 到 12 个字
> - 只输出标题本身,不要任何解释、不要标点包裹、不要 markdown
> - 标题要能反映"用户在做什么",而不是"用户最后一句话"
> - 如果用户输入很短(比如 "ok"),用会话的整体意图来概括

`maybe_retitle` uses a different system prompt that asks the model
to return `{"change": false}` or `{"change": true, "title": "..."}`,
and posts a `currentTitle` + `messages` array as the user content
(compact JSON, kept under 4 KiB by `parse_recent_messages`).

Response parsing reuses the existing `is_valid_title` +
`parse_recent_messages` helpers in `engine::title`. No change
needed there.

### Engine wiring

`EngineConfig` swaps `title_bridge: Arc<TitleBridge>` for
`title_generator: Arc<dyn TitleGenerator>`. Tests in
`engine::tests::submit_titles_via_bridge` are renamed to
`engine::tests::submit_titles_via_generator` and instantiate an
`InMemoryTitleGenerator` instead of the bash-script fakes.

`submit_session` and `follow_up` change one line: they call
`self.0.cfg.title_generator.generate(...)` instead of
`self.0.cfg.title_bridge.generate(...)`. Both paths remain
fire-and-forget via `tokio::spawn`.

### Deleted surface

- `server-rs/src/engine/title_bridge.rs` (whole file, ~250 lines)
- `server-rs/tests/fixtures/fake-sdk-bridge-*.sh` (4 fakes)
- `EngineConfig.title_bridge: Arc<TitleBridge>` field
- `Config.title_bin`, `Config.sdk_bridge_path` env-derived fields
- `Config.node_bin` (no Node)
- `AGENTIC_NODE_BIN`, `AGENTIC_TITLE_BIN`, `AGENTIC_SDK_BRIDGE` env
- `server-rs/sdk-bridge.mjs` (whole file, ~250 lines, kept on disk
  for now but unused — see Risks)
- `server-rs/package.json` + `server-rs/node_modules/`
- `Makefile` deps + bridge-staging `cp` lines

### Test surface

`engine::title_client` unit tests use `wiremock` (or
`httpmock`; pick whichever has fewer deps) to stub the
`/v1/messages` endpoint and assert that the right body shape goes
out + the right parse happens. One test per failure mode:
auth-missing-on-boot, timeout, 4xx, 5xx, empty content,
invalid-title-from-model, etc.

Existing fake-bash tests in `engine::title_bridge` are deleted
together with the module.

## Risks

- **ccswitch model `MiniMax-M3` behaviour**: this is not a
  Claude model. Whether it produces 5–12 char Chinese titles
  reliably is an empirical question. Plan B if it doesn't:
  pin `ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-4-5` and
  override `ANTHROPIC_BASE_URL` to a ccswitch profile that
  passes through to real Anthropic.
- **OAuth token expiry**: title calls fail silently when the
  access token expires (no refresh). Acceptable for v1: title
  generation is a nice-to-have, the original prompt is the
  fallback, and the user can always rename via `setTitle=true`.
- **Stale `sdk-bridge.mjs` on disk**: deleted functionally, file
  kept around for one release in case rollback is needed. Delete
  in a follow-up commit once v2 is verified in production.
- **First-call cold start**: reqwest + a fresh TLS handshake
  adds ~100–300 ms to the first title call. Negligible compared
  to the haiku latency (~2–5 s).
