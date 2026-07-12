// SDK bridge: the production turn runner for the Rust server.
//
// The Rust server (axum/tokio/engine/transcript) owns everything latency-sensitive — this tiny Node
// child is ONLY the claude transport, reusing the official @anthropic-ai/claude-agent-sdk so the
// control protocol (initialize handshake + can_use_tool) and AskUserQuestion in-turn pausing work
// correctly. The Rust SdkRunner drives one of these per turn.
//
// Protocol with the Rust parent:
//   - config via env: SDK_BRIDGE_LOG (session jsonl, appended to — same shape the tailer reads),
//     SDK_BRIDGE_CWD, SDK_BRIDGE_MODEL, SDK_BRIDGE_RESUME, SDK_BRIDGE_MODE, SDK_BRIDGE_EFFORT.
//   - stdin (one JSON line each): a user turn `{"type":"user",...}` (first turn + follow-ups), OR a
//     control line `{"__bridge":"interrupt"}` / `{"__bridge":"end"}`.
//   - SIGTERM (Rust stop()) → abort the query and exit.
//   - exit code 0 on clean end, 1 on query error (a synthetic error result line is logged first).
//
// This is the Rust server's per-turn claude transport (Agent SDK); there is no other copy to sync.
import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { query, createSdkMcpServer, tool } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod/v4"; // match the SDK's own zod import so tool() schemas are the same instance

const LOG = process.env.SDK_BRIDGE_LOG;
const writeLog = (obj) => { try { appendFileSync(LOG, JSON.stringify(obj) + "\n"); } catch { /* best-effort */ } };

// ── Stderr capture: bridge writes any stderr from itself or the SDK's child `claude` subprocess
// to SDK_BRIDGE_STDERR (a path the engine sets). On a thrown query() failure the catch block at the
// bottom reads this file and includes its tail in the synthetic error result, so the next time a
// turn fails with `claude query failed: Claude Code process exited with code 1` we can SEE why
// instead of guessing. ──
const STDERR_PATH = process.env.SDK_BRIDGE_STDERR || "";
const STDERR_TAIL_BYTES = 8 * 1024;
// Structured boot trace — key=value fields journald can index. Pairs with the engine's
// `evt=spawn_opts_resolved` and `evt=bridge_spawning` to form a self-contained timeline
// of what the engine decided, what the runner handed to node, and what the bridge did
// with it. Any "Claude Code process exited with code 1" failure must show all three
// in the same journal slice.
process.stderr.write(
  `evt=sdk_bridge_boot cwd=${process.env.SDK_BRIDGE_CWD || ""} ` +
  `resume=${process.env.SDK_BRIDGE_RESUME || ""} ` +
  `model=${process.env.SDK_BRIDGE_MODEL || ""} ` +
  `mode=${process.env.SDK_BRIDGE_MODE || ""} ` +
  `permission_mode=${process.env.SDK_BRIDGE_PERMISSION_MODE || ""} ` +
  `stderr_path=${STDERR_PATH} stderr_path_set=${STDERR_PATH ? "yes" : "no"} ` +
  `hidden_skills=${process.env.SDK_BRIDGE_HIDDEN_SKILLS || ""} ` +
  `forced_on_skills=${process.env.SDK_BRIDGE_FORCED_ON_SKILLS || ""} ` +
  `enabled_plugins=${process.env.SDK_BRIDGE_ENABLED_PLUGINS || ""} ` +
  `hidden_mcp=${process.env.SDK_BRIDGE_HIDDEN_MCP || ""} ` +
  `extra_mcp=${process.env.SDK_BRIDGE_EXTRA_MCP ? "set" : ""} ` +
  `log_path=${process.env.SDK_BRIDGE_LOG || ""}\n`
);
process.stderr.on("data", (chunk) => {
  if (!STDERR_PATH) return;
  try {
    // Best-effort append; failures here are non-fatal (the parent will see a partial tail at most).
    appendFileSync(STDERR_PATH, chunk);
  } catch (e) {
    process.stderr.write(`evt=sdk_bridge_stderr_append_failed path=${STDERR_PATH} error=${e.message}\n`);
  }
});

