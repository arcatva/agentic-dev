# agentic-dev Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a LAN web app that, given a `~/src` repo and a prompt, creates a git worktree, spawns a headless autonomous `claude` session in it, and streams the session's output live to the browser — with multiple concurrent sessions, cost tracking, and kill.

**Architecture:** A Node/TS backend with an HTTP-independent **engine** (worktree manager + headless-`claude` spawner + NDJSON stream parser + SQLite store + bounded concurrency pool), wrapped by a Fastify HTTP API and a WebSocket stream endpoint. A React + MUI (Material Design 3) front end provides a dashboard, a new-request form, and a live session view. One-shot autonomous sessions; the `claude` session id is captured for future multi-turn.

**Tech Stack:** Node 20+, TypeScript (ESM), Fastify v5 + `@fastify/websocket` + `@fastify/cors`, `better-sqlite3`, Vitest, React + Vite + MUI v6 (`@mui/material`, `@emotion/*`) + `react-router-dom`. Tests drive a **fake `claude` binary** (a shell script emitting canned stream-json) so no real API credits are spent.

**Reference spec:** `docs/superpowers/specs/2026-06-15-agentic-dev-slice1-design.md`

---

## File Structure

```
~/src/agentic-dev/
  package.json                     # backend deps + scripts (root workspace)
  tsconfig.json
  vitest.config.ts
  server/
    index.ts                       # entry: load config, build engine + app, listen
    engine/
      types.ts                     # Session, SessionStatus, ClaudeEvent, EngineConfig
      streamParser.ts              # parseLine() : raw NDJSON line -> ClaudeEvent | null
      store.ts                     # SqliteStore: session records + per-session log files
      worktree.ts                  # createWorktree() / removeWorktree()
      spawner.ts                   # spawnClaude() : child process -> ClaudeEvent stream
      repos.ts                     # listRepos() : git repos under srcRoot
      engine.ts                    # Engine: submit/list/get/subscribe/kill + pool
    api/
      config.ts                    # loadConfig() from env, with defaults
      auth.ts                      # issueToken()/verifyToken() + auth hook
      routes.ts                    # HTTP routes (login, repos, sessions)
      stream.ts                    # WS /api/sessions/:id/stream
      server.ts                    # buildApp(engine, config) : Fastify instance
    test/
      fixtures/fake-claude.sh      # emits canned stream-json
      fixtures/gitrepo.ts          # helper: make a temp git repo
  web/
    package.json                   # frontend deps + scripts
    vite.config.ts
    index.html
    src/
      main.tsx                     # React root + router + theme
      theme.ts                     # MD3 MUI theme
      api.ts                       # typed fetch + WS client
      auth.tsx                     # token storage + login guard
      pages/Login.tsx
      pages/Dashboard.tsx
      pages/NewRequest.tsx
      pages/SessionView.tsx
  scripts/
    dev.sh                         # install + build web + start server
  README.md
  CLAUDE.md
```

Files split by responsibility. The engine has zero dependency on Fastify; the API layer depends on the engine. Each engine file has one job and is unit-tested in isolation.

---

## Task 1: Backend bootstrap + toolchain smoke test

**Files:**
- Create: `package.json`, `tsconfig.json`, `vitest.config.ts`, `server/test/smoke.test.ts`

- [ ] **Step 1: Write `package.json`**

```json
{
  "name": "agentic-dev",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {
    "test": "vitest run",
    "test:watch": "vitest",
    "build": "tsc -p tsconfig.json",
    "start": "node --experimental-strip-types server/index.ts"
  },
  "dependencies": {
    "@fastify/cors": "^10.0.1",
    "@fastify/websocket": "^11.0.1",
    "better-sqlite3": "^11.3.0",
    "fastify": "^5.1.0"
  },
  "devDependencies": {
    "@types/better-sqlite3": "^7.6.11",
    "@types/node": "^22.9.0",
    "@types/ws": "^8.5.13",
    "typescript": "^5.6.3",
    "vitest": "^2.1.5"
  }
}
```

- [ ] **Step 2: Write `tsconfig.json`**

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "outDir": "dist",
    "rootDir": ".",
    "types": ["node"]
  },
  "include": ["server/**/*.ts"],
  "exclude": ["node_modules", "dist", "web"]
}
```

- [ ] **Step 3: Write `vitest.config.ts`**

```ts
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["server/**/*.test.ts"],
    environment: "node",
    testTimeout: 15000,
  },
});
```

- [ ] **Step 4: Write the smoke test** — `server/test/smoke.test.ts`

```ts
import { describe, it, expect } from "vitest";

describe("toolchain", () => {
  it("runs typescript tests", () => {
    expect(1 + 1).toBe(2);
  });
});
```

- [ ] **Step 5: Install deps and run**

Run: `cd ~/src/agentic-dev && yarn install && yarn test`
Expected: 1 passing test. (Use `yarn`; `npm install` is blocked by the global guard hook.)

- [ ] **Step 6: Commit**

```bash
git add package.json tsconfig.json vitest.config.ts server/test/smoke.test.ts yarn.lock
git commit -m "chore: backend bootstrap + vitest toolchain"
```

---

## Task 2: Shared types + stream parser

The parser turns a raw NDJSON line from `claude --output-format stream-json` into a typed `ClaudeEvent`. Pure function, no I/O.

**Files:**
- Create: `server/engine/types.ts`, `server/engine/streamParser.ts`, `server/engine/streamParser.test.ts`

- [ ] **Step 1: Write `server/engine/types.ts`**

```ts
export type SessionStatus = "pending" | "running" | "done" | "failed" | "killed";

export interface Session {
  id: string;
  repo: string;
  prompt: string;
  worktreePath: string;
  branch: string;
  claudeSessionId: string | null;
  status: SessionStatus;
  costUsd: number | null;
  exitCode: number | null;
  error: string | null;
  createdAt: number;
  startedAt: number | null;
  endedAt: number | null;
}

export type ClaudeEvent =
  | { kind: "init"; sessionId: string; raw: unknown }
  | { kind: "text"; text: string; raw: unknown }
  | { kind: "retry"; attempt: number; maxRetries: number; category: string; raw: unknown }
  | { kind: "result"; isError: boolean; costUsd: number | null; raw: unknown }
  | { kind: "other"; raw: unknown };

export interface EngineConfig {
  srcRoot: string;        // ~/src
  worktreesRoot: string;  // ~/src/agentic-worktrees
  logDir: string;         // where per-session .jsonl logs live
  dbPath: string;         // sqlite file
  claudeBin: string;      // "claude" in prod; fake path in tests
  maxConcurrent: number;  // default 3
}
```

- [ ] **Step 2: Write the failing test** — `server/engine/streamParser.test.ts`

```ts
import { describe, it, expect } from "vitest";
import { parseLine } from "./streamParser.js";

describe("parseLine", () => {
  it("returns null for blank or non-JSON lines", () => {
    expect(parseLine("")).toBeNull();
    expect(parseLine("   ")).toBeNull();
    expect(parseLine("not json")).toBeNull();
  });

  it("parses an init event and extracts session_id", () => {
    const ev = parseLine(JSON.stringify({ type: "system", subtype: "init", session_id: "abc123" }));
    expect(ev).toEqual({ kind: "init", sessionId: "abc123", raw: expect.anything() });
  });

  it("parses a text delta from a stream_event", () => {
    const line = JSON.stringify({ type: "stream_event", event: { delta: { type: "text_delta", text: "hello" } } });
    const ev = parseLine(line);
    expect(ev).toMatchObject({ kind: "text", text: "hello" });
  });

  it("parses an api_retry event", () => {
    const line = JSON.stringify({ type: "system", subtype: "api_retry", attempt: 2, max_retries: 5, error: "overloaded" });
    expect(parseLine(line)).toMatchObject({ kind: "retry", attempt: 2, maxRetries: 5, category: "overloaded" });
  });

  it("parses the final result with cost", () => {
    const line = JSON.stringify({ type: "result", subtype: "success", is_error: false, total_cost_usd: 0.0123 });
    expect(parseLine(line)).toMatchObject({ kind: "result", isError: false, costUsd: 0.0123 });
  });

  it("maps unknown shapes to other", () => {
    expect(parseLine(JSON.stringify({ type: "assistant" }))).toMatchObject({ kind: "other" });
  });
});
```

- [ ] **Step 2b: Run test to verify it fails**

Run: `yarn vitest run server/engine/streamParser.test.ts`
Expected: FAIL — cannot find `./streamParser.js`.

- [ ] **Step 3: Write `server/engine/streamParser.ts`**

```ts
import type { ClaudeEvent } from "./types.js";

