# Native Claude Model Routing Overrides Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users adjust the delegate-routing metrics (capability/priority/cost/description) of the native Claude models, per model family, from the Android "agent models" screen — with the model list still discovered dynamically from the Anthropic Models API.

**Architecture:** A thin per-family override layer sits on top of the existing dynamic discovery. The backend persists overrides in a new file keyed by family (`opus`/`sonnet`/`haiku`/`fable`), injects them into the routing candidate builder (which now emits one candidate per family — the newest), and exposes them through new `/api/native-models` CRUD routes. The Android app gains a collapsible "Claude Code official models" section listing one card per discovered family with an edit dialog. `/api/models` (the session main-model picker) is untouched.

**Tech Stack:** Rust (axum, serde, parking_lot) for `agentic-dev/server-rs`; Kotlin + Jetpack Compose (Material 3 Expressive, Ktor client, kotlinx.serialization) for `agentic-dev-android`.

## Global Constraints

- Spec: `agentic-dev/docs/superpowers/specs/2026-07-11-native-claude-model-routing-overrides-design.md`. Every task implicitly inherits it.
- Backend `engine/` stays free of `axum` imports (unit-testable in isolation).
- Backend tests never hit the network or the real `~/.agentic-dev/*`: use the data-race-free test-override statics (`PROVIDERS_FILE_OVERRIDE`, new `NATIVE_OVERRIDES_FILE_OVERRIDE`), never `env::set_var`.
- Editable families: `opus`, `sonnet`, `haiku`, `fable`. `other` is a read-only catch-all — `POST`/`DELETE` for it return `400`.
- Routing metrics are `f32` in `0.0..=1.0`: NaN-reject then clamp (mirror `providers_post`).
- Native models never serve as the router; the native form has no name/base_url/api_key/protocol/router fields.
- One PR per repo (backend first, then Android). Each repo auto-merges after Codex review per its `CLAUDE.md`; the hard floor is that code compiles.
- Android JSON: no snake_case naming strategy — snake_case wire fields use `@SerialName` on camelCase properties.

---

# Phase A — Backend (`agentic-dev/server-rs`, one PR)

Run all backend commands from `agentic-dev/server-rs`. Test a single test with `cargo test <path>::<name>`; full suite with `cargo test` (or `make test` from the repo root).

## Task A1: Family classifier (single source of truth)

**Files:**
- Modify: `server-rs/src/engine/providers.rs` (replace `family_metrics` around lines 396–411; add `family_of`, `family_default_metrics`, `DEFAULT_NATIVE_PRIORITY`)
- Test: `server-rs/src/engine/providers.rs` (in-crate `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub(crate) fn family_of(id: &str) -> &'static str`; `pub(crate) fn family_default_metrics(family: &str) -> (f32, f32)`; `pub(crate) const DEFAULT_NATIVE_PRIORITY: f32`; `pub(crate) fn family_metrics(id: &str) -> (f32, f32)` (kept, now derived).

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `providers.rs`:

```rust
#[test]
fn family_of_classifies_and_metrics_are_unchanged() {
    assert_eq!(family_of("claude-opus-4-8"), "opus");
    assert_eq!(family_of("claude-sonnet-4-6"), "sonnet");
    assert_eq!(family_of("claude-haiku-4-5-20251001"), "haiku");
    assert_eq!(family_of("claude-fable-5"), "fable");
    assert_eq!(family_of("claude-mythos-1"), "fable");
    assert_eq!(family_of("claude-3-5-something"), "other");
    // family_metrics must return exactly what it returned before the refactor
    assert_eq!(family_metrics("claude-opus-4-8"), (0.97, 0.9));
    assert_eq!(family_metrics("claude-fable-5"), (0.99, 1.0));
    assert_eq!(family_metrics("claude-sonnet-4-6"), (0.85, 0.5));
    assert_eq!(family_metrics("claude-haiku-4-5"), (0.60, 0.3));
    assert_eq!(family_metrics("claude-weird-9"), (0.85, 0.6));
    assert_eq!(DEFAULT_NATIVE_PRIORITY, 0.5);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib engine::providers::tests::family_of_classifies_and_metrics_are_unchanged`
Expected: FAIL to compile — `family_of` / `DEFAULT_NATIVE_PRIORITY` not found.

- [ ] **Step 3: Write minimal implementation**

Replace the existing `family_metrics` function (currently lines ~396–411) with:

```rust
/// Default scheduling priority for a native Claude candidate with no family override.
pub(crate) const DEFAULT_NATIVE_PRIORITY: f32 = 0.5;

/// Family bucket for a Claude model id — the single source of truth for classification.
pub(crate) fn family_of(id: &str) -> &'static str {
    if id.contains("fable") || id.contains("mythos") {
        "fable"
    } else if id.contains("opus") {
        "opus"
    } else if id.contains("sonnet") {
        "sonnet"
    } else if id.contains("haiku") {
        "haiku"
    } else {
        "other"
    }
}

/// Default (capability, cost) for a family — the values `family_metrics` returned before.
pub(crate) fn family_default_metrics(family: &str) -> (f32, f32) {
    match family {
        "fable" => (0.99, 1.0),
        "opus" => (0.97, 0.9),
        "sonnet" => (0.85, 0.5),
        "haiku" => (0.60, 0.3),
        _ => (0.85, 0.6),
    }
}

/// Rough routing metrics `(capability, cost)` per model id. Kept for `native_model_entries`
/// (the `/api/models` picker); now derived from the family classifier.
pub(crate) fn family_metrics(id: &str) -> (f32, f32) {
    family_default_metrics(family_of(id))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib engine::providers::tests::family_of_classifies_and_metrics_are_unchanged`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/providers.rs
git commit -m "refactor(providers): family_of + family_default_metrics as the classifier source of truth"
```

## Task A2: `native_overrides` persistence module

**Files:**
- Create: `server-rs/src/engine/native_overrides.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `pub mod native_overrides;` next to `pub mod providers;`, line ~3274)
- Test: `server-rs/src/engine/native_overrides.rs` (in-crate tests)

