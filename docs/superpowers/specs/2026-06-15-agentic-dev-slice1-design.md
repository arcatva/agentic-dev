# agentic-dev — Slice 1 (Engine + Thin Vertical) Design

**Date:** 2026-06-15
**Status:** Approved (design); pending spec review → writing-plans
**Project:** `agentic-dev` — an agent-driven local development platform. This spec covers
**only the first slice**: a working engine that drives headless Claude Code sessions in
git worktrees, plus the thinnest web UI/API to submit a request and watch it stream.

---

## 1. Goal

One LAN-accessible web app, running on the dev machine, that lets the user:

1. Pick a git repo under `~/src`.
2. Submit a natural-language request ("requirement") against it.
3. Have the platform create an isolated git **worktree** and spawn a **headless
   `claude` session** (`--dangerously-skip-permissions`) that works autonomously in that
   worktree.
4. Watch the session's output **stream live** in the browser; see status and cost.
5. Run **multiple such sessions concurrently** (including several against the same repo).

This slice proves the riskiest unknown — programmatically driving Claude Code and
streaming it to a browser — end to end, with the thinnest possible UI around it.

## 2. Scope

### In scope (Slice 1)
- Engine: worktree creation, headless `claude` spawn, stream-json parsing, lifecycle +
  cost tracking, bounded concurrency, kill.
- API: HTTP for repos/sessions CRUD-ish actions; WebSocket for live event streaming.
- Web UI (MD3): dashboard, new-request form, live session view.
- Auth: single shared password → bearer token; LAN bind.
- Persistence: SQLite for session records + per-session log files (reconnect/replay).
- Minimal one-click local start script (`scripts/dev.sh`).

### Deferred (later slices — NOT in this spec)
- **Multi-turn interaction** (follow-up messages into a live session). Engine captures
  `claudeSessionId` from day one so this is a clean future add via `--resume`.
- **Context/skill 下沉**: folding `tenants-dev-*` project skills into each repo's
  CLAUDE.md/docs; keeping only platform skills (rke2-ops, cloudstack-ops) + superpowers
  globally.
- **Worktree teardown / merge UX** (review, merge session branch, clean up).
- **Full packaging** (systemd user service / docker-compose), crash-survival of sessions
  across server restart (detach/reattach).
- **Android app** (will consume the same API over LAN).
- Multi-user / hardened auth.

## 3. Decisions locked

| Decision | Choice | Rationale |
|---|---|---|
| Driver | Headless CLI `claude -p`, **non-bare**, `cwd=worktree` | Most directly matches "a new session is established"; language-agnostic; crash-isolated. Non-bare so the repo's CLAUDE.md + global skills + superpowers load. |
| Interaction model | **One-shot autonomous** (capture `session_id` for future multi-turn) | Fastest end-to-end skeleton; multi-turn retrofits cleanly via `--resume`. |
| Backend stack | Node/TS (Fastify + `ws`) | Best alignment with Claude Code ecosystem + easy WS streaming; one language with the React front end. |
| Frontend stack | React + Vite + MUI (Material Design 3) | MD3 as requested; React now, revisit Flutter/React-Native when the Android app starts. |
| Persistence | SQLite (`better-sqlite3`) + per-session log files | Durable session list + log replay on reconnect; trivial to run locally. |
| Auth | Single password → bearer token, LAN/tailscale bind | Single-user local tool; Keycloak is overkill. |
| Location | New repo `~/src/agentic-dev` | Meta-tool above the three layers; not part of the federation. |

## 4. Verified facts — driving Claude Code headlessly

Confirmed against official docs (not from memory), 2026-06-15:

- **Stream:** `claude -p "<prompt>" --output-format stream-json --verbose --include-partial-messages`
  emits newline-delimited JSON, one event per line: `system/init` (carries `session_id`,
  model, tools, loaded plugins), `stream_event` (text deltas), `system/api_retry`
  (retryable-error progress), and a final `result`.
- **Cost/metadata:** `--output-format json` returns `session_id` and `total_cost_usd`
  (+ per-model breakdown) — used for cost tracking.
- **Multi-turn (future):** `--resume <session_id>` / `--continue`; session-id lookup is
  **scoped to the current project directory and its git worktrees** → fits worktree-per-request.
- **Concurrency:** multiple `claude -p` processes run independently, each with its own
  `session_id`; we run each in its own worktree dir.
- **Permissions:** `--dangerously-skip-permissions` runs autonomously (no prompts). The
  user has chosen this explicitly.
- **Do NOT use `--bare`:** it skips working-dir CLAUDE.md, skills, plugins, MCP, auto
  memory, and `~/.claude` — exactly the per-repo context this platform wants. Default is
  non-bare.
- **Background tasks** a session starts are terminated ~5s after the final result.
- **Cost caveat:** as of **2026-06-15**, Agent SDK and `claude -p` usage on subscription
  plans draws from a new **monthly Agent SDK credit**, separate from interactive usage →
  motivates a concurrency cap + visible cost.

Sources: <https://code.claude.com/docs/en/headless> ·
<https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan>

## 5. Architecture

```
Browser (React + MUI/MD3)
   │  HTTP (REST) + WebSocket (stream)
   ▼
API server (Node/TS — Fastify + ws)         ~/src/agentic-dev/server
   │  in-process calls
   ▼
Engine (orchestrator)                        server/engine
   ├─ Worktree manager   → git worktree add under ~/src/agentic-worktrees/<repo>/<id>
   ├─ Session spawner    → child_process: claude -p … (cwd = worktree)
   ├─ Stream parser      → newline-delimited JSON → events
   ├─ Concurrency pool   → bounded (default 3), queue overflow
   └─ Store              → SQLite (records) + per-session log files
```