export function parseLine(line: string): ClaudeEvent | null {
  const trimmed = line.trim();
  if (!trimmed) return null;
  let obj: any;
  try {
    obj = JSON.parse(trimmed);
  } catch {
    return null;
  }
  if (obj && obj.type === "system" && obj.subtype === "init" && typeof obj.session_id === "string") {
    return { kind: "init", sessionId: obj.session_id, raw: obj };
  }
  if (obj && obj.type === "system" && obj.subtype === "api_retry") {
    return {
      kind: "retry",
      attempt: Number(obj.attempt ?? 0),
      maxRetries: Number(obj.max_retries ?? 0),
      category: String(obj.error ?? "unknown"),
      raw: obj,
    };
  }
  if (obj && obj.type === "stream_event" && obj.event?.delta?.type === "text_delta") {
    return { kind: "text", text: String(obj.event.delta.text ?? ""), raw: obj };
  }
  if (obj && obj.type === "result") {
    const cost = typeof obj.total_cost_usd === "number" ? obj.total_cost_usd : null;
    return { kind: "result", isError: Boolean(obj.is_error), costUsd: cost, raw: obj };
  }
  return { kind: "other", raw: obj };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `yarn vitest run server/engine/streamParser.test.ts`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add server/engine/types.ts server/engine/streamParser.ts server/engine/streamParser.test.ts
git commit -m "feat(engine): typed claude stream-json parser"
```

---

## Task 3: Session store (SQLite + log files)

**Files:**
- Create: `server/engine/store.ts`, `server/engine/store.test.ts`

- [ ] **Step 1: Write the failing test** — `server/engine/store.test.ts`

```ts
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteStore } from "./store.js";

let dir: string;
let store: SqliteStore;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "agentic-store-"));
  store = new SqliteStore(join(dir, "db.sqlite"), join(dir, "logs"));
});
afterEach(() => {
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("SqliteStore", () => {
  it("creates and reads a session", () => {
    const s = store.create({ id: "s1", repo: "demo", prompt: "do x", worktreePath: "/wt/s1", branch: "agentic/s1" });
    expect(s.status).toBe("pending");
    expect(store.get("s1")?.repo).toBe("demo");
  });

  it("updates fields", () => {
    store.create({ id: "s1", repo: "demo", prompt: "p", worktreePath: "/wt", branch: "b" });
    store.update("s1", { status: "running", claudeSessionId: "cs1", startedAt: 100 });
    const s = store.get("s1")!;
    expect(s.status).toBe("running");
    expect(s.claudeSessionId).toBe("cs1");
    expect(s.startedAt).toBe(100);
  });

  it("lists newest first", () => {
    store.create({ id: "a", repo: "r", prompt: "p", worktreePath: "/w", branch: "b" });
    store.create({ id: "b", repo: "r", prompt: "p", worktreePath: "/w", branch: "b" });
    expect(store.list().map((s) => s.id)).toEqual(["b", "a"]);
  });

  it("appends and reads per-session log lines", () => {
    store.create({ id: "s1", repo: "r", prompt: "p", worktreePath: "/w", branch: "b" });
    store.appendLog("s1", '{"a":1}');
    store.appendLog("s1", '{"b":2}');
    expect(store.readLog("s1")).toEqual(['{"a":1}', '{"b":2}']);
  });
});
```

- [ ] **Step 1b: Run test to verify it fails**

Run: `yarn vitest run server/engine/store.test.ts`
Expected: FAIL — cannot find `./store.js`.

- [ ] **Step 2: Write `server/engine/store.ts`**

```ts
import Database from "better-sqlite3";
import { mkdirSync, appendFileSync, readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import type { Session, SessionStatus } from "./types.js";

export interface CreateInput {
  id: string;
  repo: string;
  prompt: string;
  worktreePath: string;
  branch: string;
}

const COLUMNS = `
  id TEXT PRIMARY KEY, repo TEXT, prompt TEXT, worktreePath TEXT, branch TEXT,
  claudeSessionId TEXT, status TEXT, costUsd REAL, exitCode INTEGER, error TEXT,
  createdAt INTEGER, startedAt INTEGER, endedAt INTEGER`;

export class SqliteStore {
  private db: Database.Database;
  private seq = 0; // tie-breaker so same-ms inserts keep insertion order

  constructor(dbPath: string, private logDir: string) {
    mkdirSync(dirname(dbPath), { recursive: true });
    mkdirSync(logDir, { recursive: true });
    this.db = new Database(dbPath);
    this.db.exec(`CREATE TABLE IF NOT EXISTS sessions (${COLUMNS}, seq INTEGER)`);
  }

  create(input: CreateInput): Session {
    const s: Session = {
      ...input,
      claudeSessionId: null,
      status: "pending",
      costUsd: null,
      exitCode: null,
      error: null,
      createdAt: Date.now(),
      startedAt: null,
      endedAt: null,
    };
    this.db
      .prepare(
        `INSERT INTO sessions
         (id,repo,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,createdAt,startedAt,endedAt,seq)
         VALUES (@id,@repo,@prompt,@worktreePath,@branch,@claudeSessionId,@status,@costUsd,@exitCode,@error,@createdAt,@startedAt,@endedAt,@seq)`
      )
      .run({ ...s, seq: this.seq++ });
    return s;
  }

  get(id: string): Session | null {
    const row = this.db.prepare(`SELECT * FROM sessions WHERE id = ?`).get(id) as any;
    return row ? this.rowToSession(row) : null;
  }

  list(): Session[] {
    const rows = this.db.prepare(`SELECT * FROM sessions ORDER BY createdAt DESC, seq DESC`).all() as any[];
    return rows.map((r) => this.rowToSession(r));
  }

  update(id: string, patch: Partial<Omit<Session, "id">>): void {
    const keys = Object.keys(patch);
    if (keys.length === 0) return;
    const set = keys.map((k) => `${k} = @${k}`).join(", ");
    this.db.prepare(`UPDATE sessions SET ${set} WHERE id = @id`).run({ ...patch, id });
  }

  appendLog(id: string, line: string): void {
    appendFileSync(this.logPath(id), line + "\n");
  }

  readLog(id: string): string[] {
    const p = this.logPath(id);
    if (!existsSync(p)) return [];
    return readFileSync(p, "utf8").split("\n").filter((l) => l.length > 0);
  }

  close(): void {
    this.db.close();
  }

  private logPath(id: string): string {
    return join(this.logDir, `${id}.jsonl`);
  }

  private rowToSession(r: any): Session {
    return {
      id: r.id, repo: r.repo, prompt: r.prompt, worktreePath: r.worktreePath, branch: r.branch,
      claudeSessionId: r.claudeSessionId, status: r.status as SessionStatus,
      costUsd: r.costUsd, exitCode: r.exitCode, error: r.error,
      createdAt: r.createdAt, startedAt: r.startedAt, endedAt: r.endedAt,
    };
  }
}
```

- [ ] **Step 3: Run test to verify it passes**

Run: `yarn vitest run server/engine/store.test.ts`
Expected: PASS (4 tests).

- [ ] **Step 4: Commit**

```bash
git add server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(engine): sqlite session store + per-session log files"
```

---

## Task 4: Worktree manager

**Files:**
- Create: `server/engine/worktree.ts`, `server/test/fixtures/gitrepo.ts`, `server/engine/worktree.test.ts`

- [ ] **Step 1: Write the git-repo fixture** — `server/test/fixtures/gitrepo.ts`

```ts
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Create a throwaway git repo with one commit; returns its path. */
export function makeTempGitRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "agentic-repo-"));
  const git = (args: string[]) => execFileSync("git", args, { cwd: dir });
  git(["init", "-q"]);
  git(["config", "user.email", "test@test.local"]);
  git(["config", "user.name", "test"]);
  writeFileSync(join(dir, "README.md"), "# temp\n");
  git(["add", "."]);
  git(["commit", "-q", "-m", "init"]);
  return dir;
}
```

- [ ] **Step 2: Write the failing test** — `server/engine/worktree.test.ts`

```ts
import { describe, it, expect, afterEach } from "vitest";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { makeTempGitRepo } from "../test/fixtures/gitrepo.js";
import { createWorktree, removeWorktree } from "./worktree.js";

