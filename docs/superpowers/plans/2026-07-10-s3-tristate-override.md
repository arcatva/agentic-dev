# S3 — Tri-state Per-session Override Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `forcedOnPlugins`, `forcedOnSkills`, and `forcedOnMcpServers` fields to sessions so each component can be forced ON, forced OFF (existing hidden* lists), or inherited from global baseline — all in a single session creation call.

**Architecture:** Mirror S2's `hidden_mcp_servers` threading exactly: the three new fields thread through `store.rs` → `engine/mod.rs` (SubmitMeta + fork) → `runner.rs` (RunSpec) → `spawner.rs` (SpawnOptions) → `sdk_runner.rs` (env) → `sdk-bridge.mjs` (skillOverrides). Resolve logic extends `plugins::resolve_enabled_plugins` (forced-on wins over hidden wins over global) and `global_settings::resolve_session_hidden_skills` (remove forced-on names from the off set). MCP forced-on is stored but a no-op at spawn with a comment.

**Tech Stack:** Rust (async/tokio, sqlx, serde), Node.js (sdk-bridge.mjs), SQLite (additive ALTER TABLE migration)

## Global Constraints

- Engine (`server-rs/src/engine/`) must stay free of axum imports (no `use axum` in engine files)
- No new crate dependencies
- Additive DB columns only (ALTER TABLE, no schema drops or rewrites)
- All three new fields use `#[serde(default)]` and `Vec<String>` type (never `Option<Vec<String>>` in the store/engine types)
- serde camelCase renames: `forcedOnPlugins`, `forcedOnSkills`, `forcedOnMcpServers`
- `make test` must stay green before commit (`cd server-rs && cargo test`)
- `cargo build` must compile before commit
- Commit only locally — do NOT push or open a PR (the user's CLAUDE.md auto-merge flow does that)
- Write report to `.superpowers/sdd/s3-report.md` after all tasks pass

---

## Files Changed / Created

| File | What changes |
|------|-------------|
| `server-rs/src/engine/store.rs` | Add 3 fields to `Session`, `CreateInput`; add 3 entries to `ADDED_COLUMNS`; extend INSERT column list + placeholders + bindings; extend `row_to_session` mapping |
| `server-rs/src/engine/mod.rs` | Add 3 fields to `SubmitMeta`; extend `submit_session` build site + `fork_session` copy; update `spawn_opts` to pass `forced_on_plugins` to resolve + pass `forced_on_skills` to both resolve fns |
| `server-rs/src/api/sessions.rs` | Add 3 optional fields to `CreateBody`; add disjoint validation (400 if same id in hidden + forced-on for same kind); map to `SubmitMeta` |
| `server-rs/src/engine/runner.rs` | Add 3 fields to `RunSpec` |
| `server-rs/src/engine/spawner.rs` | Add 3 fields to `SpawnOptions`; extend `build_spec` |
| `server-rs/src/engine/plugins.rs` | Add `forced_on: &[String]` param to `resolve_enabled_plugins`; update resolve logic; update all test call sites |
| `server-rs/src/engine/global_settings.rs` | Extend `resolve_session_hidden_skills` with `forced_on: &[String]` param (removes forced-on from off-set); add new `resolve_session_forced_on_skills(config_base, forced_on) -> Vec<String>` |
| `server-rs/src/engine/sdk_runner.rs` | Set `SDK_BRIDGE_FORCED_ON_SKILLS` env when non-empty |
| `server-rs/sdk-bridge.mjs` | Parse `SDK_BRIDGE_FORCED_ON_SKILLS`; set `skillOverrides[name]="on"` for each (after the "off" entries so forced-on wins) |
| `server-rs/src/engine/tests.rs` | Extend `spawn_opts_threads_mcp_fields` (or a new similar test) to assert forced-on fields thread through |
| `.superpowers/sdd/s3-report.md` | Final report (written in Task 7) |

---

### Task 1: Thread the three new fields through store.rs

**Files:**
- Modify: `server-rs/src/engine/store.rs`

**Interfaces:**
- Produces: `Session::forced_on_plugins: Vec<String>` (serde `forcedOnPlugins`, default), `Session::forced_on_skills: Vec<String>` (serde `forcedOnSkills`, default), `Session::forced_on_mcp_servers: Vec<String>` (serde `forcedOnMcpServers`, default); same three on `CreateInput`; DB columns `forcedOnPlugins TEXT`, `forcedOnSkills TEXT`, `forcedOnMcpServers TEXT` (defaulted, additive migration)

- [ ] **Step 1: Add three fields to the `Session` struct** (after `hidden_mcp_servers` around line 136)

Open `server-rs/src/engine/store.rs`. After the `hidden_mcp_servers` field (line 136), add:

```rust
    /// Plugin ids forced ON for this session (overrides a global-off).
    #[serde(rename = "forcedOnPlugins", default)] pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session (overrides a global-off).
    #[serde(rename = "forcedOnSkills", default)] pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON for this session (stored for symmetry; no-op at spawn
    /// until global MCP disable is implemented).
    #[serde(rename = "forcedOnMcpServers", default)] pub forced_on_mcp_servers: Vec<String>,
```

- [ ] **Step 2: Add three fields to `CreateInput`** (after `hidden_mcp_servers: Vec<String>` around line 203)

```rust
    pub forced_on_plugins: Vec<String>,
    pub forced_on_skills: Vec<String>,
    pub forced_on_mcp_servers: Vec<String>,
```

- [ ] **Step 3: Add three entries to `ADDED_COLUMNS`** (after `("extraMcpServers", "TEXT")` around line 489)

```rust
    // NULL = no forced-on overrides (rows written before the column existed keep the default).
    ("forcedOnPlugins", "TEXT"),
    ("forcedOnSkills", "TEXT"),
    ("forcedOnMcpServers", "TEXT"),
```

- [ ] **Step 4: Extend the Session creation block** (around line 661 where `hidden_mcp_servers` is set)

After `hidden_mcp_servers: input.hidden_mcp_servers.clone(),` and `extra_mcp_servers: input.extra_mcp_servers.clone(),` add:

```rust
            forced_on_plugins: input.forced_on_plugins.clone(),
            forced_on_skills: input.forced_on_skills.clone(),
            forced_on_mcp_servers: input.forced_on_mcp_servers.clone(),
```

- [ ] **Step 5: Extend the INSERT SQL statement and bindings** (around line 666-676)

The current INSERT is:
```
INSERT INTO sessions (id,repo,repos,skills,hiddenSkills,hiddenPlugins,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,permissionMode,titlePinned,parentSessionId,seq,groupId,unreadEventId,ackedEventId,origin,hiddenMcpServers,extraMcpServers) \
VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
```

Replace with (adds 3 columns, 3 placeholders):
```rust
sqlx::query("INSERT INTO sessions (id,repo,repos,skills,hiddenSkills,hiddenPlugins,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,permissionMode,titlePinned,parentSessionId,seq,groupId,unreadEventId,ackedEventId,origin,hiddenMcpServers,extraMcpServers,forcedOnPlugins,forcedOnSkills,forcedOnMcpServers) \
    VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
```

Then at the end of the binding chain (after `.bind(serde_json::to_string(&s.extra_mcp_servers)?)`) add:

```rust
            .bind(serde_json::to_string(&s.forced_on_plugins)?)
            .bind(serde_json::to_string(&s.forced_on_skills)?)
            .bind(serde_json::to_string(&s.forced_on_mcp_servers)?)
```

- [ ] **Step 6: Extend `row_to_session`** (after `hidden_mcp_servers:` around line 1137)

After `extra_mcp_servers:` mapping add:

```rust
        forced_on_plugins: safe_json_vec(r.try_get("forcedOnPlugins").ok().flatten(), vec![]),
        forced_on_skills: safe_json_vec(r.try_get("forcedOnSkills").ok().flatten(), vec![]),
        forced_on_mcp_servers: safe_json_vec(r.try_get("forcedOnMcpServers").ok().flatten(), vec![]),
```

- [ ] **Step 7: Write a failing store round-trip test** (in the `#[cfg(test)] mod tests` block at the bottom of `store.rs`)

Add this test after the existing `hidden_mcp_servers` round-trip test (around line 1776):

```rust
    #[tokio::test]
    async fn forced_on_fields_round_trip_and_old_rows_default_empty() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();

        // Create a session with all three forced-on fields populated.
        let plugins = vec!["superpowers@official".to_string()];
        let skills  = vec!["rke2-ops".to_string()];
        let mcp     = vec!["my-server".to_string()];
        let created = store.create(CreateInput {
            id: "s-forced".into(),
            repos: vec!["r".into()],
            prompt: "x".into(),
            forced_on_plugins:     plugins.clone(),
            forced_on_skills:      skills.clone(),
            forced_on_mcp_servers: mcp.clone(),
            ..Default::default()
        }).await.unwrap();

        assert_eq!(created.forced_on_plugins,     plugins);
        assert_eq!(created.forced_on_skills,      skills);
        assert_eq!(created.forced_on_mcp_servers, mcp);

        let got = store.get("s-forced").await.unwrap().unwrap();
        assert_eq!(got.forced_on_plugins,     plugins);
        assert_eq!(got.forced_on_skills,      skills);
        assert_eq!(got.forced_on_mcp_servers, mcp);

        // A session created WITHOUT forced-on fields must default to empty (legacy row compat).
        let plain = store.create(CreateInput {
            id: "s-plain".into(),
            repos: vec!["r".into()],
            prompt: "y".into(),
            ..Default::default()
        }).await.unwrap();
        assert!(plain.forced_on_plugins.is_empty());
        assert!(plain.forced_on_skills.is_empty());
        assert!(plain.forced_on_mcp_servers.is_empty());
    }
```

- [ ] **Step 8: Run the test to verify it fails (compilation needed first)**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test forced_on_fields_round_trip 2>&1 | tail -20
```

Expected: compile error about missing fields in other structs (SubmitMeta, RunSpec, SpawnOptions — we haven't added them yet). This confirms the test is wired correctly and the fields are needed.

- [ ] **Step 9: Verify the store.rs changes compile in isolation (they should — Session and CreateInput are standalone)**

Temporarily: `cargo check 2>&1 | grep "error\[" | head -20` — expect errors only about unresolved fields in mod.rs/runner.rs/spawner.rs (those we fix in later tasks).

---

### Task 2: Thread forced-on fields through SubmitMeta, RunSpec, SpawnOptions, build_spec

**Files:**
- Modify: `server-rs/src/engine/mod.rs`
- Modify: `server-rs/src/engine/runner.rs`
- Modify: `server-rs/src/engine/spawner.rs`

**Interfaces:**
- Consumes: `Session::forced_on_plugins/skills/mcp_servers` (Task 1)
- Produces: `SubmitMeta::forced_on_plugins/skills/mcp_servers: Vec<String>`; `RunSpec::forced_on_plugins/skills/mcp_servers: Vec<String>`; `SpawnOptions::forced_on_plugins/skills/mcp_servers: Vec<String>`; `build_spec` maps SpawnOptions → RunSpec for these fields

- [ ] **Step 1: Add three fields to `SubmitMeta` in `mod.rs`** (around line 268 after `hidden_mcp_servers`)

```rust
    /// Plugin ids to force ON for this session (overrides a global-off). Disjoint from hidden_plugins.
    pub forced_on_plugins: Vec<String>,
    /// Skill names to force ON for this session (overrides a global-off). Disjoint from hidden_skills.
    pub forced_on_skills: Vec<String>,
    /// MCP server names to force ON (stored; no-op at spawn until global MCP disable exists).
    pub forced_on_mcp_servers: Vec<String>,
```

- [ ] **Step 2: Extend `submit_session` CreateInput build site** (around line 1119 in `mod.rs`)

After `hidden_mcp_servers: meta.hidden_mcp_servers,` add:

```rust
            forced_on_plugins: meta.forced_on_plugins,
            forced_on_skills: meta.forced_on_skills,
            forced_on_mcp_servers: meta.forced_on_mcp_servers,
```

- [ ] **Step 3: Extend `fork_session` CreateInput build site** (around line 1676 in `mod.rs`)

After `hidden_mcp_servers: src.hidden_mcp_servers.clone(),` add:

```rust
            forced_on_plugins:     src.forced_on_plugins.clone(),
            forced_on_skills:      src.forced_on_skills.clone(),
            forced_on_mcp_servers: src.forced_on_mcp_servers.clone(),
```

- [ ] **Step 4: Add three fields to `RunSpec` in `runner.rs`** (after `hidden_mcp_servers` around line 29)

```rust
    /// Plugin ids forced ON for this session.
    pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session.
    pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON (stored; no-op at spawn).
    pub forced_on_mcp_servers: Vec<String>,
```

- [ ] **Step 5: Add three fields to `SpawnOptions` in `spawner.rs`** (after `hidden_mcp_servers` around line 26)

```rust
    /// Plugin ids forced ON for this session. Resolved by resolve_enabled_plugins.
    pub forced_on_plugins: Vec<String>,
    /// Skill names forced ON for this session.
    pub forced_on_skills: Vec<String>,
    /// MCP server names forced ON (stored; no-op at spawn until global MCP disable exists).
    pub forced_on_mcp_servers: Vec<String>,
```

- [ ] **Step 6: Extend `build_spec` in `spawner.rs`** (around line 84-88)

After `hidden_mcp_servers: opts.hidden_mcp_servers.clone(),` add:

```rust
        forced_on_plugins: opts.forced_on_plugins.clone(),
        forced_on_skills: opts.forced_on_skills.clone(),
        forced_on_mcp_servers: opts.forced_on_mcp_servers.clone(),
```

- [ ] **Step 7: Run `cargo check` to verify only engine/mod.rs spawning seam is left**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo check 2>&1 | grep "error\[" | head -20
```

Expected: errors about `spawn_opts` not passing forced-on to resolvers (next task) and API sessions.rs missing fields.

---

### Task 3: Update plugin resolve and global_settings skill resolve

**Files:**
- Modify: `server-rs/src/engine/plugins.rs`
- Modify: `server-rs/src/engine/global_settings.rs`

**Interfaces:**
- Consumes: `forced_on_plugins: &[String]` (new param), `forced_on_skills: &[String]` (new param)
- Produces: `resolve_enabled_plugins(config_dir, hidden, forced_on) -> BTreeMap<String, bool>` — forced-on entries emit `true` regardless of global state; `resolve_session_hidden_skills(config_base, hidden, forced_on) -> Vec<String>` — removes forced-on names from off-set; `resolve_session_forced_on_skills(config_base, forced_on) -> Vec<String>` — forced-on skills that are globally-off (need explicit "on" in bridge)

- [ ] **Step 1: Write failing tests for the new plugin resolve behavior** (in `plugins.rs` test block)

Add these two tests at the end of the `mod tests` block:

```rust
    #[test]
    fn forced_on_beats_global_off_plugin() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "gh@m":[{"scope":"user"}],
                "sp@m":[{"scope":"user"}]
            }}"#,
        ).unwrap();
        // Global disables gh@m.
        std::fs::write(dir.join("settings.local.json"), r#"{"enabledPlugins":{"gh@m":false}}"#).unwrap();

        // forced_on=[gh@m]: must emit true even though globally disabled.
        let map = resolve_enabled_plugins(&dir, &[], &["gh@m".to_string()]);
        assert_eq!(map.get("gh@m"), Some(&true), "forced-on must override global-off");
        assert_eq!(map.get("sp@m"), Some(&true)); // not forced, not hidden → inherit global (on)
    }

    #[test]
    fn forced_on_beats_hidden_plugin() {
        let dir = tmp();
        std::fs::write(
            dir.join("plugins").join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"gh@m":[{"scope":"user"}]}}"#,
        ).unwrap();
        // forced_on AND hidden for the same plugin: forced_on wins.
        let map = resolve_enabled_plugins(&dir, &["gh@m".to_string()], &["gh@m".to_string()]);
        assert_eq!(map.get("gh@m"), Some(&true), "forced-on must beat hidden (hidden has lower precedence)");
    }
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test forced_on_beats 2>&1 | tail -20
```

Expected: compile error (wrong arity — `resolve_enabled_plugins` still takes 2 params).

- [ ] **Step 3: Update `resolve_enabled_plugins` signature and logic** in `plugins.rs`

Change the function signature from:
```rust
pub fn resolve_enabled_plugins(
    claude_config_dir: &Path,
    hidden_plugins: &[String],
) -> std::collections::BTreeMap<String, bool> {
```
to:
```rust
pub fn resolve_enabled_plugins(
    claude_config_dir: &Path,
    hidden_plugins: &[String],
    forced_on: &[String],
) -> std::collections::BTreeMap<String, bool> {
```

Then change the hidden set build (add forced set):
```rust
    let hidden: std::collections::BTreeSet<&str> = hidden_plugins
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let forced: std::collections::BTreeSet<&str> = forced_on
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
```

Change the per-plugin enable computation (replace the `let enabled = ...` line):
```rust
        // Precedence: forced-on > hidden > global.
        let enabled = if forced.contains(name) {
            true
        } else {
            crate::engine::global_settings::plugin_globally_enabled(&toggles, name)
                && !hidden.contains(name)
        };
```

Add forced-on entries for uninstalled plugins at the end (after the `for h in hidden` loop):
```rust
    for f in &forced {
        // A forced-on id not in the registry is still emitted as true (user's intent is explicit).
        map.entry(f.to_string()).or_insert(true);
    }
```

- [ ] **Step 4: Update all existing call sites of `resolve_enabled_plugins` in tests** (within `plugins.rs` test block)

Every call to `resolve_enabled_plugins(&dir, ...)` must gain a third argument `&[]` (empty forced-on preserves current behavior). The calls are at approximately lines 122, 140, 161, 174, 182, 186, 213. Change each:

```rust
// Before:
resolve_enabled_plugins(&dir, &["github@official".into(), " ".into()])
// After:
resolve_enabled_plugins(&dir, &["github@official".into(), " ".into()], &[])
```

Apply the `&[]` addition to all five test functions: `resolve_marks_installed_enabled_and_hidden_disabled`, `resolve_empty_blacklist_enables_every_installed_plugin`, `resolve_trims_padded_registry_names_so_hidden_still_wins`, `resolve_keeps_uninstalled_hidden_ids_disabled`, `resolve_degrades_to_blacklist_without_registry`, `session_inherits_global_disable_when_not_hidden`.

- [ ] **Step 5: Write failing tests for skill resolve** (in `global_settings.rs` test block)

Add these two tests at the end of the `mod tests` block:

```rust
    #[test]
    fn forced_on_removed_from_hidden_skills_off_set() {
        let dir = tmp();
        std::fs::write(dir.join("settings.local.json"),
            r#"{"skillOverrides":{"g-off":"off","forced-skill":"off"}}"#).unwrap();
        // forced_on=[forced-skill]: it must be removed from the off-set even though globally off.
        let mut off = resolve_session_hidden_skills(&dir, &[], &["forced-skill".to_string()]);
        off.sort();
        // g-off stays in the off set; forced-skill is removed because it's forced on.
        assert_eq!(off, vec!["g-off".to_string()]);
        assert!(!off.contains(&"forced-skill".to_string()));
    }

    #[test]
    fn resolve_session_forced_on_skills_returns_globally_off_ones() {
        let dir = tmp();
        // Global disables both; local enables one of them.
        std::fs::write(dir.join("settings.json"),
            r#"{"skillOverrides":{"g-off":"off","also-off":"off"}}"#).unwrap();
        // No local override → both remain globally-off.
        let forced = resolve_session_forced_on_skills(&dir, &["g-off".to_string(), "always-on".to_string()]);
        // g-off is globally disabled → needs explicit "on" in bridge → in returned set.
        assert!(forced.contains(&"g-off".to_string()), "globally-off forced skill must appear in forced-on set");
        // always-on is globally enabled → no bridge "on" needed → NOT in set.
        assert!(!forced.contains(&"always-on".to_string()), "globally-on skill must NOT appear in forced-on set (bridge default covers it)");
    }
```

- [ ] **Step 6: Run tests to confirm they fail**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test forced_on_removed_from_hidden resolve_session_forced_on_skills_returns 2>&1 | tail -20
```

Expected: compile error (wrong arity and missing function).

- [ ] **Step 7: Update `resolve_session_hidden_skills` signature** in `global_settings.rs`

Change from:
```rust
pub fn resolve_session_hidden_skills(config_base: &Path, hidden_skills: &[String]) -> Vec<String> {
```
to:
```rust
pub fn resolve_session_hidden_skills(config_base: &Path, hidden_skills: &[String], forced_on: &[String]) -> Vec<String> {
```

After building `set` (the BTreeSet of off skills), add the forced-on removal:
```rust
    // Forced-on wins over global-off: remove any forced-on name from the off set.
    for f in forced_on {
        set.remove(f.trim());
    }
```

- [ ] **Step 8: Add `resolve_session_forced_on_skills` function** in `global_settings.rs` (after `resolve_session_hidden_skills`)

```rust
/// The subset of `forced_on` skills that the global baseline disables (skillOverrides == "off").
/// These need an explicit `"on"` override in `skillOverrides` at spawn time so they win over
/// the global-off (the base settings layer otherwise keeps them off even when the session
/// forces them on). Skills that are globally-on by default need no special bridge entry.
pub fn resolve_session_forced_on_skills(config_base: &Path, forced_on: &[String]) -> Vec<String> {
    let toggles = read_global_toggles(config_base);
    forced_on
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !skill_globally_enabled(&toggles, s))
        .map(str::to_string)
        .collect()
}
```

- [ ] **Step 9: Update the existing `resolve_session_hidden_skills` call site in `global_settings.rs` tests** (line ~327)

Change:
```rust
let mut out = resolve_session_hidden_skills(&dir, &["sess-hide".into(), " ".into()]);
```
to:
```rust
let mut out = resolve_session_hidden_skills(&dir, &["sess-hide".into(), " ".into()], &[]);
```

- [ ] **Step 10: Run the skill tests to verify they pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test --lib global_settings 2>&1 | tail -20
```

Expected: all `global_settings::tests::*` pass.

- [ ] **Step 11: Run the plugin tests to verify they pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test --lib plugins 2>&1 | tail -20
```

Expected: all `plugins::tests::*` pass including the two new forced-on tests.

---

### Task 4: Update mod.rs spawn_opts to wire forced-on into resolve calls

**Files:**
- Modify: `server-rs/src/engine/mod.rs`

**Interfaces:**
- Consumes: `SpawnOptions::forced_on_plugins/skills/mcp_servers` (Task 2); updated `resolve_enabled_plugins` (Task 3); updated `resolve_session_hidden_skills` (Task 3); new `resolve_session_forced_on_skills` (Task 3)
- Produces: `SpawnOptions` built with `forced_on_*` fields populated; `hidden_skills` resolves with forced-on excluded; `forced_on_skills` carries only globally-off names needing explicit "on"

- [ ] **Step 1: Update the `spawn_opts` method** (around lines 2324-2337)

Find this block:
```rust
            hidden_skills: crate::engine::global_settings::resolve_session_hidden_skills(
                &self.0.cfg.claude_config_base,
                &s.hidden_skills,
            ),
            enabled_plugins: crate::engine::plugins::resolve_enabled_plugins(
                &self.0.cfg.claude_config_base,
                &s.hidden_plugins,
            ),
            hidden_mcp_servers: s.hidden_mcp_servers.clone(),
```

Replace with:
```rust
            hidden_skills: crate::engine::global_settings::resolve_session_hidden_skills(
                &self.0.cfg.claude_config_base,
                &s.hidden_skills,
                &s.forced_on_skills,
            ),
            forced_on_skills: crate::engine::global_settings::resolve_session_forced_on_skills(
                &self.0.cfg.claude_config_base,
                &s.forced_on_skills,
            ),
            enabled_plugins: crate::engine::plugins::resolve_enabled_plugins(
                &self.0.cfg.claude_config_base,
                &s.hidden_plugins,
                &s.forced_on_plugins,
            ),
            forced_on_plugins: s.forced_on_plugins.clone(),
            hidden_mcp_servers: s.hidden_mcp_servers.clone(),
            forced_on_mcp_servers: s.forced_on_mcp_servers.clone(),
            // forced_on_mcp_servers is stored and threaded but has no effect at spawn:
            // MCP has no global-off yet; the field is accepted for API/UI symmetry.
```

- [ ] **Step 2: Run `cargo check` to verify no remaining errors in engine**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo check 2>&1 | grep "error\[" | head -20
```

Expected: errors only in `api/sessions.rs` (CreateBody missing fields) and in `tests.rs` (existing tests that reference SpawnOptions need updating).

---

### Task 5: API validation — CreateBody + disjoint check + SubmitMeta mapping

**Files:**
- Modify: `server-rs/src/api/sessions.rs`

**Interfaces:**
- Consumes: `SubmitMeta::forced_on_plugins/skills/mcp_servers` (Task 2)
- Produces: `CreateBody::forced_on_plugins/skills/mcp_servers: Option<Vec<String>>`; validation returns 400 when same id appears in hidden list AND forced-on list of the same kind

- [ ] **Step 1: Write a validation test** (in `server-rs/src/engine/tests.rs` or in `sessions.rs` if it has a test block — check first)

First check if `sessions.rs` has a test block:
```bash
grep -n "#\[cfg(test)\]" /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/src/api/sessions.rs
```

If no test block exists in sessions.rs, the API validation is tested via integration tests in `tests.rs`. Look at the existing engine tests to understand how HTTP responses are tested. Given the spec says "400 with a clear message", we can test this at the SubmitMeta/engine level by checking that passing a conflicting set returns an error at the `submit_session` call.

Actually, since validation happens in `create_session` (the axum handler), and the engine tests don't exercise the HTTP layer, add a unit test helper: extract the validation logic into a free function, then test the function directly. However, the simplest approach given the existing pattern is: add the validation directly in `create_session` (no helper function needed), and test it via the full-chain test harness if available, OR document it as "tested via manual API call" in the report.

Check the existing test infrastructure:
```bash
grep -n "create_session\|StatusCode" /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/src/engine/tests.rs | head -10
```

If the engine tests do not exercise the HTTP layer, we validate only the SubmitMeta path (that conflicting ids cause the engine to error). Write a direct validation test by extracting a pure function:

In `sessions.rs`, add before `create_session`:
```rust
/// Returns `Some(error_message)` if the same component id appears in both a hidden list and
/// the corresponding forced-on list, which is invalid (two contradictory overrides).
fn validate_disjoint(
    hidden: &[String],
    forced_on: &[String],
    kind: &str,
) -> Option<String> {
    let hidden_set: std::collections::HashSet<&str> = hidden.iter().map(|s| s.as_str()).collect();
    for id in forced_on {
        if hidden_set.contains(id.as_str()) {
            return Some(format!("{kind}: \"{id}\" appears in both hidden and forcedOn lists — choose one"));
        }
    }
    None
}
```

Then write a unit test in a `#[cfg(test)]` block in `sessions.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_disjoint_catches_conflict() {
        let err = validate_disjoint(
            &["gh@m".to_string(), "sp@m".to_string()],
            &["gh@m".to_string()],
            "plugins",
        );
        assert!(err.is_some());
        assert!(err.unwrap().contains("gh@m"));
    }

    #[test]
    fn validate_disjoint_allows_non_overlapping_lists() {
        let err = validate_disjoint(
            &["gh@m".to_string()],
            &["sp@m".to_string()],
            "plugins",
        );
        assert!(err.is_none());
    }

    #[test]
    fn validate_disjoint_empty_lists_ok() {
        assert!(validate_disjoint(&[], &[], "skills").is_none());
        assert!(validate_disjoint(&["a".to_string()], &[], "mcp").is_none());
        assert!(validate_disjoint(&[], &["a".to_string()], "plugins").is_none());
    }
}
```

- [ ] **Step 2: Run the validation test to confirm it fails**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test sessions::tests 2>&1 | tail -20
```

Expected: compile error (function doesn't exist yet).

- [ ] **Step 3: Add the three fields to `CreateBody`** (after `hidden_mcp_servers` around line 215)

```rust
    /// Plugin ids to force ON for this session. Must be disjoint from hiddenPlugins.
    #[serde(rename = "forcedOnPlugins")]
    pub forced_on_plugins: Option<Vec<String>>,
    /// Skill names to force ON for this session. Must be disjoint from hiddenSkills.
    #[serde(rename = "forcedOnSkills")]
    pub forced_on_skills: Option<Vec<String>>,
    /// MCP server names to force ON (stored; no-op at spawn until global MCP disable exists).
    #[serde(rename = "forcedOnMcpServers")]
    pub forced_on_mcp_servers: Option<Vec<String>>,
```

- [ ] **Step 4: Add the `validate_disjoint` helper and the disjoint validation calls** in `create_session` (in `sessions.rs`)

Add the helper function before `create_session`. Then in `create_session`, after the `extra_mcp_servers` validation block and before building `meta`, add:

```rust
    let forced_on_plugins  = b.forced_on_plugins.unwrap_or_default();
    let forced_on_skills   = b.forced_on_skills.unwrap_or_default();
    let forced_on_mcp      = b.forced_on_mcp_servers.unwrap_or_default();
    let hidden_plugins_v   = b.hidden_plugins.as_deref().unwrap_or(&[]);
    let hidden_skills_v    = b.hidden_skills.as_deref().unwrap_or(&[]);
    let hidden_mcp_v       = b.hidden_mcp_servers.as_deref().unwrap_or(&[]);

    if let Some(msg) = validate_disjoint(hidden_plugins_v, &forced_on_plugins, "hiddenPlugins/forcedOnPlugins") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Some(msg) = validate_disjoint(hidden_skills_v, &forced_on_skills, "hiddenSkills/forcedOnSkills") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Some(msg) = validate_disjoint(hidden_mcp_v, &forced_on_mcp, "hiddenMcpServers/forcedOnMcpServers") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
```

- [ ] **Step 5: Extend the `meta` build to include forced-on fields** (replace the existing `let meta = SubmitMeta { ... }` block)

After the validation, change the meta construction:
```rust
    let meta = SubmitMeta {
        model: b.model,
        effort: b.effort,
        mode: b.mode,
        permission_mode: b.permission_mode,
        hidden_skills: b.hidden_skills.unwrap_or_default(),
        hidden_plugins: b.hidden_plugins.unwrap_or_default(),
        hidden_mcp_servers: b.hidden_mcp_servers.unwrap_or_default(),
        extra_mcp_servers,
        claude_md: b.claude_md,
        staged_uploads: b.staged_uploads.unwrap_or_default(),
        forced_on_plugins,
        forced_on_skills,
        forced_on_mcp_servers: forced_on_mcp,
    };
```

Note: `hidden_plugins`, `hidden_skills`, `hidden_mcp_servers` are now `unwrap_or_default()` directly from `b`; the `forced_on_*` locals were already computed above.

- [ ] **Step 6: Run the validation tests**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test sessions::tests 2>&1 | tail -20
```

Expected: 3 tests pass.

- [ ] **Step 7: Run `cargo check` to ensure compilation**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo check 2>&1 | grep "error\[" | head -20
```

Expected: no errors (or only in tests.rs for the spawn_opts test needing update).

---

### Task 6: sdk_runner.rs env injection + sdk-bridge.mjs skillOverrides "on" entries

**Files:**
- Modify: `server-rs/src/engine/sdk_runner.rs`
- Modify: `server-rs/sdk-bridge.mjs`

**Interfaces:**
- Consumes: `RunSpec::forced_on_skills: Vec<String>` (Task 2)
- Produces: `SDK_BRIDGE_FORCED_ON_SKILLS` env (JSON name array) when non-empty; `sdk-bridge.mjs` reads it and sets `settings.skillOverrides[name]="on"` for each, applied after "off" entries so forced-on wins

- [ ] **Step 1: Write a failing sdk_runner test** (in `sdk_runner.rs` test block)

Add this test after `start_passes_hidden_skills_to_bridge_env` (around line 570):

```rust
    #[test]
    fn start_passes_forced_on_skills_to_bridge_env() {
        let dir = tmpdir();
        let rec = dir.join("rec.txt");
        let log = dir.join("session.jsonl");
        let fake = fake_node(&dir, &rec);
        let bridge = dir.join("bridge.mjs");
        std::fs::write(&bridge, "// fake bridge").unwrap();

        let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), std::env::var("PATH").unwrap_or_default());

        let spec = RunSpec {
            cwd: dir.to_string_lossy().into_owned(),
            env,
            log_path: log,
            forced_on_skills: vec!["rke2-ops".into(), "".into(), "cloudstack-ops".into()],
            ..Default::default()
        };

        let handle = runner.start(spec);
        let mut recorded = String::new();
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded = std::fs::read_to_string(&rec).unwrap_or_default();
            if recorded.contains("SDK_BRIDGE_FORCED_ON_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]") {
                break;
            }
        }
        handle.stop();

        assert!(
            recorded.contains("SDK_BRIDGE_FORCED_ON_SKILLS=[\"rke2-ops\",\"cloudstack-ops\"]"),
            "forced-on skills env must be compact JSON without blank entries; recorded={recorded}"
        );
    }
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test start_passes_forced_on_skills 2>&1 | tail -20
```

Expected: compile error (RunSpec missing `forced_on_skills` — Task 2 should have fixed this, so expected failure mode: the env is not set, so the assert fails at runtime).

- [ ] **Step 3: Add the env injection in `sdk_runner.rs`** (after the `hidden_skills` env block, around line 224)

After the `SDK_BRIDGE_HIDDEN_SKILLS` block (lines 221-225), add:

```rust
        // Forced-on skills: globally-off skills the session forces back ON.
        // The bridge applies these as skillOverrides[name]="on" AFTER the "off" entries.
        let forced_on_skills: Vec<&str> = spec
            .forced_on_skills
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !forced_on_skills.is_empty() {
            if let Ok(json) = serde_json::to_string(&forced_on_skills) {
                cmd.env("SDK_BRIDGE_FORCED_ON_SKILLS", json);
            }
        }
