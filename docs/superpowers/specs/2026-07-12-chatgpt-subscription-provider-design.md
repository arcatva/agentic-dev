# ChatGPT subscription (OAuth) provider — design

Date: 2026-07-12
Repos: `agentic-dev` (Rust backend), `agentic-dev-android` (Kotlin/Compose client)

## Goal

Let a user connect their ChatGPT subscription (OAuth login, not an API key) so GPT models
enter the platform's model catalog and can be routed to by `delegate`. GPT is a **delegate
worker**, never the main-session model (main sessions are Claude Agent SDK only).

## Confirmed background (from the request, not re-derived)

- Providers live in `server-rs/src/engine/model/providers.rs`. `openai`-protocol providers are
  called through a backend-supervised LiteLLM proxy (`engine/model/litellm.rs`) that translates
  Anthropic↔OpenAI. The proxy reads real keys from env at spawn and is regenerated + restarted on
  `litellm::request_reload()` after any provider CRUD.
- `GET /api/models` (`api/misc.rs::models_get`, no scope) already lists native Claude tiers **plus
  every registered provider** via `full_model_entries()` → so a provider in `providers.json`
  appears in the catalog for free.
- The delegate candidate filter (`engine/workflow/delegate.rs`) keeps an openai provider only when
  `!resolved_key().is_empty()` **and** `litellm::available()`.
- Android already has provider/model management UI in `feature/providers`, data in
  `core/data ProvidersRepository` + `core/network AgenticApi`.
- ChatGPT OAuth (Codex CLI shape, mid-2026): Authorization Code + PKCE(S256). authorize
  `https://auth.openai.com/oauth/authorize`, token `https://auth.openai.com/oauth/token`,
  client_id `app_EMoamEEZ73f0CkXaXp7hrann`, redirect `http://localhost:1455/auth/callback`,
  scopes `openid profile email offline_access`. Call endpoint
  `POST https://chatgpt.com/backend-api/codex/responses` (Responses API, forced streaming) with
  headers `Authorization: Bearer <access>`, `ChatGPT-Account-Id: <jwt chatgpt_account_id>`,
  `originator: codex_cli_rs`, `OpenAI-Beta: responses=experimental`. Access token is a short-lived
  JWT refreshed with the refresh_token; refresh can fail → re-login.

## Key constraint that shapes the whole design

The redirect URI `http://localhost:1455/auth/callback` is **fixed** (registered with OpenAI's
OAuth app — we cannot change it). So the OAuth callback must land on a loopback listener on
port 1455. Token exchange, storage, and refresh must happen **server-side** (the server owns the
refresh_token and the rotating access token). Therefore:

- The **agentic-dev server** runs a one-shot loopback listener on `127.0.0.1:1455` during login and
  performs the code→token exchange itself.
- **Co-location assumption:** the OAuth consent must be completed in a browser that can reach the
  server's `localhost:1455` — i.e. a browser on the server host (the normal self-hosted case: server
  on your laptop/desktop). The Android app *initiates* login and *monitors* status; it surfaces the
  authorize URL (open it on the server host) rather than trying to catch the redirect on the phone.
  This is the same model Codex CLI uses. Documented, not worked around.

## Architecture

### New engine module: `server-rs/src/engine/model/openai_oauth.rs` (HTTP-independent, unit-tested)

The real, testable core. No axum imports. Responsibilities:

1. **PKCE**: `code_verifier` (random, 43+ chars from concatenated `uuid` v4 — no new dep) and
   `code_challenge = base64url_nopad(sha256(verifier))` (deps `sha2`, `base64` already present).
2. **Authorize URL**: build with `reqwest::Url::parse_with_params` (reqwest re-exports `url`) — no
   new dep, correct percent-encoding. Params: response_type=code, client_id, redirect_uri, scope,
   code_challenge, code_challenge_method=S256, state.
3. **Token exchange / refresh**: `reqwest::blocking` form POST to the token endpoint (grant_type
   authorization_code / refresh_token). Returns `{access_token, refresh_token, id_token, expires_in}`.
