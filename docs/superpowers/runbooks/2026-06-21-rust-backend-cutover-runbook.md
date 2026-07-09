# Rust Backend Cutover Runbook

**Date:** 2026-06-21
**What:** Switch the live agentic-dev backend from the TypeScript server (`tsx server/index.ts`) to the Rust server (`server-rs/`), and how to roll back.
**Who runs the restart:** YOU. The restart interrupts any in-flight `claude` turns (identical to any service restart). Everything up to the restart is prepared and validated.

---

## 1. Why this is safe (the migration seam)

Both servers read/write the **same on-disk state** — `~/.agentic-dev/db.sqlite` (+ WAL), `~/.agentic-dev/logs/<id>.jsonl`, `~/.agentic-dev/{groups,templates,device}.json`, and the per-session `.claude-config/` layout. The Rust server opens the existing DB (same schema + idempotent migration) and recovers sessions from disk. **Cutover = a restart that happens to start a different binary.** Rollback = restart the old binary. The on-disk format is compatible both ways.

The Android client needs **no change**: the Rust server reads the same `AGENTIC_AUTH_SECRET` from the same `EnvironmentFile`, so existing tokens validate (cross-checked: a token minted by the Node server verifies in Rust and vice-versa), and the REST/WS JSON shapes match `server/engine/types.ts`.

## 1.5 Runtime dependency — the SDK runner (Node bridge)

