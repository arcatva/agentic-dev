# Session prompt tiering — design

Date: 2026-07-10

## Problem

A session's injected guidance is scattered across two repos and two languages, with
overlapping/duplicated responsibilities and no clear rule for *which model sees what*:

- `ROUTING_GUIDE`, `WORKTREE_SETUP_GUIDE`, `build_session_guide` — Rust consts/fn in
  `agentic-dev` (`server-rs/src/engine/session_guide.rs`).
- `DEFAULT_CLAUDE_MD` — Kotlin const in `agentic-dev-android`
  (`.../ui/newrequest/NewRequestViewModel.kt`); the git/PR/conflict "Session workflow" text
  that pre-fills the New-request form and rides in as `meta.claude_md`.

All four are concatenated into one session-dir `CLAUDE.md` (project memory), so **every**
role that loads CLAUDE.md by cwd sees **all** of it. Two concrete defects:

1. `ROUTING_GUIDE` is overloaded — it mixes *which model to pick* (routing) with *how to
   structure a fan-out* (title/phase, compact prompts).
2. Routing/fan-out guidance is orchestrator-only, yet it lands in project memory that
   delegate workers and the native-Claude router spawn also load — pure noise for them
   (workers are denied `delegate`/`Workflow`/`Task`/`Agent`, so they never fan out).

## Roles and prompts (verified against code)

Four roles run a model; the "router" is one of them (an LLM), not a mechanism.

| Role | Model | Fans out? |
|---|---|---|
| Orchestrator (main session) | subscription main Claude | yes — only mounter of `delegate` |
| delegate workers | cheapest capable per task | no — `delegate/Workflow/Task/Agent` denied |
| router | flagged cheap provider (HTTP) **or** native Claude spawn | no |
| title generator | small model | no |

Routing = an LLM reads `build_route_prompt` and picks a model per un-pinned task, then a
deterministic `apply_priority` layer (priority, then cost) can override it; on any failure
the task runs on native Claude. Configured via a provider flagged `router:true` (keyed,
anthropic-protocol) or `AGENTIC_ROUTER_PROVIDER`; with none configured, un-pinned tasks run
on native Claude (not a cheap model).

Authored prompt segments: ① ROUTING_GUIDE ② FANOUT_GUIDE (new) ③ WORKTREE_SETUP_GUIDE
④ orientation ⑤ DEFAULT_CLAUDE_MD ⑥ TITLE ⑦ RETITLE ⑧ build_route_prompt (+⑨ router probe).

## Design — two tiers by authority + audience

**Tier 1 — harness operating rules → appended system prompt, main session only.**
`ROUTING_GUIDE` + `FANOUT_GUIDE`. Injected via `--append-system-prompt` (APPEND, never
replace — Claude Code's base prompt + tool instructions stay intact). Orchestrator-only:
authoritative (a repo CLAUDE.md must not be able to override "use delegate"), and correctly
invisible to workers/router (who can't fan out and don't need it).

**Tier 2 — project content → session `CLAUDE.md` (project memory, repo-overridable).**
`WORKTREE_SETUP_GUIDE` (build-env; advisory — a repo may ship its own setup — and needed by
whoever builds, so it stays cwd-loaded) + orientation + `DEFAULT_CLAUDE_MD` (git/PR
workflow; **a repo's own CLAUDE.md may override it — kept explicit**).

Title prompts (⑥⑦) and `build_route_prompt` (⑧) are separate roles, untouched.

`ROUTING_GUIDE` is split: it keeps *model routing* (use delegate, don't pin, no Workflow
fan-out); the new `FANOUT_GUIDE` holds *fan-out mechanics + discipline* (title/phase, compact
prompts, **cut non-overlapping boundaries**, **parallelize only independent work**, **verify
consequential output**).

## Changes

`agentic-dev` (Rust):
1. `session_guide.rs` — rewrite `ROUTING_GUIDE` to routing-only; add `FANOUT_GUIDE`; add
   `pub fn harness_rules() -> String` = `ROUTING_GUIDE` + `FANOUT_GUIDE`.
2. `spawner.rs` — add `SpawnOptions.append_system_prompt: Option<String>` (Default None).
3. `sdk_runner.rs` — when set + non-empty, set env `SDK_BRIDGE_APPEND_SYSTEM_PROMPT`.
4. `mod.rs` — both session-CLAUDE.md assembly sites: drop `ROUTING_GUIDE` from `sections`
   (keep `WORKTREE_SETUP_GUIDE` + orientation + custom). `spawn_opts` (main turn): set
   `append_system_prompt: Some(harness_rules())`. Worker spawns (`delegate.rs`) keep the
   Default `None`.
5. `sdk-bridge.mjs` — main query only (`!isWorker`, and title one-shot returns earlier):
   `extraArgs["append-system-prompt"] = SDK_BRIDGE_APPEND_SYSTEM_PROMPT` when set.
6. Tests: `harness_rules()` contains routing + fan-out markers; session CLAUDE.md no longer
   contains the routing header but still contains the build-env guide; `sdk_runner` wires the
   env when `append_system_prompt` is set and omits it otherwise.

`agentic-dev-android` (Kotlin):
7. `DEFAULT_CLAUDE_MD` — add one line stating a repo's own CLAUDE.md may override this
   workflow; matching assertion in `NewRequestViewModelTest`.

## Isolation guarantees

- Worker spawns use `..Default::default()` → `append_system_prompt = None`; plus the bridge's
  `!isWorker` gate double-ensures Tier-1 never reaches a worker.
- Native-Claude router spawn is worker-flagged and gets no append; it still loads Tier-2
  CLAUDE.md by cwd (pre-existing, harmless noise for a JSON routing reply) — out of scope.

## Rollout

Land per each repo's own CLAUDE.md PR workflow (adversarial `delegate` verify → PR → Codex →
`gh pr merge --rebase`). New sessions pick up Tier-1 after the backend is rebuilt/redeployed
(`make build`); a redeploy is a separate on-demand step.
