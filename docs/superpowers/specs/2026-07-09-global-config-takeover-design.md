# Global Config Takeover — Design (Slice S4)

- **Date:** 2026-07-09
- **Status:** Draft (awaiting user review)
- **Scope of THIS spec:** Slice **S4** — non-destructive adoption of the local `~/.claude`
  config + a global settings layer that writes back to disk (so it also affects the standalone
  `claude` CLI), plus the minimal read foundation (S1 subset) it needs and the minimal
  session-inheritance fix that makes it end-to-end useful.
- **Repo:** `agentic-dev` (backend only). Android UI is a separate repo/slice (S5).

---

## 1. Motivation & the bigger program

Today agentic-dev exposes two per-session toggles in New Request — skills (`GET /api/skills`)
and plugins (`GET /api/plugins`) — each a *blacklist* persisted in session meta and applied at
spawn time through the command-line `--settings` layer. There is **no MCP management, no global
settings, and no notion of a shared baseline that sessions inherit**.

The user wants to refactor this into a coherent system:

1. Treat skills / plugins / MCP (and, later, any other toggleable component) uniformly.
2. Let agentic-dev **detect and take over** the local Claude Code install (`~/.claude`).
3. Provide a **global settings** surface whose toggles also affect the `claude` CLI itself.
4. Have each session **default to the global settings and override on top** (like today).

That is several subsystems. We decomposed it into slices, each with its own spec → plan →
implementation cycle:

| Slice | Title | Status |
|-------|-------|--------|
| **S1** | Unified component read layer (backend, read-only) | subset folded into S4 |
| **S2** | MCP per-session toggle + in-session ad-hoc add | later |
| **S3** | Per-session override generalized as a delta over global (tri-state) | later |
| **S4** | Global adoption + global write-back to `~/.claude` | **THIS SPEC** |
| **S5** | Android UI (global settings screen + generalized Filters) | later |

The user chose to start with **S4** — build the global-takeover skeleton first.

## 2. Three-tier configuration model (background)

The whole program rests on three layers, low → high precedence:

1. **On-disk Claude Code config** — `~/.claude/settings.json`, `~/.claude/settings.local.json`,
   `~/.claude.json` (`mcpServers`), `.mcp.json`, `plugins/installed_plugins.json`, `skills/`.
   This is what the `claude` CLI itself reads. Ground truth.
2. **agentic-dev global layer** — the toggles the user flips in the app's global settings. To
   satisfy "must also affect the CLI", this layer is **persisted into the on-disk config**
   (see §5 for the exact file). It is not a private overlay.
3. **Per-session override** — each session starts from the effective global config and applies
   its own overrides, injected at spawn through the ephemeral command-line `--settings` layer
   (never written to disk). This is today's mechanism, generalized in S3.

**Effective(session) = on-disk base ⊕ global writes ⊕ session override.** This mirrors Claude
Code's own settings precedence (user file → command-line layer); we are not inventing a new
merge engine.

## 3. Local reality (verified 2026-07-09 on this machine)

- `~/.claude` exists (full install).
- `~/.claude/settings.json` holds only `env`, `model`, `skipDangerousModePermissionPrompt`,
  `tui`. **None** of `enabledPlugins`, `skillOverrides`, `disabledMcpjsonServers`,
  `enabledMcpjsonServers` are set → everything installed is on by default.
- `~/.claude/settings.local.json` exists and holds `permissions` (written by the harness).
- 13 plugins in `plugins/installed_plugins.json`; real ids use marketplace suffixes
  `@claude-plugins-official` / `@cloudflare` (not the `@official` used in existing unit tests).
- 5 user skills in `~/.claude/skills/` (standalone; plugin-provided skills arrive via plugins).
- `~/.claude.json` is 64 KB of mostly cache/state; **no user-scope and no project-scope
  `mcpServers`** currently exist → today all MCP servers come from plugins + the in-process
  `delegate` server. ⇒ Global MCP write-back has no target yet; S4 designs the seam but does not
  implement MCP writes.
- `~/.claude/backups/` already exists → reuse it for pre-write backups.

## 4. S4 scope

**In scope**

1. **Adoption / detection (non-destructive).** On startup, detect `~/.claude`. Read current
   state as the global baseline. **Write nothing on adoption.** Missing dir → empty baseline;
   the first toggle scaffolds the target file.
2. **Unified read layer (S1 subset).** Merge `skills.rs` + `plugins.rs` enumeration into a
   `components` model with a common shape and each component's *effective global enabled* state.
   MCP is a stub (enumerated as empty for now).
3. **Global settings API (read + write).** `GET /api/global-settings` returns components with
   their global on/off; `POST /api/global-settings/toggle` flips one component globally by
   writing back to disk.
4. **Write-safety substrate.** Reuse `engine/atomic_write.rs`; back up the target file to
   `~/.claude/backups/` before each write; read-modify-write only owned keys; preserve unknown
   keys; refuse to write over corrupt JSON.