**Interfaces:**
- Produces: `pub struct NativeOverride { pub capability: f32, pub priority: f32, pub cost: f32, pub description: String }`; `pub type OverrideMap = BTreeMap<String, NativeOverride>`; `pub fn native_overrides_file_path() -> PathBuf`; `pub static NATIVE_OVERRIDES_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>>`; `pub fn load_map_from(&Path) -> io::Result<OverrideMap>`; `pub fn save_map_to`, `pub fn upsert_at(&Path, &str, NativeOverride)`, `pub fn remove_at(&Path, &str) -> io::Result<bool>`; convenience `pub fn load_map() -> OverrideMap`, `pub fn upsert(&str, NativeOverride)`, `pub fn remove(&str) -> io::Result<bool>`.

- [ ] **Step 1: Create the module file with implementation**

Create `server-rs/src/engine/native_overrides.rs`:

```rust
//! Per-family routing overrides for the native Claude models. The model LIST stays dynamic
//! (Anthropic Models API discovery); only the routing metrics per family are persisted here.
//! Mirrors the providers-file CRUD pattern: atomic write, 0600, FILE_LOCK,
//! corrupt-file-errors-rather-than-wiping, and a data-race-free test-override static.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One family's override. Full values (the edit form writes all four); "reset to default" removes
/// the whole row. An empty `description` falls back to the generated per-model description at
/// routing time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeOverride {
    pub capability: f32,
    pub priority: f32,
    pub cost: f32,
    #[serde(default)]
    pub description: String,
}

/// family -> override. `BTreeMap` for deterministic on-disk ordering.
pub type OverrideMap = BTreeMap<String, NativeOverride>;

/// Serializes read-modify-write across concurrent API requests (axum multi-threaded executor).
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Test-only override for [native_overrides_file_path]. A plain static (not `env::set_var`, which
/// races concurrent getenv from other test threads — UB in glibc). Always `None` in production.
pub static NATIVE_OVERRIDES_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// The overrides JSON file: the test override if set, else `AGENTIC_NATIVE_OVERRIDES_FILE`, else
/// `~/.agentic-dev/native-overrides.json`.
pub fn native_overrides_file_path() -> PathBuf {
    if let Some(p) = NATIVE_OVERRIDES_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_NATIVE_OVERRIDES_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agentic-dev").join("native-overrides.json")
}

/// Read the map from `path`. `Ok(empty)` when missing; `Err` when present but unreadable or invalid
/// JSON — callers MUST NOT then overwrite it (that would wipe valid data).
pub fn load_map_from(path: &Path) -> std::io::Result<OverrideMap> {
    if !path.exists() {
        return Ok(OverrideMap::new());
    }
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write the map to `path` atomically (temp + rename); mode 0600 on unix.
pub fn save_map_to(path: &Path, map: &OverrideMap) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(map).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Insert or replace one family's override in the file at `path`.
pub fn upsert_at(path: &Path, family: &str, ov: NativeOverride) -> std::io::Result<()> {
    let _guard = FILE_LOCK.lock();
    let mut map = load_map_from(path)?;
    map.insert(family.to_string(), ov);
    save_map_to(path, &map)
}

/// Remove one family's override. Returns true if one was present.
pub fn remove_at(path: &Path, family: &str) -> std::io::Result<bool> {
    let _guard = FILE_LOCK.lock();
    let mut map = load_map_from(path)?;
    let removed = map.remove(family).is_some();
    if removed {
        save_map_to(path, &map)?;
    }
    Ok(removed)
}

// Convenience wrappers on the configured file.
pub fn load_map() -> OverrideMap {
    load_map_from(&native_overrides_file_path()).unwrap_or_default()
}
pub fn upsert(family: &str, ov: NativeOverride) -> std::io::Result<()> {
    upsert_at(&native_overrides_file_path(), family, ov)
}
pub fn remove(family: &str) -> std::io::Result<bool> {
    remove_at(&native_overrides_file_path(), family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(c: f32, p: f32, k: f32, d: &str) -> NativeOverride {
        NativeOverride { capability: c, priority: p, cost: k, description: d.into() }
    }

    #[test]
    fn crud_roundtrip_on_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("native-overrides.json");
        assert!(load_map_from(&f).unwrap().is_empty());
        upsert_at(&f, "opus", ov(0.9, 0.8, 0.9, "hard only")).unwrap();
        upsert_at(&f, "haiku", ov(0.6, 0.2, 0.3, "")).unwrap();
        let m = load_map_from(&f).unwrap();
        assert_eq!(m.len(), 2);
        assert!((m["opus"].priority - 0.8).abs() < f32::EPSILON);
        // replace by same family key
        upsert_at(&f, "opus", ov(0.97, 0.5, 0.9, "x")).unwrap();
        assert!((load_map_from(&f).unwrap()["opus"].priority - 0.5).abs() < f32::EPSILON);
        // remove (idempotent second call → false)
        assert!(remove_at(&f, "opus").unwrap());
        assert!(!remove_at(&f, "opus").unwrap());
        assert_eq!(load_map_from(&f).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_file_errors_instead_of_wiping() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("native-overrides.json");
        upsert_at(&f, "opus", ov(0.9, 0.8, 0.9, "")).unwrap();
        std::fs::write(&f, "{ this is not json").unwrap();
        assert!(load_map_from(&f).is_err());
        assert!(remove_at(&f, "opus").is_err());
        assert!(upsert_at(&f, "sonnet", ov(0.8, 0.5, 0.5, "")).is_err());
        // the corrupt file is left intact, NOT wiped
        assert!(std::fs::read_to_string(&f).unwrap().contains("not json"));
    }
}
```

