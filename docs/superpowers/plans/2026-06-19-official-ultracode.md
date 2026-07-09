# Official ultracode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace agentic-dev's custom three-way orchestration preamble (Normal / Workflows / Ultra-code) with Claude Code's official ultracode, exposed as a single on/off switch.

**Architecture:** When the switch is on, the backend passes `--settings '{"ultracode":true}'` to `claude -p` (the documented non-interactive way to enable official ultracode: xhigh effort + automatic dynamic-workflow orchestration). The custom `MODE_PREAMBLE` text injection is deleted; the always-on `OUTBOX_NOTE` preamble stays. The existing `mode` column/field is reused with a collapsed value space (`null` = off, `"ultracode"` = on); legacy rows are normalized on read.

**Tech Stack:** Backend = TypeScript + Node + Fastify + better-sqlite3, tested with Vitest against `fake-claude.sh`. Client = Kotlin + Jetpack Compose (Material 3 Expressive).

## Global Constraints

- Backend package manager is **yarn**, never `npm install`. (`agentic-dev/CLAUDE.md`)
- The worktree starts with **no `node_modules`** — run `yarn install` once before tests.
- **Never hit the real `claude` in tests** — use `server/test/fixtures/fake-claude*.sh`. (`agentic-dev/CLAUDE.md`)
- The engine must stay **free of Fastify imports**. (`agentic-dev/CLAUDE.md`)
- Driver invariant: `claude -p <prompt> --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions`, non-bare, cwd = worktree. (`agentic-dev/CLAUDE.md`)
- **Android: do NOT build in the worktree** (no Gradle wrapper / keystore). Edit in the worktree, push to master, then build+test in `~/src/agentic-dev-android`; deliver the APK renamed `YYYYMMDD-HHMM.apk` into `outbox/`. (`agentic-dev-android/CLAUDE.md`)
- Compose Material3 is pinned to `1.4.0-alpha18`; do not bump it. (`agentic-dev-android/CLAUDE.md`)
- The official ultracode value string is exactly `"ultracode"`. The `--settings` JSON is exactly `{"ultracode":true}` (no spaces).

---

### Task 1: Backend — emit official ultracode flag, delete custom preamble

**Files:**
- Modify: `server/engine/spawner.ts` (delete `MODE_PREAMBLE` `:36-39`; `preambleFor` `:95-97`; `composeUserText` `:99-103`; `buildSpec` `:110-134`; `SpawnOptions.mode` comment `:15`)
- Modify: `server/engine/engine.ts:202` and `:496` (drop the `s.mode` arg to `composeUserText`)
- Modify: `server/engine/types.ts:26` (comment only)
- Test: `server/engine/spawner.test.ts:86-96` (replace the ultra-preamble test)

**Interfaces:**
- Produces: `composeUserText(text: string): string` (the `mode` parameter is removed). `buildSpec` adds `--settings`, `{"ultracode":true}` to argv when `opts.mode === "ultracode"`, and in that case omits `--effort`.
- Consumes: `SpawnOptions.mode?: string | null` (unchanged shape; value space is now `null`/`"ultracode"`).

- [ ] **Step 1: Preflight — install deps (once)**

Run: `cd ~/src/agentic-dev && yarn install`
Expected: completes; `node_modules/` now exists.

- [ ] **Step 2: Rewrite the failing spawner test**

In `server/engine/spawner.test.ts`, replace the test at lines 86-96 (`"prepends the ultracode preamble for mode=ultra, nothing for normal"`) with:

