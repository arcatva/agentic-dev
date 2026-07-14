# Phase 1 Contracts — new HTTP endpoints (additive)

All under the existing HMAC-token auth. Provider name defaults to `gpt`.

## POST `/api/providers/oauth/chatgpt/start`

Begin an OAuth login. Server generates PKCE + `state`, binds the one-shot loopback listener on
`127.0.0.1:1455`, and returns the authorize URL for the client to open.

Request body: none (or `{}`).

Response `200`:
```json
{ "authorize_url": "https://auth.openai.com/oauth/authorize?response_type=code&client_id=...&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access&code_challenge=...&code_challenge_method=S256&state=..." }
```

Errors: `500` if the loopback port can't be bound (e.g. a login already in progress).

## GET `/api/providers/oauth/chatgpt/status`

Report the connection status. Used by the client to poll after opening the authorize URL and to
render the status card.

Response `200`:
```json
{ "status": "not_connected | pending | connected | needs_reauth",
  "account_email": "user@example.com",   // present when connected
  "expires_at": 1768345678 }             // unix seconds, present when connected
```

## POST `/api/providers/oauth/chatgpt/logout`

Disconnect: remove the token from the store and remove the `gpt` provider; trigger a proxy reload.

Response `200`: `{ "ok": true }`.

## Callback (not a public API route — loopback only)

`GET http://127.0.0.1:1455/auth/callback?code=...&state=...` is served by the transient listener
started by `/start`, never by the axum app. It validates `state`, exchanges the code (with the PKCE
verifier) at the token endpoint, parses the account id from the JWT, persists the token (0600),
upserts the `gpt` provider, triggers `litellm::request_reload()` and a Claude-model-list style
catalog refresh, then returns a small "you can close this tab" HTML page.
