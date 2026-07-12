# Router redesign: joint scoring + explicit enable/disable

**Status:** approved design (2026-07-12) — pending spec review, then implementation plan.
**Scope:** `agentic-dev` (Rust backend, the router) + `agentic-dev-android` (Providers/Settings UI).

## 1. Problem

The delegate fan-out router currently selects a model per task with a **strict
lexicographic** rule:

1. **capability** is a hard gate — skip any candidate below the task's judged difficulty;
2. among survivors prefer higher **priority** (0..1);
3. break ties on lower **cost** (0..1).

An LLM makes the initial per-task model pick; a deterministic layer
(`router::apply_priority`) only overrides that pick on a **strict** improvement
(strictly higher priority, or equal priority + strictly lower cost).

Two real defects fall out of this:

- **Tie pathology.** A cheaper-or-equal, *more capable* alternative that ties the
  LLM's pick on priority **and** cost is ignored, because "strict improvement only"
  keeps the LLM's arbitrary pick. Observed live: with native tiers at priority 0,
  a registered `deepseek-v4-pro` (capability 0.90, cost 0.50, priority 0) lost every
  adversarial-verify task to `sonnet` (0.85 / 0.50 / 0) — DeepSeek cleared the 0.85
  floor and was *more* capable, but tied on priority (0=0) and cost (0.50=0.50), so
  the tie preserved the LLM's Sonnet pick. Above-floor capability (0.90 vs 0.85) was
  discarded the moment both cleared the gate.
- **Circular floor.** The capability gate `bar` is set to *the capability of whatever
  model the LLM happened to pick*. A flaky LLM pick of a strong model raises the bar
  and locks out cheaper-but-adequate models — the bar is not an independent estimate
  of task difficulty.

There is also **no way to exclude a model** from routing. Native Claude tiers
(opus/sonnet/haiku/fable) are always candidates; the sliders tune *preference*, not
*membership*, so a user cannot say "never use Sonnet." `priority = 0` is the lowest
preference but the model is still fully eligible and wins whenever it is the cheapest
that clears the floor.

## 2. Industry grounding (why this design)

- **Quality threshold / cascade is standard.** RouteLLM calibrates a threshold on a
  *predicted difficulty/quality* score to hit a target strong-model rate; FrugalGPT
  cascades on per-stage quality thresholds. So a capability floor is correct — but the
  score is an **independent** per-query estimate, not "the capability of the first pick."
- **Joint utility, not lexicographic.** The field optimizes a (quality, cost) Pareto
  frontier — pick the point maximizing quality per cost. OpenRouter composes signals
  (`sort` + `max_price`), Unify weights a metric config over {quality, latency, cost},
  NotDiamond exposes quality/cost/latency **tradeoff modes**. All are a joint objective
  with a user-set tradeoff weight.
- **Membership is a separate control.** OpenRouter uses `only`/`ignore`/`order`/
  `allow_fallbacks`/`max_price`; load balancers use weight-0 = out of rotation. Enable/
  disable is expressed **separately** from the preference signal.

Sources: OpenRouter Provider Routing; RouteLLM (LMSYS); LiteLLM Router; FrugalGPT /
cascade survey; NotDiamond docs; Unify metric config.

## 3. Design decisions (locked)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Difficulty floor source | **Router LLM emits a per-task difficulty (0..1)** in the same call; floor = that. Removes the circularity, ~free. |
| 2 | Signal combination | **Floor + joint weighted score.** Keep the capability floor; above it, argmax a joint score. Ties break toward cheaper. |
| 3 | Global tradeoff knob | **Add it, keep per-model priority.** One global "cheaper ⇄ stronger" knob sets the weights; priority stays as fine-tuning. Back-compatible. |
| 4 | Enable/disable | **Separate `Enabled` toggle** per model. Off = grayed card + removed from candidate pool. `priority` stays pure preference. |

**Architectural shift:** the LLM's job changes from *"pick the model"* to *"estimate
task difficulty (0..1)"*. A **deterministic scorer** then selects the model. One
unreliable LLM call can no longer lock out a cheaper adequate model.

## 4. Routing pipeline (replaces `apply_priority`)

```
candidates = enabled registered providers (keyed, protocol-eligible)
           + enabled native Claude families              # disabled → excluded
  │
  ├─ difficulty d ∈ [0,1]  ← router LLM, per task (independent of model choice)
  │
  ├─ floor filter: keep candidates with capability ≥ d − ε
  │     (if NONE clear the floor → fall back to the single most-capable candidate,
  │      so a hard task still runs on the best available model)
  │
  └─ joint score, argmax:
        S(m) = (1 − t)·(1 − cost_m) + t·capability_m + β·priority_m
        tie-break order: lower cost → higher capability → non-native → stable index
```

- `t` — **global tradeoff** ∈ [0,1], `0` = cheapest, `1` = strongest. Default `0.5`.
- `β` — per-model priority weight, small constant (start `0.5`; tune in impl). Priority
  becomes a persistent nudge, not a hard lexicographic axis.
- `capability_m` is used **raw** (not `capability − d`) in the score, so the knob toward
  "stronger" meaningfully prefers higher-capability models; the floor already removed the
  inadequate ones.
- **Tie-break drops "strict improvement only."** On an exact score tie the deterministic
  scorer prefers cheaper/stronger/registered — it never falls back to "keep the LLM's pick."