Production turns run through the Claude **Agent SDK**, not the raw `claude -p` CLI (that is only the test seam). The Rust server spawns a thin Node bridge (`server-rs/sdk-bridge.mjs`) per turn that reuses `@anthropic-ai/claude-agent-sdk` — this is what makes **AskUserQuestion pause in-turn** (the SDK answers claude's `can_use_tool` control protocol; the raw CLI cannot). So the Rust deploy needs, **at runtime**:

- **`node`** on `PATH` (the same one the TS server uses) — override with `AGENTIC_NODE_BIN`.
- **`node_modules/@anthropic-ai/claude-agent-sdk`** resolvable from the bridge — keep `~/src/agentic-dev/node_modules` in place (the bridge at `server-rs/sdk-bridge.mjs` resolves it from one dir up). **Do not delete node_modules after cutover.**
- Optional `AGENTIC_SDK_BRIDGE=<abs path to sdk-bridge.mjs>` to override the default (auto-derives `server-rs/sdk-bridge.mjs` next to the binary, else `~/src/agentic-dev/server-rs/sdk-bridge.mjs`).

The latency-sensitive paths (HTTP/WS/engine/transcript) are 100% Rust — the bridge is only the per-turn claude transport, so the head-of-line-blocking fix is unaffected. Keep `server/sdkRunner.ts` and `server-rs/sdk-bridge.mjs` in sync if either changes.

## 2. Validation already done (shadow run vs real state)

Built the release binary and ran it on port 7799 against a **copy of the real `~/.agentic-dev`** (19 sessions, 172 MB of logs incl. a 28 MB transcript), with test credentials:

- Boots, opens the real sqlite DB, recovers — `/healthz` → `ok`, no errors.
- `GET /api/sessions` → 19 sessions, ordered by `lastUserMessageAt DESC` (matches the DB).
- `GET /api/sessions/<28MB-session>?limit=50` → **47 ms cold / 5 ms warm**, returns the last 50 of 21,677 rendered lines (`start:21627,total:21677`). The cold read is one `spawn_blocking` pass (off the async executor → cannot block other sessions); warm reads serve from the in-memory projection.
- `?before=100&limit=50` scroll-back → 5 ms, lines [50,100).
- Full transcript (no `?limit`, back-compat) → 55 ms / 15 MB / 21,677 lines.
- No token / bad token → `401` / `401`.

This is the original head-of-line-blocking case: the TS server re-read+rendered that 28 MB file on the event loop on every request; the Rust server reads it once off the executor and then serves a 50-line window in 5 ms.

**Real-turn end-to-end (SDK runner) — validated with real claude through the Rust server:** a normal turn (reply + cost, then idle/awaiting), an **AskUserQuestion** park→answer→resume (result: success), and a multi-turn follow-up live-inject — all via the Node bridge, with the transcript served by the Rust projection.

`cargo test` (unit/integration, fake-claude + fake-bridge, never real network): **253 passing** (incl. a deterministic SDK-runner lifecycle test: env wiring, stdin→bridge, log write, persistence, `stop()` terminates the bridge, missing-node→127). The whole suite holds 0/20 under 16-way CPU load after the engine/spawner race fixes.

> ⚠️ **Two WebSocket cutover-blockers found AFTER this shadow run (the §2 validation is HTTP + a
> single real turn; it did NOT exercise the live app WS path) — both now fixed on master, but
> RE-VALIDATE in the actual Android app before flipping production:**
> 1. **Live non-rendered frames** (subagent `agentResult`, API `retry`, `init`) were dropped by the
>    poke-only WS loop — subagent/workflow output would have vanished live in the app. Now forwarded
>    via `is_live_only()`.
> 2. **Keepalive**: the server never read the inbound socket, so it never answered the client's
>    Ping → OkHttp/Ktor would tear the WS down ~every 20s (reconnect loop). Now reads inbound.
>
> App re-check: open a session with a running workflow/subagent and confirm (a) subagent output and
> a retry banner appear live, and (b) the socket stays up through an idle stretch (no ~20s reconnect).

**Caveat (shared with the TS server):** each session's `CLAUDE_CONFIG_DIR` symlinks `.credentials.json`; claude's OAuth-token refresh can diverge it, so a burst of brand-new sessions can briefly hit `401 Invalid authentication` until the token settles. This is the TS architecture too (not Rust-specific) and didn't reproduce on a single valid session — but watch the first turns after cutover.

## 3. Cutover steps (on the deploy host, master checkout `~/src/agentic-dev`)

```bash
# 3.1 Get the Rust code (all phases are on master) and build the release binary.
cd ~/src/agentic-dev && git pull
cd ~/src/agentic-dev/server-rs && cargo build --release
test -x ~/src/agentic-dev/server-rs/target/release/agentic-dev-server && echo "binary OK"
# 3.1b Vendor the SDK bridge's runtime dep so it doesn't rely on the repo-root node_modules
#      (node resolves @anthropic-ai/claude-agent-sdk from server-rs/node_modules first).
cd ~/src/agentic-dev/server-rs && npm install --no-audit --no-fund
test -d ~/src/agentic-dev/server-rs/node_modules/@anthropic-ai/claude-agent-sdk && echo "bridge dep OK"

# 3.2 Smoke-test it WITHOUT touching the live service: run on a spare port against a COPY.
rm -rf /tmp/agentic-shadow && cp -a ~/.agentic-dev/. /tmp/agentic-shadow/
AGENTIC_DATA_DIR=/tmp/agentic-shadow AGENTIC_PORT=7799 AGENTIC_HOST=127.0.0.1 \
  AGENTIC_PASSWORD=shadowtest AGENTIC_AUTH_SECRET=shadow-secret AGENTIC_CLAUDE_BIN=/bin/true \
  ~/src/agentic-dev/server-rs/target/release/agentic-dev-server &
sleep 2; curl -s localhost:7799/healthz   # → ok
# (optionally repeat the §2 checks), then: kill %1

# 3.3 Point the systemd unit at the Rust binary via a drop-in (keeps the original unit intact).
mkdir -p ~/.config/systemd/user/agentic-dev.service.d
cat > ~/.config/systemd/user/agentic-dev.service.d/10-rust.conf <<'EOF'
[Service]
# Override the TS ExecStart. EnvironmentFile (AGENTIC_AUTH_SECRET/PASSWORD/PORT/...) is inherited
# from the base unit, so existing Android tokens keep working and the port is unchanged.
ExecStart=
ExecStart=/home/arcatva/src/agentic-dev/server-rs/target/release/agentic-dev-server
EOF

# 3.4 Apply + restart  (THIS interrupts in-flight turns — your call when).
systemctl --user daemon-reload
systemctl --user restart agentic-dev.service

# 3.5 Verify.
systemctl --user status agentic-dev.service --no-pager | head
curl -s localhost:7420/healthz                       # → ok  (7420 = the live port)
# Then open the Android app: the session list loads, a big transcript opens instantly,
# sending a message streams over WS. (Recovered in-flight turns show as interrupted — resume to retry.)
```

## 4. Rollback (instant, no data migration)

```bash
rm ~/.config/systemd/user/agentic-dev.service.d/10-rust.conf   # restores the TS ExecStart
systemctl --user daemon-reload
systemctl --user restart agentic-dev.service
curl -s localhost:7420/healthz
```
On-disk state written by the Rust server is read back fine by the TS server (same schema/log/JSON format).

## 5. Known follow-ups

> ⚠️ **Pre-cutover blocker found & fixed (2026-06-21):** `c0d15e3` committed `mod util;` +
> `use crate::util::{now_ms,now_secs}` across store/api but never `git add`ed `src/util.rs`
> (it lived as an untracked working-tree file; Chunks E/F were committed on top, then a
> worktree reset destroyed it). HEAD did **not compile** (`E0583: file not found for module
> util`) — `cargo build --release` would have failed at step 3.1. Restored in `6c2fc09`.
> Baseline is green again (`cargo test`: 216 passing).

### Resolved this pass (all on master, tests green)

- ✅ **`hasActiveWorkflow` reconcile** — `has_active_workflow` is now a predicate over
  `list_workflows` with trim+lowercase status normalisation, byte-exact with
  `workflows.ts:hasActiveWorkflow`. The old hand-rolled scan mis-counted a `"  Completed "`
  summary as active. `aa24b48`.
- ✅ **Phase-6 review minors** — `runCreatedAt` rounds to nearest ms and uses `birthtime||mtime`
  for the fallback stat; `commit_graph` runs its per-repo git reads concurrently (Promise.all
  parity), order preserved. `15ccf1d`.
  - *Not changed (resolved):* malformed-JSON PUT already returns the exact 400
    `{"error":"array of groups|templates required"}`; `list_remote_repos` "dropping nameless
    entries" is stricter-and-safer than the TS `arr.map(r=>r.name)` (which would emit
    `undefined`) and is unreachable with real `gh --json name` output — intentionally left.
