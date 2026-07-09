# Session List Backend Implementation Plan (agentic-dev)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose two new signals on `GET /api/sessions` — a `workflowRunning` flag (a session whose background workflow is still running) and `lastUserMessageAt` (sort key), and order the list by the latter.

**Architecture:** All changes are in the backend repo `agentic-dev` (TypeScript/Node, API-only). `lastUserMessageAt` is a new persisted sqlite column, set on session create and on every follow-up user message, and used by `store.list()`'s `ORDER BY`. `workflowRunning` is a runtime-only field (not persisted) computed in `engine.withActivity()` by reusing the existing `listWorkflows()` file-scan, gated to finished/idle sessions to bound cost. The serialized `Session` already flows verbatim to the client, so no route changes are needed.

**Tech Stack:** TypeScript (ESM, NodeNext, `.js` import suffixes), better-sqlite3, Vitest 2.1.5, Fastify (untouched here).

## Global Constraints

- Use `yarn`, never `npm install`. Run one test file: `yarn test <path>`. Run a single case: `yarn test <path> -t "<name>"`. Run all: `yarn test`. Typecheck: `yarn build`.
- ESM: every local import ends in `.js` (e.g. `import { listWorkflows } from "./workflows.js"`).
- `server/engine/` must stay free of Fastify imports (keep it unit-testable in isolation).
- Never hit the real `claude` in tests — use `server/test/fixtures/fake-claude.sh` (local runner) / `fake-claude-stream.sh` (streaming).
- All tests green (`yarn test`) before each commit.
- Session list ordering is server-authoritative (the Android client shows server order as-is).
- Invariant: `lastUserMessageAt` is always ≥ `createdAt` and is never NULL after `migrate()`.
- Working directory for all commands: `/home/arcatva/src/agentic-worktrees/02e74d6e-44e0-49d6-be42-1ae988440f6e/agentic-dev`.

---

### Task A1: Add the `lastUserMessageAt` column

**Files:**
- Modify: `server/engine/types.ts` (Session interface, after `endedAt`)
- Modify: `server/engine/store.ts` (`UPDATABLE`, `ADDED_COLUMNS`, `migrate()`, `create()`, `rowToSession()`)
- Test: `server/engine/store.test.ts`

**Interfaces:**
- Produces: `Session.lastUserMessageAt: number` (persisted column); `store.create()` initialises it to `createdAt`; `store.update(id, { lastUserMessageAt })` is allowed; `rowToSession` never returns NULL for it.

- [ ] **Step 1: Write the failing tests**

Add these two tests inside the `describe("SqliteStore", ...)` block in `server/engine/store.test.ts` (the file already imports `Database`, `mkdtempSync`, `rmSync`, `tmpdir`, `join`):

```typescript
  it("sets lastUserMessageAt equal to createdAt on create", () => {
    const s = store.create({ id: "s1", repo: "demo", prompt: "do x", worktreePath: "/wt/s1", branch: "agentic/s1", baseSha: null });
    expect(s.lastUserMessageAt).toBe(s.createdAt);
    expect(store.get("s1")?.lastUserMessageAt).toBe(s.createdAt);
  });

  it("backfills lastUserMessageAt from createdAt for legacy rows on open", () => {
    const dir2 = mkdtempSync(join(tmpdir(), "agentic-store-mig-"));
    const dbPath = join(dir2, "db.sqlite");
    // Simulate an old DB that predates the column: a sessions table without lastUserMessageAt.
    const raw = new Database(dbPath);
    raw.exec(`CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, createdAt INTEGER, seq INTEGER)`);
    raw.prepare(`INSERT INTO sessions (id,status,createdAt,seq) VALUES (?,?,?,?)`).run("old", "done", 12345, 0);
    raw.close();
    // Opening through SqliteStore runs migrate() → ADDs the column AND backfills it from createdAt.
    const migStore = new SqliteStore(dbPath, join(dir2, "logs"));
    migStore.close();
    const raw2 = new Database(dbPath);
    const row = raw2.prepare(`SELECT lastUserMessageAt FROM sessions WHERE id = 'old'`).get() as any;
    expect(row.lastUserMessageAt).toBe(12345);
    raw2.close();
    rmSync(dir2, { recursive: true, force: true });
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `yarn test server/engine/store.test.ts -t "lastUserMessageAt"`
Expected: FAIL — first test fails (`lastUserMessageAt` is `undefined`); second fails (`no such column: lastUserMessageAt`).

- [ ] **Step 3: Add the field to the Session type**

In `server/engine/types.ts`, in the `Session` interface, add the new field immediately after the `endedAt: number | null;` line:

```typescript
  createdAt: number;
  startedAt: number | null;
  endedAt: number | null;
  /** Epoch ms of the most recent USER message (initial prompt or any follow-up). Drives the session
   *  list sort (most-recently-messaged first). Initialised to createdAt; bumped on every user turn. */
  lastUserMessageAt: number;
