# Feature Specification: ChatGPT subscription OAuth provider for delegate routing

**Feature Branch**: `001-chatgpt-subscription-oauth`

**Created**: 2026-07-13

**Status**: Draft

**Input**: User description: "Let a user connect their own ChatGPT subscription (OAuth login, not an API key) so GPT models enter the platform model list and can be routed by delegate."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Connect a ChatGPT subscription by OAuth login (Priority: P1)

A user who pays for ChatGPT wants GPT available as a delegate worker without pasting an API
key (they have no API key — they have a subscription). From the Android providers screen they
tap **Login with ChatGPT**, complete the OpenAI sign-in in a browser, and the platform stores
the resulting credential. GPT then appears in the model list and delegate can route to it.

**Why this priority**: This is the whole feature. Without OAuth registration nothing else has
meaning. It is the MVP.

**Independent Test**: Start the OAuth flow, complete sign-in against OpenAI, confirm a `gpt`
provider is registered, its token is persisted with tight file permissions, and `GET /api/models`
(default scope) lists the GPT model.

**Acceptance Scenarios**:

1. **Given** no ChatGPT provider is connected, **When** the user completes the OAuth login,
   **Then** a subscription provider is registered, the access/refresh tokens are saved to a
   secrets store separate from the human-editable providers file, and the store file is mode 0600.
2. **Given** the OAuth login succeeded, **When** the client fetches `GET /api/models` (no scope),
   **Then** the GPT model is present in the returned list.
3. **Given** a code/PKCE mismatch or a denied consent, **When** the callback fires,
   **Then** no provider is registered and the connection status reports the failure.

---

### User Story 2 - Delegate routes work to the connected GPT model (Priority: P2)

Once connected, a delegate fan-out that names a GPT model (or that the router picks) runs on the
user's ChatGPT subscription through the existing OpenAI-compat (LiteLLM) path, authenticating with
the current OAuth bearer.

**Why this priority**: Registration is worthless if the model can't actually be called. Second
because it depends on P1.

**Independent Test**: With a connected provider, confirm the generated LiteLLM config contains a
model entry pointing at the ChatGPT backend endpoint with the required Codex headers, and that the
bearer is injected from the token store via process env (never written to a config file).

**Acceptance Scenarios**:

1. **Given** a connected GPT provider, **When** the OpenAI-compat proxy config is generated,
   **Then** it contains the GPT model with the ChatGPT backend base URL, the account-id/originator/
   beta headers, and the bearer sourced from the rotating token — not a static key in the file.
2. **Given** a delegate task hinting the GPT model, **When** it runs, **Then** it is routed through
   the proxy like any other openai-protocol provider (worker points at the local proxy).

---

### User Story 3 - Tokens refresh automatically; re-login when refresh fails (Priority: P2)

Subscription access tokens are short-lived JWTs. The platform refreshes them before expiry so
routing keeps working unattended. If the refresh token itself becomes invalid, the provider status
flips to "needs re-login" and the Android UI surfaces it.

**Why this priority**: Without refresh the integration breaks within the hour. Tied with P2 because
it is required for sustained use.

**Independent Test**: Simulate a token near expiry and confirm the refresher swaps in a new access
token and reloads the proxy; simulate a failed refresh and confirm status becomes NeedsReauth.

**Acceptance Scenarios**:

1. **Given** an access token near expiry, **When** the refresher runs, **Then** it obtains a new
   access token with the stored refresh token, persists it, and asks the proxy to reload.
2. **Given** a refresh that returns invalid_grant, **When** the refresher runs, **Then** the
   provider status becomes NeedsReauth and the stored access token is treated as unusable.

---

### User Story 4 - See connection status on Android (Priority: P3)

The providers screen shows whether ChatGPT is connected, which account, when the token expires, or
that a re-login is required, and refreshes the model list after a successful login.

**Why this priority**: Quality-of-life visibility; the core routing works without it.

**Independent Test**: Render the providers screen against each status (NotConnected, Pending,
Connected, NeedsReauth) and confirm the correct affordance shows; after Connected, the model list
reloads and shows GPT.

**Acceptance Scenarios**:

1. **Given** a connected account, **When** the screen loads, **Then** it shows the account
   identifier and token expiry.