- [ ] **Step 2: Register the module**

In `server-rs/src/engine/mod.rs`, add next to the other `pub mod` lines (near `pub mod providers;`):

```rust
pub mod native_overrides;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --lib engine::native_overrides`
Expected: PASS (both tests).

- [ ] **Step 4: Commit**

```bash
git add server-rs/src/engine/native_overrides.rs server-rs/src/engine/mod.rs
git commit -m "feat(native-overrides): file-backed per-family override persistence"
```

## Task A3: Inject overrides into `native_claude_candidates` + collapse to newest-per-family

**Files:**
- Modify: `server-rs/src/engine/providers.rs` (`native_claude_candidates`, ~lines 416–439; add private `candidates_from`; add `use` for the override types)
- Modify: `server-rs/src/engine/delegate.rs` (call site ~line 796)
- Modify: `server-rs/src/engine/providers.rs` tests (call sites ~653, ~693; the `len()==5` assertion ~655)
- Modify: `server-rs/src/engine/router.rs` tests (call sites ~453, ~620)

**Interfaces:**
- Consumes: `OverrideMap`, `NativeOverride`, `family_of`, `family_default_metrics`, `DEFAULT_NATIVE_PRIORITY`.
- Produces: `pub fn native_claude_candidates(overrides: &OverrideMap) -> Vec<Provider>` (SIGNATURE CHANGED — now takes the map); private `fn candidates_from(models: &[ClaudeModel], overrides: &OverrideMap) -> Vec<Provider>`.

- [ ] **Step 1: Write the failing test**

Add to `providers.rs` tests (uses the private `candidates_from`, so it is fully hermetic — no `OnceLock`, no file):

```rust
#[test]
fn candidates_collapse_to_newest_and_inherit_family_override() {
    // `OverrideMap` is already in scope via `use super::*` (module-level import from Step 3);
    // only `NativeOverride` needs importing here.
    use crate::engine::native_overrides::NativeOverride;
    // newest-first, two opus siblings + one haiku
    let models = vec![
        ClaudeModel { id: "claude-opus-4-9".into(), display_name: "Claude Opus 4.9".into() },
        ClaudeModel { id: "claude-opus-4-8".into(), display_name: "Claude Opus 4.8".into() },
        ClaudeModel { id: "claude-haiku-5".into(), display_name: "Claude Haiku 5".into() },
    ];
    let mut ov = OverrideMap::new();
    ov.insert("opus".into(), NativeOverride { capability: 0.9, priority: 0.85, cost: 0.2, description: String::new() });

    let c = candidates_from(&models, &ov);
    // opus collapses to the NEWEST (4-9); haiku stays → 2 candidates
    assert_eq!(c.len(), 2);
    assert!(c.iter().any(|p| p.model == "claude-opus-4-9"));
    assert!(!c.iter().any(|p| p.model == "claude-opus-4-8"));
    // the opus family override is inherited by the NEW id (family keying)
    let opus = c.iter().find(|p| p.model == "claude-opus-4-9").unwrap();
    assert!((opus.priority - 0.85).abs() < f32::EPSILON);
    assert!((opus.capability - 0.9).abs() < f32::EPSILON);
    assert!((opus.cost - 0.2).abs() < f32::EPSILON);
    // empty override description → generated per-model description
    assert_eq!(opus.description.as_deref(), Some("Anthropic Claude Opus 4.9 — native (subscription)"));
    // non-overridden family keeps defaults + priority 0.5
    let haiku = c.iter().find(|p| p.model == "claude-haiku-5").unwrap();
    assert!((haiku.priority - DEFAULT_NATIVE_PRIORITY).abs() < f32::EPSILON);
    assert_eq!((haiku.capability, haiku.cost), family_default_metrics("haiku"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib engine::providers::tests::candidates_collapse_to_newest_and_inherit_family_override`
Expected: FAIL to compile — `candidates_from` not found / `native_claude_candidates` arity.

- [ ] **Step 3: Rewrite `native_claude_candidates`**

Add near the top of `providers.rs` (with the other `use` lines):

```rust
use crate::engine::native_overrides::OverrideMap;
```

Replace `native_claude_candidates` (lines ~416–439) with:

```rust
/// Native Claude candidates for delegate routing: ONE per family (the newest discovered model),
/// with per-family override metrics layered on top of the family defaults. Injecting `overrides`
/// (rather than reading the file here) keeps this hermetic — the only production caller
/// (`delegate.rs`) loads the map at the call boundary; tests pass an empty map.
pub fn native_claude_candidates(overrides: &OverrideMap) -> Vec<Provider> {
    candidates_from(native_claude_models(), overrides)
}

/// Pure core of [native_claude_candidates] — takes the model list explicitly so tests need no
/// global `OnceLock` or file.
fn candidates_from(models: &[ClaudeModel], overrides: &OverrideMap) -> Vec<Provider> {
    let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in models {
        let fam = family_of(&m.id);
        // One routing candidate per family: keep the newest (models are newest-first).
        if !seen.insert(fam) {
            continue;
        }
        let default_desc = format!("Anthropic {} — native (subscription)", m.display_name);
        let (capability, priority, cost, description) = match overrides.get(fam) {
            Some(o) => (
                o.capability,
                o.priority,
                o.cost,
                if o.description.is_empty() { default_desc } else { o.description.clone() },
            ),
            None => {
                let (c, k) = family_default_metrics(fam);
                (c, DEFAULT_NATIVE_PRIORITY, k, default_desc)
            }
        };
        out.push(Provider {
            name: m.id.clone(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: None,
            model: m.id.clone(),
            protocol: Protocol::Anthropic,
            capability,
            description: Some(description),
            priority,
            cost,
            router: false,
        });
    }
    out
}
```

