# Native Claude Model Routing Overrides Design

Date: 2026-07-11

## 摘要（中文）

官方 Claude 模型（opus/sonnet/haiku/fable…）目前是后端从 Anthropic Models API **动态发现**的，
并作为 `delegate` 子任务分发的路由候选参与竞争，但它们的路由参数写死：`priority` 固定 0.5，
`capability`/`cost` 来自按家族硬编码的 `family_metrics()`，用户改不了；它们也不在 Android 的
"agent models" 管理界面里。

本设计让官方 Claude 模型进入该管理界面：在动态发现之上加一层**按家族的覆盖**（opus/sonnet/haiku/
fable/other），用户可调 capability/priority/cost + 描述；覆盖按家族保存，Anthropic 出新版模型时
自动继承同家族的设置。界面上新增一个**可折叠分区**「Claude Code 官方模型」，默认收起、点开展示每个
家族一张卡。模型列表继续动态从 API 拉，不硬编码；无覆盖时行为与现在完全一致。native 模型跑在订阅上、
没有独立带 key 的端点，**不参与 router 选择**。

## Summary

Native Claude models are discovered dynamically from the Anthropic Models API and already compete
as candidates in the `delegate` worker-routing decision. But their routing metrics are not
user-adjustable: `priority` is hardcoded to `0.5` and `capability`/`cost` come from the per-family
constants in `family_metrics()`. They are also absent from the Android "agent models" management
screen, which today only manages BYOK providers.

This change surfaces the native Claude models in that screen and makes their routing metrics
editable, by layering a **per-family override** on top of the dynamic discovery:

- Overrides are keyed by model **family** (`opus` / `sonnet` / `haiku` / `fable` / `other`), so a
  future model release in the same family (e.g. `claude-opus-4-9`) inherits the settings the user
  configured for that family.
- Each family override can set `capability`, `priority`, `cost`, and `description` — the same
  routing axes a BYOK provider exposes.
- Overrides persist in a new file; when no override exists the behavior is byte-for-byte identical
  to today (family defaults + `priority` 0.5).
- The Android app gets a **collapsible "Claude Code official models" section** (collapsed by
  default) listing one card per discovered family.

Native models run on the subscription with no dedicated keyed endpoint, so they cannot serve as the
LLM router — the native form therefore omits the `router`, `base_url`, `api_key`, and `protocol`
fields.

## Current behavior

Backend (`server-rs/src/engine/providers.rs`):

- `init_claude_models()` fetches the model list from `https://api.anthropic.com/v1/models` at
  startup (with backoff) into a `OnceLock`; `native_claude_models()` returns them (newest first).
- `family_metrics(id)` maps a model id to `(capability, cost)` by family substring:
  fable/mythos `(0.99, 1.0)`, opus `(0.97, 0.9)`, sonnet `(0.85, 0.5)`, haiku `(0.60, 0.3)`,
  else `(0.85, 0.6)`.
- `native_claude_candidates()` turns each discovered model into a `Provider` with
  `capability`/`cost` from `family_metrics()`, a hardcoded `priority: 0.5`, an empty `base_url`/key
  (so it runs on the subscription), and a generated `description`.
- `is_native(p)` = empty `base_url` AND the model id is one of the discovered models.

Routing (`server-rs/src/engine/router.rs`): the LLM-as-router picks a model per task, then
`apply_priority()` deterministically overrides that pick using the candidates' `capability`,
`priority`, and `cost` fields. This layer already reads exactly the fields we are making editable —
**it does not need to change.**

API (`server-rs/src/api/misc.rs`, routes in `server-rs/src/api/mod.rs` lines 95–97):

- `GET /api/models` → `full_model_entries()` (native tiers + BYOK providers);
  `GET /api/models?scope=session_start` → `native_model_entries()` (native only). Each `ModelEntry`
  carries `key/label/native/default/capability/cost` (no `priority`; it is a main-thread picker).
- `GET /api/providers`, `POST /api/providers`, `DELETE /api/providers/{name}` — BYOK CRUD.
  `POST` validates: NaN-rejects and clamps `capability/priority/cost` to `0..1`, requires
  `name/base_url/model`, and probes a `router:true` provider via `router::validate_router`.
