# Phase 0 Research — ChatGPT subscription OAuth

## OAuth / Codex constants (given, confirmed)

| Field | Value |
|-------|-------|
| Grant | Authorization Code + PKCE (S256) |
| Authorize URL | `https://auth.openai.com/oauth/authorize` |
| Token URL | `https://auth.openai.com/oauth/token` |
| client_id | `app_EMoamEEZ73f0CkXaXp7hrann` (Codex CLI public client) |
| redirect_uri | `http://localhost:1455/auth/callback` (fixed loopback) |
| scopes | `openid profile email offline_access` |
| Call endpoint | `POST https://chatgpt.com/backend-api/codex/responses` (Responses API, streaming) |
| Required headers | `Authorization: Bearer <access_token>`, `ChatGPT-Account-Id: <jwt.chatgpt_account_id>`, `originator: codex_cli_rs`, `OpenAI-Beta: responses=experimental` |
| Token kind | access token = short-lived JWT; refresh with `refresh_token`; refresh token can go invalid → re-login |

## Decisions

### D1 — Callback is server-side (loopback on 1455)
The registered redirect is a fixed `http://localhost:1455/auth/callback`. That loopback must be
answered by whatever host completes the login. On this platform the **server** owns it: on
`/start` we spin a one-shot `TcpListener` on `127.0.0.1:1455`, hand the authorize URL back to the
client, and the browser's redirect lands on the server. The Android client only opens the URL and
polls status. This mirrors how the Codex CLI itself logs in. Rejected: client-side callback — the
phone's localhost has nothing listening and the fixed client_id forbids a custom redirect.

### D2 — Rotating bearer via env + proxy reload (not a static key file)
LiteLLM reads each provider's key from the process env at (re)start (`os.environ/AGENTIC_LITELLM_KEY_N`),
never from the config file. We keep that: `resolved_key()` for an oauth provider returns the current
access token from the token store, so `build_config` injects the live bearer into the proxy env. The
background refresher calls `litellm::request_reload()` after swapping the token, which regenerates the
config and restarts the proxy with the new bearer.
`ponytail:` proxy restarts once per refresh (~hourly). Fine for a single-user local platform; if
that churn ever matters, the upgrade is a LiteLLM custom-auth callback that reads the token live.

### D3 — Codex headers via LiteLLM `extra_headers`
The ChatGPT backend needs three non-standard headers and a non-default base URL. LiteLLM supports
per-model `api_base` and `extra_headers` in `litellm_params`. For the oauth provider we emit
`api_base: https://chatgpt.com/backend-api/codex` and `extra_headers` with the account-id /
originator / OpenAI-Beta values. The account-id is known at config-generation time (persisted in the
token store from the JWT).
`ponytail:` this assumes LiteLLM can speak the Codex `responses` wire shape at that base_url. That is
the one piece not provable without live credentials (tests run on the fake bridge). If LiteLLM's
translation doesn't match the Codex endpoint, the upgrade path is a ~50-line Anthropic→Codex-responses
shim in place of the proxy hop for this one provider. The token/plumbing/config generation are what
the automated tests verify.

### D4 — Randomness with no new crate
PKCE `code_verifier` and CSRF `state` need entropy. `uuid` (v4, already a dependency) gives 122
random bits per value; concatenating two v4 UUIDs' bytes yields a 32-byte verifier, base64url
(no-pad) encoded. `code_challenge = base64url_nopad(sha256(verifier))` using the already-present
`sha2` + `base64` crates. Rejected: adding `rand`/`getrandom` as a direct dep — unnecessary.

### D5 — JWT claim extraction without a JWT library
We only need `chatgpt_account_id` (and optionally `email`, `exp`) from the token payload. Decode the
middle segment (base64url) and `serde_json::from_slice`. No signature verification — the token came
straight from the token endpoint over TLS, and it is used as an opaque bearer; we are not authorizing
against it. Rejected: adding `jsonwebtoken` — overkill for reading two claims.

### D6 — Auth kind on Provider
Add `#[serde(default)] auth: AuthKind` where `AuthKind = ApiKey (default) | Oauth`. Default keeps old
`providers.json` files loading unchanged. For `Oauth`, `resolved_key()` ignores `api_key`/`api_key_env`
and reads the access token from the token store keyed by provider name. The providers file thus holds
no secret for an oauth provider (FR-003).

## Unknowns / assumptions carried forward
- Exact GPT model id to register (`gpt-5-codex` assumed as the Codex responses model). Configurable;
  the id only affects the label and the `model_name` in the proxy config.
- Token `expires_in` vs JWT `exp`: prefer the token response's `expires_in` (seconds) when present,
  else fall back to the JWT `exp`. Refresh buffer: 60s before expiry.
