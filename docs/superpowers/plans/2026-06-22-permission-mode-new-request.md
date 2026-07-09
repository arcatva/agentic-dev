# Per-session Permission Mode on New Request — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user pick a permission mode (Dangerous / Plan / Accept edits / Ask) when creating a session, have the backend launch the Claude session in that mode, and surface per-tool allow/deny and plan-approval prompts to the Android app over the existing AskUserQuestion round-trip.

**Architecture:** A new, separate `permissionMode` field is plumbed end-to-end (Android form → `POST /api/sessions` → engine → store → spawner → SDK runner). The SDK runner sets the official `permissionMode` SDK option for the non-bypass modes and, when a tool needs permission, **parks** the turn (exactly like `AskUserQuestion` today), writing a synthetic `agentic_perm` log line. `streamParser` turns that into a `perm`/`plan` event; the engine flags it watchdog-exempt and streams it; the app renders an allow/deny (or plan-approval) card; the answer returns via a dedicated `POST /api/sessions/:id/permission` endpoint that resolves the parked `canUseTool`. A durable `agentic_perm_resolved` marker keeps resolved cards from re-rendering after a reseed.

**Tech Stack:** Backend = TypeScript + Fastify + better-sqlite3 + the official `@anthropic-ai/claude-agent-sdk`, tested with vitest. Client = Kotlin + Jetpack Compose (Material 3 Expressive), Ktor.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-06-22-permission-mode-new-request-design.md` (this plan implements it).
- **`permissionMode` value space:** exactly `"plan" | "default" | "acceptEdits" | "bypassPermissions"`. `null`/absent ⇒ treated as `bypassPermissions` (today's behavior). Unknown/legacy values normalize to `null`.
- **Bypass keeps today's path:** for bypass do **not** set the SDK `permissionMode` option (it would stop `canUseTool` firing and break `AskUserQuestion`); leave it unset and auto-allow.
- **After plan approval:** continue in `"default"` — i.e. `q.setPermissionMode("default")` on approve, so each later tool still prompts.
- **Backend:** `yarn test` must stay green before every commit. Never invoke real `claude` in tests — use the fake SDK `query` / `server/test/fixtures/fake-claude*.sh`. The `engine` layer must not import Fastify. Use `yarn`, never `npm install`. (`agentic-dev/CLAUDE.md`)
- **Android:** Compose Material3 is pinned to `1.4.0-alpha18`; do not bump it. **You cannot build or run Gradle in the worktree** (no Gradle wrapper, no signing keystore) — Android code + unit tests are written in the worktree and committed, then **built/tested in the main checkout `~/src/agentic-dev-android`** after pushing to master (see `agentic-dev-android/CLAUDE.md`). Deliver the APK renamed `YYYYMMDD-HHMM.apk` into `outbox/`.
- **API stays additive:** every new field/endpoint is optional/back-compat; old clients and old DB rows keep working.
- Commit after every task. Backend commits run `yarn test` first; Android commits are verified in the final build task (Task B7).

---

# Phase A — Backend (`~/src/agentic-dev`)

Work in `cd /home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev`. Full TDD applies here (`yarn test` runs in the worktree).

### Task A1: `permissionMode` on the Session model + store persistence/migration

**Files:**
- Modify: `server/engine/types.ts` (Session interface, ~`:20-53`)
- Modify: `server/engine/store.ts` (`CreateInput` `:6-12`, `COLUMNS` `:20-24`, `ADDED_COLUMNS` `:30-41`, `normalizeMode` neighborhood `:43-47`, `create` INSERT `:102-108`, `rowToSession` `:156-175`)
- Test: `server/engine/store.test.ts`

**Interfaces:**
- Produces: `Session.permissionMode: string | null`; `CreateInput.permissionMode?: string | null`; `normalizePermissionMode(m): string | null`.

- [ ] **Step 1: Write the failing test** — append to `server/engine/store.test.ts`:

```ts
describe("permissionMode", () => {
  it("round-trips permissionMode and normalizes unknown values to null", () => {
    const dir = mkdtempSync(join(tmpdir(), "store-perm-"));
    const store = new SqliteStore(join(dir, "db.sqlite"), join(dir, "logs"));
    const a = store.create({ id: "s-plan", prompt: "p", worktreePath: dir, branch: "b", permissionMode: "plan" });
    expect(a.permissionMode).toBe("plan");
    expect(store.get("s-plan")!.permissionMode).toBe("plan");
    // Absent → null (bypass is represented as null at the data layer).
    const b = store.create({ id: "s-none", prompt: "p", worktreePath: dir, branch: "b" });
    expect(b.permissionMode).toBeNull();
    // Unknown/legacy value normalizes to null on read.
    store.create({ id: "s-junk", prompt: "p", worktreePath: dir, branch: "b", permissionMode: "garbage" });
    expect(store.get("s-junk")!.permissionMode).toBeNull();
    store.close();
    rmSync(dir, { recursive: true, force: true });
  });
});
```

(Use the existing imports at the top of `store.test.ts`; add `mkdtempSync`, `rmSync`, `tmpdir`, `join` if not already imported.)

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test store.test.ts`
Expected: FAIL — `CreateInput` has no `permissionMode`, `Session.permissionMode` is undefined.

- [ ] **Step 3: Add the field to the types** — in `server/engine/types.ts`, inside `interface Session`, right after the `mode` line (`:28`):

```ts
  mode: string | null;       // orchestration: null = off | "ultracode" = official ultracode (--settings)
  permissionMode: string | null;  // launch permission: null/"bypassPermissions" = auto-allow (today's default)
                                   // | "plan" | "acceptEdits" | "default" (each prompts via canUseTool)
```

- [ ] **Step 4: Thread it through the store** — in `server/engine/store.ts`:

In `CreateInput` (`:9`) add `permissionMode?: string | null;` next to `mode`:
```ts
  model?: string | null; effort?: string | null; mode?: string | null; permissionMode?: string | null;
```
Add the column to `COLUMNS` (end of the template literal, `:24`): change `... effort TEXT, mode TEXT` to `... effort TEXT, mode TEXT, permissionMode TEXT`.
Add to `ADDED_COLUMNS` (`:38` area, after the `mode` entry):
```ts
  ["mode", "TEXT"],
  ["permissionMode", "TEXT"],
```
Add the normalizer next to `normalizeMode` (`:47`):
```ts
/** Permission mode value space: plan | default | acceptEdits | bypassPermissions. Anything else
 *  (legacy/unknown/empty) collapses to null = bypass (today's --dangerously-skip-permissions). */
function normalizePermissionMode(m: string | null | undefined): string | null {
  return m === "plan" || m === "default" || m === "acceptEdits" || m === "bypassPermissions" ? m : null;
}
```
In `create()` (`:93-101`) add `permissionMode: normalizePermissionMode(input.permissionMode),` to the `s` object (next to `mode`). In the INSERT column list and VALUES (`:104-106`) add `permissionMode` / `@permissionMode` next to `mode`.
In `rowToSession` (`:164`) add `permissionMode: normalizePermissionMode(r.permissionMode),` next to the `mode` line.

- [ ] **Step 5: Run the test to verify it passes**

Run: `yarn test store.test.ts`
Expected: PASS.

- [ ] **Step 6: Run the full suite (migration safety) and commit**

Run: `yarn test`
Expected: PASS (the `ADDED_COLUMNS` entry keeps older DBs working; existing tests unaffected).
```bash
git add server/engine/types.ts server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(store): persist permissionMode on sessions (migration + normalize)"
```

---

### Task A2: Spawner arg mapping + engine threading + driver-invariant doc

**Files:**
- Modify: `server/engine/runner.ts` (`RunSpec` `:5-24`)
- Modify: `server/engine/spawner.ts` (`SpawnOptions` `:6-24`, `BASE_ARGS` `:26-31`, `buildSpec` `:119-143` — and **export** it)
- Modify: `server/engine/engine.ts` (`submitSession` `:199-217`, `spawnOpts` `:516-529`)
- Modify: `agentic-dev/CLAUDE.md` (Driver invariant rule)
- Test: `server/engine/spawner.test.ts`

