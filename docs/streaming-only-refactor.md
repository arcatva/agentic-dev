# Streaming-only refactor (classic one-shot backend removed)

The platform is streaming-only. The classic one-shot backend (a fresh `claude -p <prompt>` process per
turn, runnable under the systemd transient-unit runner) is removed entirely.

## What goes away

**Backend (`agentic-dev`)**
- `runner.ts`: delete `systemdRunner` (classic-only — it cannot stream). Simplify `localRunner` to always
  pipe stdin (drop the one-shot `stdio: ignore` + `unref` branch). `Runner.reattach()` is removed —
  `localRunner` never reattached, and the only reattaching runner (systemd) is gone.
- `spawner.ts`: `buildSpec` always builds the streaming argv (`-p --input-format stream-json [--resume]`);
  the prompt is always delivered over stdin. `RunHandle.write`/`endInput` become non-optional.
- `config.ts`: drop `AGENTIC_STREAMING` and the `AGENTIC_RUNNER` runner selection; always `localRunner`.
- `types.ts`: drop `EngineConfig.streaming`; `Session.awaitingInput` is always present.
- `engine.ts`: drop the `this.streaming` flag and every `if (this.streaming)` guard. The `awaiting`
  (idle/busy) map stays — it is now always tracked. `recover()` drops the reattach attempt and only
  finalizes from the log (done if the log ends in a `result`, else `interrupted`).

**Client (`agentic-dev-android`)**
- Delete `domain/BackendMode.kt` (streaming-vs-classic detector).
- `Status.kt` `statusVisual`: drop the classic interpretation; `running + awaitingInput=true` = idle.
- `SessionViewModel.kt`: delete the client-side drain queue (`startDrain`/`queued`/`queueError`);
  `submit()` is always a streaming inject; `inputLocked` is always false; `answerAsk` always resumable.
- `SessionScreen.kt`: drop the "queued" card and the locked "Working…" placeholder path.

## Capability removed (accepted)

**systemd restart-survival is gone.** Classic + the systemd runner ran each turn in its own transient
cgroup unit, so a platform restart left turns alive and reattached them on boot. Streaming needs a live
stdin pipe (local child), which dies with the server. So a platform restart now terminates all running
turns; `recover()` finalizes them (`interrupted`) and the user resumes (`--resume` restores context).
This was already the de-facto behavior once streaming became the default; the refactor makes it permanent
(no `AGENTIC_STREAMING=0` escape hatch). Getting survival back would require "Pass 2" (streaming over a
systemd FIFO) — a separate, unbuilt feature.

## Test strategy

The bulk of `engine.test.ts` runs through `makeEngine` (which defaulted to classic). Rather than rewrite
every test, **flip `makeEngine` to streaming and fix only the tests that actually break.** Most pass
unchanged because the one-shot `fake-claude.sh` ignores stdin and exits after its `result`, so a streaming
spawn of it still reaches `done`. Only tests asserting classic-specific semantics (the "session busy"
follow-up throw, one-shot concurrency timing) need adjustment. `engine.streaming.test.ts` becomes the
canonical session-loop test; `engine.recover.test.ts` already uses the local runner and keeps testing the
finalize-from-log path. `runner.test.ts`/`spawner.test.ts`/`config.test.ts` drop their systemd/one-shot/
flag cases.

## Order

config + types → runner (delete systemd) → spawner → engine → flip test fixture + fix failures → client →
docs. Verify `cargo test` green before the client; `gradle test` + APK for the client.
