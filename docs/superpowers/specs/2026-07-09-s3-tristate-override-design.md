# S3 — Tri-state per-session override

- **Date:** 2026-07-09
- **Status:** Spec (autonomous build — architecture pre-approved in the S4 program spec)
- **Builds on:** S4 (sessions inherit the global baseline), S2 (MCP fields).

## Problem
Today a session can only **force a component OFF** (`hiddenSkills` / `hiddenPlugins` / `hiddenMcpServers`
blacklists) or **inherit** the global state. It cannot **force a component ON** for just this session
when the global baseline disables it. S4 deliberately removed the old "force-enable everything"
behavior and deferred per-session force-on to here.

## Goal
Make the per-session override **tri-state** per component: **inherit** (default) / **force-off** /
**force-on**, layered over the global baseline. Additive and backward-compatible — the existing
`hidden*` lists keep meaning "force-off".

## Model (additive)
Add a per-session **force-on** list beside each existing force-off (hidden) list:
- `forcedOnPlugins: Vec<String>` (plugin ids) beside `hiddenPlugins`
- `forcedOnSkills: Vec<String>` (skill names) beside `hiddenSkills`
- `forcedOnMcpServers: Vec<String>` (server names) beside `hiddenMcpServers`

Per-component effective state for a session:
```
if id in forcedOn      -> ENABLED   (overrides a global-off)
else if id in hidden   -> DISABLED
else                   -> inherit global baseline
```
Validation: a component id must not appear in BOTH lists (400 if it does).

## Resolve (extends S4's global-aware resolve)
- **Plugins** (`plugins::resolve_enabled_plugins` / the mod.rs seam): the per-session enable map value
  for a plugin becomes: `forcedOn ? true : (hidden ? false : plugin_globally_enabled(global))`. Emit
  explicit `true` for forced-on so it wins over a global `false` in the command-line settings layer
  (this is the force-on that S4 removed, now scoped to an explicit per-session opt-in).
- **Skills** (`global_settings::resolve_session_hidden_skills` + a new resolve for forced-on): the set
  of skills turned OFF stays "global-off ∪ session-hidden **minus** session-forced-on". For forced-on
  skills that the global disables, emit `skillOverrides[name]="on"` (mirrors the S4 base-override
  fix) via a new resolved forced-on skill list → bridge.
- **MCP:** forced-on has no effect for now (MCP has no global-off yet — S2 stub); accept and store the
  field for API/UI symmetry but it is a no-op at spawn until global MCP disable exists. Documented.

## Threading (mirror the hidden* fields added in S2)
- `store.rs`: `Session` + `CreateInput` gain the three `forced_on_*` `Vec<String>` fields (serde
  camelCase, `#[serde(default)]`), three new defaulted DB columns, INSERT + read mapping.
- `api/sessions.rs`: `CreateBody` gains the three optional lists; validate disjoint-from-hidden (400).
- `engine/runner.rs` `RunSpec` + `engine/spawner.rs` `SpawnOptions`: gain the forced-on fields.
- `engine/mod.rs`: build site + `fork_session` copy them; the plugin/skill resolve consumes them.
- `engine/sdk_runner.rs`: forced-on skills that are globally-off → include in a new env
  `SDK_BRIDGE_FORCED_ON_SKILLS`; the plugin path already carries explicit true/false via the enable map
  (no new env needed — the resolve just emits `true` for forced-on plugins).
- `sdk-bridge.mjs`: `SDK_BRIDGE_FORCED_ON_SKILLS` → `settings.skillOverrides[name]="on"` (merged with
  the existing "off" entries; forced-on wins).

## Testing (Rust suite)
- Resolve: forced-on plugin over a global-off → map emits `true`; forced-on skill over global-off →
  appears in forced-on set (→ "on"), removed from the off set; id in both lists → the API rejects it.
- Precedence: forcedOn beats hidden beats global (unit test the resolve directly).
- store round-trip of the three new fields (+ old rows default empty).
- sdk_runner env: `SDK_BRIDGE_FORCED_ON_SKILLS` set when non-empty; plugin enable map has `true` for
  forced-on.
- api: CreateBody validation — same id in hidden+forcedOn → 400.

## Constraints
- Engine axum-free; no new crate deps; additive DB columns; `make test` green; `cargo build` compiles.

## Acceptance
- A session with `forcedOnPlugins:[X]` where X is globally-disabled resolves X to enabled for that
  session (map `true`); a session with `forcedOnSkills:[Y]` where Y is globally-off produces
  `skillOverrides[Y]="on"`. Both leave global config untouched. Suite green.