const cleanup: string[] = [];
afterEach(() => {
  for (const d of cleanup.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("worktree", () => {
  it("creates a worktree on a new branch off HEAD", () => {
    const repo = makeTempGitRepo();
    cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-"));
    cleanup.push(root);

    const { worktreePath, branch } = createWorktree(repo, root, "demo", "id1");
    expect(existsSync(join(worktreePath, "README.md"))).toBe(true);
    expect(branch).toBe("agentic/id1");
    const branches = execFileSync("git", ["-C", repo, "branch", "--list", branch]).toString();
    expect(branches).toContain("agentic/id1");
  });

  it("removes a worktree", () => {
    const repo = makeTempGitRepo();
    cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-"));
    cleanup.push(root);
    const { worktreePath } = createWorktree(repo, root, "demo", "id2");
    removeWorktree(repo, worktreePath);
    expect(existsSync(worktreePath)).toBe(false);
  });

  it("throws a clear error if the worktree path already exists", () => {
    const repo = makeTempGitRepo();
    cleanup.push(repo);
    const root = mkdtempSync(join(tmpdir(), "agentic-wt-"));
    cleanup.push(root);
    createWorktree(repo, root, "demo", "dup");
    expect(() => createWorktree(repo, root, "demo", "dup")).toThrow();
  });
});
```

- [ ] **Step 2b: Run test to verify it fails**

Run: `yarn vitest run server/engine/worktree.test.ts`
Expected: FAIL — cannot find `./worktree.js`.

- [ ] **Step 3: Write `server/engine/worktree.ts`**

```ts
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

export interface WorktreeInfo {
  worktreePath: string;
  branch: string;
}

/** Create an isolated git worktree for `repoPath` at `<root>/<repo>/<id>` on branch `agentic/<id>`. */
export function createWorktree(repoPath: string, root: string, repo: string, id: string): WorktreeInfo {
  const worktreePath = join(root, repo, id);
  const branch = `agentic/${id}`;
  mkdirSync(join(root, repo), { recursive: true });
  // -b creates the branch off current HEAD; fails loudly if path or branch already exists.
  execFileSync("git", ["-C", repoPath, "worktree", "add", worktreePath, "-b", branch], { stdio: "pipe" });
  return { worktreePath, branch };
}

export function removeWorktree(repoPath: string, worktreePath: string): void {
  execFileSync("git", ["-C", repoPath, "worktree", "remove", "--force", worktreePath], { stdio: "pipe" });
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `yarn vitest run server/engine/worktree.test.ts`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add server/engine/worktree.ts server/test/fixtures/gitrepo.ts server/engine/worktree.test.ts
git commit -m "feat(engine): git worktree manager"
```

---

## Task 5: Fake claude binary + spawner

**Files:**
- Create: `server/test/fixtures/fake-claude.sh`, `server/engine/spawner.ts`, `server/engine/spawner.test.ts`

- [ ] **Step 1: Write the fake binary** — `server/test/fixtures/fake-claude.sh`

```bash
#!/usr/bin/env bash
# Test double for `claude`. Ignores all args; emits canned stream-json.
# Set FAKE_CLAUDE_SLEEP to delay before the result line (to test concurrency/kill).
set -euo pipefail
echo '{"type":"system","subtype":"init","session_id":"fake-sess-123","model":"claude-opus-4-8"}'
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Hello "}}}'
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"world"}}}'
sleep "${FAKE_CLAUDE_SLEEP:-0}"
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.0042}'
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x server/test/fixtures/fake-claude.sh`

- [ ] **Step 3: Write the failing test** — `server/engine/spawner.test.ts`

```ts
import { describe, it, expect } from "vitest";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { spawnClaude } from "./spawner.js";
import type { ClaudeEvent } from "./types.js";

const FAKE = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude.sh");

function collect(handle: ReturnType<typeof spawnClaude>): Promise<{ events: ClaudeEvent[]; code: number | null }> {
  return new Promise((resolve) => {
    const events: ClaudeEvent[] = [];
    handle.on("event", (e: ClaudeEvent) => events.push(e));
    handle.on("exit", (code: number | null) => resolve({ events, code }));
  });
}

describe("spawnClaude", () => {
  it("streams parsed events and exits 0", async () => {
    const handle = spawnClaude({ bin: FAKE, cwd: tmpdir(), prompt: "ignored" });
    const { events, code } = await collect(handle);
    expect(code).toBe(0);
    expect(events.find((e) => e.kind === "init")).toMatchObject({ sessionId: "fake-sess-123" });
    const text = events.filter((e) => e.kind === "text").map((e: any) => e.text).join("");
    expect(text).toBe("Hello world");
    expect(events.find((e) => e.kind === "result")).toMatchObject({ costUsd: 0.0042 });
  });

  it("kill() terminates a slow process", async () => {
    const handle = spawnClaude({ bin: FAKE, cwd: tmpdir(), prompt: "x", env: { FAKE_CLAUDE_SLEEP: "5" } });
    setTimeout(() => handle.kill(), 100);
    const { code } = await collect(handle);
    expect(code).not.toBe(0); // killed → non-zero / null
  });
});
```

- [ ] **Step 3b: Run test to verify it fails**

Run: `yarn vitest run server/engine/spawner.test.ts`
Expected: FAIL — cannot find `./spawner.js`.

- [ ] **Step 4: Write `server/engine/spawner.ts`**

```ts
import { spawn, type ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { parseLine } from "./streamParser.js";

export interface SpawnOptions {
  bin: string;       // "claude" in prod, fake path in tests
  cwd: string;       // the worktree
  prompt: string;
  env?: Record<string, string>;
}

/** EventEmitter: 'event' (ClaudeEvent), 'exit' (code:number|null). Has kill(). */
export class SpawnHandle extends EventEmitter {
  constructor(private child: ChildProcess) {
    super();
  }
  kill(): void {
    this.child.kill("SIGTERM");
  }
}

const BASE_ARGS = [
  "--output-format", "stream-json",
  "--verbose",
  "--include-partial-messages",
  "--dangerously-skip-permissions",
];

export function spawnClaude(opts: SpawnOptions): SpawnHandle {
  // claude -p <prompt> <BASE_ARGS...>  (non-bare: loads repo CLAUDE.md + global skills)
  const args = ["-p", opts.prompt, ...BASE_ARGS];
  const child = spawn(opts.bin, args, {
    cwd: opts.cwd,
    env: { ...process.env, ...(opts.env ?? {}) },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const handle = new SpawnHandle(child);

  let buffer = "";
  child.stdout!.setEncoding("utf8");
  child.stdout!.on("data", (chunk: string) => {
    buffer += chunk;
    let nl: number;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 1);
      const ev = parseLine(line);
      if (ev) handle.emit("event", ev);
    }
  });

  let stderr = "";
  child.stderr!.setEncoding("utf8");
  child.stderr!.on("data", (c: string) => (stderr += c));

  child.on("error", (err) => {
    handle.emit("event", { kind: "other", raw: { spawnError: String(err) } });
    handle.emit("exit", null);
  });
  child.on("close", (code) => {
    if (buffer.trim()) {
      const ev = parseLine(buffer);
      if (ev) handle.emit("event", ev);
    }
    if (code !== 0 && stderr) handle.emit("event", { kind: "other", raw: { stderr } });
    handle.emit("exit", code);
  });

  return handle;
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `yarn vitest run server/engine/spawner.test.ts`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add server/test/fixtures/fake-claude.sh server/engine/spawner.ts server/engine/spawner.test.ts
git commit -m "feat(engine): claude spawner + fake-claude test double"
```

---

## Task 6: Engine orchestrator (pool + wiring)

Ties worktree + spawner + store together, enforces the concurrency cap, persists events, and re-broadcasts them to subscribers.

**Files:**
- Create: `server/engine/engine.ts`, `server/engine/engine.test.ts`

- [ ] **Step 1: Write the failing test** — `server/engine/engine.test.ts`

```ts
import { describe, it, expect, afterEach } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { makeTempGitRepo } from "../test/fixtures/gitrepo.js";
import { Engine } from "./engine.js";
import type { EngineConfig, ClaudeEvent } from "./types.js";

const FAKE = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude.sh");
const cleanup: string[] = [];
const engines: Engine[] = [];
afterEach(() => {
  for (const e of engines.splice(0)) e.close();
  for (const d of cleanup.splice(0)) rmSync(d, { recursive: true, force: true });
});

function makeEngine(srcRoot: string, overrides: Partial<EngineConfig> = {}): Engine {
  const work = mkdtempSync(join(tmpdir(), "agentic-eng-"));
  cleanup.push(work);
  const cfg: EngineConfig = {
    srcRoot,
    worktreesRoot: join(work, "wt"),
    logDir: join(work, "logs"),
    dbPath: join(work, "db.sqlite"),
    claudeBin: FAKE,
    maxConcurrent: 1,
    ...overrides,
  };
  const e = new Engine(cfg);
  engines.push(e);
  return e;
}

function waitForStatus(engine: Engine, id: string, status: string): Promise<void> {
  return new Promise((resolve) => {
    const check = () => {
      if (engine.get(id)?.status === status) resolve();
      else setTimeout(check, 20);
    };
    check();
  });
}

describe("Engine", () => {
  it("runs a session to done, captures session id, cost, and logs", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    const repo = makeTempGitRepo();
    // place the repo under src as "demo"
    const { renameSync } = await import("node:fs");
    renameSync(repo, join(src, "demo"));

    const engine = makeEngine(src);
    const id = engine.submit("demo", "do something");
    await waitForStatus(engine, id, "done");

    const s = engine.get(id)!;
    expect(s.claudeSessionId).toBe("fake-sess-123");
    expect(s.costUsd).toBe(0.0042);
    expect(s.exitCode).toBe(0);
    expect(engine.getLog(id).length).toBeGreaterThan(0);
  });

  it("subscribers receive live events", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    const repo = makeTempGitRepo();
    const { renameSync } = await import("node:fs");
    renameSync(repo, join(src, "demo"));

    const engine = makeEngine(src);
    const got: ClaudeEvent[] = [];
    const id = engine.submit("demo", "p");
    engine.subscribe(id, (e) => got.push(e));
    await waitForStatus(engine, id, "done");
    expect(got.some((e) => e.kind === "text")).toBe(true);
  });

  it("respects maxConcurrent: second session stays pending until the first finishes", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    const repo = makeTempGitRepo();
    const { renameSync } = await import("node:fs");
    renameSync(repo, join(src, "demo"));

    const engine = makeEngine(src, { maxConcurrent: 1, claudeBin: FAKE });
    // make the first one slow so the second must queue
    const id1 = engine.submit("demo", "first", { FAKE_CLAUDE_SLEEP: "1" });
    const id2 = engine.submit("demo", "second");
    // immediately, id2 must be pending
    expect(engine.get(id2)!.status).toBe("pending");
    await waitForStatus(engine, id2, "done");
    expect(engine.get(id1)!.status).toBe("done");
  });

  it("rejects submit for an unknown repo", () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    const engine = makeEngine(src);
    expect(() => engine.submit("nope", "p")).toThrow();
  });
});
```

- [ ] **Step 1b: Run test to verify it fails**

Run: `yarn vitest run server/engine/engine.test.ts`
Expected: FAIL — cannot find `./engine.js`.

- [ ] **Step 2: Write `server/engine/engine.ts`**

```ts
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { SqliteStore } from "./store.js";
import { createWorktree } from "./worktree.js";
import { spawnClaude, SpawnHandle } from "./spawner.js";
import type { ClaudeEvent, EngineConfig, Session } from "./types.js";