- [ ] **Step 4: Update the production call site in `delegate.rs`**

Replace (line ~796):

```rust
        let native_candidates = crate::engine::providers::native_claude_candidates();
```

with:

```rust
        let native_overrides = crate::engine::native_overrides::load_map();
        let native_candidates = crate::engine::providers::native_claude_candidates(&native_overrides);
```

- [ ] **Step 5: Update existing test call sites (empty map) + the collapse assertion**

In `providers.rs` tests:
- Line ~653: `let c = native_claude_candidates();` → `let c = native_claude_candidates(&Default::default());`
- Line ~655: `assert_eq!(c.len(), 5);` → `assert_eq!(c.len(), 4);` and update the surrounding comment to "one candidate per family (newest): fable, opus, sonnet, haiku". Keep the existing `any(model == "claude-opus-4-8")` (newest opus kept) and `any(model == "claude-fable-5")` assertions; add `assert!(!c.iter().any(|p| p.model == "claude-opus-4-7"));`.
- Line ~693: `let native = native_claude_candidates();` → `let native = native_claude_candidates(&Default::default());`

In `router.rs` tests:
- Line ~453: `cat.extend(crate::engine::providers::native_claude_candidates());` → `cat.extend(crate::engine::providers::native_claude_candidates(&Default::default()));`
- Line ~620: same change.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib engine::providers engine::router engine::delegate`
Expected: PASS (new test + updated existing tests + delegate compiles).

- [ ] **Step 7: Commit**

```bash
git add server-rs/src/engine/providers.rs server-rs/src/engine/delegate.rs server-rs/src/engine/router.rs
git commit -m "feat(routing): per-family native overrides + one candidate per family (D1)"
```

## Task A4: `/api/native-models` CRUD

**Files:**
- Modify: `server-rs/src/api/misc.rs` (add view/req structs, `family_label`, `is_editable_family`, `native_models_get/post/delete`)
- Modify: `server-rs/src/api/mod.rs` (register two routes inside the auth-gated `compressed` block, next to `/api/providers`, ~line 97)
- Test: `server-rs/src/api/misc.rs` tests

**Interfaces:**
- Consumes: `crate::engine::native_overrides::{load_map, upsert, remove, NativeOverride}`; `crate::engine::providers::{family_of, family_default_metrics, native_claude_models, DEFAULT_NATIVE_PRIORITY}`.
- Produces: `pub async fn native_models_get() -> Response`; `pub async fn native_models_post(Path<String>, Bytes) -> Response`; `pub async fn native_models_delete(Path<String>) -> Response`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `misc.rs` (mirrors the existing `oneshot_req` + `isolated_providers_file` patterns):

```rust
struct NativeOvGuard {
    _dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl Drop for NativeOvGuard {
    fn drop(&mut self) {
        *crate::engine::native_overrides::NATIVE_OVERRIDES_FILE_OVERRIDE.lock() = None;
    }
}
fn isolated_native_overrides_file() -> NativeOvGuard {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    *crate::engine::native_overrides::NATIVE_OVERRIDES_FILE_OVERRIDE.lock() =
        Some(dir.path().join("native-overrides.json"));
    NativeOvGuard { _dir: dir, _lock: lock }
}

#[tokio::test]
async fn native_models_get_groups_then_post_marks_customized() {
    let _ov = isolated_native_overrides_file();
    let st = test_state().await;
    crate::engine::providers::seed_claude_models_for_tests();

    // GET: families present, opus editable and not customized
    let (s, b) = oneshot_req(
        st.clone(),
        Request::get("/api/native-models")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let fams = b["families"].as_array().unwrap();
    let opus = fams.iter().find(|f| f["family"] == "opus").unwrap();
    assert_eq!(opus["editable"], true);
    assert_eq!(opus["customized"], false);

    // POST with mixed-case family normalizes and applies
    let (s2, _) = oneshot_req(
        st.clone(),
        Request::post("/api/native-models/Opus")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"capability":0.9,"priority":0.85,"cost":0.2,"description":"hard only"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    // GET again: opus is now customized with the new priority
    let (_, b3) = oneshot_req(
        st.clone(),
        Request::get("/api/native-models")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let opus3 = b3["families"].as_array().unwrap().iter().find(|f| f["family"] == "opus").unwrap().clone();
    assert_eq!(opus3["customized"], true);
    // f32→JSON widens to f64, so compare with a tolerance rather than `== 0.85` (which would fail).
    assert!((opus3["priority"].as_f64().unwrap() - 0.85).abs() < 1e-6);
    assert_eq!(opus3["description"], "hard only");
}

#[tokio::test]
async fn native_models_post_rejects_other_and_bad_family_and_clamps() {
    let _ov = isolated_native_overrides_file();
    let st = test_state().await;

    // `other` is read-only
    let (s_other, _) = oneshot_req(
        st.clone(),
        Request::post("/api/native-models/other")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"capability":0.5,"priority":0.5,"cost":0.5}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s_other, StatusCode::BAD_REQUEST);

    // unknown family
    let (s_bad, _) = oneshot_req(
        st.clone(),
        Request::post("/api/native-models/nope")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"capability":0.5,"priority":0.5,"cost":0.5}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s_bad, StatusCode::BAD_REQUEST);

    // out-of-range clamps (stored value is 1.0, not 5.0)
    let (s_ok, _) = oneshot_req(
        st.clone(),
        Request::post("/api/native-models/sonnet")
            .header("authorization", auth(&st))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"capability":5.0,"priority":-1.0,"cost":0.5}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s_ok, StatusCode::OK);
    let m = crate::engine::native_overrides::load_map();
    assert_eq!(m["sonnet"].capability, 1.0);
    assert_eq!(m["sonnet"].priority, 0.0);
}

#[tokio::test]
async fn native_models_delete_resets_and_validates_family() {
    let _ov = isolated_native_overrides_file();
    let st = test_state().await;

    // seed an override, then reset it
    crate::engine::native_overrides::upsert(
        "opus",
        crate::engine::native_overrides::NativeOverride { capability: 0.9, priority: 0.8, cost: 0.2, description: String::new() },
    )
    .unwrap();
    let (s, _) = oneshot_req(
        st.clone(),
        Request::delete("/api/native-models/opus")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(crate::engine::native_overrides::load_map().get("opus").is_none());

    // idempotent: deleting again is still OK
    let (s2, _) = oneshot_req(
        st.clone(),
        Request::delete("/api/native-models/opus")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    // invalid family → 400
    let (s3, _) = oneshot_req(
        st.clone(),
        Request::delete("/api/native-models/other")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s3, StatusCode::BAD_REQUEST);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib api::misc::tests::native_models`
Expected: FAIL — handlers/routes not found (404 or compile error).

- [ ] **Step 3: Implement the handlers**

Add to `misc.rs` (after the provider CRUD section, before or after the model-catalog section):

```rust
// ── native Claude per-family routing overrides ──

#[derive(serde::Serialize)]
struct NativeModelRef {
    id: String,
    display_name: String,
}

#[derive(serde::Serialize)]
struct NativeFamilyView {
    family: String,
    label: String,
    models: Vec<NativeModelRef>,
    capability: f32,
    priority: f32,
    cost: f32,
    description: String,
    customized: bool,
    editable: bool,
}

#[derive(serde::Deserialize)]
struct NativeOverrideReq {
    capability: f32,
    priority: f32,
    cost: f32,
    #[serde(default)]
    description: String,
}

fn is_editable_family(family: &str) -> bool {
    matches!(family, "opus" | "sonnet" | "haiku" | "fable")
}

fn family_label(family: &str) -> &'static str {
    match family {
        "opus" => "Opus",
        "sonnet" => "Sonnet",
        "haiku" => "Haiku",
        "fable" => "Fable",
        _ => "Other",
    }
}

/// GET /api/native-models — native Claude families with effective routing metrics + override state.
pub async fn native_models_get() -> Response {
    use crate::engine::providers::{family_default_metrics, family_of, native_claude_models, DEFAULT_NATIVE_PRIORITY};
    let overrides = crate::engine::native_overrides::load_map();

    // Group discovered models by family, first-seen (newest-first) order.
    let mut order: Vec<&'static str> = Vec::new();
    let mut groups: std::collections::HashMap<&'static str, Vec<NativeModelRef>> = std::collections::HashMap::new();
    for m in native_claude_models() {
        let fam = family_of(&m.id);
        groups.entry(fam).or_default().push(NativeModelRef { id: m.id.clone(), display_name: m.display_name.clone() });
        if !order.contains(&fam) {
            order.push(fam);
        }
    }

    let mut views: Vec<NativeFamilyView> = order
        .into_iter()
        .map(|fam| {
            let (dc, dk) = family_default_metrics(fam);
            let (capability, priority, cost, description, customized) = match overrides.get(fam) {
                Some(o) => (o.capability, o.priority, o.cost, o.description.clone(), true),
                None => (dc, DEFAULT_NATIVE_PRIORITY, dk, String::new(), false),
            };
            NativeFamilyView {
                family: fam.to_string(),
                label: family_label(fam).to_string(),
                models: groups.remove(fam).unwrap_or_default(),
                capability,
                priority,
                cost,
                description,
                customized,
                editable: is_editable_family(fam),
            }
        })
        .collect();
    // cheap → capable, family-name tiebreak (matches the /api/models ordering contract).
    views.sort_by(|a, b| a.capability.total_cmp(&b.capability).then_with(|| a.family.cmp(&b.family)));

    Json(json!({ "families": views })).into_response()
}

/// POST /api/native-models/{family} — set a family's routing override.
pub async fn native_models_post(
    axum::extract::Path(family): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let family = family.trim().to_lowercase();
    if !is_editable_family(&family) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("not an editable family: {family}")}))).into_response();
    }
    let req: NativeOverrideReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid override: {e}")}))).into_response(),
    };
    for (name, v) in [("capability", req.capability), ("priority", req.priority), ("cost", req.cost)] {
        if v.is_nan() {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{name} cannot be NaN")}))).into_response();
        }
    }
    let ov = crate::engine::native_overrides::NativeOverride {
        capability: req.capability.clamp(0.0, 1.0),
        priority: req.priority.clamp(0.0, 1.0),
        cost: req.cost.clamp(0.0, 1.0),
        description: req.description,
    };
    match crate::engine::native_overrides::upsert(&family, ov) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// DELETE /api/native-models/{family} — reset a family to defaults (idempotent).
