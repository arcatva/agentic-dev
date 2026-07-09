# agentic-dev Slice 2 (Multi-turn) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a finished session receive follow-up prompts that resume the same Claude conversation in the same git worktree (turn-based), streaming each new turn live, with cost accumulating across turns.

**Architecture:** Additive changes only. `spawnClaude` learns an optional `--resume <id>`. `Engine.followUp(id, prompt)` re-enqueues a turn against an existing session (reusing its worktree + captured `claudeSessionId`), accumulating cost. A new `POST /api/sessions/:id/messages` route triggers it. The WS gains `?live=1` (skip backfill) so a follow-up turn streams without re-sending history. `SessionView` shows a follow-up input on finished sessions and appends the new turn.

**Tech Stack:** Same as Slice 1 (Node/TS, Fastify, Vitest, React/MUI). Tests drive the fake `claude` binary — zero API credit.

**Reference spec:** `docs/superpowers/specs/2026-06-16-agentic-dev-slice2-multiturn-design.md`

---

## File Structure (changes only)

```
server/engine/spawner.ts           # MODIFY: SpawnOptions.resumeSessionId → --resume <id>
server/engine/engine.ts            # MODIFY: QueueItem.resumeSessionId; followUp(); cost accumulate
server/api/stream.ts               # MODIFY: ?live=1 skips backfill
server/api/routes.ts               # MODIFY: POST /api/sessions/:id/messages
web/src/api.ts                     # MODIFY: followUp(); openStream live option
web/src/pages/SessionView.tsx      # MODIFY: follow-up input + live append
server/test/fixtures/fake-claude-echoargs.sh   # CREATE: records argv for the --resume test
server/test/fixtures/fake-claude-noinit.sh     # CREATE: emits no init (no claudeSessionId)
```

All existing Slice-1 tests must stay green. Tests use `yarn`; never `npm install`.

---

## Task 1: spawner `--resume`

**Files:**
- Create: `server/test/fixtures/fake-claude-echoargs.sh`
- Modify: `server/engine/spawner.ts`, `server/engine/spawner.test.ts`

- [ ] **Step 1: Create the argv-recording fake** — `server/test/fixtures/fake-claude-echoargs.sh`

```bash
#!/usr/bin/env bash
# Test double: records its argv to $FAKE_ARGS_OUT (one arg per line), then emits canned stream-json.
set -euo pipefail
[ -n "${FAKE_ARGS_OUT:-}" ] && printf '%s\n' "$@" > "$FAKE_ARGS_OUT"
echo '{"type":"system","subtype":"init","session_id":"fake-sess-123","model":"m"}'
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"ok"}}}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
```

- [ ] **Step 2: `chmod +x` it**

Run: `chmod +x server/test/fixtures/fake-claude-echoargs.sh`

- [ ] **Step 3: Write the failing tests** — append to `server/engine/spawner.test.ts`

```ts
import { readFileSync } from "node:fs";
import { join } from "node:path";

const ECHO = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude-echoargs.sh");

describe("spawnClaude --resume", () => {
  it("includes --resume <id> in argv when resumeSessionId is set", async () => {
    const out = join(tmpdir(), `argv-resume-${process.pid}-${Math.floor(performance.now())}.txt`);
    const handle = spawnClaude({ bin: ECHO, cwd: tmpdir(), prompt: "follow up", resumeSessionId: "sess-xyz", env: { FAKE_ARGS_OUT: out } });
    await new Promise((r) => handle.on("exit", r));
    const argv = readFileSync(out, "utf8");
    expect(argv).toContain("--resume");
    expect(argv).toContain("sess-xyz");
  });

  it("omits --resume when no resumeSessionId", async () => {
    const out = join(tmpdir(), `argv-plain-${process.pid}-${Math.floor(performance.now())}.txt`);
    const handle = spawnClaude({ bin: ECHO, cwd: tmpdir(), prompt: "x", env: { FAKE_ARGS_OUT: out } });
    await new Promise((r) => handle.on("exit", r));
    expect(readFileSync(out, "utf8")).not.toContain("--resume");
  });
});
```

