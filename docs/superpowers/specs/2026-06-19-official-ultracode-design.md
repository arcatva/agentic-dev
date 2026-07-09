# Switch to official ultracode; drop the custom orchestration tiers — design

**Date:** 2026-06-19
**Status:** Draft, pending user review
**Supersedes the orchestration-mode parts of:** `2026-06-17-orchestration-modes-multiagent-view-design.md`
(the multi-agent SessionView from that spec is unaffected and stays as-is).

## Goal

Replace agentic-dev's home-grown three-way "Orchestration mode" (Normal / Workflows / Ultra-code,
implemented by injecting a custom text preamble) with Claude Code's **official ultracode** feature,
collapsing the control to a single on/off switch.

## Background

Today (`server/engine/spawner.ts`) a `MODE_PREAMBLE` table maps `mode` ∈ {`workflows`, `ultra`}
to a free-text instruction that `composeUserText` prepends to every user turn. This is a
prompt-injection approximation — agentic-dev has no real harness flag for it. Meanwhile Claude Code
ships an official ultracode feature (docs: `code.claude.com/docs/en/workflows`,
`code.claude.com/docs/en/model-config`): a session setting that combines `xhigh` reasoning effort
with automatic dynamic-workflow orchestration, enableable non-interactively by passing
`"ultracode": true` via `--settings`.

The user wants the official mechanism and the two custom tiers gone.

## Decisions (assumptions — confirm at spec review)

1. **Mechanism = official session setting (Approach A).** When the switch is on, the spawner passes
   `--settings '{"ultracode":true}'` to `claude -p`. This is the documented non-interactive path and
   is the truest match for the removed standing "ultra" tier (xhigh + auto-orchestration, persists
   across follow-up turns). *(Alternative considered: inject the official keyword `ultracode` into
   each turn's prompt — lower risk but does not force xhigh and is only a per-turn approximation.
   Rejected as "not really the official setting".)*
2. **Effort is owned by ultracode when on.** When the switch is on, the Android effort slider is
   disabled (reusing the existing `ultra → xhigh` lock UX) and the backend does **not** emit
   `--effort` — ultracode sets `xhigh` itself, and passing both risks conflict.
3. **The `OUTBOX_NOTE` preamble stays.** It is unrelated to orchestration (it is how a no-terminal
   mobile session hands files to the user) and must remain prepended to every turn.
4. **Minimal data-model churn: keep the `mode` column.** Its value space collapses to
   `null` (off) | `"ultracode"` (on). No column rename, no new migration. The strings `workflows`
   and `ultra` are removed from all producers/consumers.

## Architecture / changes by layer

### Backend (`~/src/agentic-dev`)

- **`server/engine/spawner.ts`**
  - Delete `MODE_PREAMBLE`.
  - `preambleFor(mode)` → returns just `OUTBOX_NOTE` (drop the mode branch). `composeUserText`
    signature can keep `mode` for now or drop it; simplest is to drop the `mode` param since it no
    longer affects the text. (Decision: drop the param — `composeUserText(text)` — and update all
    three call sites: one in spawner `buildSpec` (`:130`) and two in `engine.ts` (`:202`, `:496`).)
  - `buildSpec`: when `opts.mode === "ultracode"`, push `--settings`, `{"ultracode":true}` into
    `extra`. When on, **skip** the `--effort` push even if `opts.effort` is set.
  - Keep the `mode?` field on `SpawnOptions`; update the header comment.
- **`server/engine/types.ts`** — update the `Session.mode` comment to `null | "ultracode"`.
- **`server/engine/store.ts`** — no schema change. (Existing rows with `"workflows"`/`"ultra"` become
  inert: treated as "not ultracode". A one-line normalization in `rowToSession` MAY map legacy
  `"ultra"` → `"ultracode"` and `"workflows"` → `null`; **decision: do the normalization** so old
  sessions display consistently.)
- **`server/engine/engine.ts`** — `composeUserText` call sites (`:202`, `:496`) drop the `s.mode`
  arg. `spawnOpts` still forwards `mode: s.mode`.
- **`server/api/routes.ts`** — unchanged in shape; `mode` still read from the body / template. (No
  validation added beyond what exists; an unknown value is simply non-"ultracode" = off.)

### Android (`~/src/agentic-dev-android`)

- **`ui/newrequest/NewRequestScreen.kt`**
  - Delete the `MODES` list and the three-`ToggleButton` row.
  - Add a single labeled `Switch` "Ultracode" → `realVm.setMode(if (on) "ultracode" else null)`.
  - The `ultra` effort-lock logic generalizes: `val ultra = s.mode == "ultracode"` continues to lock
    the effort slider (`enabled = !ultra`, label "Effort (locked by Ultracode)") and the
    `LaunchedEffect(ultra){ if (ultra) setEffort("xhigh") }` stays (UI still shows xhigh for honesty,
    even though the backend now lets ultracode set it).
- **`ui/newrequest/NewRequestViewModel.kt`** — no signature change (`setMode(String?)` already
  binary-friendly). `applyTemplate` still copies `t.mode`.
- **`data/net/Models.kt`** — `mode` fields unchanged in type (`String?`); only the value space
  narrows. Templates carrying `"ultra"`/`"workflows"` are tolerated (treated as on/off respectively
  by the same normalization, applied client-side if needed — **decision: keep client tolerant, no
  hard validation**).
- **Mode chip on SessionView** (from the 2026-06-17 spec) — relabel to show "Ultracode" when set.

### Tests

- **Backend** (`server/engine/*.test.ts`, fake-claude): replace assertions that checked per-mode
  preamble text with:
  - off → argv has **no** `--settings` and no ultracode; `OUTBOX_NOTE` still present in the turn text.
  - on → argv contains `--settings` `{"ultracode":true}`; argv has **no** `--effort`; `OUTBOX_NOTE`
    still present.
  - legacy `"ultra"`/`"workflows"` normalization (if implemented in `rowToSession`).
- **Android** — update `NewRequestScreen`/VM tests that referenced the three modes to the switch.

## Risk & verification gate (important)

The single material risk: **fake-claude cannot prove the real `claude` binary honors
`--settings '{"ultracode":true}'` in headless `-p` mode.** Unit tests only prove we *emit* the flag.
Mitigation — a mandatory manual smoke test before this is considered done, using the headless verify
recipe in `docs/internals.md`:

1. Launch a real session with the switch ON; confirm via the stream that effort is `xhigh` and that
   Claude opts into a workflow on a substantive task (a `Workflow` tool_use appears) without the user
   typing the keyword.
2. Launch with the switch OFF; confirm no auto-workflow and default effort.

If the flag turns out to be rejected/ignored by the deployed `claude` version, **fall back to
Approach B** (inject the official `ultracode` keyword into each turn via `composeUserText`, keep the
effort slider user-controlled). This fallback is recorded here so the implementation plan can branch
on the smoke-test result.

## Out of scope

- The multi-agent SessionView / workflow status rendering (unchanged).
- Any change to skill isolation (`CLAUDE_CONFIG_DIR`), cgroup caps, streaming, or templates beyond
  the `mode` value space.
- A DB migration to drop/rename the `mode` column (deliberately avoided).

## Open questions for review

- **Q1** Approach A (official `--settings`) vs B (official keyword)? Spec assumes **A**, with B as the
  documented fallback if the smoke test fails.
- **Q2** When ultracode is on, fully cede effort to ultracode and disable the slider? Spec assumes
  **yes**.
- **Q3** Normalize legacy `"ultra"`/`"workflows"` rows (`ultra`→on, `workflows`→off) or just treat any
  non-`"ultracode"` value as off? Spec assumes **normalize**.