```ts
  it("mode=ultracode adds --settings ultracode and omits --effort; off adds neither", async () => {
    const outU = join(tmpdir(), `argv-uc-${process.pid}-${Math.floor(performance.now())}.txt`);
    const hU = spawnClaude({ bin: ECHO, cwd: tmpdir(), prompt: "do the thing", mode: "ultracode", effort: "high", env: { FAKE_ARGS_OUT: outU }, logPath: lp(), unit: "t" });
    await new Promise((r) => hU.on("exit", r));
    const aU = readFileSync(outU, "utf8");
    expect(aU).toContain("--settings");
    expect(aU).toContain('{"ultracode":true}');
    expect(aU).not.toContain("--effort");        // ultracode owns reasoning effort
    expect(aU).toContain("Delivering files");    // OUTBOX_NOTE still prepended

    const outN = join(tmpdir(), `argv-off-${process.pid}-${Math.floor(performance.now())}.txt`);
    const hN = spawnClaude({ bin: ECHO, cwd: tmpdir(), prompt: "do the thing", mode: null, env: { FAKE_ARGS_OUT: outN }, logPath: lp(), unit: "t" });
    await new Promise((r) => hN.on("exit", r));
    const aN = readFileSync(outN, "utf8");
    expect(aN).not.toContain("--settings");
    expect(aN).not.toContain("ultracode");
  });
```

- [ ] **Step 3: Run the test — expect failure**

Run: `cd ~/src/agentic-dev && yarn vitest run server/engine/spawner.test.ts`
Expected: FAIL — the suite won't compile (`composeUserText` still takes `mode`) and/or the new assertions fail (no `--settings` emitted yet).

- [ ] **Step 4: Delete `MODE_PREAMBLE`**

In `server/engine/spawner.ts`, delete lines 34-39 (the comment block + the `const MODE_PREAMBLE: Record<string, string> = { … };`).

- [ ] **Step 5: Simplify `preambleFor` and `composeUserText`**

Replace:

```ts
function preambleFor(mode?: string | null): string {
  return [OUTBOX_NOTE, mode ? MODE_PREAMBLE[mode] : null].filter(Boolean).join("\n");
}

/** Compose one user turn's text (preamble + the user's prompt). Used for the one-shot argv prompt and
 *  for every streaming stdin message, so both modes carry the same outbox/orchestration preamble. */
export function composeUserText(mode: string | null | undefined, text: string): string {
  return `${preambleFor(mode)}\n\n---\n\n${text}`;
}
```

with:

```ts
function preambleFor(): string {
  return OUTBOX_NOTE;
}

/** Compose one user turn's text (preamble + the user's prompt). Used for the one-shot argv prompt and
 *  for every streaming stdin message, so both carry the same always-on outbox preamble. Orchestration
 *  is no longer a preamble — official ultracode is enabled via the --settings flag in buildSpec. */
export function composeUserText(text: string): string {
  return `${preambleFor()}\n\n---\n\n${text}`;
}
```

- [ ] **Step 6: Emit `--settings` for ultracode in `buildSpec`; skip `--effort` when on**

In `buildSpec`, replace:

```ts
  const extra: string[] = [];
  if (opts.model) extra.push("--model", opts.model);
  if (opts.effort) extra.push("--effort", opts.effort);
```

with:

```ts
  const extra: string[] = [];
  const ultracode = opts.mode === "ultracode";
  if (opts.model) extra.push("--model", opts.model);
  // Official ultracode forces xhigh itself; passing --effort too would conflict, so omit it when on.
  if (opts.effort && !ultracode) extra.push("--effort", opts.effort);
  // Official ultracode session setting = xhigh + automatic dynamic-workflow orchestration.
  if (ultracode) extra.push("--settings", '{"ultracode":true}');
```

Then, in the non-streaming branch of `buildSpec`, replace:

```ts
  const prompt = composeUserText(opts.mode, opts.prompt);
```

with:

```ts
  const prompt = composeUserText(opts.prompt);
```

- [ ] **Step 7: Update the `SpawnOptions.mode` comment**

In `server/engine/spawner.ts`, change line 15 from:

```ts
  mode?: string | null;      // orchestration preamble: "workflows" | "ultra" (else none)
```

to:

```ts
  mode?: string | null;      // "ultracode" => pass --settings {"ultracode":true} (else off)
```

- [ ] **Step 8: Fix the two `composeUserText` call sites in the engine**

In `server/engine/engine.ts`, line 202, change `composeUserText(s.mode, prompt)` → `composeUserText(prompt)`.
In `server/engine/engine.ts`, line 496, change `composeUserText(s.mode, item.prompt)` → `composeUserText(item.prompt)`.