(The existing test file already imports `tmpdir`, `fileURLToPath`, `dirname`, `join` and `spawnClaude`. If `tmpdir`/`performance` are not yet imported, add `import { tmpdir } from "node:os";` — `performance` is a Node global.)

- [ ] **Step 4: Run to verify failure**

Run: `yarn vitest run server/engine/spawner.test.ts`
Expected: FAIL — `resumeSessionId` not accepted / `--resume` not present.

- [ ] **Step 5: Implement in `server/engine/spawner.ts`**

Add `resumeSessionId` to the interface:

```ts
export interface SpawnOptions {
  bin: string;       // "claude" in prod, fake path in tests
  cwd: string;       // the worktree
  prompt: string;
  env?: Record<string, string>;
  resumeSessionId?: string;  // when set, continue an existing claude conversation
}
```

Change the args construction inside `spawnClaude`:

```ts
  // claude -p <prompt> [--resume <id>] <BASE_ARGS...>  (non-bare: loads repo CLAUDE.md + global skills)
  const args = opts.resumeSessionId
    ? ["-p", opts.prompt, "--resume", opts.resumeSessionId, ...BASE_ARGS]
    : ["-p", opts.prompt, ...BASE_ARGS];
```

- [ ] **Step 6: Run to verify pass**

Run: `yarn vitest run server/engine/spawner.test.ts`
Expected: PASS (existing 2 + new 2 = 4).

- [ ] **Step 7: Commit**

```bash
git add server/engine/spawner.ts server/engine/spawner.test.ts server/test/fixtures/fake-claude-echoargs.sh
git commit -m "feat(engine): spawnClaude --resume for multi-turn"
```

---

## Task 2: `Engine.followUp` + cost accumulation

**Files:**
- Create: `server/test/fixtures/fake-claude-noinit.sh`
- Modify: `server/engine/engine.ts`, `server/engine/engine.test.ts`

- [ ] **Step 1: Create a no-init fake** — `server/test/fixtures/fake-claude-noinit.sh`

```bash
#!/usr/bin/env bash
# Test double that emits NO init event → the session never captures a claudeSessionId.
set -euo pipefail
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"noinit"}}}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}'
```

Then: `chmod +x server/test/fixtures/fake-claude-noinit.sh`

- [ ] **Step 2: Write the failing tests** — append to `server/engine/engine.test.ts`

```ts
const NOINIT = join(dirname(fileURLToPath(import.meta.url)), "../test/fixtures/fake-claude-noinit.sh");

describe("Engine.followUp", () => {
  it("re-runs a finished session in the SAME worktree and accumulates cost", async () => {
    const { mkdtempSync, renameSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src);

    const id = engine.submit("demo", "first");
    await waitForStatus(engine, id, "done");
    const wt1 = engine.get(id)!.worktreePath;
    const cost1 = engine.get(id)!.costUsd!;
    const logLen1 = engine.getLog(id).length;

    engine.followUp(id, "second");
    await waitForStatus(engine, id, "done");

    const s = engine.get(id)!;
    expect(s.worktreePath).toBe(wt1);                 // same worktree, no new one
    expect(s.costUsd!).toBeCloseTo(cost1 * 2, 6);     // two turns of fake cost
    expect(engine.getLog(id).length).toBeGreaterThan(logLen1); // transcript grew
  });

  it("rejects a follow-up while the session is running (busy)", async () => {
    const { mkdtempSync, renameSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src);
    const id = engine.submit("demo", "slow", { FAKE_CLAUDE_SLEEP: "2" });
    await waitForStatus(engine, id, "running");
    expect(() => engine.followUp(id, "x")).toThrow(/busy/);
  });

  it("rejects a follow-up when the session has no claudeSessionId", async () => {
    const { mkdtempSync, renameSync } = await import("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    renameSync(makeTempGitRepo(), join(src, "demo"));
    const engine = makeEngine(src, { claudeBin: NOINIT });
    const id = engine.submit("demo", "first");
    await waitForStatus(engine, id, "done");
    expect(engine.get(id)!.claudeSessionId).toBeNull();
    expect(() => engine.followUp(id, "x")).toThrow(/resum/i);
  });

  it("rejects a follow-up for an unknown session", () => {
    const { mkdtempSync } = require("node:fs");
    const src = mkdtempSync(join(tmpdir(), "agentic-src-"));
    cleanup.push(src);
    const engine = makeEngine(src);
    expect(() => engine.followUp("nope", "x")).toThrow();
  });
});
```

