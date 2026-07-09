# Orchestration modes + multi-agent SessionView — design

**Goal:** Let a session run with workflow/multi-agent orchestration (a Mode control), and view the agents it spawns in a left-list / right-detail (master-detail) layout.

## Background (verified by probing headless `claude -p`)
- Headless sessions load both `Task` (subagents; the tool appears as name `Agent` in the stream) and `Workflow`.
- **Subagent activity is tagged with `parent_tool_use_id`** = the id of the spawning `Agent` tool_use. Main-agent events have `parent_tool_use_id: null`. → events can be grouped into per-agent columns. (Confirmed: a `user` message carrying the subagent's prompt and the subagent's turns are tagged with the parent id; the final `tool_result` returns to the parent.)
- **Workflow runs in a background channel** (`/workflows`), NOT in the main stream-json. Headless only sees the `Workflow` tool_use + a completion notification. So v1 renders a workflow as a single **status node**, not per-internal-agent.
- **"ultracode" has no CLI flag** — it is a prompt/standing opt-in. Workflow usage is one axis: `Normal → Workflows-allowed → Ultra-code (standing, aggressive)`. Ultra-code ⊇ manual-workflow, so they are not independent toggles → a single Mode control.

## 1. Mode control (NewRequest)
A single **Mode** dropdown next to Model/Effort:
| Mode | Injected preamble (delimited, appended to the turn prompt) |
|---|---|
| Normal (default) | nothing |
| Workflows | "Workflows are enabled for this session — use the Workflow tool when a task genuinely benefits from multi-agent orchestration." |
| Ultra-code | "ultracode is ON for this session — author and run a workflow for every substantive task by default; fan out and adversarially verify. Token cost is not a constraint." |

- Persisted on the session (`store` column `mode`), passed through the spawner, injected as a clearly-delimited preamble on each turn (so follow-ups keep the mode). Shown as a chip on SessionView.
- Implemented in `spawner.buildSpec` (prepend preamble to the prompt) — keeps it out of the args/flags.

## 2. Stream model (backend)
- `streamParser`: attach `parentToolUseId` (from the raw line's top-level `parent_tool_use_id`) to `text` / `skill` / `ask` events so the UI can route each part to the right agent column.
- New events from an assistant message's tool_use:
  - `Agent` → `{ kind: "agent", id, agentType, description, parentToolUseId }` (a subagent was spawned).
  - `Workflow` → `{ kind: "workflow", id, name, parentToolUseId }` (+ later a status from its tool_result).
- Engine emits these to subscribers; no new persistence (raw log already has `parent_tool_use_id`; `renderFromLog` reads it directly).

## 3. SessionView — master-detail
- Build an **agent-node tree** keyed by tool_use id: root = main agent (`parentToolUseId == null`); each `agent` event adds a child node (label = `agentType` + truncated `description`); every part (text/skill/ask) routes to the node named by its `parentToolUseId` (or main).
- **Left column** (vertical list): Main agent + nested subagent nodes (indented one level), each row = status dot + label + last-activity hint. Workflow nodes show a ⚙ + run status.
- **Right pane**: the selected node's parts rendered exactly as today (markdown / skill chips / ask panel). The follow-up box + worktree actions stay below, tied to the session (not a node).
- Default selection = main agent; live, new nodes appear as they're spawned and the count updates; clicking a node switches the right pane.
- **Responsive**: on narrow screens collapse the left list into a top dropdown/segmented selector above the content.
- Back-compat: a session with no subagents shows just "Main agent" — visually ~the current single-pane view.

## Phases
1. **Backend** — `parentToolUseId` on events; `agent` + `workflow` events; parser tests.
2. **Mode** — NewRequest dropdown; `store.mode` (+ migration); spawner preamble; engine plumb; tests.
3. **SessionView master-detail** — node tree from parts; left list + right pane; responsive; ask/skill still work per-node.
4. **Verify** — real session that spawns subagents (and an ultra-code workflow); left list populates, clicking expands; survives restart (reattach already covered).

## Risks / non-goals
- Subagent *internal* assistant turns may or may not stream (depends on claude verbosity); at minimum a subagent shows its input prompt + final result. Acceptable for v1.
- Workflow internals are not visible headless (documented) — single status node only.
- Ultra-code enablement is heuristic (keyword/preamble), not a hard flag.