### Degradation
- Router disabled / call fails → no per-task difficulty. Fall back to difficulty `0.5`
  (or run the existing native-default path) so the fan-out still completes; log it.
- Empty candidate set (everything disabled) → error surfaced to the caller, not a silent
  native fallback. (Guard: never let the user disable themselves into a no-op.)

## 5. Worked example (the DeepSeek fix)

Difficulty `d = 0.85`. Enabled candidates & floor(cap ≥ 0.85): DeepSeek(0.90) ✓,
Sonnet(0.85) ✓, Opus(0.97) ✓, Fable(0.99) ✓; MiniMax(0.70) ✗, Haiku(0.60) ✗.
`t = 0.5`, `β = 0.5`, all priority `0`:

| model | S = (1−t)(1−cost) + t·cap + β·prio | S |
|-------|-----------------------------------|---|
| **DeepSeek** (0.90/0.50/0) | 0.5·0.50 + 0.5·0.90 + 0 | **0.700 ✅** |
| Sonnet (0.85/0.50/0) | 0.5·0.50 + 0.5·0.85 + 0 | 0.675 |
| Opus (0.97/0.90/0) | 0.5·0.10 + 0.5·0.97 + 0 | 0.535 |
| Fable (0.99/1.00/0) | 0.5·0.00 + 0.5·0.99 + 0 | 0.495 |

DeepSeek wins — above-floor capability (0.90 > 0.85) is no longer discarded. Knob to
`t = 0.9` picks Opus (0.883 > 0.86); knob to `t = 0.2` keeps DeepSeek. Intuitive.

## 6. Data model & API changes

### Rust (`server-rs/src/engine/`)
- `providers::Provider` — add `#[serde(default = "default_enabled")] enabled: bool` (default `true`).
- `native_overrides::NativeOverride` — add `enabled: bool` (default `true`).
- **Global tradeoff config** — a new tiny persisted value `t ∈ [0,1]` (its own file,
  e.g. `~/.agentic-dev/routing.json` `{ "tradeoff": 0.5 }`, mirroring the providers-file
  CRUD: atomic write, 0600, corrupt-file-errors-not-wipes, test-override static). GET/POST API.
- `router.rs` — split responsibilities:
  - `estimate_difficulty(tasks, router, ask) -> HashMap<idx, f32>` (LLM emits difficulty; JSON parse + validation reused).
  - `select_model(difficulty, candidates, t, β) -> RouteChoice` (deterministic floor + joint score + tie-break). **Replaces `apply_priority`.**
  - Keep the transport seam (`AskFn`) and pure/unit-tested split.
- `delegate.rs` — filter candidates by `enabled`; call `estimate_difficulty` then
  `select_model`; surface the decision in the run summary reason, e.g.
  `w1→deepseek (difficulty 0.85, S 0.70 > sonnet 0.675)`.
- `native_claude_candidates` — carry `enabled` through; drop disabled families.

### API (`server-rs/src/api/`)
- Provider CRUD payloads gain `enabled`.
- Native-family override payloads gain `enabled`.
- New `GET/POST /api/routing` (or extend an existing settings route) for the global `tradeoff`.

### Android (`agentic-dev-android`)
- `Models.kt` — add `enabled` to `Provider`, `NativeFamily`, `NativeOverrideReq`; add a
  routing-config model for `tradeoff`.
- `ProvidersScreen.kt` — per-card **Enabled** switch (card grays + drops out visually when
  off); a global **tradeoff** slider ("更便宜 ⇄ 更强") at the top of the Providers/Settings
  screen; keep the priority slider but **label its direction** (like the cost slider's
  "(0 = cheapest)").
- Network layer (`KtorAgenticApi.kt`) — wire the new fields + routing endpoint.

## 7. Back-compat & migration
- `enabled` defaults `true`; old `providers.json` / `native-overrides.json` load unchanged
  (every model enabled).
- `tradeoff` defaults `0.5`; absent file → default.
- Existing capability/priority/cost values keep their meaning; with `t = 0.5` and current
  values the new math is sensible from day one. No destructive migration.

## 8. Testing
- **Pure unit tests** (in-crate, no network) for `select_model`: the DeepSeek tie case,
  floor filtering, empty-floor fallback, tie-break ordering, knob extremes (`t=0`, `t=1`),
  disabled-exclusion, priority nudge.
- `estimate_difficulty`: JSON parse/validation + transport-failure → graceful default
  (reuse the existing `AskFn` fake-transport pattern).
- Delegate flow: candidate filtering by `enabled`; summary reason string.
- Android: keep it light — a smoke test that `enabled=false` removes a card from the pool
  representation if such tests exist; otherwise manual.

## 9. Delivery
Two PRs (backend first, then Android), each via its repo's Codex-review auto-merge
workflow, with the delegate adversarial-verify pre-flight gate run on each diff.

## 10. Out of scope (YAGNI)
- Learned/trained routers (matrix factorization, BERT) — the LLM difficulty estimate is
  enough for a BYOK self-host; revisit only if difficulty estimates prove unreliable.
- Latency/throughput as a routing signal — not modeled today; add a third weight later if
  needed (the score is already a weighted sum, so it extends cleanly).
- Per-task tradeoff override — the knob is global for now.