(`makeEngine`, `waitForStatus`, `makeTempGitRepo`, `cleanup`, `tmpdir`, `join`, `dirname`, `fileURLToPath` already exist in this test file from Slice 1. Use `await import("node:fs")` as the file already does.)

- [ ] **Step 3: Run to verify failure**

Run: `yarn vitest run server/engine/engine.test.ts`
Expected: FAIL — `followUp` is not a function.

- [ ] **Step 4: Implement in `server/engine/engine.ts`**

Add `resumeSessionId` to `QueueItem`:

```ts
interface QueueItem {
  id: string;
  repoPath: string;
  prompt: string;
  env?: Record<string, string>;
  resumeSessionId?: string;
}
```

Add the `followUp` method (place it right after `submit`):

```ts
  /** Queue a follow-up turn on an existing, finished session (resumes the same claude conversation
   *  in the same worktree). Throws if the session is missing, busy, or has no resumable session id. */
  followUp(id: string, prompt: string): void {
    const s = this.store.get(id);
    if (!s) throw new Error(`unknown session: ${id}`);
    if (this.running.has(id) || s.status === "pending" || s.status === "running") {
      throw new Error("session busy");
    }
    if (!s.claudeSessionId) throw new Error("session has no resumable claude session");
    const repoPath = join(this.cfg.srcRoot, s.repo);
    this.queue.push({ id, repoPath, prompt, resumeSessionId: s.claudeSessionId });
    this.pump();
  }
```

In `start`, pass the resume id to the spawner:

```ts
    const handle = spawnClaude({ bin: this.cfg.claudeBin, cwd: worktreePath, prompt: item.prompt, env: item.env, resumeSessionId: item.resumeSessionId });
```

In `start`'s `event` handler, change the `result` cost line from overwrite to accumulate:

```ts
      if (ev.kind === "result") {
        const cur = this.store.get(item.id);
        this.store.update(item.id, { costUsd: (cur?.costUsd ?? 0) + (ev.costUsd ?? 0) });
      }
```

- [ ] **Step 5: Run to verify pass**

Run: `yarn vitest run server/engine/engine.test.ts`
Expected: PASS (existing 5 + new 4 = 9). The existing Slice-1 cost test (`costUsd === 0.0042`) still passes (0 + 0.0042).

- [ ] **Step 6: Commit**

```bash
git add server/engine/engine.ts server/engine/engine.test.ts server/test/fixtures/fake-claude-noinit.sh
git commit -m "feat(engine): followUp() resumes a finished session, accumulates cost"
```

---

## Task 3: WS `?live=1` (skip backfill)

**Files:**
- Modify: `server/api/stream.ts`, `server/api/stream.test.ts`

- [ ] **Step 1: Write the failing test** — append to `server/api/stream.test.ts`