- [ ] **Step 9: Update the `Session.mode` comment in types**

In `server/engine/types.ts`, change line 26 from:

```ts
  mode: string | null;       // orchestration: null/"normal" | "workflows" | "ultra"
```

to:

```ts
  mode: string | null;       // orchestration: null = off | "ultracode" = official ultracode (--settings)
```

- [ ] **Step 10: Run the spawner test — expect pass**

Run: `cd ~/src/agentic-dev && yarn vitest run server/engine/spawner.test.ts`
Expected: PASS (all spawner tests, including the rewritten one).

- [ ] **Step 11: Run the full backend suite — expect green**

Run: `cd ~/src/agentic-dev && yarn test`
Expected: PASS (no other test referenced the deleted modes; `engine.streaming.test.ts` exercises the new `composeUserText` signature indirectly).

- [ ] **Step 12: Commit**

```bash
cd ~/src/agentic-dev
git add server/engine/spawner.ts server/engine/spawner.test.ts server/engine/engine.ts server/engine/types.ts
git commit -m "feat(engine): enable official ultracode via --settings; drop custom mode preamble"
```

---

### Task 2: Backend — normalize legacy `mode` values on read

**Files:**
- Modify: `server/engine/store.ts` (add `normalizeMode` helper; use it in `rowToSession:146`)
- Test: `server/engine/store.test.ts` (add a normalization test)

**Interfaces:**
- Produces: `rowToSession(...).mode` is `"ultracode"` for stored `"ultracode"`/`"ultra"`, else `null`.

- [ ] **Step 1: Write the failing test**

In `server/engine/store.test.ts`, inside the `describe("SqliteStore", …)` block, add:

```ts
  it("normalizes legacy orchestration modes on read", () => {
    store.create({ id: "u", repo: "r", prompt: "p", worktreePath: "/w", branch: "b", baseSha: null, mode: "ultra" });
    expect(store.get("u")?.mode).toBe("ultracode");        // legacy standing tier → official ultracode
    store.create({ id: "w", repo: "r", prompt: "p", worktreePath: "/w", branch: "b", baseSha: null, mode: "workflows" });
    expect(store.get("w")?.mode).toBeNull();               // legacy soft tier → off
    store.create({ id: "c", repo: "r", prompt: "p", worktreePath: "/w", branch: "b", baseSha: null, mode: "ultracode" });
    expect(store.get("c")?.mode).toBe("ultracode");        // current value passes through
  });
```

- [ ] **Step 2: Run it — expect failure**

Run: `cd ~/src/agentic-dev && yarn vitest run server/engine/store.test.ts`
Expected: FAIL — `store.get("u")?.mode` is `"ultra"`, not `"ultracode"`.

- [ ] **Step 3: Add the `normalizeMode` helper**

In `server/engine/store.ts`, just above the `rowToSession` function, add:

```ts
/** Legacy orchestration modes collapse onto the official on/off: the old standing "ultra" tier maps to
 *  "ultracode"; "workflows"/"normal"/anything else means off (null). */
function normalizeMode(m: string | null | undefined): string | null {
  return m === "ultracode" || m === "ultra" ? "ultracode" : null;
}
```

- [ ] **Step 4: Use it in `rowToSession`**

In `server/engine/store.ts:146`, change `mode: r.mode ?? null,` → `mode: normalizeMode(r.mode),`.

- [ ] **Step 5: Run the store test — expect pass**

Run: `cd ~/src/agentic-dev && yarn vitest run server/engine/store.test.ts`
Expected: PASS.

- [ ] **Step 6: Run the full backend suite — expect green**

Run: `cd ~/src/agentic-dev && yarn test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cd ~/src/agentic-dev
git add server/engine/store.ts server/engine/store.test.ts
git commit -m "feat(store): normalize legacy orchestration modes to official ultracode on/off"
```

---

### Task 3: Android — replace the 3-way mode toggle with a single Ultracode switch

**Files:**
- Modify: `app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt` (delete `MODES:67-71`; rename `ultra`→`ultracode`:112-116, 215, 218; replace the toggle Row :222-232; add imports)