4. **JWT claim parse**: split on `.`, base64url-decode the payload, extract `chatgpt_account_id`
   (checked top-level and nested under `https://api.openai.com/auth`). Tried on access_token then
   id_token. Pure fn over a token string — directly unit-testable with a hand-built fake JWT.
5. **Token store** `~/.agentic-dev/openai-oauth.json`, mode **0600**, atomic write (temp+rename,
   mirrors `providers::save_list_to`). Struct `StoredToken { access_token, refresh_token, id_token,
   account_id, expires_at }` (`expires_at` = unix secs). Path is injectable via a test override
   static (mirrors `PROVIDERS_FILE_OVERRIDE`) so tests never touch the real home dir.
6. Helpers: `current_access_token() -> Option<String>` (the store's token, empty→None),
   `account_id()`, `expires_at()`, `needs_reauth()` state, `is_expired(now)`, and
   `refresh_if_needed()` (refresh when within a skew window of expiry; on refresh failure set a
   `needs_reauth` marker in the store and return an error).

**The token is never written to `providers.json`** (criterion 1): the provider entry there carries
no key; the credential lives only in the 0600 store.

### Provider wiring (`providers.rs`) — no struct field added (avoid 20-literal churn)

The single well-known subscription provider is identified by a reserved **name constant**
`OPENAI_SUBSCRIPTION_PROVIDER = "chatgpt"`. Add two small methods:

- `Provider::is_subscription(&self) -> bool` — `name.eq_ignore_ascii_case(OPENAI_SUBSCRIPTION_PROVIDER)`.
- `Provider::effective_key(&self) -> String` — subscription → `openai_oauth::current_access_token()`
  (the rotating bearer); otherwise the existing `resolved_key()`.

Replace the three call sites that must be subscription-aware with `effective_key()`:
- delegate candidate filter (`!resolved_key().is_empty()` → `!effective_key().is_empty()`),
- litellm `build_config` key read,
- `provider_view.has_key` (so the UI reflects "connected").
`resolved_key()` stays pure everywhere else (router probe etc.).

### LiteLLM config (`litellm.rs::build_config`)

For a subscription openai provider, emit the model_list entry pointing `api_base` at the codex
backend and add the required headers:
```yaml
- model_name: "gpt-5"
  litellm_params:
    model: "openai/gpt-5"
    api_base: "https://chatgpt.com/backend-api/codex"
    api_key: os.environ/AGENTIC_LITELLM_KEY_N     # rotating bearer, injected at spawn
    extra_headers:
      ChatGPT-Account-Id: "<account_id from store>"
      originator: "codex_cli_rs"
      OpenAI-Beta: "responses=experimental"
```
The key comes from `effective_key()` (current access token). **Rotating bearer**: the startup
refresh task (below) calls `litellm::request_reload()` after each refresh → config regenerated with
the new token → proxy restarted. `ponytail:` proxy restart per refresh drops in-flight requests;
fine for single-user, upgrade to hot key reload if throughput matters.

`ponytail:` the codex endpoint uses the Responses API (`/responses`, forced streaming), which
differs from standard `/chat/completions`. The config carries the correct base_url, rotating bearer,
and headers; whether LiteLLM's default openai translation targets the codex `/responses` shape is
the one integration point needing a live check (untestable here — tests use the fake bridge, no API
cost). Upgrade path: a custom LiteLLM route/provider if the default shape mismatches. Noted, scoped.

### API layer (`api/misc.rs` + routes in `api/mod.rs`)

- `POST /api/providers/openai-subscription/login` — generate PKCE+state, start the one-shot
  `127.0.0.1:1455` loopback listener (temporary axum Router with oneshot shutdown), return
  `{ authorize_url }`. The listener, on `GET /auth/callback?code=&state=`: verify state, exchange
  code, parse account_id, write the 0600 store, `upsert` the `chatgpt` provider into providers.json
  (protocol=openai, base_url=codex backend, model default `gpt-5`, empty key, capability/cost
  sensible defaults), `litellm::request_reload()`, respond to the browser with a success HTML page,
  then shut the listener down. A login timeout (a few minutes) tears the listener down if abandoned.
- `GET /api/providers/openai-subscription/status` — `{ connected, account_id, expires_at,
  needs_reauth }` from the store. Source of truth for the client's status chip.
- `POST /api/providers/openai-subscription/logout` — delete the store + remove the `chatgpt`
  provider + reload. (Small, symmetric, cheap.)

### Startup (`main.rs`)

Spawn a background refresh task alongside `litellm::start_supervisor()`: periodically
`refresh_if_needed()`; on successful refresh call `litellm::request_reload()`. On refresh failure it
marks `needs_reauth` (surfaced by the status endpoint) and keeps looping (a later manual re-login
fixes it).

### Android (`agentic-dev-android`)

- `core/network AgenticApi` + `KtorAgenticApi`: `startSubscriptionLogin(): SubscriptionLogin`
  (`{authorizeUrl}`), `subscriptionStatus(): SubscriptionStatus`
  (`{connected, accountId, expiresAt, needsReauth}`), `subscriptionLogout()`. DTOs in `data/net`.
  Add matching defaults to the `FakeAgenticApi` test fake.
- `core/data ProvidersRepository`: pass-through methods.
- `feature/providers ProvidersViewModel` + `ProvidersScreen`: a "ChatGPT subscription" card at the
  top — **Connect** button (opens `authorizeUrl` via `LocalUriHandler`, then polls status until
  `connected`), status line (account / expiry / "needs re-login"), **Disconnect**. On connect
  success, refresh the provider + model list so GPT shows up.

## Data flow (happy path)

1. App → `POST …/login` → server makes PKCE+state, opens `:1455` listener, returns authorize URL.
2. User completes OAuth in a server-host browser → OpenAI redirects to `:1455/auth/callback?code`.
3. Listener exchanges code → tokens, parses account_id, writes 0600 store, upserts `chatgpt`
   provider, reloads LiteLLM, shows success page.
4. App polls `…/status` → `connected` → refreshes models → GPT appears (also in `GET /api/models`).
5. `delegate` routes a task to `chatgpt`: candidate passes the filter (effective_key non-empty),
   runs via LiteLLM with the rotating bearer + codex headers.
6. Background task refreshes the token before expiry and reloads LiteLLM (rotating bearer).

## Error handling

- Token store missing/corrupt → treated as "not connected" (never panics; parse errors → None).
- State mismatch on callback → 400, listener stays until timeout for a valid one.
- Refresh failure → `needs_reauth` marker; status endpoint reports it; delegate filter naturally
  drops the provider once `current_access_token()` is empty/expired-unrefreshable.
- Corrupt providers.json → existing load/save guards already refuse to wipe it.

## Testing (all via fake bridge / no API cost — criterion 5)

Rust unit tests (in-crate, hermetic with injected store path + `seed_claude_models_for_tests`):
- PKCE challenge = base64url_nopad(sha256(verifier)); known vector.
- JWT account_id extraction (top-level + nested claim; malformed → None).
- Token store roundtrip; file mode 0600; atomic; corrupt → None.
- `is_expired` / `refresh_if_needed` decision boundary (no network — inject a fake token-exchange
  fn, or test the pure decision separately from the HTTP call).
- `effective_key()` returns the store token for the `chatgpt` provider, empty when store empty.
- `build_config` emits the codex api_base + the three headers + env key for a subscription provider.
- `full_model_entries()` includes the `chatgpt` provider (catalog membership — criterion 2).
- delegate candidate filter keeps `chatgpt` when the store has a token, drops it when empty.

Android: DTO (de)serialization + ViewModel state transitions against `FakeAgenticApi`; `assemble`
compiles (criterion 5: "at least compiles").

## Out of scope (YAGNI)

- Multiple simultaneous ChatGPT accounts (one subscription provider; re-login replaces it).
- Hot LiteLLM key reload without restart (restart-on-refresh is enough for single-user).
- Catching the OAuth redirect on the phone (fixed redirect_uri makes server-side loopback the
  correct place; documented co-location assumption).
- Making GPT selectable as a main-session model (explicitly forbidden — worker only).

## PR plan

Two PRs (one per repo — they version independently and the backend must land first for the client
to have endpoints to call). Both **non-draft**, then stop (benchmark arm — no self-merge).
