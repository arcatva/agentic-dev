# Implementation Plan: ChatGPT subscription OAuth provider for delegate routing

**Branch**: `001-chatgpt-subscription-oauth` | **Date**: 2026-07-13 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-chatgpt-subscription-oauth/spec.md`

## Summary

Add a ChatGPT-subscription auth path so a user can connect their paid ChatGPT account by OAuth
(Authorization Code + PKCE) instead of an API key. The credential is stored in a 0600 secrets file
separate from the human-editable providers file, auto-refreshed before expiry, and injected as a
rotating bearer into the existing OpenAI-compat (LiteLLM) proxy so GPT becomes a routable delegate
worker. The Android providers screen gains a login entry point and a status card.

Approach: reuse everything already in place. `Provider` already carries `protocol=openai`, which
already flows into `GET /api/models` and into `litellm::build_config`. We add one auth variant
(`auth=oauth`) that resolves its key from a token store, one `oauth` module (PKCE + token
exchange/refresh + store), two API routes + a loopback callback, one background refresher, and a
small Android login+status UI. No new crates (sha2/base64/uuid/reqwest/parking_lot already present).

## Technical Context

**Language/Version**: Rust (edition per repo toolchain) backend; Kotlin + Jetpack Compose client.

**Primary Dependencies**: axum 0.8, reqwest 0.12 (blocking + json, rustls), tokio, serde, sha2,
base64, uuid, parking_lot — all already in `server-rs/Cargo.toml`. Android: Ktor client, Compose,
manual DI (AppContainer). No new dependencies on either side.

**Storage**: JSON files under `~/.agentic-dev/`. New: `oauth-tokens.json` (0600). Existing:
`providers.json` (unchanged shape apart from an optional `auth` field), `litellm-config.yaml`.

**Testing**: `cargo test` via `make test` (fake bridge, no API). Hermetic unit tests only — PKCE
challenge, JWT account-id parse, token-store round-trip + perms, litellm config generation for an
oauth provider. Android: module compiles (`:feature:providers`, `:core:network`, `:app`).

**Target Platform**: Linux/macOS server; Android app (minSdk 26, target 35).

**Project Type**: Mobile + API (Rust backend + Android client) — two repos in this workspace.

**Constraints**: No plaintext token in the providers file or the proxy config file; token file
0600; the loopback listener binds `127.0.0.1:1455` only during an active login. Must not start or
disturb the running platform server (session rule) — verify with `cargo build`/`cargo test` only.

**Scale/Scope**: One subscription provider (`gpt`) per install; single-user local platform.

## Constitution Check

The repo constitution template is unpopulated (placeholder). Applying the project's real rules from
`CLAUDE.md` as the gate:

- **Engine stays axum-free**: the `oauth` module lives in `engine/model/` and must not import axum.
  The HTTP handlers stay in `api/`. PASS by design.
- **Tests never hit real `claude` / no API cost**: all new tests are pure/hermetic; the OAuth token
  endpoint is never called in tests (exchange/refresh are unit-tested by parsing crafted responses,
  not by network). PASS.
- **API stays stable for the Android client**: additive routes only; existing DTOs unchanged
  (`Provider` gains an optional `auth` field that defaults to the current behavior). PASS.
- **Secrets never printed / tight perms**: token store is 0600, tokens never logged. PASS.

No violations → Complexity Tracking left empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-chatgpt-subscription-oauth/
├── plan.md              # This file
├── spec.md              # Feature spec
├── research.md          # Phase 0 — OAuth/Codex facts + integration decisions
├── data-model.md        # Phase 1 — entities & files
├── contracts/
│   └── api.md           # Phase 1 — new HTTP endpoints
├── quickstart.md        # Phase 1 — how to exercise the flow
└── tasks.md             # Phase 2 — /speckit-tasks output
```

### Source Code (both repos in this workspace)

```text
agentic-dev/                          # Rust backend
└── server-rs/src/
    ├── engine/model/
    │   ├── oauth.rs                  # NEW: token store, PKCE, exchange/refresh, JWT claim
    │   ├── providers.rs              # EDIT: add AuthKind; resolved_key reads token store for oauth
    │   ├── litellm.rs                # EDIT: codex base_url + extra_headers for oauth providers
    │   └── mod.rs                    # EDIT: `pub mod oauth;`
    ├── api/
    │   ├── misc.rs                   # EDIT: oauth start/status/logout handlers (near provider CRUD)
    │   └── mod.rs                    # EDIT: register the 3 routes
    └── main.rs                       # EDIT: spawn the oauth refresher at boot

agentic-dev-android/                  # Kotlin/Compose client
├── core/network/.../net/
│   ├── Models.kt                     # EDIT: OAuth start/status DTOs
│   ├── AgenticApi.kt                 # EDIT: startChatgptLogin(), chatgptStatus()
│   └── KtorAgenticApi.kt             # EDIT: implement the two calls
├── core/data/.../repo/
│   └── ProvidersRepository.kt        # EDIT: pass-through methods
└── feature/providers/.../providers/
    ├── ChatgptLoginViewModel.kt      # NEW: start login, poll status, open URL, refresh catalog
    └── ProvidersScreen.kt            # EDIT: login button + status card
```

**Structure Decision**: Mobile + API. The backend keeps HTTP-independent logic in
`engine/model/oauth.rs` (unit-testable in isolation, no axum) and the HTTP surface in `api/`. The
Android change is confined to the existing `feature/providers` + its network/data layers; no new
nav destination (the providers screen is embedded in Global Settings and gains one section).

## Phase notes

- **Phase 0 (research.md)**: pin the exact OAuth/Codex constants, decide server-side loopback
  callback (fixed redirect port 1455), decide rotating-bearer delivery (env + proxy reload on
  refresh), decide randomness source (uuid v4 bytes → no new crate).
- **Phase 1 (data-model.md + contracts/)**: the SubscriptionToken / PendingLogin entities and the
  three additive endpoints.
- **Phase 2 (tasks.md)**: produced by `/speckit-tasks`, grouped by user story, backend before
  client, tests alongside the logic they cover.

## Complexity Tracking

No constitution violations; nothing to justify.