**Interfaces:**
- Consumes: `Session.permissionMode` (Task A1).
- Produces: `SpawnOptions.permissionMode?`, `RunSpec.permissionMode?`, exported `buildSpec(opts): RunSpec`; `engine.submitSession(repos, skills, prompt, env?, meta?)` where `meta` gains `permissionMode?: string | null`.

- [ ] **Step 1: Write the failing test** — append to `server/engine/spawner.test.ts` (import `buildSpec` from `./spawner.js` at the top):

```ts
describe("buildSpec permission mode", () => {
  const base = { bin: "claude", cwd: "/tmp", prompt: "p", logPath: "/tmp/l", unit: "u" };
  it("bypass (null) keeps --dangerously-skip-permissions and no --permission-mode", () => {
    const s = buildSpec({ ...base });
    expect(s.args).toContain("--dangerously-skip-permissions");
    expect(s.args).not.toContain("--permission-mode");
    expect(s.permissionMode ?? null).toBeNull();
  });
  it("plan maps to --permission-mode plan and drops --dangerously-skip-permissions", () => {
    const s = buildSpec({ ...base, permissionMode: "plan" });
    expect(s.args).toEqual(expect.arrayContaining(["--permission-mode", "plan"]));
    expect(s.args).not.toContain("--dangerously-skip-permissions");
    expect(s.permissionMode).toBe("plan");
  });
  it("default/acceptEdits map to their --permission-mode flag", () => {
    expect(buildSpec({ ...base, permissionMode: "default" }).args).toEqual(expect.arrayContaining(["--permission-mode", "default"]));
    expect(buildSpec({ ...base, permissionMode: "acceptEdits" }).args).toEqual(expect.arrayContaining(["--permission-mode", "acceptEdits"]));
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test spawner.test.ts`
Expected: FAIL — `buildSpec` is not exported and `--dangerously-skip-permissions` is unconditional.

- [ ] **Step 3: Implement the arg mapping** — in `server/engine/spawner.ts`:

Remove `--dangerously-skip-permissions` from `BASE_ARGS` (`:26-31`):
```ts
const BASE_ARGS = [
  "--output-format", "stream-json",
  "--verbose",
  "--include-partial-messages",
];
```
Add to `SpawnOptions` (after the `mode?` line `:15`):
```ts
  permissionMode?: string | null;  // null/"bypassPermissions" => --dangerously-skip-permissions (else --permission-mode <x>)
```
Export and extend `buildSpec` (`:119`): change `function buildSpec` to `export function buildSpec`, and inside, after the effort/ultracode pushes (`:124-126`), add the permission flag:
```ts
  // Permission mode: bypass (null) keeps the legacy skip-permissions flag; any other mode uses the
  // official --permission-mode. (This path drives the localRunner/test seam; the production sdkRunner
  // maps permissionMode to the SDK option in sdkRunner.ts instead.)
  const pm = opts.permissionMode ?? null;
  if (!pm || pm === "bypassPermissions") extra.push("--dangerously-skip-permissions");
  else extra.push("--permission-mode", pm);
```
In the returned `RunSpec` (`:142`) add `permissionMode: opts.permissionMode` next to `mode`.

In `server/engine/runner.ts`, add to `RunSpec` (after `mode?` `:22`):
```ts
  permissionMode?: string | null;
```

- [ ] **Step 4: Thread it through the engine** — in `server/engine/engine.ts`:

`submitSession` signature (`:199`) — widen `meta`:
```ts
  submitSession(repos: string[], skills: string[], prompt: string, env?: Record<string, string>, meta?: { model?: string | null; effort?: string | null; mode?: string | null; permissionMode?: string | null }): string {
```
In the `this.store.create({...})` call (`:213`) add `permissionMode: meta?.permissionMode ?? null` next to `mode`.
In `spawnOpts` (`:523`) add `permissionMode: s.permissionMode,` next to `mode: s.mode,`.

- [ ] **Step 5: Update the driver-invariant doc** — in `agentic-dev/CLAUDE.md`, replace the Driver invariant bullet:
```md
- Driver invariant: spawn `claude -p <prompt> --output-format stream-json --verbose
  --include-partial-messages`, **non-bare**, cwd = worktree. The permission flag is now per-session:
  bypass mode passes `--dangerously-skip-permissions`; plan/default/acceptEdits pass
  `--permission-mode <x>` (the production sdkRunner maps this to the SDK option, not argv).
```

- [ ] **Step 6: Run tests and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/engine/runner.ts server/engine/spawner.ts server/engine/spawner.test.ts server/engine/engine.ts CLAUDE.md
git commit -m "feat(spawner): map permissionMode to CLI flag; thread through engine"
```

---

### Task A3: API accepts `permissionMode` (new session + template start)

**Files:**
- Modify: `server/api/routes.ts` (`POST /api/sessions` `:204-215`, `POST /api/templates/start` `:163-189`)
- Modify: `server/engine/templates.ts` (`Template` type, ~`:12-20`)
- Modify: `server/engine/engine.test.ts` (extend an existing creation test)

**Interfaces:**
- Consumes: `engine.submitSession(..., { permissionMode })` (Task A2).
- Produces: `Template.permissionMode?: string | null`.

- [ ] **Step 1: Write the failing assertion** — in `server/engine/engine.test.ts`, find the existing test that calls `engine.submitSession(...)` and asserts on the stored session (search `submitSession`). Add an assertion that `permissionMode` round-trips through the engine. Minimal addition (adapt variable names to that test's harness):

```ts
it("persists permissionMode from submitSession meta", () => {
  const id = engine.submitSession(["repo"], [], "do it", undefined, { permissionMode: "plan" });
  expect(engine.get(id)!.permissionMode).toBe("plan");
});
```
(Place it in the same `describe` block that already constructs `engine` with the test harness's injected `cloneFn`/`syncFn`/`runner`, so no new scaffolding is needed.)

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test engine.test.ts`
Expected: FAIL only if A2 wasn't wired; if A2 is in, this should already pass — in that case keep it as a regression guard and continue. (It exercises the engine path end-to-end to the store.)

- [ ] **Step 3: Wire the routes** — in `server/api/routes.ts`:

`POST /api/sessions` (`:204`) — add `permissionMode?: string` to the Body type and pass it:
```ts
  app.post<{ Body: { repo?: string; repos?: string[]; skills?: string[]; prompt?: string; model?: string; effort?: string; mode?: string; permissionMode?: string } }>("/api/sessions", async (req, reply) => {
    const b = req.body ?? {};
    const repos = b.repos ?? (b.repo ? [b.repo] : []);
    const skills = b.skills ?? [];
    if (!b.prompt) return reply.code(400).send({ error: "prompt required" });
    try {
      const id = engine.submitSession(repos, skills, b.prompt, undefined, { model: b.model, effort: b.effort, mode: b.mode, permissionMode: b.permissionMode });
      return { id };
    } catch (err: any) {
      return reply.code(400).send({ error: String(err?.message ?? err) });
    }
  });
```
`POST /api/templates/start` (`:163`) — add `permissionMode?: string` to the Body type and the meta with template fallback:
```ts
            mode: b.mode ?? tpl.mode ?? null,
            permissionMode: b.permissionMode ?? tpl.permissionMode ?? null,
```

In `server/engine/templates.ts`, add to the `Template` type next to `mode`:
```ts
  permissionMode?: string | null;
```