5. **Minimal session-inheritance fix** (the S4↔S3 seam, §6.3) so that a globally-disabled
   component actually stays disabled inside agentic-dev sessions — making S4 end-to-end useful,
   not just CLI-only.

**Out of scope (deferred, must not be broken)**

- MCP per-session toggle + ad-hoc add → **S2**.
- Full tri-state per-session override (inherit / force-on / force-off) → **S3**. S4 only removes
  the force-on-everything clobber; it does not add a force-on-above-global capability.
- Android UI → **S5**.
- Global toggles for component types beyond plugins + skills (subagents, hooks, commands,
  output styles). The model leaves an extension point but S4 ships only plugins + skills.

## 5. Design decision: write target = `settings.local.json`

Global writes go to **`~/.claude/settings.local.json`**, not `~/.claude/settings.json`.

- `settings.local.json` is the machine-local override layer (gitignored, already present). It is
  the least-surprising place for an automated tool to write, and it **never disturbs the user's
  hand-curated `settings.json`**. "Reset to default" = delete our key → the pristine
  `settings.json` / installed-default shows through.
- Same-scope precedence: `settings.local.json` overrides `settings.json`, so writes here still
  take effect for the CLI. (Implementation must **verify** this user-scope precedence; the
  harness already writes `permissions` here, strong evidence it is read.)
- We touch only the keys we own and preserve everything else (e.g. the existing `permissions`).

**Write-target mapping (implementable today):**

| Component | Global disable | Re-enable (back to default) |
|-----------|----------------|------------------------------|
| Plugin | `enabledPlugins["<id>@<marketplace>"] = false` | delete that key |
| Skill | `skillOverrides["<name>"] = "off"` | delete that key |
| MCP | *(deferred — no target yet; design in S2)* | — |

We write an explicit `false` / `"off"` only for components the user actually toggles off
("改动才写"). Untouched components get no key and inherit the on-disk default. Re-enabling a
component we previously disabled deletes the key rather than writing `true`, keeping the file
minimal and `settings.json` authoritative for defaults.

## 6. Components, data flow, and the seam

### 6.1 Modules (engine stays free of axum)

- **`engine/components.rs`** (new) — `ComponentInfo { kind, id, name, description, source,
  global_enabled }` and an enumerator that merges `skills::list_skills` + `plugins::list_plugins`
  (+ MCP stub) and computes `global_enabled` from the effective settings.
  - `kind`: `"skill" | "plugin" | "mcp"`.
  - `source`: `"user" | "project" | "plugin"` (skills dir = user; installed plugins = plugin;
    MCP later).
- **`engine/global_settings.rs`** (new) — read/parse the toggle keys from
  `settings.local.json` (with `settings.json` underneath for effective state), compute effective
  global enabled per component, and apply a single toggle via surgical read-modify-write with
  backup + `atomic_write::write_file_atomic`.
- **`api/misc.rs`** (or a small new `api/global.rs`) — the two axum handlers, mirroring the
  existing `skills_route` / `plugins_route`.

### 6.2 API

`GET /api/global-settings`
```jsonc
[
  { "kind": "plugin", "id": "github@claude-plugins-official", "name": "github",
    "description": "...", "source": "plugin", "globalEnabled": true },
  { "kind": "skill", "id": "rke2-ops", "name": "rke2-ops",
    "description": "...", "source": "user", "globalEnabled": false }
]
```
Read flow:
1. Read `settings.local.json` + `settings.json` toggle keys (`enabledPlugins`, `skillOverrides`);
   `settings.local.json` wins.
2. Enumerate installed plugins (`installed_plugins.json`) and skills (`skills/`).
3. `globalEnabled`:
   - plugin = `enabledPlugins[id]` if present, else `true` (installed ⇒ default on).
   - skill = `false` if `skillOverrides[name] == "off"` (in either file, local wins), else `true`.
