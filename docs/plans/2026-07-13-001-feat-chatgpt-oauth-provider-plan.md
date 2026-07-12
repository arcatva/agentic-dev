---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
execution: code
product_contract_source: ce-plan-bootstrap
title: "feat: Connect ChatGPT subscription via OAuth as a delegate-routable provider"
date: 2026-07-13
type: feat
---

# feat: Connect ChatGPT subscription via OAuth as a delegate-routable provider

**Target repos:** `agentic-dev` (Rust backend) and `agentic-dev-android` (Kotlin/Compose client) — both checked out in this session workspace. Paths below are repo-relative; each unit names its repo.

Product Contract preservation: no upstream brainstorm; this plan is the source of scope (`ce-plan-bootstrap`).

---

## Summary

Let a user connect their personal ChatGPT subscription with an OAuth login (Authorization Code + PKCE) instead of an API key. On success the platform registers a `chatgpt` provider (`protocol: openai`) whose bearer is the OAuth access token, persists the tokens in a permission-tightened file separate from the human-editable `providers.json`, refreshes them before expiry, and routes GPT calls through the **existing** LiteLLM proxy using LiteLLM's native `chatgpt/` provider (which speaks the ChatGPT Responses API and injects the required headers). GPT then appears in `GET /api/models` and is routable by `delegate`. The Android app gains a "Connect ChatGPT" entry that drives the browser login, shows connection status, and refreshes the model list.

---

## Problem Frame

The platform's delegate router already fans work out across registered BYOK providers (Anthropic-protocol called directly; OpenAI-protocol proxied through a supervised LiteLLM sidecar). Today a provider authenticates only with a static `api_key`/`api_key_env`. A ChatGPT *subscription* has no API key — it authenticates with a short-lived OAuth JWT that must be refreshed, is called at a non-standard endpoint (`https://chatgpt.com/backend-api/codex/responses`, the Responses API) with custom headers (`ChatGPT-Account-Id`, `originator`, `OpenAI-Beta`). The gap: (1) no OAuth login/persist/refresh machinery, (2) the LiteLLM config generator only emits static-key `openai/<model>` stanzas and skips keyless providers, (3) no client UI to log in or see status.

---

## Requirements

- **R1** Register a ChatGPT subscription provider via an OAuth flow (Authorization Code + PKCE S256); no API key entered by the user.
- **R2** Persist OAuth tokens in a file with tightened permissions (0600), **separate** from the human-editable `providers.json` — the plaintext token never lands in `providers.json`.
- **R3** Auto-refresh the access token before expiry using the refresh token; surface "needs re-login" when the refresh token is dead.
- **R4** GPT model(s) appear in `GET /api/models` default scope.
- **R5** GPT is routable through the existing LiteLLM/delegate path (openai protocol + rotating bearer).
- **R6** Android: login entry + provider status (account / expiry / needs-relogin); after login the model list refreshes and shows GPT.
- **R7** `cargo build` + `make test` green (tests run through the fake bridge, no API cost); Android at least compiles.

---

## Key Technical Decisions

