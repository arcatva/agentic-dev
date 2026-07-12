//! LLM-as-router: ask a cheap "router" model to pick the best registered model for each delegated
//! task, evaluating across the WHOLE BYOK catalog (N-way), not a binary cheap/strong toggle.
//!
//! Why an LLM and not a trained router (RouteLLM etc.): a trained binary router is the wrong shape
//! (strong-vs-weak, not N-way) and pulls a multi-GB torch stack. An LLM-as-router is N-way over the
//! exact registered catalog, needs no training data, no torch, and reuses the `description` /
//! `capability` / `price` metadata already on each `Provider`. One cheap call per delegate is noise
//! against a worker that then runs a multi-turn tool loop (up to `WORKER_TIMEOUT`).
//!
//! The pure pieces (prompt build + response parse + router selection) are unit-tested. The HTTP
//! transport is injectable (mirrors `engine::usage`), so tests never hit the network. On ANY failure
//! (no router configured, call/parse error) the caller runs the task on NATIVE Claude Code instead —
//! no provider overlay, no heuristic, no guessed cheap model.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::engine::delegate::DelegateTask;
use crate::engine::providers::{Protocol, Provider, ProviderRegistry};

/// Shared HTTP client (connection pool); one per process, like `engine::usage`.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// The router's decision for one task.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteChoice {
    /// The chosen model id (validated to exist in the catalog).
    pub model: String,
    /// A short human-readable reason, shown in the run summary.
    pub reason: String,
}

/// Transport seam: given the routing prompt, return the router model's raw text reply.
/// Prod passes `None` (real Anthropic `/v1/messages` call); tests pass a fake.
pub type AskFn = dyn Fn(&str) -> Result<String, String> + Send + Sync;

/// Pick the provider that runs the routing call: `AGENTIC_ROUTER_PROVIDER` by name if set and keyed,
/// else the cheapest **anthropic-protocol** provider that has a usable key. `None` disables routing
/// (the caller then uses the heuristic). `AGENTIC_ROUTER=off|0` force-disables it.
pub fn router_provider(reg: &ProviderRegistry) -> Option<&Provider> {
    if std::env::var("AGENTIC_ROUTER")
        .map(|v| v.eq_ignore_ascii_case("off") || v == "0")
        .unwrap_or(false)
    {
        return None;
    }
    // 1. A provider the user explicitly marked as the router (in the Providers UI) wins — but it must
    // be a keyed anthropic provider to actually run the routing call (we never raw-call an openai or a
    // keyless endpoint). If MULTIPLE providers are flagged, use the first ELIGIBLE one; if one or more
    // are flagged but NONE is eligible, DISABLE routing (None) rather than silently routing through a
    // DIFFERENT (unflagged or ineligible) provider — the same strict contract as AGENTIC_ROUTER_PROVIDER
    // below. Scanning past the first flagged provider is what lets a keyed-anthropic second choice win
    // over an ineligible (openai/keyless) first one.
    if reg.providers.iter().any(|p| p.router) {
        return reg.providers.iter().find(|p| {
            p.router && matches!(p.protocol, Protocol::Anthropic) && !p.resolved_key().is_empty()
        });
    }
    if let Ok(name) = std::env::var("AGENTIC_ROUTER_PROVIDER") {
        let name = name.trim();
        // Explicitly configured → respect it STRICTLY: if it's missing, keyless, or non-anthropic,
        // return None (disable routing) rather than silently routing through a DIFFERENT provider
        // (which could leak the prompt to an unintended endpoint or cost the user money).
        return reg.providers.iter().find(|p| {
            p.name.eq_ignore_ascii_case(name)
                && matches!(p.protocol, Protocol::Anthropic)
                && !p.resolved_key().is_empty()
        });
    }
    // Nothing is flagged as the router and there's no env override → do NOT silently promote a
    // third-party provider to router. Auto-routing would leak the prompt to that endpoint and cost
    // money without the user opting in. Return None: the delegate fan-out then runs every un-pinned
    // task on the native Claude main model (subscription). To enable routing, flag a keyed
    // anthropic provider as the router (Providers screen) or set AGENTIC_ROUTER_PROVIDER.
    None
}

/// Build the routing prompt: the catalog (each model's id / good_at / capability / price) + the
/// tasks to route (numbered by their ORIGINAL index, 1-based), asking for a strict JSON array.
pub fn build_route_prompt(
    tasks: &[DelegateTask],
    route_idxs: &[usize],
    candidates: &[&Provider],
) -> String {
    let catalog: Vec<serde_json::Value> = candidates
        .iter()
        .map(|p| {
            serde_json::json!({
                "model": p.model,
                "good_at": p.description.clone().unwrap_or_default(),
                "capability": p.capability,
                "priority": p.priority,
                "cost": p.cost,
            })
        })
        .collect();
    let mut tlines = String::new();
    for &i in route_idxs {
        if let Some(t) = tasks.get(i) {
            let preview: String = t.prompt.chars().take(400).collect();
            tlines.push_str(&format!("{}. {}\n", i + 1, preview.replace('\n', " ")));
        }
    }
    format!(
        "You are a model router for a coding agent's worker fan-out. For EACH task, choose the single \
         best model from the catalog. Optimize: pick a model that is capable enough, preferring higher \
         `priority`; among equally-prioritized models, prefer lower `cost`. Also prefer a model whose \
         \"good_at\" matches the task. `capability` is 0..1; `priority` is 0..1 (higher = prefer this \
         model); `cost` is 0..1 (lower = cheaper).\n\n\
         Catalog:\n{catalog}\n\n\
         Tasks:\n{tlines}\n\
         Respond with ONLY a JSON array, exactly one object per task, no prose and no markdown fences:\n\
         [{{\"task\": <task number>, \"model\": \"<exact model string from the catalog>\", \"reason\": \"<at most 8 words>\"}}]",
        catalog = serde_json::to_string_pretty(&catalog).unwrap_or_default(),
    )
}

