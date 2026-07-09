# Crash-Survival for agentic-dev sessions — Implementation Plan

> Execute task-by-task with TDD. Steps use `- [ ]` for tracking.

**Goal:** Spawned `claude` turns survive an `agentic-dev` service restart (deploy or crash); the platform reattaches and keeps streaming.

**Architecture:** Each turn runs in a transient `systemd --user` service (`agentic-<id>.service`) — its own cgroup, so restarting the platform service does not kill it. `claude`'s stream-json goes straight to the session log file via systemd `StandardOutput=append:`. The platform no longer reads a stdout pipe; instead it **tails the log file** for live events and uses `systemctl` for liveness / exit code / kill. On startup it **reattaches** to still-active units and finalizes dead ones.

**Why this shape:** the durable artifact is the file. Whoever is alive (the original process or a freshly-restarted one) just tails the file + asks systemd whether the unit is still active. No parent-child pipe to break.

**Tech:** `systemd-run --user`, `systemctl --user`, Node fs polling tailer. A `Runner` abstraction keeps unit tests systemd-free (a `localRunner` detached-spawns to the same file).

## Risks & recovery
- **Breaks live streaming for ALL sessions** (tailer replaces pipe parsing). Mitigation: tailer is unit-tested in isolation with the fake claude before wiring; verified end-to-end after.
- **systemd env**: transient units don't inherit the platform's full env. Forward `process.env` via repeated `--setenv` (argv, no shell quoting). Verified by the real restart test (claude fails loudly if a var is missing).
- **Unit name reuse across turns**: `reset-failed` + `CollectMode=inactive-or-failed` before each start.
- **Recovery if it regresses**: the change is isolated to `runner/tailer/spawner` + engine wiring; revert the commit and `localRunner`/pipe behavior returns. No data migration.

## Files
- `server/engine/runner.ts` (NEW) — `Runner` interface; `localRunner` (detached spawn → file, PID liveness) and `systemdRunner` (transient unit). `RunHandle { isActive(), exitCode(), stop() }`.
- `server/engine/tailer.ts` (NEW) — poll a file from a byte offset, `parseLine` complete lines → callback; expose current offset.
- `server/engine/spawner.ts` (REWRITE) — `spawnClaude(opts, runner)` wires runner + tailer into the existing `SpawnHandle` (`'event'`/`'exit'`/`kill()`); add `reattach(opts, runner)`.
- `server/engine/engine.ts` — supply logPath + runner; drop per-event `appendLog` (systemd writes the file; keep the `agentic_prompt` marker append); `reconcileOrphans` → reattach active units / finalize dead.
- `server/api/config.ts`, `server/index.ts` — select `systemdRunner` in prod, `localRunner` otherwise (injectable for tests).
- `agentic-dev.service` — no change required (transient units are separate cgroups); documented.

## Stages
1. **Runner + tailer (systemd-free, TDD).** `localRunner` + tailer with the fake claude; prove file-tail streaming + exit + kill. Low risk.
2. **Spawner rewrite** to use runner+tailer behind the same `SpawnHandle`; keep all existing engine/stream tests green with `localRunner`.
3. **Reattach** on startup; engine wiring; drop per-event appendLog.
4. **systemdRunner** (prod) + config selection.
5. **Verify**: full tests green; controlled real test — start a session, `systemctl --user restart agentic-dev` mid-run, confirm it keeps running and streaming, finishes, transcript intact.