- **KTD1 — Route through LiteLLM's native `chatgpt/` provider, do not reimplement the Responses API.** The delegate worker (headless `claude`) only speaks Anthropic `/v1/messages` to the proxy; only LiteLLM can translate Anthropic → ChatGPT Responses. LiteLLM ships a `chatgpt/` provider (`model: chatgpt/<model>`, `model_info: {mode: responses}`) that reads OAuth tokens from `CHATGPT_AUTH_FILE` and injects `ChatGPT-Account-Id`/`originator`/`OpenAI-Beta`. We write the token file it reads. (Reuse over reinvention — we could not correctly hand-build the translation.)
- **KTD2 — Server owns the OAuth; token materialized into LiteLLM's auth file.** We run PKCE login, persist, and refresh server-side (testable, single source of truth). The persisted file **is** the Codex/LiteLLM-format `auth.json` LiteLLM consumes (`{ "OPENAI_API_KEY": null, "tokens": {id_token, access_token, refresh_token, account_id}, "last_refresh": <ms> }`), so there is one file, not two. Our refresher keeps the token valid ahead of expiry so LiteLLM rarely needs to refresh itself (documented ceiling). Location: `$AGENTIC_CHATGPT_AUTH_FILE`, default `~/.agentic-dev/chatgpt-auth.json`.
- **KTD3 — Provider marker field, token stays out of `providers.json`.** Add `#[serde(default)] chatgpt_oauth: bool` to `Provider`. The `providers.json` entry carries only the marker + model + protocol (no secret). `build_config()` emits a `chatgpt/<model>` stanza for a `chatgpt_oauth` provider even though its `api_key` is empty, and points `CHATGPT_TOKEN_DIR`/`CHATGPT_AUTH_FILE` at our file.
- **KTD4 — No new crates.** PKCE verifier from two `uuid::Uuid::new_v4()` simple hex (64 unreserved chars); `code_challenge = base64url_nopad(sha256(verifier))` via existing `base64` + `sha2`. `account_id` by base64url-decoding the JWT payload segment and reading `chatgpt_account_id` (top-level or under the `https://api.openai.com/auth` claim) — no signature verification (TLS-trusted token endpoint), no `jsonwebtoken` crate. HTTP via existing `reqwest` blocking client (mirrors `fetch_claude_models`).
- **KTD5 — Reuse the atomic writer.** Add `write_file_atomic_mode(path, content, mode)` beside `write_file_atomic` (same fsync-tmp-then-rename, plus `set_permissions(mode)` on the tmp before rename). Do not hand-roll a second atomic writer, and do not drop the fsync.
- **KTD6 — On-device loopback for the fixed redirect.** `redirect_uri` is fixed to `http://localhost:1455/auth/callback` and the browser runs on the Android device, so the app runs a one-shot `ServerSocket` on `127.0.0.1:1455` to catch `?code&state`, then POSTs to the server's complete endpoint. No new dependency (raw socket; `INTERNET` permission already present). Browser opened via `Intent.ACTION_VIEW` (no Custom Tabs dependency).

---

## High-Level Technical Design

Login sequence (numbers are the two server round-trips the client makes):

```mermaid
sequenceDiagram
    participant A as Android app
    participant L as loopback 127.0.0.1:1455
    participant S as agentic-dev server
    participant O as auth.openai.com
    A->>S: POST /api/providers/chatgpt/login/start
    S->>S: gen PKCE (verifier,challenge,state); stash state→verifier (TTL)
    S-->>A: { authorize_url, state }
    A->>L: bind ServerSocket :1455 (one-shot)
    A->>O: ACTION_VIEW authorize_url (system browser)
    O-->>L: GET /auth/callback?code&state
    L-->>A: code, state (then 200 "you can close this tab")
    A->>S: POST /api/providers/chatgpt/login/complete { code, state }
    S->>S: pop verifier by state
    S->>O: POST /oauth/token (code, verifier, redirect, client_id)
    O-->>S: { access_token, refresh_token, id_token, expires_in }
    S->>S: account_id = jwt(access_token or id_token).chatgpt_account_id
    S->>S: write chatgpt-auth.json (0600); upsert chatgpt provider; litellm reload; spawn refresher
    S-->>A: { account_id, expires_at }
    A->>S: refresh providers + GET /api/models  (GPT now present)
```

Routing at call time (existing path, unchanged except the config stanza): delegate picks the `chatgpt` provider → worker env overlay points `ANTHROPIC_BASE_URL` at the local LiteLLM proxy → LiteLLM's `chatgpt/<model>` provider reads the access token from `chatgpt-auth.json` and calls the ChatGPT Responses API with the required headers.

---

## Scope Boundaries

In scope: OAuth login/persist/refresh, provider registration, LiteLLM config emission, model catalog exposure, Android login/status/refresh, tests, two PRs.

Out of scope / **Deferred to Follow-Up Work**:
- Multiple ChatGPT accounts (one `chatgpt` provider). 
- Registering multiple GPT model ids at once (default one, `AGENTIC_CHATGPT_MODEL`).
- A device-code fallback login (we use the PKCE loopback the Android app can drive).
- Making the LiteLLM binary present/installed — unchanged; `available()` already gates the sidecar. When absent, login still persists tokens and the provider still lists, but calls won't route (same behavior as today's openai providers without the binary).