2. **Given** status NeedsReauth, **When** the screen loads, **Then** it shows a re-login button.

---

### Edge Cases

- Callback arrives with a `state` that doesn't match the pending login → reject, register nothing.
- Login started but the user never finishes → pending listener times out and is cleaned up; status
  returns to NotConnected.
- Token store file missing or corrupt → treated as "not connected"; never crashes routing.
- JWT missing the `chatgpt_account_id` claim → registration fails with a clear error (the account
  header is mandatory for the backend call).
- LiteLLM binary absent → openai providers (incl. GPT) simply don't run; no crash (existing
  behavior preserved).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST let a user register a ChatGPT-subscription provider via OAuth 2.0
  Authorization Code + PKCE (S256), without an API key.
- **FR-002**: System MUST persist the access token, refresh token, expiry, and ChatGPT account id
  in a secrets store separate from the human-editable providers file, with owner-only (0600) perms.
- **FR-003**: System MUST NOT write any OAuth token into the providers file or the proxy config file.
- **FR-004**: The registered GPT model MUST appear in `GET /api/models` default scope.
- **FR-005**: System MUST route delegate work to the GPT model through the existing OpenAI-compat
  (LiteLLM) path, authenticating with the current OAuth bearer and the required Codex request
  headers (account-id, originator, responses beta).
- **FR-006**: System MUST refresh the access token before expiry using the refresh token and make
  the new bearer take effect for routing.
- **FR-007**: When a refresh fails irrecoverably, System MUST mark the provider as needing re-login
  and expose that via a status endpoint.
- **FR-008**: Android MUST provide a login entry point, open the OpenAI authorize URL, show
  connection status (account / expiry / needs-relogin), and refresh the model list after success.
- **FR-009**: Server `cargo` build MUST pass and `make test` (fake bridge, no real API spend) MUST
  be green; Android MUST at least compile.

### Key Entities *(include if feature involves data)*

- **SubscriptionToken**: the persisted OAuth credential for one provider — access token, refresh
  token, expiry timestamp, account id, optional account email; lives in the secrets store.
- **PendingLogin**: transient state for an in-flight OAuth login — PKCE verifier, CSRF `state`, and
  the loopback callback listener; discarded once the callback is handled or times out.
- **Provider (existing, extended)**: gains an auth kind so a record can declare "credential comes
  from the OAuth token store" instead of carrying a literal/env key.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user completes the ChatGPT OAuth login and a GPT model appears in `GET /api/models`
  (default scope) with no API key entered.
- **SC-002**: The persisted token file is mode 0600 and contains no plaintext token inside the
  providers file the UI edits.
- **SC-003**: The generated proxy config for the GPT provider carries the ChatGPT backend endpoint,
  the account-id/originator/beta headers, and a bearer injected via env — verifiable by a unit test.
- **SC-004**: A near-expiry token is refreshed automatically and routing continues without manual
  action; an invalid refresh flips status to needs-re-login.
- **SC-005**: `cargo build` and `make test` are green; the Android providers module compiles.

## Assumptions

- The OAuth client registration is the Codex-CLI public client (client_id
  `app_EMoamEEZ73f0CkXaXp7hrann`, redirect `http://localhost:1455/auth/callback`, scopes
  `openid profile email offline_access`), and the call endpoint is
  `POST https://chatgpt.com/backend-api/codex/responses` (Responses API, streaming) — as given.
- Because the redirect is a fixed loopback on port 1455, the OAuth callback is handled **server-side**
  (the platform host runs the one-shot loopback listener); the Android client only opens the authorize
  URL and polls status. This matches how the Codex CLI performs the same login.
- GPT cannot be the main-session model (the main session is Claude Agent SDK driven); GPT enters only
  as a delegate provider. Session-start (`scope=session_start`) model pickers remain Claude-only.
- The OpenAI-compat proxy (LiteLLM) can reach the ChatGPT backend given the correct base URL,
  bearer, and extra headers. Proving a live end-to-end call needs real credentials and is out of
  scope for automated tests (tests run on the fake bridge); the config-generation and token plumbing
  are what the tests verify.
- One ChatGPT subscription provider at a time (named `gpt`) is sufficient for v1.
