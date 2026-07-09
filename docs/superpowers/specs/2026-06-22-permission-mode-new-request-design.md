# Per-session permission mode on new request — design

**Date:** 2026-06-22
**Status:** Draft, pending user review
**Repos touched:** `agentic-dev` (backend) and `agentic-dev-android` (the only client).
**Related:** introduces a field alongside the orchestration `mode` from
`2026-06-19-official-ultracode-design.md`. The two are independent and must not be conflated.

## Goal

Let the user choose, when creating a new request, the **permission mode** the Claude session
launches in — Dangerous (bypass) / Plan / Accept edits / Ask — instead of today's hardcoded,
always-on `--dangerously-skip-permissions`. The chosen mode is applied for real by the backend, and
`Ask` / `Accept edits` / `Plan` surface their permission/plan prompts to the app and wait for the
user, reusing the existing `AskUserQuestion` round-trip.

## Background

- The backend is non-interactive: it spawns one persistent `claude` per session through the official
  Agent SDK (`server/engine/sdkRunner.ts`, the sole production runner). The SDK's `canUseTool` hook
  currently **parks only on `AskUserQuestion`** and **auto-allows every other tool**
  (`sdkRunner.ts:56–65`) — the "same effect as `--dangerously-skip-permissions`".
- The legacy raw-CLI path (`spawner.ts` `BASE_ARGS`, `runner.ts` `localRunner`) hardcodes
  `--dangerously-skip-permissions`. It survives only as the engine/spawner **test seam** (fake claude).
- There is already an end-to-end "ask the user and wait" round-trip: `canUseTool` parks → the
  assistant `tool_use` is in the session log → `streamParser` emits `kind:"ask"` → the engine flags
  `pendingAsk` (watchdog-exempt) and streams it over WS → the Android app renders an ask card →
  the user's answer returns via `POST /api/sessions/:id/messages` → `sdkRunner.write()` resolves the
  parked `canUseTool`. **This spec builds a parallel `perm` / `plan` track on the same machinery.**
- The orchestration `mode` field (`null | "ultracode"`) is a **different** concept (xhigh + dynamic
  workflows). We do **not** overload it.

## Decisions (confirmed with the user)

1. **Scope = full build, end-to-end, all four modes genuinely functional** — including the net-new
   per-tool allow/deny permission card UI in the app (not a phased stub).