type Subscriber = (e: ClaudeEvent) => void;

interface QueueItem {
  id: string;
  repoPath: string;
  prompt: string;
  env?: Record<string, string>;
}

export class Engine {
  private store: SqliteStore;
  private subs = new Map<string, Set<Subscriber>>();
  private running = new Map<string, SpawnHandle>();
  private queue: QueueItem[] = [];

  constructor(private cfg: EngineConfig) {
    this.store = new SqliteStore(cfg.dbPath, cfg.logDir);
  }

  /** Validate repo, create worktree + record, enqueue. Returns session id. Throws on bad repo. */
  submit(repo: string, prompt: string, env?: Record<string, string>): string {
    const repoPath = join(this.cfg.srcRoot, repo);
    if (!existsSync(join(repoPath, ".git"))) {
      throw new Error(`not a git repo: ${repo}`);
    }
    const id = randomUUID();
    const { worktreePath, branch } = createWorktree(repoPath, this.cfg.worktreesRoot, repo, id);
    this.store.create({ id, repo, prompt, worktreePath, branch });
    this.queue.push({ id, repoPath, prompt, env });
    this.pump();
    return id;
  }

  list(): Session[] {
    return this.store.list();
  }
  get(id: string): Session | null {
    return this.store.get(id);
  }
  getLog(id: string): string[] {
    return this.store.readLog(id);
  }

  /** Subscribe to live events for a session. Returns an unsubscribe fn. */
  subscribe(id: string, fn: Subscriber): () => void {
    let set = this.subs.get(id);
    if (!set) {
      set = new Set();
      this.subs.set(id, set);
    }
    set.add(fn);
    return () => set!.delete(fn);
  }

  kill(id: string): void {
    const handle = this.running.get(id);
    if (handle) handle.kill();
    else {
      // queued but not started: drop it and mark killed
      this.queue = this.queue.filter((q) => q.id !== id);
      if (this.store.get(id)?.status === "pending") {
        this.store.update(id, { status: "killed", endedAt: Date.now() });
      }
    }
  }

  close(): void {
    for (const h of this.running.values()) h.kill();
    this.store.close();
  }

  private pump(): void {
    while (this.running.size < this.cfg.maxConcurrent && this.queue.length > 0) {
      const item = this.queue.shift()!;
      this.start(item);
    }
  }

  private start(item: QueueItem): void {
    const s = this.store.get(item.id);
    const worktreePath = s!.worktreePath;
    this.store.update(item.id, { status: "running", startedAt: Date.now() });
    const handle = spawnClaude({ bin: this.cfg.claudeBin, cwd: worktreePath, prompt: item.prompt, env: item.env });
    this.running.set(item.id, handle);

    handle.on("event", (ev: ClaudeEvent) => {
      this.store.appendLog(item.id, JSON.stringify(ev.raw));
      if (ev.kind === "init") this.store.update(item.id, { claudeSessionId: ev.sessionId });
      if (ev.kind === "result") this.store.update(item.id, { costUsd: ev.costUsd });
      this.emit(item.id, ev);
    });

    handle.on("exit", (code: number | null) => {
      this.running.delete(item.id);
      const cur = this.store.get(item.id);
      const status = cur?.status === "killed" ? "killed" : code === 0 ? "done" : "failed";
      this.store.update(item.id, { status, exitCode: code, endedAt: Date.now() });
      this.emit(item.id, { kind: "other", raw: { engineExit: { code, status } } });
      this.pump();
    });
  }

  private emit(id: string, ev: ClaudeEvent): void {
    const set = this.subs.get(id);
    if (set) for (const fn of set) fn(ev);
  }
}
```

Note: when `kill()` is called on a running session, the spawner's SIGTERM triggers the `exit` handler; we mark `killed` only if the store status was already set to `killed` — so add a status set in `kill()` for the running case too. Fix that in the next step's test-driven adjustment.

- [ ] **Step 3: Run tests to verify they pass**

Run: `yarn vitest run server/engine/engine.test.ts`
Expected: PASS (4 tests).

- [ ] **Step 4: Add a kill test and fix running-kill status** — append to `server/engine/engine.test.ts`

```ts
it("kill() on a running session marks it killed", async () => {
  const { mkdtempSync, renameSync } = await import("node:fs");
  const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
  cleanup.push(src);
  const repo = makeTempGitRepo();
  renameSync(repo, join(src, "demo"));
  const engine = makeEngine(src, { claudeBin: FAKE });
  const id = engine.submit("demo", "slow", { FAKE_CLAUDE_SLEEP: "5" });
  await waitForStatus(engine, id, "running");
  engine.kill(id);
  await waitForStatus(engine, id, "killed");
  expect(engine.get(id)!.status).toBe("killed");
});
```

- [ ] **Step 5: Update `kill()` so a running kill records status before exit fires**

In `server/engine/engine.ts`, change the `kill` method:

```ts
  kill(id: string): void {
    const handle = this.running.get(id);
    if (handle) {
      this.store.update(id, { status: "killed" });
      handle.kill();
    } else {
      this.queue = this.queue.filter((q) => q.id !== id);
      if (this.store.get(id)?.status === "pending") {
        this.store.update(id, { status: "killed", endedAt: Date.now() });
      }
    }
  }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `yarn vitest run server/engine/engine.test.ts`
Expected: PASS (5 tests).

- [ ] **Step 7: Commit**

```bash
git add server/engine/engine.ts server/engine/engine.test.ts
git commit -m "feat(engine): orchestrator with concurrency pool, subscribe, kill"
```

---

## Task 7: Repo enumeration

**Files:**
- Create: `server/engine/repos.ts`, `server/engine/repos.test.ts`

- [ ] **Step 1: Write the failing test** — `server/engine/repos.test.ts`

```ts
import { describe, it, expect, afterEach } from "vitest";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { listRepos } from "./repos.js";

const cleanup: string[] = [];
afterEach(() => {
  for (const d of cleanup.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("listRepos", () => {
  it("returns only direct children that are git repos, sorted", () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-repos-"));
    cleanup.push(src);
    mkdirSync(join(src, "beta", ".git"), { recursive: true });
    mkdirSync(join(src, "alpha", ".git"), { recursive: true });
    mkdirSync(join(src, "not-a-repo"), { recursive: true });
    expect(listRepos(src)).toEqual(["alpha", "beta"]);
  });

  it("returns empty array if srcRoot is missing", () => {
    expect(listRepos(join(tmpdir(), "does-not-exist-xyz"))).toEqual([]);
  });
});
```

- [ ] **Step 1b: Run test to verify it fails**

Run: `yarn vitest run server/engine/repos.test.ts`
Expected: FAIL — cannot find `./repos.js`.

- [ ] **Step 2: Write `server/engine/repos.ts`**

```ts
import { readdirSync, existsSync } from "node:fs";
import { join } from "node:path";

/** Direct child directories of srcRoot that contain a .git, sorted alphabetically. */
export function listRepos(srcRoot: string): string[] {
  if (!existsSync(srcRoot)) return [];
  return readdirSync(srcRoot, { withFileTypes: true })
    .filter((d) => d.isDirectory() && existsSync(join(srcRoot, d.name, ".git")))
    .map((d) => d.name)
    .sort();
}
```

- [ ] **Step 3: Run test to verify it passes**

Run: `yarn vitest run server/engine/repos.test.ts`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add server/engine/repos.ts server/engine/repos.test.ts
git commit -m "feat(engine): list git repos under src root"
```

---

## Task 8: Config + auth

**Files:**
- Create: `server/api/config.ts`, `server/api/auth.ts`, `server/api/auth.test.ts`

- [ ] **Step 1: Write `server/api/config.ts`**

```ts
import { homedir } from "node:os";
import { join } from "node:path";
import type { EngineConfig } from "../engine/types.js";

export interface AppConfig extends EngineConfig {
  port: number;
  host: string;        // bind address
  password: string;    // login password
  authSecret: string;  // HMAC secret for tokens
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): AppConfig {
  const src = env.AGENTIC_SRC_ROOT ?? join(homedir(), "src");
  const dataDir = env.AGENTIC_DATA_DIR ?? join(homedir(), ".agentic-dev");
  return {
    srcRoot: src,
    worktreesRoot: env.AGENTIC_WORKTREES_ROOT ?? join(src, "agentic-worktrees"),
    logDir: join(dataDir, "logs"),
    dbPath: join(dataDir, "db.sqlite"),
    claudeBin: env.AGENTIC_CLAUDE_BIN ?? "claude",
    maxConcurrent: Number(env.AGENTIC_MAX_CONCURRENT ?? "3"),
    port: Number(env.AGENTIC_PORT ?? "7420"),
    host: env.AGENTIC_HOST ?? "0.0.0.0",
    password: env.AGENTIC_PASSWORD ?? "changeme",
    authSecret: env.AGENTIC_AUTH_SECRET ?? "dev-insecure-secret",
  };
}
```

- [ ] **Step 2: Write the failing test** — `server/api/auth.test.ts`

```ts
import { describe, it, expect } from "vitest";
import { issueToken, verifyToken } from "./auth.js";