// ── Pushable streaming input (engine.write → SDK query prompt) ──
const queue = [];
let wake = null;
let inputDone = false;
const drain = () => { const w = wake; wake = null; w?.(); };
const input = (async function* () {
  while (true) {
    if (queue.length) { yield queue.shift(); continue; }
    if (inputDone) return;
    await new Promise((r) => { wake = r; });
  }
})();

// ── permission mode ──
// Only "plan"/"default"/"acceptEdits" are INTERACTIVE: tools that need permission are parked for an
// allow/deny round-trip with the Rust parent (which relays the app's decision). Everything else —
// "bypassPermissions", unset, or any unknown/legacy value — keeps today's auto-allow (an explicit
// whitelist, not an exclude list, so a stray value can never accidentally start parking tools).
// Defined before canUseTool because the callback closes over `interactivePerms`.
const permissionMode = process.env.SDK_BRIDGE_PERMISSION_MODE || "";
const interactivePerms = ["plan", "default", "acceptEdits"].includes(permissionMode);
// Non-Claude model providers (deepseek, etc. via litellm) time out idle API streams
// after ~60s. When canUseTool parks waiting for the user's permission decision, the
// API connection sits idle until the user responds. Bump the CLI's stream timeout
// to 10 min so the user has time to review and allow/deny the permission card.
// Default is 60s; permission cards routinely take 1-2 min of human review.
if (interactivePerms && !process.env.CLAUDE_CODE_STREAM_CLOSE_TIMEOUT) {
  process.env.CLAUDE_CODE_STREAM_CLOSE_TIMEOUT = "600000";
}
// A worker bridge (spawned by the delegate fan-out) must NOT mount the delegate tool (no recursive
// fan-out) and must NOT park on AskUserQuestion (headless, no user).
const isWorker = process.env.SDK_BRIDGE_WORKER === "1";

// ── Pending AskUserQuestion (canUseTool parks here until the next user line is the answer) ──
const ask = { resolve: null, input: null };

// ── Pending perm/plan permission request. canUseTool parks it here and writes a synthetic
// `agentic_perm` log line (which streamParser turns into a perm/plan card); the Rust parent answers
// with {"__bridge":"perm",...} → the line handler resolves it below. Only one interactive request is
// ever live at a time — the turn blocks on exactly one canUseTool call. ──
const perm = { resolve: null, input: null, kind: null, id: null };
let permSeq = 0;
// Resolve a parked perm/plan with deny (interrupt / session end). Writes a resolution marker so a
// reseed renders the card decided, not still-actionable.
const denyPerm = (message) => {
  if (!perm.resolve) return;
  const { resolve, kind, id } = perm;
  perm.resolve = null; perm.input = null; perm.kind = null; perm.id = null;
  if ((kind === "perm" || kind === "plan") && id) {
    writeLog({ type: "agentic_perm_resolved", id, decision: "deny", at: Date.now() });
  }
  resolve({ behavior: "deny", message });
};

