# Session Start Model Scope Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep MiniMax/DeepSeek/BYOK provider models out of New Request and Session Settings main-thread model pickers while preserving the full model catalog for display and provider-aware uses.

**Architecture:** Extend the existing backend `GET /api/models` endpoint with a `scope=session_start` query parameter. Android keeps the full catalog for labels, adds a second session-start catalog cache for pickers, and makes New Request plus Session Settings use only that scoped catalog.

**Tech Stack:** Rust/axum backend in `agentic-dev/server-rs`; Kotlin/Jetpack Compose Android client in `agentic-dev-android/app/src/main/java/dev/agentic`.

## Global Constraints

- `GET /api/models` without `scope` must remain backward-compatible and return native Claude models plus registered BYOK providers.
- `GET /api/models?scope=session_start` must return only native Claude/session-start models.
- Unknown `scope` values must return `400 Bad Request`.
- New Request and Session Settings model pickers must show only scoped session-start models plus `Default`.
- If no scoped Claude/native models are available, the picker must show only `Default` and submit/patch `model = null`.
- BYOK providers remain available for delegate/fanout routing and provider management.
- Android worktree cannot build APK here; do not run Gradle in this worktree.
- Backend tests are run from `agentic-dev/server-rs` with `cargo test`.

---

## File Structure

### Backend repo: `agentic-dev/`

- Modify: `server-rs/src/api/misc.rs`
  - Add query parsing for `GET /api/models`.
  - Split catalog construction into native-only and full variants.
  - Return `400 Bad Request` for unknown scopes.
  - Add tests for default full catalog, scoped session-start catalog, and invalid scope.

### Android repo: `agentic-dev-android/`

- Modify: `app/src/main/java/dev/agentic/data/net/AgenticApi.kt`
  - Add a scoped/session-start model catalog API method.
- Modify: `app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt`
  - Implement the scoped API call to `/api/models?scope=session_start`.
- Modify: `app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt`
  - Add repository method that fetches session-start models and initializes the picker catalog.
- Modify: `app/src/main/java/dev/agentic/ui/ModelCatalog.kt`
  - Add separate session-start cache and picker helpers.
- Modify: `app/src/main/java/dev/agentic/ui/ModelEffortLabels.kt`
  - Add `SESSION_START_MODEL_OPTIONS` or equivalent, preserving `MODEL_OPTIONS` for full catalog labels/legacy uses.
- Modify: `app/src/main/java/dev/agentic/ui/newrequest/NewRequestViewModel.kt`
  - Load session-start model catalog and use default session-start model key.
- Modify: `app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt`
  - Use the session-start model options for the Model slider.
- Modify: `app/src/main/java/dev/agentic/ui/session/SessionSettingsScreen.kt`
  - Use the session-start model options for the Model slider.

---

### Task 1: Backend scoped model catalog

**Files:**
- Modify: `agentic-dev/server-rs/src/api/misc.rs:317-367`
- Test: `agentic-dev/server-rs/src/api/misc.rs` test module

**Interfaces:**
- Consumes: existing `native_claude_candidates()`, `native_claude_model_id()`, `native_claude_label()`, and `load_list()` from `crate::engine::providers`.
- Produces: `GET /api/models?scope=session_start` returning `{ "models": [ModelEntry...] }` with only native entries; `GET /api/models?scope=<unknown>` returning 400.

- [ ] **Step 1: Add failing backend tests**

Add tests to the existing `#[cfg(test)] mod tests` in `server-rs/src/api/misc.rs`.

Use these test names and bodies. If helper signatures differ, adapt only the request construction to match existing tests in the same module.

```rust
#[tokio::test]
async fn models_get_default_scope_includes_registered_providers() {
    let st = test_state().await;

    crate::engine::providers::upsert_at(
        &st.config.providers_path,
        crate::engine::providers::Provider {
            name: "minimax".into(),
            base_url: "https://api.minimaxi.com/anthropic".into(),
            api_key: "SECRET".into(),
            api_key_env: None,
            model: "MiniMax-M3".into(),
            protocol: crate::engine::providers::Protocol::Anthropic,
            capability: 0.5,
            description: None,
            priority: 0.5,
            cost: 0.3,
            router: false,
        },
    )
    .unwrap();

    let (s, b) = oneshot_req(
        st.clone(),
        Request::get("/api/models")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(s, StatusCode::OK);
    let models = b["models"].as_array().unwrap();
    assert!(models.iter().any(|m| m["key"] == "minimax"));
    assert!(models.iter().any(|m| m["native"] == false));
}

#[tokio::test]
async fn models_get_session_start_scope_excludes_registered_providers() {
    let st = test_state().await;

    crate::engine::providers::upsert_at(
        &st.config.providers_path,
        crate::engine::providers::Provider {
            name: "deepseek".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "SECRET".into(),
            api_key_env: None,
            model: "deepseek-chat".into(),
            protocol: crate::engine::providers::Protocol::Anthropic,
            capability: 0.6,
            description: None,
            priority: 0.5,
            cost: 0.5,
            router: false,
        },
    )
    .unwrap();

    let (s, b) = oneshot_req(
        st.clone(),
        Request::get("/api/models?scope=session_start")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(s, StatusCode::OK);
    let models = b["models"].as_array().unwrap();
    assert!(!models.is_empty(), "native Claude candidates should be present");
    assert!(!models.iter().any(|m| m["key"] == "deepseek"));
    assert!(models.iter().all(|m| m["native"] == true));
}

#[tokio::test]
async fn models_get_rejects_unknown_scope() {
    let st = test_state().await;

    let (s, b) = oneshot_req(
        st.clone(),
        Request::get("/api/models?scope=delegate")
            .header("authorization", auth(&st))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("scope"));
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev/server-rs
cargo test models_get_ -- --nocapture
```