describe("auth tokens", () => {
  const secret = "s3cr3t";
  it("issues a token that verifies", () => {
    const t = issueToken(secret, 3600);
    expect(verifyToken(secret, t)).toBe(true);
  });
  it("rejects a token signed with a different secret", () => {
    const t = issueToken("other", 3600);
    expect(verifyToken(secret, t)).toBe(false);
  });
  it("rejects an expired token", () => {
    const t = issueToken(secret, -1);
    expect(verifyToken(secret, t)).toBe(false);
  });
  it("rejects garbage", () => {
    expect(verifyToken(secret, "not.a.token")).toBe(false);
  });
});
```

- [ ] **Step 2b: Run test to verify it fails**

Run: `yarn vitest run server/api/auth.test.ts`
Expected: FAIL — cannot find `./auth.js`.

- [ ] **Step 3: Write `server/api/auth.ts`**

```ts
import { createHmac, timingSafeEqual } from "node:crypto";

function b64url(buf: Buffer): string {
  return buf.toString("base64url");
}
function sign(secret: string, payload: string): string {
  return b64url(createHmac("sha256", secret).update(payload).digest());
}

/** Token = "<expEpochSec>.<hmac>" */
export function issueToken(secret: string, ttlSeconds: number): string {
  const exp = Math.floor(Date.now() / 1000) + ttlSeconds;
  const payload = String(exp);
  return `${payload}.${sign(secret, payload)}`;
}

export function verifyToken(secret: string, token: string): boolean {
  const dot = token.indexOf(".");
  if (dot < 0) return false;
  const payload = token.slice(0, dot);
  const sig = token.slice(dot + 1);
  const expected = sign(secret, payload);
  const a = Buffer.from(sig);
  const b = Buffer.from(expected);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return false;
  const exp = Number(payload);
  return Number.isFinite(exp) && exp > Math.floor(Date.now() / 1000);
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `yarn vitest run server/api/auth.test.ts`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add server/api/config.ts server/api/auth.ts server/api/auth.test.ts
git commit -m "feat(api): config loader + HMAC bearer tokens"
```

---

## Task 9: HTTP API

**Files:**
- Create: `server/api/server.ts`, `server/api/routes.ts`, `server/api/server.test.ts`

- [ ] **Step 1: Write the failing integration test** — `server/api/server.test.ts`

```ts
import { describe, it, expect, afterEach } from "vitest";
import { mkdtempSync, rmSync, renameSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { makeTempGitRepo } from "../test/fixtures/gitrepo.js";
import { Engine } from "../engine/engine.js";
import { buildApp } from "./server.js";
import type { AppConfig } from "./config.js";

const FAKE = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude.sh");
const cleanup: string[] = [];
afterEach(() => {
  for (const d of cleanup.splice(0)) rmSync(d, { recursive: true, force: true });
});

function setup() {
  const src = mkdtempSync(join(tmpdir(), "agentic-api-src-"));
  cleanup.push(src);
  const repo = makeTempGitRepo();
  renameSync(repo, join(src, "demo"));
  const work = mkdtempSync(join(tmpdir(), "agentic-api-"));
  cleanup.push(work);
  const cfg: AppConfig = {
    srcRoot: src,
    worktreesRoot: join(work, "wt"),
    logDir: join(work, "logs"),
    dbPath: join(work, "db.sqlite"),
    claudeBin: FAKE,
    maxConcurrent: 2,
    port: 0,
    host: "127.0.0.1",
    password: "pw",
    authSecret: "sec",
  };
  const engine = new Engine(cfg);
  const app = buildApp(engine, cfg);
  return { app, engine, cfg };
}

describe("HTTP API", () => {
  it("rejects unauthenticated requests", async () => {
    const { app } = setup();
    const res = await app.inject({ method: "GET", url: "/api/repos" });
    expect(res.statusCode).toBe(401);
    await app.close();
  });

  it("logs in and lists repos", async () => {
    const { app } = setup();
    const login = await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } });
    expect(login.statusCode).toBe(200);
    const token = login.json().token as string;
    const repos = await app.inject({ method: "GET", url: "/api/repos", headers: { authorization: `Bearer ${token}` } });
    expect(repos.json()).toEqual(["demo"]);
    await app.close();
  });

  it("rejects a bad password", async () => {
    const { app } = setup();
    const login = await app.inject({ method: "POST", url: "/api/login", payload: { password: "wrong" } });
    expect(login.statusCode).toBe(401);
    await app.close();
  });

  it("creates a session and reports it via GET", async () => {
    const { app } = setup();
    const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
    const auth = { authorization: `Bearer ${token}` };
    const created = await app.inject({ method: "POST", url: "/api/sessions", headers: auth, payload: { repo: "demo", prompt: "go" } });
    expect(created.statusCode).toBe(200);
    const id = created.json().id as string;
    const got = await app.inject({ method: "GET", url: `/api/sessions/${id}`, headers: auth });
    expect(got.json().session.repo).toBe("demo");
    await app.close();
  });

  it("returns 400 for an unknown repo", async () => {
    const { app } = setup();
    const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
    const res = await app.inject({ method: "POST", url: "/api/sessions", headers: { authorization: `Bearer ${token}` }, payload: { repo: "nope", prompt: "x" } });
    expect(res.statusCode).toBe(400);
    await app.close();
  });
});
```

- [ ] **Step 1b: Run test to verify it fails**

Run: `yarn vitest run server/api/server.test.ts`
Expected: FAIL — cannot find `./server.js`.

- [ ] **Step 2: Write `server/api/routes.ts`**

```ts
import type { FastifyInstance } from "fastify";
import type { Engine } from "../engine/engine.js";
import type { AppConfig } from "./config.js";
import { issueToken, verifyToken } from "./auth.js";
import { listRepos } from "../engine/repos.js";

export function registerRoutes(app: FastifyInstance, engine: Engine, cfg: AppConfig): void {
  app.post<{ Body: { password?: string } }>("/api/login", async (req, reply) => {
    if (req.body?.password !== cfg.password) return reply.code(401).send({ error: "bad password" });
    return { token: issueToken(cfg.authSecret, 12 * 3600) };
  });

  // Auth gate for everything else under /api (except /api/login handled above).
  app.addHook("onRequest", async (req, reply) => {
    if (req.url === "/api/login" || !req.url.startsWith("/api/")) return;
    const header = req.headers.authorization ?? "";
    const token = header.startsWith("Bearer ") ? header.slice(7) : "";
    if (!verifyToken(cfg.authSecret, token)) return reply.code(401).send({ error: "unauthorized" });
  });

  app.get("/api/repos", async () => listRepos(cfg.srcRoot));

  app.get("/api/sessions", async () => ({ sessions: engine.list() }));

  app.get<{ Params: { id: string } }>("/api/sessions/:id", async (req, reply) => {
    const s = engine.get(req.params.id);
    if (!s) return reply.code(404).send({ error: "not found" });
    return { session: s, log: engine.getLog(req.params.id) };
  });

  app.post<{ Body: { repo?: string; prompt?: string } }>("/api/sessions", async (req, reply) => {
    const { repo, prompt } = req.body ?? {};
    if (!repo || !prompt) return reply.code(400).send({ error: "repo and prompt required" });
    try {
      const id = engine.submit(repo, prompt);
      return { id };
    } catch (err: any) {
      return reply.code(400).send({ error: String(err?.message ?? err) });
    }
  });

  app.delete<{ Params: { id: string } }>("/api/sessions/:id", async (req, reply) => {
    if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
    engine.kill(req.params.id);
    return { ok: true };
  });
}
```

- [ ] **Step 3: Write `server/api/server.ts`**

```ts
import Fastify, { type FastifyInstance } from "fastify";
import cors from "@fastify/cors";
import websocket from "@fastify/websocket";
import type { Engine } from "../engine/engine.js";
import type { AppConfig } from "./config.js";
import { registerRoutes } from "./routes.js";
import { registerStream } from "./stream.js";

export function buildApp(engine: Engine, cfg: AppConfig): FastifyInstance {
  const app = Fastify({ logger: false });
  app.register(cors, { origin: true });
  app.register(websocket);
  app.register(async (instance) => {
    registerRoutes(instance, engine, cfg);
    registerStream(instance, engine, cfg);
  });
  return app;
}
```

- [ ] **Step 4: Create a stub `server/api/stream.ts` so the import resolves**

```ts
import type { FastifyInstance } from "fastify";
import type { Engine } from "../engine/engine.js";
import type { AppConfig } from "./config.js";