const canUseTool = async (toolName, toolInput) => {
  // The delegate fan-out tool is always auto-allowed (it is never an interactive approval).
  if (typeof toolName === "string" && toolName.startsWith("mcp__agentic__")) {
    return { behavior: "allow", updatedInput: toolInput };
  }
  if (toolName === "AskUserQuestion") {
    // Workers are headless (no user) → deny instead of parking forever.
    if (isWorker) return { behavior: "deny", message: "delegated workers cannot ask questions" };
    if (process.env.SDK_BRIDGE_DEBUG) process.stderr.write("ASK_PARKED\n");
    return await new Promise((resolve) => { ask.resolve = resolve; ask.input = toolInput; });
  }
  // A worker is a ONE-SHOT headless spawn: it runs to completion and the process exits the moment it
  // returns its final text — there is no interactive loop left to receive a later <task-notification>.
  // So any tool that spawns ASYNC / background work whose result only arrives via such a notification
  // (a sub-agent via Agent/Task, an in-process Workflow) can never deliver back: the worker either
  // returns a useless "I started a background job, waiting for the notification" placeholder or hangs
  // until the fan-out deadline kills it — and a worker with no result line is recorded as FAILED. Deny
  // them so the worker does the work INLINE itself. (delegate is likewise not mounted, and
  // AskUserQuestion is denied above, for this same one-shot reason.)
  if (isWorker && ["Workflow", "Task", "Agent"].includes(toolName)) {
    return {
      behavior: "deny",
      message:
        `delegated workers cannot spawn sub-agents or background work (${toolName}): you are a ` +
        "one-shot worker that exits as soon as you return, so a spawned Workflow/Task/Agent's result " +
        "would arrive via a notification you will never receive. Do the work yourself in this turn — " +
        "call WebSearch/WebFetch/Read/Grep/Bash/etc. directly — and return your findings as your " +
        "final text. Do NOT try to delegate, fan out, or kick off a background job.",
    };
  }
  // bypass / unset: auto-allow everything else (today's behavior).
  if (!interactivePerms) return { behavior: "allow", updatedInput: toolInput };
  // plan / default / acceptEdits: park the request and surface a card via a synthetic log line. The
  // Rust parent answers via POST /api/sessions/:id/permission → {"__bridge":"perm",decision,feedback}.
  const kind = toolName === "ExitPlanMode" ? "plan" : "perm";
  const id = `perm-${++permSeq}`;
  writeLog(
    kind === "plan"
      ? { type: "agentic_perm", permKind: "plan", id, plan: (toolInput && typeof toolInput.plan === "string") ? toolInput.plan : "", at: Date.now() }
      : { type: "agentic_perm", permKind: "perm", id, tool: toolName, input: toolInput, at: Date.now() },
  );
  if (process.env.SDK_BRIDGE_DEBUG) process.stderr.write(`PERM_PARKED ${kind} ${toolName}\n`);
  return await new Promise((resolve) => { perm.resolve = resolve; perm.input = toolInput; perm.kind = kind; perm.id = id; });
};

// ── delegate fan-out (in-process MCP tool) ──
// Only the MAIN session mounts this; worker bridges (SDK_BRIDGE_WORKER) skip it so a worker cannot
// recursively fan out. The tool parks, writes an `agentic_delegate_request` log line the Rust engine
// picks up, runs the cheap workers, and replies via stdin {"__bridge":"delegate", id, summaries}.
const delegates = new Map(); // request id -> resolve
let delegSeq = 0;
const delegateServer = isWorker ? null : createSdkMcpServer({
  name: "agentic",
  version: "0.1.0",
  tools: [
    tool(
      "delegate",
      "Fan out subtasks to cheap worker models that run in parallel; returns each worker's distilled result. Each task has two modes. READ (default): read-only explore / search / locate / read-heavy work — the worker shares your worktree but should not edit it. WRITE (`write: true`): the worker runs in its OWN isolated git worktree (forked from your current working state, uncommitted edits included) where it CAN freely edit files; when it finishes, its changes come back to you as a unified-diff PATCH (one block per repo) appended to its result. Nothing is auto-applied — YOU stay the single writer: review each patch and `git apply` it yourself in the matching repo subdir, resolving any conflicts. Use WRITE workers to parallelize INDEPENDENT code changes (disjoint files/areas) so a fan-out can modify code, not just investigate it; keep tightly-coupled edits in one worker (or your own thread) to avoid overlapping patches. Pass a short `title` naming the batch (it becomes the workflow card title) and an optional `phase` per task to group workers under phase headers — just like a native Workflow. Do NOT set a task's `model` unless the user explicitly names one — leave it UNSET so the system auto-routes each task to the cheapest capable model (setting it pins the task and bypasses routing). Keep each task's `prompt` compact: say what to do and give the repo path, then let the worker read the files itself — do NOT paste file contents or long absolute-path lists into the prompt, and prefer forward slashes. A large, escape-heavy payload makes this whole tool call more likely to be emitted as malformed JSON (which is rejected and costs a wasted retry turn).",
      { title: z.string().optional(), tasks: z.array(z.object({ prompt: z.string(), role: z.string().optional(), model: z.string().optional(), phase: z.string().optional(), write: z.boolean().optional() })).min(1) },
      async ({ title, tasks }) => {
        const id = `deleg-${++delegSeq}`;
        const runId = `wfdeleg-${process.pid}-${delegSeq}`;
        writeLog({ type: "agentic_delegate_request", id, runId, title, tasks, at: Date.now() });
        const summaries = await new Promise((resolve) => { delegates.set(id, resolve); });
        return { content: [{ type: "text", text: JSON.stringify(summaries) }] };
      },
    ),
  ],
});
// Resolve any parked delegate tools (interrupt / session end) so the turn never hangs on them.
const failDelegates = (message) => {
  for (const resolve of delegates.values()) resolve([{ error: message }]);
  delegates.clear();
};