```ts
it("with ?live=1 sends no backfill — only live events", async () => {
  const src = mkdtempSync(join(tmpdir(), "agentic-live-src-"));
  cleanup.push(src);
  renameSync(makeTempGitRepo(), join(src, "demo"));
  const work = mkdtempSync(join(tmpdir(), "agentic-live-"));
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
  // let it finish so a log exists to (not) backfill
  await new Promise((r) => setTimeout(r, 400));
  const token = issueToken("sec", 3600);
  const ws = new WebSocket(`ws://127.0.0.1:${port}/api/sessions/${id}/stream?token=${token}&live=1`);

  const msgs: any[] = [];
  ws.on("message", (d) => msgs.push(JSON.parse(d.toString())));
  ws.on("error", () => {});
  await new Promise((r) => setTimeout(r, 400)); // window for any (unexpected) backfill
  ws.close();

  expect(msgs.length).toBe(0); // no backfill delivered under live=1
  await app.close();
  engine.close();
});
```

(`mkdtempSync`, `renameSync`, `WebSocket`, `issueToken`, `Engine`, `buildApp`, `FAKE`, `cleanup`, `AppConfig` are already imported in this file from Slice 1.)

- [ ] **Step 2: Run to verify failure**

Run: `yarn vitest run server/api/stream.test.ts`
Expected: FAIL — backfill frames are still delivered (msgs.length > 0).

- [ ] **Step 3: Implement in `server/api/stream.ts`**

Add `live` to the querystring type and gate the backfill. Replace the handler body so the type param and the backfill block become:

```ts
  app.get<{ Params: { id: string }; Querystring: { token?: string; live?: string } }>(
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

      const live = req.query.live === "1" || req.query.live === "true";

      if (!live) {
        // Backfill persisted log — re-parse each raw line into its ClaudeEvent.
        for (const line of engine.getLog(id)) {
          const ev = parseLine(line) ?? { kind: "backfill" as const, raw: safeParse(line) };
          socket.send(JSON.stringify(ev));
        }
        // Already finished: close after backfill.
        if (session.status === "done" || session.status === "failed" || session.status === "killed") {
          socket.send(JSON.stringify({ kind: "other", raw: { engineExit: { status: session.status } } }));
          socket.close(1000, "ended");
          return;
        }
      }

      // Live subscription (always for live=1; and for non-terminal sessions otherwise).
      const unsub = engine.subscribe(id, (ev) => {
        socket.send(JSON.stringify(ev));
        if (ev.kind === "other" && (ev.raw as any)?.engineExit) {
          socket.close(1000, "ended");
        }
      });
      socket.on("close", () => unsub());
    }
  );
```

(Keep the `safeParse` helper at the bottom unchanged.)

- [ ] **Step 4: Run to verify pass**

Run: `yarn vitest run server/api/stream.test.ts`
Expected: PASS (existing 2 + new 1 = 3).

- [ ] **Step 5: Commit**

```bash
git add server/api/stream.ts server/api/stream.test.ts
git commit -m "feat(api): WS ?live=1 skips backfill (for follow-up turns)"
```

---

## Task 4: `POST /api/sessions/:id/messages`

**Files:**
- Modify: `server/api/routes.ts`, `server/api/server.test.ts`

- [ ] **Step 1: Write the failing tests** — append inside the `describe("HTTP API", ...)` block in `server/api/server.test.ts`

```ts
it("POST /messages resumes a finished session", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  const auth = { authorization: `Bearer ${token}` };
  const id = (await app.inject({ method: "POST", url: "/api/sessions", headers: auth, payload: { repo: "demo", prompt: "first" } })).json().id;
  // wait for the first turn to finish (fake is fast)
  await new Promise((r) => setTimeout(r, 400));
  const res = await app.inject({ method: "POST", url: `/api/sessions/${id}/messages`, headers: auth, payload: { prompt: "second" } });
  expect(res.statusCode).toBe(200);
  await app.close();
});

it("POST /messages on an unknown session is 404", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  const res = await app.inject({ method: "POST", url: "/api/sessions/nope/messages", headers: { authorization: `Bearer ${token}` }, payload: { prompt: "x" } });
  expect(res.statusCode).toBe(404);
  await app.close();
});

