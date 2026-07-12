# Router joint-scoring redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the delegate router's strict-lexicographic selection with an explicit-enable + capability-floor + joint-score design, so an equally-capable cheaper model wins and models can be turned off.

**Architecture:** The LLM still picks a per-task model, but its pick only sets the difficulty floor `d = capability(pick)`. A deterministic `select_model` re-ranks all candidates at least that capable by a joint score `S = (1−t)(1−cost) + t·capability + β·priority`, replacing `apply_priority`. A per-model `enabled` flag controls candidate membership; a global `tradeoff` (t) knob sets the cost⇄quality balance.

**Tech Stack:** Rust (`server-rs`, engine crate, serde, axum), Kotlin/Compose (`agentic-dev-android`).

**Spec:** `docs/superpowers/specs/2026-07-12-router-joint-scoring-redesign.md` (read it first, incl. §11 adversarial-review fixes).

## Global Constraints

- `enabled` defaults `true` (serde default); old `providers.json` / `native-overrides.json` load unchanged.
- Global `tradeoff` `t ∈ [0,1]` defaults `0.5`; priority nudge weight `β = 0.1` (constant, must stay `≪ 1`).
- Score main axis `(1−t)(1−cost) + t·capability ∈ [0,1]`; report decisions on `/1.0`.
- Engine (`server-rs/src/engine/`) stays free of axum imports.
- Tests never hit real `claude`/network — router tests use the injectable `AskFn` fake transport; file CRUD uses the data-race-free `*_FILE_OVERRIDE` static, never `env::set_var`.
- `make test` (`cd server-rs && cargo test`) green before each commit; `cargo fmt`.
- Two PRs: Phase A (backend) lands first, then Phase B (Android). Each via the repo's Codex-review auto-merge workflow, with a delegate adversarial-verify pass on the diff before commit.

---

# Phase A — Backend (`agentic-dev`, one PR)

## Task A1: Add `enabled` to Provider and NativeOverride

**Files:**
- Modify: `server-rs/src/engine/model/providers.rs` (struct `Provider`, add default fn + field)
- Modify: `server-rs/src/engine/model/native_overrides.rs` (struct `NativeOverride`)
- Test: in-file `#[cfg(test)]` modules in both

**Interfaces:**
- Produces: `Provider.enabled: bool`, `NativeOverride.enabled: bool`, `fn default_enabled() -> bool { true }`.

- [ ] **Step 1: Write the failing test** (providers.rs tests) — old JSON without `enabled` loads as `true`, and `enabled:false` round-trips.

```rust
#[test]
fn enabled_defaults_true_and_roundtrips() {
    // old file: no `enabled` field → true (back-compat)
    let p: Provider = serde_json::from_str(
        r#"{"name":"m","base_url":"u","model":"m1"}"#).unwrap();
    assert!(p.enabled);
    // explicit false round-trips
    let p2: Provider = serde_json::from_str(
        r#"{"name":"m","base_url":"u","model":"m1","enabled":false}"#).unwrap();
    assert!(!p2.enabled);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cd server-rs && cargo test -p <crate> enabled_defaults_true_and_roundtrips` → FAIL (no field `enabled`).

- [ ] **Step 3: Add the field + default fn** in `providers.rs`:

```rust
fn default_enabled() -> bool { true }
// in struct Provider, after `router`:
    /// Whether this model participates in routing at all. false → excluded from the
    /// candidate pool (and cannot act as the router). Defaults true (old files, unspecified).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
```
Add the same `default_enabled` + `enabled` field to `NativeOverride` in `native_overrides.rs`.

- [ ] **Step 4: Fix every `Provider { .. }` / `NativeOverride { .. }` literal** (tests, `from_env_defaults`, `candidates_from`) to set `enabled: true`. Run `cargo build` and fix each "missing field `enabled`" until it compiles.

- [ ] **Step 5: Run tests** — `cargo test` → PASS.

- [ ] **Step 6: Commit** — `git commit -am "feat(router): add enabled flag to Provider and NativeOverride (default true)"`

## Task A2: Global tradeoff config file + CRUD