// ── extraArgs: effort / per-session settings passed through to the SDK query ──
const ultracode = process.env.SDK_BRIDGE_MODE === "ultracode";
const extraArgs = {};
if (process.env.SDK_BRIDGE_EFFORT && !ultracode) extraArgs.effort = process.env.SDK_BRIDGE_EFFORT;
// MUST be kebab-case: the SDK forwards each extraArgs key to the claude CLI verbatim as `--<key>`,
// and the CLI flag is `--permission-mode` (camelCase `--permissionMode` is rejected → "unknown option"
// → "Claude Code process exited with code 1" the instant a permissionMode session spawns). `effort`
// above is a single word so `--effort` is already correct.
// Permission mode routing:
// - When the bridge's canUseTool handles permissions (interactivePerms true), don't
//   forward the mode to the CLI. The SDK's default "default" is correct — it makes the
//   CLI ask the SDK, the SDK calls canUseTool, and the bridge parks until the engine
//   answers. Adding a duplicate --permission-mode via extraArgs would compete with the
//   SDK's internal handling (overriding to bypassPermissions skips canUseTool entirely).
// - When interactivePerms is false, the bridge auto-allows in canUseTool; pass the mode
//   through so the CLI layer agrees.
if (permissionMode && !interactivePerms) {
  extraArgs["permission-mode"] = permissionMode;
}

const parseNameListEnv = (envName) => {
  try {
    const raw = process.env[envName] || "[]";
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter((s) => typeof s === "string" && s.trim()) : [];
  } catch {
    return [];
  }
};