pub async fn native_models_delete(axum::extract::Path(family): axum::extract::Path<String>) -> Response {
    let family = family.trim().to_lowercase();
    if !is_editable_family(&family) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("not an editable family: {family}")}))).into_response();
    }
    match crate::engine::native_overrides::remove(&family) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}
```

- [ ] **Step 4: Register the routes**

In `server-rs/src/api/mod.rs`, inside the auth-gated `compressed` block, right after the `/api/providers/{name}` route (~line 97):

```rust
        .route("/api/native-models", get(misc::native_models_get))
        .route("/api/native-models/{family}", post(misc::native_models_post).delete(misc::native_models_delete))
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib api::misc::tests::native_models`
Expected: PASS (all three tests). Then `cargo build` to confirm the whole crate compiles.

- [ ] **Step 6: Commit**

```bash
git add server-rs/src/api/misc.rs server-rs/src/api/mod.rs
git commit -m "feat(api): /api/native-models CRUD for per-family routing overrides"
```

## Phase A wrap-up

- [ ] Run the full suite: `make test` (from `agentic-dev/`). Expected: green.
- [ ] Adversarially verify the diff with `delegate` (per repo `CLAUDE.md` step 1), fix anything real.
- [ ] Push the branch, open a PR with `gh pr create` targeting `master`; follow the Codex-review → auto-merge flow. Backend must land before the Android PR (the app depends on the endpoints).

---

# Phase B — Android (`agentic-dev-android`, one PR)

No unit-test scaffolding is assumed; each task ends with a Kotlin compile check. Run from the repo root; ensure `local.properties` is present (symlink from the main checkout per `CLAUDE.md` if missing). Compile check: `./gradlew :app:compileDebugKotlin`.

## Task B1: DTOs + API client methods

**Files:**
- Modify: `app/src/main/java/dev/agentic/data/net/Models.kt` (add DTOs near `Provider`, ~line 492)
- Modify: `app/src/main/java/dev/agentic/data/net/AgenticApi.kt` (add interface methods after `deleteProvider`, ~line 87)
- Modify: `app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt` (add implementations after `deleteProvider`, ~line 701)

**Interfaces:**
- Produces: `NativeModelRef`, `NativeFamily`, `NativeFamilyList`, `NativeOverrideReq` DTOs; `AgenticApi.nativeModels()`, `putNativeOverride(family, req)`, `deleteNativeOverride(family)`.

- [ ] **Step 1: Add the DTOs** (`Models.kt`, after the `ProviderList` block)

```kotlin
/** One native Claude model as discovered by the Anthropic Models API. */
@Serializable
data class NativeModelRef(
    val id: String,
    @SerialName("display_name") val displayName: String,
)

