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
- **Floor coupled to the pick** (secondary). The gate `bar` = capability of the model the
  LLM picked, so a flaky *strong* pick raises the floor and can lock out cheaper-but-adequate
  models. Note: adversarial review (§11) concluded this coupling is the *lesser evil* — the
  pick is a committed judgment that bounds mis-estimation, whereas a free-floating difficulty
  scalar is unanchored. So the redesign **keeps** `floor = capability(pick)` and instead fixes
  the primary bug (the tie pathology) with the joint score above the floor.

There is also **no way to exclude a model** from routing. Native Claude tiers
(opus/sonnet/haiku/fable) are always candidates; the sliders tune *preference*, not
*membership*, so a user cannot say "never use Sonnet." `priority = 0` is the lowest
preference but the model is still fully eligible and wins whenever it is the cheapest
that clears the floor.

## 2. Industry grounding (why this design)

- **Quality threshold / cascade is standard.** RouteLLM calibrates a threshold on a
  *predicted difficulty/quality* score to hit a target strong-model rate; FrugalGPT
  cascades on per-stage quality thresholds. So a capability floor is correct. (Ideally the
  score is an independent per-query estimate; here we anchor it to the LLM's committed pick —
  see §11 for why that beat a free-floating difficulty scalar in review.)
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
| 1 | Difficulty floor source | **Keep the LLM's per-task model pick; floor `d = capability(pick)`.** *(Revised after adversarial review — see §11.)* A bare emitted difficulty scalar is *unanchored* (the LLM stakes nothing) and regresses on mis-estimation; the pick is a *committed* judgment, so its capability is a self-anchored floor. The pick's ONLY role is to set the floor. |
| 2 | Signal combination | **Floor + joint weighted score.** Keep the capability floor; above it, argmax a joint score. Ties break toward cheaper. |
| 3 | Global tradeoff knob | **Add it, keep per-model priority.** One global "cheaper ⇄ stronger" knob sets the weights; priority stays as fine-tuning. Back-compatible. |
| 4 | Enable/disable | **Separate `Enabled` toggle** per model. Off = grayed card + removed from candidate pool. `priority` stays pure preference. |

**Architectural shift:** the LLM still picks a per-task model, but its pick now only sets
the **difficulty floor** `d = capability(pick)`; a **deterministic joint scorer** re-ranks
all floor-survivors and makes the final choice. The lexicographic strict-improvement
override (`apply_priority`) is replaced. One unreliable LLM pick can no longer lock out a
cheaper *equally-capable* model, because above-floor capability is now scored, not discarded.

## 4. Routing pipeline (replaces `apply_priority`)