---

## Assumptions

- LiteLLM's `chatgpt/` provider reads the Codex-shaped `auth.json` (`tokens.{access_token,refresh_token,id_token,account_id}`). If the exact key layout differs in the deployed LiteLLM, the auth-file writer (`chatgpt_oauth::write_auth_file`) is the single place to adjust; mark it with a `ponytail:` comment. CI cannot exercise a live ChatGPT call (fake bridge), so this is the one seam verified by shape, not by a live request.
- The `chatgpt_account_id` claim is present in the access or id token JWT (Codex mid-2026 shape). If absent, `account_id` is `None`; login still succeeds and status shows connected without an account label.

---

## Implementation Units

### U1. Add `write_file_atomic_mode` (reuse the atomic writer)

**Repo:** `agentic-dev`  
**Goal:** One atomic+durable writer that also sets a unix mode, so the token store gets fsync **and** 0600.  
**Requirements:** R2.  
**Dependencies:** none.  
**Files:** `server-rs/src/engine/persistence/atomic_write.rs` (add fn + test).  
**Approach:** Extract the existing tmp→fsync→rename→dir-fsync body; add `write_file_atomic_mode(path, content, mode: u32)` that, on unix, `set_permissions(Permissions::from_mode(mode))` on the tmp file *before* rename (so the file is never briefly world-readable). Keep `write_file_atomic` as a thin caller (mode = inherit/umask) to avoid touching its callers.  
**Test scenarios:**
- Writes then overwrites atomically; no leftover `.tmp` (existing test still passes).
- `write_file_atomic_mode(p, "x", 0o600)` → file contents correct AND `metadata().permissions().mode() & 0o777 == 0o600` (unix only, `#[cfg(unix)]`).

### U2. `chatgpt_oauth` module — PKCE, token exchange/refresh, JWT account id, token store

**Repo:** `agentic-dev`  
**Goal:** All OAuth machinery, pure and testable, no axum.  
**Requirements:** R1, R2, R3, KTD4, KTD2.  
**Dependencies:** U1.  
**Files:** `server-rs/src/engine/model/chatgpt_oauth.rs` (new), register `mod chatgpt_oauth;` in `server-rs/src/engine/model/mod.rs`.  
**Approach:**
- Constants: `CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"`, `REDIRECT_URI = "http://localhost:1455/auth/callback"`, `AUTHORIZE_URL`, `TOKEN_URL`, `SCOPES = "openid profile email offline_access"`.
- `auth_file_path()` → `$AGENTIC_CHATGPT_AUTH_FILE` else `~/.agentic-dev/chatgpt-auth.json`; a test override static (mirror `PROVIDERS_FILE_OVERRIDE`) so tests never touch a real home.
- `gen_pkce() -> Pkce{verifier, challenge}`: verifier = two v4 uuids simple hex; challenge = `base64url_nopad(sha256(verifier))`.
- `authorize_url(challenge, state) -> String` with query params (`response_type=code`, client_id, redirect_uri, scope, code_challenge, `code_challenge_method=S256`, state).
- Pending PKCE map: `static PENDING: parking_lot::Mutex<HashMap<String,(String,u64)>>` (state → (verifier, created_ms)); `begin_login()` returns `(authorize_url, state)` and stores; `take_verifier(state)` pops and rejects if missing or older than TTL (e.g. 10 min).
- `exchange_code(code, verifier) -> io::Result<Tokens>` and `refresh(refresh_token) -> io::Result<Tokens>`: blocking `reqwest` POST (form-encoded) to `TOKEN_URL`; parse `access_token`/`refresh_token`/`id_token`/`expires_in`. Inject a client for tests (fn takes a base-url override via env `AGENTIC_CHATGPT_TOKEN_URL`, so a wiremock server can stand in).
- `account_id_from_jwt(jwt) -> Option<String>`: split on `.`, base64url-decode `[1]`, parse JSON, read `chatgpt_account_id` at top level or under `["https://api.openai.com/auth"]["chatgpt_account_id"]`.
- `Tokens{access_token, refresh_token, id_token, account_id, expires_at_ms}` and `AuthFile` (Codex shape); `load()`/`save()` (save via `write_file_atomic_mode(.., 0o600)`), `expires_at_ms`, `needs_refresh(now_ms, margin_ms)` (true when `now + margin >= expires_at`).  
**Test scenarios (offline — inject env/URL, no live OpenAI):**
- PKCE: challenge == `base64url_nopad(sha256(verifier))` for a fixed verifier (assert against a precomputed value); verifier length in [43,128] and only unreserved chars.
- `account_id_from_jwt`: top-level claim; nested `https://api.openai.com/auth` claim; missing → `None`; malformed base64 / non-JWT → `None`.
- Token store round-trip: `save` then `load` equal; file mode 0600 (`#[cfg(unix)]`).
- `needs_refresh`: false well before expiry; true within margin; true when already expired.
- `take_verifier`: unknown state → `None`; expired state (created older than TTL) → `None`; valid → returns verifier and removes it (second take → `None`).
- `exchange_code`/`refresh` against a `wiremock` token endpoint: happy path parses tokens + `expires_at`; non-200 → `Err`; refresh 400 (dead refresh token) → `Err` (drives needs-relogin at the API layer).