2. **New separate field `permissionMode`** (not the orchestration `mode`).
3. **Picker = a 4-notch `SliderField`** labeled "Permissions", ordered left→right by ascending
   autonomy: `Plan · Ask · Accept edits · Dangerous`. Default thumb = **Dangerous** (preserves
   today's behavior).
4. **Plan approval = surface the plan for approval**; on approve, the session continues executing in
   **`default` (Ask)** — every subsequent tool still prompts via a permission card.
5. **permissionMode is session-level** (set at creation, like model/effort/mode); follow-ups inherit
   it. Changing it per-turn is out of scope.

## The field

`permissionMode: string | null`. Canonical values match the Claude Code SDK `PermissionMode` union so
they map straight through to the SDK option / CLI flag:

| UI label (slider notch) | stored value          | behavior                                                        |
|-------------------------|-----------------------|-----------------------------------------------------------------|
| Plan                    | `"plan"`              | read-only; ends by surfacing a plan for approval                |
| Ask                     | `"default"`           | every tool needing permission prompts the user                  |
| Accept edits            | `"acceptEdits"`       | edits auto-accepted; non-edit tools (Bash/WebFetch…) prompt     |
| Dangerous               | `"bypassPermissions"` | auto-allow everything — **today's behavior**                    |

**Back-compat:** `null` / absent ⇒ treated as `bypassPermissions`. Existing sessions, templates, and
the current default are unchanged. A `normalizePermissionMode()` in `store.rowToSession` (mirroring
`normalizeMode`) maps any unknown/legacy value to `null` (= bypass).

## Architecture / changes by layer

### Backend (`~/src/agentic-dev`)

**Plumbing (mechanical, mirrors `mode` exactly):**

- **`server/engine/types.ts`** — add `permissionMode: string | null` to `Session` (with a comment
  documenting the value space).
- **`server/engine/store.ts`** — add `permissionMode` to `CreateInput`; add `permissionMode TEXT` to
  `COLUMNS`; add `["permissionMode","TEXT"]` to `ADDED_COLUMNS` (idempotent migration for existing
  DBs); set it in `create()`'s INSERT; read it in `rowToSession` via `normalizePermissionMode(r.permissionMode)`.
- **`server/engine/engine.ts`** — `submitSession(..., meta)` gains `meta.permissionMode`; pass to
  `store.create`. `spawnOpts` forwards `permissionMode: s.permissionMode`.
- **`server/engine/spawner.ts`** — add `permissionMode?: string | null` to `SpawnOptions`; thread into
  `buildSpec`'s returned `RunSpec`.
- **`server/engine/runner.ts`** — add `permissionMode?: string | null` to `RunSpec`.
- **`server/api/routes.ts`** — add `permissionMode?` to the `POST /api/sessions` body and the
  `POST /api/templates/start` body; thread into `submitSession(..., { model, effort, mode, permissionMode })`.
  Template fallback `b.permissionMode ?? tpl.permissionMode ?? null`.
- **`server/engine/templates.ts`** — add `permissionMode?` to the `Template` type.

**The interactive permission round-trip (`server/engine/sdkRunner.ts` — the core new work):**

- Generalize the single pending-`ask` slot into one **pending** slot:
  `{ kind: "ask" | "perm" | "plan", resolve, input, toolName, id }`. Only one is ever live at a time —
  the turn blocks on exactly one `canUseTool` call.
- Set the SDK `permissionMode` option from `spec.permissionMode`, **except for bypass**: when bypass
  (or `null`), leave `permissionMode` unset and keep `canUseTool` auto-allowing everything except
  `AskUserQuestion`. *Rationale:* setting `permissionMode:"bypassPermissions"` stops the SDK calling
  `canUseTool` at all, which would break the existing `AskUserQuestion` parking. Bypass therefore =
  exactly today's code path, unchanged.
- New `canUseTool` logic for the non-bypass modes:
  - `AskUserQuestion` → park as `kind:"ask"` (existing behavior).
  - `ExitPlanMode` (only reachable in plan mode) → park as `kind:"plan"`.
  - any other tool the SDK routes to `canUseTool` (all tools in `default`; non-edit tools in
    `acceptEdits`) → park as `kind:"perm"`.
- On parking a `perm`/`plan`, append a **synthetic log line** (same channel the engine already uses
  for `agentic_prompt` — the runner→engine→client path is the log file):
  ```json
  {"type":"agentic_perm","permKind":"perm","tool":"Bash","input":{…},"id":"<reqId>","at":<ms>}
  {"type":"agentic_perm","permKind":"plan","plan":"<markdown>","id":"<reqId>","at":<ms>}
  ```
  (`agentic_prompt` is already a precedent for engine/runner-authored synthetic lines that
  `streamParser` turns into events. `Date.now()` is available in `sdkRunner` — this is normal server
  code, not a workflow script.)
- Resolution: the parked promise is resolved by a new `respondPermission(decision, feedback?)` on the
  `RunHandle` (see endpoint below):
  - `perm` allow → `{behavior:"allow",updatedInput:input}`; deny → `{behavior:"deny",message:feedback ?? "denied by user"}`.
  - `plan` approve → resolve `ExitPlanMode` allow **and** transition the live query to `default`
    (`q.setPermissionMode("default")`) so execution proceeds but each tool still prompts; "keep
    planning" → `{behavior:"deny",message:feedback}` (Claude stays read-only).
- Extend the existing safety cleanup that denies a stranded `ask` on `interrupt()` and on the pump
  `finally` (query died mid-question) to cover the generalized pending slot — no leaked promises.

**`server/engine/streamParser.ts`:**

- Parse `type:"agentic_perm"` into new `ClaudeEvent`s:
  - `{ kind:"perm", tool, input, id, raw }`
  - `{ kind:"plan", plan, id, raw }`
- Add both kinds to the `ClaudeEvent` union in `types.ts`.

**`server/engine/engine.ts` bookkeeping:**

- Add a `pendingPerm: Set<string>` mirroring `pendingAsk`; set it on `kind:"perm"`/`kind:"plan"`,
  clear it on `result` / interrupt / exit / answer. Generalize the watchdog exemption (`tickWatchdog`,
  `:97`, `:109`) and `forgetSession` (`:417`) to treat `pendingPerm` like `pendingAsk`.
- Add `respondPermission(id, decision, feedback?)` → `this.running.get(id)?.respondPermission(...)`,
  and clear `pendingPerm` (a later `perm`/`plan` event re-arms it), mirroring how `followUp` clears
  `pendingAsk` (`:247`).

**`server/api/routes.ts` — answer channel (dedicated endpoint, not overloading the prompt):**

- `POST /api/sessions/:id/permission` body `{ decision: "allow" | "deny", feedback?: string }` →
  `engine.respondPermission(id, decision, feedback)`. Mirrors the `/interrupt` endpoint shape.
- (WS stream and `streamParser` carry the new `perm`/`plan` events to the client unchanged.)

### Android (`~/src/agentic-dev-android`)

**Plumbing:**

- **`data/net/Models.kt`** — add `permissionMode: String? = null` to `Session`, `NewSessionReq`, and
  `Template`. Add a `PermDecisionReq(decision: String, feedback: String? = null)` for the new endpoint.
- **`ui/newrequest/NewRequestViewModel.kt`** — add `permissionMode: String? = null` to
  `NewRequestUiState`; add `setPermissionMode(String?)`; include it in the `NewSessionReq` built in
  `submit()`; copy `t.permissionMode` in `applyTemplate`.

**New-request picker (`ui/newrequest/NewRequestScreen.kt`):**

- Add a 4-notch `SliderField` labeled **Permissions**, placed near the Model/Effort sliders, options
  ordered `Plan · Ask · Accept edits · Dangerous`:
  ```kotlin
  private val PERMISSION_MODES = listOf(
      "plan" to "Plan",
      "default" to "Ask",
      "acceptEdits" to "Accept edits",
      "bypassPermissions" to "Dangerous",
  )
  ```
  `value = s.permissionMode ?: "bypassPermissions"` (default notch = Dangerous);
  `onSelect = { realVm.setPermissionMode(it) }`. No accent animation (that's reserved for ultracode).

**Session screen permission/plan cards (parallel to the ask card):**

- **`domain/Node.kt`** — add `PermNode(tool, input, id, decided=false, decision="")` and
  `PlanNode(plan, id, decided=false, decision="")`.
- **`domain/Transcript.kt`** — in `applyEvent` (live, ~`:198`) handle `kind:"perm"` → `PermNode`,
  `kind:"plan"` → `PlanNode`; in `buildFromLog` (~`:134`) handle `type:"agentic_perm"`. Add both to
  `frameBusy` as "awaiting user" (busy = true, like `ask`).
- **`ui/session/Transcript.kt`** — add `PermCardView` (tool name + input summary, **Allow** /
  **Deny** with optional feedback) and a plan-approval card (plan markdown, **Approve & run** /
  **Keep planning** with feedback), reusing `AskCardView`'s expand/collapse, optimistic-answered, and
  countdown patterns.
- **`ui/session/SessionViewModel.kt`** — add `respondPermission(node, decision, feedback?)` mirroring
  `answerAsk` (optimistic mark + rollback on failure).
- **`data/repo/SessionsRepository.kt` + `data/net/KtorAgenticApi.kt`** — add `respondPermission(id,
  decision, feedback)` → `POST /api/sessions/:id/permission`.

### CLI mapping + docs

- **`server/engine/spawner.ts`** — pull `--dangerously-skip-permissions` **out of unconditional
  `BASE_ARGS`**. In `buildSpec`: bypass/`null` → push `--dangerously-skip-permissions`; else push
  `--permission-mode <plan|acceptEdits|default>`. (This path is the test seam, but keep it correct.)
- **`agentic-dev/CLAUDE.md`** — update the "Driver invariant" rule: `--dangerously-skip-permissions`
  is now conditional on the bypass permission mode, not always passed.

## Tests

**Backend** (never hits real `claude`; fake `Query` for `sdkRunner`, `fake-claude.sh` for the spawner):

- `sdkRunner.test.ts`: in `default` mode, a non-`AskUserQuestion` tool parks (synthetic
  `agentic_perm` line written) and resumes on `respondPermission("allow"|"deny")`; plan mode parks on
  `ExitPlanMode` and `setPermissionMode("default")` is called on approve; interrupt and query-death
  deny the stranded `perm`/`plan` (no leak). Bypass mode = unchanged (auto-allow, ask still parks).
- `streamParser.test.ts`: `type:"agentic_perm"` → `kind:"perm"` / `kind:"plan"`.
- `engine.test.ts`: `pendingPerm` exempts the session from idle/wall watchdog; `respondPermission`
  routes to the handle; `permissionMode` persists and reaches `spawnOpts`.
- `store.test.ts`: migration adds the column; `normalizePermissionMode` maps unknown→null.
- `routes.test.ts`: `permissionMode` accepted on `POST /api/sessions`; `POST …/permission` validates
  and dispatches.

**Android:** mirror the existing ask-card tests for the perm/plan cards and `respondPermission`; add a
`NewRequestScreen`/VM test for the Permissions slider default + selection.

## Risk & verification gate

The single material risk mirrors the ultracode spec: **fake-claude / fake-Query cannot prove the real
`claude` Agent SDK honors `permissionMode` and routes `default`/`acceptEdits` tools through
`canUseTool`, nor that `ExitPlanMode` arrives via `canUseTool` and `setPermissionMode` transitions a
live query.** Unit tests only prove our wiring. Mandatory manual smoke test before "done" (headless
verify recipe in `docs/internals.md`):

1. Launch a real session in **Ask** mode on a task that runs a tool → confirm a permission card
   appears and the turn waits; Allow resumes, Deny is reflected to Claude.
2. Launch in **Plan** mode → confirm Claude stays read-only and a plan-approval card appears; Approve
   continues execution with per-tool prompts (i.e. now in `default`).
3. Launch in **Accept edits** → an edit applies without a card; a Bash call surfaces a card.
4. Launch in **Dangerous** → behavior identical to today (no cards; `AskUserQuestion` still works).

If the SDK's `canUseTool`/`setPermissionMode` semantics differ from the above, the spec's mechanism
(synthetic line + parked promise) is unaffected for `default`/`acceptEdits`; only the plan-transition
detail (step 2) may need an alternate SDK call — record the finding and adjust in the plan.

## Out of scope

- Changing `permissionMode` per follow-up turn (session-level only for now).
- "Always allow this tool" / per-tool allow-lists (only once-off allow/deny + plan approve/keep).
- Any change to orchestration `mode` (ultracode), skill isolation, cgroup caps, or templates beyond
  the new field.
- A web client (none exists; Android is the only client).

## Open questions for review

- **Q1** Slider notch order — spec assumes `Plan · Ask · Accept edits · Dangerous` (ascending
  autonomy), default thumb on **Dangerous**. OK, or surface the default differently?
- **Q2** Dedicated `POST …/permission` endpoint (spec's choice) vs reusing `/messages` with a sentinel
  answer. Spec assumes the dedicated endpoint for explicit allow/deny semantics.
- **Q3** After plan approval the spec continues in `default` (Ask) per your choice — confirm you want
  per-tool prompting to continue rather than running freely.