```
candidates = enabled registered providers (keyed, protocol-eligible)
           + enabled native Claude families              # disabled → excluded
  │   (if candidates is EMPTY → fail the delegate call with a clear error;      ← §11 fix
  │    NEVER fall through to the silent native-default path)
  │
  ├─ LLM pick per task  → difficulty floor  d = capability(pick)   # self-anchored, §3
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
- **Score scale.** The main axis `(1−t)·(1−cost) + t·capability` lies in `[0,1]` (its two
  coefficients sum to 1). Keep the priority term on the SAME scale so it stays a nudge.
- `β` — per-model priority weight, **`0.1`** (revised from 0.5 — see §11). At `β=0.1` a
  `priority=1` model gets at most `+0.1`, i.e. it only wins when it is already within `0.1`
  of the top on the main axis — a genuine nudge, provably not a hard override. `β` must stay
  `≪ 1`.
- `capability_m` is used **raw** (not `capability − d`) in the score, so the knob toward
  "stronger" meaningfully prefers higher-capability models; the floor already removed the
  inadequate ones.
- **Tie-break drops "strict improvement only."** On an exact score tie the deterministic
  scorer prefers cheaper/stronger/registered — it never falls back to "keep the LLM's pick."
- Report the decision on `/1.0` (main axis) in the summary reason so the printed comparison
  is apples-to-apples; if `β·priority` moved the pick, say so explicitly.

### Degradation
- Router disabled / call fails → no LLM pick. Fall back to a **mid floor `d = 0.5`** and run
  the joint scorer over the enabled candidates (so the fan-out still completes and still
  respects the knob/cost); log it. Do NOT drop to the raw subscription default model.
- **Empty candidate set** (user disabled everything, or Claude discovery failed AND all
  registered are disabled) → **fail the delegate call with an explicit error** ("no models
  enabled for routing"), surfaced as the tool result. This guard lives in `run_delegate`
  BEFORE the `router_provider` match, ahead of the existing silent native path that §11
  showed would otherwise swallow it.

## 5. Worked example (the DeepSeek fix)

LLM picks Sonnet (cap 0.85) → floor `d = 0.85`. Enabled candidates clearing the floor:
DeepSeek(0.90) ✓, Sonnet(0.85) ✓, Opus(0.97) ✓, Fable(0.99) ✓; MiniMax(0.70) ✗,
Haiku(0.60) ✗. `t = 0.5`, `β = 0.1`, all priority `0` (so the priority term is 0 here):

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
- `router.rs`:
  - Keep the LLM per-task pick (`route_batch` / `route_via_native_claude` largely unchanged —
    they still return a per-task picked model).
  - `select_model(picked, candidates, t, β) -> RouteChoice` — **replaces `apply_priority`**:
    `d = capability(picked)`; floor-filter `candidates` by `cap ≥ d − ε` (empty → most-capable);
    argmax the joint score; tie-break cheaper→stronger→registered.
  - `router_provider()` — **must also require `enabled`** (§11 fix): a flagged-but-disabled
    router provider is skipped, falling through to native-Claude-as-router. A disabled model
    can neither receive work nor keep spending the user's key as the router.
  - Keep the transport seam (`AskFn`) and pure/unit-tested split.
- `delegate.rs` — filter candidates by `enabled`; **guard the empty candidate set** (return
  `Err` from `run_delegate` before the `router_provider` match, §11); call the LLM pick then
  `select_model`; surface the decision in the run summary reason, e.g.
  `w1→deepseek (floor 0.85, S 0.70 > sonnet 0.675)`.
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
  floor filtering, empty-floor→most-capable fallback, tie-break ordering, knob extremes
  (`t=0`, `t=1`), disabled-exclusion, and the **`β` nudge bounds** — assert a `priority=1`
  model does NOT beat a model that leads it by `> β` on the main axis (guards §11 regression).
- `router_provider()`: a disabled flagged router is skipped (§11).
- Delegate flow: candidate filtering by `enabled`; **empty-set → `Err`, not native fallback**;
  router-fail → `d=0.5` degradation; summary reason string.
- Android: keep it light — a smoke test that `enabled=false` removes a card from the pool
  representation if such tests exist; otherwise manual.

## 9. Delivery
Two PRs (backend first, then Android), each via its repo's Codex-review auto-merge
workflow, with the delegate adversarial-verify pre-flight gate run on each diff.

## 10. Out of scope (YAGNI)
- Learned/trained routers (matrix factorization, BERT) — the LLM pick + deterministic
  joint scorer is enough for a BYOK self-host; revisit only if picks prove unreliable.
- Latency/throughput as a routing signal — not modeled today; add a third weight later if
  needed (the score is already a weighted sum, so it extends cleanly).
- Per-task tradeoff override — the knob is global for now.

## 11. Adversarial review outcomes (2026-07-12)

Three refutation workers reviewed this design before implementation. Four real flaws found;
all resolved above.

1. **`β=0.5` was a hard override, not a nudge** (scoring). priority=1 added +0.5 — half the
   entire cap+cost dynamic range — so a weak high-priority model beat a strong one for any
   `t`. **Fix:** `β=0.1`, documented `≪ 1`, with a unit test asserting priority can't overcome
   a `> β` lead on the main axis. Also noted the score scale ([0,1] main axis) so the summary
   reason compares apples-to-apples.
2. **Emitting a difficulty scalar didn't fix the bug and regressed** (floor). The DeepSeek
   pathology is purely a *tie-break* problem — solved by the joint score, not by changing the
   floor source. A bare difficulty scalar is unanchored (mis-estimate → crypto on Haiku / typo
   on Opus). **Fix:** reverted decision #1 — keep the LLM pick as a *committed* anchor,
   `floor = capability(pick)`; the joint score does the real work above it. Smaller diff too.
3. **"Empty candidate set → error" was contradicted by existing code** (state). Disabling
   everything would hit the silent native-default fallback at `delegate.rs:970` via an
   unguarded `route_via_native_claude`. **Fix:** explicit empty-set guard in `run_delegate`
   *before* the `router_provider` match, failing the delegate call.
4. **Disabling the router provider was unhandled** (state). `router_provider()` ignored
   `enabled`, so a toggled-off MiniMax would keep making every routing call (user's key/money)
   while unable to receive work. **Fix:** `router_provider()` requires `enabled`; a disabled
   flagged router falls through to native-Claude-as-router.