const settings = {};
if (ultracode) settings.ultracode = true;
const hiddenSkills = parseNameListEnv("SDK_BRIDGE_HIDDEN_SKILLS");
const forcedOnSkills = parseNameListEnv("SDK_BRIDGE_FORCED_ON_SKILLS");
if (hiddenSkills.length || forcedOnSkills.length) {
  // Build skillOverrides: "off" entries first, then "on" entries (forced-on wins if a name
  // appears in both — API rejects that combination, but belt-and-suspenders here too).
  const overrides = {};
  for (const name of hiddenSkills) { if (name.trim()) overrides[name.trim()] = "off"; }
  for (const name of forcedOnSkills) { if (name.trim()) overrides[name.trim()] = "on"; }
  if (Object.keys(overrides).length) settings.skillOverrides = overrides;
}
// Per-session EXPLICIT plugin enable map: `{"<plugin>@<marketplace>": true|false}` resolved by the
// engine from the New-request Filters selection × the installed-plugin registry. Explicit
// enabledPlugins values in this command-line settings layer always win over the on-disk settings
// files, so `true` FORCE-ENABLES a selected plugin even when it is disabled (or was never enabled)
// in ~/.claude/settings.json — the app's toggle is authoritative, not hide-only. `false` disables
// a deselected plugin for this session. Env unset/empty → no key → CLI defaults apply.
const parseObjectEnv = (envName) => {
  try {
    const parsed = JSON.parse(process.env[envName] || "{}");
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
};
const enabledPlugins = Object.entries(parseObjectEnv("SDK_BRIDGE_ENABLED_PLUGINS"))
  .filter(([id]) => typeof id === "string" && id.trim());
if (enabledPlugins.length) {
  settings.enabledPlugins = Object.fromEntries(enabledPlugins.map(([id, on]) => [id.trim(), on === true]));
}
// Per-session MCP hidden list: set settings.disabledMcpjsonServers so Claude Code
// disables those .mcp.json-configured servers for this session.
const hiddenMcp = (() => {
  try {
    const parsed = JSON.parse(process.env.SDK_BRIDGE_HIDDEN_MCP || "[]");
    return Array.isArray(parsed) ? parsed.filter((n) => typeof n === "string" && n.trim()) : [];
  } catch { return []; }
})();
if (hiddenMcp.length) settings.disabledMcpjsonServers = hiddenMcp;

// Per-session extra MCP server defs: build {[name]: config} for the mcpServers option.
const extraMcpDefs = (() => {
  try {
    const parsed = JSON.parse(process.env.SDK_BRIDGE_EXTRA_MCP || "[]");
    return Array.isArray(parsed) ? parsed : [];
  } catch { return []; }
})();
const hiddenMcpSet = new Set(hiddenMcp);
const extraMcpServers = {};
for (const def of extraMcpDefs) {
  if (!def || typeof def.name !== "string" || !def.name.trim()) continue;
  if (hiddenMcpSet.has(def.name)) continue;
  if (typeof def.command === "string") {
    extraMcpServers[def.name] = {
      command: def.command,
      ...(Array.isArray(def.args) ? { args: def.args } : {}),
      ...(def.env && typeof def.env === "object" ? { env: def.env } : {}),
    };
  } else if (typeof def.url === "string") {
    extraMcpServers[def.name] = {
      type: def.type || "http",
      url: def.url,
      ...(def.headers && typeof def.headers === "object" ? { headers: def.headers } : {}),
    };
  }
}

if (Object.keys(settings).length) extraArgs.settings = JSON.stringify(settings);

const abort = new AbortController();
const model = process.env.SDK_BRIDGE_MODEL || "";
const resume = process.env.SDK_BRIDGE_RESUME || "";

async function main() {

// ── one-shot title/retitle mode (Rust engine; not the turn loop) ──
// When SDK_BRIDGE_MODE is "title" or "retitle", do a single haiku call,
// write the answer to stdout, and exit 0. No log file, no resume, no
// AskUserQuestion support, no for-await loop. Mirrors what
// engine::title_bridge::TitleBridge expects on the wire.
const oneShotMode = process.env.SDK_BRIDGE_MODE;
if (oneShotMode === "title" || oneShotMode === "retitle") {
  (async () => {
    const readline = await import("node:readline");
    const rl = readline.createInterface({ input: process.stdin });
    let userText = "";
    for await (const line of rl) { userText = line; break; } // single line
    rl.close();
    const systemPrompt = oneShotMode === "title"
      ? "你的任务是根据用户给出的请求,生成一个会话标题。要求:\n- 用中文,5 到 12 个字\n- 只输出标题本身,不要任何解释、不要标点包裹、不要 markdown\n- 标题要能反映\"用户在做什么\",而不是\"用户最后一句话\"\n- 如果用户输入很短(比如 \"ok\"),用会话的整体意图来概括\n"
      : "你的任务是判断这个 session 的标题是否需要更新。\n\n输入包含:\n- 当前标题\n- 最近 10 条消息 (按时间顺序)\n\n输出必须是以下 JSON 之一,不要其他内容,不要 markdown 代码块:\n- 不需要改: {\"change\": false}\n- 需要改:   {\"change\": true, \"title\": \"<5-12 个汉字的新标题>\"}\n\n判断标准:\n- 标题要反映\"session 当前在做什么\",而不是第一句话或最后一句话\n- 如果当前标题仍然准确,输出 {\"change\": false}\n- 如果话题已经明显改变,输出新标题\n- 新标题跟旧标题不能完全相同\n";
    let assistantText = "";
    try {
      for await (const msg of query({
        prompt: userText,
        options: {
          cwd: process.env.SDK_BRIDGE_CWD || process.cwd(),
          env: process.env,
          model: "haiku",
          maxTurns: 1,
          systemPrompt,
        },
      })) {
        if (msg && msg.type === "assistant" && Array.isArray(msg.message?.content)) {
          for (const block of msg.message.content) {
            if (block && block.type === "text" && typeof block.text === "string") {
              assistantText += block.text;
            }
          }
        }
      }
    } catch (err) {
      process.stderr.write(`fake-sdk-bridge one-shot ${oneShotMode} failed: ${err && err.message ? err.message : err}\n`);
      process.exit(1);
    }
    process.stdout.write(assistantText);
    process.exit(0);
  })();
  // Stops the rest of the file from running.
  return;
}

// ── Tier-1 harness rules → appended system prompt (main session only) ──
// The engine sets SDK_BRIDGE_APPEND_SYSTEM_PROMPT to the routing + fan-out discipline on the MAIN
// turn only (worker SpawnOptions leave it unset, so this env is absent for a worker). APPEND — never
// replace — so Claude Code's base prompt + tool instructions stay intact. The `!isWorker` check is a
// belt-and-suspenders guard on top of that. Rides the same extraArgs → claude-CLI path
// (`--append-system-prompt`) as permission-mode / effort / settings.
const appendSystemPrompt = process.env.SDK_BRIDGE_APPEND_SYSTEM_PROMPT || "";
if (!isWorker && appendSystemPrompt.trim()) extraArgs["append-system-prompt"] = appendSystemPrompt;

// ── prefer `delegate` over the native `Workflow` tool ──
// A Workflow's agents run in-process on the MAIN model and can't be cost-routed; `delegate` is the
// routed equivalent. Nudge the agent there: DENY the first Workflow call (steering it to delegate),
// but allow a retry so Workflow stays available as a genuine fallback for multi-stage scripting that a
// single delegate batch can't express. Main session only — workers don't fan out. The bridge process
// PERSISTS across user turns, so the flag is reset at the start of each new turn (see rl.on("line")
// below) — the nudge fires once PER TURN, not once per session.
let workflowDenied = false;
const workflowHook = isWorker ? undefined : async (hookInput) => {
  if (hookInput?.tool_name !== "Workflow") return { continue: true };
  if (workflowDenied) return { continue: true }; // already nudged once this turn → allow as fallback
  workflowDenied = true;
  return {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason:
        "Prefer the `delegate` tool for parallel / fan-out work: it routes each task to the cheapest " +
        "capable model (your registered providers + the native Claude tiers) and renders the same " +
        "titled, phased workflow card. A `Workflow` runs its agents UNROUTED on the main model. Re-do " +
        "this fan-out with `delegate` (pass a `title` and a per-task `phase`). ONLY if you genuinely " +
        "need multi-stage Workflow scripting (loops / conditionals / pipelines across rounds) that a " +
        "single delegate batch cannot express, call `Workflow` again now and it will proceed.",
    },
  };
};

const q = query({
  prompt: input,
  options: {
    cwd: process.env.SDK_BRIDGE_CWD || process.cwd(),
    env: process.env,
    // Partials ON → token-by-token live typing. The SDK emits `stream_event` lines (text_delta /
    // thinking_delta) as the model streams, then ONE complete `assistant` message that restates the
    // same blocks. The server marks the stream_event frames `delta:true` and the assistant frames
    // `delta:false` (sealed); the Android reducer accumulates deltas into the growing node and lets a
    // sealed block FINALIZE that run (replace, not append) — so the duplicate is reconciled by block
    // identity, not double-rendered. `display:"summarized"` makes thinking carry readable text (it is
    // "omitted" → empty by default on Opus 4.8), so the Thinking card streams too.
    includePartialMessages: true,
    thinking: { type: "adaptive", display: "summarized" },
    canUseTool,
    abortController: abort,
    // Capture the claude CLI subprocess's OWN stderr. The SDK pipes the child's stderr to itself (it
    // never reaches node's stderr), which is why every failed turn logged stderr_tail_len=0 and the
    // real error ("unknown option '--permissionMode'") was invisible. Routing it to STDERR_PATH makes
    // the synthetic error result's stderr_tail carry the actual CLI failure — self-diagnosing.
    stderr: (d) => { if (STDERR_PATH) { try { appendFileSync(STDERR_PATH, d); } catch { /* best-effort */ } } },
    ...(model ? { model } : {}),
    ...(resume ? { resume } : {}),
    ...(delegateServer || Object.keys(extraMcpServers).length
      ? { mcpServers: { ...extraMcpServers, ...(delegateServer ? { agentic: delegateServer } : {}) } }
      : {}),
    ...(workflowHook ? { hooks: { PreToolUse: [{ hooks: [workflowHook] }] } } : {}),
    ...(Object.keys(extraArgs).length ? { extraArgs } : {}),
  },
});

// ── ask-answer mapping ──
const stripPreamble = (t) => { const i = t.lastIndexOf("\n\n---\n\n"); return i >= 0 ? t.slice(i + 7) : t; };
const userContent = (line) => {
  try { const o = JSON.parse(line); return typeof o?.message?.content === "string" ? o.message.content : ""; }
  catch { return ""; }
};
const buildAnswers = (askInput, answerText) => {
  const qs = Array.isArray(askInput?.questions) ? askInput.questions : [];
  if (qs.length <= 1) return qs.length === 1 ? { [qs[0].question ?? ""]: answerText } : {};
  const lines = answerText.split("\n");
  const out = {};
  for (const qq of qs) {
    const key = qq.question ?? "";
    const line = lines.find((l) => l.startsWith(key + ":"));
    out[key] = line ? line.slice(key.length + 1).trim() : answerText;
  }
  return out;
};

// ── stdin: user turns + control lines from the Rust parent ──
const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
  if (!line) return;
  try {
    const o = JSON.parse(line);
    if (o && o.__bridge === "interrupt") {
      const stranded = ask.resolve;
      if (stranded) { ask.resolve = null; ask.input = null; stranded({ behavior: "deny", message: "interrupted" }); }
      denyPerm("interrupted");
      failDelegates("interrupted");
      try { void q.interrupt(); } catch { /* */ }
      return;
    }
    if (o && o.__bridge === "delegate") {
      // The engine finished a delegate fan-out — resolve the parked tool with the worker summaries.
      const resolve = delegates.get(o.id);
      if (resolve) { delegates.delete(o.id); resolve(Array.isArray(o.summaries) ? o.summaries : []); }
      return;
    }
    if (o && o.__bridge === "perm") {
      // Answer to a parked perm/plan request (from POST /api/sessions/:id/permission via the parent).
      if (perm.resolve) {
        const { resolve, kind, id, input } = perm;
        perm.resolve = null; perm.input = null; perm.kind = null; perm.id = null;
        const decision = o.decision === "allow" ? "allow" : "deny";
        if (id) writeLog({ type: "agentic_perm_resolved", id, decision, at: Date.now() });
        if (decision === "allow") {
          // Approving a plan exits plan mode; continue in `default` so each later tool still prompts.
          if (kind === "plan") { try { void q.setPermissionMode("default"); } catch { /* */ } }
          resolve({ behavior: "allow", updatedInput: input ?? {} });
        } else {
          const feedback = typeof o.feedback === "string" && o.feedback ? o.feedback : "denied by user";
          resolve({ behavior: "deny", message: feedback });
        }
      }
      return;
    }
    if (o && o.__bridge === "end") { inputDone = true; drain(); return; }
  } catch { /* not a control line; treat as a user message below */ }
  // A user turn. If a question is parked, THIS line is the answer → resume the same turn.
  if (ask.resolve) {
    const resolve = ask.resolve; const askInput = ask.input ?? {};
    ask.resolve = null; ask.input = null;
    const answerText = stripPreamble(userContent(line));
    const answers = buildAnswers(askInput, answerText);
    if (process.env.SDK_BRIDGE_DEBUG) process.stderr.write("ANSWER_RESOLVED " + JSON.stringify(answers) + "\n");
    resolve({ behavior: "allow", updatedInput: { ...askInput, answers } });
    return;
  }
  if (process.env.SDK_BRIDGE_DEBUG) process.stderr.write("QUEUED_MSG\n");
  workflowDenied = false; // new user turn → re-nudge to delegate on this turn's first Workflow call
  try { queue.push(JSON.parse(line)); drain(); } catch { /* drop malformed */ }
});
rl.on("close", () => { inputDone = true; drain(); });