```

- [ ] **Step 4: Run the sdk_runner test to verify it passes**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test start_passes_forced_on_skills 2>&1 | tail -20
```

Expected: PASS (the fake node records the env).

- [ ] **Step 5: Update the boot log line in sdk-bridge.mjs** (around line 44)

In `sdk-bridge.mjs`, after:
```js
  `hidden_skills=${process.env.SDK_BRIDGE_HIDDEN_SKILLS || ""} ` +
```
add:
```js
  `forced_on_skills=${process.env.SDK_BRIDGE_FORCED_ON_SKILLS || ""} ` +
```

- [ ] **Step 6: Add the `SDK_BRIDGE_FORCED_ON_SKILLS` skillOverrides "on" application in `sdk-bridge.mjs`**

Find the existing skillOverrides block (around line 224-226):
```js
const hiddenSkills = parseNameListEnv("SDK_BRIDGE_HIDDEN_SKILLS");
if (hiddenSkills.length) {
  settings.skillOverrides = Object.fromEntries(hiddenSkills.map((name) => [name.trim(), "off"]));
```

Replace this block with:
```js
const hiddenSkills = parseNameListEnv("SDK_BRIDGE_HIDDEN_SKILLS");
const forcedOnSkills = parseNameListEnv("SDK_BRIDGE_FORCED_ON_SKILLS");
if (hiddenSkills.length || forcedOnSkills.length) {
  // Build skillOverrides: "off" entries first, then "on" entries (forced-on wins).
  const overrides = {};
  for (const name of hiddenSkills) { if (name.trim()) overrides[name.trim()] = "off"; }
  for (const name of forcedOnSkills) { if (name.trim()) overrides[name.trim()] = "on"; }
  if (Object.keys(overrides).length) settings.skillOverrides = overrides;
}
```