/** A native Claude model family with its effective routing metrics (GET /api/native-models).
 *  [editable] is false for the `other` catch-all; [customized] means an override row exists. */
@Serializable
data class NativeFamily(
    val family: String,
    val label: String,
    val models: List<NativeModelRef> = emptyList(),
    val capability: Float = 0.5f,
    val priority: Float = 0.5f,
    val cost: Float = 0.5f,
    val description: String = "",
    val customized: Boolean = false,
    val editable: Boolean = false,
)

@Serializable
data class NativeFamilyList(val families: List<NativeFamily> = emptyList())

/** Body for POST /api/native-models/{family}. */
@Serializable
data class NativeOverrideReq(
    val capability: Float,
    val priority: Float,
    val cost: Float,
    val description: String = "",
)
```

- [ ] **Step 2: Add the interface methods** (`AgenticApi.kt`, after `deleteProvider`)

```kotlin
    // Native Claude model per-family routing overrides.
    suspend fun nativeModels(): List<NativeFamily> = emptyList()
    suspend fun putNativeOverride(family: String, req: NativeOverrideReq) {}
    suspend fun deleteNativeOverride(family: String) {}
```

- [ ] **Step 3: Add the implementations** (`KtorAgenticApi.kt`, after `deleteProvider`)

```kotlin
    // ── Feature: Native Claude model per-family routing overrides ──────────────
    override suspend fun nativeModels(): List<NativeFamily> {
        return try {
            val r: List<NativeFamily> = client.get("$baseUrl/api/native-models") { auth() }
                .body<NativeFamilyList>().families
            AppLog.d("API", "GET native-models -> OK (${r.size})")
            r
        } catch (e: Exception) {
            AppLog.w("API", "GET native-models -> FAILED: ${e.message}")
            throw e
        }
    }

    override suspend fun putNativeOverride(family: String, req: NativeOverrideReq) {
        try {
            client.post("$baseUrl/api/native-models/${family.encodeURLPathPart()}") {
                auth(); contentType(ContentType.Application.Json); setBody(req)
            }
            AppLog.d("API", "POST native-models/$family -> OK")
        } catch (e: Exception) {
            AppLog.w("API", "POST native-models/$family -> FAILED: ${e.message}")
            throw e
        }
    }

    override suspend fun deleteNativeOverride(family: String) {
        try {
            client.delete("$baseUrl/api/native-models/${family.encodeURLPathPart()}") { auth() }
            AppLog.d("API", "DELETE native-models/$family -> OK")
        } catch (e: Exception) {
            AppLog.w("API", "DELETE native-models/$family -> FAILED: ${e.message}")
            throw e
        }
    }
