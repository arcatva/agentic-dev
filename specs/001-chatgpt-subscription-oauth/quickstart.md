# Quickstart — exercise the ChatGPT OAuth provider

## Backend (local)

1. Build: `cd server-rs && cargo build`. Run tests: `make test` (fake bridge, no API cost).
2. Start a login: `POST /api/providers/oauth/chatgpt/start` → open the returned `authorize_url` in a
   browser on the server host. Sign in to ChatGPT. The redirect to `http://localhost:1455/auth/callback`
   is captured by the server's one-shot listener.
3. Check: `GET /api/providers/oauth/chatgpt/status` → `connected`; `GET /api/models` now lists the
   GPT model. `~/.agentic-dev/oauth-tokens.json` exists at mode 0600; `~/.agentic-dev/providers.json`
   contains a `gpt` entry with `auth: "oauth"` and **no** token.
4. Route: a delegate task hinting `gpt` runs through the LiteLLM proxy (needs the litellm binary
   installed; absent → provider simply doesn't run, no crash).

## Android

Global Settings → Models section → **Login with ChatGPT**. Opens the authorize URL in a browser.
After completing sign-in on the server host, the status card shows the account + expiry and the
model list refreshes to include GPT. If a refresh later fails, the card shows **Re-login**.

## What the automated tests cover (hermetic, no network)

- PKCE: `code_challenge == base64url_nopad(sha256(code_verifier))`.
- JWT: `chatgpt_account_id` extracted from a crafted payload segment.
- Token store: round-trips; file mode is 0600; missing/corrupt file → "not connected".
- LiteLLM config for an oauth provider carries the codex base_url + the three extra headers, bearer
  via env not file.