Expected: tests fail because scoped query behavior is not implemented or because helper access needs adjustment.

- [ ] **Step 3: Implement scope parsing and catalog helpers**

In `server-rs/src/api/misc.rs`, replace `models_get()` with query-aware logic.

Use this shape:

```rust
#[derive(serde::Deserialize)]
pub struct ModelsQuery {
    scope: Option<String>,
}

fn native_model_entries() -> Vec<ModelEntry> {
    use crate::engine::providers::{
        native_claude_candidates, native_claude_label, native_claude_model_id,
    };

    let natives = native_claude_candidates();
    let mut entries: Vec<ModelEntry> = natives
        .iter()
        .enumerate()
        .map(|(i, p)| ModelEntry {
            key: native_claude_model_id(&p.model).to_string(),
            label: native_claude_label(&p.model).to_string(),
            native: true,
            default: i == 0,
            capability: p.capability,
            cost: p.cost,
        })
        .collect();
    entries.sort_by(|a, b| a.capability.total_cmp(&b.capability));
    entries
}

fn full_model_entries() -> Vec<ModelEntry> {
    let mut entries = native_model_entries();
    for p in &crate::engine::providers::load_list() {
        entries.push(ModelEntry {
            key: p.name.clone(),
            label: format!("{} ({})", p.model, p.name),
            native: false,
            default: false,
            capability: p.capability,
            cost: p.cost,
        });
    }
    entries.sort_by(|a, b| a.capability.total_cmp(&b.capability));
    entries
}

/// GET /api/models — model catalog.
/// - no scope: native Claude tiers + registered BYOK providers
/// - scope=session_start: native Claude tiers only, for main-thread model pickers
pub async fn models_get(axum::extract::Query(q): axum::extract::Query<ModelsQuery>) -> Response {
    let entries = match q.scope.as_deref() {
        None | Some("") => full_model_entries(),
        Some("session_start") => native_model_entries(),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid models scope: {other}") })),
            )
                .into_response();
        }
    };

    Json(json!({ "models": entries })).into_response()
}
```

If `axum::extract::Query` is already imported in the file, use the existing import style instead of fully qualifying it.

- [ ] **Step 4: Run backend scoped tests**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev/server-rs
cargo test models_get_ -- --nocapture
```

Expected: all three `models_get_...` tests pass.

- [ ] **Step 5: Run backend test suite**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev/server-rs
cargo test
```

Expected: PASS. If unrelated existing tests fail, capture exact failing names and output.

