# agentic-dev Slice 3 (Worktree review + cleanup) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a finished session's worktree be reviewed (diff) and cleaned up — Archive (keep the branch, free the checkout) or Discard (delete worktree + branch) — so worktrees stop accumulating. No auto-merge into real repos in v1.

**Architecture:** Additive. Sessions record a `baseSha` (repo HEAD at submit) and a `worktreeState` (`live`/`archived`/`discarded`). New `worktree.ts` helpers (`diffWorktree`/`archiveWorktree`/`discardWorktree`) wrap git; the engine exposes `diff`/`archive`/`discard`; three routes expose them; the UI shows a diff panel + Archive/Discard on terminal, live-worktree sessions.

**Tech Stack:** Same as Slices 1–2 (Node/TS, Fastify, Vitest, React/MUI). Tests drive the fake `claude` binary + temp git repos — zero API credit.

**Reference spec:** `docs/superpowers/specs/2026-06-16-agentic-dev-slice3-worktree-cleanup-design.md`

---

## File Structure (changes only)

```
server/engine/types.ts        # MODIFY: Session + baseSha + worktreeState
server/engine/store.ts        # MODIFY: columns/create/rowToSession + UPDATABLE
server/engine/worktree.ts     # MODIFY: + diffWorktree / archiveWorktree / discardWorktree
server/engine/engine.ts       # MODIFY: submit records baseSha; + diff/archive/discard
server/api/routes.ts          # MODIFY: + GET /diff, POST /archive, POST /discard
web/src/api.ts                # MODIFY: + diff/archive/discard; Session.worktreeState
web/src/pages/SessionView.tsx # MODIFY: diff panel + Archive/Discard
web/src/pages/Dashboard.tsx   # MODIFY: worktreeState chip
```

All Slice 1–2 tests must stay green. Use `yarn`; never `npm install`.

---

## Task 1: types + store (baseSha + worktreeState)

**Files:** Modify `server/engine/types.ts`, `server/engine/store.ts`, `server/engine/store.test.ts`

- [ ] **Step 1: types.ts** — add two fields to `Session` (after `branch`):

```ts
  branch: string;
  baseSha: string | null;
  worktreeState: "live" | "archived" | "discarded";
```

- [ ] **Step 2: Failing test** — append to `server/engine/store.test.ts`:

```ts
it("defaults worktreeState to live and stores baseSha", () => {
  store.create({ id: "s1", repo: "r", prompt: "p", worktreePath: "/w", branch: "b", baseSha: "abc123" });
  const s = store.get("s1")!;
  expect(s.baseSha).toBe("abc123");
  expect(s.worktreeState).toBe("live");
});
it("updates worktreeState", () => {
  store.create({ id: "s1", repo: "r", prompt: "p", worktreePath: "/w", branch: "b", baseSha: null });
  store.update("s1", { worktreeState: "archived" });
  expect(store.get("s1")!.worktreeState).toBe("archived");
});
```

- [ ] **Step 2b: Run → fail** (`yarn vitest run server/engine/store.test.ts`): type error / column missing.

- [ ] **Step 3: store.ts** — four edits:

(a) `CreateInput` add `baseSha: string | null`:
```ts
export interface CreateInput {
  id: string; repo: string; prompt: string; worktreePath: string; branch: string;
  baseSha: string | null;
}
```
(b) `COLUMNS` add the two columns (append before the closing backtick):
```ts
  createdAt INTEGER, startedAt INTEGER, endedAt INTEGER,
  baseSha TEXT, worktreeState TEXT`;
```
(c) In `create`, set the defaults on the `Session` object and add them to the INSERT:
```ts
    const s: Session = {
      ...input,
      claudeSessionId: null, status: "pending", costUsd: null, exitCode: null, error: null,
      createdAt: Date.now(), startedAt: null, endedAt: null,
      worktreeState: "live",
    };
```
and extend the INSERT column list + VALUES with `baseSha` and `worktreeState`:
```ts
      .prepare(
        `INSERT INTO sessions
         (id,repo,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,createdAt,startedAt,endedAt,baseSha,worktreeState,seq)
         VALUES (@id,@repo,@prompt,@worktreePath,@branch,@claudeSessionId,@status,@costUsd,@exitCode,@error,@createdAt,@startedAt,@endedAt,@baseSha,@worktreeState,@seq)`
      )