it("POST /messages with no prompt is 400", async () => {
  const { app } = setup();
  const token = (await app.inject({ method: "POST", url: "/api/login", payload: { password: "pw" } })).json().token;
  const id = (await app.inject({ method: "POST", url: "/api/sessions", headers: { authorization: `Bearer ${token}` }, payload: { repo: "demo", prompt: "p" } })).json().id;
  const res = await app.inject({ method: "POST", url: `/api/sessions/${id}/messages`, headers: { authorization: `Bearer ${token}` }, payload: {} });
  expect(res.statusCode).toBe(400);
  await app.close();
});
```

(`setup` already exists in this file and registers engines for cleanup.)

- [ ] **Step 2: Run to verify failure**

Run: `yarn vitest run server/api/server.test.ts`
Expected: FAIL — route not found (404 for the resume test too, but for the wrong reason / 200 expectation fails).

- [ ] **Step 3: Implement in `server/api/routes.ts`**

Add this route inside `registerRoutes`, after the `POST /api/sessions` handler:

```ts
  app.post<{ Params: { id: string }; Body: { prompt?: string } }>("/api/sessions/:id/messages", async (req, reply) => {
    if (!engine.get(req.params.id)) return reply.code(404).send({ error: "not found" });
    const prompt = req.body?.prompt;
    if (!prompt) return reply.code(400).send({ error: "prompt required" });
    try {
      engine.followUp(req.params.id, prompt);
      return { ok: true };
    } catch (err: any) {
      return reply.code(400).send({ error: String(err?.message ?? err) });
    }
  });
```

- [ ] **Step 4: Run to verify pass**

Run: `yarn vitest run server/api/server.test.ts`
Expected: PASS (existing 5 + new 3 = 8).

- [ ] **Step 5: Run the full backend suite + confirm exit 0**

Run: `yarn test; echo "EXIT=$?"`
Expected: all pass, `EXIT=0`.

- [ ] **Step 6: Commit**

```bash
git add server/api/routes.ts server/api/server.test.ts
git commit -m "feat(api): POST /api/sessions/:id/messages (follow-up turn)"
```

---

## Task 5: web API client — `followUp` + `openStream` live option

**Files:**
- Modify: `web/src/api.ts`

- [ ] **Step 1: Add `followUp` to the `api` object** (after `submit`)

```ts
  submit: (repo: string, prompt: string): Promise<{ id: string }> =>
    req("/api/sessions", { method: "POST", body: JSON.stringify({ repo, prompt }) }),
  followUp: (id: string, prompt: string): Promise<{ ok: boolean }> =>
    req(`/api/sessions/${id}/messages`, { method: "POST", body: JSON.stringify({ prompt }) }),
  kill: (id: string) => req(`/api/sessions/${id}`, { method: "DELETE" }),
```

- [ ] **Step 2: Add a `live` option to `openStream`**

```ts
/** Open the live event WebSocket for a session. Pass { live: true } to skip backfill. */
export function openStream(id: string, onEvent: (ev: any) => void, onClose: () => void, opts?: { live?: boolean }): WebSocket {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const q = `token=${encodeURIComponent(getToken())}${opts?.live ? "&live=1" : ""}`;
  const ws = new WebSocket(`${proto}://${location.host}/api/sessions/${id}/stream?${q}`);
  ws.onmessage = (m) => onEvent(JSON.parse(m.data));
  ws.onclose = onClose;
  return ws;
}
```

- [ ] **Step 3: Type-check (no runtime test for the client; build verifies)**

Run: `cd web && yarn build`
Expected: clean build, no type errors.

- [ ] **Step 4: Commit**

```bash
git add web/src/api.ts
git commit -m "feat(web): api.followUp + openStream live option"
```

---

## Task 6: `SessionView` follow-up input + live append

**Files:**
- Modify: `web/src/pages/SessionView.tsx`

- [ ] **Step 1: Replace `web/src/pages/SessionView.tsx` with the multi-turn version**

```tsx
import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router-dom";
import {
  AppBar, Toolbar, Typography, Box, Button, Chip, Stack, Paper, TextField,
} from "@mui/material";
import { api, openStream, type Session } from "../api";

const TERMINAL = new Set(["done", "failed", "killed"]);

// Rebuild the transcript from the persisted REST log (raw stream-json lines).
function renderFromLog(log: string[]): { text: string; events: string[] } {
  let text = "";
  const events: string[] = [];
  for (const line of log) {
    let raw: any;
    try { raw = JSON.parse(line); } catch { continue; }
    if (raw?.type === "stream_event" && raw.event?.delta?.type === "text_delta") {
      text += raw.event.delta.text ?? "";
    } else if (raw?.type === "system" && raw.subtype === "api_retry") {
      events.push(`↻ retry ${raw.attempt}/${raw.max_retries} (${raw.error})`);
    } else if (raw?.type === "result") {
      const cost = typeof raw.total_cost_usd === "number" ? raw.total_cost_usd : 0;
      events.push(`✓ result, cost $${cost.toFixed(4)}`);
    }
  }
  return { text, events };
}