- Provider persistence pattern in `providers.rs`: `providers_file_path()` (env
  `AGENTIC_PROVIDERS_FILE` else `~/.agentic-dev/providers.json`, plus a data-race-free test override
  static), `load_list_from`/`save_list_to` (atomic temp+rename, mode 0600), `upsert_at`/`remove_at`
  guarded by a `FILE_LOCK`, and "corrupt file errors rather than wiping."

Android (`agentic-dev-android`, `ui/providers/ProvidersScreen.kt`):

- `ModelsSections()` (embedded in `GlobalSettingsScreen`) renders `SectionCard`s "Router" and
  "Sub-agent models", a delete-confirm dialog, and the add/edit form revealed via
  `AnimatedVisibility`.
- `ProviderCard` shows an avatar, name, router badge, `model·protocol` subtitle, description, and
  three `MetricRow`s (capability/priority/cost, each a `LinearProgressIndicator` + `%.2f`).
- `AddOrEditForm` is the provider form (name/base_url/api_key/model/protocol/capability/priority/
  cost/router/description) with preset `FilterChip`s and a `ProtocolSelector` (`ButtonGroup`).

## Requirements

1. Native Claude models appear in the "agent models" management screen, grouped by family, listing
   the concrete discovered model id(s) under each family.
2. Each family's `capability`, `priority`, `cost`, and `description` are editable and affect
   `delegate` worker routing.
3. Overrides are keyed by family so a new model release inherits its family's settings.
4. Overrides persist across restarts and are reset-to-default-able.
5. The native model list stays dynamic (from the Models API); nothing is hardcoded. When discovery
   is empty, the section shows nothing (no fabricated families).
6. With no override present, routing and the model catalog behave exactly as today (back-compat).
7. The native section is a collapsible sub-panel, collapsed by default.
8. Native models are not offered as the router and do not gain key/endpoint/protocol fields.
9. The session main-model picker (`/api/models`) is left **unchanged**. Overrides affect only the
   delegate routing candidates and the new `/api/native-models` view — not the main-model picker's
   indicators or ordering. This decouples the two surfaces and shrinks the change's blast radius.

## Backend design

### Family classification (single source of truth)

Add to `providers.rs`, and route `family_metrics` through it so classification lives in one place:

```rust
/// Family bucket for a Claude model id. Mirrors the tiers family_metrics recognizes.
pub(crate) fn family_of(id: &str) -> &'static str {
    if id.contains("fable") || id.contains("mythos") { "fable" }
    else if id.contains("opus") { "opus" }
    else if id.contains("sonnet") { "sonnet" }
    else if id.contains("haiku") { "haiku" }
    else { "other" }
}

/// Default (capability, cost) for a family — the values family_metrics returned before.
pub(crate) fn family_default_metrics(family: &str) -> (f32, f32) {
    match family {
        "fable" => (0.99, 1.0),
        "opus"  => (0.97, 0.9),
        "sonnet"=> (0.85, 0.5),
        "haiku" => (0.60, 0.3),
        _       => (0.85, 0.6),
    }
}

pub(crate) fn family_metrics(id: &str) -> (f32, f32) {
    family_default_metrics(family_of(id))
}
```

`DEFAULT_NATIVE_PRIORITY: f32 = 0.5` names the current hardcoded priority.

`family_of` always returns lowercase, so every key, API path, and lookup is lowercase (the handlers
must normalize the `{family}` path param — see API layer).

**Editable taxonomy = `{opus, sonnet, haiku, fable}`.** `other` is a deliberate read-only
catch-all: it collects any id matching none of the known families, so a single editable override
there would silently apply to structurally-unrelated future models (e.g. a new `claude-nova-*`
inheriting a months-old `other` setting). `GET` still lists an `other` group so discovered unknown
models are visible, but with `customized:false` and default metrics; `POST`/`DELETE` for `other`
return `400`. `family_label(family)` maps to "Opus"/"Sonnet"/"Haiku"/"Fable"/"Other".

Known limitations (both pre-existing, called out so they are not surprises): `fable` and `mythos`
share one bucket/key because `family_metrics` already merges them; and, by the durability the user
chose, a same-family new release inherits the family override even if its true tier shifts — re-tune
if a future model changes tier materially.

### Persistence — `engine/native_overrides.rs` (new module)