**Interfaces:**
- Consumes: `NewRequestViewModel.setMode(String?)` (unchanged). On → `setMode("ultracode")`, off → `setMode(null)`.
- No ViewModel / Models / repository change. `NewRequestViewModelTest` is value-agnostic and stays green.

- [ ] **Step 1: Add the two new imports**

In `NewRequestScreen.kt`, add (keeping the existing alphabetical-ish grouping):

```kotlin
import androidx.compose.material3.Switch
import androidx.compose.ui.Alignment
```

- [ ] **Step 2: Delete the `MODES` constant**

Delete lines 67-71:

```kotlin
private val MODES = listOf(
    "normal" to "Normal",
    "workflows" to "Workflows",
    "ultra" to "Ultra-code",
)
```

- [ ] **Step 3: Rename the `ultra` flag to `ultracode` and its derived UI**

Replace lines 112-116:

```kotlin
    // Ultra-code mode locks effort to xhigh.
    val ultra = s.mode == "ultra"
    LaunchedEffect(ultra) {
        if (ultra) realVm.setEffort("xhigh")
    }
```

with:

```kotlin
    // Ultracode forces xhigh; reflect that in the (disabled) effort slider for honesty.
    val ultracode = s.mode == "ultracode"
    LaunchedEffect(ultracode) {
        if (ultracode) realVm.setEffort("xhigh")
    }
```

Then in the effort `SliderField` (lines 214-220), replace:

```kotlin
            SliderField(
                label = if (ultra) "Effort (locked by Ultra-code)" else "Effort",
                options = EFFORTS,
                value = s.effort ?: "",
                enabled = !ultra,
                onSelect = { realVm.setEffort(it.ifBlank { null }) },
            )
```

with:

```kotlin
            SliderField(
                label = if (ultracode) "Effort (locked by Ultracode)" else "Effort",
                options = EFFORTS,
                value = s.effort ?: "",
                enabled = !ultracode,
                onSelect = { realVm.setEffort(it.ifBlank { null }) },
            )
```

- [ ] **Step 4: Replace the orchestration toggle Row with a single switch**

Replace lines 222-232:

```kotlin
            // ── Orchestration mode ───────────────────────────────────────────────────
            Text("Orchestration mode", style = MaterialTheme.typography.titleSmall)
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                MODES.forEach { (k, lbl) ->
                    ToggleButton(
                        checked = (s.mode ?: "normal") == k,
                        onCheckedChange = { realVm.setMode(k) },
                        modifier = Modifier.weight(1f),
                    ) { Text(lbl) }
                }
            }
```

with:

```kotlin
            // ── Ultracode ────────────────────────────────────────────────────────────
            // Official Claude Code ultracode: xhigh effort + automatic dynamic-workflow orchestration.
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Ultracode", style = MaterialTheme.typography.titleSmall)
                Switch(
                    checked = ultracode,
                    onCheckedChange = { on -> realVm.setMode(if (on) "ultracode" else null) },
                )
            }
```

- [ ] **Step 5: Remove the now-unused `ToggleButton` import if unreferenced**

Grep for other uses: `grep -n "ToggleButton" app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt`. If the only hits were the deleted Row, remove `import androidx.compose.material3.ToggleButton` (line 30). If still used elsewhere, leave it.

- [ ] **Step 6: Verify in the worktree by reading (cannot build here)**

Run: `grep -n "ultracode\|Switch\|MODES\|ToggleButton" app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt`
Expected: `MODES` gone; `Switch` present; `ultracode` used; `ToggleButton` only if still referenced.

- [ ] **Step 7: Commit (worktree)**

```bash
cd ~/src/agentic-dev-android
git add app/src/main/java/dev/agentic/ui/newrequest/NewRequestScreen.kt
git commit -m "feat(ui): replace 3-way orchestration toggle with a single Ultracode switch"
```

---

### Task 4: Integration build, tests, and the real-claude verification gate

This task has no new code. It compiles/tests both repos where they actually build and performs the mandatory smoke test from the spec's risk section. **Do not consider the feature done until Step 4 passes (or the fallback in Step 5 is applied).**