- [ ] **Step 6: Commit backend API change**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev
git add server-rs/src/api/misc.rs
git commit -m "feat: scope session-start model catalog

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Android session-start catalog API and cache

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/net/AgenticApi.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/ModelCatalog.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/ModelEffortLabels.kt`

**Interfaces:**
- Consumes: backend `GET /api/models?scope=session_start`.
- Produces:
  - `AgenticApi.sessionStartModels(): List<ModelEntry>`
  - `SessionsRepository.sessionStartModelCatalog(): List<ModelEntry>`
  - `ModelCatalog.initSessionStart(entries: List<ModelEntry>)`
  - `ModelCatalog.sessionStartModelOptions(): List<Pair<String, String>>`
  - `ModelCatalog.defaultSessionStartModelKey(): String?`
  - `SESSION_START_MODEL_OPTIONS: List<Pair<String, String>>`

- [ ] **Step 1: Add API method to `AgenticApi.kt`**

Add this method near the existing `models()` method:

```kotlin
/** Model catalog for main-thread/session-start model pickers. Returns only Claude/native models. */
suspend fun sessionStartModels(): List<ModelEntry> = emptyList()
```

- [ ] **Step 2: Implement scoped request in `KtorAgenticApi.kt`**

Near the existing `override suspend fun models(): List<ModelEntry>`, add:

```kotlin
override suspend fun sessionStartModels(): List<ModelEntry> {
    return try {
        val r: List<ModelEntry> = client.get("$baseUrl/api/models") {
            auth()
            parameter("scope", "session_start")
        }.body<ModelsResponse>().models
        AppLog.d("API", "GET models?scope=session_start -> OK")
        r
    } catch (e: Exception) {
        AppLog.w("API", "GET models?scope=session_start -> FAILED: ${e.message}")
        emptyList()
    }
}
```

Keep the existing `models()` method unchanged.

- [ ] **Step 3: Add repository method in `SessionsRepository.kt`**

Near `modelCatalog()`, add:

```kotlin
/** Fetch the Claude-only model catalog for New Request and Session Settings pickers. */
suspend fun sessionStartModelCatalog(): List<ModelEntry> = try {
    api.sessionStartModels().also { dev.agentic.ui.ModelCatalog.initSessionStart(it) }
} catch (e: Exception) {
    AppLog.d("Repo", "sessionStartModelCatalog load failed: ${e.message}")
    emptyList()
}
```

Do not change `modelCatalog()`; it still initializes the full catalog.

- [ ] **Step 4: Extend `ModelCatalog.kt` with session-start cache**

Change the object to maintain two caches. Preserve existing `init`, `invalidate`, `isLoaded`, `modelOptions`, `defaultModelKey`, and `modelLabel` behavior for the full catalog.

Add:

```kotlin
@Volatile
private var sessionStartCached: List<ModelEntry>? = null

fun initSessionStart(entries: List<ModelEntry>) { sessionStartCached = entries }
```

Update `invalidate()` so it clears both caches:

```kotlin
fun invalidate() {
    cached = null
    sessionStartCached = null
}
```

Add picker helpers:

```kotlin
val isSessionStartLoaded: Boolean get() = sessionStartCached != null

/** Ordered weakest → strongest, with "Default" as the first notch. Only Claude/native entries. */
fun sessionStartModelOptions(): List<Pair<String, String>> {
    val entries = sessionStartCached.orEmpty().filter { it.native }
    if (entries.isEmpty()) return listOf("" to "Default")
    return listOf("" to "Default") + entries.map { it.key to it.label }
}

/** The model key pre-selected for new sessions from the Claude-only catalog. */
fun defaultSessionStartModelKey(): String? =
    sessionStartCached.orEmpty().firstOrNull { it.native && it.default }?.key
```

- [ ] **Step 5: Add picker options in `ModelEffortLabels.kt`**

Keep existing `MODEL_OPTIONS` unchanged. Add:

```kotlin
// Claude/native-only options for main-thread model pickers (New Request and Session Settings).
val SESSION_START_MODEL_OPTIONS: List<Pair<String, String>>
    get() = ModelCatalog.sessionStartModelOptions()
```

- [ ] **Step 6: Verify Android source text**

Run a syntax-oriented grep check:

```bash
grep -RIn "sessionStartModels\|sessionStartModelCatalog\|initSessionStart\|SESSION_START_MODEL_OPTIONS" \
  /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android/app/src/main/java/dev/agentic
```

Expected: all five new symbols appear in the files above.

- [ ] **Step 7: Commit Android API/cache change**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android
git add app/src/main/java/dev/agentic/data/net/AgenticApi.kt \
  app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt \
  app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt \
  app/src/main/java/dev/agentic/ui/ModelCatalog.kt \
  app/src/main/java/dev/agentic/ui/ModelEffortLabels.kt
git commit -m "feat: add session-start model catalog

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Android New Request and Session Settings picker wiring

**Files:**
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/newrequest/NewRequestViewModel.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt`
- Modify: `agentic-dev-android/app/src/main/java/dev/agentic/ui/session/SessionSettingsScreen.kt`

**Interfaces:**
- Consumes from Task 2:
  - `SessionsRepository.sessionStartModelCatalog()`
  - `ModelCatalog.defaultSessionStartModelKey()`
  - `SESSION_START_MODEL_OPTIONS`
- Produces: New Request and Session Settings model sliders that only offer `Default` plus Claude/native scoped entries.

- [ ] **Step 1: Update imports**

In `NewRequestScreen.kt`, replace:

```kotlin
import dev.agentic.ui.MODEL_OPTIONS
```

with:

```kotlin
import dev.agentic.ui.SESSION_START_MODEL_OPTIONS
```

In `SessionSettingsScreen.kt`, replace:

```kotlin
import dev.agentic.ui.MODEL_OPTIONS
```

with:

```kotlin
import dev.agentic.ui.SESSION_START_MODEL_OPTIONS
```

- [ ] **Step 2: Update New Request initial model and catalog load**