```
(d) `rowToSession` map the two new columns:
```ts
      createdAt: r.createdAt, startedAt: r.startedAt, endedAt: r.endedAt,
      baseSha: r.baseSha ?? null, worktreeState: (r.worktreeState ?? "live") as Session["worktreeState"],
```
(e) `UPDATABLE` set — add `worktreeState` (NOT baseSha; it's set once at create):
```ts
const UPDATABLE = new Set([
  "repo","prompt","worktreePath","branch","claudeSessionId","status",
  "costUsd","exitCode","error","createdAt","startedAt","endedAt","worktreeState",
]);
```

- [ ] **Step 4: Run → pass** (`yarn vitest run server/engine/store.test.ts`): all pass (note: other Slice-1 store tests call `create` without `baseSha` — **update those existing `create({...})` calls in store.test.ts to include `baseSha: null`** so they type-check).

- [ ] **Step 5: Commit**
```bash
git add server/engine/types.ts server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(engine): session baseSha + worktreeState"
```

---

## Task 2: worktree.ts — diff / archive / discard

**Files:** Modify `server/engine/worktree.ts`, `server/engine/worktree.test.ts`

- [ ] **Step 1: Failing tests** — append to `server/engine/worktree.test.ts`:

```ts
import { writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { diffWorktree, archiveWorktree, discardWorktree } from "./worktree.js";

describe("worktree review + cleanup", () => {
  it("diffWorktree shows uncommitted + untracked changes vs base", () => {
    const repo = makeTempGitRepo(); cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-")); cleanup.push(root);
    const base = execFileSync("git", ["-C", repo, "rev-parse", "HEAD"]).toString().trim();
    const { worktreePath } = createWorktree(repo, root, "demo", "d1");
    writeFileSync(join(worktreePath, "NEW.md"), "hello\n");           // untracked
    writeFileSync(join(worktreePath, "README.md"), "changed\n");      // modified
    const diff = diffWorktree(worktreePath, base);
    expect(diff).toContain("NEW.md");
    expect(diff).toContain("README.md");
    expect(diff).toContain("hello");
  });

  it("archiveWorktree commits dirty changes then removes the worktree, keeping the branch", () => {
    const repo = makeTempGitRepo(); cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-")); cleanup.push(root);
    const { worktreePath, branch } = createWorktree(repo, root, "demo", "d2");
    writeFileSync(join(worktreePath, "WORK.md"), "work\n");
    archiveWorktree(repo, worktreePath, branch, "session d2");
    expect(existsSync(worktreePath)).toBe(false);                     // worktree gone
    expect(execFileSync("git", ["-C", repo, "branch", "--list", branch]).toString()).toContain(branch); // branch kept
    expect(execFileSync("git", ["-C", repo, "log", "-1", "--format=%s", branch]).toString()).toContain("session d2"); // committed
  });

  it("discardWorktree removes the worktree and deletes the branch", () => {
    const repo = makeTempGitRepo(); cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-")); cleanup.push(root);
    const { worktreePath, branch } = createWorktree(repo, root, "demo", "d3");
    writeFileSync(join(worktreePath, "X.md"), "x\n");
    discardWorktree(repo, worktreePath, branch);
    expect(existsSync(worktreePath)).toBe(false);
    expect(execFileSync("git", ["-C", repo, "branch", "--list", branch]).toString()).toBe("");
  });

  it("archive/discard are idempotent if the worktree path is already gone", () => {
    const repo = makeTempGitRepo(); cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-")); cleanup.push(root);
    const { worktreePath, branch } = createWorktree(repo, root, "demo", "d4");
    discardWorktree(repo, worktreePath, branch);
    expect(() => discardWorktree(repo, worktreePath, branch)).not.toThrow();
  });
});
```

(`mkdtempSync`, `existsSync`, `tmpdir`, `join`, `cleanup`, `createWorktree`, `makeTempGitRepo` are already imported/defined in this file from Slice 1.)

- [ ] **Step 1b: Run → fail** (functions undefined).

- [ ] **Step 2: worktree.ts** — append these functions:

```ts
import { existsSync } from "node:fs";

/** Unified diff of everything changed in the worktree vs baseSha (incl. uncommitted + untracked). */
export function diffWorktree(worktreePath: string, baseSha: string): string {
  // intent-to-add so untracked files appear in the diff without fully staging
  execFileSync("git", ["-C", worktreePath, "add", "-AN"], { stdio: "pipe" });
  return execFileSync("git", ["-C", worktreePath, "--no-pager", "diff", baseSha], {
    stdio: "pipe", maxBuffer: 64 * 1024 * 1024,
  }).toString();
}

/** Commit any uncommitted changes onto the branch, then remove the worktree (branch kept). Idempotent. */
export function archiveWorktree(repoPath: string, worktreePath: string, branch: string, message: string): void {
  if (existsSync(worktreePath)) {
    const status = execFileSync("git", ["-C", worktreePath, "status", "--porcelain"], { stdio: "pipe" }).toString();
    if (status.trim()) {
      execFileSync("git", ["-C", worktreePath, "add", "-A"], { stdio: "pipe" });
      execFileSync("git", ["-C", worktreePath, "commit", "-m", message], { stdio: "pipe" });
    }
    execFileSync("git", ["-C", repoPath, "worktree", "remove", worktreePath], { stdio: "pipe" });
  } else {
    execFileSync("git", ["-C", repoPath, "worktree", "prune"], { stdio: "pipe" });
  }
}

/** Remove the worktree (force) and delete the branch. Idempotent. */
export function discardWorktree(repoPath: string, worktreePath: string, branch: string): void {
  if (existsSync(worktreePath)) {
    execFileSync("git", ["-C", repoPath, "worktree", "remove", "--force", worktreePath], { stdio: "pipe" });
  } else {
    execFileSync("git", ["-C", repoPath, "worktree", "prune"], { stdio: "pipe" });
  }
  // delete the branch if it still exists
  try { execFileSync("git", ["-C", repoPath, "branch", "-D", branch], { stdio: "pipe" }); } catch { /* already gone */ }
}
```

(`execFileSync` is already imported at the top of worktree.ts; add the `existsSync` import if not present.)

- [ ] **Step 3: Run → pass** (4 new tests).
- [ ] **Step 4: Commit**
```bash
git add server/engine/worktree.ts server/engine/worktree.test.ts
git commit -m "feat(engine): worktree diff/archive/discard helpers"
```

---

## Task 3: engine — record baseSha + diff/archive/discard

**Files:** Modify `server/engine/engine.ts`, `server/engine/engine.test.ts`

- [ ] **Step 1: Failing tests** — append to `server/engine/engine.test.ts` (inside the existing describe or a new one):

```ts
describe("Engine worktree cleanup", () => {
  async function doneSession() {
    const { mkdtempSync, renameSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-")); cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src);
    const id = engine.submit("demo", "go");
    await waitForStatus(engine, id, "done");
    return { engine, id };
  }

  it("submit records a non-null baseSha", async () => {
    const { engine, id } = await doneSession();
    expect(engine.get(id)!.baseSha).toMatch(/^[0-9a-f]{7,40}$/);
  });

  it("diff returns the agent's change", async () => {
    const { engine, id } = await doneSession();
    // the fake binary doesn't write files, so write one into the worktree to simulate agent output
    const { writeFileSync } = await import("node:fs");
    writeFileSync(join(engine.get(id)!.worktreePath, "OUT.md"), "out\n");
    expect(engine.diff(id)).toContain("OUT.md");
  });

  it("archive keeps the branch, sets worktreeState=archived", async () => {
    const { engine, id } = await doneSession();
    const { writeFileSync } = await import("node:fs"); const { execFileSync } = await import("node:child_process");
    const s = engine.get(id)!;
    writeFileSync(join(s.worktreePath, "OUT.md"), "out\n");
    engine.archive(id);
    expect(engine.get(id)!.worktreeState).toBe("archived");
    const repoPath = join((engine as any).cfg.srcRoot, "demo");
    expect(execFileSync("git", ["-C", repoPath, "branch", "--list", s.branch]).toString()).toContain(s.branch);
  });

  it("discard deletes the branch, sets worktreeState=discarded", async () => {
    const { engine, id } = await doneSession();
    const s = engine.get(id)!;
    engine.discard(id);
    expect(engine.get(id)!.worktreeState).toBe("discarded");
    const { execFileSync } = await import("node:child_process");
    const repoPath = join((engine as any).cfg.srcRoot, "demo");
    expect(execFileSync("git", ["-C", repoPath, "branch", "--list", s.branch]).toString().trim()).toBe("");
  });

  it("rejects cleanup on a running session and on an already-cleaned one", async () => {
    const { engine, id } = await doneSession();
    engine.discard(id);
    expect(() => engine.archive(id)).toThrow();   // already discarded
    const running = engine.submit("demo", "slow", { FAKE_CLAUDE_SLEEP: "2" });
    await waitForStatus(engine, running, "running");
    expect(() => engine.discard(running)).toThrow(/busy|running/i);
  });
});
```

- [ ] **Step 1b: Run → fail.**

- [ ] **Step 2: engine.ts** — edits:

(a) imports — add the new helpers:
```ts
import { createWorktree, diffWorktree, archiveWorktree, discardWorktree } from "./worktree.js";
import { execFileSync } from "node:child_process";
```
(b) `submit` — record baseSha before creating the worktree:
```ts
    const id = randomUUID();
    const baseSha = execFileSync("git", ["-C", repoPath, "rev-parse", "HEAD"], { stdio: "pipe" }).toString().trim();
    const { worktreePath, branch } = createWorktree(repoPath, this.cfg.worktreesRoot, repo, id);
    this.store.create({ id, repo, prompt, worktreePath, branch, baseSha });
```
(c) add the three methods (after `getLog`):
```ts
  private liveSession(id: string): Session {
    const s = this.store.get(id);
    if (!s) throw new Error(`unknown session: ${id}`);
    if (this.running.has(id) || s.status === "pending" || s.status === "running") throw new Error("session busy");
    if (s.worktreeState !== "live") throw new Error("worktree already cleaned");
    return s;
  }

  diff(id: string): string {
    const s = this.store.get(id);
    if (!s) throw new Error(`unknown session: ${id}`);
    if (s.worktreeState !== "live") throw new Error("worktree already cleaned");
    if (!s.baseSha) return execFileSync("git", ["-C", s.worktreePath, "--no-pager", "diff", "HEAD"], { stdio: "pipe", maxBuffer: 64 * 1024 * 1024 }).toString();
    return diffWorktree(s.worktreePath, s.baseSha);
  }

  archive(id: string): void {
    const s = this.liveSession(id);
    archiveWorktree(join(this.cfg.srcRoot, s.repo), s.worktreePath, s.branch, `agentic session ${id}: ${s.prompt.slice(0, 72)}`);
    this.store.update(id, { worktreeState: "archived" });
  }

  discard(id: string): void {
    const s = this.liveSession(id);
    discardWorktree(join(this.cfg.srcRoot, s.repo), s.worktreePath, s.branch);
    this.store.update(id, { worktreeState: "discarded" });
  }
```

- [ ] **Step 3: Run → pass** (the 6 new tests; existing engine tests still pass — they don't read baseSha/worktreeState).
- [ ] **Step 4: Commit**
```bash
git add server/engine/engine.ts server/engine/engine.test.ts
git commit -m "feat(engine): record baseSha + diff/archive/discard"
```

---

## Task 4: routes — diff / archive / discard endpoints

**Files:** Modify `server/api/routes.ts`, `server/api/server.test.ts`

- [ ] **Step 1: Failing tests** — append inside `describe("HTTP API", ...)` in `server/api/server.test.ts`:

```ts
async function doneId(app: any, auth: any) {
  const id = (await app.inject({ method: "POST", url: "/api/sessions", headers: auth, payload: { repo: "demo", prompt: "p" } })).json().id;
  let st = ""; for (let i = 0; i < 100 && st !== "done"; i++) { await new Promise((r) => setTimeout(r, 20)); st = (await app.inject({ method: "GET", url: `/api/sessions/${id}`, headers: auth })).json().session.status; }
  return id;
}

it("GET /diff returns a diff string", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  const auth = { authorization: `Bearer ${token}` };
  const id = await doneId(app, auth);
  const res = await app.inject({ method: "GET", url: `/api/sessions/${id}/diff`, headers: auth });
  expect(res.statusCode).toBe(200);
  expect(typeof res.json().diff).toBe("string");
  await app.close();
});

it("POST /discard cleans up, then diff/discard 400", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  const auth = { authorization: `Bearer ${token}` };
  const id = await doneId(app, auth);
  expect((await app.inject({ method: "POST", url: `/api/sessions/${id}/discard`, headers: auth })).statusCode).toBe(200);
  expect((await app.inject({ method: "POST", url: `/api/sessions/${id}/discard`, headers: auth })).statusCode).toBe(400);
  await app.close();
});