// Real implementation in Task 10.
export function registerStream(_app: FastifyInstance, _engine: Engine, _cfg: AppConfig): void {}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `yarn vitest run server/api/server.test.ts`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add server/api/server.ts server/api/routes.ts server/api/stream.ts server/api/server.test.ts
git commit -m "feat(api): fastify http routes (login, repos, sessions)"
```

---

## Task 10: WebSocket streaming

`/api/sessions/:id/stream` backfills the persisted log on connect, then forwards live events. Tokens are passed as a `?token=` query param (browsers can't set WS auth headers).

**Files:**
- Modify: `server/api/stream.ts` (replace the stub)
- Create: `server/api/stream.test.ts`

- [ ] **Step 1: Write the failing test** — `server/api/stream.test.ts`

```ts
import { describe, it, expect, afterEach } from "vitest";
import { mkdtempSync, rmSync, renameSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { makeTempGitRepo } from "../test/fixtures/gitrepo.js";
import { Engine } from "../engine/engine.js";
import { buildApp } from "./server.js";
import { issueToken } from "./auth.js";
import type { AppConfig } from "./config.js";

const FAKE = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude.sh");
const cleanup: string[] = [];
afterEach(() => {
  for (const d of cleanup.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("WS stream", () => {
  it("streams events for a session and closes when it ends", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-ws-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const work = mkdtempSync(join(tmpdir(), "agentic-ws-"));
    cleanup.push(work);
    const cfg: AppConfig = {
      srcRoot: src, worktreesRoot: join(work, "wt"), logDir: join(work, "logs"),
      dbPath: join(work, "db.sqlite"), claudeBin: FAKE, maxConcurrent: 2,
      port: 0, host: "127.0.0.1", password: "pw", authSecret: "sec",
    };
    const engine = new Engine(cfg);
    const app = buildApp(engine, cfg);
    await app.listen({ port: 0, host: "127.0.0.1" });
    const addr = app.server.address();
    const port = typeof addr === "object" && addr ? addr.port : 0;

    const id = engine.submit("demo", "go", { FAKE_CLAUDE_SLEEP: "0.3" });
    const token = issueToken("sec", 3600);
    const ws = new WebSocket(`ws://127.0.0.1:${port}/api/sessions/${id}/stream?token=${token}`);

    const messages: any[] = [];
    await new Promise<void>((resolve, reject) => {
      ws.on("message", (data) => messages.push(JSON.parse(data.toString())));
      ws.on("close", () => resolve());
      ws.on("error", reject);
      setTimeout(() => reject(new Error("timeout")), 10000);
    });

    expect(messages.some((m) => m.kind === "text")).toBe(true);
    await app.close();
    engine.close();
  });

  it("rejects a WS connection with a bad token", async () => {
    const src = mkdtempSync(join(tmpdir(), "agentic-ws-src2-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const work = mkdtempSync(join(tmpdir(), "agentic-ws2-"));
    cleanup.push(work);
    const cfg: AppConfig = {
      srcRoot: src, worktreesRoot: join(work, "wt"), logDir: join(work, "logs"),
      dbPath: join(work, "db.sqlite"), claudeBin: FAKE, maxConcurrent: 2,
      port: 0, host: "127.0.0.1", password: "pw", authSecret: "sec",
    };
    const engine = new Engine(cfg);
    const app = buildApp(engine, cfg);
    await app.listen({ port: 0, host: "127.0.0.1" });
    const addr = app.server.address();
    const port = typeof addr === "object" && addr ? addr.port : 0;
    const id = engine.submit("demo", "go");
    const ws = new WebSocket(`ws://127.0.0.1:${port}/api/sessions/${id}/stream?token=bad`);
    const closed = await new Promise<boolean>((resolve) => {
      ws.on("close", () => resolve(true));
      ws.on("open", () => resolve(false));
      setTimeout(() => resolve(false), 3000);
    });
    expect(closed).toBe(true);
    await app.close();
    engine.close();
  });
});
```

- [ ] **Step 1b: Add `ws` as a dependency (used by the test client and types)**

Run: `cd ~/src/agentic-dev && yarn add ws && yarn add -D @types/ws`
(`@fastify/websocket` bundles `ws` for the server; this makes the client import explicit.)

- [ ] **Step 1c: Run test to verify it fails**

Run: `yarn vitest run server/api/stream.test.ts`
Expected: FAIL — stream returns nothing / connection never receives messages.

- [ ] **Step 2: Replace `server/api/stream.ts`**

```ts
import type { FastifyInstance } from "fastify";
import type { Engine } from "../engine/engine.js";
import type { AppConfig } from "./config.js";
import { verifyToken } from "./auth.js";

export function registerStream(app: FastifyInstance, engine: Engine, cfg: AppConfig): void {
  app.get<{ Params: { id: string }; Querystring: { token?: string } }>(
    "/api/sessions/:id/stream",
    { websocket: true },
    (socket, req) => {
      const token = req.query.token ?? "";
      if (!verifyToken(cfg.authSecret, token)) {
        socket.close(1008, "unauthorized");
        return;
      }
      const id = req.params.id;
      const session = engine.get(id);
      if (!session) {
        socket.close(1008, "not found");
        return;
      }

      // 1. Backfill persisted log.
      for (const line of engine.getLog(id)) {
        socket.send(JSON.stringify({ kind: "backfill", raw: safeParse(line) }));
      }

      // 2. If already finished, close after backfill.
      if (session.status === "done" || session.status === "failed" || session.status === "killed") {
        socket.send(JSON.stringify({ kind: "other", raw: { engineExit: { status: session.status } } }));
        socket.close(1000, "ended");
        return;
      }

      // 3. Live subscription.
      const unsub = engine.subscribe(id, (ev) => {
        socket.send(JSON.stringify(ev));
        if (ev.kind === "other" && (ev.raw as any)?.engineExit) {
          socket.close(1000, "ended");
        }
      });
      socket.on("close", () => unsub());
    }
  );
}

function safeParse(line: string): unknown {
  try {
    return JSON.parse(line);
  } catch {
    return line;
  }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `yarn vitest run server/api/stream.test.ts`
Expected: PASS (2 tests).

- [ ] **Step 4: Run the whole backend suite**

Run: `cd ~/src/agentic-dev && yarn test`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add server/api/stream.ts server/api/stream.test.ts package.json yarn.lock
git commit -m "feat(api): websocket session stream with backfill + auth"
```

---

## Task 11: Entry point + dev script

**Files:**
- Create: `server/index.ts`, `scripts/dev.sh`

- [ ] **Step 1: Write `server/index.ts`**

```ts
import { loadConfig } from "./api/config.js";
import { Engine } from "./engine/engine.js";
import { buildApp } from "./api/server.js";

const cfg = loadConfig();
const engine = new Engine(cfg);
const app = buildApp(engine, cfg);

app.listen({ port: cfg.port, host: cfg.host }).then((addr) => {
  console.log(`agentic-dev listening on ${addr} (src=${cfg.srcRoot}, maxConcurrent=${cfg.maxConcurrent})`);
  if (cfg.password === "changeme") console.warn("WARNING: default password 'changeme' — set AGENTIC_PASSWORD");
});

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    engine.close();
    app.close().then(() => process.exit(0));
  });
}
```

- [ ] **Step 2: Write `scripts/dev.sh`**

```bash
#!/usr/bin/env bash
# One-command local start: install deps, build the web UI, run the server.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

command -v claude >/dev/null || { echo "ERROR: 'claude' CLI not on PATH"; exit 1; }

echo "== backend deps =="; yarn install
echo "== web deps + build =="; (cd web && yarn install && yarn build)

: "${AGENTIC_PASSWORD:?set AGENTIC_PASSWORD before starting (no default in dev.sh)}"
echo "== starting server on :${AGENTIC_PORT:-7420} =="
exec node --experimental-strip-types server/index.ts
```

- [ ] **Step 3: Make it executable + verify the server boots**

Run:
```bash
chmod +x scripts/dev.sh
cd ~/src/agentic-dev
AGENTIC_PASSWORD=test AGENTIC_PORT=7420 AGENTIC_CLAUDE_BIN=server/test/fixtures/fake-claude.sh \
  node --experimental-strip-types server/index.ts &
sleep 2
curl -s -X POST localhost:7420/api/login -H 'content-type: application/json' -d '{"password":"test"}'
kill %1
```
Expected: a JSON `{"token":"..."}` response, then the server is killed.

- [ ] **Step 4: Commit**

```bash
git add server/index.ts scripts/dev.sh
git commit -m "feat: server entry point + dev.sh one-command start"
```

---

## Task 12: Frontend bootstrap (Vite + React + MUI/MD3) + API client

**Files:**
- Create: `web/package.json`, `web/vite.config.ts`, `web/index.html`, `web/src/theme.ts`, `web/src/api.ts`, `web/src/auth.tsx`, `web/src/main.tsx`

- [ ] **Step 1: Write `web/package.json`**

```json
{
  "name": "agentic-dev-web",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "vite",
    "build": "tsc -b && vite build",
    "preview": "vite preview"
  },
  "dependencies": {
    "@emotion/react": "^11.13.3",
    "@emotion/styled": "^11.13.0",
    "@mui/material": "^6.1.6",
    "react": "^18.3.1",
    "react-dom": "^18.3.1",
    "react-router-dom": "^6.28.0"
  },
  "devDependencies": {
    "@types/react": "^18.3.12",
    "@types/react-dom": "^18.3.1",
    "@vitejs/plugin-react": "^4.3.3",
    "typescript": "^5.6.3",
    "vite": "^5.4.10"
  }
}
```

- [ ] **Step 2: Write `web/vite.config.ts`** (proxy `/api` to the backend so dev + prod use the same origin)

```ts
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": { target: "http://localhost:7420", ws: true, changeOrigin: true },
    },
  },
  build: { outDir: "dist" },
});
```

- [ ] **Step 3: Write `web/index.html`**

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>agentic-dev</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
```