/// Extract the first balanced JSON array from possibly-noisy model text (strips prose / code fences).
fn extract_json_array(text: &str) -> Option<Vec<serde_json::Value>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]).ok()
}

/// Parse the router reply into per-task choices. Tolerant: ignores entries for tasks we didn't ask
/// about, and drops any `model` that isn't actually in the catalog (so the caller falls back).
pub fn parse_route_response(
    text: &str,
    route_idxs: &[usize],
    candidates: &[&Provider],
) -> HashMap<usize, RouteChoice> {
    let mut out = HashMap::new();
    let Some(arr) = extract_json_array(text) else {
        return out;
    };
    for entry in arr {
        let Some(task_num) = entry.get("task").and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(idx) = (task_num as usize).checked_sub(1) else {
            continue;
        };
        if !route_idxs.contains(&idx) {
            continue;
        }
        // Trim + reject empty: an empty model would pass the substring check below
        // (`x.contains("")` is always true) and then match the first provider arbitrarily.
        let Some(model) = entry
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            continue;
        };
        // The model must resolve to a catalog entry — substring-tolerant (the LLM may abbreviate,
        // e.g. "MiniMax" for "MiniMax-M3"), via the shared `Provider::matches`.
        if !candidates.iter().any(|p| p.matches(model)) {
            continue;
        }
        let reason = entry
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        out.insert(
            idx,
            RouteChoice {
                model: model.to_string(),
                reason,
            },
        );
    }
    out
}

/// Apply provider PRIORITY (then COST as tiebreaker) deterministically on top of the LLM's per-task
/// pick. The LLM's chosen model sets the capability BAR (how hard it judged the task); among
/// candidates AT LEAST that capable, the highest `priority` wins — the user's explicit preference.
/// When priorities are tied, the lowest `cost` wins — cheaper is better. A mere tie keeps the LLM's
/// pick (the cheapest-sufficient one). This makes `priority` authoritative even when the cheap router
/// model ignores the "prefer higher priority" instruction and picks a strong model by reputation.
pub(crate) fn apply_priority(
    choices: HashMap<usize, RouteChoice>,
    candidates: &[&Provider],
) -> HashMap<usize, RouteChoice> {
    const EPS: f32 = 1e-4;
    choices
        .into_iter()
        .map(|(idx, choice)| {
            let Some(picked) =
                crate::engine::providers::resolve_candidate(candidates, &choice.model)
            else {
                return (idx, choice);
            };
            let bar = picked.capability;
            // `picked` is the initial best, so an equal-priority+equal-cost candidate never displaces
            // the LLM's (cheapest-sufficient) choice; only a STRICTLY higher priority (or equal
            // priority + strictly lower cost) at-least-as-capable wins.
            let mut best = picked;
            // Track whether we've already decided to override the LLM's pick.
            // While best == picked, use EPS thresholds so negligible differences
            // don't flip the LLM's choice.  Once overridden, use strict comparison
            // among alternatives so the true maximum-priority / minimum-cost
            // candidate wins regardless of iteration order.
            let mut overrode = false;
            for c in candidates.iter().copied() {
                if c.capability + EPS < bar {
                    continue;
                }
                if !overrode {
                    if c.priority > picked.priority + EPS {
                        best = c;
                        overrode = true;
                    } else if (c.priority - picked.priority).abs() < EPS
                        && c.cost < picked.cost - EPS
                    {
                        best = c;
                        overrode = true;
                    }
                } else {
                    if c.priority > best.priority
                        || ((c.priority - best.priority).abs() < EPS && c.cost < best.cost)
                    {
                        best = c;
                    }
                }
            }
            if best.name == picked.name {
                (idx, choice)
            } else {
                let detail = if choice.reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", choice.reason)
                };
                let kind = if (best.priority - picked.priority).abs() > EPS {
                    "priority"
                } else {
                    "cost"
                };
                let reason: String = format!("{kind} pick over {}{}", picked.model, detail)
                    .chars()
                    .take(80)
                    .collect();
                (
                    idx,
                    RouteChoice {
                        model: best.model.clone(),
                        reason,
                    },
                )
            }
        })
        .collect()
}

/// How close (on the MAIN-axis score) two models must be for `priority` to decide between them.
/// Priority is a BOUNDED near-tie nudge, not an additive term: an additive `β·priority` is not
/// scale-consistent — capability is weighted by `t` and cost by `1−t`, so a constant weight would
/// dominate whichever is down-weighted at extreme `t` (at low `t` a fixed β overrides an arbitrarily
/// large capability gap). A margin instead makes priority decide ONLY among candidates whose main
/// scores are within `PRIORITY_MARGIN`, so it can never override a meaningful quality/cost gap at
/// any `t` — a genuine nudge. `0.05` = "within 5% of the top on the [0,1] main axis".
pub(crate) const PRIORITY_MARGIN: f32 = 0.05;

