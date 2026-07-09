# Adopt & Re-sync Claude Code Sessions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let agentic-dev (a) **adopt** an existing Claude Code CLI session as a first-class agentic-dev session, and (b) cleanly **hand a session off to** a terminal `claude` and **re-sync** the terminal-added turns back when it returns — both powered by one shared native→agentic transcript importer.

**Architecture:** Every agentic-dev session already *is* a Claude Code session: the SDK bridge drives `claude` against the shared `~/.claude` and writes the native transcript to `~/.claude/projects/<cwd-slug>/<claudeSessionId>.jsonl` (call it **#2**), while the bridge separately mirrors a rendered stream to agentic-dev's own log `<data>/logs/<id>.jsonl` (call it **#1**) — the file the Android client renders. #2 is the complete record (every turn, from either tool); #1 holds only agentic-dev-driven turns. This feature adds a **native-transcript reader + translator** that converts #2 lines into the rendered line types #1 accepts, plus an `adopt` path that creates a session row pointing at an external `claudeSessionId`, and a `detach`/`reconcile` pair that closes the round-trip gap.

**Tech Stack:** Rust (axum + sqlx/SQLite engine, `server-rs/`), Node SDK bridge (`sdk-bridge.mjs`), Kotlin/Compose Android client (`agentic-dev-android/`).

## Global Constraints

- Engine (`server-rs/src/engine/`) stays free of `axum` imports — keep it unit-testable in isolation.
- `make test` (= `cd server-rs && cargo test`) must stay green before every commit; tests never hit real `claude` (they use `tests/fixtures/fake-sdk-bridge-*.sh`).
- New sqlite columns are added **only** via the idempotent `ADDED_COLUMNS` ALTER pattern in `store.rs` (never edit `COLUMNS_DDL` for the base table shape of existing installs) — existing rows must read a sensible default.
- All agentic-dev sessions share the real `~/.claude` config dir (`cfg.claude_config_base`); never introduce a per-session config copy.
- `cwd → slug` rule is fixed and must be reused verbatim: `cwd.replace(|c: char| !c.is_ascii_alphanumeric(), "-")` (see `engine/mod.rs:1745`). Transcript path = `claude_config_base.join("projects").join(slug).join(format!("{csid}.jsonl"))`.
- Resume only works when `resume_gate::transcript_is_resumable(path)` is true (any `"id":"msg_"` present, or a non-Anthropic model). Adopted sessions from a real Claude Code CLI run satisfy this; SDK-synthetic-only transcripts do not — surface that as "read-only" rather than failing a resume at spawn time.
- Keep the API stable for the Android app; server changes are additive (new columns serialized only when set; new routes).

---

## Design & Data Shapes (read before tasks)

### The two files

| | Path | Written by | Read by |
|---|---|---|---|
| **#1** agentic-dev log | `Store::log_path(id)` = `<data>/logs/<id>.jsonl` | bridge (`writeLog`) + engine (`Store::append_log`) | `EventTailer`, `RenderedProjection` (`engine/transcript.rs`), Android client |
| **#2** native transcript | `~/.claude/projects/<slug>/<csid>.jsonl` | `claude` itself (both agentic-dev turns and terminal turns) | `--resume`, `resume_gate`, this feature's importer |

### Rendered line types the client accepts (`engine/transcript.rs:17-31`)
`agentic_prompt`, `stream_event`, `assistant`, `result`, `agent_result`, `agentic_file`, `agentic_perm`, `agentic_perm_resolved`, `pr`, `workflowRun`. Raw `user` and `system/init` lines are **dropped** from the projection.

### #1 line shapes (verified)
- User prompt: `{"type":"agentic_prompt","text":"<prompt>","at":<i64 ms>}` — builder `prompt_event_json(text, at)` at `mod.rs:40`, written via `Store::append_log(id, &line).await` (async) / `append_log_blocking` (sync, `mod.rs:478`).
- Assistant / result / stream_event: raw SDK messages the bridge writes verbatim. An `assistant` line is `{"type":"assistant","message":{<anthropic message: id, role, model, content[], stop_reason, usage>}, ...}`; a `result` line is `{"type":"result","subtype":"success"|"error_during_execution","is_error":bool,"result":"<text>", ...}`.

### #2 line shapes (verified, ~10 live sessions on disk)
- `type:"user"`: `{ type, uuid, parentUuid, sessionId, timestamp, cwd, gitBranch, version, isMeta?, isSidechain?, isCompactSummary?, message:{ role:"user", content: <string> | <block[]> } }`. Blocks include `text`, `tool_result`, image.
- `type:"assistant"`: `{ type, uuid, parentUuid, sessionId, requestId, timestamp, message:{ id, role:"assistant", model, content:[ text | thinking | tool_use ], stop_reason, stop_details, usage } }`.
- **No** native `result`/`summary` line — end-of-turn is `message.stop_reason` on the assistant line.

### Translation map (#2 → #1)
| #2 line | Condition | #1 line emitted |
|---|---|---|
| `user` | `message` has real authored text (string, or a `text` block) AND not `isMeta`/`isSidechain`/`isCompactSummary` AND content is not solely `tool_result` blocks | `{"type":"agentic_prompt","text":<joined text>,"at":<ms(timestamp)>}` |
| `user` | content is solely `tool_result` blocks, or `isMeta`/sidechain/compact | *(skip — tool output, not user prose)* |
| `assistant` | always | `{"type":"assistant","message":<the native message object verbatim>}` |
| `assistant` | `message.stop_reason == "end_turn"` | additionally `{"type":"result","subtype":"success","is_error":false,"result":""}` (turn boundary marker) |

**Known v1 fidelity limit:** native `tool_result` blocks (which arrive as `user` lines) are not rendered on their own, so an *imported* history shows assistant text + tool calls but not tool *outputs*. Live turns after adoption render fully (bridge path unchanged). Document this in the PR.

### The watermark model (avoids double-import)
`#2` is a superset of `#1`. To reconcile only the delta without per-line dedup:
- **Adopt:** translate **all** of #2 into #1, then set `nativeWatermarkLines` = line-count(#2).
- **Detach:** set `detached = 1` and `nativeWatermarkLines` = line-count(#2) (agentic-dev is about to stop; #2 will only grow from the terminal after this point).
- **Reconcile (reopen):** translate #2 lines `[nativeWatermarkLines .. end)` into #1, set `nativeWatermarkLines` = new line-count(#2), clear `detached`. Because agentic-dev was stopped in the interval, that range is exactly the terminal-added turns.

`count_user_turns` reads #1 (`mod.rs:842`), so once the delta is appended the turn count self-corrects — no separate counter fix needed.

### New columns (all via `ADDED_COLUMNS`)
- `origin TEXT DEFAULT 'native'` — provenance: `'native'` | `'fork'` | `'adopted'`. Immutable source of truth (survives user regrouping).
- `detached INTEGER DEFAULT 0` — 1 while handed off to a terminal `claude` (single-writer guard / reconcile trigger).
- `nativeWatermarkLines INTEGER DEFAULT 0` — line-count of #2 already reflected in #1.

### Worktree strategy for adopt (v1 = adopt-in-place)
The native session ran in some cwd `X`. For `--resume` to find #2, agentic-dev's computed cwd must slugify to `slug(X)`. On a resume turn agentic-dev sets `cwd = worktree_path` (`mod.rs` resume branch), so **set `worktree_path = X`** and do **not** create a new git worktree. `branch`/`baseSha` are read from `X` if it is a git repo, else left empty; `repos = []`. Isolation is intentionally *not* added for adopt in v1 (the session already lives in a real directory). "Adopt into a fresh isolated worktree" (copy/symlink #2 into the new slug) is a documented future enhancement.

### File structure
- **Create** `server-rs/src/engine/native_transcript.rs` — read #2, discovery scan, translate #2→#1 lines. One responsibility: everything that understands the native format. No axum.
- **Modify** `server-rs/src/engine/store.rs` — 3 new columns + `Session` fields + `CreateInput` builders + `set_detached`/`set_watermark`/`ensure_group` helpers.
- **Modify** `server-rs/src/engine/mod.rs` — `adopt_session`, `detach_session`, `reconcile_from_native`; call reconcile on the reopen/follow-up path when `detached`.
- **Modify** `server-rs/src/engine/mod.rs` (module decl) — `mod native_transcript;`.
- **Modify** `server-rs/src/api/sessions.rs` + `server-rs/src/api/mod.rs` — `GET /api/adoptable`, `POST /api/sessions/adopt`, `POST /api/sessions/:id/detach` routes + handlers.
- **Modify** `agentic-dev-android/app/…` — render the default "Claude Code Adopted" group + `origin` badge; adopt picker; "Open in Claude Code" affordance.

> **Signatures to confirm at implement-time** (read the cited line before writing; do not guess): `Store::CreateInput` builder methods and `Store::create` (`store.rs:~223-266,~540-590`); `worktree.rs` git-branch/HEAD read helpers; the exact `status` string set (fork uses `"pending"` — reuse it); `submit_session` body for the create-sequence pattern (`mod.rs:586`); groups CRUD route names (`api/misc.rs:~87`). These are wiring details; the shapes and logic below are fixed.

---

## Task 1: Provenance & handoff columns

**Files:**
- Modify: `server-rs/src/engine/store.rs` (`ADDED_COLUMNS` array `store.rs:~414-419`; `Session` struct `store.rs:~40-80`; `CreateInput` `store.rs:~140-266`; INSERT `store.rs:577`)
- Test: `server-rs/src/engine/store.rs` (in-crate `#[cfg(test)]`)

**Interfaces:**
- Produces: `Session { origin: String, detached: bool, native_watermark_lines: i64 }`; `CreateInput::origin(v)`, `Store::set_detached(id, bool)`, `Store::set_watermark(id, i64)`, `Store::get(id) -> Option<Session>` (already exists — confirm name).

- [ ] **Step 1: Write the failing test** — new columns round-trip through create + read.

```rust
#[tokio::test]
async fn adopted_origin_and_watermark_round_trip() {
    let store = Store::open_in_memory().await.unwrap(); // confirm helper name at store.rs
    let id = "sess-adopt-1";
    store.create(CreateInput::new(id).origin("adopted").claude_session_id("ext-csid")).await.unwrap();
    let s = store.get(id).await.unwrap().unwrap();
    assert_eq!(s.origin, "adopted");
    assert_eq!(s.detached, false);
    assert_eq!(s.native_watermark_lines, 0);

    store.set_watermark(id, 42).await.unwrap();
    store.set_detached(id, true).await.unwrap();
    let s2 = store.get(id).await.unwrap().unwrap();
    assert_eq!(s2.native_watermark_lines, 42);
    assert_eq!(s2.detached, true);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test adopted_origin_and_watermark_round_trip`
Expected: FAIL — `CreateInput::origin` / `set_watermark` / `set_detached` not found, and `Session` has no `origin` field.

- [ ] **Step 3: Add the columns.** In the `ADDED_COLUMNS` array add three tuples:

```rust
("origin", "TEXT DEFAULT 'native'"),
("detached", "INTEGER DEFAULT 0"),
("nativeWatermarkLines", "INTEGER DEFAULT 0"),
```

Add to `Session` (match the serde-rename style already used):

```rust
#[serde(default = "default_origin")] pub origin: String,
#[serde(default)] pub detached: bool,
#[serde(rename = "nativeWatermarkLines", default)] pub native_watermark_lines: i64,
```

with `fn default_origin() -> String { "native".into() }`. Extend the `SELECT`/row-mapping used by `get`/`list` to read these three columns (coerce `INTEGER` → `bool` for `detached`). Add the `CreateInput.origin: Option<String>` field + builder:

```rust
pub fn origin(mut self, v: impl Into<String>) -> Self { self.origin = Some(v.into()); self }
```

Bind `origin` in the INSERT (`store.rs:577`, default `"native"` when unset); `detached`/`nativeWatermarkLines` take the DDL defaults at create. Add the two setters:

```rust
pub async fn set_detached(&self, id: &str, v: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET detached=?1 WHERE id=?2").bind(v as i64).bind(id).execute(&self.pool).await?;
    Ok(())
}
pub async fn set_watermark(&self, id: &str, n: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET nativeWatermarkLines=?1 WHERE id=?2").bind(n).bind(id).execute(&self.pool).await?;
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test adopted_origin_and_watermark_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/store.rs
git commit -m "feat(store): add origin/detached/nativeWatermarkLines columns"
```

---

## Task 2: Native transcript reader + discovery scan

**Files:**
- Create: `server-rs/src/engine/native_transcript.rs`
- Modify: `server-rs/src/engine/mod.rs` (add `mod native_transcript;` near the other `mod` decls)
- Test: `server-rs/src/engine/native_transcript.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub fn slug_for_cwd(cwd: &str) -> String` (the verbatim slug rule).
  - `pub fn transcript_path(config_base: &Path, cwd: &str, csid: &str) -> PathBuf`.
  - `pub struct Adoptable { pub session_id: String, pub cwd: String, pub slug: String, pub first_prompt: String, pub mtime_ms: i64, pub resumable: bool, pub line_count: i64 }`.
  - `pub fn scan_adoptable(config_base: &Path, known_csids: &HashSet<String>) -> Vec<Adoptable>` — walks `<config_base>/projects/*/*.jsonl`, skips any `session_id` already in `known_csids`, returns newest-first.

- [ ] **Step 1: Write the failing test** — slug rule + scan of a synthetic projects dir.

```rust
#[test]
fn slug_matches_engine_rule() {
    assert_eq!(slug_for_cwd("/home/me/proj"), "-home-me-proj");
    assert_eq!(slug_for_cwd("/a/b_c"), "-a-b-c");
}

#[test]
fn scan_lists_resumable_and_skips_known() {
    let tmp = tempfile::tempdir().unwrap();
    let projects = tmp.path().join("projects").join("-home-me-proj");
    std::fs::create_dir_all(&projects).unwrap();
    // resumable: has a msg_ id and a real user prompt
    std::fs::write(projects.join("csid-A.jsonl"),
        "{\"type\":\"user\",\"timestamp\":\"2026-07-09T00:00:00Z\",\"cwd\":\"/home/me/proj\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n\
         {\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"stop_reason\":\"end_turn\"}}\n").unwrap();
    // already adopted -> must be skipped
    std::fs::write(projects.join("csid-known.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n").unwrap();

    let known: std::collections::HashSet<String> = ["csid-known".to_string()].into_iter().collect();
    let got = scan_adoptable(tmp.path(), &known);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].session_id, "csid-A");
    assert_eq!(got[0].cwd, "/home/me/proj");
    assert_eq!(got[0].first_prompt, "hello");
    assert!(got[0].resumable);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test native_transcript`
Expected: FAIL — module/functions do not exist.

- [ ] **Step 3: Implement the reader.** Create `native_transcript.rs`:

```rust
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn slug_for_cwd(cwd: &str) -> String {
    cwd.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
}

pub fn transcript_path(config_base: &Path, cwd: &str, csid: &str) -> PathBuf {
    config_base.join("projects").join(slug_for_cwd(cwd)).join(format!("{csid}.jsonl"))
}

#[derive(serde::Serialize, Debug)]
pub struct Adoptable {
    #[serde(rename = "sessionId")] pub session_id: String,
    pub cwd: String,
    pub slug: String,
    #[serde(rename = "firstPrompt")] pub first_prompt: String,
    #[serde(rename = "mtimeMs")] pub mtime_ms: i64,
    pub resumable: bool,
    #[serde(rename = "lineCount")] pub line_count: i64,
}

fn iso_to_ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.timestamp_millis()).unwrap_or(0)
}

/// Extract plain user text from a native `user` line's message.content.
/// Returns None for tool-result-only / meta / sidechain / non-user lines.
pub fn user_prompt_text(line: &serde_json::Value) -> Option<String> {
    if line.get("type").and_then(|v| v.as_str()) != Some("user") { return None; }
    for flag in ["isMeta", "isSidechain", "isCompactSummary"] {
        if line.get(flag).and_then(|v| v.as_bool()) == Some(true) { return None; }
    }
    let content = line.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return if s.trim().is_empty() { None } else { Some(s.to_string()) };
    }
    let arr = content.as_array()?;
    let text: String = arr.iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() { None } else { Some(text) }
}

fn read_lines(path: &Path) -> Vec<serde_json::Value> {
    let Ok(raw) = std::fs::read_to_string(path) else { return vec![]; };
    raw.lines().filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

pub fn scan_adoptable(config_base: &Path, known_csids: &HashSet<String>) -> Vec<Adoptable> {
    let root = config_base.join("projects");
    let mut out = vec![];
    let Ok(projects) = std::fs::read_dir(&root) else { return out; };
    for proj in projects.flatten() {
        let Ok(files) = std::fs::read_dir(proj.path()) else { continue; };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            let Some(csid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue; };
            if known_csids.contains(&csid) { continue; }
            let lines = read_lines(&p);
            if lines.is_empty() { continue; }
            let cwd = lines.iter().find_map(|l| l.get("cwd").and_then(|v| v.as_str())).unwrap_or("").to_string();
            let first_prompt = lines.iter().find_map(|l| user_prompt_text(l)).unwrap_or_default();
            let resumable = super::resume_gate::transcript_is_resumable(&p);
            let mtime_ms = f.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            out.push(Adoptable {
                session_id: csid, slug: p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string(),
                cwd, first_prompt, mtime_ms, resumable, line_count: lines.len() as i64,
            });
        }
    }
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    out
}
```

(Confirm `chrono` and `tempfile` are already deps — both are used elsewhere in the crate. `user_prompt_text` takes `&Value`; adjust the `line.pointer` call to `&mut`-free access as shown.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test native_transcript`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/native_transcript.rs server-rs/src/engine/mod.rs
git commit -m "feat(engine): native transcript reader + adoptable scan"
```

---

## Task 3: Translator #2 → #1 rendered lines

**Files:**
- Modify: `server-rs/src/engine/native_transcript.rs`
- Test: `server-rs/src/engine/native_transcript.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `user_prompt_text` (Task 2), `read_lines` (make `pub(crate)`).
- Produces: `pub fn translate_lines(native: &[serde_json::Value]) -> Vec<String>` — returns ready-to-append #1 JSONL strings (no trailing newline); `pub fn translate_range(path: &Path, from_line: usize) -> (Vec<String>, usize)` — returns (`#1 lines`, `new_total_line_count`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn translate_maps_user_and_assistant() {
    let native: Vec<serde_json::Value> = [
        r#"{"type":"user","timestamp":"2026-07-09T00:00:00Z","message":{"role":"user","content":"do a thing"}}"#,
        r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","model":"claude-x","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
        r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta noise"}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#,
    ].iter().map(|s| serde_json::from_str(s).unwrap()).collect();

    let out = translate_lines(&native);
    let types: Vec<String> = out.iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["type"].as_str().unwrap().to_string())
        .collect();
    // user prompt -> agentic_prompt ; assistant -> assistant + result(end_turn) ; meta + tool_result-only user -> skipped
    assert_eq!(types, vec!["agentic_prompt", "assistant", "result"]);

    let first: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
    assert_eq!(first["text"], "do a thing");
    assert_eq!(first["at"], 1783641600000i64); // ms of 2026-07-09T00:00:00Z
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test translate_maps_user_and_assistant`
Expected: FAIL — `translate_lines` not found.

- [ ] **Step 3: Implement the translator**

```rust
pub fn translate_lines(native: &[serde_json::Value]) -> Vec<String> {
    let mut out = vec![];
    for line in native {
        match line.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                if let Some(text) = user_prompt_text(line) {
                    let at = line.get("timestamp").and_then(|v| v.as_str()).map(iso_to_ms).unwrap_or(0);
                    out.push(serde_json::json!({"type":"agentic_prompt","text":text,"at":at}).to_string());
                }
            }
            Some("assistant") => {
                if let Some(msg) = line.get("message") {
                    out.push(serde_json::json!({"type":"assistant","message":msg}).to_string());
                    if msg.get("stop_reason").and_then(|v| v.as_str()) == Some("end_turn") {
                        out.push(serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":""}).to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn translate_range(path: &Path, from_line: usize) -> (Vec<String>, usize) {
    let all = read_lines(path);
    let total = all.len();
    let slice = if from_line >= total { &[][..] } else { &all[from_line..] };
    (translate_lines(slice), total)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test translate`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/native_transcript.rs
git commit -m "feat(engine): translate native transcript lines to rendered log lines"
```

---

## Task 4: `reconcile_from_native` engine method (shared core)

**Files:**
- Modify: `server-rs/src/engine/mod.rs`
- Test: `server-rs/src/engine/tests.rs` (or in-crate near other engine tests)

**Interfaces:**
- Consumes: `native_transcript::{transcript_path, translate_range}`, `Store::{append_log, set_watermark, get}`, `cfg.claude_config_base`.
- Produces: `pub async fn reconcile_from_native(&self, id: &str) -> Result<usize, String>` — imports #2[`watermark`..] into #1, updates the watermark, returns count of #1 lines appended. Idempotent: a second call with no new native lines appends 0.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn reconcile_imports_delta_and_is_idempotent() {
    let h = TestEngine::new().await; // existing test harness — confirm constructor name in tests.rs
    let id = "sess-recon";
    // create an adopted-style row with a cwd whose transcript we control
    let cwd = h.tmp.path().join("proj"); std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    h.store().create(CreateInput::new(id).origin("adopted").claude_session_id("csidR").worktree_path(&cwd_s)).await.unwrap();
    // write a native transcript at the slug path
    let slug_dir = h.cfg_config_base().join("projects").join(native_transcript::slug_for_cwd(&cwd_s));
    std::fs::create_dir_all(&slug_dir).unwrap();
    let tp = slug_dir.join("csidR.jsonl");
    std::fs::write(&tp, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();

    let n = h.engine.reconcile_from_native(id).await.unwrap();
    assert_eq!(n, 1); // one agentic_prompt appended
    assert_eq!(h.engine.reconcile_from_native(id).await.unwrap(), 0); // idempotent

    // append a terminal turn, reconcile pulls exactly that
    std::fs::OpenOptions::new().append(true).open(&tp).unwrap()
        .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_9\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"yo\"}],\"stop_reason\":\"end_turn\"}}\n").unwrap();
    assert_eq!(h.engine.reconcile_from_native(id).await.unwrap(), 2); // assistant + result
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test reconcile_imports_delta_and_is_idempotent`
Expected: FAIL — `reconcile_from_native` not found.

- [ ] **Step 3: Implement**

```rust
pub async fn reconcile_from_native(&self, id: &str) -> Result<usize, String> {
    let s = self.0.store.get(id).await.map_err(|e| e.to_string())?.ok_or("no such session")?;
    let csid = s.claude_session_id.clone().ok_or("session has no claudeSessionId")?;
    let cwd = s.worktree_path.clone();
    let path = crate::engine::native_transcript::transcript_path(&self.0.cfg.claude_config_base, &cwd, &csid);
    if !path.is_file() { return Ok(0); }
    let from = s.native_watermark_lines.max(0) as usize;
    let (lines, total) = crate::engine::native_transcript::translate_range(&path, from);
    for l in &lines {
        self.0.store.append_log(id, l).await.map_err(|e| e.to_string())?;
    }
    self.0.store.set_watermark(id, total as i64).await.map_err(|e| e.to_string())?;
    Ok(lines.len())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test reconcile_imports_delta_and_is_idempotent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/mod.rs server-rs/src/engine/tests.rs
git commit -m "feat(engine): reconcile_from_native — watermark-based native->log import"
```

---

## Task 5: `adopt_session` engine method

**Files:**
- Modify: `server-rs/src/engine/mod.rs`
- Test: `server-rs/src/engine/tests.rs`

**Interfaces:**
- Consumes: `new_session_id()` (`mod.rs:290`), `Store::create`, `reconcile_from_native` (Task 4), `native_transcript::{transcript_path, read_lines}`, `Store::set_watermark`, `Store::ensure_group` (Task 6 — but see note), worktree git-read helpers.
- Produces: `pub async fn adopt_session(&self, csid: &str, cwd: &str) -> Result<String, String>` — returns the new agentic-dev session id.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn adopt_creates_row_and_imports_full_history() {
    let h = TestEngine::new().await;
    let cwd = h.tmp.path().join("proj"); std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let slug_dir = h.cfg_config_base().join("projects").join(native_transcript::slug_for_cwd(&cwd_s));
    std::fs::create_dir_all(&slug_dir).unwrap();
    std::fs::write(slug_dir.join("csidX.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n\
         {\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"stop_reason\":\"end_turn\"}}\n").unwrap();

    let id = h.engine.adopt_session("csidX", &cwd_s).await.unwrap();
    let s = h.store().get(&id).await.unwrap().unwrap();
    assert_eq!(s.origin, "adopted");
    assert_eq!(s.claude_session_id.as_deref(), Some("csidX"));
    assert_eq!(s.worktree_path, cwd_s);
    assert_eq!(s.status, "pending");
    assert_eq!(s.prompt, "first prompt"); // seeded title/prompt
    assert_eq!(s.native_watermark_lines, 2);
    // #1 now renders the imported history
    let log = h.store().read_log(&id);
    assert!(log.iter().any(|l| l.contains("\"agentic_prompt\"")));
    assert!(log.iter().any(|l| l.contains("\"assistant\"")));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test adopt_creates_row_and_imports_full_history`
Expected: FAIL — `adopt_session` not found.

- [ ] **Step 3: Implement** (uses `Store::ensure_group` from Task 6; if implementing Task 5 first, temporarily pass `group_id=None` and add the group wiring in Task 6’s step):

```rust
pub async fn adopt_session(&self, csid: &str, cwd: &str) -> Result<String, String> {
    // guard: don't double-adopt
    if self.0.store.session_by_csid(csid).await.map_err(|e| e.to_string())?.is_some() {
        return Err(format!("session {csid} already adopted"));
    }
    let path = crate::engine::native_transcript::transcript_path(&self.0.cfg.claude_config_base, cwd, csid);
    if !path.is_file() { return Err(format!("no transcript at {}", path.display())); }
    let native = crate::engine::native_transcript::read_lines(&path);
    let first_prompt = native.iter().find_map(crate::engine::native_transcript::user_prompt_text).unwrap_or_default();

    let id = Self::new_session_id();
    let group_id = self.0.store.ensure_group("Claude Code Adopted").await.map_err(|e| e.to_string())?;
    let git = self.read_git_head(cwd); // (branch, base_sha) helper reading `cwd` if it's a git repo; ("", "") otherwise
    self.0.store.create(
        CreateInput::new(&id)
            .origin("adopted")
            .claude_session_id(csid)
            .worktree_path(cwd)
            .prompt(&first_prompt)
            .status("pending")
            .branch(&git.0)
            .base_sha(&git.1)
            .group_id(&group_id)
    ).await.map_err(|e| e.to_string())?;

    // full history import + watermark
    self.reconcile_from_native(&id).await?;
    Ok(id)
}
```

Add a small `read_git_head(&self, cwd: &str) -> (String, String)` helper (reuse whatever `worktree.rs` uses to run `git -C <cwd> rev-parse HEAD` / `--abbrev-ref HEAD`; return empty strings on non-git). Add `Store::session_by_csid(csid) -> Option<Session>` (`SELECT … WHERE claudeSessionId=?1`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test adopt_creates_row_and_imports_full_history`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/mod.rs server-rs/src/engine/store.rs server-rs/src/engine/tests.rs
git commit -m "feat(engine): adopt_session — create row from external Claude session + import history"
```

---

## Task 6: Default group helper + wire adopt to it

**Files:**
- Modify: `server-rs/src/engine/store.rs`
- Test: `server-rs/src/engine/store.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `pub async fn ensure_group(&self, name: &str) -> anyhow::Result<String>` — returns the id of the group with that `name`, creating it (stable id `grp-adopted` for "Claude Code Adopted") if absent. Idempotent.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn ensure_group_is_idempotent() {
    let store = Store::open_in_memory().await.unwrap();
    let a = store.ensure_group("Claude Code Adopted").await.unwrap();
    let b = store.ensure_group("Claude Code Adopted").await.unwrap();
    assert_eq!(a, b);
    assert_eq!(store.list_groups().await.unwrap().iter().filter(|g| g.name == "Claude Code Adopted").count(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test ensure_group_is_idempotent`
Expected: FAIL — `ensure_group` not found.

- [ ] **Step 3: Implement** (confirm the `groups` DDL columns `id,name,icon,sortOrder,createdAt` and `list_groups` at `store.rs`):

```rust
pub async fn ensure_group(&self, name: &str) -> anyhow::Result<String> {
    if let Some(g) = sqlx::query_scalar::<_, String>("SELECT id FROM groups WHERE name=?1")
        .bind(name).fetch_optional(&self.pool).await? { return Ok(g); }
    let id = if name == "Claude Code Adopted" { "grp-adopted".to_string() } else { format!("grp-{}", uuid_v4()) };
    sqlx::query("INSERT OR IGNORE INTO groups (id,name,icon,sortOrder,createdAt) VALUES (?1,?2,?3,?4,?5)")
        .bind(&id).bind(name).bind(Option::<String>::None).bind(1000i64).bind(now_ms()).execute(&self.pool).await?;
    // resolve winner in case of a concurrent insert
    Ok(sqlx::query_scalar::<_, String>("SELECT id FROM groups WHERE name=?1").bind(name).fetch_one(&self.pool).await?)
}
```

(Use the crate's existing uuid + millis helpers; confirm names.) If Task 5 was landed with `group_id=None`, now switch its `create(...)` to `.group_id(&self.0.store.ensure_group("Claude Code Adopted").await?)` and re-run Task 5’s test.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test ensure_group_is_idempotent && cargo test adopt_creates_row`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/store.rs server-rs/src/engine/mod.rs
git commit -m "feat(store): ensure_group + default 'Claude Code Adopted' group for adopted sessions"
```

---

## Task 7: `detach_session` + reconcile on reopen

**Files:**
- Modify: `server-rs/src/engine/mod.rs`
- Test: `server-rs/src/engine/tests.rs`

**Interfaces:**
- Consumes: existing hard-stop (`stop`/SIGTERM) path, `Store::{set_detached, set_watermark}`, `native_transcript::transcript_path`, `reconcile_from_native` (Task 4).
- Produces:
  - `pub async fn detach_session(&self, id: &str) -> Result<DetachInfo, String>` where `DetachInfo { cwd: String, claude_session_id: String, resume_cmd: String }`.
  - Reconcile hook: in `follow_up` (and/or on the read/open path), if `session.detached` then call `reconcile_from_native(id)` and `set_detached(id, false)` **before** the resume turn is prepared.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn detach_sets_flag_watermark_and_resume_cmd() {
    let h = TestEngine::new().await;
    let cwd = h.tmp.path().join("proj"); std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().to_string();
    let slug_dir = h.cfg_config_base().join("projects").join(native_transcript::slug_for_cwd(&cwd_s));
    std::fs::create_dir_all(&slug_dir).unwrap();
    std::fs::write(slug_dir.join("csidD.jsonl"), "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();
    let id = h.engine.adopt_session("csidD", &cwd_s).await.unwrap();

    let info = h.engine.detach_session(&id).await.unwrap();
    assert_eq!(info.claude_session_id, "csidD");
    assert!(info.resume_cmd.contains("claude --resume csidD"));
    let s = h.store().get(&id).await.unwrap().unwrap();
    assert!(s.detached);
    assert_eq!(s.native_watermark_lines, 1); // frozen at detach

    // terminal adds a turn while detached
    std::fs::OpenOptions::new().append(true).open(slug_dir.join("csidD.jsonl")).unwrap()
        .write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"back\"}],\"stop_reason\":\"end_turn\"}}\n").unwrap();

    let n = h.engine.reconcile_from_native(&id).await.unwrap();
    assert_eq!(n, 2); // assistant + result pulled in
    h.store().set_detached(&id, false).await.unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test detach_sets_flag_watermark_and_resume_cmd`
Expected: FAIL — `detach_session`/`DetachInfo` not found.

- [ ] **Step 3: Implement**

```rust
pub struct DetachInfo { pub cwd: String, pub claude_session_id: String, pub resume_cmd: String }

pub async fn detach_session(&self, id: &str) -> Result<String_or_DetachInfo, String> {
    let s = self.0.store.get(id).await.map_err(|e| e.to_string())?.ok_or("no such session")?;
    let csid = s.claude_session_id.clone().ok_or("session has no claudeSessionId")?;
    // hard-stop the streaming process so the terminal is the single writer
    let _ = self.stop(id).await; // reuse existing SIGTERM stop; ignore "already idle"
    // freeze the watermark at the current native line count
    let path = crate::engine::native_transcript::transcript_path(&self.0.cfg.claude_config_base, &s.worktree_path, &csid);
    let total = crate::engine::native_transcript::read_lines(&path).len() as i64;
    self.0.store.set_watermark(id, total).await.map_err(|e| e.to_string())?;
    self.0.store.set_detached(id, true).await.map_err(|e| e.to_string())?;
    let resume_cmd = format!("cd {} && claude --resume {}", s.worktree_path, csid);
    Ok(DetachInfo { cwd: s.worktree_path, claude_session_id: csid, resume_cmd })
}
```

Then in `follow_up` (the resume/idle path, near `mod.rs:942`/`478`), **before** building the turn, add:

```rust
if self.0.store.get(id).await.ok().flatten().map(|s| s.detached).unwrap_or(false) {
    let _ = self.reconcile_from_native(id).await;      // pull terminal turns into #1
    let _ = self.0.store.set_detached(id, false).await; // agentic-dev reclaims ownership
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test detach_sets_flag_watermark_and_resume_cmd`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/engine/mod.rs server-rs/src/engine/tests.rs
git commit -m "feat(engine): detach_session + auto-reconcile on reclaim"
```

---

## Task 8: HTTP surface — adoptable / adopt / detach

**Files:**
- Modify: `server-rs/src/api/sessions.rs`
- Modify: `server-rs/src/api/mod.rs` (route registration near `mod.rs:45`)
- Test: `server-rs/src/api/sessions.rs` or an api integration test module

**Interfaces:**
- Consumes: `engine.{adopt_session, detach_session, reconcile_from_native}`, `native_transcript::scan_adoptable`, `store.list` (for `known_csids`).
- Produces:
  - `GET /api/adoptable` → `200 [{ sessionId, cwd, slug, firstPrompt, mtimeMs, resumable, lineCount }]` (newest first; excludes already-adopted csids).
  - `POST /api/sessions/adopt` body `{ "claudeSessionId": "...", "cwd": "..." }` → `201 { "id": "<new>" }`.
  - `POST /api/sessions/:id/detach` → `200 { cwd, claudeSessionId, resumeCmd }`.

- [ ] **Step 1: Write the failing test** (api handler smoke — mirror an existing `api/sessions.rs` test that builds an app + calls a route; confirm helper names in `api/test_support.rs`):

```rust
#[tokio::test]
async fn adopt_route_creates_session() {
    let app = test_app().await; // existing helper
    // seed a native transcript under the test config base + a cwd
    seed_native_transcript(&app, "/tmp/proj", "csidHTTP", &[
        r#"{"type":"user","message":{"role":"user","content":"hey"}}"#,
    ]);
    let resp = app.post_json("/api/sessions/adopt", serde_json::json!({"claudeSessionId":"csidHTTP","cwd":"/tmp/proj"})).await;
    assert_eq!(resp.status(), 201);
    let id = resp.json()["id"].as_str().unwrap().to_string();
    let list = app.get_json("/api/sessions").await;
    assert!(list.as_array().unwrap().iter().any(|s| s["id"] == id && s["origin"] == "adopted"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd server-rs && cargo test adopt_route_creates_session`
Expected: FAIL — route/handlers missing (404 / compile error).

- [ ] **Step 3: Implement handlers** (mirror `create_session` at `api/sessions.rs:227`):

```rust
#[derive(serde::Deserialize)]
pub struct AdoptBody { #[serde(rename = "claudeSessionId")] pub claude_session_id: String, pub cwd: String }

pub async fn list_adoptable(State(st): State<AppState>) -> Response {
    let known = st.engine.known_claude_session_ids().await; // HashSet<String> from store.list
    let items = crate::engine::native_transcript::scan_adoptable(&st.engine.config_base(), &known);
    Json(items).into_response()
}

pub async fn adopt_session(State(st): State<AppState>, body: Bytes) -> Response {
    let b: AdoptBody = match serde_json::from_slice(&body) { Ok(b) => b, Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response() };
    match st.engine.adopt_session(&b.claude_session_id, &b.cwd).await {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({"id": id}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn detach_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.engine.detach_session(&id).await {
        Ok(info) => Json(serde_json::json!({"cwd": info.cwd, "claudeSessionId": info.claude_session_id, "resumeCmd": info.resume_cmd})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}
```

Register routes in `api/mod.rs`:

```rust
.route("/api/adoptable", get(sessions::list_adoptable))
.route("/api/sessions/adopt", post(sessions::adopt_session))
.route("/api/sessions/:id/detach", post(sessions::detach_session))
```

Add the two small engine accessors used above (`known_claude_session_ids()`, `config_base()`) if not already present — thin wrappers over `store.list` / `cfg.claude_config_base.clone()`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd server-rs && cargo test adopt_route_creates_session && make test`
Expected: PASS (and full suite green).

- [ ] **Step 5: Commit**

```bash
git add server-rs/src/api/sessions.rs server-rs/src/api/mod.rs server-rs/src/engine/mod.rs
git commit -m "feat(api): /api/adoptable, /api/sessions/adopt, /api/sessions/:id/detach"
```

---

## Task 9: Server compile + full-suite gate, restart, manual smoke

**Files:** none (verification task)

- [ ] **Step 1: Build release + run full suite**

Run: `cd server-rs && cargo build --release && make test`
Expected: builds clean; all tests pass.

- [ ] **Step 2: Restart the service** (streaming turns terminate on restart by design):

Run: `systemctl --user restart agentic-dev && systemctl --user is-active agentic-dev`
Expected: `active`.

- [ ] **Step 3: Manual smoke via API** (login → token → adopt round-trip; do not print secrets):

```bash
TOKEN=$(curl -s localhost:7420/api/login -d "{\"password\":\"$(cat ~/.agentic-dev/login-password)\"}" | jq -r .token)
curl -s localhost:7420/api/adoptable -H "Authorization: Bearer $TOKEN" | jq '.[0]'
# pick a sessionId+cwd from that list, then:
curl -s localhost:7420/api/sessions/adopt -H "Authorization: Bearer $TOKEN" \
  -d '{"claudeSessionId":"<id>","cwd":"<cwd>"}' | jq .
```
Expected: `/api/adoptable` lists real local Claude sessions; adopt returns `201 {id}`; `GET /api/sessions` shows it with `origin:"adopted"` in the `Claude Code Adopted` group; opening it and sending a follow-up resumes the real session.

- [ ] **Step 4: Commit** (if any doc/notes updates were needed)

```bash
git add -A && git commit -m "docs: adopt/detach ops notes" || echo "nothing to commit"
```

---

## Task 10: Android client — group, badge, adopt picker, hand-off (agentic-dev-android)

**Files:** `agentic-dev-android/app/…` (session list + models — read current code; the app changed recently, e.g. `ca324af`). This task ships **after** the server tasks and is independently testable.

**Contract from the server (already delivered by Tasks 1–8):**
- `Session` JSON gains `origin` (`"native"|"fork"|"adopted"`) and `detached` (bool); adopted sessions carry `groupId = "grp-adopted"` (group name "Claude Code Adopted").
- `GET /api/adoptable` → array of `{ sessionId, cwd, slug, firstPrompt, mtimeMs, resumable, lineCount }`.
- `POST /api/sessions/adopt {claudeSessionId, cwd}` → `{id}`; `POST /api/sessions/:id/detach` → `{cwd, claudeSessionId, resumeCmd}`.

- [ ] **Step 1: Model** — add `origin: String = "native"` and `detached: Boolean = false` to the session data class; add an `Adoptable` data class + `AdoptableRepository` calls for the three endpoints (mirror an existing repository method).
- [ ] **Step 2: List rendering** — the home screen already groups by `groupId`; confirm the "Claude Code Adopted" group renders from the server-provided group (no client-side special-casing needed). Add a small "adopted" badge/chip on rows where `origin == "adopted"`.
- [ ] **Step 3: Adopt picker** — a screen/sheet that calls `GET /api/adoptable`, lists candidates (firstPrompt + cwd + relative mtime; disable/grey `resumable == false` with a "read-only" note), and on tap calls `POST /api/sessions/adopt`, then navigates to the new session.
- [ ] **Step 4: Hand-off affordance** — in session detail, an "Open in Claude Code" action that calls `POST /api/sessions/:id/detach` and shows the returned `resumeCmd` (copyable). Reflect `detached == true` with a banner ("handed off to a terminal — reopen to re-sync").
- [ ] **Step 5: Test + build** — update/extend the relevant `HomeViewModelTest`/repository tests for the new fields and endpoints; then `./gradlew assembleRelease` (or the project's build task). Expected: builds; tests green.
- [ ] **Step 6: Commit** on the android session branch, PR per that repo's flow.

---

## Self-Review

**Spec coverage:**
- A1 re-sync (round-trip): Tasks 4 (importer) + 7 (detach freezes watermark, reconcile on reopen). ✓
- B1 adopt: Tasks 2 (discovery) + 5 (adopt) + 8 (HTTP). ✓
- Shared importer: Task 3/4 used by both adopt (full import) and reconcile (delta). ✓
- `origin` column: Task 1. ✓
- Default "Claude Code Adopted" group: Task 6, wired in Task 5. ✓
- Single-writer / owner state: `detached` flag (Task 1) + hard-stop on detach + reclaim-on-follow-up (Task 7). ✓
- Client surface: Task 10. ✓

**Placeholder scan:** No `TBD`/`add error handling`-style steps; every code step carries real code. The few "confirm at implement-time" notes point at exact files for *wiring signatures* (builder method names, test-harness constructor), not missing logic — the logic and data shapes are fully specified. Acceptable per the design section's explicit signature list.

**Type consistency:** `reconcile_from_native(id) -> Result<usize,String>`, `adopt_session(csid, cwd) -> Result<String,String>`, `detach_session(id) -> DetachInfo`, `translate_range(path, from) -> (Vec<String>, usize)`, `scan_adoptable(base, known) -> Vec<Adoptable>` are used consistently across tasks. Watermark is **lines** everywhere. `origin` values `native|fork|adopted` consistent. Slug rule identical to `mod.rs:1745`.

**Known limitations (documented in PR):** imported history does not render tool *outputs* (native `tool_result` `user` lines are dropped by the projection); adopt is in-place (no isolation) in v1; reconcile assumes the terminal session ended before reopen (single-writer is advised, not enforced).