This ensures that if the same skill name appears in both lists (which the API rejects, but belt-and-suspenders), the "on" wins.

- [ ] **Step 7: Verify `sdk-bridge.mjs` change looks correct**

```bash
grep -n "forcedOnSkills\|FORCED_ON_SKILLS\|skillOverrides" /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/sdk-bridge.mjs | head -15
```

Expected: both the boot log and the skillOverrides block reference `FORCED_ON_SKILLS`.

---

### Task 7: Engine integration test — spawn_opts threads forced-on fields

**Files:**
- Modify: `server-rs/src/engine/tests.rs`

**Interfaces:**
- Consumes: all previous tasks complete and compiling

- [ ] **Step 1: Find the existing `spawn_opts_threads_mcp_fields` test** (around line 2887)

Read lines 2887-2960 of `tests.rs` to understand the test structure:

```bash
grep -n "spawn_opts_threads_mcp_fields\|forced_on\|hidden_mcp" /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/src/engine/tests.rs | head -20
```

- [ ] **Step 2: Add a new full-chain test for forced-on threading** (after `spawn_opts_threads_mcp_fields`)

The pattern mirrors the existing MCP threading test. Add:

```rust
    /// forced_on_plugins / forced_on_skills / forced_on_mcp_servers must thread from
    /// submit_session → store → spawn_opts and appear verbatim on SpawnOptions.
    /// (A missing `.forced_on_*` assignment in the spawn_opts build causes this test to FAIL.)
    #[tokio::test]
    async fn spawn_opts_threads_forced_on_fields() {
        let dir = tmp_dir();
        // Register a fake repo so submit_session can find a worktree root.
        let repo_root = dir.join("myrepo");
        std::fs::create_dir_all(&repo_root).unwrap();
        // Run git init so the worktree sync doesn't fail.
        let _ = std::process::Command::new("git").arg("init").current_dir(&repo_root).output();

        let cfg = build_test_cfg(&dir);
        let store = crate::engine::store::Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let engine = build_test_engine(cfg, store.clone()).await;

        let plugins = vec!["superpowers@claude-plugins-official".to_string()];
        let skills  = vec!["rke2-ops".to_string()];
        let mcp     = vec!["forced-server".to_string()];

        let id = engine
            .submit_session(
                vec![],
                vec![],
                "test forced-on threading".to_string(),
                std::collections::HashMap::new(),
                crate::engine::SubmitMeta {
                    forced_on_plugins:     plugins.clone(),
                    forced_on_skills:      skills.clone(),
                    forced_on_mcp_servers: mcp.clone(),
                    hidden_plugins: vec![],
                    hidden_mcp_servers: vec![],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let s = store.get(&id).await.unwrap().unwrap();
        assert_eq!(s.forced_on_plugins,     plugins, "forced_on_plugins not persisted to store");
        assert_eq!(s.forced_on_skills,      skills,  "forced_on_skills not persisted to store");
        assert_eq!(s.forced_on_mcp_servers, mcp,     "forced_on_mcp_servers not persisted to store");

        // Build a QueueItem to call spawn_opts.
        let item = build_test_queue_item(&id, &s);
        let opts = engine.spawn_opts(&s, &item);

        // forced_on_plugins threads verbatim to SpawnOptions.
        assert_eq!(opts.forced_on_plugins, plugins, "forced_on_plugins not in SpawnOptions");
        // forced_on_mcp_servers threads verbatim (even though it's a spawn no-op).
        assert_eq!(opts.forced_on_mcp_servers, mcp, "forced_on_mcp_servers not in SpawnOptions");
        // forced_on_skills: since there are no global overrides in test, rke2-ops is globally on,
        // so resolve_session_forced_on_skills returns [] (no explicit "on" needed).
        // The important thing: forced_on_skills in the SpawnOptions is empty (no assertion needed
        // for the exact value — the resolve behavior is tested in global_settings unit tests).
    }
```