```

(If `NativeFamily`/`NativeFamilyList`/`NativeOverrideReq` aren't auto-imported, add `import dev.agentic.data.net.*` equivalents — they live in the same `dev.agentic.data.net` package as `KtorAgenticApi`, so no import is needed.)

- [ ] **Step 4: Compile check**

Run: `./gradlew :app:compileDebugKotlin`
Expected: BUILD SUCCESSFUL.

- [ ] **Step 5: Commit**

```bash
git add app/src/main/java/dev/agentic/data/net/Models.kt app/src/main/java/dev/agentic/data/net/AgenticApi.kt app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt
git commit -m "feat(api): native-models client methods + DTOs"
```

## Task B2: `NativeModelsViewModel`

**Files:**
- Create: `app/src/main/java/dev/agentic/ui/providers/NativeModelsViewModel.kt`

**Interfaces:**
- Consumes: `AgenticApi.nativeModels/putNativeOverride/deleteNativeOverride`, `NativeFamily`, `NativeOverrideReq`.
- Produces: `class NativeModelsViewModel(api)`; `NativeModelsUiState(families, loading, busy, error)`; `refresh()`, `save(family, req, onResult)`, `reset(family)`.

- [ ] **Step 1: Create the ViewModel** (mirrors `ProvidersViewModel`; note it does NOT call `ModelCatalog.invalidate()` — overrides don't touch `/api/models`)

```kotlin
package dev.agentic.ui.providers

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.agentic.data.log.AppLog
import dev.agentic.data.net.AgenticApi
import dev.agentic.data.net.NativeFamily
import dev.agentic.data.net.NativeOverrideReq
import dev.agentic.data.net.Outcome
import dev.agentic.data.net.runCatchingOutcome
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

data class NativeModelsUiState(
    val families: List<NativeFamily> = emptyList(),
    val loading: Boolean = true,
    val busy: Boolean = false,
    val error: String? = null,
)

/**
 * Manage per-family routing overrides for the native Claude models
 * (GET/POST/DELETE /api/native-models). Separate from ProvidersViewModel: native models are
 * discovered, not BYOK, and these overrides do not affect the session main-model picker.
 */
class NativeModelsViewModel(private val api: AgenticApi) : ViewModel() {

    private val _uiState = MutableStateFlow(NativeModelsUiState())
    val uiState: StateFlow<NativeModelsUiState> = _uiState.asStateFlow()

    init { refresh() }

    fun refresh() {
        _uiState.update { it.copy(loading = true, error = null) }
        viewModelScope.launch {
            when (val r = runCatchingOutcome { api.nativeModels() }) {
                is Outcome.Success -> _uiState.update { it.copy(families = r.value, loading = false) }
                is Outcome.Failure -> {
                    AppLog.w("VM", "native models load failed err=${r.error}")
                    _uiState.update { it.copy(loading = false, error = r.error.toString()) }
                }
            }
        }
    }

    /** Save a family override, then refresh. [onResult] gets null on success or an error message. */
    fun save(family: String, req: NativeOverrideReq, onResult: (String?) -> Unit) {
        if (_uiState.value.busy) return
        _uiState.update { it.copy(busy = true, error = null) }
        viewModelScope.launch {
            when (val r = runCatchingOutcome { api.putNativeOverride(family, req) }) {
                is Outcome.Success -> {
                    _uiState.update { it.copy(busy = false) }
                    refresh()
                    onResult(null)
                }
                is Outcome.Failure -> {
                    val msg = r.error.toString()
                    _uiState.update { it.copy(busy = false, error = msg) }
                    onResult(msg)
                }
            }
        }
    }

    /** Reset a family to defaults, then refresh. */
    fun reset(family: String) {
        if (_uiState.value.busy) return
        _uiState.update { it.copy(busy = true, error = null) }
        viewModelScope.launch {
            when (val r = runCatchingOutcome { api.deleteNativeOverride(family) }) {
                is Outcome.Success -> {
                    _uiState.update { it.copy(busy = false) }
                    refresh()
                }
                is Outcome.Failure -> _uiState.update { it.copy(busy = false, error = r.error.toString()) }
            }
        }
    }
}
```

- [ ] **Step 2: Compile check**

Run: `./gradlew :app:compileDebugKotlin`
Expected: BUILD SUCCESSFUL.

- [ ] **Step 3: Commit**

```bash
git add app/src/main/java/dev/agentic/ui/providers/NativeModelsViewModel.kt
git commit -m "feat(ui): NativeModelsViewModel for per-family overrides"
```

## Task B3: Collapsible native section + family card + edit dialog

**Files:**
- Modify: `app/src/main/java/dev/agentic/ui/providers/ProvidersScreen.kt` (call `NativeModelsSection()` at the end of `ModelsSections`; add three private composables in the same file so they can reuse the file-private `MetricRow`)

**Interfaces:**
- Consumes: `NativeModelsViewModel`, `NativeFamily`, `NativeOverrideReq`, file-private `MetricRow`, `SectionCard`, `FloatSliderField`, `AppTextField`, `appContainer()`.

- [ ] **Step 1: Add imports** (top of `ProvidersScreen.kt`, matching existing import style)

```kotlin
import androidx.compose.material.icons.rounded.ExpandLess
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import dev.agentic.data.net.NativeFamily
import dev.agentic.data.net.NativeOverrideReq
import dev.agentic.ui.components.AppTextField
import dev.agentic.ui.components.FloatSliderField
```

(Some may already be imported — `AppTextField`/`FloatSliderField` are used by the provider form; skip duplicates.)

- [ ] **Step 2: Call the section from `ModelsSections`**

At the very end of `fun ModelsSections()` (after the "Sub-agent models" `SectionCard { ... }` block, before the function's closing brace ~line 322), add:

```kotlin
    NativeModelsSection()
```

- [ ] **Step 3: Add the section, card, and dialog composables** (append to `ProvidersScreen.kt`)

```kotlin
// ── Claude Code official (native) models — collapsible per-family override section ──