- [ ] **Step 4: Write `web/src/theme.ts`** (Material Design 3 palette via MUI)

```ts
import { createTheme } from "@mui/material/styles";

// MD3-ish: rounded shapes, tonal primary. MUI v6 approximates MD3.
export const theme = createTheme({
  shape: { borderRadius: 16 },
  palette: {
    mode: "light",
    primary: { main: "#6750A4" },     // MD3 baseline primary
    secondary: { main: "#625B71" },
    background: { default: "#FEF7FF", paper: "#FFFFFF" },
  },
  typography: { fontFamily: "Roboto, system-ui, sans-serif" },
});
```

- [ ] **Step 5: Write `web/src/api.ts`** (typed client + WS helper)

```ts
export interface Session {
  id: string;
  repo: string;
  prompt: string;
  status: "pending" | "running" | "done" | "failed" | "killed";
  costUsd: number | null;
  claudeSessionId: string | null;
  createdAt: number;
}

const TOKEN_KEY = "agentic-token";
export const getToken = () => localStorage.getItem(TOKEN_KEY) ?? "";
export const setToken = (t: string) => localStorage.setItem(TOKEN_KEY, t);
export const clearToken = () => localStorage.removeItem(TOKEN_KEY);

async function req(path: string, init: RequestInit = {}): Promise<any> {
  const res = await fetch(path, {
    ...init,
    headers: { "content-type": "application/json", authorization: `Bearer ${getToken()}`, ...(init.headers ?? {}) },
  });
  if (res.status === 401) {
    clearToken();
    throw new Error("unauthorized");
  }
  if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error ?? res.statusText);
  return res.json();
}

export const api = {
  login: (password: string) =>
    fetch("/api/login", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ password }) })
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error("bad password")))),
  repos: (): Promise<string[]> => req("/api/repos"),
  sessions: (): Promise<{ sessions: Session[] }> => req("/api/sessions"),
  session: (id: string): Promise<{ session: Session; log: string[] }> => req(`/api/sessions/${id}`),
  submit: (repo: string, prompt: string): Promise<{ id: string }> =>
    req("/api/sessions", { method: "POST", body: JSON.stringify({ repo, prompt }) }),
  kill: (id: string) => req(`/api/sessions/${id}`, { method: "DELETE" }),
};

/** Open the live event WebSocket for a session. */
export function openStream(id: string, onEvent: (ev: any) => void, onClose: () => void): WebSocket {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/api/sessions/${id}/stream?token=${encodeURIComponent(getToken())}`);
  ws.onmessage = (m) => onEvent(JSON.parse(m.data));
  ws.onclose = onClose;
  return ws;
}
```

- [ ] **Step 6: Write `web/src/auth.tsx`** (login guard)

```tsx
import { type ReactNode } from "react";
import { Navigate } from "react-router-dom";
import { getToken } from "./api";

export function RequireAuth({ children }: { children: ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}
```

- [ ] **Step 7: Write `web/src/main.tsx`** (router + theme; pages added in Task 13)

```tsx
import React from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Routes, Route } from "react-router-dom";
import { ThemeProvider, CssBaseline } from "@mui/material";
import { theme } from "./theme";
import { RequireAuth } from "./auth";
import { Login } from "./pages/Login";
import { Dashboard } from "./pages/Dashboard";
import { NewRequest } from "./pages/NewRequest";
import { SessionView } from "./pages/SessionView";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ThemeProvider theme={theme}>
      <CssBaseline />
      <BrowserRouter>
        <Routes>
          <Route path="/login" element={<Login />} />
          <Route path="/" element={<RequireAuth><Dashboard /></RequireAuth>} />
          <Route path="/new" element={<RequireAuth><NewRequest /></RequireAuth>} />
          <Route path="/sessions/:id" element={<RequireAuth><SessionView /></RequireAuth>} />
        </Routes>
      </BrowserRouter>
    </ThemeProvider>
  </React.StrictMode>
);
```

- [ ] **Step 8: Add a `web/tsconfig.json`**

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "lib": ["ES2022", "DOM", "DOM.Iterable"],
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "jsx": "react-jsx",
    "strict": true,
    "skipLibCheck": true,
    "noEmit": true
  },
  "include": ["src"]
}
```

- [ ] **Step 9: Install (do not build yet — pages come in Task 13)**

Run: `cd ~/src/agentic-dev/web && yarn install`
Expected: deps install cleanly.

- [ ] **Step 10: Commit**

```bash
cd ~/src/agentic-dev
git add web/package.json web/vite.config.ts web/index.html web/tsconfig.json web/src/theme.ts web/src/api.ts web/src/auth.tsx web/src/main.tsx web/yarn.lock
git commit -m "feat(web): vite+react+mui(md3) bootstrap + api client"
```

---

## Task 13: Frontend pages

**Files:**
- Create: `web/src/pages/Login.tsx`, `web/src/pages/Dashboard.tsx`, `web/src/pages/NewRequest.tsx`, `web/src/pages/SessionView.tsx`

- [ ] **Step 1: Write `web/src/pages/Login.tsx`**

```tsx
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Box, Button, Card, CardContent, TextField, Typography } from "@mui/material";
import { api, setToken } from "../api";

export function Login() {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const nav = useNavigate();

  const submit = async () => {
    try {
      const { token } = await api.login(password);
      setToken(token);
      nav("/");
    } catch {
      setError("Wrong password");
    }
  };

  return (
    <Box sx={{ display: "grid", placeItems: "center", minHeight: "100vh" }}>
      <Card sx={{ width: 360 }}>
        <CardContent sx={{ display: "grid", gap: 2 }}>
          <Typography variant="h5">agentic-dev</Typography>
          <TextField type="password" label="Password" value={password}
            onChange={(e) => setPassword(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()} error={!!error} helperText={error} />
          <Button variant="contained" onClick={submit}>Log in</Button>
        </CardContent>
      </Card>
    </Box>
  );
}
```

- [ ] **Step 2: Write `web/src/pages/Dashboard.tsx`**

```tsx
import { useEffect, useState } from "react";
import { Link as RouterLink } from "react-router-dom";
import {
  AppBar, Toolbar, Typography, Button, Box, Card, CardActionArea, CardContent,
  Chip, Stack, List, ListItem, ListItemText,
} from "@mui/material";
import { api, type Session } from "../api";

const statusColor: Record<Session["status"], "default" | "info" | "success" | "error" | "warning"> = {
  pending: "default", running: "info", done: "success", failed: "error", killed: "warning",
};

export function Dashboard() {
  const [sessions, setSessions] = useState<Session[]>([]);

  useEffect(() => {
    const load = () => api.sessions().then((r) => setSessions(r.sessions)).catch(() => {});
    load();
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, []);

  const total = sessions.reduce((sum, s) => sum + (s.costUsd ?? 0), 0);

  return (
    <Box>
      <AppBar position="static" color="primary">
        <Toolbar sx={{ gap: 2 }}>
          <Typography variant="h6" sx={{ flexGrow: 1 }}>agentic-dev</Typography>
          <Typography variant="body2">total ${total.toFixed(4)}</Typography>
          <Button color="inherit" component={RouterLink} to="/new">New request</Button>
        </Toolbar>
      </AppBar>
      <Box sx={{ p: 3, display: "grid", gap: 2 }}>
        <Typography variant="h6">Sessions</Typography>
        <List>
          {sessions.map((s) => (
            <Card key={s.id} sx={{ mb: 1 }}>
              <CardActionArea component={RouterLink} to={`/sessions/${s.id}`}>
                <CardContent>
                  <Stack direction="row" spacing={2} alignItems="center">
                    <Chip label={s.status} color={statusColor[s.status]} size="small" />
                    <Typography sx={{ flexGrow: 1 }}>{s.repo}: {s.prompt.slice(0, 80)}</Typography>
                    <Typography variant="body2">${(s.costUsd ?? 0).toFixed(4)}</Typography>
                  </Stack>
                </CardContent>
              </CardActionArea>
            </Card>
          ))}
          {sessions.length === 0 && <ListItem><ListItemText primary="No sessions yet." /></ListItem>}
        </List>
      </Box>
    </Box>
  );
}
```

- [ ] **Step 3: Write `web/src/pages/NewRequest.tsx`**