/// Deterministic final model choice, replacing `apply_priority`. The LLM's `picked` model sets the
/// difficulty floor `d = capability(picked)` — a committed judgment, self-anchored (see the design
/// spec §3/§11). Among candidates AT LEAST that capable, the winner maximizes the MAIN score
///
///   `M(m) = (1 − t)·(1 − cost_m) + t·capability_m`     (`t`: 0 = cheapest .. 1 = strongest)
///
/// with `priority` acting as a bounded tiebreaker among the near-top band (`M ≥ max − PRIORITY_MARGIN`).
/// If NO candidate clears the floor (shouldn't happen — `picked` is itself a candidate — but guards a
/// caller that passes a synthetic pick), fall back to the single most-capable candidate. Within the
/// band the order is: higher priority → higher M → lower cost → higher capability → registered over
/// native → lower index (stable).
pub(crate) fn select_model(picked: &Provider, candidates: &[&Provider], t: f32) -> RouteChoice {
    const EPS: f32 = 1e-4;
    let d = picked.capability;
    let mut pool: Vec<&Provider> = candidates
        .iter()
        .copied()
        .filter(|c| c.capability + EPS >= d)
        .collect();
    if pool.is_empty() {
        if let Some(m) = candidates
            .iter()
            .copied()
            .max_by(|a, b| a.capability.total_cmp(&b.capability))
        {
            pool.push(m);
        }
    }
    let main = |m: &Provider| (1.0 - t) * (1.0 - m.cost) + t * m.capability;
    let best_main = pool.iter().map(|m| main(m)).fold(f32::MIN, f32::max);
    let is_native = crate::engine::providers::is_native;
    // Only the near-top band competes on priority; everything else is already beaten on M.
    let best = pool
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, m)| main(m) + PRIORITY_MARGIN + EPS >= best_main)
        .max_by(|(ia, a), (ib, b)| {
            a.priority
                .total_cmp(&b.priority) // higher priority wins the near-tie
                .then(main(a).total_cmp(&main(b))) // then higher main score
                .then(b.cost.total_cmp(&a.cost)) // then lower cost
                .then(a.capability.total_cmp(&b.capability)) // then higher capability
                .then(is_native(a).cmp(&is_native(b)).reverse()) // registered (false) beats native (true)
                .then(ib.cmp(ia)) // lower index wins (stable)
        })
        .map(|(_, m)| m)
        .unwrap_or(picked);
    // Honest reason: name the runner-up by MAIN score and state what decided. If `best` is also the
    // top-M candidate we print `≥`; if priority pulled a slightly-lower-M model up, we say so — never
    // a false `>`.
    let top_other = pool
        .iter()
        .copied()
        .filter(|m| m.name != best.name)
        .max_by(|a, b| main(a).total_cmp(&main(b)));
    let reason: String = match top_other {
        Some(r) if main(best) + EPS >= main(r) => {
            format!(
                "floor {:.2}, M {:.2} ≥ {} {:.2}",
                d,
                main(best),
                r.model,
                main(r)
            )
        }
        Some(r) => format!(
            "floor {:.2}, priority pick {} over {} (M {:.2}~{:.2})",
            d,
            best.model,
            r.model,
            main(best),
            main(r)
        ),
        None => format!("floor {:.2}, only candidate", d),
    }
    .chars()
    .take(80)
    .collect();
    RouteChoice {
        model: best.model.clone(),
        reason,
    }
}

/// Real transport: one Anthropic `/v1/messages` call to the router provider; returns the reply text.
async fn http_ask(router: &Provider, prompt: &str) -> Result<String, String> {
    let key = router.resolved_key();
    if key.is_empty() {
        return Err("router provider has no api key".into());
    }
    let url = format!("{}/v1/messages", router.base_url.trim_end_matches('/'));
    // The budget must cover a THINKING router's reasoning block PLUS the JSON answer. A reasoning
    // model (e.g. deepseek-v4-pro) emits a `thinking` content block first; with too small a budget it
    // spends the whole allowance thinking and is cut off (stop_reason=max_tokens) BEFORE any `text`
    // block — which the parser below then reads as "no text", failing every routing call. 512 was too
    // small for such models; 4096 leaves ample room for the (tiny) JSON reply after the thinking.
    let body = serde_json::json!({
        "model": router.model,
        "max_tokens": 4096,
        "messages": [{ "role": "user", "content": prompt }],
    });
    let res = HTTP_CLIENT
        .post(&url)
        // Official Anthropic endpoints authenticate with `x-api-key`; many third-party proxies use
        // `Authorization: Bearer`. Send both so the router works against either.
        .header("x-api-key", key.as_str())
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("router http {}", res.status().as_u16()));
    }
    let v: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    // Anthropic response shape: { content: [ { type: "text", text: "..." }, ... ] }. A `thinking`
    // block carries no `text` field, so it's skipped — only the model's actual answer is collected.
    let text = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        // Surface stop_reason so a truncated thinking model (stop_reason=max_tokens, thinking-only)
        // is diagnosable from the log instead of an opaque "no text".
        let stop = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("?");
        Err(format!("router returned no text (stop_reason={stop})"))
    } else {
        Ok(text)
    }
}

