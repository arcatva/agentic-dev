# Phase 1 Data Model

## SubscriptionToken (persisted, secrets store)

File: `~/.agentic-dev/oauth-tokens.json`, mode **0600**, atomic write (temp + rename). A JSON map
`{ provider_name -> SubscriptionToken }`.

| Field | Type | Notes |
|-------|------|-------|
| `access_token` | string | short-lived JWT bearer |
| `refresh_token` | string | used to mint new access tokens |
| `account_id` | string | `chatgpt_account_id` claim → `ChatGPT-Account-Id` header |
| `account_email` | string? | `email` claim, for UI display |
| `expires_at` | i64 | unix seconds; refresh 60s before |
| `needs_reauth` | bool | set true when a refresh returns invalid_grant |

Never written into `providers.json` or `litellm-config.yaml`. Never logged.

## PendingLogin (transient, in-memory)

Held in a process-global map keyed by `state` while a login is in flight.

| Field | Type | Notes |
|-------|------|-------|
| `state` | string | CSRF token, echoed by the callback |
| `code_verifier` | string | PKCE verifier (base64url, 32 bytes) |
| `provider_name` | string | which provider this login registers (default `gpt`) |
| `created_at` | i64 | for the pending-login timeout / cleanup |

The one-shot loopback `TcpListener` on `127.0.0.1:1455` belongs to the same login; closed after the
callback is handled or on timeout.

## Provider (existing — NOT extended)

Implementation note: rather than add an `auth` field (which would break ~16 `Provider {}` struct
literals across the codebase), a provider "is" a subscription provider **iff the token store holds a
token under its name**. This keeps `Provider`'s schema and every existing literal untouched:

- `resolved_key()` already returns the literal/env key first; we only add a final fallback — when
  those are empty, read the current `access_token` from the token store keyed by `self.name`.
- `litellm::build_config` adds the Codex extra-headers only when `oauth::subscription_token(name)`
  is present.

The registered `gpt` provider is a normal `openai`-protocol record with an empty key:
`{ name: "gpt", protocol: openai, auth: oauth, base_url: "https://chatgpt.com/backend-api/codex",
model: "gpt-5-codex", capability ~0.9, cost ~0.6, enabled: true }`.

## Status projection (derived, for the status endpoint)

`ConnectionStatus = NotConnected | Pending | Connected | NeedsReauth`, derived from whether a
PendingLogin exists, whether a token exists, and its `needs_reauth` flag. Carries `account_email`
and `expires_at` when Connected.