- [ ] **Step 4: Run tests and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/api/routes.ts server/engine/templates.ts server/engine/engine.test.ts
git commit -m "feat(api): accept permissionMode on session + template start"
```

---

### Task A4: `perm` / `plan` / `permResolved` events in the stream parser

**Files:**
- Modify: `server/engine/types.ts` (`ClaudeEvent` union `:59-72`)
- Modify: `server/engine/streamParser.ts` (`:32-46` area)
- Modify: `server/api/renderedLog.ts` (`RENDERED_PREFIXES` `:7`)
- Test: `server/engine/streamParser.test.ts`

**Interfaces:**
- Produces: events `{ kind:"perm", id, tool, input }`, `{ kind:"plan", id, plan }`, `{ kind:"permResolved", id, decision }`; synthetic log line shapes `{"type":"agentic_perm",...}` and `{"type":"agentic_perm_resolved",...}` are now rendered (kept by `filterRendered`).

- [ ] **Step 1: Write the failing test** — append to `server/engine/streamParser.test.ts`:

```ts
describe("permission events", () => {
  it("parses an agentic_perm tool request", () => {
    const ev = parseLine(JSON.stringify({ type: "agentic_perm", permKind: "perm", id: "perm-1", tool: "Bash", input: { command: "ls" }, at: 1 }));
    expect(ev).toEqual([{ kind: "perm", id: "perm-1", tool: "Bash", input: { command: "ls" }, raw: expect.anything() }]);
  });
  it("parses an agentic_perm plan request", () => {
    const ev = parseLine(JSON.stringify({ type: "agentic_perm", permKind: "plan", id: "perm-2", plan: "# Plan\n- step", at: 1 }));
    expect(ev).toEqual([{ kind: "plan", id: "perm-2", plan: "# Plan\n- step", raw: expect.anything() }]);
  });
  it("parses an agentic_perm_resolved marker", () => {
    const ev = parseLine(JSON.stringify({ type: "agentic_perm_resolved", id: "perm-1", decision: "allow", at: 2 }));
    expect(ev).toEqual([{ kind: "permResolved", id: "perm-1", decision: "allow", raw: expect.anything() }]);
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test streamParser.test.ts`
Expected: FAIL — these lines currently fall through to `{ kind: "other" }`.

- [ ] **Step 3: Add the union members** — in `server/engine/types.ts`, in `ClaudeEvent` (after the `ask` line `:64`):
```ts
  | { kind: "perm"; id: string; tool: string; input: unknown; raw: unknown }   // a tool awaiting allow/deny
  | { kind: "plan"; id: string; plan: string; raw: unknown }                   // ExitPlanMode awaiting approval
  | { kind: "permResolved"; id: string; decision: string; raw: unknown }       // a perm/plan was answered
```

- [ ] **Step 4: Parse the synthetic lines** — in `server/engine/streamParser.ts`, after the `agentic_prompt` branch (`:35-37`):
```ts
  if (obj && obj.type === "agentic_perm") {
    const id = String(obj.id ?? "");
    if (obj.permKind === "plan") return [{ kind: "plan", id, plan: String(obj.plan ?? ""), raw: obj }];
    return [{ kind: "perm", id, tool: String(obj.tool ?? "tool"), input: obj.input ?? {}, raw: obj }];
  }
  if (obj && obj.type === "agentic_perm_resolved") {
    return [{ kind: "permResolved", id: String(obj.id ?? ""), decision: String(obj.decision ?? "deny"), raw: obj }];
  }
```

- [ ] **Step 5: Keep the synthetic lines in the rendered view** — in `server/api/renderedLog.ts`, add the two prefixes to `RENDERED_PREFIXES` (`:7`):
```ts
const RENDERED_PREFIXES = ['{"type":"agentic_prompt"', '{"type":"agentic_perm"', '{"type":"agentic_perm_resolved"', '{"type":"stream_event"', '{"type":"assistant"', '{"type":"result"'];
```
(They must be rendered: the client both renders the cards from them AND counts the filtered log for its `since` cursor, so they have to be present in that filtered view after a reseed.)

- [ ] **Step 6: Run tests and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/engine/types.ts server/engine/streamParser.ts server/api/renderedLog.ts server/engine/streamParser.test.ts
git commit -m "feat(stream): parse agentic_perm/agentic_perm_resolved events; keep them rendered"
```

---

### Task A5: SDK runner — set permissionMode, park perm/plan, respond/deny

**Files:**
- Modify: `server/engine/runner.ts` (`RunHandle` interface `:27-37`)
- Modify: `server/engine/sdkRunner.ts` (`:24-180`)
- Test: `server/engine/sdkRunner.test.ts`

**Interfaces:**
- Consumes: `RunSpec.permissionMode` (Task A2).
- Produces: `RunHandle.respondPermission?(decision: "allow" | "deny", feedback?: string): void`; SDK option `permissionMode` set for non-bypass; synthetic `agentic_perm` / `agentic_perm_resolved` lines written; `q.setPermissionMode("default")` called on plan-approve.

- [ ] **Step 1: Write the failing tests** — append to `server/engine/sdkRunner.test.ts`. First extend `fakeSdk()` so the fake `Query` records `setPermissionMode` (the real Query has it; sdkRunner now calls it on plan-approve). After the `(gen as any).interrupt = ...` line add:
```ts
    (gen as any).setPermissionMode = async (m: any) => { captured.permissionModeSet = m; };
```
and add `permissionModeSet: undefined as any` to the `captured` object literal. Add `makeSpec` overload usage inline. Then the tests:

```ts
it("does not set the SDK permissionMode for bypass (null) and still auto-allows tools", async () => {
  const { query, captured, endGen } = fakeSdk();
  const spec = makeSpec();   // no permissionMode = bypass
  sdkRunner({ query }).start(spec);
  expect(captured.options.permissionMode).toBeUndefined();
  const r = await captured.options.canUseTool("Bash", { command: "echo hi" }, askOpts);
  expect(r).toEqual({ behavior: "allow", updatedInput: { command: "echo hi" } });
  endGen(); await tick();
  rmSync(spec.cwd, { recursive: true, force: true });
});

it("default mode: parks a tool, writes a synthetic line, and resolves on respondPermission allow", async () => {
  const { query, captured, endGen } = fakeSdk();
  const spec = { ...makeSpec(), permissionMode: "default" };
  const h = sdkRunner({ query }).start(spec);
  expect(captured.options.permissionMode).toBe("default");
  let resolved = false;
  const p = captured.options.canUseTool("Bash", { command: "rm -rf x" }, askOpts).then((r: any) => { resolved = true; return r; });
  await tick();
  expect(resolved).toBe(false);                                   // parked, not auto-allowed
  expect(readFileSync(spec.logPath, "utf8")).toContain('"type":"agentic_perm"');
  h.respondPermission!("allow");
  expect(await p).toEqual({ behavior: "allow", updatedInput: { command: "rm -rf x" } });
  expect(readFileSync(spec.logPath, "utf8")).toContain('"type":"agentic_perm_resolved"');
  endGen(); await tick();
  rmSync(spec.cwd, { recursive: true, force: true });
});

it("default mode: deny carries the user's feedback as the SDK deny message", async () => {
  const { query, captured, endGen } = fakeSdk();
  const spec = { ...makeSpec(), permissionMode: "default" };
  const h = sdkRunner({ query }).start(spec);
  const p = captured.options.canUseTool("Bash", { command: "x" }, askOpts);
  await tick();
  h.respondPermission!("deny", "too risky");
  expect(await p).toEqual({ behavior: "deny", message: "too risky" });
  endGen(); await tick();
  rmSync(spec.cwd, { recursive: true, force: true });
});

it("plan mode: ExitPlanMode parks as a plan; approve allows it and switches to default", async () => {
  const { query, captured, endGen } = fakeSdk();
  const spec = { ...makeSpec(), permissionMode: "plan" };
  const h = sdkRunner({ query }).start(spec);
  const p = captured.options.canUseTool("ExitPlanMode", { plan: "# do X" }, askOpts);
  await tick();
  expect(readFileSync(spec.logPath, "utf8")).toContain('"permKind":"plan"');
  h.respondPermission!("allow");
  expect(await p).toEqual({ behavior: "allow", updatedInput: { plan: "# do X" } });
  expect(captured.permissionModeSet).toBe("default");
  endGen(); await tick();
  rmSync(spec.cwd, { recursive: true, force: true });
});

it("interrupt denies a parked permission (no leaked promise)", async () => {
  const { query, captured, endGen } = fakeSdk();
  const spec = { ...makeSpec(), permissionMode: "default" };
  const h = sdkRunner({ query }).start(spec);
  const p = captured.options.canUseTool("Bash", { command: "x" }, askOpts);
  await tick();
  h.interrupt!();
  expect(await p).toEqual({ behavior: "deny", message: "interrupted" });
  endGen(); await tick();
  rmSync(spec.cwd, { recursive: true, force: true });
});
```

- [ ] **Step 2: Run them and watch them fail**

Run: `yarn test sdkRunner.test.ts`
Expected: FAIL — `respondPermission` undefined; no permissionMode option; tools auto-allow.

- [ ] **Step 3: Add the interface member** — in `server/engine/runner.ts`, in `RunHandle` (after `interrupt?()` `:33`):
```ts
  // Resolve a parked perm/plan permission request (sdkRunner only; absent on the raw-CLI runner).
  respondPermission?(decision: "allow" | "deny", feedback?: string): void;
```

- [ ] **Step 4: Rework `sdkRunner`** — in `server/engine/sdkRunner.ts`:

Replace the `ask` slot (`:47-54`) with a generalized pending slot + interactivity flag:
```ts
      // ── One parked interactive request (ask | perm | plan) ───────────────────
      // The turn blocks on exactly one canUseTool call at a time. `ask` resolves from the next write()
      // (the answer text); `perm`/`plan` resolve from respondPermission(). `id` correlates the synthetic
      // agentic_perm line with its agentic_perm_resolved marker (perm/plan only).
      type PendingKind = "ask" | "perm" | "plan";
      const pending: {
        kind: PendingKind | null;
        resolve: ((r: PermissionResult) => void) | null;
        input: Record<string, unknown> | null;
        id: string | null;
      } = { kind: null, resolve: null, input: null, id: null };
      let permSeq = 0;
      // Bypass = leave permissionMode unset so the SDK never routes tools through canUseTool except the
      // ask it always does — preserving today's auto-allow + AskUserQuestion behavior.
      const interactivePerms = !!spec.permissionMode && spec.permissionMode !== "bypassPermissions";
```

Replace `canUseTool` (`:56-65`):
```ts
      const canUseTool = async (toolName: string, toolInput: Record<string, unknown>): Promise<PermissionResult> => {
        if (toolName === "AskUserQuestion") {
          return await new Promise<PermissionResult>((resolve) => {
            pending.kind = "ask"; pending.resolve = resolve; pending.input = toolInput; pending.id = null;
          });
        }
        // Bypass: auto-allow everything else (today's behavior).
        if (!interactivePerms) return { behavior: "allow", updatedInput: toolInput };
        // default/acceptEdits/plan: park the request and surface a card via a synthetic log line.
        const kind: PendingKind = toolName === "ExitPlanMode" ? "plan" : "perm";
        const id = `perm-${++permSeq}`;
        writeLog(
          kind === "plan"
            ? { type: "agentic_perm", permKind: "plan", id, plan: typeof toolInput.plan === "string" ? toolInput.plan : "", at: Date.now() }
            : { type: "agentic_perm", permKind: "perm", id, tool: toolName, input: toolInput, at: Date.now() },
        );
        return await new Promise<PermissionResult>((resolve) => {
          pending.kind = kind; pending.resolve = resolve; pending.input = toolInput; pending.id = id;
        });
      };
```

Note: `writeLog` is defined just below (`:68`); hoist it ABOVE `canUseTool` (move the `const writeLog = ...` block to before `canUseTool`) so `canUseTool` can call it.

Add `permissionMode` to the SDK options (`:84-93`), after the `resume` spread:
```ts
          ...(interactivePerms ? { permissionMode: spec.permissionMode as "default" | "acceptEdits" | "plan" } : {}),
```

Add a shared deny helper just before the `return { ... }` handle (`:147`):
```ts
      // Resolve a parked request with deny (interrupt / query death). perm/plan also get a resolution
      // marker so a reseed renders the card decided, not actionable.
      const denyPending = (message: string): void => {
        const p = pending;
        if (!p.resolve) return;
        const { resolve, kind, id } = p;
        pending.kind = null; pending.resolve = null; pending.input = null; pending.id = null;
        if ((kind === "perm" || kind === "plan") && id) writeLog({ type: "agentic_perm_resolved", id, decision: "deny", at: Date.now() });
        resolve({ behavior: "deny", message });
      };
```

In the pump `finally` (`:110-117`), replace the stranded-ask cleanup with `denyPending("session ended");`.

In `interrupt` (`:151-159`), replace the stranded-ask cleanup with `denyPending("interrupted");` (keep the `try { void q.interrupt(); } catch {}`).

In `write` (`:160-175`), gate the answer path on the ask kind:
```ts
        write: (line: string) => {
          if (pending.kind === "ask" && pending.resolve) {
            const resolve = pending.resolve;
            const askInput = pending.input ?? {};
            pending.kind = null; pending.resolve = null; pending.input = null; pending.id = null;
            const answerText = stripPreamble(userContent(line));
            resolve({ behavior: "allow", updatedInput: { ...askInput, answers: buildAnswers(askInput, answerText) } });
            return;
          }
          try { queue.push(JSON.parse(line) as SDKUserMessage); drain(); } catch { /* drop malformed */ }
        },
```

Add `respondPermission` to the returned handle (next to `write`):
```ts
        respondPermission: (decision: "allow" | "deny", feedback?: string) => {
          const p = pending;
          if (!p.resolve || (p.kind !== "perm" && p.kind !== "plan")) return;
          const { resolve, kind, id, input } = p;
          pending.kind = null; pending.resolve = null; pending.input = null; pending.id = null;
          if (id) writeLog({ type: "agentic_perm_resolved", id, decision, at: Date.now() });
          if (decision === "allow") {
            // Approving a plan exits plan mode; continue in `default` so each later tool still prompts.
            if (kind === "plan") { try { void q.setPermissionMode("default"); } catch { /* */ } }
            resolve({ behavior: "allow", updatedInput: input ?? {} });
          } else {
            resolve({ behavior: "deny", message: feedback || "denied by user" });
          }
        },
```

(`q` is the `Query` from `runQuery(...)` already in scope; `PermissionResult` is already imported `:2`.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `yarn test sdkRunner.test.ts`
Expected: PASS (including the unchanged AskUserQuestion tests — ask still parks and resolves from `write()`).

- [ ] **Step 6: Run the full suite and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/engine/runner.ts server/engine/sdkRunner.ts server/engine/sdkRunner.test.ts
git commit -m "feat(sdkRunner): permissionMode option + park/respond perm & plan requests"
```

---

### Task A6: Engine bookkeeping + respondPermission + SpawnHandle passthrough

**Files:**
- Modify: `server/engine/spawner.ts` (`SpawnHandle` `:45-101`)
- Modify: `server/engine/engine.ts` (`pendingPerm`, `tickWatchdog` `:86-129`, `attach` `:532-569`, `forgetSession` `:417-424`, new `respondPermission`)
- Test: `server/engine/engine.test.ts`

**Interfaces:**
- Consumes: `RunHandle.respondPermission` (Task A5), `perm`/`plan`/`permResolved` events (Task A4).
- Produces: `engine.respondPermission(id, decision, feedback?)`; `SpawnHandle.respondPermission(decision, feedback?)`.

- [ ] **Step 1: Write the failing test** — append to `server/engine/engine.test.ts` (use the file's existing streaming harness with a fake runner; mirror an existing test that drives events through a handle). The key behaviors: a `perm` event makes the session watchdog-exempt, and `respondPermission` forwards to the running handle. Concretely, add:

```ts
it("a pending perm exempts the session from the idle watchdog and respondPermission forwards to the handle", async () => {
  // Build the engine with the test harness's fake runner that records respondPermission calls and lets
  // the test push events. (Reuse the same setup helper the streaming tests use; see the top of this file.)
  const { engine, handleFor, pushEvent, advance } = makeStreamingEngine();   // existing helper in this file
  const id = engine.submitSession(["repo"], [], "go");
  await advance();                                                            // let start() attach the handle
  pushEvent(id, { kind: "perm", id: "perm-1", tool: "Bash", input: {}, raw: {} });
  engine.triggerWatchdog();                                                   // would reap an idle turn; must NOT here
  expect(engine.get(id)!.status).toBe("running");                            // still alive (parked, exempt)
  engine.respondPermission(id, "allow");
  expect(handleFor(id).respondPermissionCalls).toEqual([["allow", undefined]]);
});
```
If `engine.test.ts` has no such named helper, follow the existing pattern in that file (a fake `Runner` whose `RunHandle` records calls and emits events) and add `respondPermission` recording to that fake handle.

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test engine.test.ts`
Expected: FAIL — `engine.respondPermission` and `pendingPerm` don't exist.

- [ ] **Step 3: Add the `SpawnHandle` passthrough** — in `server/engine/spawner.ts`, in `SpawnHandle`, next to `interrupt()` (`:88-93`):
```ts
  /** Forward a perm/plan allow/deny to the runner's parked canUseTool (sdkRunner). No-op on the
   *  raw-CLI runner, which has no respondPermission. */
  respondPermission(decision: "allow" | "deny", feedback?: string): void {
    this.run.respondPermission?.(decision, feedback);
  }
```

- [ ] **Step 4: Engine bookkeeping** — in `server/engine/engine.ts`:

Add the set next to `pendingAsk` (`:52`):
```ts
  // A turn parked on a perm/plan permission request — like pendingAsk, exempt from the idle watchdog
  // until the user answers (or the turn ends).
  private pendingPerm = new Set<string>();
```
In `tickWatchdog`, extend the idle-TTL guard (`:97`) and the `parked` test (`:109`):
```ts
      if (this.cfg.idleTtlMs !== undefined && this.awaiting.get(id) === true && !this.pendingAsk.has(id) && !this.pendingPerm.has(id) && (now - last) > this.cfg.idleTtlMs) {
```
```ts
      const parked = this.awaiting.get(id) === true || this.pendingAsk.has(id) || this.pendingPerm.has(id);
```
In `attach`'s event handler, next to the `ask` flag (`:549`):
```ts
      if (ev.kind === "perm" || ev.kind === "plan") this.pendingPerm.add(id);
      if (ev.kind === "permResolved") this.pendingPerm.delete(id);
```
In the `result` handler (`:556`) clear it alongside `pendingAsk`:
```ts
        this.pendingAsk.delete(id);
        this.pendingPerm.delete(id);
```
In the `exit` handler (`:577`) and `interrupt` (`:455`), clear `pendingPerm` next to each existing `pendingAsk` clear. In `forgetSession` (`:422`) add `this.pendingPerm.delete(id);`.
Add the public method (next to `interrupt` `:451`):
```ts
  /** Answer a parked perm/plan request for a live session: forward to its handle and drop the
   *  watchdog exemption (a later perm/plan event re-arms it). No-op if the session isn't running. */
  respondPermission(id: string, decision: "allow" | "deny", feedback?: string): void {
    this.pendingPerm.delete(id);
    this.running.get(id)?.respondPermission(decision, feedback);
  }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `yarn test engine.test.ts`
Expected: PASS.

- [ ] **Step 6: Run the full suite and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/engine/spawner.ts server/engine/engine.ts server/engine/engine.test.ts
git commit -m "feat(engine): pendingPerm watchdog exemption + respondPermission routing"
```

---

### Task A7: `POST /api/sessions/:id/permission` endpoint

**Files:**
- Modify: `server/api/routes.ts` (next to `/interrupt` `:242-246`)
- Test: `server/api/server.test.ts`

**Interfaces:**
- Consumes: `engine.respondPermission` (Task A6).

- [ ] **Step 1: Write the failing test** — in `server/api/server.test.ts`, mirror the existing `/interrupt` test (search `interrupt`). Add a test that POSTs a valid decision and gets `{ ok: true }`, and that a bad decision is rejected 400. Example (adapt to the harness's app/token helpers):

```ts
it("POST /permission forwards a decision and rejects bad input", async () => {
  const id = engine.submitSession(["repo"], [], "go");
  const ok = await app.inject({ method: "POST", url: `/api/sessions/${id}/permission`, headers: authHeader, payload: { decision: "allow" } });
  expect(ok.statusCode).toBe(200);
  expect(ok.json()).toEqual({ ok: true });
  const bad = await app.inject({ method: "POST", url: `/api/sessions/${id}/permission`, headers: authHeader, payload: { decision: "maybe" } });
  expect(bad.statusCode).toBe(400);
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `yarn test server.test.ts`
Expected: FAIL — route not found (404).

- [ ] **Step 3: Add the route** — in `server/api/routes.ts`, after the `/interrupt` handler (`:246`):
```ts
  // Answer a parked permission/plan prompt (allow/deny, with optional feedback used as the deny reason
  // or plan revision note). Distinct from /messages (a chat turn) and /interrupt (stop the turn).
  app.post<{ Params: { id: string }; Body: { decision?: string; feedback?: string } }>("/api/sessions/:id/permission", async (req, reply) => {
    if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
    const decision = req.body?.decision;
    if (decision !== "allow" && decision !== "deny") return reply.code(400).send({ error: "decision must be 'allow' or 'deny'" });
    engine.respondPermission(req.params.id, decision, req.body?.feedback);
    return { ok: true };
  });
```

- [ ] **Step 4: Run tests and commit**

Run: `yarn test`
Expected: PASS.
```bash
git add server/api/routes.ts server/api/server.test.ts
git commit -m "feat(api): POST /api/sessions/:id/permission to answer a parked prompt"
```

**Backend phase done.** Run `yarn test` once more — all green — before starting Phase B.

---

# Phase B — Android client (`~/src/agentic-dev-android`)

Work in `cd /home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev-android`. **Do NOT run Gradle here.** Write code + unit tests and commit each task; everything is compiled, tested, and APK-built in the main checkout in Task B7. Each task's "verify" is deferred to B7 except where noted.

### Task B1: Network/data models

**Files:**
- Modify: `app/src/main/java/dev/agentic/data/net/Models.kt` (`Session` `:6-35`, `NewSessionReq` `:58-69`, `Template` `:137-148`)

**Interfaces:**
- Produces: `Session.permissionMode`, `NewSessionReq.permissionMode`, `Template.permissionMode`, `PermDecisionReq(decision, feedback)`.

- [ ] **Step 1: Add the fields** — in `Models.kt`:

`Session` — after `val mode: String? = null,` (`:20`): `val permissionMode: String? = null,`
`NewSessionReq` — after `val mode: String? = null,` (`:68`): `val permissionMode: String? = null,`
`Template` — after `val mode: String? = null,` (`:145`): `val permissionMode: String? = null,`
Add the request DTO near `PromptReq` (`:75`):
```kotlin
/** Body for POST /api/sessions/:id/permission — answer a parked allow/deny or plan-approval prompt.
 *  feedback is the deny reason or plan-revision note (ignored on allow). */
@Serializable
data class PermDecisionReq(val decision: String, val feedback: String? = null)
```

- [ ] **Step 2: Commit**
```bash
git add app/src/main/java/dev/agentic/data/net/Models.kt
git commit -m "feat(models): permissionMode fields + PermDecisionReq"
```

---

### Task B2: New-request Permissions slider

**Files:**
- Modify: `app/src/main/java/dev/agentic/ui/newrequest/NewRequestViewModel.kt` (`NewRequestUiState` `:17-31`, setters `:63-69`, `applyTemplate` `:75-86`, `submit` `:93-117`)
- Modify: `app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt` (slider list `:67-84`, layout `:229-255`)
- Test: `app/src/test/java/dev/agentic/ui/newrequest/NewRequestViewModelTest.kt` (create if absent; otherwise add a test)

**Interfaces:**
- Consumes: `NewSessionReq.permissionMode` (B1).
- Produces: `NewRequestViewModel.setPermissionMode(String?)`; the form submits `permissionMode`.

- [ ] **Step 1: Write the unit test** (run in B7) — create/extend `NewRequestViewModelTest.kt`:
```kotlin
@Test
fun submit_includes_permissionMode() = runTest {
    val api = FakeApi()                       // the project's existing fake AgenticApi test double
    val vm = NewRequestViewModel(SessionsRepository(api, this))
    vm.setPrompt("do it")
    vm.setPermissionMode("plan")
    vm.submit()
    advanceUntilIdle()
    assertEquals("plan", api.lastNewSessionReq?.permissionMode)
}
```
(Mirror whatever fake/`SessionsRepository` construction the existing new-request tests use; if there is no `FakeApi`, reuse the test double the backend-facing repo tests already define.)

- [ ] **Step 2: ViewModel wiring** — in `NewRequestViewModel.kt`:

`NewRequestUiState` — after `val mode: String? = null,` (`:27`): `val permissionMode: String? = null,`
Add setter after `setMode` (`:69`):
```kotlin
    fun setPermissionMode(permissionMode: String?) { _uiState.update { it.copy(permissionMode = permissionMode) } }
```
`applyTemplate` — add `permissionMode = t.permissionMode,` inside the `copy { }` (after `mode = t.mode,` `:83`).
`submit` — add to the `NewSessionReq(...)` (after `mode = s.mode,` `:106`): `permissionMode = s.permissionMode,`

- [ ] **Step 3: Slider UI** — in `NewRequestScreen.kt`:

Add the option list after `EFFORTS` (`:84`):
```kotlin
// Permission mode the session launches in. Ordered left→right by ascending autonomy so the slider
// reads "least free" → "most free": Plan (read-only) · Ask (prompt each tool) · Accept edits ·
// Dangerous (auto-allow = today's default). VM stores null for Dangerous (back-compat); we map
// "bypassPermissions" ↔ null at the VM boundary so the default notch sends no override.
private val PERMISSION_MODES = listOf(
    "plan" to "Plan",
    "default" to "Ask",
    "acceptEdits" to "Accept edits",
    "bypassPermissions" to "Dangerous",
)
```
Add the slider right after the Effort `SliderField` block (`:255`), before the prompt field:
```kotlin
            // ── Permission mode ──────────────────────────────────────────────────────
            // Default notch = Dangerous (today's behavior); VM null ⇒ "bypassPermissions".
            SliderField(
                label = "Permissions",
                options = PERMISSION_MODES,
                value = s.permissionMode ?: "bypassPermissions",
                onSelect = { key -> realVm.setPermissionMode(if (key == "bypassPermissions") null else key) },
            )
```
(`SliderField` `:289` already snaps a 4-item list — `steps = (options.size - 2)` = 2 — and shows `label: <pick>` above; no change needed there.)

- [ ] **Step 4: Commit**
```bash
git add app/src/main/java/dev/agentic/ui/newrequest/NewRequestViewModel.kt \
        app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt \
        app/src/test/java/dev/agentic/ui/newrequest/NewRequestViewModelTest.kt
git commit -m "feat(newrequest): Permissions slider + submit permissionMode"
```

---

### Task B3: Domain nodes + transcript reducer for perm/plan

**Files:**
- Modify: `app/src/main/java/dev/agentic/domain/Node.kt` (`:38`)
- Modify: `app/src/main/java/dev/agentic/domain/Transcript.kt` (`buildFromLog` `:97-154`, `applyEvent` `:157-218`, `frameBusy` `:222-228`)
- Test: `app/src/test/java/dev/agentic/domain/TranscriptPermTest.kt` (create)

**Interfaces:**
- Consumes: backend events `perm`/`plan`/`permResolved` and log types `agentic_perm`/`agentic_perm_resolved`.
- Produces: `PermNode(id, tool, summary, decided, decision)`, `PlanNode(id, plan, decided, decision)`, `markPermDecided(nodes, id, decision)`.

- [ ] **Step 1: Write the unit test** (run in B7) — create `TranscriptPermTest.kt`:
```kotlin
package dev.agentic.domain
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class TranscriptPermTest {
    @Test fun live_perm_then_resolved_marks_decided() {
        var nodes = emptyList<Node>()
        nodes = applyEvent(nodes, """{"kind":"perm","id":"perm-1","tool":"Bash","input":{"command":"ls"}}""").first
        assertTrue(nodes.last() is PermNode)
        assertEquals(false, (nodes.last() as PermNode).decided)
        nodes = applyEvent(nodes, """{"kind":"permResolved","id":"perm-1","decision":"allow"}""").first
        assertEquals("allow", (nodes.single { it is PermNode } as PermNode).decision)
        assertTrue((nodes.single { it is PermNode } as PermNode).decided)
    }
    @Test fun log_plan_renders_and_resolves() {
        val log = listOf(
            """{"type":"agentic_perm","permKind":"plan","id":"p2","plan":"# Plan"}""",
            """{"type":"agentic_perm_resolved","id":"p2","decision":"deny"}""",
        )
        val nodes = buildFromLog(log)
        val plan = nodes.single { it is PlanNode } as PlanNode
        assertEquals("# Plan", plan.plan)
        assertTrue(plan.decided)
        assertEquals("deny", plan.decision)
    }
    @Test fun frameBusy_true_for_perm_and_plan() {
        assertEquals(true, frameBusy("""{"kind":"perm","id":"x"}"""))
        assertEquals(true, frameBusy("""{"kind":"plan","id":"x"}"""))
    }
}
```

- [ ] **Step 2: Add the nodes** — in `Node.kt`, after `AskNode` (`:38`):
```kotlin
/** A tool awaiting the user's allow/deny (permission mode = default/acceptEdits). [id] correlates the
 *  card with its backend agentic_perm_resolved marker so a reseed shows it decided. [summary] is a
 *  short human label for the tool call (e.g. "Bash · rm -rf x"). */
data class PermNode(val id: String, val tool: String, val summary: String = "", val decided: Boolean = false, val decision: String = "") : Node
/** A plan (ExitPlanMode in plan mode) awaiting Approve / Keep-planning. */
data class PlanNode(val id: String, val plan: String, val decided: Boolean = false, val decision: String = "") : Node
```

- [ ] **Step 3: Add the decided-marker helper** — in `domain/Transcript.kt`, after `attachAgentResult` (`:64`):
```kotlin
/** Mark the most recent Perm/Plan node with [id] as decided (from a permResolved event / log marker).
 *  No-op if none matches (the perm line may have been filtered out of a partial log). */
fun markPermDecided(nodes: List<Node>, id: String, decision: String): List<Node> {
    val idx = nodes.indexOfLast { (it is PermNode && it.id == id) || (it is PlanNode && it.id == id) }
    if (idx < 0) return nodes
    return nodes.toMutableList().also {
        it[idx] = when (val n = it[idx]) {
            is PermNode -> n.copy(decided = true, decision = decision)
            is PlanNode -> n.copy(decided = true, decision = decision)
            else -> n
        }
    }
}
```

- [ ] **Step 4: Parse the log lines** — in `buildFromLog`'s `when` (`:104`), add cases (after `"agentic_prompt"`):
```kotlin
            "agentic_perm" -> {
                val id = o["id"]?.jsonPrimitive?.contentOrNull ?: ""
                nodes = if (o["permKind"]?.jsonPrimitive?.contentOrNull == "plan") {
                    nodes + PlanNode(id, o["plan"]?.jsonPrimitive?.contentOrNull ?: "")
                } else {
                    val tool = o["tool"]?.jsonPrimitive?.contentOrNull ?: "tool"
                    nodes + PermNode(id, tool, permSummary(tool, o["input"] as? JsonObject))
                }
            }
            "agentic_perm_resolved" -> nodes = markPermDecided(
                nodes,
                o["id"]?.jsonPrimitive?.contentOrNull ?: "",
                o["decision"]?.jsonPrimitive?.contentOrNull ?: "",
            )
```
Add a small summary helper near the bottom of the file:
```kotlin
/** Short one-line label for a permission card's tool call (reuses the tool summary the chips use). */
fun permSummary(tool: String, input: JsonObject?): String {
    val s = toolSummary(tool, input)
    return tool.replaceFirstChar { it.uppercase() } + if (s.isNotBlank()) " · $s" else ""
}
```
(`toolSummary` already exists and is used by `buildFromLog` for `ToolNode` `:137`.)

- [ ] **Step 5: Handle the live events** — in `applyEvent`'s `when` (`:162`), add cases (after `"ask"`):
```kotlin
        "perm" -> {
            val id = o["id"]?.jsonPrimitive?.contentOrNull ?: ""
            val tool = o["tool"]?.jsonPrimitive?.contentOrNull ?: "tool"
            (nodes + PermNode(id, tool, permSummary(tool, o["input"] as? JsonObject))) to false
        }
        "plan" -> {
            val id = o["id"]?.jsonPrimitive?.contentOrNull ?: ""
            (nodes + PlanNode(id, o["plan"]?.jsonPrimitive?.contentOrNull ?: "")) to false
        }
        "permResolved" -> markPermDecided(
            nodes,
            o["id"]?.jsonPrimitive?.contentOrNull ?: "",
            o["decision"]?.jsonPrimitive?.contentOrNull ?: "",
        ) to false
```
In `frameBusy` (`:225`), add `"perm", "plan"` to the `true` list (awaiting the user = still a live turn), and treat `"permResolved"` as non-signal (falls into `else -> null`):
```kotlin
        "prompt", "text", "thinking", "tool", "skill", "agent", "workflow", "ask", "perm", "plan", "retry" -> true
```

- [ ] **Step 6: Commit**
```bash
git add app/src/main/java/dev/agentic/domain/Node.kt \
        app/src/main/java/dev/agentic/domain/Transcript.kt \
        app/src/test/java/dev/agentic/domain/TranscriptPermTest.kt
git commit -m "feat(domain): PermNode/PlanNode + reducer for perm/plan/permResolved"
```

---

### Task B4: API + repository `respondPermission`

**Files:**
- Modify: `app/src/main/java/dev/agentic/data/net/AgenticApi.kt` (`:29` area)
- Modify: `app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt` (`:135` area)
- Modify: `app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt` (`:150` area)

**Interfaces:**
- Consumes: `PermDecisionReq` (B1), `POST /api/sessions/:id/permission` (Task A7).
- Produces: `SessionsRepository.respondPermission(id, decision, feedback?)`.

- [ ] **Step 1: Interface** — in `AgenticApi.kt`, after `interrupt` (`:28`):
```kotlin
    /** Answer a parked permission/plan prompt for [id] (allow/deny, optional feedback). */
    suspend fun respondPermission(id: String, decision: String, feedback: String? = null)
```

- [ ] **Step 2: Ktor impl** — in `KtorAgenticApi.kt`, after `interrupt` (`:135`):
```kotlin
    override suspend fun respondPermission(id: String, decision: String, feedback: String?) {
        client.post("$baseUrl/api/sessions/$id/permission") {
            auth(); contentType(ContentType.Application.Json); setBody(PermDecisionReq(decision, feedback))
        }
    }
```

- [ ] **Step 3: Repository passthrough** — in `SessionsRepository.kt`, after `interrupt` (`:150`):
```kotlin
    /** Answer a parked permission/plan prompt; best-effort like interrupt (no transcript reseed needed —
     *  the live stream delivers the permResolved marker). */
    suspend fun respondPermission(id: String, decision: String, feedback: String? = null) {
        runCatchingOutcome { api.respondPermission(id, decision, feedback) }
    }
```

- [ ] **Step 4: Commit**
```bash
git add app/src/main/java/dev/agentic/data/net/AgenticApi.kt \
        app/src/main/java/dev/agentic/data/net/KtorAgenticApi.kt \
        app/src/main/java/dev/agentic/data/repo/SessionsRepository.kt
git commit -m "feat(api-client): respondPermission -> POST /permission"
```
(If the project has fakes/mocks implementing `AgenticApi` — e.g. in `app/src/test` — add the new `respondPermission` override there too, or the test sources won't compile. Grep `: AgenticApi` and `override.*interrupt` to find them.)

---

### Task B5: ViewModel `respondPermission` + optimistic decided overlay

**Files:**
- Modify: `app/src/main/java/dev/agentic/ui/session/SessionViewModel.kt` (`Local` `:111-126`, `buildUiState` `:152-218`, events region `:351-401`)
- Test: `app/src/test/java/dev/agentic/ui/session/SessionViewModelPermTest.kt` (create)

**Interfaces:**
- Consumes: `PermNode`/`PlanNode` (B3), `SessionsRepository.respondPermission` (B4).
- Produces: `SessionViewModel.respondPermission(id, decision, feedback?)`.

- [ ] **Step 1: Write the unit test** (run in B7) — create `SessionViewModelPermTest.kt`, mirroring the existing `answerAsk` test pattern in this module: drive a session whose transcript holds a `PermNode`, call `respondPermission`, assert the repo received the call and the node shows decided optimistically. (Reuse the test's existing `SessionsRepository`/fake-api harness used by the ask tests.)

- [ ] **Step 2: Add the optimistic overlay field** — in `Local` (`:123`), after `answeredOverrides`:
```kotlin
        /** Optimistic perm/plan decisions, keyed by the request id, applied until the backend's
         *  permResolved marker lands (harmless once both agree). */
        val permDecided: Map<String, String> = emptyMap(),
```

- [ ] **Step 3: Apply it in `buildUiState`** — after the `answeredOverrides` block (`:181`):
```kotlin
        // 3b. Optimistic permission decisions: mark matching Perm/Plan nodes decided.
        if (l.permDecided.isNotEmpty()) {
            nodes = nodes.map { n ->
                when (n) {
                    is PermNode -> l.permDecided[n.id]?.let { n.copy(decided = true, decision = it) } ?: n
                    is PlanNode -> l.permDecided[n.id]?.let { n.copy(decided = true, decision = it) } ?: n
                    else -> n
                }
            }
        }
```
Add the imports `import dev.agentic.domain.PermNode` and `import dev.agentic.domain.PlanNode` at the top.

- [ ] **Step 4: Add the event** — after `answerAsk` (`:376`):
```kotlin
    /** Answer a parked permission/plan prompt: optimistically mark the card decided, then POST it.
     *  feedback is the deny reason / plan-revision note (null on a plain allow). */
    fun respondPermission(id: String, decision: String, feedback: String? = null) {
        local.update { it.copy(permDecided = it.permDecided + (id to decision)) }
        viewModelScope.launch { sessionsRepo.respondPermission(id, decision, feedback) }
    }
```

- [ ] **Step 5: Commit**
```bash
git add app/src/main/java/dev/agentic/ui/session/SessionViewModel.kt \
        app/src/test/java/dev/agentic/ui/session/SessionViewModelPermTest.kt
git commit -m "feat(session-vm): respondPermission + optimistic decided overlay"
```

---

### Task B6: Permission & plan cards + screen wiring

**Files:**
- Modify: `app/src/main/java/dev/agentic/ui/session/Transcript.kt` (render `when` `:198-294`, stable key `:331-344`, new composables near `AskCardView` `:561`)
- Modify: `app/src/main/java/dev/agentic/ui/session/SessionScreen.kt` (`Transcript(...)` call `:324-334`)

**Interfaces:**
- Consumes: `PermNode`/`PlanNode` (B3), `SessionViewModel.respondPermission` (B5).

- [ ] **Step 1: Extend the `Transcript` signature** — in `Transcript.kt`, add a callback param to `fun Transcript(...)` (after `onAnswerAsk` `:140`):
```kotlin
    onRespondPermission: (id: String, decision: String, feedback: String?) -> Unit,
```
Add imports `import dev.agentic.domain.PermNode` and `import dev.agentic.domain.PlanNode`.

- [ ] **Step 2: Render the new nodes** — in the item `when (val node = nodes[i])` (after the `is AskNode` branch `:284`):
```kotlin
                    is PermNode -> PermCardView(
                        node = node,
                        canAnswer = canAnswer && !node.decided,
                        onRespond = { decision, feedback -> onRespondPermission(node.id, decision, feedback) },
                    )

                    is PlanNode -> PlanCardView(
                        node = node,
                        canAnswer = canAnswer && !node.decided,
                        onRespond = { decision, feedback -> onRespondPermission(node.id, decision, feedback) },
                    )
```
Add stable keys in `stableNodeKey` (`:344`, before the closing `}`):
```kotlin
    is PermNode       -> "perm:$i:${node.id}:${node.decided}"
    is PlanNode       -> "plan:$i:${node.id}:${node.decided}"
```

- [ ] **Step 3: Add the card composables** — in `Transcript.kt`, after `AskCardView`/`buildCombinedAnswer` (`:712`):
```kotlin
/**
 * Permission card: a tool the agent wants to run in `default`/`acceptEdits` mode. Shows the tool call
 * summary + Allow / Deny, with an optional reason field that is sent as the deny feedback. Once decided
 * it collapses to a status line. Mirrors AskCardView's non-selectable, full-width card style.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PermCardView(
    node: PermNode,
    canAnswer: Boolean,
    onRespond: (decision: String, feedback: String?) -> Unit,
) {
    var reason by remember(node.id) { mutableStateOf("") }
    DisableSelection {
        Card(
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
            shape = MaterialTheme.shapes.large,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Icon(Icons.Rounded.Code, null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(18.dp))
                    Text(
                        node.summary.ifBlank { node.tool },
                        style = MaterialTheme.typography.bodyLarge,
                        fontWeight = FontWeight.SemiBold,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                }
                if (node.decided) {
                    Text(
                        if (node.decision == "allow") "Allowed" else "Denied",
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    OutlinedTextField(
                        value = reason,
                        onValueChange = { reason = it },
                        label = { Text("Reason (sent on Deny)…") },
                        singleLine = true,
                        enabled = canAnswer,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.align(Alignment.End)) {
                        androidx.compose.material3.TextButton(
                            onClick = { onRespond("deny", reason.ifBlank { null }) },
                            enabled = canAnswer,
                        ) { Text("Deny") }
                        Button(
                            onClick = { onRespond("allow", null) },
                            enabled = canAnswer,
                        ) { Text("Allow") }
                    }
                    if (!canAnswer) Text(
                        "This session can't take a response right now.",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        }
    }
}

/**
 * Plan-approval card (plan mode): renders the proposed plan markdown with Approve & run / Keep planning.
 * Approve sends allow (the backend switches the session to `default`, so subsequent tools prompt);
 * Keep planning sends deny with the typed feedback so Claude revises without leaving plan mode.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PlanCardView(
    node: PlanNode,
    canAnswer: Boolean,
    onRespond: (decision: String, feedback: String?) -> Unit,
) {
    var feedback by remember(node.id) { mutableStateOf("") }
    DisableSelection {
        Card(
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
            shape = MaterialTheme.shapes.large,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("Plan", style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
                // Plan body is selectable prose — wrap only the markdown back into a SelectionContainer.
                SelectionContainer { MarkdownText(node.plan) }
                if (node.decided) {
                    Text(
                        if (node.decision == "allow") "Approved — executing" else "Sent back for changes",
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    OutlinedTextField(
                        value = feedback,
                        onValueChange = { feedback = it },
                        label = { Text("Feedback (sent on Keep planning)…") },
                        singleLine = false,
                        enabled = canAnswer,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.align(Alignment.End)) {
                        androidx.compose.material3.TextButton(
                            onClick = { onRespond("deny", feedback.ifBlank { null }) },
                            enabled = canAnswer,
                        ) { Text("Keep planning") }
                        Button(
                            onClick = { onRespond("allow", null) },
                            enabled = canAnswer,
                        ) { Text("Approve & run") }
                    }
                    if (!canAnswer) Text(
                        "This session can't take a response right now.",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        }
    }
}
```

- [ ] **Step 4: Wire the screen** — in `SessionScreen.kt`, add to the `Transcript(...)` call (`:326`, after `onAnswerAsk`):
```kotlin
                    onRespondPermission = { id, decision, feedback -> realVm.respondPermission(id, decision, feedback) },
```

- [ ] **Step 5: Commit**
```bash
git add app/src/main/java/dev/agentic/ui/session/Transcript.kt \
        app/src/main/java/dev/agentic/ui/session/SessionScreen.kt
git commit -m "feat(session-ui): permission + plan approval cards"
```
(If there are other `Transcript(` call sites — e.g. a preview/`@Preview` or the wide/adaptive layout — grep `Transcript(` under `app/src/main` and add the `onRespondPermission = { _, _, _ -> }` arg so they compile.)

---

### Task B7: Build, test, deliver APK, and manual smoke test

This is the verification task — it runs in the **main checkout**, not the worktree.

- [ ] **Step 1: Push both repos to master**
```bash
cd /home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev && git push origin HEAD:master
cd /home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev-android && git push origin HEAD:master
```
(If a push is rejected: `git pull --rebase origin master && git push origin HEAD:master`.)

- [ ] **Step 2: Run the Android unit tests in the main checkout**
```bash
cd ~/src/agentic-dev-android && git pull --ff-only origin master
~/.local/share/gradle-8.10.2/bin/gradle testReleaseUnitTest
```
Expected: PASS (the B2/B3/B5 unit tests). Fix any compile/test failures in the worktree, recommit, re-push, repeat.

- [ ] **Step 3: Deploy the backend** and confirm it boots with the new routes (per `agentic-dev/docs/internals.md` deploy notes). Confirm `yarn test` is green on master.

- [ ] **Step 4: Build + sign the release APK and deliver it**
```bash
cd ~/src/agentic-dev-android
~/.local/share/gradle-8.10.2/bin/gradle assembleRelease
mkdir -p /home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev-android/outbox
cp app/build/outputs/apk/release/app-release.apk \
   "/home/arcatva/src/agentic-worktrees/bcb82bf5-7aff-4230-b660-88418c217606/agentic-dev-android/outbox/$(date +%Y%m%d-%H%M).apk"
```

- [ ] **Step 5: Manual smoke test (the spec's verification gate — fake claude can't prove the real SDK semantics).** Install the APK against the deployed backend and, for each mode:
  1. **Ask** — start a session that runs a tool; confirm a permission card appears and the turn waits; Allow resumes; Deny (with a reason) is reflected to Claude.
  2. **Plan** — confirm Claude stays read-only and a plan-approval card appears; Approve continues execution with per-tool prompts (now in `default`); Keep-planning sends feedback and stays in plan.
  3. **Accept edits** — an edit applies with no card; a Bash call surfaces a card.
  4. **Dangerous** — identical to today (no cards); `AskUserQuestion` still works.
  Also verify a card stays **decided** after backgrounding/reopening the session (reseed durability via `agentic_perm_resolved`).
  If the real SDK's `canUseTool`/`setPermissionMode`/`ExitPlanMode` behavior differs from the tests, record it and adjust Task A5 (the parking + synthetic-line mechanism is unaffected; only the SDK call details may change).

- [ ] **Step 6: Final commit (if the smoke test required fixes)** and re-deliver the APK.

---

## Self-Review

**Spec coverage:** field + value space (A1) ✓; backend applies the mode via CLI + SDK option (A2, A5) ✓; `POST /api/sessions` + template accept it (A3) ✓; interactive perm round-trip with synthetic line + dedicated endpoint (A4–A7) ✓; plan approval → continue in `default` (A5 `setPermissionMode("default")`) ✓; reseed durability (`agentic_perm_resolved` + rendered-log prefix, A4) — a refinement beyond the spec's sketch, noted in the plan header ✓; Android picker as a 4-notch slider (B2) ✓; per-tool allow/deny + plan cards (B6) ✓; watchdog exemption (A6) ✓; CLAUDE.md driver-invariant update (A2) ✓; manual smoke gate (B7) ✓.

**Type consistency:** event kinds `perm`/`plan`/`permResolved` and synthetic line shapes match across `streamParser.ts` (A4), `sdkRunner.ts` (A5), `engine.ts` (A6), and the Android reducer (B3). `respondPermission(decision, feedback?)` has the same `"allow" | "deny"` + optional-string signature in `RunHandle` (A5) → `SpawnHandle` (A6) → `engine` (A6) → `AgenticApi`/repo (B4) → ViewModel (B5) → card callbacks (B6). `permissionMode` value space (`plan|default|acceptEdits|bypassPermissions`, null=bypass) is identical in `normalizePermissionMode` (A1), `buildSpec` (A2), `sdkRunner` `interactivePerms` (A5), and `PERMISSION_MODES` (B2, with the `bypassPermissions`↔null mapping at the VM boundary).

**No placeholders:** every step carries the concrete code/diff and exact path; the only "follow the existing harness" notes are for adding assertions to already-established test files (`engine.test.ts`, `server.test.ts`) and for fakes that implement `AgenticApi`, where the surrounding scaffolding already exists.