In `NewRequestViewModel.kt`, change the state default:

```kotlin
val model: String? = ModelCatalog.defaultSessionStartModelKey(),
```

In the init block, replace the model catalog load block:

```kotlin
sessionsRepo.modelCatalog()
val defaultModel = ModelCatalog.defaultModelKey()
```

with:

```kotlin
sessionsRepo.sessionStartModelCatalog()
val defaultModel = ModelCatalog.defaultSessionStartModelKey()
```

Keep the existing conditional update:

```kotlin
if (defaultModel != null) {
    _uiState.update { if (it.model == null) it.copy(model = defaultModel) else it }
}
```

Do not set a non-null model when the scoped catalog is empty.

- [ ] **Step 3: Update New Request slider options**

In `NewRequestScreen.kt`, change the Model `SliderField` options from:

```kotlin
options = MODEL_OPTIONS,
```

to:

```kotlin
options = SESSION_START_MODEL_OPTIONS,
```

Keep the existing `onSelect = { realVm.setModel(it.ifBlank { null }) }`.

- [ ] **Step 4: Update Session Settings slider options**

In `SessionSettingsScreen.kt`, change the Model `SliderField` options from:

```kotlin
options = MODEL_OPTIONS,
```

to:

```kotlin
options = SESSION_START_MODEL_OPTIONS,
```

Keep the existing `value = s.pendingModel ?: ""` and `onSelect = { vm.setPendingModel(it.ifBlank { null }) }`.

- [ ] **Step 5: Ensure session-start catalog loads before settings use**

Find the session settings view model if it already loads `modelCatalog()`. Search:

```bash
grep -RIn "modelCatalog()\|sessionStartModelCatalog()" \
  /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android/app/src/main/java/dev/agentic/ui/session \
  /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android/app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt
```

If the settings view model does not load any model catalog, add a best-effort load in its init block or screen launch path:

```kotlin
viewModelScope.launch {
    try {
        sessionsRepo.sessionStartModelCatalog()
    } catch (e: Exception) {
        AppLog.d("VM", "session settings model catalog load failed: ${e.message}")
    }
}
```

Use the actual repository property and logging imports already present in that view model.

- [ ] **Step 6: Verify no main-thread picker still uses full options**

Run:

```bash
grep -RIn "options = MODEL_OPTIONS\|import dev.agentic.ui.MODEL_OPTIONS" \
  /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android/app/src/main/java/dev/agentic/ui/newrequest \
  /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android/app/src/main/java/dev/agentic/ui/session
```

Expected: no output for `NewRequestScreen.kt` or `SessionSettingsScreen.kt`. If other session UI files still use full catalog only for labels, leave them unchanged.

- [ ] **Step 7: Commit Android picker wiring**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android
git add app/src/main/java/dev/agentic/ui/newrequest/NewRequestViewModel.kt \
  app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt \
  app/src/main/java/dev/agentic/ui/session/SessionSettingsScreen.kt
git commit -m "fix: limit main-thread model pickers to Claude models

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Final verification and PR preparation

**Files:**
- Inspect: `agentic-dev/server-rs/src/api/misc.rs`
- Inspect: changed Android Kotlin files

**Interfaces:**
- Consumes: completed backend and Android commits.
- Produces: verified branch state ready for PRs in both repositories.

- [ ] **Step 1: Verify backend tests**

Run:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev/server-rs
cargo test
```

Expected: PASS.

- [ ] **Step 2: Verify backend diff contains no secrets or accidental provider removal**

Run:

```bash
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev diff --stat HEAD~2..HEAD
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev diff HEAD~2..HEAD -- server-rs/src/api/misc.rs
```

Expected: only scoped catalog behavior and tests changed; provider registry behavior remains intact.

- [ ] **Step 3: Verify Android diff contains scoped picker wiring only**

Run:

```bash
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android diff --stat HEAD~2..HEAD
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android diff HEAD~2..HEAD -- app/src/main/java/dev/agentic
```

Expected: no provider presets removed, no APK/build artifacts, no secrets.

- [ ] **Step 4: Check repository statuses**

Run:

```bash
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev status --short
git -C /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android status --short
```

Expected: clean working trees, except any intentionally uncommitted plan/spec documents if implementation has not committed them yet.

- [ ] **Step 5: Prepare PRs per repo workflow**

Backend PR:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev
git push -u origin HEAD
gh pr create --fill
```

Android PR:

```bash
cd /home/arcatva/src/agentic-worktrees/f02ec220-15f4-43b1-883f-2460360ee554/agentic-dev-android
git push -u origin HEAD
gh pr create --fill
```

Expected: one PR per repository. Follow each repo's CLAUDE.md: wait for Codex review, triage comments, then auto-merge with `gh pr merge <n> --rebase` unless a genuine blocker appears.