// ── teardown on SIGTERM (Rust stop()) ──
const teardown = () => { try { abort.abort(); } catch { /* */ } failDelegates("session ended"); inputDone = true; drain(); };
process.on("SIGTERM", teardown);
process.on("SIGINT", teardown);

// ── pump the SDK message stream into the log, then exit ──
(async () => {
  let exitCode = 0;
  try {
    for await (const msg of q) writeLog(msg);
  } catch (err) {
    // A deliberate stop (Rust stop() → SIGTERM → abort.abort()) is NOT a failure: the user asked to
    // stop. Logging a synthetic error result here is what put a red "claude query failed: Operation
    // aborted" line in the transcript and got it classified as claude_error. Only log for a REAL query
    // failure (auth, spawn/ENOENT, a usage limit thrown as an exception) — i.e. when we did NOT abort.
    if (abort.signal.aborted) {
      exitCode = 0;
    } else {
      exitCode = 1;
      const msg = err instanceof Error ? err.message : String(err);
      const stack = err instanceof Error ? err.stack : "";
      // Read the tail of the stderr capture so the next failed turn shows WHY the SDK query
      // threw (e.g. "Claude Code process exited with code 1") instead of a useless empty result.
      // Without this, the engine + Android only ever see "Claude Code process exited with code 1"
      // and we can't tell auth-failed vs OOM vs cli-version-mismatch vs cwd-missing apart.
      let stderrTail = "";
      if (STDERR_PATH) {
        try {
          const fs = require("node:fs");
          const stat = fs.statSync(STDERR_PATH);
          const start = Math.max(0, stat.size - STDERR_TAIL_BYTES);
          const fd = fs.openSync(STDERR_PATH, "r");
          const buf = Buffer.alloc(stat.size - start);
          fs.readSync(fd, buf, 0, buf.length, start);
          fs.closeSync(fd);
          stderrTail = buf.toString("utf8").trim();
        } catch { /* ignore */ }
      }
      writeLog({
        type: "result",
        subtype: "error_during_execution",
        is_error: true,
        result: `claude query failed: ${msg}`,
        stack,
        stderr_tail: stderrTail,
        stderr_path: STDERR_PATH || undefined,
      });
      // Structured bridge-side trace — one line per thrown query(). Pairs with the engine's
      // `evt=spawn_opts_resolved` + `evt=bridge_spawning` + `evt=stderr_pipe_opened` to give a
      // complete diagnosis of "Claude Code process exited with code 1" failures from journald
      // alone (no transcript digging required).
      const stderrTailSummary = stderrTail ? stderrTail.replace(/\s+/g, " ").slice(-200) : "";
      process.stderr.write(
        `evt=sdk_bridge_query_threw err=${msg.replace(/[\r\n]+/g, " ")} ` +
        `stderr_tail_len=${stderrTail.length} stderr_tail_summary=${JSON.stringify(stderrTailSummary)}\n`
      );
    }
  } finally {
    const stranded = ask.resolve;
    ask.resolve = null; ask.input = null;
    stranded?.({ behavior: "deny", message: "session ended" });
    denyPerm("session ended");
    failDelegates("session ended");
    rl.close();
    process.exit(exitCode);
  }
})();
}

main();