**Files:**
- Create: `server-rs/src/engine/model/routing_config.rs`
- Modify: `server-rs/src/engine/model/mod.rs` (or the model module's `mod` list) to `pub mod routing_config;`
- Test: in-file `#[cfg(test)]`

**Interfaces:**
- Produces: `struct RoutingConfig { tradeoff: f32 }` (default 0.5); `load() -> RoutingConfig`; `save(&RoutingConfig) -> io::Result<()>`; `routing_config_file_path()`; test-override `ROUTING_FILE_OVERRIDE: Mutex<Option<PathBuf>>`.

- [ ] **Step 1: Write the failing test** — default when missing, clamp to [0,1], round-trip, corrupt-errors-not-wipes. Mirror `native_overrides.rs` tests.

```rust
#[test]
fn tradeoff_defaults_clamps_and_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("routing.json");
    assert_eq!(load_from(&f).tradeoff, 0.5);              // missing → default
    save_to(&f, &RoutingConfig { tradeoff: 1.7 }).unwrap();
    assert!((load_from(&f).tradeoff - 1.0).abs() < f32::EPSILON); // clamped
}
```

- [ ] **Step 2: Run to verify it fails** — module doesn't exist → FAIL.

- [ ] **Step 3: Implement** `routing_config.rs` mirroring `native_overrides.rs` (atomic temp+rename write, 0600 on unix, `FILE_LOCK`, `load_from`/`save_to` taking a path, corrupt→`Err`, `ROUTING_FILE_OVERRIDE` static, env `AGENTIC_ROUTING_FILE` else `~/.agentic-dev/routing.json`). `load()` clamps `tradeoff` into `[0,1]`.

- [ ] **Step 4: Run tests** → PASS.

- [ ] **Step 5: Commit** — `git commit -am "feat(router): global tradeoff config (routing.json) with CRUD"`

## Task A3: `select_model` joint scorer (core — replaces `apply_priority`)

**Files:**
- Modify: `server-rs/src/engine/model/router.rs` (add `select_model`, keep `apply_priority` until A4 rewires, then delete)
- Test: `router.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Provider` (`.capability/.cost/.priority`), `providers::is_native`, `providers::resolve_candidate`.
- Produces: `pub(crate) fn select_model(picked: &Provider, candidates: &[&Provider], t: f32, beta: f32) -> RouteChoice`. `pub(crate) const PRIORITY_BETA: f32 = 0.1;`

- [ ] **Step 1: Write the failing tests** (the §5 fix + guards from §11):

```rust
#[test]
fn select_model_prefers_more_capable_at_equal_cost_priority() {
    providers::seed_claude_models_for_tests();
    let ds = p("deepseek","deepseek-v4-pro",0.90,0.0,0.50,Protocol::Anthropic,"k");
    let mut cat = vec![ds.clone()];
    cat.extend(providers::native_claude_candidates(&Default::default()));
    let cands: Vec<&Provider> = cat.iter().collect();
    let sonnet = cands.iter().find(|c| c.matches("sonnet")).copied().unwrap();
    // LLM picked sonnet → floor 0.85; deepseek (0.90) must win the joint score.
    let got = select_model(sonnet, &cands, 0.5, PRIORITY_BETA);
    assert_eq!(got.model, "deepseek-v4-pro");
}

#[test]
fn beta_priority_is_a_nudge_not_an_override() {
    // A leads B by 0.30 on capability at equal cost; B has priority 1. β=0.1 must NOT flip it.
    let a = p("a","a",0.85,0.0,0.50,Protocol::Anthropic,"k");
    let b = p("b","b",0.55,1.0,0.50,Protocol::Anthropic,"k");
    let cat = vec![a.clone(), b.clone()];
    let cands: Vec<&Provider> = cat.iter().collect();
    // pick = b so floor=0.55 lets both through; strong A must still win.
    let got = select_model(&b, &cands, 0.5, PRIORITY_BETA);
    assert_eq!(got.model, "a");
}

#[test]
fn knob_extremes_move_cheaper_to_stronger() {
    let cheap = p("cheap","cheap",0.85,0.0,0.20,Protocol::Anthropic,"k");
    let strong = p("strong","strong",0.97,0.0,0.90,Protocol::Anthropic,"k");
    let cat = vec![cheap.clone(), strong.clone()];
    let cands: Vec<&Provider> = cat.iter().collect();
    assert_eq!(select_model(&cheap, &cands, 0.0, PRIORITY_BETA).model, "cheap");   // t=0 → cheapest
    assert_eq!(select_model(&strong, &cands, 1.0, PRIORITY_BETA).model, "strong"); // t=1 → strongest
}

#[test]
fn empty_floor_falls_back_to_most_capable() {
    // pick capability above every candidate → floor empties → most-capable wins.
    let a = p("a","a",0.50,0.0,0.10,Protocol::Anthropic,"k");
    let b = p("b","b",0.70,0.0,0.90,Protocol::Anthropic,"k");
    let phantom = p("x","x",0.99,0.0,0.50,Protocol::Anthropic,"k");
    let cat = vec![a, b.clone()];
    let cands: Vec<&Provider> = cat.iter().collect();
    assert_eq!(select_model(&phantom, &cands, 0.5, PRIORITY_BETA).model, "b");
}
```

- [ ] **Step 2: Run to verify they fail** — `select_model` undefined → FAIL.

- [ ] **Step 3: Implement `select_model`:**

```rust
pub(crate) const PRIORITY_BETA: f32 = 0.1;

/// Deterministic final pick. The LLM's `picked` model sets the difficulty floor
/// `d = capability(picked)`; among candidates at least that capable, the highest joint
/// score `S = (1−t)(1−cost) + t·capability + β·priority` wins. Empty floor → most capable.
/// Tie-break: higher S → lower cost → higher capability → registered over native → stable.
pub(crate) fn select_model(
    picked: &Provider,
    candidates: &[&Provider],
    t: f32,
    beta: f32,
) -> RouteChoice {
    const EPS: f32 = 1e-4;
    let d = picked.capability;
    let mut pool: Vec<&Provider> =
        candidates.iter().copied().filter(|c| c.capability + EPS >= d).collect();
    if pool.is_empty() {
        if let Some(m) = candidates.iter().copied()
            .max_by(|a, b| a.capability.total_cmp(&b.capability)) {
            pool.push(m);
        }
    }
    let score = |m: &Provider| (1.0 - t) * (1.0 - m.cost) + t * m.capability + beta * m.priority;
    let best = pool.iter().copied().enumerate().max_by(|(ia, a), (ib, b)| {
        score(a).total_cmp(&score(b))
            .then(b.cost.total_cmp(&a.cost))                       // lower cost wins
            .then(a.capability.total_cmp(&b.capability))           // higher capability wins
            .then(crate::engine::providers::is_native(a)
                  .cmp(&crate::engine::providers::is_native(b)).reverse()) // registered (false) wins
            .then(ib.cmp(ia))                                      // lower index wins (stable)
    }).map(|(_, m)| m).unwrap_or(picked);
    // reason on the /1.0 main axis (exclude β·priority) so the print is apples-to-apples
    let main = |m: &Provider| (1.0 - t) * (1.0 - m.cost) + t * m.capability;
    let runner = pool.iter().copied().filter(|m| m.name != best.name)
        .max_by(|a, b| score(a).total_cmp(&score(b)));
    let reason: String = match runner {
        Some(r) => format!("floor {:.2}, S {:.2} > {} {:.2}", d, main(best), r.model, main(r)),
        None => format!("floor {:.2}, only candidate", d),
    }.chars().take(80).collect();
    RouteChoice { model: best.model.clone(), reason }
}
```

- [ ] **Step 4: Run tests** → PASS. Run `cargo test -p <crate> select_model` and the three named tests.

- [ ] **Step 5: Commit** — `git commit -am "feat(router): joint-score select_model (floor + weighted score)"`

## Task A4: Rewire routing to `select_model`; delete `apply_priority`

**Files:**
- Modify: `server-rs/src/engine/model/router.rs` (`route_batch`; remove `apply_priority` + its tests, port them to `select_model`)
- Modify: `server-rs/src/engine/workflow/delegate.rs` (`route_via_native_claude` final `apply_priority` call)

**Interfaces:**
- Consumes: `select_model`, `providers::resolve_candidate`, `routing_config::load().tradeoff`.

- [ ] **Step 1: Update `route_batch`** — after `parse_route_response` yields per-idx picks, map each through `select_model` instead of `apply_priority`:

```rust
Ok(text) => {
    let picks = parse_route_response(&text, &route_idxs, candidates);
    let t = crate::engine::routing_config::load().tradeoff;
    picks.into_iter().filter_map(|(idx, choice)| {
        let picked = crate::engine::providers::resolve_candidate(candidates, &choice.model)?;
        Some((idx, select_model(picked, candidates, t, PRIORITY_BETA)))
    }).collect()
}
```

- [ ] **Step 2: Update `route_via_native_claude`** (delegate.rs) the same way — replace its `router::apply_priority(parse_route_response(...), candidates)` tail with the `select_model` map above.

- [ ] **Step 3: Port the two `apply_priority` tests** (`priority_overrides_...`, `cost_tiebreaker_...`) to assert the new joint-score behavior, then delete `apply_priority`. Update `route_batch_*` tests' expectations for the joint score.

- [ ] **Step 4: Run tests** — `cargo test` → PASS. Fix any test that encoded old lexicographic expectations.

- [ ] **Step 5: Commit** — `git commit -am "refactor(router): route via select_model, remove apply_priority"`

## Task A5: `router_provider()` requires `enabled`

**Files:** Modify `server-rs/src/engine/model/router.rs` (`router_provider`)

- [ ] **Step 1: Write the failing test** — a flagged router with `enabled:false` is skipped:

```rust
#[test]
fn router_provider_skips_a_disabled_flagged_router() {
    let mut r = reg();
    r.providers[0].router = true;
    r.providers[0].enabled = false;               // disabled → not eligible as router
    assert!(router_provider(&r).is_none());
}
```
(Requires the `p(...)` test helper to set `enabled: true` by default — update it.)

- [ ] **Step 2: Run → FAIL** (disabled router still returned).

- [ ] **Step 3: Add `&& p.enabled`** to both `find` predicates in `router_provider` (the `p.router && … ` branch and the `AGENTIC_ROUTER_PROVIDER` branch).

- [ ] **Step 4: Run tests** → PASS.

- [ ] **Step 5: Commit** — `git commit -am "fix(router): a disabled provider cannot be the router"`

## Task A6: delegate.rs — enabled filter, empty-set guard, degradation

**Files:** Modify `server-rs/src/engine/workflow/delegate.rs` (`run_delegate`, candidate build); `native_claude_candidates` call site.

- [ ] **Step 1: Filter candidates by `enabled`** — in the `candidates` builder (delegate.rs ~905): add `&& p.enabled` to the registered-provider filter; filter `native_candidates` to `.filter(|p| p.enabled)`. (Native `enabled` comes from the family override; carry it in `candidates_from`.)

- [ ] **Step 2: Empty-set guard** — immediately after building `candidates`, before the `router_provider` match:

```rust
if candidates.is_empty() {
    self.clear_delegate_pending(caller_id);
    return Err("no models enabled for routing (enable at least one provider or native tier)".into());
}
```

- [ ] **Step 3: Router-fail degradation** — in the worker-spec build, when a task has no pick (router disabled/failed), run `select_model` with a synthetic mid-floor pick instead of dropping to the raw default: resolve the most-capable candidate ≤ `d=0.5`… — simplest: when `picks.get(&i)` is `None` and the task is un-pinned, call `select_model` against a synthetic `Provider{capability:0.5,..}` floor. Add a helper `fallback_pick(candidates) -> &Provider` = argmax score at `t=load().tradeoff`, `d=0.5`. Wire it into the `None =>` arm (replacing the `explicit.unwrap_or("")` native-default path for the *un-pinned* case; keep explicit-unknown → native override).

- [ ] **Step 4: Carry `enabled` through `candidates_from`** (providers.rs) — native candidates inherit `enabled` from the family override (default true), so Step 1's native filter works.

- [ ] **Step 5: Tests** — add a unit test for the candidate-filter helper (all-disabled → empty) and `fallback_pick`. Full `run_delegate` stays covered by existing engine tests. `cargo test` → PASS.

- [ ] **Step 6: Commit** — `git commit -am "feat(router): enabled candidate filter, empty-set guard, mid-floor degradation"`

## Task A7: API surface — enabled + tradeoff

**Files:** Modify `server-rs/src/api/` provider + native-override handlers; add routing route in the router/mod that registers axum routes.

- [ ] **Step 1:** Provider create/update + native-override payloads accept and persist `enabled` (serde default true so old clients keep working).
- [ ] **Step 2:** Add `GET /api/routing` → `{ "tradeoff": f32 }` and `POST /api/routing` (clamps to [0,1], persists via `routing_config::save`). Register in the axum router.
- [ ] **Step 3:** An API-level test (existing pattern) round-trips `enabled` and `tradeoff`.
- [ ] **Step 4: Commit** — `git commit -am "feat(api): expose enabled + global tradeoff"`

## Task A8: Phase-A verify + PR

- [ ] `cd server-rs && cargo fmt && cargo test` → all green; `cargo build --release` compiles.
- [ ] Delegate adversarial-verify pass on the diff (logic/regression, edge/error, security, tests) per repo CLAUDE.md; fix real findings.
- [ ] `gh pr create` targeting `master`; follow Codex-review auto-merge workflow.

---

# Phase B — Android (`agentic-dev-android`, separate PR, after Phase A merges)

## Task B1: Models + API wiring
- Modify `core/network/.../Models.kt`: add `enabled: Boolean = true` to `Provider`, `NativeFamily`, `NativeOverrideReq`; add `data class RoutingConfig(val tradeoff: Float = 0.5f)`.
- Modify `core/network/.../KtorAgenticApi.kt`: send `enabled`; add `getRouting()` / `setRouting(tradeoff)` hitting `/api/routing`.
- Test: a serialization test that a payload without `enabled` decodes to `true`.
- Commit.

## Task B2: Enabled toggle + grayed card
- Modify `feature/providers/.../ProvidersScreen.kt`: add an `Enabled` `Switch` to each provider card and each native-family card; when off, render the card with reduced alpha / muted container and an "off" affordance; persist via upsert. Keep the `router` toggle disabled when `enabled` is off.
- Commit.

## Task B3: Global tradeoff slider
- Add a `FloatSliderField(label = "路由取向 · 更便宜 ⇄ 更强")` at the top of the Providers/Settings screen bound to `RoutingConfig.tradeoff`, loaded on open, saved on change (debounced) via `setRouting`.
- Commit.

## Task B4: Priority slider direction label
- Change the priority slider label to `Scheduling priority (0 = 最不优先)` (mirror the cost slider's direction hint) in both the provider form and the native-family form.
- Commit.

## Task B5: Phase-B verify + PR
- `./gradlew testDebugUnitTest` (worktree compile check); delegate adversarial-verify on the diff; `gh pr create`; Codex-review auto-merge.

---

## Self-review notes
- **Spec coverage:** enabled (A1,A6,A7,B1,B2) · tradeoff (A2,A4,A7,B1,B3) · joint score (A3) · floor=capability(pick) (A3) · β=0.1 nudge bound (A3 test) · remove apply_priority (A4) · router_provider enabled (A5) · empty-set guard (A6) · degradation d=0.5 (A6) · reason string (A3) · priority label (B4). All §-items mapped.
- **Types:** `select_model(picked, candidates, t, beta) -> RouteChoice`, `PRIORITY_BETA=0.1`, `RoutingConfig{tradeoff}` used consistently across A3/A4/A6/A7.
- **Placeholders:** A6 Step 3 (`fallback_pick`) is the one spot needing implementer judgment on wiring into the existing `None =>` arm; the helper's contract (argmax score at d=0.5) is specified — refine against the real match arm during execution.