```

- [ ] **Step 4: Add the column, migration backfill, create init, update allow-list, and row mapping**

In `server/engine/store.ts`:

(a) Add to the `UPDATABLE` set (so `engine.update` may set it on follow-ups):

```typescript
const UPDATABLE = new Set([
  "repo","prompt","worktreePath","branch","claudeSessionId","status",
  "costUsd","exitCode","error","errorKind","createdAt","startedAt","endedAt","worktreeState",
  "lastUserMessageAt",
]);
```

(b) Add to `ADDED_COLUMNS` (so old DBs get the column via `ALTER TABLE`):

```typescript
const ADDED_COLUMNS: Array<[name: string, decl: string]> = [
  ["baseSha", "TEXT"],
  ["worktreeState", "TEXT DEFAULT 'live'"],
  ["repos", "TEXT"],
  ["skills", "TEXT"],
  ["baseShas", "TEXT"],
  ["model", "TEXT"],
  ["effort", "TEXT"],
  ["mode", "TEXT"],
  ["errorKind", "TEXT"],
  ["lastUserMessageAt", "INTEGER"],
];
```

(c) Backfill in `migrate()` — add the `UPDATE` after the ALTER loop:

```typescript
  private migrate(): void {
    const have = new Set(
      (this.db.prepare(`PRAGMA table_info(sessions)`).all() as Array<{ name: string }>).map((c) => c.name)
    );
    for (const [name, decl] of ADDED_COLUMNS) {
      if (!have.has(name)) this.db.exec(`ALTER TABLE sessions ADD COLUMN ${name} ${decl}`);
    }
    // New rows set lastUserMessageAt on insert; backfill legacy rows so the sort key is never NULL.
    this.db.exec(`UPDATE sessions SET lastUserMessageAt = createdAt WHERE lastUserMessageAt IS NULL`);
  }
```

(d) In `create()`, compute a single `now` and set both timestamps from it; add the column to the INSERT:

```typescript
  create(input: CreateInput): Session {
    const repos = input.repos ?? (input.repo ? [input.repo] : []);
    const skills = input.skills ?? [];
    const baseShas = input.baseShas ?? (input.repo ? { [input.repo]: input.baseSha ?? null } : {});
    const repo = repos[0] ?? "";
    const baseSha = repo in baseShas ? baseShas[repo] : (input.baseSha ?? null);
    const now = Date.now();
    const s: Session = {
      id: input.id, repo, repos, skills, prompt: input.prompt,
      model: input.model ?? null, effort: input.effort ?? null, mode: input.mode ?? null,
      worktreePath: input.worktreePath, branch: input.branch, baseSha, baseShas,
      claudeSessionId: null, status: "pending", costUsd: null, exitCode: null, error: null, errorKind: null,
      createdAt: now, startedAt: null, endedAt: null,
      lastUserMessageAt: now,
      worktreeState: "live",
    };
    this.db
      .prepare(
        `INSERT INTO sessions
         (id,repo,repos,skills,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,seq)
         VALUES (@id,@repo,@repos,@skills,@prompt,@worktreePath,@branch,@claudeSessionId,@status,@costUsd,@exitCode,@error,@errorKind,@createdAt,@startedAt,@endedAt,@lastUserMessageAt,@baseSha,@baseShas,@worktreeState,@model,@effort,@mode,@seq)`
      )
      .run({ ...s, repos: JSON.stringify(repos), skills: JSON.stringify(skills), baseShas: JSON.stringify(baseShas), seq: this.seq++ });
    return s;
  }
```

(e) In `rowToSession()`, map the column (fall back to `createdAt` for any row not yet backfilled), adding the line after `endedAt`:

```typescript
    createdAt: r.createdAt, startedAt: r.startedAt, endedAt: r.endedAt,
    lastUserMessageAt: r.lastUserMessageAt ?? r.createdAt,
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `yarn test server/engine/store.test.ts`
Expected: PASS (all store tests, including the two new ones).

- [ ] **Step 6: Typecheck**

Run: `yarn build`
Expected: no type errors (the `Session` interface now requires `lastUserMessageAt`, which `create()` and `rowToSession()` both provide).

- [ ] **Step 7: Commit**