```tsx
import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  AppBar, Toolbar, Typography, Box, MenuItem, TextField, Button, Stack,
} from "@mui/material";
import { api } from "../api";

export function NewRequest() {
  const [repos, setRepos] = useState<string[]>([]);
  const [repo, setRepo] = useState("");
  const [prompt, setPrompt] = useState("");
  const [error, setError] = useState("");
  const nav = useNavigate();

  useEffect(() => {
    api.repos().then((r) => { setRepos(r); if (r[0]) setRepo(r[0]); }).catch((e) => setError(String(e.message)));
  }, []);

  const submit = async () => {
    try {
      const { id } = await api.submit(repo, prompt);
      nav(`/sessions/${id}`);
    } catch (e: any) {
      setError(String(e.message));
    }
  };

  return (
    <Box>
      <AppBar position="static" color="primary"><Toolbar><Typography variant="h6">New request</Typography></Toolbar></AppBar>
      <Box sx={{ p: 3, maxWidth: 720 }}>
        <Stack spacing={2}>
          <TextField select label="Repo" value={repo} onChange={(e) => setRepo(e.target.value)}>
            {repos.map((r) => <MenuItem key={r} value={r}>{r}</MenuItem>)}
          </TextField>
          <TextField label="Request" multiline minRows={6} value={prompt} onChange={(e) => setPrompt(e.target.value)} />
          {error && <Typography color="error">{error}</Typography>}
          <Button variant="contained" disabled={!repo || !prompt} onClick={submit}>Launch session</Button>
        </Stack>
      </Box>
    </Box>
  );
}
```

- [ ] **Step 4: Write `web/src/pages/SessionView.tsx`**

```tsx
import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router-dom";
import {
  AppBar, Toolbar, Typography, Box, Button, Chip, Stack, Paper,
} from "@mui/material";
import { api, openStream, type Session } from "../api";

export function SessionView() {
  const { id = "" } = useParams();
  const [session, setSession] = useState<Session | null>(null);
  const [text, setText] = useState("");
  const [events, setEvents] = useState<string[]>([]);
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    api.session(id).then((r) => setSession(r.session)).catch(() => {});
    const ws = openStream(
      id,
      (ev) => {
        if (ev.kind === "text") setText((t) => t + ev.text);
        else if (ev.kind === "backfill") {
          const raw = ev.raw;
          if (raw?.type === "stream_event" && raw.event?.delta?.type === "text_delta") {
            setText((t) => t + raw.event.delta.text);
          }
        } else if (ev.kind === "retry") setEvents((e) => [...e, `↻ retry ${ev.attempt}/${ev.maxRetries} (${ev.category})`]);
        else if (ev.kind === "result") setEvents((e) => [...e, `✓ result, cost $${(ev.costUsd ?? 0).toFixed(4)}`]);
      },
      () => api.session(id).then((r) => setSession(r.session)).catch(() => {})
    );
    return () => ws.close();
  }, [id]);

  useEffect(() => { endRef.current?.scrollIntoView(); }, [text, events]);

  return (
    <Box>
      <AppBar position="static" color="primary">
        <Toolbar sx={{ gap: 2 }}>
          <Typography variant="h6" sx={{ flexGrow: 1 }}>{session?.repo} session</Typography>
          {session && <Chip label={session.status} color="default" size="small" />}
          {session && <Typography variant="body2">${(session.costUsd ?? 0).toFixed(4)}</Typography>}
          <Button color="inherit" onClick={() => api.kill(id)}>Kill</Button>
        </Toolbar>
      </AppBar>
      <Box sx={{ p: 3, display: "grid", gap: 2 }}>
        {session && <Typography variant="body2" color="text.secondary">{session.prompt}</Typography>}
        <Paper variant="outlined" sx={{ p: 2, fontFamily: "monospace", whiteSpace: "pre-wrap", minHeight: 200 }}>
          {text}
          <Stack sx={{ mt: 2 }} spacing={0.5}>
            {events.map((e, i) => <Typography key={i} variant="caption" color="text.secondary">{e}</Typography>)}
          </Stack>
          <div ref={endRef} />
        </Paper>
      </Box>
    </Box>
  );
}
```

- [ ] **Step 5: Build the web app**

Run: `cd ~/src/agentic-dev/web && yarn build`
Expected: type-checks and builds to `web/dist` with no errors.

- [ ] **Step 6: Commit**

```bash
cd ~/src/agentic-dev
git add web/src/pages
git commit -m "feat(web): login, dashboard, new-request, live session view"
```

---

## Task 14: Docs + end-to-end manual smoke

**Files:**
- Create: `README.md`, `CLAUDE.md`

- [ ] **Step 1: Write `README.md`**

```markdown
# agentic-dev

Agent-driven local dev platform. Submit a request against a `~/src` repo from a LAN web UI;
it creates a git worktree and runs a headless autonomous `claude` session in it, streaming
output to the browser. Slice 1: one-shot autonomous sessions, concurrency, cost, kill.

## Run
    AGENTIC_PASSWORD=<pick-one> bash scripts/dev.sh
Then open http://<this-host>:7420 on the LAN.

## Config (env)
| Var | Default | Meaning |
|---|---|---|
| AGENTIC_PASSWORD | (required by dev.sh) | login password |
| AGENTIC_PORT | 7420 | listen port |
| AGENTIC_HOST | 0.0.0.0 | bind address (set to tailscale IP to restrict) |
| AGENTIC_SRC_ROOT | ~/src | repos root |
| AGENTIC_MAX_CONCURRENT | 3 | concurrent sessions |
| AGENTIC_CLAUDE_BIN | claude | claude binary |

Requires the `claude` CLI on PATH and authenticated. Sessions draw from the monthly Agent
SDK credit — watch the cost total.

## Test
    yarn test     # backend, uses a fake claude binary (no API cost)
```

- [ ] **Step 2: Write `CLAUDE.md`**

```markdown
# agentic-dev — Claude guidance

Local agent-driven dev platform that spawns headless `claude -p` sessions in git worktrees
and streams them to a LAN web UI.

## Layout
- `server/engine/` — HTTP-independent core: streamParser, store (sqlite + log files),
  worktree, spawner, repos, engine (pool/subscribe/kill). Unit-tested with a fake claude.
- `server/api/` — Fastify routes + WS stream + HMAC token auth + config.
- `web/` — React + Vite + MUI (MD3): Login, Dashboard, NewRequest, SessionView.

## Rules
- Tests must stay green before commit: `yarn test`. Never hit the real `claude` in tests —
  use `server/test/fixtures/fake-claude.sh`.
- Engine stays free of Fastify imports (keep it unit-testable in isolation).
- Driver invariant: spawn `claude -p <prompt> --output-format stream-json --verbose
  --include-partial-messages --dangerously-skip-permissions`, **non-bare**, cwd = worktree.
- Use `yarn`, never `npm install`.

## Deferred (later slices)
Multi-turn (--resume; session id already captured), skill 下沉, worktree teardown/merge UX,
packaging, crash-survival, Android app.
```

- [ ] **Step 3: Manual end-to-end smoke with the REAL claude**

Run:
```bash
cd ~/src/agentic-dev
AGENTIC_PASSWORD=test bash scripts/dev.sh
```
Then in a browser on the LAN: open `http://<host>:7420`, log in with `test`, pick a small
throwaway repo under `~/src`, submit a tiny prompt (e.g. "create a file HELLO.md containing
'hi'"). Confirm:
- output streams live in the session view,
- status goes `running` → `done`, a cost appears,
- the worktree exists at `~/src/agentic-worktrees/<repo>/<id>` with the change,
- launching a second request against the same repo runs concurrently,
- Kill on a running session flips it to `killed`.

- [ ] **Step 4: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs: README + CLAUDE.md for agentic-dev slice 1"
```

---

## Self-Review (completed by plan author)

**1. Spec coverage** — every spec section maps to a task:
- §6 Engine (worktree/spawn/parse/pool/kill/cost/session-id) → Tasks 2–6.
- §7 API surface (login/repos/sessions CRUD + WS) → Tasks 8–10.
- §8 Web UI (dashboard/new/session view, MD3) → Tasks 12–13.
- §5 Auth (password→token, LAN bind) → Tasks 8, 11.
- §3 Persistence (SQLite + log files, replay) → Task 3 + WS backfill in Task 10.
- §9 Error handling (spawn fail, api_retry, worktree fail, WS reconnect/backfill, orphaned-running) → Tasks 5/6/9/10 (orphaned-running on restart is surfaced via status; full reattach is deferred per spec §2).
- §10 Testing (fake claude binary; engine unit + API integration) → Tasks 5–10.
- §11 Repo layout → matches the File Structure section.
- §12 Cost guardrail (cap + per-session + total) → Task 8 (`maxConcurrent`), Task 6 (cost capture), Task 13 (Dashboard total).
- §13 Success criteria → Task 14 manual smoke covers all 7.

**2. Placeholder scan** — no TBD/TODO; every code step contains complete code; every command has expected output. The `stream.ts` stub in Task 9 is explicitly replaced in Task 10 (intentional, not a placeholder).

**3. Type consistency** — `Session`/`SessionStatus`/`ClaudeEvent`/`EngineConfig` defined once in `types.ts` (Task 2) and reused. `AppConfig extends EngineConfig` (Task 8). Engine methods (`submit/list/get/getLog/subscribe/kill/close`) match their uses in routes (Task 9) and stream (Task 10). Frontend `Session` type mirrors the backend fields the API returns. `spawnClaude` options (`bin/cwd/prompt/env`) match all call sites.

**Note on `Date.now()`:** used in the server runtime (store/engine), which is fine — the workflow-script restriction on `Date.now()` does not apply to the application code being built.