@Composable
private fun NativeModelsSection() {
    val container = appContainer()
    val vm: NativeModelsViewModel = viewModel(
        factory = viewModelFactory { initializer { NativeModelsViewModel(container.api) } },
    )
    val ui by vm.uiState.collectAsStateWithLifecycle()
    var expanded by rememberSaveable { mutableStateOf(false) }
    var editing by remember { mutableStateOf<NativeFamily?>(null) }

    SectionCard(
        title = "Claude Code official models",
        trailing = {
            IconButton(onClick = { expanded = !expanded }) {
                Icon(
                    if (expanded) Icons.Rounded.ExpandLess else Icons.Rounded.ExpandMore,
                    contentDescription = if (expanded) "Collapse" else "Expand",
                )
            }
        },
    ) {
        // Single Column child so a collapsed AnimatedVisibility doesn't double the card's gap.
        Column {
            AnimatedVisibility(
                visible = expanded,
                enter = expandVertically() + fadeIn(),
                exit = shrinkVertically() + fadeOut(),
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    val err = ui.error
                    if (err != null) {
                        Text(err, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
                    }
                    when {
                        ui.loading -> LoadingIndicator()
                        ui.families.isEmpty() -> Text(
                            "No native Claude models discovered.",
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            style = MaterialTheme.typography.bodyMedium,
                        )
                        else -> ui.families.forEach { fam ->
                            key(fam.family) {
                                NativeFamilyCard(fam, ui.busy, onEdit = { editing = fam })
                            }
                        }
                    }
                }
            }
        }
    }

    val target = editing
    if (target != null) {
        NativeOverrideDialog(
            family = target,
            busy = ui.busy,
            onDismiss = { editing = null },
            onSave = { req -> vm.save(target.family, req) { e -> if (e == null) editing = null } },
            onReset = { vm.reset(target.family); editing = null },
        )
    }
}

@Composable
private fun NativeFamilyCard(fam: NativeFamily, busy: Boolean, onEdit: () -> Unit) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        shape = MaterialTheme.shapes.medium,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Text(fam.label, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
                        if (fam.customized) {
                            Text(
                                "Customized",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.primary,
                                fontWeight = FontWeight.Bold,
                            )
                        }
                    }
                    if (fam.models.isNotEmpty()) {
                        Text(
                            fam.models.joinToString(", ") { it.id },
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                if (fam.editable) {
                    IconButton(onClick = onEdit, enabled = !busy) {
                        Icon(Icons.Rounded.Edit, contentDescription = "Edit ${fam.label}")
                    }
                }
            }
            val labelColor = MaterialTheme.colorScheme.onSurfaceVariant
            val barColor = MaterialTheme.colorScheme.primary
            val trackColor = MaterialTheme.colorScheme.surfaceContainerHighest
            MetricRow("Capability", fam.capability, barColor, trackColor, labelColor)
            MetricRow("Priority", fam.priority, barColor, trackColor, labelColor)
            MetricRow("Cost", fam.cost, barColor, trackColor, labelColor)
        }
    }
}

@Composable
private fun NativeOverrideDialog(
    family: NativeFamily,
    busy: Boolean,
    onDismiss: () -> Unit,
    onSave: (NativeOverrideReq) -> Unit,
    onReset: () -> Unit,
) {
    var capability by remember(family.family) { mutableFloatStateOf(family.capability) }
    var priority by remember(family.family) { mutableFloatStateOf(family.priority) }
    var cost by remember(family.family) { mutableFloatStateOf(family.cost) }
    var description by remember(family.family) { mutableStateOf(family.description) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("${family.label} routing") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                FloatSliderField(label = "Capability", value = { capability }, onValueChange = { capability = it })
                FloatSliderField(label = "Scheduling priority", value = { priority }, onValueChange = { priority = it })
                FloatSliderField(label = "Relative cost (0 = cheapest)", value = { cost }, onValueChange = { cost = it })
                AppTextField(
                    value = description,
                    onValueChange = { description = it },
                    placeholder = "Good at (the router reads this)",
                    enabled = !busy,
                    modifier = Modifier.fillMaxWidth(),
                )
                if (family.customized) {
                    TextButton(onClick = onReset, enabled = !busy) {
                        Text("Reset to default", color = MaterialTheme.colorScheme.error)
                    }
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = !busy,
                onClick = {
                    onSave(NativeOverrideReq(capability = capability, priority = priority, cost = cost, description = description))
                },
            ) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
```

- [ ] **Step 4: Compile check**

Run: `./gradlew :app:compileDebugKotlin`
Expected: BUILD SUCCESSFUL. Fix any signature mismatches against the real components (`AppTextField`, `FloatSliderField`, `SectionCard`, `MetricRow`) if the compiler flags them.

- [ ] **Step 5: Commit**

```bash
git add app/src/main/java/dev/agentic/ui/providers/ProvidersScreen.kt
git commit -m "feat(ui): collapsible Claude Code official models section with per-family override editor"
```

## Phase B wrap-up

- [ ] Adversarially verify the diff with `delegate` (per repo `CLAUDE.md` step 1), fix anything real.
- [ ] Push the branch, open a PR with `gh pr create` targeting `master`; follow the Codex-review → auto-merge flow.
- [ ] APK build/delivery is a separate downstream step in the main checkout after merge (only if the user asks for an APK).

---

## Manual verification (after both PRs merge)

1. Start the backend; on the Android app open Global Settings → the new "Claude Code official models" section is collapsed.
2. Expand it → one card per discovered family (Opus/Sonnet/Haiku/…); `other` (if present) has no edit button.
3. Edit Opus → raise Priority, Save → the card shows "Customized" and the new value; the overrides file `~/.agentic-dev/native-overrides.json` has an `opus` row.
4. Run a delegate fan-out with an un-pinned task the router would send to a capable model → confirm the raised-priority family is preferred (check the run summary's route reason).
5. Reset Opus → card returns to defaults, "Customized" badge gone, the `opus` row is removed from the file.