### U3. `Provider.chatgpt_oauth` marker + LiteLLM `chatgpt/` stanza

**Repo:** `agentic-dev`  
**Goal:** Represent the subscription provider and make the proxy config include it.  
**Requirements:** R4 (indirect), R5, KTD3.  
**Dependencies:** U2.  
**Files:** `server-rs/src/engine/model/providers.rs`, `server-rs/src/engine/model/litellm.rs`.  
**Approach:**
- `providers.rs`: add `#[serde(default)] pub chatgpt_oauth: bool` to `Provider` (serde default false → old files load; every struct literal in tests/`from_env_defaults`/`candidates_from` needs the field — grep for `Provider {` and add `chatgpt_oauth: false`). Keep it out of routing logic (it routes as a normal openai provider by name/model).
- `litellm.rs build_config()`: for a provider with `chatgpt_oauth == true` AND a loadable auth file with a non-empty access token, emit a `chatgpt/<model>` stanza with `model_name: <model>`, `litellm_params.model: chatgpt/<model>`, and `model_info.mode: responses` (extend `yaml` writing to support the `model_info` block); do **not** bake a key env var for it (LiteLLM reads the auth file). Set process env for the proxy: `CHATGPT_TOKEN_DIR`=auth-file dir, `CHATGPT_AUTH_FILE`=auth-file name, `CHATGPT_API_BASE`=`https://chatgpt.com/backend-api/codex`, `CHATGPT_ORIGINATOR`=`codex_cli_rs` (append to the returned env pairs). If `chatgpt_oauth` but no valid token file, skip it (same as an empty-key openai provider) and log once.  
**Test scenarios:**
- Round-trip: `Provider` JSON without `chatgpt_oauth` → false; with `true` → true.
- `build_config` (drive via the providers-file + auth-file test overrides): a `chatgpt_oauth` provider with a seeded auth file → config text contains `chatgpt/<model>` and `mode: responses`, and the returned env pairs include `CHATGPT_AUTH_FILE`; with no auth file → stanza absent.
- A normal `openai` provider still emits `openai/<model>` with a baked `AGENTIC_LITELLM_KEY_*` (regression).

### U4. OAuth HTTP endpoints + provider upsert/logout + refresher spawn

