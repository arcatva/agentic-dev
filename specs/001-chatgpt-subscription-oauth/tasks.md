---
description: "Task list for ChatGPT subscription OAuth provider"
---

# Tasks: ChatGPT subscription OAuth provider for delegate routing

**Input**: Design docs in `/specs/001-chatgpt-subscription-oauth/`

**Tests**: Included (hermetic Rust unit tests only, per spec FR-009 / SC-003; no network, no API cost).

## Format: `[ID] [P?] [Story] Description`

- **[P]** = can run in parallel (different files, no dependency).
- Backend = `agentic-dev/server-rs`; Android = `agentic-dev-android`.

---

## Phase 1: Setup

- [ ] T001 Confirm no new crates needed (sha2, base64, uuid, reqwest, parking_lot already in
  `server-rs/Cargo.toml`); add `pub mod oauth;` to `server-rs/src/engine/model/mod.rs`.

## Phase 2: Foundational (blocks all stories)

- [ ] T002 [P] New `server-rs/src/engine/model/oauth.rs`: constants (client_id, authorize/token URLs,
  redirect, scopes, codex base_url + headers), `SubscriptionToken` type, and the 0600 token-store
  load/save (atomic temp+rename, path override for tests).
- [ ] T003 [P] In `oauth.rs`: PKCE (`code_verifier` via two uuid-v4 → base64url; `code_challenge =
  base64url_nopad(sha256(verifier))`), `authorize_url(state, challenge, provider)`, and JWT payload
  claim parse (`chatgpt_account_id`, `email`, `exp`).
- [ ] T004 In `oauth.rs`: `exchange_code(code, verifier)` and `refresh(refresh_token)` (reqwest
  blocking POST to the token URL) returning a parsed `SubscriptionToken`; parse-only unit-testable
  helpers separated from the network call.
- [ ] T005 `providers.rs`: add `AuthKind { ApiKey (default), Oauth }` field to `Provider`
  (`#[serde(default)]`); `resolved_key()` returns the token-store access token when `auth = oauth`.

**Checkpoint**: token store + PKCE + provider auth kind compile and are unit-tested.

---

## Phase 3: User Story 1 — Connect by OAuth login (P1) 🎯 MVP

- [ ] T006 [US1] `oauth.rs`: `register_from_token(provider_name, token)` — persist token, upsert the
  `gpt` provider (`protocol=openai, auth=oauth, base_url=codex, model`), request litellm reload +
  Claude/GPT catalog refresh.
- [ ] T007 [US1] `api/misc.rs`: `oauth_chatgpt_start` handler — make PKCE+state, store PendingLogin,
  spawn the one-shot `127.0.0.1:1455` loopback listener that on callback validates state, exchanges
  the code, calls `register_from_token`, returns "close this tab"; respond with `{authorize_url}`.
- [ ] T008 [US1] `api/misc.rs`: `oauth_chatgpt_status` + `oauth_chatgpt_logout` handlers.
- [ ] T009 [US1] `api/mod.rs`: register the 3 routes.
- [ ] T010 [US1] Tests: PKCE challenge vector; JWT account-id parse; token-store round-trip + mode
  0600; missing/corrupt store → not connected.

**Checkpoint**: a completed login registers `gpt`; `GET /api/models` lists it; token file is 0600
and providers.json holds no token.

---

## Phase 4: User Story 2 — Delegate routes to GPT (P2)

- [ ] T011 [US2] `litellm.rs` `build_config`: for an `auth=oauth` provider emit `api_base = codex
  base_url`, `extra_headers` (ChatGPT-Account-Id from token account_id, originator, OpenAI-Beta), and
  bearer via env (already the path). Keep back-compat for plain openai providers.
- [ ] T012 [US2] Test: `build_config` for an oauth provider produces the codex base_url + all three
  headers and puts the bearer in the env map, not the yaml file.

**Checkpoint**: generated proxy config is correct for GPT; existing openai providers unaffected.

---

## Phase 5: User Story 3 — Auto-refresh + re-login (P2)

- [ ] T013 [US3] `oauth.rs`: `refresh_due()` scan + `run_refresh_once()` — refresh tokens within the
  60s buffer, persist, `litellm::request_reload()`; on invalid_grant set `needs_reauth`.
- [ ] T014 [US3] `oauth.rs`: `start_refresher()` background loop (sleeps to soonest expiry-60s);
  spawn it from `main.rs` at boot next to `litellm::start_supervisor()`.
- [ ] T015 [US3] Test: near-expiry token → `run_refresh_once` swaps token (with an injectable
  refresh fn); invalid refresh → `needs_reauth = true`.

**Checkpoint**: tokens refresh unattended; failed refresh flips status.

---

## Phase 6: User Story 4 — Android login + status (P3)

- [ ] T016 [US4] `core/network/.../net/Models.kt`: `ChatgptStartResp(authorize_url)`,
  `ChatgptStatus(status, account_email?, expires_at?)` DTOs.
- [ ] T017 [US4] `AgenticApi.kt` + `KtorAgenticApi.kt`: `startChatgptLogin()`, `chatgptStatus()`,
  `chatgptLogout()`.
- [ ] T018 [US4] `core/data/.../repo/ProvidersRepository.kt`: pass-through methods.
- [ ] T019 [US4] New `feature/providers/.../providers/ChatgptLoginViewModel.kt`: start → open URL
  (ACTION_VIEW intent) → poll status → on Connected `ModelCatalog.invalidate()` + refresh.
- [ ] T020 [US4] `ProvidersScreen.kt`: "Login with ChatGPT" button + status card (account / expiry /
  needs-relogin) wired to the view model.

**Checkpoint**: Android shows login entry + status and refreshes GPT into the list after login.

---

## Phase 7: Polish & verification

- [ ] T021 Backend: `cargo build` + `make test` green.
- [ ] T022 Android: `:core:network`, `:feature:providers`, `:app` assemble.
- [ ] T023 Adversarial self-review (delegate fan-out per repo CLAUDE.md), fix real findings.
- [ ] T024 Open non-draft PR(s); STOP (benchmark rule: do not merge).

## Dependencies

- Phase 2 blocks 3–6. US1(P1) is the MVP. US2/US3 depend on US1's provider registration. US4 depends
  on the US1 endpoints existing. Backend (US1–US3) before Android (US4).