4. Best-effort: unreadable/corrupt files degrade to empty maps (like today's `list_plugins`).

`POST /api/global-settings/toggle`
```jsonc
// request
{ "kind": "plugin", "id": "github@claude-plugins-official", "enabled": false }
// response: the new effective ComponentInfo (or the full refreshed list)
```
Write flow:
1. Read current `settings.local.json` (missing ⇒ `{}`; **corrupt ⇒ abort with error**, never
   overwrite).
2. Back up the existing file to `~/.claude/backups/settings.local.json.<timestamp>.bak`
   (skip if the file does not exist yet). Prune to the last N (e.g. 20).
3. Mutate only the owned key per §5 (disable ⇒ set `false`/`"off"`; enable ⇒ delete the key),
   preserving all other keys (`permissions`, etc.).
4. `write_file_atomic` the serialized JSON.
5. Return the recomputed effective state.

Validation: unknown `kind`, or an `id` that is not installed/enumerable ⇒ 4xx.

### 6.3 The S4↔S3 seam — minimal session-inheritance fix

Today `engine/mod.rs` (~line 1825) builds each session's plugin map via
`plugins::resolve_enabled_plugins(claude_config_base, hidden_plugins)`, which writes **`true` for
every installed plugin** not in the session blacklist. Injected through the command-line
`--settings` layer, that `true` **overrides** a global `false` in `settings.local.json` — so a
globally-disabled plugin would be force-enabled inside agentic-dev sessions, defeating the whole
feature (it would only work for the standalone CLI).

**Fix (S4):** make the session inherit the global baseline instead of force-enabling everything.
Concretely, when resolving the per-session map:

- Emit `false` for components the session explicitly hides (`hidden_plugins` / `hidden_skills`).
- **Omit** components the session does not touch, so the on-disk global (`settings.local.json` /
  `settings.json`) shows through unchanged.

Implementation options (decide in the plan): either change `resolve_enabled_plugins` to stop
emitting `true` for untouched plugins, or intersect its output with the adopted global state so a
globally-`false` plugin is never re-emitted as `true`. Skills follow the same rule via
`skillOverrides`.

**Cost / explicitly deferred to S3:** this removes the current ability to force a
globally-disabled component *on* for a single session. That requires a tri-state per-session
override model (inherit / force-on / force-off) and a matching UI, which is S3. Since
`enabledPlugins` is currently empty on disk and nothing depends on the force-on behavior, dropping
it now is safe.

Regression test required: global-off plugin + session does **not** hide it ⇒ the resolved session
map must **not** contain `true` for it.

## 7. Write safety & concurrency

- Use `serde_json::Value` for read-modify-write; only owned keys change, all unknown keys are
  preserved verbatim.
- Every write is preceded by a backup and performed via `atomic_write::write_file_atomic`
  (temp + fsync + rename + parent-dir fsync).
- `~/.claude/backups/` is created if missing; keep the most recent N backups, prune older.
- Timestamps via `std::time::SystemTime` (available in the Rust runtime).
- An in-engine `Mutex` serializes global-settings writes within the process.
- Cross-process race (the CLI editing `settings.local.json` at the same instant) is a small,
  accepted risk for a single-user local tool: we read fresh immediately before writing and rename
  atomically; worst case we overwrite one concurrent hand-edit. Documented, not solved in S4.

## 8. Error handling

- Missing `settings.local.json` → read as `{}`; first write scaffolds it.
- **Corrupt JSON on write → refuse** (return error, leave file untouched); never silently reset.
- Corrupt JSON on read → degrade to empty (best-effort), consistent with `list_plugins`.
- Unknown `kind` / not-installed `id` → 4xx.
- Disk full / rename failure → `write_file_atomic` errors; original file intact; return 5xx.
- Missing `~/.claude` entirely → empty component list; toggles create files under a freshly
  created config dir.

## 9. Testing strategy

In-crate unit tests, no real `claude` (matches existing conventions):

- **`global_settings`**: effective-state merge (`settings.local.json` overrides `settings.json`);
  toggle writes the correct key; disable→`false`/`"off"`, enable→key deleted; unknown keys
  (`permissions`) preserved; corrupt file refused on write; missing file → created; backup file
  produced.
- **`components`**: skills + plugins merged enumeration; `global_enabled` computed correctly;
  `source` labeling; MCP stub empty.
- **Seam regression** (§6.3): globally-disabled plugin, session does not hide it ⇒ resolved map
  omits/`false`, never `true`.
- **API**: use the existing `api/mod.rs` test harness pattern (redirect `claude_config_base` and
  writable paths into a temp dir, as done today for `skills_dir` / `templates_path`).

`make test` (= `cargo test`) stays green; `cargo build` must compile.

## 10. Open questions to resolve during planning

1. Confirm user-scope precedence of `settings.local.json` over `settings.json` for
   `enabledPlugins` / `skillOverrides` (verify against the actual CLI).
2. Exact home for the seam fix: patch `resolve_enabled_plugins` vs. intersect at the `mod.rs`
   call site. Also generalize the same inheritance rule to skills.
3. Backup retention count / naming (`settings.local.json.<ts>.bak`) and prune policy.
4. Whether `GET /api/global-settings` supersedes or coexists with the existing `GET /api/skills`
   and `GET /api/plugins` (S5/UI concern; keep both for now to avoid breaking the current app).

## 11. Acceptance criteria

- `GET /api/global-settings` lists installed plugins + user skills with correct `globalEnabled`.
- `POST /api/global-settings/toggle` disabling a plugin writes
  `enabledPlugins["<id>"]=false` into `settings.local.json`, backs up the prior file, preserves
  `permissions`, and is reflected by a subsequent GET.
- Re-enabling deletes the key; `settings.json` is never modified.
- A globally-disabled plugin/skill is **not** force-enabled inside agentic-dev sessions
  (seam regression test passes).
- The standalone `claude` CLI observes the same global toggle (manual verification).
- `cargo build` compiles; `cargo test` green.