/// Probe a provider the user is configuring as the router: run one routing-style call and confirm it
/// returns a usable JSON-array reply. Catches a broken router (wrong model id, thinking-only
/// truncation, bad key/endpoint, or a non-anthropic protocol) at SET time — otherwise the misconfig
/// is invisible until it silently makes EVERY later delegate fan-out fall back to native Claude.
/// Transport is injectable (like `route_batch`): prod passes `None`, tests pass a fake.
pub async fn validate_router(router: &Provider, ask: Option<&AskFn>) -> Result<(), String> {
    if !matches!(router.protocol, Protocol::Anthropic) {
        return Err("router must be an anthropic-protocol provider".into());
    }
    if router.resolved_key().is_empty() {
        return Err("router provider has no api key".into());
    }
    let prompt = "You are a model router. Respond with ONLY this JSON array and nothing else: \
                  [{\"task\": 1, \"model\": \"probe\", \"reason\": \"ok\"}]";
    let text = match ask {
        Some(f) => f(prompt),
        None => http_ask(router, prompt).await,
    }?;
    if extract_json_array(&text).is_some() {
        Ok(())
    } else {
        Err(format!(
            "router replied without a usable JSON array: {}",
            text.chars().take(160).collect::<String>()
        ))
    }
}

/// Route every task that has NO explicit model hint, in ONE router call. Returns a map of
/// original-task-index → choice; tasks not present in the map (explicit, unrouted, or failed) are
/// left for the caller's heuristic fallback. Never errors — any failure yields an empty map.
///
/// `ask` is the transport seam: `None` → the real HTTP call; `Some(f)` → a test fake.
pub async fn route_batch(
    tasks: &[DelegateTask],
    candidates: &[&Provider],
    router: &Provider,
    ask: Option<&AskFn>,
) -> HashMap<usize, RouteChoice> {
    // Only auto-route tasks without an explicit model; with <2 candidates there's nothing to pick.
    let route_idxs: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.model.as_deref().unwrap_or("").trim().is_empty())
        .map(|(i, _)| i)
        .collect();
    if route_idxs.is_empty() || candidates.is_empty() {
        return HashMap::new();
    }
    // A single candidate means there is no routing DECISION to make — assign every un-pinned task to
    // it directly, with no LLM call. (Falling through to "native Claude" here would ignore the one
    // cheap model the user registered.)
    if candidates.len() == 1 {
        let only = candidates[0];
        return route_idxs
            .into_iter()
            .map(|i| {
                (
                    i,
                    RouteChoice {
                        model: only.model.clone(),
                        reason: "only registered model".into(),
                    },
                )
            })
            .collect();
    }
    let prompt = build_route_prompt(tasks, &route_idxs, candidates);
    let text = match ask {
        Some(f) => f(&prompt),
        None => http_ask(router, &prompt).await,
    };
    match text {
        Ok(t) => apply_priority(
            parse_route_response(&t, &route_idxs, candidates),
            candidates,
        ),
        Err(e) => {
            tracing::warn!("[router] routing call failed; task(s) fall back to native Claude: {e}");
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(
        name: &str,
        model: &str,
        cap: f32,
        priority: f32,
        cost: f32,
        proto: Protocol,
        key: &str,
    ) -> Provider {
        Provider {
            name: name.into(),
            base_url: "https://x/anthropic".into(),
            api_key: key.into(),
            api_key_env: None,
            model: model.into(),
            protocol: proto,
            capability: cap,
            description: Some(format!("{name} desc")),
            priority,
            cost,
            router: false,
            enabled: true,
        }
    }

    fn reg() -> ProviderRegistry {
        ProviderRegistry {
            providers: vec![
                p(
                    "minimax",
                    "MiniMax-M3",
                    0.5,
                    0.3,
                    0.3,
                    Protocol::Anthropic,
                    "mk",
                ),
                p(
                    "deepseek",
                    "deepseek-chat",
                    0.6,
                    0.5,
                    0.5,
                    Protocol::Anthropic,
                    "dk",
                ),
                p("gpt", "gpt-4o-mini", 0.7, 1.0, 0.7, Protocol::Openai, "gk"),
            ],
        }
    }

    fn tasks() -> Vec<DelegateTask> {
        vec![
            DelegateTask {
                prompt: "grep for TODO".into(),
                role: "explorer".into(),
                model: None,
                phase: None,
                write: false,
            },
            DelegateTask {
                prompt: "refactor the auth module".into(),
                role: "coder".into(),
                model: Some("deepseek".into()),
                phase: None,
                write: false,
            },
            DelegateTask {
                prompt: "summarize the long thread".into(),
                role: "summarizer".into(),
                model: None,
                phase: None,
                write: false,
            },
        ]
    }

    // ── select_model (joint scorer, replaces apply_priority) ──

    #[test]
    fn select_model_prefers_more_capable_at_equal_cost_priority() {
        // The tie pathology the redesign fixes: an equally-priced, equally-prioritized, MORE capable
        // model (deepseek 0.90) must beat the LLM's pick (sonnet-like 0.85). Lexicographic discarded
        // the above-floor 0.90 vs 0.85 difference; the joint score keeps it.
        let strong = p(
            "deepseek",
            "deepseek-v4-pro",
            0.90,
            0.5,
            0.50,
            Protocol::Anthropic,
            "k",
        );
        let weak = p(
            "sonnetlike",
            "sonnet-ish",
            0.85,
            0.5,
            0.50,
            Protocol::Anthropic,
            "k",
        );
        let cat = vec![strong, weak.clone()];
        let cands: Vec<&Provider> = cat.iter().collect();
        let got = select_model(&weak, &cands, 0.5);
        assert_eq!(got.model, "deepseek-v4-pro");
        assert!(got.reason.contains("floor 0.85"), "reason: {}", got.reason);
    }

    #[test]
    fn priority_is_a_bounded_near_tie_nudge_not_an_override() {
        // A leads B by 0.30 on capability at equal cost; B has priority 1. At t=0.5 that lead is
        // 0.15 on the main axis — WELL outside PRIORITY_MARGIN (0.05) — so B is not even in the
        // near-tie band and priority cannot pull it up. Strong A wins. (Unlike an additive β term,
        // this holds because the margin, not a constant weight, bounds priority's reach.)
        let a = p("a", "a", 0.85, 0.0, 0.50, Protocol::Anthropic, "k");
        let b = p("b", "b", 0.55, 1.0, 0.50, Protocol::Anthropic, "k");
        let cat = vec![a, b.clone()];
        let cands: Vec<&Provider> = cat.iter().collect();
        // pick = b so the floor (0.55) lets both through; strong A must still win.
        assert_eq!(select_model(&b, &cands, 0.5).model, "a");
    }

    #[test]
    fn priority_decides_within_the_near_tie_band() {
        // Two models within PRIORITY_MARGIN on the main axis (cap 0.86 vs 0.85, equal cost) → the
        // higher-priority one wins. This is the "persistent nudge": among near-equals, priority is
        // decisive; it just cannot override a gap bigger than the margin (see the test above).
        let hi_pri = p("hipri", "hipri", 0.85, 1.0, 0.50, Protocol::Anthropic, "k");
        let hi_cap = p("hicap", "hicap", 0.86, 0.0, 0.50, Protocol::Anthropic, "k");
        let cat = vec![hi_cap, hi_pri.clone()];
        let cands: Vec<&Provider> = cat.iter().collect();
        // pick = hi_pri (floor 0.85, both clear). hi_cap leads main by only 0.005 (t=0.5) → in band
        // → priority breaks it for hi_pri.
        let got = select_model(&hi_pri, &cands, 0.5);
        assert_eq!(got.model, "hipri");
        assert!(
            got.reason.contains("priority pick"),
            "reason should name priority as the basis: {}",
            got.reason
        );
    }

    #[test]
    fn user_live_config_deepseek_beats_native_sonnet() {
        // Regression for the reported bug: registered deepseek (0.90/prio0/0.50) must beat native
        // sonnet (0.85/prio0/0.50) when the LLM picked sonnet — the exact live config. Native opus/
        // fable stay at their default priority 0.5 but are out of the near-tie band on cost/cap.
        crate::engine::providers::seed_claude_models_for_tests();
        use crate::engine::native_overrides::{NativeOverride, OverrideMap};
        let mut ov = OverrideMap::new();
        ov.insert(
            "sonnet".into(),
            NativeOverride {
                capability: 0.85,
                priority: 0.0,
                cost: 0.5,
                description: String::new(),
                enabled: true,
            },
        );
        let ds = p(
            "deepseek",
            "deepseek-v4-pro",
            0.90,
            0.0,
            0.50,
            Protocol::Anthropic,
            "k",
        );
        let mut cat = vec![ds];
        cat.extend(crate::engine::providers::native_claude_candidates(&ov));
        let cands: Vec<&Provider> = cat.iter().collect();
        let sonnet = cands
            .iter()
            .copied()
            .find(|c| c.model.contains("sonnet"))
            .expect("native sonnet present");
        let got = select_model(sonnet, &cands, 0.5);
        assert_eq!(got.model, "deepseek-v4-pro", "reason: {}", got.reason);
    }

    #[test]
    fn knob_extremes_move_cheaper_to_stronger() {
        let cheap = p("cheap", "cheap", 0.85, 0.0, 0.20, Protocol::Anthropic, "k");
        let strong = p(
            "strong",
            "strong",
            0.97,
            0.0,
            0.90,
            Protocol::Anthropic,
            "k",
        );
        let cat = vec![cheap.clone(), strong];
        let cands: Vec<&Provider> = cat.iter().collect();
        // pick = cheap → floor 0.85, both clear it. t=0 → cheapest; t=1 → strongest.
        assert_eq!(select_model(&cheap, &cands, 0.0).model, "cheap");
        assert_eq!(select_model(&cheap, &cands, 1.0).model, "strong");
    }

    #[test]
    fn empty_floor_falls_back_to_most_capable() {
        // A synthetic pick more capable than every candidate empties the floor → most-capable wins,
        // so a hard task still runs on the best available model instead of nothing.
        let a = p("a", "a", 0.50, 0.0, 0.10, Protocol::Anthropic, "k");
        let b = p("b", "b", 0.70, 0.0, 0.90, Protocol::Anthropic, "k");
        let phantom = p("x", "x", 0.99, 0.0, 0.50, Protocol::Anthropic, "k");
        let cat = vec![a, b];
        let cands: Vec<&Provider> = cat.iter().collect();
        assert_eq!(select_model(&phantom, &cands, 0.5).model, "b");
    }

    #[test]
    fn router_provider_none_when_nothing_is_flagged() {
        // No provider is flagged as the router (and no env override) → None, so the delegate fan-out
        // falls back to the native Claude main model instead of silently promoting a third-party.
        let r = reg();
        assert!(router_provider(&r).is_none());
    }

    #[test]
    fn router_provider_prefers_the_router_flagged_provider() {
        // minimax has LOWER priority than deepseek, but is explicitly flagged as the router → it wins.
        let mut r = reg();
        r.providers[0].router = true; // minimax
        assert_eq!(router_provider(&r).unwrap().name, "minimax");
    }

    #[test]
    fn router_provider_flagged_but_ineligible_disables_routing() {
        // A provider flagged as the router but ineligible (openai protocol) → routing is DISABLED
        // (None), NOT a silent fallback to a different provider.
        let mut r = reg();
        r.providers[2].router = true; // gpt — openai protocol → ineligible as a router
        assert!(router_provider(&r).is_none());
    }

    #[test]
    fn router_provider_picks_first_eligible_among_multiple_flagged() {
        // TWO providers flagged as the router; the first (openai) is ineligible, the second
        // (keyed anthropic) is eligible → the eligible one wins instead of routing being disabled.
        let mut r = ProviderRegistry {
            providers: vec![
                p("gpt", "gpt-4o-mini", 0.7, 0.5, 0.7, Protocol::Openai, "gk"),
                p(
                    "deepseek",
                    "deepseek-chat",
                    0.6,
                    0.5,
                    0.5,
                    Protocol::Anthropic,
                    "dk",
                ),
            ],
        };
        r.providers[0].router = true;
        r.providers[1].router = true;
        assert_eq!(router_provider(&r).unwrap().name, "deepseek");
    }

    #[tokio::test]
    async fn priority_overrides_the_llm_pick_when_capable_enough() {
        crate::engine::providers::seed_claude_models_for_tests();
        // The router (LLM) picks "opus" for a hard task, but a registered model that is at least as
        // capable AND higher-priority must win deterministically.
        let mk = |cap: f32| {
            let mut cat = vec![p(
                "minimax",
                "MiniMax-M3",
                cap,
                1.0,
                0.3,
                Protocol::Anthropic,
                "mk",
            )];
            cat.extend(crate::engine::providers::native_claude_candidates(
                &Default::default(),
            ));
            cat
        };
        let router = p(
            "minimax",
            "MiniMax-M3",
            1.0,
            1.0,
            0.3,
            Protocol::Anthropic,
            "mk",
        );
        let ts = vec![DelegateTask {
            prompt: "explore the architecture".into(),
            role: "explorer".into(),
            model: None,
            phase: None,
            write: false,
        }];
        let pick_opus = |_: &str| -> Result<String, String> {
            Ok(r#"[{"task":1,"model":"opus","reason":"deep exploration"}]"#.into())
        };

        // minimax capability 1.0 (≥ opus's 0.97) + priority 1.0 (> native 0.5) → overrides opus.
        let cat = mk(1.0);
        let cands: Vec<&Provider> = cat.iter().collect();
        let got = route_batch(&ts, &cands, &router, Some(&pick_opus)).await;
        assert_eq!(
            got.get(&0).unwrap().model,
            "MiniMax-M3",
            "priority must override the LLM's opus pick"
        );

        // minimax capability 0.5 (< opus's 0.97) → NOT capable enough → opus stays.
        let cat2 = mk(0.5);
        let cands2: Vec<&Provider> = cat2.iter().collect();
        let got2 = route_batch(&ts, &cands2, &router, Some(&pick_opus)).await;
        assert_eq!(
            got2.get(&0).unwrap().model,
            "opus",
            "a less-capable model must not override on priority alone"
        );
    }

    #[tokio::test]
    async fn cost_tiebreaker_prefers_cheaper_when_priorities_are_tied() {
        // Three equally-capable models with identical priority but different cost. The router (LLM)
        // picks the expensive one, but cost tiebreaker overrides it with the cheapest eligible model.
        let cat = vec![
            p(
                "cheap",
                "cheap-model",
                0.8,
                0.5,
                0.1,
                Protocol::Anthropic,
                "k",
            ),
            p("mid", "mid-model", 0.8, 0.5, 0.5, Protocol::Anthropic, "k"),
            p(
                "expensive",
                "expensive-model",
                0.8,
                0.5,
                0.9,
                Protocol::Anthropic,
                "k",
            ),
        ];
        let cands: Vec<&Provider> = cat.iter().collect();
        let ts = vec![DelegateTask {
            prompt: "a typical task".into(),
            role: "worker".into(),
            model: None,
            phase: None,
            write: false,
        }];
        let router = p("mid", "mid-model", 0.5, 0.5, 0.5, Protocol::Anthropic, "k");
        // The LLM routes to "expensive-model" but cost tiebreaker should switch to the cheapest.
        let fake = |_: &str| -> Result<String, String> {
            Ok(r#"[{"task":1,"model":"expensive-model","reason":"looks good"}]"#.into())
        };
        let got = route_batch(&ts, &cands, &router, Some(&fake)).await;
        assert_eq!(
            got.get(&0).unwrap().model,
            "cheap-model",
            "cost tiebreaker must prefer cheapest when priorities are tied"
        );
        assert!(
            got[&0].reason.contains("cost"),
            "reason must mention 'cost': {}",
            got[&0].reason
        );

        // When a higher-priority model exists, it wins regardless of cost.
        let cat2 = vec![
            p(
                "cheap",
                "cheap-model",
                0.8,
                0.3,
                0.1,
                Protocol::Anthropic,
                "k",
            ),
            p(
                "expensive",
                "expensive-model",
                0.8,
                0.9,
                0.9,
                Protocol::Anthropic,
                "k",
            ),
        ];
        let cands2: Vec<&Provider> = cat2.iter().collect();
        let fake2 = |_: &str| -> Result<String, String> {
            Ok(r#"[{"task":1,"model":"cheap-model","reason":"cheap"}]"#.into())
        };
        let got2 = route_batch(&ts, &cands2, &router, Some(&fake2)).await;
        assert_eq!(
            got2.get(&0).unwrap().model,
            "expensive-model",
            "higher priority must win over cheaper cost"
        );
        assert!(
            got2[&0].reason.contains("priority"),
            "reason must mention 'priority': {}",
            got2[&0].reason
        );
    }

    #[test]
    fn router_provider_skips_openai_protocol_and_keyless() {
        // Only an openai-protocol provider and a keyless anthropic one → no eligible router.
        let r = ProviderRegistry {
            providers: vec![
                p("gpt", "gpt-4o-mini", 0.7, 0.1, 0.7, Protocol::Openai, "gk"),
                p("nokey", "m", 0.5, 0.2, 0.5, Protocol::Anthropic, ""),
            ],
        };
        assert!(router_provider(&r).is_none());
    }

    #[test]
    fn prompt_lists_catalog_and_only_unrouted_tasks() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        let ts = tasks();
        // tasks 0 and 2 have no explicit model; task 1 ("deepseek") is pinned.
        let prompt = build_route_prompt(&ts, &[0, 2], &cands);
        assert!(prompt.contains("MiniMax-M3") && prompt.contains("deepseek-chat"));
        assert!(prompt.contains("1. grep for TODO"));
        assert!(prompt.contains("3. summarize the long thread"));
        assert!(
            !prompt.contains("2. refactor"),
            "pinned task must not be offered for routing"
        );
    }

    #[test]
    fn parse_handles_clean_array_and_validates_models() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        let text = r#"[{"task":1,"model":"MiniMax-M3","reason":"cheap lookup"},
                       {"task":3,"model":"deepseek-chat","reason":"reasoning"},
                       {"task":9,"model":"MiniMax-M3","reason":"ignored - not asked"}]"#;
        let got = parse_route_response(text, &[0, 2], &cands);
        assert_eq!(got.len(), 2);
        assert_eq!(got[&0].model, "MiniMax-M3");
        assert_eq!(got[&0].reason, "cheap lookup");
        assert_eq!(got[&2].model, "deepseek-chat");
        assert!(!got.contains_key(&8), "task 9 wasn't requested → dropped");
    }

    #[test]
    fn parse_strips_prose_and_drops_unknown_models() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        let text = "Sure! Here is the routing:\n```json\n[{\"task\":1,\"model\":\"some-unregistered-model\",\"reason\":\"x\"},{\"task\":3,\"model\":\"deepseek\",\"reason\":\"ok\"}]\n```\nDone.";
        let got = parse_route_response(text, &[0, 2], &cands);
        // unknown model dropped; "deepseek" matches by provider NAME.
        assert_eq!(got.len(), 1);
        assert_eq!(got[&2].model, "deepseek");
    }

    #[test]
    fn parse_accepts_abbreviated_model_names() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        // LLM abbreviates "MiniMax-M3" → "MiniMax"; substring-tolerant matching still accepts it
        // (run_delegate's registry.find later resolves the abbreviation to the real provider).
        let got = parse_route_response(
            r#"[{"task":1,"model":"MiniMax","reason":"x"}]"#,
            &[0],
            &cands,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[&0].model, "MiniMax");
    }

    #[test]
    fn parse_drops_empty_or_whitespace_model_names() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        // An empty / whitespace model must be rejected (it would otherwise match the first provider
        // via the always-true `contains("")` and silently mis-route).
        let text = r#"[{"task":1,"model":"   ","reason":"x"},{"task":3,"model":"","reason":"y"}]"#;
        assert!(parse_route_response(text, &[0, 2], &cands).is_empty());
    }

    #[test]
    fn parse_returns_empty_on_garbage() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        assert!(parse_route_response("no json here", &[0], &cands).is_empty());
        assert!(parse_route_response("", &[0], &cands).is_empty());
    }

    #[tokio::test]
    async fn route_batch_uses_fake_transport_and_skips_pinned() {
        // EQUAL-priority anthropic candidates (production filters openai out before route_batch), so
        // this test isolates transport + pinned-skipping; the priority layer is covered separately by
        // priority_overrides_the_llm_pick_when_capable_enough.
        let cat = vec![
            p(
                "minimax",
                "MiniMax-M3",
                0.5,
                0.5,
                0.3,
                Protocol::Anthropic,
                "mk",
            ),
            p(
                "deepseek",
                "deepseek-chat",
                0.6,
                0.5,
                0.5,
                Protocol::Anthropic,
                "dk",
            ),
        ];
        let cands: Vec<&Provider> = cat.iter().collect();
        let ts = tasks();
        let router = p(
            "minimax",
            "MiniMax-M3",
            0.5,
            0.5,
            0.3,
            Protocol::Anthropic,
            "mk",
        );
        let fake = |prompt: &str| -> Result<String, String> {
            assert!(prompt.contains("grep for TODO"));
            Ok(r#"[{"task":1,"model":"MiniMax-M3","reason":"cheap"},{"task":3,"model":"deepseek-chat","reason":"reasoning"}]"#.to_string())
        };
        let got = route_batch(&ts, &cands, &router, Some(&fake)).await;
        // task 1 (idx 0) and task 3 (idx 2) routed; pinned task 2 (idx 1) absent.
        assert_eq!(got.len(), 2);
        assert_eq!(got[&0].model, "MiniMax-M3");
        assert_eq!(got[&2].model, "deepseek-chat");
        assert!(!got.contains_key(&1));
    }

    #[tokio::test]
    async fn route_batch_can_pick_a_native_claude_model() {
        crate::engine::providers::seed_claude_models_for_tests();
        // catalog = one registered cheap model + the native Claude tiers; the router picks Claude.
        let mut cat = vec![p(
            "minimax",
            "MiniMax-M3",
            0.5,
            0.3,
            0.3,
            Protocol::Anthropic,
            "mk",
        )];
        cat.extend(crate::engine::providers::native_claude_candidates(
            &Default::default(),
        ));
        let cands: Vec<&Provider> = cat.iter().collect();
        let ts = vec![DelegateTask {
            prompt: "a hard architecture/reasoning task".into(),
            role: "".into(),
            model: None,
            phase: None,
            write: false,
        }];
        let router = p(
            "minimax",
            "MiniMax-M3",
            0.5,
            0.3,
            0.3,
            Protocol::Anthropic,
            "mk",
        );
        let fake = |_: &str| -> Result<String, String> {
            Ok(r#"[{"task":1,"model":"sonnet","reason":"needs strong reasoning"}]"#.to_string())
        };
        let got = route_batch(&ts, &cands, &router, Some(&fake)).await;
        // "sonnet" validates against the native candidate (model "sonnet"), so the task is routed.
        assert_eq!(got.len(), 1);
        assert_eq!(got[&0].model, "sonnet");
    }

    #[tokio::test]
    async fn route_batch_empty_on_transport_error() {
        let r = reg();
        let cands: Vec<&Provider> = r.providers.iter().collect();
        let ts = tasks();
        let router = p(
            "minimax",
            "MiniMax-M3",
            0.5,
            0.3,
            0.3,
            Protocol::Anthropic,
            "mk",
        );
        let fail = |_: &str| -> Result<String, String> { Err("boom".into()) };
        assert!(route_batch(&ts, &cands, &router, Some(&fail))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn route_batch_single_candidate_assigns_it_without_calling_the_router() {
        let one = ProviderRegistry {
            providers: vec![p(
                "minimax",
                "MiniMax-M3",
                0.5,
                0.3,
                0.3,
                Protocol::Anthropic,
                "mk",
            )],
        };
        let cands: Vec<&Provider> = one.providers.iter().collect();
        let ts = tasks(); // idx 0 & 2 un-pinned, idx 1 pinned to "deepseek"
        let router = one.providers[0].clone();
        // AskFn is `'static`, so own the flag via an Arc rather than borrowing a local.
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let fake = move |_: &str| -> Result<String, String> {
            c.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("[]".into())
        };
        let got = route_batch(&ts, &cands, &router, Some(&fake)).await;
        // The single registered model is assigned to BOTH un-pinned tasks, with no LLM call.
        assert_eq!(got.len(), 2);
        assert_eq!(got[&0].model, "MiniMax-M3");
        assert_eq!(got[&2].model, "MiniMax-M3");
        assert!(!got.contains_key(&1), "pinned task is left alone");
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "must not call the router with a single candidate"
        );
    }

    #[tokio::test]
    async fn validate_router_accepts_a_usable_json_array_reply() {
        let router = p("rtr", "m", 0.5, 0.5, 0.5, Protocol::Anthropic, "k");
        // Prose around the array (e.g. a thinking model's preamble) is fine — extract_json_array digs
        // the array out, exactly as the real routing path does.
        let ok = |_: &str| -> Result<String, String> {
            Ok("sure: [{\"task\":1,\"model\":\"probe\",\"reason\":\"ok\"}]".into())
        };
        assert!(validate_router(&router, Some(&ok)).await.is_ok());
    }

    #[tokio::test]
    async fn validate_router_rejects_no_array_and_transport_error() {
        let router = p("rtr", "m", 0.5, 0.5, 0.5, Protocol::Anthropic, "k");
        // A reply with no JSON array (e.g. a thinking-only truncation that yielded only prose).
        let no_array =
            |_: &str| -> Result<String, String> { Ok("thought hard, produced no array".into()) };
        assert!(validate_router(&router, Some(&no_array)).await.is_err());
        // A transport error (e.g. http_ask's "router returned no text (stop_reason=max_tokens)") propagates.
        let boom = |_: &str| -> Result<String, String> {
            Err("router returned no text (stop_reason=max_tokens)".into())
        };
        assert!(validate_router(&router, Some(&boom)).await.is_err());
    }

    #[tokio::test]
    async fn validate_router_rejects_non_anthropic_or_keyless_without_transport() {
        // Protocol + key are checked BEFORE the transport, so `None` (real HTTP) is never reached —
        // these assertions are network-free and deterministic.
        let openai = p("rtr", "m", 0.5, 0.5, 0.5, Protocol::Openai, "k");
        assert!(validate_router(&openai, None).await.is_err());
        let keyless = p("rtr", "m", 0.5, 0.5, 0.5, Protocol::Anthropic, "");
        assert!(validate_router(&keyless, None).await.is_err());
    }
}