Note: if `build_test_cfg`, `build_test_engine`, `tmp_dir`, `build_test_queue_item` are the actual helper names used in `tests.rs`, adapt accordingly. Read the top of `tests.rs` to find the exact helper function names before writing this test.

- [ ] **Step 3: Read the actual helper names in tests.rs**

```bash
grep -n "^fn tmp_dir\|^fn build_test\|^async fn build_test\|^fn fake_engine\|fn test_engine\|fn make_engine\|let engine = " /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/src/engine/tests.rs | head -20
```

Adjust the test body to use the correct helper names and patterns found.

- [ ] **Step 4: Run the new test**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test spawn_opts_threads_forced_on_fields 2>&1 | tail -30
```

Expected: PASS.

- [ ] **Step 5: Update existing spawn_opts tests that construct SpawnOptions / QueueItem / SubmitMeta literals**

Search for existing tests that will break due to new required fields:

```bash
grep -n "hidden_mcp_servers: vec!" /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/src/engine/tests.rs | head -20
```

For every struct literal that now has missing `forced_on_*` fields, add:
```rust
forced_on_plugins: vec![],
forced_on_skills: vec![],
forced_on_mcp_servers: vec![],
```

Or use `..Default::default()` if the struct already uses it.

---

### Task 8: Full suite run + build + write report

**Files:**
- Run tests
- Create: `.superpowers/sdd/s3-report.md`

- [ ] **Step 1: Run the full test suite**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test 2>&1 | tail -30
```

