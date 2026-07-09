# agentic-dev — Slice 2 (Multi-turn interaction) Design

**Date:** 2026-06-16
**Status:** Approved (design). Proceeding to plan/implement under the user's standing "keep going".
**Builds on:** Slice 1 (`2026-06-15-agentic-dev-slice1-design.md`). Slice 1 already captures
`claudeSessionId` per session and appends a per-session log — the hooks this slice needs.

## Goal

Let a **finished** session receive follow-up prompts that continue the **same** Claude
conversation in the **same git worktree** (turn-based), streaming each new turn live, with
cost accumulating across turns.

## Scope

**In:** `spawnClaude` `--resume`; `engine.followUp`; `POST /api/sessions/:id/messages`;
WS `?live=1` (skip backfill); web `api.followUp` + a follow-up input on `SessionView`; cost
accumulation; tests.

**Out (deferred):** queue-a-follow-up-while-running; branching/forking a conversation;
editing or retrying a prior turn; multi-user.

## Decisions locked

- **Turn-based:** a follow-up is allowed only when the session is terminal (`done`/`failed`/
  `killed`). Sending to a `running`/`pending` session → `400 busy`.
- **Same worktree + `--resume <claudeSessionId>`** — continues the same conversation with the
  first turn's context. No new worktree is created for a follow-up.
- **Cost accumulates:** on each turn's `result`, `costUsd += turn.total_cost_usd` (Slice 1
  overwrote; this slice changes it to add).
- **New turn streams via WS `?live=1`** (no re-backfill). The prior transcript is already on
  screen from the REST `log` (Slice 1's reload fix), so the WS only tails the new turn —
  no duplicate history, no backfill flash.
- **No new `Session` fields.** Reuse `claudeSessionId` / `costUsd` / `status`. The per-session
  log append IS the persisted multi-turn transcript.

## Components / changes

| File | Change |
|---|---|
| `server/engine/spawner.ts` | `SpawnOptions` gains optional `resumeSessionId`. When set, args become `["-p", prompt, "--resume", resumeSessionId, ...BASE_ARGS]`. Otherwise unchanged. |
| `server/engine/engine.ts` | `QueueItem` gains optional `resumeSessionId`. New `followUp(id, prompt)`: validate session exists, is terminal, and has `claudeSessionId`; reuse `session.worktreePath` (no `createWorktree`); enqueue `{id, repoPath, prompt, resumeSessionId: session.claudeSessionId}`; `pump()`. In `start`, pass `resumeSessionId` to `spawnClaude`. On `result`, accumulate: `costUsd = (cur.costUsd ?? 0) + (ev.costUsd ?? 0)`. |
| `server/api/routes.ts` | `POST /api/sessions/:id/messages {prompt}` → `engine.followUp(id, prompt)`; 404 unknown, 400 on busy/no-resume/missing-prompt. |
| `server/api/stream.ts` | Honor `?live=1` (or `live=true`): skip the backfill loop; subscribe to live events only. (Auth + close-on-end unchanged.) |
| `web/src/api.ts` | `followUp(id, prompt)` (POST messages); `openStream(id, onEvent, onClose, opts?)` gains `{ live?: boolean }` → appends `&live=1`. |
| `web/src/pages/SessionView.tsx` | When session is terminal, show a follow-up input (TextField + Send). On send: POST message; optimistically set status `running`; open the WS with `live: true` and append the new turn's `text`/`retry`/`result` under the existing transcript; on the turn's `engineExit`, refetch session (status/cost). |

## Data flow (a follow-up turn)

```
session is done (transcript shown from REST log)
  → user types follow-up, clicks Send
  → POST /api/sessions/:id/messages {prompt}
  → engine.followUp: validate terminal + has claudeSessionId
       enqueue { id, repoPath, prompt, resumeSessionId: claudeSessionId }  (reuse worktreePath)
       status → running
  → start: spawnClaude({ cwd: worktreePath, prompt, resumeSessionId })
       → claude -p "<prompt>" --resume <claudeSessionId> … --dangerously-skip-permissions
  → stream events: append to same log, emit to subscribers
       on result: costUsd += turn cost
  → exit: status → done
client: on Send → open WS ?live=1 → append new turn's text/result beneath prior transcript
```

## Error handling

- `followUp` on a `running`/`pending` session → throw `session busy` → `400`.
- `followUp` on a session with no `claudeSessionId` → throw → `400`.
- `followUp` on an unknown id → `404`.
- Resume-turn spawn failure → `status failed`, stderr captured (Slice 1 path, unchanged).
- Kill during a turn → `killed` (Slice 1 path, unchanged).

## Testing (fake `claude` binary, zero API credit)

- **spawner:** with `resumeSessionId`, the spawned argv includes `--resume <id>` (use an
  args-recording fake, or assert on the constructed args).
- **engine.followUp:** re-runs a `done` session **in the same worktree** (no new worktree
  directory created); status cycles `done → running → done`; `costUsd` accumulates across two
  turns; the log grows; rejects when the session is `running`; rejects when `claudeSessionId`
  is null.
- **stream `?live=1`:** connecting with `live=1` delivers no backfill frames — only live
  events.
- **routes:** `POST /api/sessions/:id/messages` → `200` (resumes); busy → `400`; unknown id
  → `404`; missing prompt → `400`.

## Success criteria

1. In the browser, a `done` session shows a follow-up input; sending streams a new turn live,
   which lands `done` with **accumulated** cost.
2. The follow-up **continues the same conversation** (resume) in the same worktree — the agent
   has the first turn's context.
3. Reloading shows the **full multi-turn transcript** (all turns) instantly from the REST log.
4. Sending a follow-up to a `running` session is rejected (busy).
5. All backend tests green on the fake binary.

## Verification (non-disruptive to the live instance)

Build + tests + a Playwright multi-turn check run on a **separate port** (not `:7420`). The
live `:7420` instance is **not** restarted until the user approves a redeploy.