- [ ] **Step 1: Push both repos to master**

```bash
cd ~/src/agentic-dev        && git push origin HEAD:master
cd ~/src/agentic-dev-android && git push origin HEAD:master
```
(If rejected: `git pull --rebase origin master && git push origin HEAD:master`.)

- [ ] **Step 2: Backend full suite in main checkout**

Run: `cd ~/src/agentic-dev && git pull --ff-only origin master && yarn test`
Expected: PASS.

- [ ] **Step 3: Android build + unit tests in the main checkout**

Run:
```bash
cd ~/src/agentic-dev-android && git pull --ff-only origin master
~/.local/share/gradle-8.10.2/bin/gradle test assembleRelease
```
Expected: tests PASS; `app/build/outputs/apk/release/app-release.apk` produced.

- [ ] **Step 4: Real-claude smoke test (the verification gate)**

Using the headless verify recipe in `agentic-dev/docs/internals.md`, start two real sessions:
- **Switch ON:** confirm from the stream that (a) effort runs at `xhigh`, and (b) Claude opts into a workflow on a substantive task (a `Workflow` tool_use appears) **without** the user typing the keyword.
- **Switch OFF:** confirm no automatic workflow and default effort.

Expected: ON triggers official ultracode behavior; OFF does not. If both hold, the `--settings '{"ultracode":true}'` path is confirmed → feature done.

- [ ] **Step 5: Fallback to Approach B — ONLY if Step 4 shows the flag is ignored/rejected**

If the deployed `claude` rejects or ignores `--settings '{"ultracode":true}'`:
1. Revert the `buildSpec` `--settings` push (Task 1 Step 6) — keep the `--effort` skip removed too (i.e., restore `if (opts.effort) extra.push("--effort", opts.effort);`).
2. Re-introduce a one-line preamble in `composeUserText` that injects the official keyword each turn. Change `composeUserText(text)` back to `composeUserText(mode, text)` and prepend `mode === "ultracode" ? "ultracode" : null` to the join (and restore the two engine call sites to pass `s.mode`).
3. Re-run the smoke test (Step 4); the keyword path is already proven to fire in headless sessions.
4. Commit: `git commit -m "fix(engine): fall back to official ultracode keyword injection"` and re-push.

- [ ] **Step 6: Deliver the APK**

```bash
mkdir -p ~/src/agentic-worktrees/<session>/outbox
cp ~/src/agentic-dev-android/app/build/outputs/apk/release/app-release.apk \
   ~/src/agentic-worktrees/<session>/outbox/"$(date +%Y%m%d-%H%M).apk"
```
Expected: timestamped APK appears in `outbox/` for the user to download.

---

## Self-Review

**Spec coverage:**
- Mechanism = official `--settings` → Task 1 Step 6. ✓
- Effort owned by ultracode (skip `--effort`, lock slider) → Task 1 Step 6 + Task 3 Step 3. ✓
- `OUTBOX_NOTE` preserved → Task 1 Steps 5/2 (assertion `toContain("Delivering files")`). ✓
- Reuse `mode` column, no migration → Tasks 1-2 touch no schema. ✓
- Legacy normalization (`ultra`→on, `workflows`→off) → Task 2. ✓
- UI: 3 toggles → single switch → Task 3. ✓
- Tests updated (spawner, store) → Tasks 1-2. Android VM tests are value-agnostic (use `"fast"`/`"auto"`), so they stay green; no Compose UI-test harness exists, so the switch is build-/manual-verified (Task 3 Step 6, Task 4 Step 3). ✓
- Risk/verification gate + Approach-B fallback → Task 4 Steps 4-5. ✓

**Placeholder scan:** No TBD/TODO; every code step shows the exact before/after. The only `<session>` token is the literal worktree path placeholder in Task 4 Step 6, which the executor substitutes.

**Type consistency:** `composeUserText(text: string)` is defined in Task 1 and used (no `mode` arg) at all three call sites in the same task. `normalizeMode(m): string | null` (Task 2) returns `"ultracode"`/`null`, matching `Session.mode`. UI `setMode("ultracode" | null)` matches the value space the backend reads.