The Engine is a standalone module with a clean interface, independent of the HTTP layer,
so it is unit-testable without a server:

```
submit(repo, prompt) -> sessionId
subscribe(sessionId) -> async event stream
list() -> Session[]
get(sessionId) -> Session + log
kill(sessionId) -> void
```

## 6. Engine — core detail

### Session record
| Field | Notes |
|---|---|
| `id` | server-generated UUID for this session |
| `repo` | repo name under `~/src` |
| `worktreePath` | `~/src/agentic-worktrees/<repo>/<id>` |
| `branch` | `agentic/<id>` off the repo's current HEAD |
| `claudeSessionId` | captured from `system/init` |
| `status` | `pending → running → done | failed | killed` |
| `prompt` | the submitted request |
| `costUsd` | from final `result` / json |
| `startedAt` / `endedAt` / `exitCode` | lifecycle |

### Lifecycle (one-shot autonomous)
1. **Validate**: repo exists under `~/src` and is a git repo; repo not in a broken state.
2. **Worktree**: `git -C ~/src/<repo> worktree add ~/src/agentic-worktrees/<repo>/<id> -b agentic/<id>`
   off current HEAD. (Lifts the core of the existing `new-session.sh`, generalized to any
   `~/src` repo, minus the ghostty/TUI launch.)
3. **Spawn**: `claude -p "<prompt>" --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions`,
   `cwd = worktreePath`, inheriting Anthropic auth from the environment. Non-bare.
4. **Parse stream**: read stdout line by line; on `system/init` capture `claudeSessionId`;
   forward every event to subscribers (WS) and append to the per-session log file; on
   final `result` capture `total_cost_usd` and set `status = done`.
5. **Concurrency**: bounded pool, default **3** concurrent (configurable). Excess queued
   (`status = pending`) and started as slots free.
6. **Kill**: SIGTERM the child; `status = killed`. (Background bash tasks self-terminate.)
7. **Completion**: leave worktree + branch in place for later review/merge (teardown is a
   later slice). Record `exitCode`.

## 7. API surface

All `/api/*` require a bearer token except `POST /api/login`.

| Method / path | Purpose |
|---|---|
| `POST /api/login {password}` | → `{token}` |
| `GET /api/repos` | enumerate git repos under `~/src` |
| `POST /api/sessions {repo, prompt}` | create worktree + spawn → `{id}` |
| `GET /api/sessions` | list with status + cost |
| `GET /api/sessions/:id` | detail + persisted log (replay) |
| `WS /api/sessions/:id/stream` | on connect backfill from log, then live events |
| `DELETE /api/sessions/:id` | kill |

## 8. Web UI (MD3, minimal)

- **Dashboard**: repo cards + active/recent sessions (status chips + cost + running total).
- **New request**: pick repo, prompt textarea, submit → navigate to session view.
- **Session view**: live streaming log (render `text_delta`s; collapse tool-use events),
  status, cost, kill button. On reconnect, replay persisted log then resume live.
- MUI themed to a Material Design 3 palette. React + Vite.

## 9. Error handling

| Failure | Handling |
|---|---|
| `claude` not on PATH / not authenticated | spawn fails → `status = failed`, capture stderr, surface in UI |
| `system/api_retry` events | forward to UI (retry progress) |
| Worktree create fails (dirty/locked/exists) | reject `POST /api/sessions` with a clear error |
| WebSocket disconnect | client reconnects; server backfills from log file |
| Server restart with `running` sessions | mark orphaned sessions `unknown` (children don't survive restart in v1; detach/reattach deferred) |

## 10. Testing

- **Engine unit tests** use a **fake `claude` binary** (a shell script that emits canned
  stream-json on a configurable cadence) — exercises parse, lifecycle, concurrency, kill
  with **zero API cost**. This is the primary test strategy.
- **API integration tests** run against the engine wired to the fake binary.
- **One manual smoke test**: a real `claude -p` one-shot against a throwaway repo, to
  confirm the real binary + auth + streaming path.

## 11. Repo layout

```
~/src/agentic-dev/
  server/
    engine/      # worktree mgr, spawner, stream parser, pool, store
    api/         # Fastify routes + ws + auth
    test/        # unit + integration; fixtures/fake-claude.sh
  web/           # React + Vite + MUI (MD3)
  scripts/
    dev.sh       # install deps, build web, start server on a fixed LAN port
  docs/superpowers/specs/   # this spec
  CLAUDE.md
  README.md
```

## 12. Cost guardrail

Concurrency cap (default 3, configurable) + per-session `total_cost_usd` + a running total
shown in the dashboard — to keep the new monthly Agent SDK credit from being silently
drained by concurrent autonomous sessions.

## 13. Success criteria (definition of done for Slice 1)

1. From a browser on the LAN, log in, pick a `~/src` repo, submit a prompt.
2. A worktree is created and a headless `claude` session spawns in it.
3. The browser shows the session's output streaming live, then a final result + cost.
4. Two sessions against the same repo run concurrently without clobbering each other.
5. Killing a session terminates its process and reflects `killed` in the UI.
6. Engine unit + API integration tests pass against the fake `claude` binary.
7. `scripts/dev.sh` brings the whole thing up on the dev machine in one command.

## 14. Tunables to confirm during writing-plans
- Concurrency default (3) and whether it's per-repo or global.
- Worktree root path (`~/src/agentic-worktrees/…`) and branch naming (`agentic/<id>`).
- Fastify vs. alternative; SQLite vs. a JSON store for v1.
- Fixed LAN port + whether to bind to the tailscale IP specifically.
