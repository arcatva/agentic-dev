# Remove Legacy TS Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete the legacy TypeScript backend (`server/`) and everything that points at it, leaving the repo Rust-only and internally consistent (build, deploy, docs, hooks all reference `server-rs/`).

**Architecture:** `server-rs/` (Rust, axum/tokio) already replaced `server/` (TS, Fastify) and passed cutover validation (`docs/superpowers/runbooks/2026-06-21-rust-backend-cutover-runbook.md`). The only TS→Rust coupling is the Rust **test** suite reading `server/test/fixtures/*.sh`. Per the spec, those fake-claude shell fixtures are **relocated** into `server-rs/tests/fixtures/` (all 259 tests preserved), then `server/` and the TS toolchain are removed, then deploy/docs/hooks are rewired and parity comments scrubbed.

**Tech Stack:** Rust (cargo), systemd user unit, bash, markdown. Per-turn claude transport is the Node bridge `server-rs/sdk-bridge.mjs` using `@anthropic-ai/claude-agent-sdk`.

**Spec:** `docs/superpowers/specs/2026-06-22-remove-legacy-ts-backend-design.md`

## Global Constraints

- Work in `~/src/agentic-dev` (this worktree). Never touch the Android repo.
- `cargo build` / `cargo test` are run **inside `server-rs/`**.
- Production Rust must not depend on `server/`. After Task 1, no Rust file references `../server/test/fixtures` (the fixtures now live under `server-rs/tests/fixtures/`, referenced as `tests/fixtures`).
- RELOCATE the fixtures (do NOT delete tests). All 259 tests must still pass.
- Commit after each task. Branch: current `agentic/<session>` worktree branch (do not switch to master).
- Final state: `cargo test` green (259 tests, zero warnings that fail the build), no live references to `server/` (TS), `tsx`, `yarn`, `vitest`, `tsconfig`. `server-rs/tests/fixtures/` is a legitimate path, not a leftover.

---

### Task 1: Relocate the fake-claude fixtures into server-rs

**Files:**
- Create (via `git mv`): `server-rs/tests/fixtures/*.sh` (10 files)
- Remove (via the same `git mv`): `server/test/fixtures/*.sh`
- Modify: `server-rs/src/api/test_support.rs` (fixture base path)
- Modify: `server-rs/src/engine/tests.rs` (fixture base path)
- Modify: `server-rs/src/engine/spawner.rs` (fixture base path)
- Modify: `server-rs/src/engine/runner.rs` (fixture base path)

**Interfaces:**
- Consumes: nothing.
- Produces: a self-contained Rust test suite — every fixture path resolves to `server-rs/tests/fixtures/`, no reference to `../server/test/fixtures`. All 259 tests still pass.

- [ ] **Step 1: Confirm the current baseline is green**

Run: `cd server-rs && cargo test 2>&1 | tail -5`
Expected: all tests pass (259 = 257 lib + 2 smoke). If not, stop and report — do not proceed on a red baseline.

- [ ] **Step 2: `git mv` the 10 fixtures into server-rs**

```bash
cd ~/src/agentic-dev
mkdir -p server-rs/tests/fixtures
git mv server/test/fixtures/fake-claude.sh \
       server/test/fixtures/fake-claude-agentresult.sh \
       server/test/fixtures/fake-claude-crash.sh \
       server/test/fixtures/fake-claude-echoargs.sh \
       server/test/fixtures/fake-claude-echoenv.sh \
       server/test/fixtures/fake-claude-error.sh \
       server/test/fixtures/fake-claude-noinit.sh \
       server/test/fixtures/fake-claude-ratelimit.sh \
       server/test/fixtures/fake-claude-stream.sh \
       server/test/fixtures/fake-claude-stream-crash2.sh \
       server-rs/tests/fixtures/
```
(Leave `server/test/fixtures/gitrepo.ts` in place — it is TS-only and is removed with `server/` in Task 2.)

- [ ] **Step 3: Verify the move (count + executable bit + only gitrepo.ts left behind)**

Run:
```bash
ls server-rs/tests/fixtures/*.sh | wc -l                          # expect 10
find server-rs/tests/fixtures -name '*.sh' ! -perm -u+x           # expect NO output (all executable)
ls server/test/fixtures/                                          # expect only: gitrepo.ts
```
Expected: 10 scripts present and executable; only `gitrepo.ts` remains in the old dir. If any `.sh` lost `+x`, run `chmod +x server-rs/tests/fixtures/*.sh`.

