# Architecture

High-level map of the `agentic-dev` backend. For depth (ops recipes, stream quirks, deploy),
see [`docs/internals.md`](docs/internals.md).

## What it is

A local, operator-run control plane that spawns headless `claude` coding sessions in git
worktrees and streams them to clients over an HTTP + WebSocket API. There is no server-rendered
UI; the only client is the [Android app](https://github.com/arcatva/agentic-dev-android).

## Components

```
                    ┌────────────────────────── server-rs (Rust) ──────────────────────────┐
  Android client    │                                                                       │
  (LAN, TLS,   ───► │  api/          axum routes + WS stream + HMAC bearer auth + TLS        │
  bearer token)     │   │            (the whole surface the client depends on)              │
                    │   ▼                                                                    │
                    │  engine/       HTTP-independent core (no axum imports):                │
                    │                sessions/lifecycle, store (sqlite + log files),         │
                    │                stream parser, worktree/repos, providers/router,        │
                    │                workflows, runner (SdkRunner)                           │
                    │   │                                                                    │
                    │   ▼  spawns per turn                                                   │
                    │  sdk-bridge.mjs (Node)  ──►  @anthropic-ai/claude-agent-sdk  ──► claude │
                    └───────────────────────────────────────────────────────────────────────┘
```

- **`server-rs/src/api/`** — axum HTTP routes + the WebSocket stream, HMAC token auth
  (`/api/login` issues a bearer token), TLS termination, config. This is the stable contract the
  Android client consumes.
- **`server-rs/src/engine/`** — the HTTP-independent core: session lifecycle & queue, the
  sqlite + log-file store, the stream-json parser, git worktree/repo management, provider/model
  routing, workflows, and the runner. Kept free of `axum` so it stays unit-testable in isolation.
- **`server-rs/sdk-bridge.mjs`** — the per-turn transport. The Rust `SdkRunner` spawns this Node
  bridge, which drives `claude` through the official Agent SDK and mirrors every SDK message back
  to the session log in stream-json shape. This is what makes in-turn pauses (AskUserQuestion)
  actually wait.

## Request flow (a session turn)

1. Client authenticates at `/api/login`, then calls the API with `Authorization: Bearer <token>`.
2. `api/` hands work to `engine/`, which creates/uses a git worktree and enqueues the turn.
3. `SdkRunner` spawns `sdk-bridge.mjs`; the bridge runs the turn via the Agent SDK.
4. Stream-json output is mirrored to the session log; the tailer/parser turn it into events
   streamed to the client over the WebSocket, while the store persists session state.

## Testing seam

Tests inject an `SdkRunner` pointed at a fake bridge script
(`server-rs/tests/fixtures/fake-sdk-bridge-*.sh`, run via `bash` not `node`) that appends canned
stream-json — the same path the real bridge writes. So the engine and API are tested end-to-end
with no Node and no API cost.