export function SessionView() {
  const { id = "" } = useParams();
  const [session, setSession] = useState<Session | null>(null);
  const [text, setText] = useState("");
  const [events, setEvents] = useState<string[]>([]);
  const [followUp, setFollowUp] = useState("");
  const [sending, setSending] = useState(false);
  const endRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WebSocket | null>(null);

  // Append a single live event to the transcript.
  function applyLive(ev: any) {
    if (ev.kind === "text") setText((t) => t + ev.text);
    else if (ev.kind === "retry") setEvents((e) => [...e, `↻ retry ${ev.attempt}/${ev.maxRetries} (${ev.category})`]);
    else if (ev.kind === "result") setEvents((e) => [...e, `✓ result, cost $${(ev.costUsd ?? 0).toFixed(4)}`]);
  }

  useEffect(() => {
    let cancelled = false;
    setText(""); setEvents([]);
    api.session(id).then((r) => {
      if (cancelled) return;
      setSession(r.session);
      if (TERMINAL.has(r.session.status)) {
        const fromLog = renderFromLog(r.log);
        setText(fromLog.text);
        setEvents(fromLog.events);
      } else {
        wsRef.current = openStream(id, applyLive, () => {
          api.session(id).then((r2) => { if (!cancelled) setSession(r2.session); }).catch(() => {});
        });
      }
    }).catch(() => {});
    return () => { cancelled = true; wsRef.current?.close(); wsRef.current = null; };
  }, [id]);

  useEffect(() => { endRef.current?.scrollIntoView(); }, [text, events]);

  // Send a follow-up: server resumes the session; we tail only the new turn (live=1, no re-backfill).
  async function send() {
    const prompt = followUp.trim();
    if (!prompt || sending) return;
    setSending(true);
    try {
      await api.followUp(id, prompt);
      setFollowUp("");
      setSession((s) => (s ? { ...s, status: "running" } : s));
      wsRef.current?.close();
      wsRef.current = openStream(
        id,
        applyLive,
        () => api.session(id).then((r) => setSession(r.session)).catch(() => {}),
        { live: true }
      );
    } catch {
      // leave the input as-is so the user can retry
    } finally {
      setSending(false);
    }
  }

  const terminal = session != null && TERMINAL.has(session.status);
  const connecting = session != null && !terminal && text === "";

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
          {connecting && <Typography variant="caption" color="text.secondary">connecting…</Typography>}
          <Stack sx={{ mt: 2 }} spacing={0.5}>
            {events.map((e, i) => <Typography key={i} variant="caption" color="text.secondary">{e}</Typography>)}
          </Stack>
          <div ref={endRef} />
        </Paper>
        {terminal && (
          <Stack direction="row" spacing={1}>
            <TextField
              fullWidth size="small" placeholder="Follow up…" value={followUp}
              onChange={(e) => setFollowUp(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } }}
              disabled={sending}
            />
            <Button variant="contained" onClick={send} disabled={sending || followUp.trim() === ""}>Send</Button>
          </Stack>
        )}
      </Box>
    </Box>
  );
}
```

- [ ] **Step 2: Build**

Run: `cd web && yarn build`
Expected: clean build, no type errors.

- [ ] **Step 3: Commit**

```bash
git add web/src/pages/SessionView.tsx
git commit -m "feat(web): follow-up input on finished sessions, live-append new turn"
```

---

## Task 7: Verification (separate port, zero credit) — DO NOT restart :7420

**Files:** none (verification only)

- [ ] **Step 1: Full backend suite**

Run: `cd ~/src/agentic-dev && yarn test; echo "EXIT=$?"`
Expected: all green (Slice-1 + new ≈ 47 tests), `EXIT=0`.

- [ ] **Step 2: Web build**

Run: `cd ~/src/agentic-dev/web && yarn build`
Expected: clean.

- [ ] **Step 3: Multi-turn smoke on a NON-7420 port with the fake binary** (the live :7420 instance is left running and untouched)

```bash
cd ~/src/agentic-dev
SRC=$(mktemp -d); WT=$(mktemp -d); DATA=$(mktemp -d)
git init -q "$SRC/demo"; git -C "$SRC/demo" config user.email t@t.local; git -C "$SRC/demo" config user.name t
( cd "$SRC/demo" && echo hi>README.md && git add . && git commit -qm init )
AGENTIC_PASSWORD=smk AGENTIC_AUTH_SECRET=smk AGENTIC_PORT=7480 AGENTIC_HOST=127.0.0.1 \
  AGENTIC_SRC_ROOT="$SRC" AGENTIC_WORKTREES_ROOT="$WT" AGENTIC_DATA_DIR="$DATA" \
  AGENTIC_CLAUDE_BIN="$PWD/server/test/fixtures/fake-claude.sh" node_modules/.bin/tsx server/index.ts & SRV=$!
