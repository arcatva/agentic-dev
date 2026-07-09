# S3 — Tri-state Per-session Override: Implementation Report

**Date:** 2026-07-10
**Branch:** agentic/7a5b3fa6-51d9-4deb-a442-c30944eee3ab
**Suite result:** 544 passed; 0 failed (cargo test, full parallel run)

---

## Files Changed

| File | Change |
|------|--------|
| `server-rs/src/engine/store.rs` | Added `forced_on_plugins`, `forced_on_skills`, `forced_on_mcp_servers` to `Session` and `CreateInput`; added 3 entries to `ADDED_COLUMNS`; extended INSERT SQL (35→38 columns/placeholders); extended binding chain; extended `row_to_session`; added `forced_on_fields_round_trip_and_old_rows_default_empty` test |
| `server-rs/src/engine/mod.rs` | Added 3 fields to `SubmitMeta`; extended `submit_session` CreateInput build; extended `fork_session` CreateInput build; updated `spawn_opts` to call `resolve_session_hidden_skills` with `forced_on`, call new `resolve_session_forced_on_skills`, call `resolve_enabled_plugins` with `forced_on_plugins`, thread `forced_on_plugins`/`forced_on_mcp_servers` verbatim |
| `server-rs/src/engine/runner.rs` | Added 3 fields to `RunSpec` |
| `server-rs/src/engine/spawner.rs` | Added 3 fields to `SpawnOptions`; extended `build_spec` |
| `server-rs/src/engine/plugins.rs` | Added `forced_on: &[String]` param to `resolve_enabled_plugins`; updated resolve logic (forcedOn > hidden > global); added uninstalled forced-on entries; updated all 6 test call sites to pass `&[]`; added 3 new tests |
| `server-rs/src/engine/global_settings.rs` | Added `forced_on: &[String]` param to `resolve_session_hidden_skills` (removes forced-on from off-set); added `resolve_session_forced_on_skills` fn; updated existing test call site; added 2 new tests |
| `server-rs/src/engine/sdk_runner.rs` | Added `SDK_BRIDGE_FORCED_ON_SKILLS` env injection block after hidden skills; added `start_passes_forced_on_skills_to_bridge_env` test |
| `server-rs/src/engine/tests.rs` | Updated 6 explicit `SubmitMeta` literals with `forced_on_*: vec![]`; added `spawn_opts_threads_forced_on_fields` integration test |
| `server-rs/src/api/sessions.rs` | Added 3 optional fields to `CreateBody` (serde camelCase); added `validate_disjoint` helper fn; added disjoint validation calls in `create_session` (→ 400); extended `SubmitMeta` construction; added 4 unit tests |
| `server-rs/src/api/misc.rs` | Extended `SubmitMeta` literal in template submit path with `forced_on_*: Vec::new()` |
| `server-rs/sdk-bridge.mjs` | Added `forced_on_skills` to boot log; replaced `hiddenSkills.length` branch with merged `hiddenSkills || forcedOnSkills` block; reads `SDK_BRIDGE_FORCED_ON_SKILLS`; applies "on" entries after "off" entries so forced-on wins |
| `docs/superpowers/plans/2026-07-10-s3-tristate-override.md` | Implementation plan |
| `.superpowers/sdd/s3-report.md` | This file |

---

## Resolve-Precedence Logic

### Plugins (`plugins.rs::resolve_enabled_plugins`)

For each installed plugin by name:
```
if forced.contains(name) → true
else if plugin_globally_enabled(&toggles, name) && !hidden.contains(name) → true
else → false
```
Uninstalled but forced-on ids are also emitted as `true` (explicit user intent).
Uninstalled hidden ids are still emitted as `false` (existing behavior preserved).

### Skills (`global_settings.rs`)

**Off set** = `(global_skill_overrides == "off") ∪ session_hidden_skills − session_forced_on_skills`

Implemented in `resolve_session_hidden_skills(config_base, hidden, forced_on)`:
- builds the off BTreeSet from global toggles + session hidden list
- removes every name in `forced_on` from the set

**Forced-on skills that need explicit "on"** = skills in `forced_on` where `!skill_globally_enabled`:

Implemented in `resolve_session_forced_on_skills(config_base, forced_on)`:
- returns the subset of `forced_on` where the global baseline has `skillOverrides == "off"`
- these are sent to the bridge as `SDK_BRIDGE_FORCED_ON_SKILLS` → `skillOverrides[name]="on"`

### MCP

`forced_on_mcp_servers` is stored, threaded through all layers, and appears on `SpawnOptions`. It has no effect at spawn (no global MCP disable exists yet). A comment in `mod.rs::spawn_opts` documents this.

---

## Test Evidence

### Commands run

```
cd server-rs && cargo test
```

### Results

```
test result: ok. 544 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.05s
```

### New tests added (13 total)

| Test | File | Covers |
|------|------|--------|
| `forced_on_fields_round_trip_and_old_rows_default_empty` | `store.rs` | DB INSERT + read + old rows default |
| `forced_on_beats_global_off_plugin` | `plugins.rs` | forcedOn > global-off |
| `forced_on_beats_hidden_plugin` | `plugins.rs` | forcedOn > hidden |
| `forced_on_uninstalled_plugin_emits_true` | `plugins.rs` | uninstalled forced-on |
| `forced_on_removed_from_hidden_skills_off_set` | `global_settings.rs` | forcedOn removes from off-set |
| `resolve_session_forced_on_skills_returns_globally_off_ones` | `global_settings.rs` | only globally-off skills need explicit "on" |
| `start_passes_forced_on_skills_to_bridge_env` | `sdk_runner.rs` | SDK_BRIDGE_FORCED_ON_SKILLS env |
| `spawn_opts_threads_forced_on_fields` | `tests.rs` | full chain submit→store→spawn_opts |
| `validate_disjoint_catches_conflict` | `sessions.rs` | 400 on same id in hidden+forcedOn |
| `validate_disjoint_allows_non_overlapping_lists` | `sessions.rs` | valid non-overlapping |
| `validate_disjoint_empty_lists_ok` | `sessions.rs` | empty lists always valid |
| `create_session_rejects_same_id_in_hidden_and_forced_on` | `sessions.rs` | HTTP 400 for plugin + skill conflicts |

### Build

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.33s
```

---

## Concerns / Deviations

None material. Minor notes:

1. **`forced_on_mcp_servers` is a no-op at spawn.** Documented in both `runner.rs` (field comment) and `mod.rs` (spawn_opts build site comment). The field is fully stored, serialized, and threaded to SpawnOptions — it simply isn't consumed by the bridge because MCP has no global-off concept yet.

2. **`sdk-bridge.mjs` changes are JS-only, untested by the Rust suite.** The bridge receives `SDK_BRIDGE_FORCED_ON_SKILLS` and applies `skillOverrides[name]="on"` for each, after "off" entries. This is the same pattern as the existing hidden-skills handling and was verified by manual inspection. No Node.js test harness exists in this repo.

3. **`forced_on_skills` in `SpawnOptions` carries only the globally-off subset.** The raw session value (all forced-on skills the user specified) lives in `Session::forced_on_skills`. The `SpawnOptions::forced_on_skills` field carries only the subset returned by `resolve_session_forced_on_skills` — the ones that actually need an explicit "on" bridge override (globally-off skills). Skills that are globally-on need no bridge intervention. The `spawn_opts_threads_forced_on_fields` integration test asserts this correctly.

4. **One pre-existing dead_code warning (`compose_turn_text`)** appears on build; it predates S3 and is unrelated to this change.