- ✅ **gzip compression** — `tower-http` `CompressionLayer` (SizeAbove 2 KB) on all JSON routes;
  `/file` and the `/stream` WS upgrade are added outside the layer (the predicate can't see the
  request path). `b9b11a6`.
- ✅ **Watchdog / cgroup env vars** — `AGENTIC_TURN_IDLE_SEC` / `_WALL_SEC` / `AGENTIC_IDLE_TTL_SEC`
  (sec→ms) and `AGENTIC_MEM_*` / `AGENTIC_CPU_QUOTA` / `AGENTIC_TASKS_MAX` now parse in `Config`
  and thread into `EngineConfig` (parity `config.ts:52-60`). Unset → engine defaults (unchanged).
  `1240ada`.
- ✅ **`Engine::close()` graceful detach** — added `close_with(kill_running)`; `false` detaches
  in-flight runs (child keeps writing its log, finalized by next boot's `recover()`), parity with
  `close({killRunning})`. `close()` still defaults to kill. No production caller wires graceful
  yet (no SIGTERM handler; a cutover restart wants kill+interrupted+recover). `7172138`.

### Still deferred (deliberate — not done this pass)

- **`block_in_place` sync-over-async** — **deferred.** Correct as-is on the prod multi-thread
  runtime; the change is *not* the mechanical "add `.await`" it looks like: several `block_async`
  sites run inside a held `parking_lot::Mutex` guard (`state.lock()`), and a guard cannot be held
  across an `.await` — so async-ifying `submit_session`/`follow_up`/`kill` forces lock-scope
  restructuring across the engine + every route caller, for **zero behavioral change**. Not worth
  the blast radius/regression risk pre-cutover. Revisit as a standalone cleanup if desired.
- **FCM push** (`AGENTIC_FCM_*` / device token) — **deferred (manual check).** Exercised only with
  a faked HTTP sender; a real send needs a live device token + a backgrounded app, which isn't
  reproducible headlessly. Confirm one real push after cutover (run a turn to completion, app
  backgrounded).

- **Already done before this pass** (no longer gaps): the production **SDK runner** (Node bridge —
  AskUserQuestion works), and **sessionGuide** (multi-repo orientation CLAUDE.md).

## 6. Conformance suite (optional, stronger gate before cutover)

For a byte-for-byte gate, run the same request set against BOTH servers (Node on one port, Rust on another, each against its own copy of the state) and diff the responses. The §2 shadow run is the pragmatic subset; a full cross-impl diff harness is the remaining hardening if you want it before flipping production.