**Repo:** `agentic-dev`  
**Goal:** The client-facing API and the glue that registers the provider and starts refresh.  
**Requirements:** R1, R3, R4, R5, R6.  
**Dependencies:** U2, U3.  
**Files:** `server-rs/src/api/misc.rs` (handlers), `server-rs/src/api/mod.rs` (routes), `server-rs/src/engine/model/chatgpt_oauth.rs` (refresher task), `server-rs/src/main.rs` (spawn refresher at boot if a token file already exists).  
**Approach:**
- `POST /api/providers/chatgpt/login/start` → `{ authorize_url, state }` (calls `begin_login`).
- `POST /api/providers/chatgpt/login/complete { code, state }` → `take_verifier(state)` (404/400 if unknown), `exchange_code`, compute `account_id`, `save()` auth file, `providers::upsert(Provider{ name:"chatgpt", base_url:"https://chatgpt.com/backend-api/codex", model: env AGENTIC_CHATGPT_MODEL || "gpt-5", protocol: Openai, chatgpt_oauth:true, api_key:"", .. })`, `litellm::request_reload()`, spawn refresher, return `{ account_id, expires_at }`. On `exchange_code` error → 502 with a clear message.
- `GET /api/providers/chatgpt/status` → `{ connected: bool, account_id, expires_at, needs_relogin: bool }` (connected = auth file loads with a token; needs_relogin set by the refresher when refresh failed — persist a `needs_relogin` flag in the auth file or a sibling marker).
- Extend `providers_delete`: when `name.eq_ignore_ascii_case("chatgpt")`, also delete the auth file (logout) after removing the provider entry.
- Refresher: `spawn_refresher()` idempotent (a `OnceLock<()>` guard or check a running flag); loop: load auth file; if none, stop; sleep until `expires_at - margin` (5 min); re-load (adopt any LiteLLM rotation); `refresh(refresh_token)`; on success `save()` + `request_reload()`; on failure set `needs_relogin=true`, save, and stop (user must re-login). `ponytail:` comment the "we refresh ahead so LiteLLM rarely refreshes" ceiling.
- `main.rs`: if `chatgpt_oauth::auth_file_path()` exists at boot, `spawn_refresher()` (so refresh survives restarts). Guard so tests/fake bridge don't spawn network work (it self-stops when no file).
- Routes registered in `mod.rs` near the other `/api/providers` routes; auth-gated automatically.
- `provider_view`/`ProviderView`: report `has_key = true` for a `chatgpt_oauth` provider when the token file loads (so the client shows it connected), and include `chatgpt_oauth` in the view.  
**Test scenarios (axum handler tests via `oneshot_req`, token endpoint mocked):**
- `login/start` returns a well-formed `authorize_url` (contains client_id, `code_challenge_method=S256`, the returned `state`) and 200.
- `login/complete` with unknown state → 400/404, no provider written.
- `login/complete` happy path (mock token endpoint): 200 with `account_id`; provider "chatgpt" now in `load_list()`; auth file exists 0600.
- `login/complete` when token endpoint 400s → 502; no provider written; no auth file.
- `GET status`: disconnected before login; connected after; `needs_relogin` true after a simulated refresh failure (set the flag) .
- `DELETE /api/providers/chatgpt` removes provider AND auth file.
- Covers R4: after `login/complete`, `GET /api/models` (default scope) includes an entry whose key is `chatgpt` (assert in the models test).

### U5. Android — API methods + DTOs

**Repo:** `agentic-dev-android`  
**Goal:** Client can call the three new endpoints.  
**Requirements:** R6.  
**Dependencies:** U4 (contract).  
**Files:** `core/network/src/main/kotlin/dev/agentic/data/net/AgenticApi.kt`, `KtorAgenticApi.kt`, `Models.kt`.  
**Approach:** Add `suspend fun chatgptLoginStart(): ChatgptLoginStart`, `chatgptLoginComplete(code, state): ChatgptStatus`, `chatgptStatus(): ChatgptStatus` to the interface (default no-op / empty for fakes) and implement in `KtorAgenticApi` with `{ auth() }`. DTOs (kotlinx.serialization, snake_case via `@SerialName`): `ChatgptLoginStart(authorizeUrl, state)`, `ChatgptStatus(connected, accountId?, expiresAt?, needsRelogin)`. Add `@SerialName("chatgpt_oauth") val chatgptOauth: Boolean = false` to the `Provider` DTO to mirror the server marker.  
**Test scenarios:** `Test expectation: none — DTO/interface plumbing; covered by compilation and the U6 flow.` (If the module has a serialization test pattern, add one round-trip for `ChatgptStatus`.)

