# Orchestration modes + multi-agent SessionView — Implementation Plan

> Execute task-by-task with TDD. Spec: docs/superpowers/specs/2026-06-17-orchestration-modes-multiagent-view-design.md

**Goal:** A session Mode control (Normal / Workflows / Ultra-code) + a master-detail SessionView that lists spawned agents (left) and shows the selected agent's transcript (right).

**Architecture:** Stream events carry `parentToolUseId`; new `agent`/`workflow` events mark spawned subagents/workflows. The frontend groups parts into an agent-node tree keyed by tool_use id (main = null parent). Mode injects an opt-in preamble into the turn prompt.

---

## Phase 1 — Backend: parentToolUseId + agent/workflow events

**Files:** `server/engine/streamParser.ts`, `server/engine/types.ts`, `server/engine/tailer.ts`, `server/api/stream.ts`, `streamParser.test.ts`, `tailer.test.ts`.

- [ ] `parseLine` returns `ClaudeEvent[]` (0..n per line) so one assistant message can yield multiple tool events. Update callers: `tailer.poll` (flatten), `stream.ts` backfill (`parseLine(line).forEach(...)` with fallback to a `backfill` event when empty).
- [ ] Every event gains `parentToolUseId?: string | null` (read from raw top-level `parent_tool_use_id`). text/skill/ask carry it.
- [ ] Assistant message tool_use detection (one pass): collect Skill→names, AskUserQuestion→questions, `Agent`→{id,agentType:input.subagent_type,description:input.description}, `Workflow`→{id,name}. Emit a `skill`/`ask`/`agent`/`workflow` event for each present kind.
- [ ] Types: `{kind:"agent", agents:{id,agentType,description}[], parentToolUseId, raw}`, `{kind:"workflow", id, name, parentToolUseId, raw}`.
- [ ] Tests: array return; parentToolUseId extracted; Agent tool_use → agent event with subagent_type/description; Workflow → workflow event; existing skill/ask/text still pass.

## Phase 2 — Mode control

**Files:** `server/engine/types.ts` (Session.mode), `server/engine/store.ts` (col + migration + create/rowToSession), `server/engine/spawner.ts` (preamble), `server/engine/engine.ts` (plumb), `server/api/routes.ts`, `web/src/api.ts`, `web/src/pages/NewRequest.tsx`, tests.

- [ ] `Session.mode: string | null` ("normal"|"workflows"|"ultra"|null). Store col `mode` + ADDED_COLUMNS migration + create/rowToSession.
- [ ] `spawner.buildSpec`: if mode set, prepend a delimited preamble to `prompt` (Normal=none; Workflows=enable text; Ultra=ultracode standing opt-in text).
- [ ] `engine.submitSession` meta gains `mode`; `spawnOpts` passes `mode` from the session.
- [ ] route POST /api/sessions reads `mode`; `api.submit(...mode)`; NewRequest Mode `<Select>` (Normal/Workflows/Ultra-code); SessionView shows a mode chip.
- [ ] Tests: spawner prepends the right preamble per mode; server persists mode.

## Phase 3 — SessionView master-detail

**Files:** `web/src/pages/SessionView.tsx`, `web/src/api.ts`.

- [ ] Replace flat `turns` with an agent-node model: `Node {id, parentId, label, kind:'main'|'agent'|'workflow', turns: Turn[], status?}`. Build from events: `agent` event → add child node(s); parts route to the node whose id === part.parentToolUseId (else main). renderFromLog + applyLive build the same tree.
- [ ] Left list: Main + nested agent nodes (indent by depth) + workflow nodes (⚙ + status); selected highlighted; live count. Right pane: selected node's turns (existing text/skill/ask rendering, per-node). Follow-up + worktree actions stay session-level below.
- [ ] Responsive: narrow → left list becomes a top selector.
- [ ] Default select main; new nodes appear live; clicking switches pane.

## Phase 4 — Verify

- [ ] yarn build + web build + yarn test green.
- [ ] Restart; real session in Ultra-code that spawns subagents (Task) → left list shows Main + subagents, clicking expands each; workflow shows as status node. Survives restart.
- [ ] Screenshots; commit + push per phase.

## Risks
- parseLine→array touches core streaming — phase 1 keeps all existing tests green before moving on.
- Subagent internal turns may not stream (min: input+result). Workflow internals not visible. Ultra-code is preamble-heuristic.