```bash
git add server/engine/types.ts server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(store): add lastUserMessageAt column with backfill

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task A2: Order the list by `lastUserMessageAt`

**Files:**
- Modify: `server/engine/store.ts` (`list()` ORDER BY)
- Test: `server/engine/store.test.ts`

**Interfaces:**
- Consumes: `Session.lastUserMessageAt` (Task A1).
- Produces: `store.list()` ordered by most-recent user message first, `seq DESC` tie-break.

- [ ] **Step 1: Write the failing test**

Add inside `describe("SqliteStore", ...)` in `server/engine/store.test.ts`:

```typescript
  it("orders list() by lastUserMessageAt (most recent first), not createdAt", () => {
    const a = store.create({ id: "a", repo: "demo", prompt: "a", worktreePath: "/wt/a", branch: "agentic/a", baseSha: null });
    const b = store.create({ id: "b", repo: "demo", prompt: "b", worktreePath: "/wt/b", branch: "agentic/b", baseSha: null });
    // Default order = newest-created first (b before a), since lastUserMessageAt == createdAt.
    expect(store.list().map((s) => s.id)).toEqual(["b", "a"]);
    // Bump a's last-message time past b → a now sorts first.
    store.update("a", { lastUserMessageAt: b.createdAt + 10_000 });
    expect(store.list().map((s) => s.id)).toEqual(["a", "b"]);
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `yarn test server/engine/store.test.ts -t "orders list"`
Expected: FAIL on the second assertion — `["b","a"]` is returned because `list()` still orders by `createdAt`.

- [ ] **Step 3: Change the ORDER BY**

In `server/engine/store.ts`, `list()`:

```typescript
  list(): Session[] {
    const rows = this.db.prepare(`SELECT * FROM sessions ORDER BY COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC`).all() as any[];
    return rows.map((r) => this.rowToSession(r));
  }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `yarn test server/engine/store.test.ts -t "orders list"`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(store): order session list by lastUserMessageAt

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task A3: Stamp `lastUserMessageAt` on follow-up turns

**Files:**
- Modify: `server/engine/engine.ts` (`followUp()` — both the live-injection branch and the queued branch)
- Test: `server/engine/engine.test.ts` (queued path), `server/engine/engine.streaming.test.ts` (live-injection path)

**Interfaces:**
- Consumes: `store.update(id, { lastUserMessageAt })` (Task A1).
- Produces: after any follow-up, `engine.get(id).lastUserMessageAt` is the time of that follow-up.

- [ ] **Step 1: Write the failing tests**

(a) In `server/engine/engine.test.ts`, add inside the top-level `describe(...)` block (the file already defines `makeEngine`, `waitForStatus`, `makeTempGitRepo`, `cleanup`):

```typescript
  it("bumps lastUserMessageAt on a queued follow-up turn", async () => {
    const { mkdtempSync, renameSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src);
    const id = engine.submit("demo", "first");
    await waitForStatus(engine, id, "done");
    const t1 = engine.get(id)!.lastUserMessageAt;
    engine.followUp(id, "second");
    await waitForStatus(engine, id, "running");
    await waitForStatus(engine, id, "done");
    const t2 = engine.get(id)!.lastUserMessageAt;
    expect(t2).toBeGreaterThan(t1);
  });
```

(b) In `server/engine/engine.streaming.test.ts`, add inside the `describe("Engine streaming (Pass 1)", ...)` block (the file already defines `makeStreamingEngine`, `waitFor`):

```typescript
  it("bumps lastUserMessageAt when a follow-up is injected over stdin", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-src-")); cleanup.push(src);
    const engine = makeStreamingEngine(src);
    let results = 0;
    const id = engine.submitSession([], [], "first turn");
    engine.subscribe(id, (e: ClaudeEvent) => { if (e.kind === "result") results++; });
    await waitFor(() => results >= 1);
    const t1 = engine.get(id)!.lastUserMessageAt;
    engine.followUp(id, "second turn");
    await waitFor(() => results >= 2);
    const t2 = engine.get(id)!.lastUserMessageAt;
    expect(t2).toBeGreaterThan(t1);
    engine.kill(id);
    await waitFor(() => engine.get(id)!.status === "killed");
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `yarn test server/engine/engine.test.ts -t "queued follow-up"` then `yarn test server/engine/engine.streaming.test.ts -t "injected over stdin"`
Expected: FAIL — `t2` equals `t1` (follow-ups don't yet touch `lastUserMessageAt`).

- [ ] **Step 3: Stamp the field in both `followUp()` branches**

In `server/engine/engine.ts`, `followUp()`:

(a) The **live-injection** branch (inside `if (this.running.has(id)) { ... }`) already computes `now`. Add `lastUserMessageAt: now` to its existing `store.update`:

```typescript
    this.store.update(id, { ...(setTitle ? { prompt } : {}), error: null, errorKind: null, exitCode: null, lastUserMessageAt: now });
```

(b) The **queued** branch (after the `if (this.isBusy(...)) throw` guard) has no `now` in scope. Compute one and add the field to its `store.update`:

```typescript
    if (this.isBusy(id, s.status)) {
      throw new Error("session busy");
    }
    const now = (this.cfg.nowFn ?? Date.now)();
    this.store.update(id, { ...(setTitle ? { prompt } : {}), status: "pending", lastUserMessageAt: now });
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `yarn test server/engine/engine.test.ts -t "queued follow-up"` then `yarn test server/engine/engine.streaming.test.ts -t "injected over stdin"`
Expected: PASS.

- [ ] **Step 5: Run the full engine suites to confirm no regressions**

Run: `yarn test server/engine/engine.test.ts server/engine/engine.streaming.test.ts`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/engine/engine.ts server/engine/engine.test.ts server/engine/engine.streaming.test.ts
git commit -m "feat(engine): stamp lastUserMessageAt on follow-up turns

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task A4: `hasActiveWorkflow()` helper

**Files:**
- Modify: `server/engine/workflows.ts` (add exported `hasActiveWorkflow` + `WORKFLOW_TERMINAL`)
- Test: `server/engine/workflows.test.ts`

**Interfaces:**
- Consumes: existing `listWorkflows(base: string): WorkflowRun[]`.
- Produces: `hasActiveWorkflow(base: string): boolean` — true iff any run under `base` is non-terminal.

- [ ] **Step 1: Write the failing tests**

Add inside `describe("workflows", ...)` in `server/engine/workflows.test.ts` (the file already defines `seed()`, `dirs`, and imports `mkdtempSync`, `mkdirSync`, `writeFileSync`, `join`, `tmpdir`). Also add `hasActiveWorkflow` to the import on line 5:

```typescript
import { listWorkflows, readWorkflowAgent, hasActiveWorkflow } from "./workflows.js";
```

```typescript
  it("hasActiveWorkflow: true for an in-flight run", () => {
    const base = mkdtempSync(join(tmpdir(), "agentic-wf-")); dirs.push(base);
    const rd = join(base, "projects", "-slug", "sess", "subagents", "workflows", "wf_live");
    mkdirSync(rd, { recursive: true });
    writeFileSync(join(rd, "agent-a1.meta.json"), JSON.stringify({ agentType: "workflow-subagent" }));
    expect(hasActiveWorkflow(base)).toBe(true);
  });

  it("hasActiveWorkflow: false when only a completed summary exists", () => {
    expect(hasActiveWorkflow(seed())).toBe(false);   // seed() writes a status:"completed" summary
  });

  it("hasActiveWorkflow: false when nothing exists", () => {
    const base = mkdtempSync(join(tmpdir(), "agentic-wf-")); dirs.push(base);
    expect(hasActiveWorkflow(base)).toBe(false);
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `yarn test server/engine/workflows.test.ts -t "hasActiveWorkflow"`
Expected: FAIL — `hasActiveWorkflow is not a function` (not yet exported).

- [ ] **Step 3: Implement the helper**

In `server/engine/workflows.ts`, add near the top (after the `WorkflowRun` interface, before `projectDirs`):

```typescript
/** Workflow run statuses that mean the run has finished (normalised: trim + lowercase). Mirrors the
 *  client's WORKFLOW_DONE set. An in-flight run synthesised by readRunningRun() has status "running",
 *  which is NOT in this set. */
const WORKFLOW_TERMINAL = new Set(["done", "complete", "completed", "failed", "error", "killed", "cancelled", "canceled"]);

/** True iff this session config base has ANY workflow run that has not reached a terminal state. Reuses
 *  listWorkflows() (completed summaries win over the live journal, so a finished run whose subagents dir
 *  lingers is correctly counted as done). Cheap when there are no workflows: projectDirs() returns []. */
export function hasActiveWorkflow(base: string): boolean {
  return listWorkflows(base).some((r) => !WORKFLOW_TERMINAL.has(r.status.trim().toLowerCase()));
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `yarn test server/engine/workflows.test.ts`
Expected: PASS (all workflow tests, including the three new ones).

- [ ] **Step 5: Commit**

```bash
git add server/engine/workflows.ts server/engine/workflows.test.ts
git commit -m "feat(workflows): add hasActiveWorkflow helper

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task A5: Surface `workflowRunning` on finished/idle sessions

**Files:**
- Modify: `server/engine/types.ts` (Session interface — runtime-only optional field)
- Modify: `server/engine/engine.ts` (imports + `withActivity()`)
- Test: `server/engine/engine.test.ts`

**Interfaces:**
- Consumes: `hasActiveWorkflow(base)` (Task A4); `configBaseFor` rule = `join(s.worktreePath, ".claude-config")`.
- Produces: `Session.workflowRunning?: boolean` set to `true` on the API response when a finished/idle session has a live workflow.

- [ ] **Step 1: Write the failing test**

Add inside the top-level `describe(...)` in `server/engine/engine.test.ts`. It seeds an in-flight workflow under the finished session's `.claude-config` and asserts the flag (uses `mkdirSync`/`writeFileSync` from `node:fs`):

```typescript
  it("reports workflowRunning=true when a finished session has a live workflow", async () => {
    const { mkdtempSync, renameSync, mkdirSync, writeFileSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src);
    const id = engine.submit("demo", "first");
    await waitForStatus(engine, id, "done");
    expect(engine.get(id)!.workflowRunning).toBeFalsy();   // no workflow yet

    // Seed an in-flight workflow run under this session's config base.
    const base = join(engine.get(id)!.worktreePath, ".claude-config");
    const rd = join(base, "projects", "-slug", "sess", "subagents", "workflows", "wf_live");
    mkdirSync(rd, { recursive: true });
    writeFileSync(join(rd, "agent-a1.meta.json"), JSON.stringify({ agentType: "workflow-subagent" }));

    expect(engine.get(id)!.workflowRunning).toBe(true);
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `yarn test server/engine/engine.test.ts -t "workflowRunning"`
Expected: FAIL — `workflowRunning` is `undefined` after seeding (last assertion fails).

- [ ] **Step 3: Add the runtime-only field to the type**

In `server/engine/types.ts`, in the `Session` interface, add after the existing `awaitingInput?: boolean;` field:

```typescript
  awaitingInput?: boolean;   // (existing)
  /** Runtime-only (not persisted): true when the turn is finished/idle but a background workflow is
   *  still running, so the client shows the session as running. Set by engine.withActivity(). */
  workflowRunning?: boolean;
```

- [ ] **Step 4: Wire it into `withActivity()`**

In `server/engine/engine.ts`:

(a) Add the import at the top. `join` from `node:path` is **already imported** in `engine.ts` (used by `submitSession`) — do NOT add it again; add only:

```typescript
import { hasActiveWorkflow } from "./workflows.js";
```

(b) Replace `withActivity()` with:

```typescript
  private withActivity(s: Session): Session {
    const a = this.activity.get(s.id);
    let out = a ? { ...s, activity: a } : s;
    // Surface the idle/busy sub-state so the client can unlock the input box while a background
    // workflow is still running.
    const awaiting = this.awaiting.has(s.id) ? this.awaiting.get(s.id) : undefined;
    if (awaiting !== undefined) out = { ...out, awaitingInput: awaiting };
    // A background workflow can outlive the turn (session idle or done). Surface that as "running" to
    // the client. Only check finished/idle sessions — a mid-turn session is already running, and the
    // filesystem scan isn't free.
    const finishedOrIdle = s.status === "done" || s.status === "failed" || s.status === "killed" || awaiting === true;
    if (finishedOrIdle && hasActiveWorkflow(join(s.worktreePath, ".claude-config"))) {
      out = { ...out, workflowRunning: true };
    }
    return out;
  }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `yarn test server/engine/engine.test.ts -t "workflowRunning"`
Expected: PASS.

- [ ] **Step 6: Full suite + typecheck**

Run: `yarn test` then `yarn build`
Expected: PASS, no type errors.

- [ ] **Step 7: Commit**

```bash
git add server/engine/types.ts server/engine/engine.ts server/engine/engine.test.ts
git commit -m "feat(engine): surface workflowRunning on finished/idle sessions

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Done criteria (backend)

- `GET /api/sessions` returns sessions ordered by `lastUserMessageAt DESC`, each carrying `lastUserMessageAt` and (when applicable) `workflowRunning: true`.
- `yarn test` is fully green; `yarn build` is clean.
- The Android client (Plan B) consumes these fields; no route/DTO changes were needed because `withActivity()`'s session object is serialized verbatim by `GET /api/sessions`.