### U6. Android — loopback + login flow + status UI

**Repo:** `agentic-dev-android`  
**Goal:** The "Connect ChatGPT" experience and model-list refresh.  
**Requirements:** R6, KTD6.  
**Dependencies:** U5.  
**Files:** `feature/providers/.../ChatGptOAuthLoopback.kt` (new helper), `feature/providers/.../ProvidersViewModel.kt`, `feature/providers/.../ProvidersScreen.kt`, plus wiring an Android `Context`/intent launcher the VM can use to `ACTION_VIEW`.  
**Approach:**
- `ChatGptOAuthLoopback`: `suspend fun awaitCode(expectedState): Result<String>` — bind `ServerSocket(1455, backlog, InetAddress.getByName("127.0.0.1"))` on `Dispatchers.IO`, accept one connection, read the request line, parse `code`+`state` from the query, verify `state`, write a minimal `200 OK` body ("You can close this tab and return to the app."), close. Timeout (e.g. 5 min) and port-in-use (`BindException`) → typed error surfaced to the UI.
- VM `connectChatgpt()`: `start()` → get `authorizeUrl`+`state`; launch loopback await (async) then fire `ACTION_VIEW(authorizeUrl)`; on code → `complete(code,state)`; on success `ModelCatalog.invalidate()` + `refresh()` (providers + models); expose status in `ProvidersUiState`. Map errors (port in use, timeout, 502) to user-facing text.
- UI: a "Connect ChatGPT" button in `ProvidersScreen` (near add-provider). When connected, show account/expiry and a "needs re-login" state that re-triggers the flow; a disconnect action calls `deleteProvider("chatgpt")`.  
**Execution note:** UI/integration behavior; prefer a compile + a light VM-level check over heavy instrumentation. Criterion R7 is "at least compiles".  
**Test scenarios:** `Test expectation: minimal` — if `feature/providers` has a ViewModel test harness with a fake `AgenticApi`, add: connect success sets connected state + invalidates catalog; `complete` failure surfaces error. Otherwise rely on compilation (the loopback parser is the one piece of real logic — add a tiny JVM unit test for "parse code&state from a GET request line" if a `src/test` sourceset exists).

---

## Verification Contract

- `agentic-dev`: `cd server-rs && cargo build` and `make test` (fake bridge, no API cost) both green. New tests in U1–U4 pass.
- `agentic-dev-android`: the affected modules compile (`core/network`, `feature/providers`) — e.g. `./gradlew :core:network:compileDebugKotlin :feature:providers:compileDebugKotlin` (or assembleDebug if feasible in the worktree with the inherited build env).
- Manual/contract check (not CI): `GET /api/models` default scope lists `chatgpt` after login; `providers.json` contains the marker but **no token**; `chatgpt-auth.json` is 0600.

---

## Definition of Done

- R1–R7 satisfied. Two non-draft PRs opened (one per repo), BENCHMARK mode: not merged.
- Six-lens adversarial verification (logic/regression, edge/error, security, failure-mode test coverage, reuse/reinvention, cross-layer/cross-repo contract) run via delegate before commit; real findings fixed.
- No new crates (server) / no new gradle deps (Android). Secrets never printed to the transcript.

---

## Sources & Research

- LiteLLM ChatGPT subscription provider — `chatgpt/<model>`, `model_info.mode: responses`, `CHATGPT_API_BASE`/`CHATGPT_TOKEN_DIR`/`CHATGPT_AUTH_FILE`/`CHATGPT_ORIGINATOR`, local token storage + refresh: https://docs.litellm.ai/docs/providers/chatgpt
- LiteLLM Responses API / OpenAI-compatible endpoints: https://docs.litellm.ai/docs/providers/openai/responses_api , https://docs.litellm.ai/docs/providers/openai_compatible
- Existing in-repo precedent for subscription OAuth as a separate credentials file: `server-rs/src/engine/model/providers.rs` (`is_oauth_token`, `oauth_token_from_credentials`, `claude_credentials_path`).
- ChatGPT OAuth facts (client_id, endpoints, redirect, scopes, headers) provided as confirmed input in the task brief.
