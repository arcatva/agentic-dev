# Session Start Model Scope Design

Date: 2026-06-29

## Summary

The New Request and Session Settings model pickers choose the main-thread/session-start model. They must only show Claude/native models returned by the backend's Claude model source. BYOK provider models such as MiniMax and DeepSeek are for delegated worker routing and must not appear as session starters.

The backend will keep the existing full model catalog for compatibility, and add a scoped query on the same endpoint:

- `GET /api/models` returns the current full catalog: native Claude models plus registered BYOK providers.
- `GET /api/models?scope=session_start` returns only native Claude/session-start models.

If the scoped catalog is empty or cannot be loaded, the Android picker shows only `Default`, and session creation/patching sends `model = null`.

## Current behavior

Backend `server-rs/src/api/misc.rs` builds `/api/models` by combining:

1. `native_claude_candidates()` entries, serialized as `native: true`.
2. `load_list()` BYOK providers, serialized as `native: false`.

Android stores that catalog in `ModelCatalog` and exposes `MODEL_OPTIONS` to both:

- `NewRequestScreen` for new session creation.
- `SessionSettingsScreen` for changing a session's model.

Because `/api/models` currently includes BYOK providers, external models such as MiniMax and DeepSeek can appear in these main-thread model pickers.

## Requirements

1. New Request model picker must not show external providers such as MiniMax or DeepSeek.
2. Session Settings model picker must follow the same rule, because it also changes the session/main-thread model.
3. The external provider registry must remain available for delegate/fanout routing.
4. Existing `/api/models` semantics must remain compatible for existing display code and any provider-aware UI.
5. If no Claude/native model is available, show `Default` only.
6. Unknown `scope` values should return a client error rather than silently returning the full catalog.

## Backend design

### Endpoint

Extend the existing route:

```text
GET /api/models
GET /api/models?scope=session_start
```

Response shape remains unchanged:

```json
{
  "models": [
    {
      "key": "claude-opus-4-8",
      "label": "Opus 4.8",
      "native": true,
      "default": true,
      "capability": 0.97,
      "cost": 1.0
    }
  ]
}
```

### Scope behavior

- No `scope`: full model catalog, same as today.
- `scope=session_start`: native Claude entries only.
- Any other scope: `400 Bad Request` with an error message naming the invalid scope.

### Catalog construction

Keep one helper that builds native entries from `native_claude_candidates()` and one helper that appends provider entries for the full catalog. Both catalog variants sort by `capability` ascending so the Android slider remains left-to-right weakest-to-strongest.

The `default` flag remains on the first native Claude candidate before sorting, matching the current behavior.

## Android design

### API layer

Add a scoped model-catalog call in `AgenticApi` / `KtorAgenticApi`. Either of these shapes is acceptable; prefer the smallest code change that fits current style:

```kotlin
suspend fun models(scope: String? = null): List<ModelEntry>
```

or

```kotlin
suspend fun sessionStartModels(): List<ModelEntry>
```

The scoped call must request:

```text
/api/models?scope=session_start
```

### Catalog cache

The Android app needs two catalog uses:

1. Full catalog: label/display compatibility for raw model ids and any provider-aware UI.
2. Session-start catalog: model picker options for New Request and Session Settings.

Implement this by either splitting the catalog cache or adding a second cache inside `ModelCatalog`, for example:

```kotlin
ModelCatalog.init(entries)                  // full catalog
ModelCatalog.initSessionStart(entries)      // Claude-only picker catalog
ModelCatalog.sessionStartModelOptions()
ModelCatalog.defaultSessionStartModelKey()
```

Fallback behavior remains simple:

- If the session-start catalog has not loaded, failed to load, or is empty: `listOf("" to "Default")`.
- If a raw model id is not found in the full catalog, labels still fall back to stripping the `claude-` prefix as today.

### New Request

`NewRequestViewModel` loads the session-start catalog during initialization. It uses `defaultSessionStartModelKey()` for its initial model. `NewRequestScreen` uses `sessionStartModelOptions()` for the Model slider.

If the scoped catalog returns empty, `model` remains `null`, the picker displays `Default`, and `NewSessionReq.model` remains null.

### Session Settings

`SessionSettingsScreen` also uses `sessionStartModelOptions()` for the Model slider. This keeps session model changes aligned with New Request: only Claude/native models or `Default` can be selected for the main thread.

Existing sessions whose stored model is an external provider should still render a readable tag through `modelLabel(rawModel)`, but the settings picker should not offer that provider as a selectable target. If such a session opens settings, the slider can show `Default` when the current external model is not present in session-start options; saving should only occur when the user taps Save.

## Testing plan

### Backend tests

Add tests in `server-rs/src/api/misc.rs` or adjacent API tests:

1. `GET /api/models` includes BYOK providers when registered.
2. `GET /api/models?scope=session_start` excludes BYOK providers and only includes `native: true` entries.
3. `GET /api/models?scope=bogus` returns `400 Bad Request`.

### Android tests / checks

If existing unit-test structure can cover it cheaply:

1. Session-start catalog with only Claude entries produces picker options: `Default + Claude entries`.
2. Empty session-start catalog produces only `Default`.
3. Full catalog labels still resolve provider model ids for display.

If Android test scaffolding is not available in the worktree, keep changes small and rely on code review plus downstream APK build in the main checkout per project guidance.

## Out of scope

- Removing BYOK providers or provider presets.
- Changing delegate/fanout routing behavior.
- Changing stored session model semantics.
- Renaming MiniMax/DeepSeek provider settings.