sleep 2
TOK=$(curl -s -X POST localhost:7480/api/login -H 'content-type: application/json' -d '{"password":"smk"}' | sed -E 's/.*"token":"([^"]+)".*/\1/')
ID=$(curl -s -X POST localhost:7480/api/sessions -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{"repo":"demo","prompt":"t1"}' | sed -E 's/.*"id":"([^"]+)".*/\1/')
sleep 2
echo "after turn1: $(curl -s localhost:7480/api/sessions/$ID -H "authorization: Bearer $TOK")"
echo "followUp: $(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:7480/api/sessions/$ID/messages -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{"prompt":"t2"}')"
sleep 2
echo "after turn2: $(curl -s localhost:7480/api/sessions/$ID -H "authorization: Bearer $TOK")"
kill $SRV 2>/dev/null; rm -rf "$SRC" "$WT" "$DATA"
```
Confirm: turn1 reaches `done` with a cost; the follow-up POST returns `200`; after turn2 the status is `done` again and `costUsd` is roughly **double** turn1's (cost accumulated). The log should contain two `result` events.

- [ ] **Step 4: (Optional) browser multi-turn check via Playwright** on port 7480 — submit, wait done, type a follow-up, Send, confirm the new turn streams and appends beneath the first. (Fake binary → zero credit.)

- [ ] **Step 5: Hand back for redeploy decision** — report results; do NOT restart the live `:7420` instance until the user says "redeploy". When they do: rebuild web (`cd web && yarn build`), then restart the `:7420` process with the new code.

---

## Self-Review (plan author)

**1. Spec coverage:** spawner `--resume` → Task 1 (§components row 1). `engine.followUp` + reuse-worktree + cost accumulate + busy/no-resume rejects → Task 2 (§components row 2, §decisions, §error-handling). `POST /messages` → Task 4 (§components row 3). WS `?live=1` → Task 3 (§components row 4). web `followUp`+`openStream` live → Task 5 (§components row 5). `SessionView` input + live append → Task 6 (§components row 6). Success criteria 1–5 → Task 7 smoke (and §13 carry-over). Turn-based reject (running→busy) → Task 2 + Task 4. No new Session fields → confirmed (reuse claudeSessionId/costUsd/status).

**2. Placeholder scan:** none — every code step shows complete code; commands have expected output. Task 4 Step 5 / Task 7 assert exit 0.

**3. Type consistency:** `resumeSessionId` named identically across `SpawnOptions`, `QueueItem`, `followUp` enqueue, and `start`'s `spawnClaude` call. `openStream(..., opts?: { live?: boolean })` signature matches its one new call site (Task 6, `{ live: true }`) and the existing call site (no opts). `api.followUp(id, prompt)` matches its use in `SessionView.send`. Cost accumulation reads `cur?.costUsd ?? 0` consistent with the `Session.costUsd: number | null` type.

**Note:** `Math.floor(performance.now())` / `process.pid` / `setTimeout` are used in tests (runtime), which is fine — the workflow-script restriction on time/RNG does not apply to application/test code.
