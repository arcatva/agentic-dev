# Contributing

Thanks for helping improve `agentic-dev` (the Rust backend for the agentic-dev platform). This is
the server only — the client lives in [`agentic-dev-android`](https://github.com/arcatva/agentic-dev-android).

## Prerequisites

- Rust (pinned via `server-rs/rust-toolchain.toml`; `rustup` honors it automatically).
- Node.js — the per-turn transport is a Node bridge (`server-rs/sdk-bridge.mjs`) using
  `@anthropic-ai/claude-agent-sdk`.

## Build & test

```bash
make build     # runs the bridge's `npm install` (in server-rs/), then `cargo build --release`
make test      # = cd server-rs && cargo test — the whole suite (engine + api, in-crate)
```

Tests never hit the real `claude`: they run through the production runner (`SdkRunner`) pointed at
a fake bridge script in `server-rs/tests/fixtures/` — no Node, no API cost.

## Code style & gates

CI runs on every PR (`.github/workflows/ci.yml`):

- `cargo fmt --check` — formatting is enforced (config in `server-rs/rustfmt.toml`).
- `cargo clippy --all-targets` — currently **warn-only** while the existing backlog is cleared;
  new code should not add warnings.
- `cargo-deny check licenses bans sources` — supply-chain gate.

Run `cargo fmt` and skim `cargo clippy` before pushing.

## Architectural rules (keep these true)

- **`server-rs/src/engine/` stays free of `axum` imports** — the engine is the HTTP-independent
  core and must remain unit-testable in isolation.
- **`server-rs/src/api/` is the whole surface the Android client depends on.** Treat it as a
  stable contract: avoid changing request/response shapes unless intentional and coordinated.
- The per-turn transport is the SDK bridge only (`SdkRunner` → `sdk-bridge.mjs`) in both
  production and tests.

See [`docs/internals.md`](docs/internals.md) for architecture depth and ops recipes, and
[`ARCHITECTURE.md`](ARCHITECTURE.md) for the high-level map.

## Pull requests

- One coherent change per PR; keep behavior-preserving changes (moves, formatting) separate from
  logic changes so diffs stay reviewable.
- Describe how you verified it. The hard merge floor is that the code compiles.
- Opening a non-draft PR triggers an automated review; address correct feedback before merge.