A small module mirroring the providers-file pattern exactly (atomic write, 0600, `FILE_LOCK`,
corrupt-file-errors-not-wipes, test-override static).

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeOverride {
    pub capability: f32,
    pub priority: f32,
    pub cost: f32,
    #[serde(default)]
    pub description: String,
}
```

Stored as a JSON object keyed by family (a `BTreeMap<String, NativeOverride>` for deterministic
output):

```json
{
  "opus":  { "capability": 0.97, "priority": 0.80, "cost": 0.90, "description": "only the hardest tasks" },
  "haiku": { "capability": 0.60, "priority": 0.20, "cost": 0.30, "description": "" }
}
```

Path: env `AGENTIC_NATIVE_OVERRIDES_FILE`, else `~/.agentic-dev/native-overrides.json`, plus a
`NATIVE_OVERRIDES_FILE_OVERRIDE` `Mutex<Option<PathBuf>>` for tests. Functions:
`load_map_from(&Path) -> io::Result<BTreeMap<..>>` (`Ok(empty)` when missing, `Err` when present but
unparseable — callers must not overwrite), `save_map_to`, `upsert_at(path, family, NativeOverride)`,
`remove_at(path, family) -> bool`, and the convenience wrappers `load_map()`/`upsert()`/`remove()`.
This module is pure persistence — no axum.

### Applying overrides in routing

The overrides map is **injected as a parameter, never read inside the helper** — this keeps
`native_claude_candidates` hermetic. Its only production caller is `delegate.rs` (`run_delegate`),
which loads the map at the call boundary; the four existing test callers (`providers.rs:653/693`,
`router.rs:453/620`) pass an empty map, so none of them touch the real
`~/.agentic-dev/native-overrides.json` or race a concurrent `set_var` (the UB the provider file
override static was written to avoid):

```rust
pub fn native_claude_candidates(overrides: &BTreeMap<String, NativeOverride>) -> Vec<Provider> {
    // per discovered model m:
    let fam = family_of(&m.id);
    let (cap, cost, prio, desc) = match overrides.get(fam) {
        Some(o) => (o.capability, o.cost, o.priority,
                    if o.description.is_empty() { default_desc(m) } else { o.description.clone() }),
        None => { let (c, k) = family_default_metrics(fam); (c, k, DEFAULT_NATIVE_PRIORITY, default_desc(m)) }
    };
    // ... build the Provider as today ...
}
```

Production call site (`delegate.rs`):
`let ov = native_overrides::load_map(); let native = native_claude_candidates(&ov);`
(Reading the small file once per delegate batch matches the existing `load_list()` cost.)

`router.rs` is otherwise untouched: it already reads `capability`/`priority`/`cost` off each
candidate, and `apply_priority` makes a raised family `priority` authoritative among
capable-enough candidates — the whole point of the feature.

**Open decision (see "Open decisions" below):** whether a family should be a SINGLE routing
candidate (newest model only) or keep every discovered sibling as its own candidate as today. This
governs whether a raised family priority can be undercut by the router LLM naming an older sibling
that now carries identical family metrics.

### API layer (`api/misc.rs`, routes in `api/mod.rs`)

New serialized views:

```rust
#[derive(Serialize)] struct NativeModelRef { id: String, display_name: String }
#[derive(Serialize)] struct NativeFamilyView {
    family: String,            // "opus"
    label: String,             // "Opus"
    models: Vec<NativeModelRef>,   // discovered ids in this family (may be >1)
    capability: f32, priority: f32, cost: f32, description: String,  // EFFECTIVE values
    customized: bool,          // an override row exists for this family
    editable: bool,            // false for the `other` catch-all; the client hides edit/reset
}
```

Routes:

- `GET /api/native-models` → group `native_claude_models()` by `family_of`; for each **non-empty**
  family emit a `NativeFamilyView` with effective params, `customized`, and `editable` (false only
  for `other`). Sorted cheap→capable by effective `capability` (family name tiebreak), matching the
  `/api/models` ordering contract. Empty when discovery failed.
- `POST /api/native-models/{family}` → body `{capability, priority, cost, description?}`
  (snake_case). Handler **first normalizes `family = family.trim().to_lowercase()`** — the key
  `family_of` produces is always lowercase, so storing a raw-cased path param (`"Opus"`) would make
  `overrides.get(fam)` never match at apply time: a silently-dead override that `POST` still `200`s
  and `GET` still reports `customized:false`. Then validate `family` ∈ **editable taxonomy**
  `{opus, sonnet, haiku, fable}` (else `400`; `other` is rejected — see Taxonomy); NaN-reject then
  clamp each metric to `0..1` (same as `providers_post`); upsert. `{ "ok": true }`.
- `DELETE /api/native-models/{family}` → same lowercase-normalize + editable-taxonomy validation
  (else `400`, so it matches `POST` and this file's `providers_delete` validation habit rather than
  silently `200`-ing a typo'd family), then remove the override. Idempotent: `{ "ok": true }`
  whether or not a row existed (the UI only offers reset when `customized`).

Registration — **inside the auth-gated `compressed` router block** in `mod.rs` (the block wrapped by
`middleware::from_fn_with_state(state, auth_gate)`, alongside `/api/providers`), so the routes
inherit token auth like every other `/api/*` route:

```rust
.route("/api/native-models", get(misc::native_models_get))
.route("/api/native-models/{family}", post(misc::native_models_post).delete(misc::native_models_delete))
```

`native_model_entries()` and `/api/models` are **left unchanged** (Requirement 9) — overrides do not
flow into the main-model picker. Overrides are read only in the routing candidate builder and in
`native_models_get`.

**Description semantics (two audiences, one stored field).** The stored `override.description` is a
single family-level string the user edits. It is consumed two ways:

- `GET /api/native-models` returns it verbatim (empty string when the family is uncustomized or the
  user cleared it) — this is what the edit form shows and edits.
- `native_claude_candidates()` (routing) uses `override.description` when non-empty, but falls back
  to the generated per-model string (`"Anthropic {display_name} — native (subscription)"`) when it
  is empty, so the router prompt's `good_at` line is never blank.

## Android design

### Networking + DTOs

Add to the Ktor API client:

```kotlin
suspend fun nativeModels(): List<NativeFamily>
suspend fun putNativeOverride(family: String, req: NativeOverrideReq)   // POST /api/native-models/{family}
suspend fun deleteNativeOverride(family: String)                        // DELETE
```

The app's `Json` config has no `SnakeCase` naming strategy, so every snake_case wire field is
mapped with `@SerialName` on a camelCase property (as `Provider.baseUrl`/`hasKey` already do). Only
`display_name` is multi-word here:

```kotlin
@Serializable data class NativeModelRef(val id: String, @SerialName("display_name") val displayName: String)
@Serializable data class NativeFamily(
    val family: String, val label: String, val models: List<NativeModelRef>,
    val capability: Float, val priority: Float, val cost: Float,
    val description: String, val customized: Boolean, val editable: Boolean,
)
@Serializable data class NativeOverrideReq(
    val capability: Float, val priority: Float, val cost: Float, val description: String,
)
```

### UI — collapsible native section in `ModelsSections`

- After the "Sub-agent models" `SectionCard`, add a `SectionCard` titled **"Claude Code official
  models"** whose header row carries an expand/collapse chevron toggling a
  `rememberSaveable { mutableStateOf(false) }` (collapsed by default). The body is wrapped in
  `AnimatedVisibility`, reusing the existing reveal pattern.
- `ProviderCard` and `MetricRow` are `private` in `ProvidersScreen.kt`, and `ProviderCard` takes a
  `Provider` (reads name/base_url/has_key/router), so they cannot be reused directly. First **hoist
  `MetricRow`** (and the metric-slider sub-composable) into a shared `internal` component, then add a
  **new `NativeFamilyCard`**: avatar/title (`label`) + the discovered `models` ids as subtitle
  (e.g. `claude-opus-4-8`) + three hoisted `MetricRow`s + a "Customized" badge when `customized`.
- `NativeOverrideForm` (new, slim): three sliders (capability/priority/cost) prefilled from the
  family's effective values + a description `AppTextField`, a **Save** button (`putNativeOverride`)
  and a **Reset to default** button (`deleteNativeOverride`). It omits
  name/base_url/api_key/protocol/router. The client keys off `NativeFamily.editable` (not a
  hardcoded family list): `editable=false` cards (the `other` catch-all) render read-only — ids +
  default metrics, no edit/reset.
- The ViewModel backing `ModelsSections` gains native-families state: load via `nativeModels()` on
  init and after each save/reset; save/reset call the new API methods and reload.

The Android app is the only client; the new routes are additive and do not change any existing
endpoint, so API stability for older app builds is preserved.

## Testing plan

### Backend engine tests (`engine/native_overrides.rs` + `engine/providers.rs`)

1. `family_of` classifies opus/sonnet/haiku/fable/mythos/unknown correctly; `family_metrics`
   still returns the same tuples it did before the refactor.
2. Overrides CRUD roundtrip on a temp file (upsert, replace-by-family, remove, missing→false).
3. Corrupt overrides file **errors** on read/upsert/remove rather than wiping (mirrors the existing
   provider test).
4. `native_claude_candidates(&overrides)` reflects an override: seed discovered models, pass an
   `opus` override with a raised `priority`/changed `capability`/`cost`, assert the opus candidate
   carries them and non-overridden families keep defaults + `priority` 0.5.
5. **Inheritance across releases:** seed `claude-opus-4-9` (a hypothetical new id), pass the `opus`
   family override, assert the new model's candidate inherits it — the durability guarantee.
6. **Hermeticity:** `native_claude_candidates(&BTreeMap::new())` reproduces today's defaults
   verbatim; the existing candidate/resolve tests are updated to pass an empty map and therefore
   never read the real overrides file. (Guards against the "helper silently reads `$HOME`" regression
   and the `set_var` race.)

### Backend API tests (`api/misc.rs`)

7. `GET /api/native-models` groups discovered models by family with `customized:false` initially;
   after `POST`, the family shows the new effective values and `customized:true`; an `other` group
   (if any) is present but `customized:false`.
8. `POST /api/native-models/{family}` clamps out-of-range metrics and rejects NaN; a mixed-case
   family (`Opus`) normalizes and applies; an invalid family and `other` both return `400`.
9. `DELETE /api/native-models/{family}` resets to defaults (subsequent `GET` shows defaults,
   `customized:false`), is idempotent for a valid-but-absent family, and `400`s an invalid/`other`
   family.

### Android

Rely on the release APK build in the main checkout plus code review, per repo guidance
(no test scaffolding assumed in the worktree). Keep the new composables small and pure where
possible.

### Invariants

- `engine/` stays axum-free; `native_overrides.rs` is pure persistence.
- `make build` (backend) and the Android compile must pass before commit; `make test` should be
  green for the backend.

## Open decisions

**D1 — Is a family ONE routing candidate, or one per discovered sibling?**
Today `native_claude_candidates()` emits every discovered model as its own candidate ("no tier
bucketing, no latest-per-family cap" — an explicit current invariant, asserted at
`providers.rs:653`). Once family keying gives all siblings in a family identical metrics, if the
router LLM names an older sibling (`claude-opus-4-7` while `-4-8` exists), `apply_priority` cannot
upgrade it (metrics are equal), so an older model can run despite a raised family priority.

- **Option A (recommended): collapse to newest-per-family for ROUTING candidates.** A family becomes
  one routing unit — consistent with family keying, shrinks the router prompt, removes the
  older-sibling ambiguity. Cost: reverses the current invariant and updates the
  `native_claude_candidates_compete_as_routing_candidates` test; an explicit `model` pin to an older
  native id would no longer resolve to a native candidate (falls back to the main model — negligible,
  same subscription/tier).
- **Option B: keep every sibling a candidate (status quo).** Smaller change, preserves the invariant;
  accepts that the router may occasionally pick an older same-tier sibling (functionally equivalent —
  same subscription, same cost, marginally older quality).

This only affects the routing candidate list, never the picker or the `/api/native-models` view.

**D2 — pre-existing `apply_priority` EPS asymmetry.** A reviewer flagged that `apply_priority`'s
first-swap branch uses EPS-thresholded comparisons while later swaps use strict `>`/`<`, which can
be order-sensitive under near-equal priorities. It predates this feature and is out of scope; Option
A (well-separated cross-family metrics, one candidate per family) sidesteps it in practice. Noted so
it is not forgotten.

## Out of scope

- Letting native Claude models serve as the LLM router (they run on the subscription with no keyed
  endpoint; `router::validate_router` requires a keyed anthropic provider).
- Per-exact-model-id overrides (we chose family keying for durability).
- Any change to BYOK provider CRUD, presets, or the routing algorithm in `router.rs`.
- The session main-model picker (`/api/models`) entirely — its shape, ordering, `default` flag, and
  `capability`/`cost` indicators are unchanged; overrides do not reach it.
- Editing families that have no discovered models (the section only lists discovered families), and
  editing the `other` catch-all (read-only).