it("cleanup on unknown id is 404", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  expect((await app.inject({ method: "POST", url: "/api/sessions/zzz/archive", headers: { authorization: `Bearer ${token}` } })).statusCode).toBe(404);
  await app.close();
});
```

- [ ] **Step 1b: Run → fail.**

- [ ] **Step 2: routes.ts** — add after the `POST /messages` route:

```ts
  app.get<{ Params: { id: string } }>("/api/sessions/:id/diff", async (req, reply) => {
    if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
    try { return { diff: engine.diff(req.params.id) }; }
    catch (err: any) { return reply.code(400).send({ error: String(err?.message ?? err) }); }
  });

  for (const action of ["archive", "discard"] as const) {
    app.post<{ Params: { id: string } }>(`/api/sessions/:id/${action}`, async (req, reply) => {
      if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
      try { engine[action](req.params.id); return { ok: true }; }
      catch (err: any) { return reply.code(400).send({ error: String(err?.message ?? err) }); }
    });
  }
```

- [ ] **Step 3: Run → pass.** Then full suite `yarn test; echo EXIT=$?` → green.
- [ ] **Step 4: Commit**
```bash
git add server/api/routes.ts server/api/server.test.ts
git commit -m "feat(api): GET /diff + POST /archive + /discard"
```

---

## Task 5: web — api client + SessionView + Dashboard

**Files:** Modify `web/src/api.ts`, `web/src/pages/SessionView.tsx`, `web/src/pages/Dashboard.tsx`

- [ ] **Step 1: `web/src/api.ts`** — add `worktreeState` to the `Session` interface:
```ts
  createdAt: number;
  worktreeState?: "live" | "archived" | "discarded";