Expected: all tests pass. If `start_passes_forced_on_skills_to_bridge_env` or another sdk_runner test flakes (timeout under parallel load), re-run it individually:

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo test start_passes_forced_on_skills -- --nocapture 2>&1 | tail -30
```

- [ ] **Step 2: Run a release build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs && cargo build 2>&1 | tail -10
```

Expected: `Compiling agentic-dev v... Finished release`.

- [ ] **Step 3: Write the report** to `.superpowers/sdd/s3-report.md`

```bash
mkdir -p /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/.superpowers/sdd
```

Create the report with:
- Files changed list
- Resolve-precedence logic summary
- Test evidence (commands + pass counts)
- Concerns / deviations
- Note: sdk-bridge.mjs changes are not covered by the Rust test suite (JS is untested)

- [ ] **Step 4: Commit all changes**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev && git add server-rs/src/engine/store.rs server-rs/src/engine/mod.rs server-rs/src/engine/runner.rs server-rs/src/engine/spawner.rs server-rs/src/engine/plugins.rs server-rs/src/engine/global_settings.rs server-rs/src/engine/sdk_runner.rs server-rs/src/engine/tests.rs server-rs/src/api/sessions.rs server-rs/sdk-bridge.mjs .superpowers/sdd/s3-report.md docs/superpowers/plans/2026-07-10-s3-tristate-override.md
```

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev && git commit -m "$(cat <<'EOF'
feat(s3): tri-state per-session override (forcedOn* fields)

Add forcedOnPlugins, forcedOnSkills, forcedOnMcpServers per-session fields
that override the global baseline for a session. Precedence: forcedOn >
hidden > global inherit. Threaded through store/engine/api/sdk-bridge.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage check:**

| Spec requirement | Task that implements it |
|-----------------|------------------------|
| Three new Vec<String> fields serde camelCase | Task 1 (store.rs) |
| `#[serde(default)]` | Task 1 (store.rs) |
| 3 new defaulted DB TEXT columns | Task 1 (store.rs, ADDED_COLUMNS) |
| INSERT column list + placeholders + bindings | Task 1 (store.rs) |
| row→Session read mapping | Task 1 (store.rs, row_to_session) |
| CreateInput gains 3 fields | Task 1 (store.rs) |
| CreateBody optional fields | Task 5 (sessions.rs) |
| API 400 disjoint validation | Task 5 (sessions.rs) |
| SubmitMeta 3 fields | Task 2 (mod.rs) |
| RunSpec 3 fields | Task 2 (runner.rs) |
| SpawnOptions 3 fields | Task 2 (spawner.rs) |
| build_spec maps all 3 | Task 2 (spawner.rs) |
| submit_session build site | Task 2 (mod.rs) |
| fork_session copy | Task 2 (mod.rs) |
| resolve_enabled_plugins forced_on param + precedence | Task 3 (plugins.rs) |
| mod.rs call site passes forced_on_plugins | Task 4 (mod.rs) |
| existing plugin tests pass empty forced-on | Task 3 (plugins.rs) |
| resolve_session_hidden_skills gains forced_on param | Task 3 (global_settings.rs) |
| resolve_session_forced_on_skills new fn | Task 3 (global_settings.rs) |
| mod.rs call site for skill resolves | Task 4 (mod.rs) |
| SDK_BRIDGE_FORCED_ON_SKILLS env | Task 6 (sdk_runner.rs) |
| sdk-bridge.mjs skillOverrides "on" | Task 6 (sdk-bridge.mjs) |
| MCP forced-on is no-op with comment | Task 2 + Task 4 |
| Store round-trip test | Task 1 |
| Plugin resolve precedence tests | Task 3 |
| Skill resolve tests | Task 3 |
| sdk_runner env test | Task 6 |
| Full-chain spawn_opts test | Task 7 |
| Report to .superpowers/sdd/s3-report.md | Task 8 |
| `make test` green before commit | Task 8 |
| `cargo build` compiles | Task 8 |

**Placeholder scan:** No TBD/TODO markers in any step — all code is shown explicitly.

**Type consistency:**
- `forced_on_plugins: Vec<String>` used consistently across Session, CreateInput, SubmitMeta, RunSpec, SpawnOptions, and SpawnOptions → SpawnOptions
- `forcedOnPlugins` camelCase rename used in Session (serde) and CreateBody (serde)
- `resolve_enabled_plugins(dir, hidden, forced_on)` — 3-arg signature used consistently in Task 3 and Task 4
- `resolve_session_hidden_skills(base, hidden, forced_on)` — 3-arg signature used consistently in Task 3 and Task 4
- `resolve_session_forced_on_skills(base, forced_on)` — new 2-arg fn used in Task 3 and Task 4