- [ ] **Step 4: Repoint the 4 fixture-path helpers**

In each file below, change ONLY the `.join(...)` argument from `../server/test/fixtures` to `tests/fixtures` (leave the rest of each helper unchanged). `CARGO_MANIFEST_DIR` is `server-rs/`, so `tests/fixtures` resolves to `server-rs/tests/fixtures` where the scripts now live.
- `server-rs/src/api/test_support.rs` (~line 68): `.join("../server/test/fixtures")` → `.join("tests/fixtures")`
- `server-rs/src/engine/spawner.rs` (~line 340): `.join("../server/test/fixtures")` → `.join("tests/fixtures")`
- `server-rs/src/engine/runner.rs` (~line 193): `.join("../server/test/fixtures")` → `.join("tests/fixtures")`
- `server-rs/src/engine/tests.rs` (~line 20): `.join("../server/test/fixtures")` → `.join("tests/fixtures")`

- [ ] **Step 5: Verify no Rust file references the old path**

Run: `grep -rn "server/test/fixtures" server-rs/`
Expected: no output (all four now read `tests/fixtures`).

- [ ] **Step 6: Run the full suite**

Run: `cd server-rs && cargo test 2>&1 | tail -8`
Expected: PASS, 259 tests (257 lib + 2 smoke), 0 failed. (A failure mentioning "No such file" means a fixture didn't move or a path is wrong — fix and re-run.)

- [ ] **Step 7: Commit**

```bash
cd ~/src/agentic-dev
git add -A
git commit -m "test(rs): relocate fake-claude fixtures into server-rs/tests/fixtures"
```

---

### Task 2: Delete the legacy TS backend tree and toolchain

**Files:**
- Delete: `server/` (entire directory)
- Delete: `tsconfig.json`, `vitest.config.ts`, `package.json`, `yarn.lock`, `scripts/dev.sh`

**Interfaces:**
- Consumes: Task 1 (Rust tests no longer need `server/test/fixtures`).
- Produces: a repo with no TS backend and no root Node project. `server-rs/` builds and tests unchanged.

- [ ] **Step 1: Remove the TS tree and toolchain from git**

```bash
cd ~/src/agentic-dev
git rm -r server
git rm tsconfig.json vitest.config.ts package.json yarn.lock scripts/dev.sh
rmdir scripts 2>/dev/null || true   # remove scripts/ if now empty
```

- [ ] **Step 2: Verify the Rust build is unaffected**

Run: `cd server-rs && cargo build 2>&1 | tail -3`
Expected: builds successfully (no reference to the deleted tree).

- [ ] **Step 3: Verify the Rust tests still pass**

Run: `cd server-rs && cargo test 2>&1 | tail -8`
Expected: PASS, ~230 tests, 0 failed.

- [ ] **Step 4: Confirm the directory and files are gone**

Run: `ls server 2>&1; ls tsconfig.json vitest.config.ts package.json yarn.lock 2>&1`
Expected: "No such file or directory" for each.

- [ ] **Step 5: Commit**

```bash
cd ~/src/agentic-dev
git add -A
git commit -m "chore: delete legacy TS backend (server/) and its Node toolchain"
```

---

### Task 3: Rewire deploy unit + READMEs to the Rust backend

**Files:**
- Modify: `deploy/agentic-dev.service`
- Modify: `README.md`
- Modify: `deploy/README.md`

**Interfaces:**
- Consumes: Task 2.
- Produces: deploy + run + test docs that reference the Rust binary and the bridge's `npm install`, no `tsx`/`yarn`.

- [ ] **Step 1: Point the systemd unit at the Rust binary**

In `deploy/agentic-dev.service`, replace the comment + ExecStart:

Old:
```
# claude must be on PATH (the server spawns `claude -p`); it lives in ~/.local/bin here.
Environment=PATH=/home/arcatva/.local/bin:/usr/local/bin:/usr/bin:/bin
```
New:
```
# claude + node must be on PATH: the server spawns the Node SDK bridge (sdk-bridge.mjs) per turn,
# which drives claude. Both live in ~/.local/bin here.
Environment=PATH=/home/arcatva/.local/bin:/usr/local/bin:/usr/bin:/bin
```

Old:
```
ExecStart=/home/arcatva/src/agentic-dev/node_modules/.bin/tsx /home/arcatva/src/agentic-dev/server/index.ts
```
New:
```
ExecStart=/home/arcatva/src/agentic-dev/server-rs/target/release/agentic-dev-server
```

- [ ] **Step 2: Update `README.md` Run/Config/Test**

Replace lines 8-10 (Run section):
```
## Run
    AGENTIC_PASSWORD=<pick-one> bash scripts/dev.sh
The API listens on http://<this-host>:7420 (LAN/tailscale). Point the Android app's Host at it.
```
with:
```
## Run
    ( cd server-rs && npm install )           # once: the per-turn Node bridge's Claude SDK
    ( cd server-rs && cargo build --release )
    AGENTIC_PASSWORD=<pick-one> ./server-rs/target/release/agentic-dev-server
The API listens on http://<this-host>:7420 (LAN/tailscale). Point the Android app's Host at it.
```

Replace the `AGENTIC_PASSWORD` table row:
```
| AGENTIC_PASSWORD | (required by dev.sh) | login password |
```
with:
```
| AGENTIC_PASSWORD | (required) | login password |
```

Replace the Test section (lines 25-26):
```
## Test
    yarn test     # backend, uses a fake claude binary (no API cost)
```
with:
```
## Test
    ( cd server-rs && cargo test )     # never hits real claude (no API cost)
```

- [ ] **Step 3: Update `deploy/README.md`**

Replace line 14:
```
( cd ~/src/agentic-dev && yarn install )
```
with:
```
( cd ~/src/agentic-dev/server-rs && cargo build --release && npm install )
```

Replace the "Redeploy after code changes" block (lines 46-49):
```
cd ~/src/agentic-dev && git pull
yarn install                               # only if deps changed
systemctl --user restart agentic-dev       # restart with new backend
```
with:
```
cd ~/src/agentic-dev && git pull
( cd server-rs && cargo build --release )  # rebuild the binary
( cd server-rs && npm install )            # only if the bridge's SDK dep changed
systemctl --user restart agentic-dev       # restart with new backend
```

- [ ] **Step 4: Verify no `tsx`/`yarn`/`dev.sh` remain in these files**

Run: `grep -nE 'tsx|yarn|dev\.sh' deploy/agentic-dev.service README.md deploy/README.md`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
cd ~/src/agentic-dev
git add deploy/agentic-dev.service README.md deploy/README.md
git commit -m "docs(deploy): point service + READMEs at the Rust binary (drop tsx/yarn)"
```

---

### Task 4: Update architecture docs to Rust

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/internals.md`
- Modify: `docs/errorkind-stop-reason.md`
- Modify: `docs/streaming-only-refactor.md`

**Interfaces:**
- Consumes: Task 2.
- Produces: docs whose file references point at `server-rs/src/...` and whose build/test commands are cargo-based.

- [ ] **Step 1: Rewrite `CLAUDE.md` Layout + Rules**

Replace the `## Layout` block:
```
## Layout
- `server/engine/` — HTTP-independent core: streamParser, store (sqlite + log files),
  worktree, spawner, repos, engine (pool/subscribe/kill). Unit-tested with a fake claude.
- `server/api/` — Fastify routes + WS stream + HMAC token auth + config. This is the whole
  surface the Android client talks to; there is no server-rendered UI.
```
with:
```
## Layout
- `server-rs/src/engine/` — HTTP-independent core: stream parser, store (sqlite + log files),
  worktree, spawner, runner, repos, engine (pool/subscribe/kill). Unit-tested in-crate.
- `server-rs/src/api/` — axum routes + WS stream + HMAC token auth + config. This is the whole
  surface the Android client talks to; there is no server-rendered UI.
- `server-rs/sdk-bridge.mjs` — the only Node code: the per-turn claude transport (Agent SDK),
  spawned by the Rust runner so AskUserQuestion can pause in-turn.
```

Replace the `## Rules` block:
```
## Rules
- Tests must stay green before commit: `yarn test`. Never hit the real `claude` in tests —
  use `server/test/fixtures/fake-claude.sh`.
- Engine stays free of Fastify imports (keep it unit-testable in isolation).
- Driver invariant: spawn `claude -p <prompt> --output-format stream-json --verbose
  --include-partial-messages --dangerously-skip-permissions`, **non-bare**, cwd = worktree.
- Use `yarn`, never `npm install`.
```
with:
```
## Rules
- Tests must stay green before commit: `cd server-rs && cargo test`. Tests never hit the real
  `claude` — they use the fake scripts in `server-rs/tests/fixtures/` (e.g. `fake-claude.sh`).
- Engine (`server-rs/src/engine/`) stays free of axum imports (keep it unit-testable in isolation).
- Driver invariant: the SDK bridge drives `claude` with stream-json over the control protocol,
  **non-bare**, cwd = worktree.
- The bridge's Claude SDK is installed with `npm install` in `server-rs/` (not yarn).
```

- [ ] **Step 2: Rewrite the `internals.md` Runner bullet (lines 17-29)**

Replace:
```
- **Runner = the official Agent SDK (since 2026-06-20): `sdkRunner` drives claude via
  `@anthropic-ai/claude-agent-sdk`'s `query()`**, not the raw `claude -p` CLI. The SDK is the *harness*
  side of claude's stream-json control protocol — it does the `initialize` handshake and answers
  `can_use_tool`. That is what makes **AskUserQuestion actually wait** for the user (see stream-quirks
  below). `sdkRunner` mirrors every SDK message to the session log in the same stream-json shape, so the
  tailer/streamParser/client are unchanged; it maps `RunSpec` → SDK options (cwd, env incl.
  `CLAUDE_CONFIG_DIR`, model, effort/ultracode via `extraArgs`, `resume`), auto-allows every tool except
  AskUserQuestion in `canUseTool`, and exposes `interrupt()` (q.interrupt — turn only) + `stop()`
  (AbortController — hard kill). The SDK ships its own ~233 MB claude binary
  (`@anthropic-ai/claude-agent-sdk-linux-x64`, same 2.1.183 as the system CLI) and uses the `~/.claude`
  login. The legacy `localRunner` (raw `spawn` of a claude binary) stays in the tree **only as the
  Engine/spawner test seam** — tests inject it with `server/test/fixtures/fake-claude.sh`; production
  (`loadConfig`) always uses `sdkRunner`. No env flag; `yarn install` is required after pulling (new dep).
```
with:
```
- **Runner = the official Agent SDK via a Node bridge.** The Rust `SdkRunner` spawns
  `server-rs/sdk-bridge.mjs` per turn; the bridge drives claude through
  `@anthropic-ai/claude-agent-sdk`, not the raw `claude -p` CLI. The SDK is the *harness* side of
  claude's stream-json control protocol — it does the `initialize` handshake and answers
  `can_use_tool`. That is what makes **AskUserQuestion actually wait** for the user (see stream-quirks
  below). The bridge mirrors every SDK message to the session log in the same stream-json shape, so the
  tailer/stream-parser/client are unchanged; it maps `RunSpec` → SDK options (cwd, env incl.
  `CLAUDE_CONFIG_DIR`, model, effort/ultracode, `resume`), auto-allows every tool except
  AskUserQuestion, and supports interrupt (turn only) + stop (SIGTERM — hard kill). The SDK ships its
  own claude binary and uses the `~/.claude` login. `LocalRunner` (raw `spawn`) remains as the
  spawner/runner mechanism; production drives turns through the bridge. The bridge's SDK dep is
  installed with `npm install` in `server-rs/` after pulling.
```

- [ ] **Step 3: Rewrite the `internals.md` Tests / build section (lines 31-33)**

Replace:
```
## Tests / build
- `yarn test` — vitest at the repo root. Typecheck = root `yarn build` (`tsc -p tsconfig.json`).
  Keep both green before commit.
```
with:
```
## Tests / build
- `cd server-rs && cargo test` — the whole suite (engine + api, in-crate). `cargo build --release`
  produces the service binary. Keep tests green before commit.
```

- [ ] **Step 4: Fix the `internals.md` workflows reference (line 50)**

Replace `` `server/engine/workflows.ts` `` with `` `server-rs/src/engine/workflows.rs` ``.

- [ ] **Step 5: Fix `errorkind-stop-reason.md` file references**

Replace `` (`ErrorKind` in `server/engine/types.ts`) `` with `` (`ErrorKind` in `server-rs/src/engine/classify_error.rs`) ``.
Replace `set where (engine.ts)` with `set where (engine/mod.rs)`.
Replace `` `classifyClaudeError` (`server/engine/classifyError.ts`) `` with `` `classify_claude_error` (`server-rs/src/engine/classify_error.rs`) ``.
Replace `` `recover()` + `store.reconcileOrphans()` `` with `` `recover()` + store reconcile (`recover.rs`) ``.

- [ ] **Step 6: Fix `streaming-only-refactor.md` (the cargo line)**

Replace `` Verify `yarn test` + `tsc` green before the client `` with `` Verify `cargo test` green before the client ``.

- [ ] **Step 7: Verify no stale TS references remain in docs**

Run: `grep -rnE 'server/(engine|api|index)|sdkRunner|classifyError\.ts|types\.ts|yarn test|tsc ' CLAUDE.md docs/internals.md docs/errorkind-stop-reason.md docs/streaming-only-refactor.md`
Expected: no output. (References inside `docs/superpowers/{specs,plans,runbooks}/` are historical and out of scope.)

- [ ] **Step 8: Commit**

```bash
cd ~/src/agentic-dev
git add CLAUDE.md docs/internals.md docs/errorkind-stop-reason.md docs/streaming-only-refactor.md
git commit -m "docs: retarget architecture notes from server/ (TS) to server-rs (Rust)"
```

---

### Task 5: Fix the npm-guard session hook

**Files:**
- Modify: `session-hooks/guard.sh`

**Interfaces:**
- Consumes: Task 2 (repo no longer a yarn project).
- Produces: a guard that no longer blocks the bridge's legitimate `npm install`.

- [ ] **Step 1: Remove the npm-block rule**

Delete this block (rule #3):
```bash
# 3. npm in a yarn project (breaks the frozen lockfile + CI).
if echo "$scan" | grep -qE '(^|[[:space:];&|])npm[[:space:]]+(install|i|ci)([[:space:]]|$)'; then
  block "this project uses yarn — 'npm install' breaks the lockfile and CI. Use yarn instead."
fi
```
Leave the rest of the hook (rules #1 force-push and #2 tfvars, and the trailing `exit 0`) intact.

- [ ] **Step 2: Verify the hook still parses**

Run: `bash -n session-hooks/guard.sh && echo "syntax ok"`
Expected: `syntax ok`.

- [ ] **Step 3: Verify the npm rule is gone**

Run: `grep -n "npm" session-hooks/guard.sh`
Expected: no output.

- [ ] **Step 4: Commit**

```bash
cd ~/src/agentic-dev
git add session-hooks/guard.sh
git commit -m "chore(hooks): drop the npm-install block (repo is no longer a yarn project)"
```

---

### Task 6: Scrub parity comments referencing the deleted TS files

**Files:**
- Modify: `server-rs/src/lib.rs`
- Modify: `server-rs/src/api/mod.rs`, `server-rs/src/api/auth.rs`, `server-rs/src/api/config.rs`
- Modify: `server-rs/src/engine/mod.rs`, `server-rs/src/engine/session_guide.rs`, `server-rs/src/engine/workflows.rs`
- Modify: `server-rs/sdk-bridge.mjs`

**Interfaces:**
- Consumes: Task 2.
- Produces: server-rs comments that no longer point at deleted `server/*.ts` files.

- [ ] **Step 1: Rewrite each comment (drop the `server/*.ts` reference)**

Apply these edits (comment text only — do not change code):

- `src/lib.rs:9` — `the HTTP-independent core (parity with \`server/engine/\`): the \`Engine\`` → `the HTTP-independent core: the \`Engine\``
- `src/lib.rs:14` — `the HTTP layer (parity with \`server/api/\`): routes + WS + middleware, plus` → `the HTTP layer: routes + WS + middleware, plus`
- `src/api/mod.rs:8` — `// HTTP-layer support modules (parity with TS server/api/).` → `// HTTP-layer support modules.`
- `src/api/auth.rs:15` — `— identical to server/api/auth.ts.` → `(HMAC-SHA256 over the expiry).` (keep the token-format part before the dash)
- `src/api/config.rs:24` — `(parity: server/api/config.ts:52-60). All opt-in;` → `All opt-in;`
- `src/engine/mod.rs:171` — `/// server/engine/workflows.ts \`hasActiveWorkflow\`. Cheap when there are no workflows` → `/// \`has_active_workflow\`. Cheap when there are no workflows`
- `src/engine/mod.rs:1361` — `// HTTP-independent core modules (parity with TS server/engine/).` → `// HTTP-independent core modules.`
- `src/engine/session_guide.rs:3` — the line `//! server/engine/sessionGuide.ts.` → `//! the multi-repo orientation CLAUDE.md generator.` (adjust to fit the surrounding doc-comment sentence; the goal is no `server/...ts` path)
- `src/engine/workflows.rs:34` — `/// Mirrors server/engine/workflows.ts WORKFLOW_TERMINAL.` → `/// Workflow terminal-status set.`
- `server-rs/sdk-bridge.mjs:16` — `// This file mirrors server/engine/sdkRunner.ts; keep them in sync.` → `// This is the Rust server's per-turn claude transport (Agent SDK); there is no other copy to sync.`

For `auth.rs:15`, the full line is a doc comment beginning `/// Token = "..."`; keep that prefix and only replace the trailing `— identical to server/api/auth.ts.` clause.

- [ ] **Step 2: Verify no `server/` references remain in server-rs source**

Run: `grep -rn "server/" server-rs/src server-rs/sdk-bridge.mjs`
Expected: no output.

- [ ] **Step 3: Verify the crate still builds (comments only, but confirm no accidental edit)**

Run: `cd server-rs && cargo build 2>&1 | tail -3`
Expected: builds successfully.

- [ ] **Step 4: Commit**

```bash
cd ~/src/agentic-dev
git add server-rs/src server-rs/sdk-bridge.mjs
git commit -m "docs(rs): scrub parity comments referencing the deleted TS server"
```

---

### Task 7: Final verification sweep

**Files:** none (verification only)

**Interfaces:**
- Consumes: Tasks 1-6.
- Produces: evidence the cutover cleanup is complete and consistent.

- [ ] **Step 1: Release build**

Run: `cd server-rs && cargo build --release 2>&1 | tail -3`
Expected: `Finished \`release\`` (success).

- [ ] **Step 2: Full test suite**

Run: `cd server-rs && cargo test 2>&1 | tail -8`
Expected: PASS, 259 tests, 0 failed.

- [ ] **Step 3: No live references to the TS backend anywhere outside historical docs**

Run:
```bash
cd ~/src/agentic-dev
grep -rnE 'server/(engine|api|index)|server/test/fixtures|tsx |\byarn\b|vitest|tsconfig|server/index\.ts' \
  --exclude-dir=docs/superpowers --exclude-dir=target --exclude-dir=node_modules . \
  | grep -v 'skills/tenants-dev'   # that skill's yarn refs belong to a different project
```
Expected: no output. (Historical mentions under `docs/superpowers/{specs,plans,runbooks}/` are intentionally preserved; the `skills/tenants-dev` yarn refs are for an unrelated project.)

- [ ] **Step 4: Confirm deleted files are gone**

Run: `ls server tsconfig.json vitest.config.ts package.json yarn.lock scripts/dev.sh 2>&1`
Expected: "No such file or directory" for each.

- [ ] **Step 5: Confirm the service unit points at Rust**

Run: `grep ExecStart deploy/agentic-dev.service`
Expected: `ExecStart=/home/arcatva/src/agentic-dev/server-rs/target/release/agentic-dev-server`

- [ ] **Step 6: Report**

Summarize: tests before/after count, files deleted, files rewired, and the deploy-host reminders from spec §11 (run `npm install` in `server-rs/`; systemd already points at Rust; `rm -rf` the local root `node_modules`; rollback now requires `git revert`).

---

## Self-Review

**Spec coverage:**
- §3 delete TS tree/toolchain → Task 2 ✓
- §4 relocate fake-claude fixtures into server-rs (all tests kept) → Task 1 ✓
- §5 rewire deploy/service + READMEs → Task 3 ✓
- §6 doc updates (CLAUDE.md, internals, errorkind, streaming-only) → Task 4 ✓
- §7 guard.sh npm rule → Task 5 ✓
- §8 scrub parity comments → Task 6 ✓
- §10 verification (cargo build/test, grep sweeps, ExecStart) → Task 7 ✓
- §11 deploy reminders → Task 7 Step 6 report ✓

**Placeholder scan:** No TBD/TODO; every code/edit step has concrete commands, exact `git mv` file lists, and exact old→new strings.

**Type consistency:** Task 1 only edits a path string (`../server/test/fixtures` → `tests/fixtures`) in 4 helpers — no signatures change, all tests preserved. Rust file targets in later tasks (`classify_error.rs`, `workflows.rs`, `engine/mod.rs`) match the actual tree. Task 2's `git rm -r server` is safe because Task 1 already moved the `.sh` fixtures out (only `gitrepo.ts` remains under `server/test/fixtures`).