```
and add to the `api` object (after `kill`):
```ts
  diff: (id: string): Promise<{ diff: string }> => req(`/api/sessions/${id}/diff`),
  archive: (id: string) => req(`/api/sessions/${id}/archive`, { method: "POST" }),
  discard: (id: string) => req(`/api/sessions/${id}/discard`, { method: "POST" }),
```

- [ ] **Step 2: `web/src/pages/SessionView.tsx`** — add diff + cleanup UI. Add state near the other `useState`s:
```ts
  const [diff, setDiff] = useState<string | null>(null);
```
Add a helper inside the component:
```ts
  async function cleanup(kind: "archive" | "discard") {
    try { await api[kind](id); const r = await api.session(id); setSession(r.session); }
    catch { /* ignore */ }
  }
```
In the returned JSX, after the transcript `<Paper>` and before/around the follow-up `Stack`, add (only when terminal and worktree still live):
```tsx
        {terminal && session?.worktreeState === "live" && (
          <Stack direction="row" spacing={1} alignItems="center">
            <Button size="small" onClick={async () => setDiff((await api.diff(id)).diff)}>View diff</Button>
            <Button size="small" onClick={() => cleanup("archive")}>Archive (keep branch)</Button>
            <Button size="small" color="error" onClick={() => cleanup("discard")}>Discard</Button>
          </Stack>
        )}
        {session && session.worktreeState && session.worktreeState !== "live" && (
          <Typography variant="caption" color="text.secondary">worktree {session.worktreeState}</Typography>
        )}
        {diff !== null && (
          <Paper variant="outlined" sx={{ p: 2, fontFamily: "monospace", whiteSpace: "pre-wrap", fontSize: 12, maxHeight: 400, overflow: "auto" }}>
            {diff || "(no changes)"}
          </Paper>
        )}
```
(Keep the existing follow-up input; the follow-up box and cleanup row can both show when terminal+live.)

- [ ] **Step 3: `web/src/pages/Dashboard.tsx`** — show worktree state. In the session card row (where the status chip + cost are), add after the cost:
```tsx
                    {s.worktreeState && s.worktreeState !== "live" && (
                      <Chip label={s.worktreeState} size="small" variant="outlined" />
                    )}
```
(Adjust to match the actual JSX; `Chip` is already imported in Dashboard.)

- [ ] **Step 4: Build** — `cd web && yarn build` → clean, no type errors.

- [ ] **Step 5: Commit**
```bash
git add web/src/api.ts web/src/pages/SessionView.tsx web/src/pages/Dashboard.tsx
git commit -m "feat(web): diff view + archive/discard + worktree state chip"
```

---

## Task 6: Verify (separate port) + redeploy gate

- [ ] **Step 1:** Full suite `yarn test; echo EXIT=$?` → green. `cd web && yarn build` → clean.

- [ ] **Step 2: Smoke on a NON-7420 port (fake binary, temp dirs):** boot the server on port 7483 with `AGENTIC_CLAUDE_BIN=…/fake-claude.sh` and temp SRC/WT/DATA + a throwaway "demo" repo. Then:
```bash
TOK=$(curl -s -X POST localhost:7483/api/login -H 'content-type: application/json' -d '{"password":"smk"}' | sed -E 's/.*"token":"([^"]+)".*/\1/')
ID=$(curl -s -X POST localhost:7483/api/sessions -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{"repo":"demo","prompt":"t1"}' | sed -E 's/.*"id":"([^"]+)".*/\1/')
# wait done (poll), then write a file into the worktree to simulate agent output, then:
echo "diff: $(curl -s localhost:7483/api/sessions/$ID/diff -H "authorization: Bearer $TOK" | head -c 80)"
echo "archive: $(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:7483/api/sessions/$ID/archive -H "authorization: Bearer $TOK")"
echo "session: $(curl -s localhost:7483/api/sessions/$ID -H "authorization: Bearer $TOK" | grep -o '\"worktreeState\":\"[^\"]*\"')"
```
Confirm: diff returns a string; archive → 200; worktreeState → archived; the worktree dir is gone but the branch exists. Kill the server + rm temp dirs.

- [ ] **Step 3: (Optional) Playwright** on 7483: a finished session shows View diff / Archive / Discard; clicking Archive flips the state chip and hides the actions.

- [ ] **Step 4: Hand back for redeploy** — report; the controller merges `slice3-worktree-cleanup` → master and, on the user's go-ahead, rebuilds web + `systemctl --user restart agentic-dev`.

---

## Self-Review (plan author)

**1. Spec coverage:** baseSha + worktreeState → Task 1. diff/archive/discard helpers → Task 2. engine records baseSha + diff/archive/discard + guards (terminal, live-only) → Task 3. routes (GET /diff, POST /archive, /discard; 404/400) → Task 4. web api + SessionView diff/cleanup + Dashboard chip → Task 5. No-auto-merge: nothing in the plan merges to a default branch (Archive keeps the branch for manual merge). Idempotent cleanup → Task 2 tests. Null-baseSha fallback → Task 3 `diff`. Success criteria 1–5 → Task 6.

**2. Placeholder scan:** none — complete code for new functions; exact edit locations for modifications; commands with expected outcomes.

**3. Type consistency:** `worktreeState` literal union identical in types.ts, store UPDATABLE, engine guards, api.ts, and the components. `baseSha: string | null` consistent (CreateInput, Session, store, engine submit passes a string). `diff`/`archive`/`discard` names match across engine, routes (`engine[action]`), api.ts, and SessionView. `CreateInput` now requires `baseSha` — existing Slice-1 `create({...})` test calls are updated in Task 1 Step 4 to pass `baseSha: null`.
