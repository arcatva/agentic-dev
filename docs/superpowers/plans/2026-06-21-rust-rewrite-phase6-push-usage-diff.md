# Rust Backend Rewrite — Phase 6: push (FCM), usage cache, diff/upload/file, misc endpoints

**Date:** 2026-06-21
**Crate:** `agentic-dev/server-rs`
**Strategy spec:** `docs/superpowers/specs/2026-06-20-rust-backend-rewrite-design.md`
**Parity bar:** Behavioral parity with the TS reference (`server/engine/*.ts`, `server/api/routes.ts`). The Android client must not be able to tell which backend it is talking to.

Phases 0–5 are done (scaffold, store+transcript, stream parser, spawner/runner, engine, API+WS core routes). This phase fills the **remaining REST surface** the Android client uses and the **FCM push hook** the engine fires on turn exit. It is the last functional phase before the Phase 7 conformance/cutover work.

## Scope (this phase)

Mirror these TS sources, behavior-for-behavior:

- `server/engine/push.ts` — device-token JSON store + FCM HTTP v1 body + `sendPush` no-op rules; engine fires it on session exit.
- `server/engine/usage.ts` — Anthropic OAuth plan-usage fetch; plus the **60s fresh / 10min stale / single-flight** cache in `routes.ts`.
- `server/engine/structuredDiff.ts` — `commitGraphForRepo` + `commitFilesForRepo` (commit-graph view + per-commit changed files), async git off the executor.
- `server/engine/workflows.ts` — `listWorkflows` + `readWorkflowAgent` (replaces the Phase 5 status-only stub).
- `server/engine/repos.ts` (`listRemoteRepos`), `server/engine/skills.ts` (`listSkills`), `server/engine/groups.ts`, `server/engine/templates.ts`, `server/engine/atomicWrite.ts`.
- `server/api/routes.ts` — the routes still missing in Rust: `/api/repos`, `/api/skills`, `/api/usage`, `/api/groups` (GET/PUT), `/api/devices` (POST), `/api/templates` (GET/PUT), `/api/templates/start`, `/api/sessions/:id/file`, `/api/sessions/:id/upload`, `/api/sessions/:id/outbox`, `/api/sessions/:id/commits`, `/api/sessions/:id/commits/:sha/files`, `/api/sessions/:id/workflows` (real payload), `/api/sessions/:id/workflows/:runId/agents/:agentId`.

**Already done — do NOT re-implement:** `is_rendered`/`filter_rendered`/`raw_to_rendered_offset` live in `src/transcript.rs`. `ensure_local`/`default_clone` (local repo resolution) live in `src/repos.rs`. `list_skills` does NOT exist yet (only `ensure_local`); `list_repos` (local) is in the engine path already but the **`/api/repos` route** is not wired — re-use whatever local lister exists, add the route. Session create/get/message/lifecycle/stream routes are done in `src/api/sessions.rs` + `src/api/stream.rs`.

---

## Global Constraints (exact parity values — copy verbatim)

### FCM push (`push.ts`)
- Env vars: `AGENTIC_FCM_PROJECT_ID`, `AGENTIC_FCM_SERVER_KEY`. Both must be present or creds = null (no-op).
- Device-token store JSON shape: `{ "token": <string>, "registeredAt": <epoch ms> }`. Field names exactly `token`, `registeredAt`.
- `loadDeviceToken`: missing file → null; parse error → null; record with empty/non-string `token` → null.
- `saveDeviceToken`: overwrites (last wins); `mkdir -p` the parent; written atomically.
- `PushPayload` fields: `sessionId`, `status`, `isError` (bool), `errorText?`, `costUsd` (number | null).
- FCM body shape (exact):
  ```json
  {"message":{"token":"<deviceToken>",
    "notification":{"title":"Session <status>",
      "body":"<Error: <errorText|unknown error>>  OR  <Completed[ ($<costUsd.toFixed(4)>)]>"},
    "data":{"sessionId":"<id>","status":"<status>","isError":"<\"true\"|\"false\">","costUsd":"<costUsd ?? \"\">"}}}
  ```
  - Success body: `"Completed"` + (costUsd != null ? ` ($${costUsd.toFixed(4)})` : ""). **4 decimal places.**
  - Error body: `"Error: " + (errorText ?? "unknown error")`.
  - `data.isError` is the **string** `"true"`/`"false"`. `data.costUsd` is `String(costUsd ?? "")` (null → empty string).
- FCM send URL: `https://fcm.googleapis.com/v1/projects/<projectId>/messages:send`, `POST`, headers `Content-Type: application/json`, `Authorization: Bearer <serverKey>`.
- `sendPush` no-ops cleanly when deviceToken is null/empty OR creds is null. **A network/HTTP error must be swallowed** (never block session completion). A non-2xx logs a warning, does not throw.
- Engine hook: on **session exit** the engine builds the payload from the just-finalized session (`status`, `isError = status != "done"` — see note below, `errorText = error`, `costUsd = cost_usd`) and fires push. No-op when no token or no creds. Fire-and-forget (must not block re-pump).
- `isError` derivation (mirror TS engine call site): the engine's exit handler in TS passes `isError: status !== "done"` and `errorText: s.error`. Match that.

### Usage (`usage.ts` + `routes.ts`)
- Endpoint: `https://api.anthropic.com/api/oauth/usage`.
- OAuth token read from `<base>/.credentials.json` → `claudeAiOauth.accessToken`. Missing/empty → error `"no oauth token in credentials"`.
- Request headers (exact): `authorization: Bearer <token>`, `anthropic-beta: oauth-2025-04-20`, `content-type: application/json`.
- Non-OK response → error `"usage endpoint <status>"`.
- `UsageInfo` is pass-through JSON (serve the upstream body verbatim). Known windows: `five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`, each `{ utilization: number, resets_at: string | null }`; plus arbitrary extra keys (`[k:string]: unknown`).
- Cache in the `/api/usage` route:
  - `USAGE_FRESH_MS = 60_000` — serve cache without refetch within this window.
  - `USAGE_STALE_MAX_MS = 10 * 60_000` — on upstream failure, serve last-good while younger than this; set header `x-usage-stale: 1`.
  - Past the stale bound → `503 { "error": "<message>" }`.
  - Single-flight: concurrent cache-miss callers coalesce onto **one** upstream fetch. `usageCache` holds the last **successful** fetch only.

### Structured diff (`structuredDiff.ts`)
- `GIT_TIMEOUT_MS = 30_000`, `MAX_BUF = 64 * 1024 * 1024`. A hung git is killed after the timeout; a non-zero exit yields whatever stdout was produced (lenient, never throws).
- `git(cwd, args)` runs `git -C <cwd> <args...>` async.
- Field separator: `SEP = "\x1f"` (ASCII unit separator). Log format: `%H\x1f%P\x1f%s\x1f%an\x1f%at`.
- `letterToStatus`: `A`→`added`, `D`→`deleted`, `R`→`renamed`, `C`→`renamed`, `M`→`modified`, else `unknown`.
- `commitGraphForRepo(worktree, baseSha)`:
  - sessionSet = `git rev-list <baseSha>..HEAD` (empty when baseSha falsy → nothing flagged `isSession`).
  - `git log --no-color -n 30 --pretty=<LOG_FMT> HEAD`. Each line split on SEP → `[sha, parentsStr, subject, author, at]`.
    - `shortSha = sha[..7]`, `parents = parentsStr.split(" ").filter(Boolean)`, `at = Number(at) * 1000` (sec→ms), `isSession = sessionSet.has(sha)`.
  - uncommitted from `git status --porcelain`: code = first 2 chars; `??` or contains `A` → added++; contains `D` → deleted++; else modified++. Null when all zero.
  - Returns `{ commits, uncommitted }` (route adds `repo`).
- `commitFilesForRepo(worktree, sha)`:
  - `sha === "working"`: `git diff --no-color --name-status HEAD` + `git diff --no-color --numstat HEAD`; plus untracked (`??` in porcelain) surfaced as `added`.
  - else: `git show --no-color --name-status --format= <sha>` + `git show --no-color --numstat --format= <sha>`.
  - name-status regex: `^([ACDMRT])\d*\t(.+)` → path = last tab-segment (renames: `<old>\t<new>` → new). numstat: `adds\tdels\tpath`; binary `-` → 0.
  - `mergeFiles`: union of paths; default status `modified`; default additions/deletions 0.
  - `CommitFile` fields: `path`, `status` (FileStatus string), `additions`, `deletions`.
- Engine `commitFiles` validation (mirror): `repo ∈ s.repos` else error `unknown repo: <repo>`; `sha === "working"` OR matches `^[0-9a-fA-F]{4,40}$` else error `bad sha: <sha>`. `commitGraph`/`commitFiles` are read-only (no busy/worktree-state guard).
- `repoWorktree(s, repo) = join(s.worktreePath, repo)`. `base = s.baseShas[repo] ?? null`.

### Workflows (`workflows.ts`)
- `WORKFLOW_TERMINAL` set (normalized trim+lowercase): `done`, `complete`, `completed`, `failed`, `error`, `killed`, `cancelled`, `canceled`.
- Project dirs: `<base>/projects/<slug>/<sid>/` where the sid dir has a `workflows/` OR `subagents/workflows/` subdir.
- `listWorkflows(base)`:
  - Completed runs from `<dir>/workflows/*.json`: `runId = d.runId ?? filename-without-.json`; dedupe by runId (first wins). agents from `d.workflowProgress` filtered to `type === "workflow_agent"`. Fields below.
  - In-flight runs from `<dir>/subagents/workflows/<runId>/` (skip if runId already seen): synthesize via `readRunningRun`.
- `WorkflowRun` JSON fields: `runId`, `name`, `status`, `summary?`, `agentCount?`, `createdAt` (epoch ms), `phases: [{title, detail?}]`, `agents: WorkflowAgent[]`, `logs: string[]`.
- `WorkflowAgent` JSON fields: `agentId`, `label`, `state`, `model`, `phaseTitle?`, `promptPreview?`, `resultPreview?`.
- `readRunningRun`: agent ids from `agent-<id>.meta.json` files (strip prefix `agent-`, suffix `.meta.json`), sorted; journal `journal.jsonl` lines `{agentId, type, result?}` mark `type==="result"` agents done + capture result preview; ids in journal but without meta appended. `null` if no ids. Synth agent: `label = "agent <i+1>"`, `state = done ? "done":"running"`, `model = ""`, `resultPreview` from journal. Run: `status:"running"`, `name = runName(...)`, `agentCount = ids.len`, `phases:[]`, `logs:[]`.
- `runName(dir, runId)`: find `<dir>/workflows/scripts/<name>-<runId>.js` → `name`; else `"workflow"`.
- `runCreatedAt(dir, runId, fallback)`: script mtime (round to int ms) if found; else `birthtime || mtime` of fallback rounded; else 0.
- `readWorkflowAgent(base, runId, agentId)`: reject runId/agentId not matching `^[A-Za-z0-9_.-]+$` or containing `..` → `""`. Find `<dir>/subagents/workflows/<runId>/agent-<agentId>.jsonl`. Parse lines: `type==="user"` → input from `message.content` text; `type==="assistant"` → output. Result string: `"**Task**\n\n<inputs joined \n\n>"` + (if both) `"\n\n---\n\n"` + `"**Output**\n\n<outputs joined \n\n>"`. `""` if file missing or empty.
- `hasActiveWorkflow` already exists in `src/engine.rs` (Phase 4) — but it is a **partial** scan; `listWorkflows`-based parity is part of this phase only for the route payload. Leave `has_active_workflow` as-is (it feeds `workflowRunning`); do NOT rewire it.
- `textOf(content)`: string → itself; array → blocks with `type==="text"` joined by `\n`; else `""`.

### Repos / skills / groups / templates
- `listRemoteRepos(gitOrg)`: `gh repo list <org> --json name -L 200` → parse `[{name}]`, map names, **sorted**; `[]` on any failure.
- `/api/repos` returns `{ local: <listRepos(srcRoot)>, remote: <listRemoteRepos(gitOrg)> }`. `listRepos`: direct child dirs of srcRoot containing `.git`, **sorted**; `[]` if srcRoot missing.
- `listSkills(skillsDir)`: each entry under skillsDir with a `SKILL.md`; parse frontmatter `^---\n(...)\n---`; `name:` (fallback entry name), `description:` (fallback ""); both `.trim()`. Sorted by name (`localeCompare`). `SkillInfo { name, description }`. `[]` if dir missing.
- `Group { name, repos: string[], skills: string[] }`. normalize: drop if `name` not a non-empty string (trimmed); `repos`/`skills` keep only strings (default []). `listGroups`: `[]` if missing/malformed/non-array. `saveGroups`: normalize each, **dedupe by name (last wins)**, atomic write, return clean list. `PUT /api/groups`: 400 `{error:"array of groups required"}` if body not an array.
- `Template { name, repos, skills, promptBody, model?, effort?, mode?, vars? }`. normalize: drop if `name` empty or `promptBody` not a string; `model/effort/mode` → string or null; `repos/skills/vars` keep strings (default []). `listTemplates`/`saveTemplates` mirror groups. `PUT /api/templates`: 400 `{error:"array of templates required"}` if not array.
- `resolvePrompt(promptBody, vars)`: replace `{{varName}}` (regex `\{\{(\w+)\}\}`) with `vars[name]` if present, else leave `{{name}}` verbatim.
- `POST /api/templates/start` body `{name?, vars?, model?, effort?, mode?}`: 400 `{error:"name required"}` if no name; 404 `{error:"template '<name>' not found"}`; resolve prompt; `submitSession(tpl.repos, tpl.skills, prompt, undefined, {model: b.model ?? tpl.model ?? null, effort: ..., mode: ...})`; return `{id}`; submit error → 400 `{error}`.
- `POST /api/devices` body `{token?}`: 400 `{error:"token required"}` if not a non-empty (trimmed) string; else save trimmed token, return `{ok:true, registeredAt:<ms>}`.

### atomicWrite
- `writeFileAtomic(path, content)`: write to `<path>.tmp`, fsync, rename over target, best-effort fsync the parent dir (never error on dir-fsync failure).

### File / upload / outbox routes
- `sessionCwd(s)`: `s.repos.len() == 1` → `join(worktreePath, repos[0])`, else `worktreePath`.
- `configBaseFor(s)`: `join(s.worktreePath, ".claude-config")`.
- `safePath(s, p)`: `base = realpath(worktreePath)`; `full = realpath(resolve(sessionCwd, p))`; ok iff `full == base` or `full.startsWith(base + MAIN_SEPARATOR)`. Returns absolute path or null (escape). Only call once the file exists (realpath needs an existing path).
- MIME map (extension → type), default `application/octet-stream`:
  `.png`→`image/png`, `.jpg`/`.jpeg`→`image/jpeg`, `.gif`→`image/gif`, `.webp`→`image/webp`, `.bmp`→`image/bmp`, `.svg`→`image/svg+xml`, `.pdf`→`application/pdf`, `.txt`→`text/plain`, `.md`→`text/markdown`, `.json`→`application/json`, `.csv`→`text/csv`, `.html`→`text/html`, `.mp4`→`video/mp4`, `.webm`→`video/webm`, `.mp3`→`audio/mpeg`, `.wav`→`audio/wav`.
- `GET /api/sessions/:id/file?path=`: 404 if session missing; 400 `{error:"path required"}` if no `path`; resolve+confine via safePath; 404 `{error:"not found"}` if missing/not a file; respond with `content-type: <MIME>`, `content-disposition: inline; filename="<basename>"`, **`content-length: <size>`** (the client needs a real length — do NOT gzip/chunk), body = file bytes. Route must NOT compress.
- `POST /api/sessions/:id/upload` (multipart, single file): 404 if session missing; 400 `{error:"session has no worktree"}` if no worktree; 400 `{error:"no file"}` if no file part; safe name = `basename(filename||"upload")` with `[^\w.\-]+` → `_`, fallback `"upload"`; dest = `<sessionCwd>/uploads/<safe>` (mkdir -p). On over-cap (`AGENTIC_UPLOAD_MAX_BYTES`, default 64MB) remove the partial + 413 `{error:"file too large"}`. Success → `{path: "uploads/<safe>"}`.
- `GET /api/sessions/:id/outbox`: 404 if session missing; `{files:[]}` if no worktree. Scan `<sessionCwd>/outbox` (+ for multi-repo: each `<worktreePath>/<repo>/outbox` with prefix `<repo>/outbox`). Each file entry: `{path: "<rel>/<name>", name, mtime: floor(mtimeMs)}`. mtime floored to integer.
- `GET /api/sessions/:id/commits`: 404 if missing; `{repos: <commitGraph>}`; engine error → 400 `{error}`.
- `GET /api/sessions/:id/commits/:sha/files?repo=`: 404 if missing; `{files: <commitFiles(id, repo, sha)>}`; error → 400 `{error}`.
- `GET /api/sessions/:id/workflows`: 404 if missing; `{workflows:[]}` if no worktree; else `{workflows: listWorkflows(configBaseFor(s))}`.
- `GET /api/sessions/:id/workflows/:runId/agents/:agentId`: 404 if missing or no worktree; `{transcript: readWorkflowAgent(configBaseFor(s), runId, agentId)}`.

### Config additions (`AGENTIC_*` env, same names/defaults as `config.ts`)
- `AGENTIC_SKILLS_DIR` default `<home>/.claude/skills`.
- `AGENTIC_GROUPS_PATH` default `<dataDir>/groups.json`.
- `AGENTIC_TEMPLATES_PATH` default `<dataDir>/templates.json`.
- `AGENTIC_DEVICE_TOKEN_PATH` default `<dataDir>/device.json`.
- `AGENTIC_UPLOAD_MAX_BYTES` default 64 MB (`64 * 1024 * 1024`).
- `AGENTIC_FCM_PROJECT_ID`, `AGENTIC_FCM_SERVER_KEY` (no default; absent → push no-op).
- `dataDir = AGENTIC_DATA_DIR ?? <home>/.agentic-dev`.

---

## File Structure

New / changed files under `server-rs/`:

```
Cargo.toml                       # + reqwest (rustls, json), tempfile (dev). NOT a new tokio feature.
src/atomic_write.rs              # NEW — write_file_atomic (T1)
src/push.rs                      # NEW — DeviceRecord, load/save token, FcmCreds, PushPayload, send_push (T2)
src/usage.rs                     # NEW — fetch_usage + UsageError (T3)
src/structured_diff.rs           # NEW — commit_graph_for_repo, commit_files_for_repo (T4)
src/workflows.rs                 # NEW — list_workflows, read_workflow_agent (T5)
src/skills.rs                    # NEW — list_skills (T6); list_remote_repos added to repos.rs
src/groups.rs                    # NEW — Group, list/save (T7)
src/templates.rs                 # NEW — Template, list/save, resolve_prompt (T8)
src/repos.rs                     # + list_repos (local) + list_remote_repos (T6)
src/config.rs                    # + skills_dir, groups_path, templates_path, device_token_path,
                                 #   upload_max_bytes, fcm_* (T9)
src/state.rs                     # + usage_cache: Arc<Mutex<UsageCache>> (T3 route)
src/engine.rs                    # commit_graph/commit_files methods + push hook wiring (T4, T2)
src/api/misc.rs                  # NEW — repos/skills/usage/groups/devices/templates routes (T3,T6,T7,T8)
src/api/sessions.rs              # + file/upload/outbox/commits/workflows routes (T4,T5,T10)
src/api/mod.rs                   # register all new routes (each task wires its route)
src/main.rs                      # wire push_fn closure + usage_cache + config fields (T11)
```

`src/main.rs` `mod` list gains: `atomic_write`, `push`, `usage`, `structured_diff`, `workflows`, `skills`, `groups`, `templates`.

---

## Cargo dependency additions (do once, in T2)

Add to `[dependencies]`:
```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
indexmap = "2"
```
Change the existing axum line to enable multipart:
```toml
axum = { version = "0.8", features = ["ws", "multipart"] }
```
Add to `[dev-dependencies]`:
```toml
tempfile = "3"
```
- `reqwest` is only used by `usage.rs` (real fetch) and the `send_push` real-network path; both are behind injectable seams so unit tests never touch the network.
- `indexmap` gives an insertion-ordered map for the groups/templates dedupe-last-wins (parity with the TS `Map`).
- `multipart` upload parsing uses axum's built-in `axum::extract::Multipart` (needs the `multipart` feature).
- `tempfile` is only needed if a test prefers it over the manual `temp_dir()` helpers used elsewhere; the tests in this plan use the existing manual-temp pattern, so `tempfile` is optional — include it only if you adopt it.

---

# TDD Tasks

Each task: write the failing test, run `cargo test` (expect failure), implement, run `cargo test` (expect pass), then `git -C <repo> add -A && git -C <repo> commit -m "..."` — **commit only, never push**. Run from `server-rs/`: all `cargo` commands assume cwd `server-rs`.

---

## T1 — atomic write helper

**Files:** `src/atomic_write.rs` (new), `src/main.rs` (add `mod atomic_write;`).

**Interfaces:**
```rust
pub fn write_file_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()>;
```

### Step 1 — failing test
Add `mod atomic_write;` to `src/main.rs`, then create `src/atomic_write.rs`:
```rust
use std::io::Write;
use std::path::Path;

/// Write a file atomically + durably: write to `<path>.tmp`, fsync it, rename over the target,
/// then best-effort fsync the parent dir so the rename survives power loss. Mirrors atomicWrite.ts.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-aw-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_and_overwrites_atomically() {
        let dir = tmp();
        let p = dir.join("f.json");
        write_file_atomic(&p, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first");
        write_file_atomic(&p, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");
        // no leftover temp file
        assert!(!dir.join("f.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
```
Run: `cargo test atomic_write` → fails (`todo!`).

### Step 2 — implement
```rust
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?; // fsync the bytes
    }
    std::fs::rename(&tmp, path)?;
    // Best-effort directory fsync so the rename is durable. Must never turn success into an error.
    if let Some(parent) = path.parent() {
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}
```
Run: `cargo test atomic_write` → pass.

### Step 3 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: atomic write helper (T1)"`

---

## T2 — FCM push: device-token store + send_push + Cargo deps

**Files:** `Cargo.toml`, `src/push.rs` (new), `src/main.rs` (`mod push;`).

**Interfaces:**
```rust
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct DeviceRecord {
    pub token: String,
    #[serde(rename = "registeredAt")]
    pub registered_at: i64,
}

pub fn load_device_token(path: &std::path::Path) -> Option<DeviceRecord>;
pub fn save_device_token(path: &std::path::Path, token: &str) -> std::io::Result<DeviceRecord>;

#[derive(Clone, Debug)]
pub struct FcmCreds { pub project_id: String, pub server_key: String }
pub fn load_fcm_creds(get: impl Fn(&str) -> Option<String>) -> Option<FcmCreds>;

#[derive(Clone, Debug)]
pub struct PushPayload {
    pub session_id: String,
    pub status: String,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub cost_usd: Option<f64>,
}

/// Build the FCM HTTP v1 body for a payload (the exact JSON the TS sendPush sends).
pub fn fcm_body(payload: &PushPayload, device_token: &str) -> serde_json::Value;

/// Send a finish-line push. No-ops when token or creds is None. `send_fn` is the injectable
/// sender (tests pass a closure that records the body); None → real reqwest POST (errors swallowed).
pub async fn send_push(
    payload: &PushPayload,
    device_token: Option<&str>,
    creds: Option<&FcmCreds>,
    send_fn: Option<&(dyn Fn(serde_json::Value) + Send + Sync)>,
);
```

### Step 1 — Cargo deps
Edit `Cargo.toml`: add `multipart` to axum features, add `reqwest` to `[dependencies]`, add `tempfile` to `[dev-dependencies]` (see "Cargo dependency additions" above). Run `cargo build` to confirm it resolves.

### Step 2 — failing test
`src/push.rs` skeleton with `todo!()` bodies, then:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-push-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_returns_none_when_missing() {
        assert!(load_device_token(&tmp().join("device.json")).is_none());
    }

    #[test]
    fn round_trips_and_last_wins() {
        let p = tmp().join("device.json");
        let r = save_device_token(&p, "tok-abc").unwrap();
        assert_eq!(r.token, "tok-abc");
        assert_eq!(load_device_token(&p).unwrap().token, "tok-abc");
        save_device_token(&p, "second").unwrap();
        assert_eq!(load_device_token(&p).unwrap().token, "second");
    }

    #[test]
    fn load_returns_none_for_empty_token_or_garbage() {
        let p = tmp().join("d.json");
        std::fs::write(&p, r#"{"token":"","registeredAt":1}"#).unwrap();
        assert!(load_device_token(&p).is_none());
        std::fs::write(&p, "not json").unwrap();
        assert!(load_device_token(&p).is_none());
    }

    #[test]
    fn creds_require_both_env_vars() {
        assert!(load_fcm_creds(|_| None).is_none());
        assert!(load_fcm_creds(|k| if k == "AGENTIC_FCM_PROJECT_ID" { Some("p".into()) } else { None }).is_none());
        let c = load_fcm_creds(|k| match k {
            "AGENTIC_FCM_PROJECT_ID" => Some("proj".into()),
            "AGENTIC_FCM_SERVER_KEY" => Some("key".into()),
            _ => None,
        }).unwrap();
        assert_eq!(c.project_id, "proj");
        assert_eq!(c.server_key, "key");
    }

    fn payload() -> PushPayload {
        PushPayload { session_id: "s1".into(), status: "done".into(), is_error: false,
            error_text: None, cost_usd: Some(0.0042) }
    }

    #[tokio::test]
    async fn no_op_when_token_or_creds_missing() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        let f = move |_b: serde_json::Value| { *c.lock().unwrap() += 1; };
        let creds = FcmCreds { project_id: "p".into(), server_key: "k".into() };
        send_push(&payload(), None, Some(&creds), Some(&f)).await;          // no token
        send_push(&payload(), Some("dev"), None, Some(&f)).await;           // no creds
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn calls_send_fn_with_correct_body() {
        use std::sync::{Arc, Mutex};
        let body = Arc::new(Mutex::new(serde_json::Value::Null));
        let b = body.clone();
        let f = move |v: serde_json::Value| { *b.lock().unwrap() = v; };
        let creds = FcmCreds { project_id: "my-project".into(), server_key: "my-key".into() };
        send_push(&payload(), Some("device-token-xyz"), Some(&creds), Some(&f)).await;
        let v = body.lock().unwrap().clone();
        assert_eq!(v["message"]["token"], "device-token-xyz");
        assert_eq!(v["message"]["data"]["sessionId"], "s1");
        assert_eq!(v["message"]["data"]["status"], "done");
        assert_eq!(v["message"]["data"]["isError"], "false");
        // success body: "Completed ($0.0042)" — 4 decimals
        assert_eq!(v["message"]["notification"]["body"], "Completed ($0.0042)");
        assert_eq!(v["message"]["notification"]["title"], "Session done");
    }

    #[test]
    fn error_body_and_null_cost_serialize_to_strings() {
        let p = PushPayload { session_id: "s2".into(), status: "failed".into(), is_error: true,
            error_text: Some("boom".into()), cost_usd: None };
        let v = fcm_body(&p, "tok");
        assert_eq!(v["message"]["notification"]["body"], "Error: boom");
        assert_eq!(v["message"]["data"]["isError"], "true");
        assert_eq!(v["message"]["data"]["costUsd"], "");     // null → empty string
        // missing errorText → "unknown error"
        let p2 = PushPayload { error_text: None, ..p.clone() };
        let v2 = fcm_body(&p2, "tok");
        assert_eq!(v2["message"]["notification"]["body"], "Error: unknown error");
    }
}
```
(Add `#[derive(Clone)]` already on `PushPayload`.) Run: `cargo test push` → fails.

### Step 3 — implement
```rust
use std::path::Path;
use crate::atomic_write::write_file_atomic;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct DeviceRecord {
    pub token: String,
    #[serde(rename = "registeredAt")]
    pub registered_at: i64,
}

pub fn load_device_token(path: &Path) -> Option<DeviceRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    let rec: DeviceRecord = serde_json::from_str(&text).ok()?;
    if rec.token.is_empty() { None } else { Some(rec) }
}

pub fn save_device_token(path: &Path, token: &str) -> std::io::Result<DeviceRecord> {
    let rec = DeviceRecord {
        token: token.to_string(),
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64,
    };
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    write_file_atomic(path, &serde_json::to_string_pretty(&rec).unwrap())?;
    Ok(rec)
}

#[derive(Clone, Debug)]
pub struct FcmCreds { pub project_id: String, pub server_key: String }

pub fn load_fcm_creds(get: impl Fn(&str) -> Option<String>) -> Option<FcmCreds> {
    let project_id = get("AGENTIC_FCM_PROJECT_ID").filter(|s| !s.is_empty())?;
    let server_key = get("AGENTIC_FCM_SERVER_KEY").filter(|s| !s.is_empty())?;
    Some(FcmCreds { project_id, server_key })
}

#[derive(Clone, Debug)]
pub struct PushPayload {
    pub session_id: String,
    pub status: String,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub cost_usd: Option<f64>,
}

pub fn fcm_body(payload: &PushPayload, device_token: &str) -> serde_json::Value {
    let title = format!("Session {}", payload.status);
    let body = if payload.is_error {
        format!("Error: {}", payload.error_text.as_deref().unwrap_or("unknown error"))
    } else {
        match payload.cost_usd {
            Some(c) => format!("Completed (${:.4})", c),
            None => "Completed".to_string(),
        }
    };
    let cost_str = match payload.cost_usd { Some(c) => c.to_string(), None => String::new() };
    serde_json::json!({
        "message": {
            "token": device_token,
            "notification": { "title": title, "body": body },
            "data": {
                "sessionId": payload.session_id,
                "status": payload.status,
                "isError": payload.is_error.to_string(),
                "costUsd": cost_str,
            }
        }
    })
}

pub async fn send_push(
    payload: &PushPayload,
    device_token: Option<&str>,
    creds: Option<&FcmCreds>,
    send_fn: Option<&(dyn Fn(serde_json::Value) + Send + Sync)>,
) {
    let (Some(token), Some(creds)) = (device_token.filter(|t| !t.is_empty()), creds) else { return; };
    let body = fcm_body(payload, token);
    if let Some(f) = send_fn { f(body); return; }
    // Real FCM HTTP v1 send — failure is swallowed (a push must never block session completion).
    let url = format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", creds.project_id);
    let client = reqwest::Client::new();
    match client.post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", creds.server_key))
        .json(&body).send().await
    {
        Ok(res) if !res.status().is_success() =>
            tracing::warn!("[push] FCM send failed: {}", res.status()),
        Ok(_) => {}
        Err(e) => tracing::warn!("[push] FCM send error (ignored): {e}"),
    }
}
```
Add `mod push;` (and `mod atomic_write;` if not yet) to `src/main.rs`. Run: `cargo test push` → pass.

> **Gotcha (cost formatting):** TS `(0.0042).toFixed(4) === "0.0042"`; Rust `format!("{:.4}", 0.0042) == "0.0042"` — matches. But `costUsd` data field uses `String(0.0042)` in TS = `"0.0042"`; Rust `0.0042f64.to_string() == "0.0042"` — matches. Verify the test asserts both. For round numbers TS `String(0.01) === "0.01"` and Rust `0.01f64.to_string() == "0.01"` — matches.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: FCM push store + send_push (T2)"`

---

## T3 — usage fetch + cached `/api/usage` route

**Files:** `src/usage.rs` (new), `src/state.rs` (+ usage_cache), `src/api/misc.rs` (new, usage route), `src/api/mod.rs` (register), `src/main.rs` (`mod usage;` + `mod misc;` under api).

**Interfaces:**
```rust
// usage.rs
#[derive(thiserror::Error, Debug)]
pub enum UsageError {
    #[error("no oauth token in credentials")] NoToken,
    #[error("usage endpoint {0}")] Status(u16),
    #[error("{0}")] Other(String),
}

/// fetch_fn lets tests inject a fake upstream: it receives (url, bearer_token) and returns
/// Ok(json) or Err(status). None → real reqwest GET against the Anthropic usage endpoint.
pub async fn fetch_usage(
    base: &std::path::Path,
    fetch_fn: Option<&(dyn Fn(&str, &str) -> Result<serde_json::Value, u16> + Send + Sync)>,
) -> Result<serde_json::Value, UsageError>;
```

```rust
// state.rs additions
#[derive(Default)]
pub struct UsageCache {
    pub at: i64,                       // epoch ms of last SUCCESSFUL fetch
    pub data: Option<serde_json::Value>,
}
// AppState gains: pub usage_cache: Arc<Mutex<UsageCache>>,
```

### Step 1 — failing test (usage.rs)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn base_with(creds: serde_json::Value) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".credentials.json"), creds.to_string()).unwrap();
        d
    }

    #[tokio::test]
    async fn sends_bearer_and_returns_parsed() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{"accessToken":"tok-123"}}));
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(String::new()));
        let s = seen.clone();
        let f = move |_url: &str, tok: &str| -> Result<serde_json::Value, u16> {
            *s.lock().unwrap() = tok.to_string();
            Ok(serde_json::json!({"seven_day":{"utilization":49,"resets_at":"y"}}))
        };
        let u = fetch_usage(&base, Some(&f)).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), "tok-123");
        assert_eq!(u["seven_day"]["utilization"], 49);
    }

    #[tokio::test]
    async fn errors_when_token_missing() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{}}));
        let f = |_u: &str, _t: &str| -> Result<serde_json::Value, u16> { Ok(serde_json::json!({})) };
        let e = fetch_usage(&base, Some(&f)).await.unwrap_err();
        assert!(matches!(e, UsageError::NoToken));
    }

    #[tokio::test]
    async fn errors_on_non_ok() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{"accessToken":"t"}}));
        let f = |_u: &str, _t: &str| -> Result<serde_json::Value, u16> { Err(401) };
        let e = fetch_usage(&base, Some(&f)).await.unwrap_err();
        assert!(matches!(e, UsageError::Status(401)));
    }
}
```
Run: `cargo test usage` → fails.

### Step 2 — implement (usage.rs)
```rust
use std::path::Path;

pub const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(thiserror::Error, Debug)]
pub enum UsageError {
    #[error("no oauth token in credentials")] NoToken,
    #[error("usage endpoint {0}")] Status(u16),
    #[error("{0}")] Other(String),
}

pub async fn fetch_usage(
    base: &Path,
    fetch_fn: Option<&(dyn Fn(&str, &str) -> Result<serde_json::Value, u16> + Send + Sync)>,
) -> Result<serde_json::Value, UsageError> {
    let text = std::fs::read_to_string(base.join(".credentials.json"))
        .map_err(|e| UsageError::Other(e.to_string()))?;
    let creds: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| UsageError::Other(e.to_string()))?;
    let token = creds.get("claudeAiOauth").and_then(|c| c.get("accessToken"))
        .and_then(|t| t.as_str()).filter(|t| !t.is_empty())
        .ok_or(UsageError::NoToken)?;
    if let Some(f) = fetch_fn {
        return f(USAGE_ENDPOINT, token).map_err(UsageError::Status);
    }
    let client = reqwest::Client::new();
    let res = client.get(USAGE_ENDPOINT)
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("content-type", "application/json")
        .send().await.map_err(|e| UsageError::Other(e.to_string()))?;
    if !res.status().is_success() { return Err(UsageError::Status(res.status().as_u16())); }
    res.json::<serde_json::Value>().await.map_err(|e| UsageError::Other(e.to_string()))
}
```

### Step 3 — state + route
Add to `src/state.rs`:
```rust
#[derive(Default)]
pub struct UsageCache { pub at: i64, pub data: Option<serde_json::Value> }
```
and `pub usage_cache: Arc<Mutex<UsageCache>>,` on `AppState` (update both test_support constructors + `api/mod.rs` tests + `main.rs` to init `Arc::new(Mutex::new(UsageCache::default()))`).

Create `src/api/misc.rs` with the usage route (cache logic). Because a true single-flight across awaits in Rust needs care, mirror the TS semantics with a simpler-but-equivalent guard: hold the last-good in the mutex; on a miss do the fetch (an in-process `Mutex<Option<tokio::sync::Notify>>`-style single-flight is optional — the simplest parity-correct version below serializes misses behind a `tokio::sync::Mutex` "inflight" lock so concurrent callers coalesce):
```rust
use axum::{extract::State, http::{StatusCode, HeaderMap}, response::{IntoResponse, Response}, Json};
use serde_json::json;
use crate::state::AppState;

const USAGE_FRESH_MS: i64 = 60_000;
const USAGE_STALE_MAX_MS: i64 = 10 * 60_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

pub async fn usage_route(State(st): State<AppState>) -> Response {
    let now = now_ms();
    // Fast path: fresh cache.
    {
        let c = st.usage_cache.lock().unwrap();
        if let Some(ref data) = c.data {
            if now - c.at < USAGE_FRESH_MS { return Json(data.clone()).into_response(); }
        }
    }
    // Coalesce concurrent misses behind the inflight async lock (single-flight).
    let _guard = st.usage_inflight.lock().await;
    // Re-check freshness after acquiring the lock (another waiter may have just refreshed).
    {
        let c = st.usage_cache.lock().unwrap();
        if let Some(ref data) = c.data {
            if now_ms() - c.at < USAGE_FRESH_MS { return Json(data.clone()).into_response(); }
        }
    }
    let base = st.config.claude_config_base.clone();
    // `usage_fn` lives on AppState (a test seam), NOT on Config — see the AppState note below.
    let fetched = match &st.usage_fn {
        Some(f) => f().await,
        None => crate::usage::fetch_usage(&base, None).await,
    };
    match fetched {
        Ok(data) => {
            { let mut c = st.usage_cache.lock().unwrap(); c.at = now_ms(); c.data = Some(data.clone()); }
            Json(data).into_response()
        }
        Err(e) => {
            // Transient failure: serve last-good while not too stale, with x-usage-stale: 1.
            let c = st.usage_cache.lock().unwrap();
            if let Some(ref data) = c.data {
                if now - c.at < USAGE_STALE_MAX_MS {
                    let mut headers = HeaderMap::new();
                    headers.insert("x-usage-stale", "1".parse().unwrap());
                    return (headers, Json(data.clone())).into_response();
                }
            }
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))).into_response()
        }
    }
}
```
Add three fields to `AppState` (NOT to `Config` — keep `Config: Clone` simple). The `usage_fn` test seam uses a hand-written boxed-future alias (no `futures` crate dependency):
```rust
// state.rs
pub type UsageFn = std::sync::Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, crate::usage::UsageError>> + Send>> + Send + Sync>;
// AppState gains:
//   pub usage_cache: Arc<Mutex<UsageCache>>,
//   pub usage_inflight: Arc<tokio::sync::Mutex<()>>,
//   pub usage_fn: Option<UsageFn>,
```
Register in `api/mod.rs`: `.route("/api/usage", get(misc::usage_route))`.

### Step 4 — route test (in `src/api/misc.rs` tests)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{test_state, auth, oneshot_req};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn usage_serves_fetch_then_caches() {
        let mut st = test_state().await;
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        st.usage_fn = Some(Arc::new(move || {
            let c = c.clone();
            Box::pin(async move { *c.lock().unwrap() += 1;
                Ok(serde_json::json!({"five_hour":{"utilization":12,"resets_at":"x"}})) })
        }));
        for _ in 0..2 {
            let (s, b) = oneshot_req(st.clone(), Request::get("/api/usage")
                .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(b["five_hour"]["utilization"], 12);
        }
        // Within the 60s freshness window the second call is served from cache → fetch ran once.
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn usage_failure_with_no_cache_is_503() {
        let mut st = test_state().await;
        st.usage_fn = Some(Arc::new(|| Box::pin(async {
            Err(crate::usage::UsageError::Status(429)) })));
        let (s, b) = oneshot_req(st.clone(), Request::get("/api/usage")
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(b["error"], "usage endpoint 429");
    }
}
```
> Note `oneshot_req`/`test_state` must thread the new `usage_fn`/`usage_cache`/`usage_inflight` fields. Update `test_support.rs` + `api/mod.rs::test_state` accordingly (default `usage_fn: None`). Run: `cargo test usage` → pass.

### Step 5 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: usage fetch + cached /api/usage (T3)"`

---

## T4 — structured diff + engine commit_graph/commit_files + commits routes

**Files:** `src/structured_diff.rs` (new), `src/engine.rs` (+ methods), `src/api/sessions.rs` (+ routes), `src/api/mod.rs` (register), `src/main.rs` (`mod structured_diff;`).

**Interfaces:**
```rust
// structured_diff.rs
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus { Added, Modified, Deleted, Renamed, Unknown }

#[derive(serde::Serialize, Clone, Debug)]
pub struct CommitNode {
    pub sha: String,
    #[serde(rename = "shortSha")] pub short_sha: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    pub at: i64,
    #[serde(rename = "isSession")] pub is_session: bool,
}
#[derive(serde::Serialize, Clone, Debug)]
pub struct Uncommitted { pub added: u32, pub modified: u32, pub deleted: u32 }
#[derive(serde::Serialize, Clone, Debug)]
pub struct RepoGraph { pub commits: Vec<CommitNode>, pub uncommitted: Option<Uncommitted> }
#[derive(serde::Serialize, Clone, Debug)]
pub struct CommitFile { pub path: String, pub status: FileStatus, pub additions: u32, pub deletions: u32 }

pub async fn commit_graph_for_repo(worktree: &std::path::Path, base_sha: Option<&str>) -> RepoGraph;
pub async fn commit_files_for_repo(worktree: &std::path::Path, sha: &str) -> Vec<CommitFile>;
```
```rust
// engine.rs (impl Engine)
pub async fn commit_graph(&self, id: &str) -> Result<serde_json::Value, String>; // [{repo, commits, uncommitted}]
pub async fn commit_files(&self, id: &str, repo: &str, sha: &str) -> Result<Vec<CommitFile>, String>;
```

### Step 1 — failing test (structured_diff.rs)
Mirror `structuredDiff.test.ts` exactly. Use a helper to make a git worktree on a temp repo:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::path::{Path, PathBuf};

    fn run(cwd: &Path, args: &[&str]) -> String {
        let o = Command::new("git").args(args).current_dir(cwd).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn temp_repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-sd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        for a in [vec!["init","-q"],vec!["config","user.email","t@t"],vec!["config","user.name","t"]] {
            Command::new("git").args(&a).current_dir(&d).status().unwrap();
        }
        std::fs::write(d.join("README.md"), "# r\n").unwrap();
        run(&d, &["add","."]); run(&d, &["commit","-q","-m","init"]);
        d
    }
    fn commit_file(wt: &Path, name: &str, content: &str, msg: &str) -> String {
        std::fs::write(wt.join(name), content).unwrap();
        run(wt, &["add","."]); run(wt, &["commit","-q","-m",msg]);
        run(wt, &["rev-parse","HEAD"])
    }

    #[tokio::test]
    async fn graph_newest_first_with_session_flags_and_uncommitted() {
        let repo = temp_repo();
        let base = run(&repo, &["rev-parse","HEAD"]);
        let sha1 = commit_file(&repo, "a.txt", "a\n", "add a");
        let sha2 = commit_file(&repo, "b.txt", "b\n", "add b");
        let clean = commit_graph_for_repo(&repo, Some(&base)).await;
        assert!(clean.uncommitted.is_none());
        assert_eq!(clean.commits[0].sha, sha2);
        assert_eq!(clean.commits[1].sha, sha1);
        assert_eq!(clean.commits[2].sha, base);
        let by = |s: &str| clean.commits.iter().find(|c| c.sha == s).unwrap();
        assert!(by(&sha2).is_session);
        assert!(by(&sha1).is_session);
        assert!(!by(&base).is_session);
        assert_eq!(by(&sha2).parents, vec![sha1.clone()]);
        assert_eq!(by(&sha2).short_sha, sha2[..7]);
        assert!(by(&sha2).at > 0);
        // dirty
        std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
        let dirty = commit_graph_for_repo(&repo, Some(&base)).await;
        assert!(dirty.uncommitted.unwrap().modified > 0);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn graph_marks_nothing_session_when_base_falsy() {
        let repo = temp_repo();
        commit_file(&repo, "a.txt", "a\n", "add a");
        let g = commit_graph_for_repo(&repo, None).await;
        assert!(g.commits.iter().all(|c| !c.is_session));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn files_for_a_real_sha() {
        let repo = temp_repo();
        let sha = commit_file(&repo, "new.txt", "one\ntwo\n", "add new");
        let files = commit_files_for_repo(&repo, &sha).await;
        let f = files.iter().find(|x| x.path == "new.txt").unwrap();
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 0);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn files_for_working_includes_untracked_as_added() {
        let repo = temp_repo();
        std::fs::write(repo.join("README.md"), "# changed\n\nmore\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "x\n").unwrap();
        let files = commit_files_for_repo(&repo, "working").await;
        assert_eq!(files.iter().find(|x| x.path=="README.md").unwrap().status, FileStatus::Modified);
        assert_eq!(files.iter().find(|x| x.path=="fresh.txt").unwrap().status, FileStatus::Added);
        std::fs::remove_dir_all(&repo).ok();
    }
}
```
Run: `cargo test structured_diff` → fails.

### Step 2 — implement (structured_diff.rs)
```rust
use std::path::Path;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const SEP: char = '\u{1f}';

async fn git(cwd: &Path, args: &[&str]) -> String {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    let fut = cmd.output();
    match tokio::time::timeout(GIT_TIMEOUT, fut).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).into_owned(), // lenient: stdout even on non-zero
        _ => String::new(),
    }
}

fn letter_to_status(l: char) -> FileStatus {
    match l {
        'A' => FileStatus::Added, 'D' => FileStatus::Deleted,
        'R' | 'C' => FileStatus::Renamed, 'M' => FileStatus::Modified,
        _ => FileStatus::Unknown,
    }
}

pub async fn commit_graph_for_repo(worktree: &Path, base_sha: Option<&str>) -> RepoGraph {
    use std::collections::HashSet;
    let mut session_set: HashSet<String> = HashSet::new();
    if let Some(base) = base_sha.filter(|b| !b.is_empty()) {
        let out = git(worktree, &["rev-list", &format!("{base}..HEAD")]).await;
        for line in out.lines() { let s = line.trim(); if !s.is_empty() { session_set.insert(s.to_string()); } }
    }
    let fmt = format!("--pretty=%H{SEP}%P{SEP}%s{SEP}%an{SEP}%at");
    let log = git(worktree, &["log", "--no-color", "-n", "30", &fmt, "HEAD"]).await;
    let mut commits = Vec::new();
    for line in log.split('\n') {
        if line.is_empty() { continue; }
        let parts: Vec<&str> = line.splitn(5, SEP).collect();
        let sha = parts.first().copied().unwrap_or("");
        if sha.is_empty() { continue; }
        let parents_str = parts.get(1).copied().unwrap_or("");
        let subject = parts.get(2).copied().unwrap_or("").to_string();
        let author = parts.get(3).copied().unwrap_or("").to_string();
        let at_sec: i64 = parts.get(4).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        commits.push(CommitNode {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            parents: parents_str.split(' ').filter(|p| !p.is_empty()).map(|s| s.to_string()).collect(),
            subject, author,
            at: at_sec * 1000,
            is_session: session_set.contains(sha),
        });
    }
    let status = git(worktree, &["status", "--porcelain"]).await;
    let (mut added, mut modified, mut deleted) = (0u32, 0u32, 0u32);
    for line in status.split('\n') {
        if line.trim().is_empty() { continue; }
        let code: String = line.chars().take(2).collect();
        if code == "??" || code.contains('A') { added += 1; }
        else if code.contains('D') { deleted += 1; }
        else { modified += 1; }
    }
    let uncommitted = if added > 0 || modified > 0 || deleted > 0 {
        Some(Uncommitted { added, modified, deleted })
    } else { None };
    RepoGraph { commits, uncommitted }
}

fn parse_name_status(out: &str) -> std::collections::HashMap<String, FileStatus> {
    use std::collections::HashMap;
    let re = regex::Regex::new(r"^([ACDMRT])\d*\t(.+)").unwrap();
    let mut map = HashMap::new();
    for line in out.split('\n') {
        if let Some(c) = re.captures(line) {
            let letter = c.get(1).unwrap().as_str().chars().next().unwrap();
            let rest = c.get(2).unwrap().as_str();
            let path = rest.split('\t').last().unwrap().to_string(); // renames: <old>\t<new> → new
            map.insert(path, letter_to_status(letter));
        }
    }
    map
}

fn parse_numstat(out: &str) -> std::collections::HashMap<String, (u32, u32)> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    for line in out.split('\n') {
        if line.trim().is_empty() { continue; }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 { continue; }
        let adds = if parts[0] == "-" { 0 } else { parts[0].parse().unwrap_or(0) };
        let dels = if parts[1] == "-" { 0 } else { parts[1].parse().unwrap_or(0) };
        let path = parts[2..].join("\t");
        map.insert(path, (adds, dels));
    }
    map
}

fn merge_files(
    status_map: std::collections::HashMap<String, FileStatus>,
    numstat_map: std::collections::HashMap<String, (u32, u32)>,
) -> Vec<CommitFile> {
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(status_map.keys().cloned());
    paths.extend(numstat_map.keys().cloned());
    paths.into_iter().map(|path| {
        let (additions, deletions) = numstat_map.get(&path).copied().unwrap_or((0, 0));
        CommitFile {
            status: status_map.get(&path).cloned().unwrap_or(FileStatus::Modified),
            additions, deletions, path,
        }
    }).collect()
}

pub async fn commit_files_for_repo(worktree: &Path, sha: &str) -> Vec<CommitFile> {
    if sha == "working" {
        let mut status_map = parse_name_status(&git(worktree, &["diff","--no-color","--name-status","HEAD"]).await);
        let numstat_map = parse_numstat(&git(worktree, &["diff","--no-color","--numstat","HEAD"]).await);
        let status = git(worktree, &["status","--porcelain"]).await;
        for line in status.split('\n') {
            if line.chars().take(2).collect::<String>() == "??" {
                let path = line.chars().skip(3).collect::<String>().trim().to_string();
                if !path.is_empty() && !status_map.contains_key(&path) {
                    status_map.insert(path, FileStatus::Added);
                }
            }
        }
        return merge_files(status_map, numstat_map);
    }
    let status_map = parse_name_status(&git(worktree, &["show","--no-color","--name-status","--format=",sha]).await);
    let numstat_map = parse_numstat(&git(worktree, &["show","--no-color","--numstat","--format=",sha]).await);
    merge_files(status_map, numstat_map)
}
```
> **Note** `merge_files` uses a `BTreeSet` (sorted) for deterministic order; the TS used insertion order of a `Set`. Order is not asserted by any test (lookups are by path), and a deterministic order is preferable for the client — acceptable parity. If exact insertion order is ever required, switch to an order-preserving map; not needed now.

### Step 3 — engine methods (engine.rs, inside `impl Engine`)
```rust
/// Per-repo commit-history graph. Read-only (works for running and terminal sessions).
pub async fn commit_graph(&self, id: &str) -> Result<serde_json::Value, String> {
    let s = self.get(id).await.ok_or_else(|| format!("unknown session: {id}"))?;
    let wt_root = s.worktree_path.clone().ok_or_else(|| "session has no worktree".to_string())?;
    let mut out = Vec::new();
    for repo in &s.repos {
        let wt = std::path::Path::new(&wt_root).join(repo);
        let base = s.base_shas.get(repo).and_then(|o| o.clone());
        let g = crate::structured_diff::commit_graph_for_repo(&wt, base.as_deref()).await;
        out.push(serde_json::json!({ "repo": repo, "commits": g.commits, "uncommitted": g.uncommitted }));
    }
    Ok(serde_json::Value::Array(out))
}

/// Changed-file list for one commit (or the working tree) in a repo. Validates repo ∈ s.repos and sha.
pub async fn commit_files(&self, id: &str, repo: &str, sha: &str) -> Result<Vec<crate::structured_diff::CommitFile>, String> {
    let s = self.get(id).await.ok_or_else(|| format!("unknown session: {id}"))?;
    if !s.repos.iter().any(|r| r == repo) { return Err(format!("unknown repo: {repo}")); }
    let valid_sha = sha == "working"
        || (sha.len() >= 4 && sha.len() <= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid_sha { return Err(format!("bad sha: {sha}")); }
    let wt_root = s.worktree_path.clone().ok_or_else(|| "session has no worktree".to_string())?;
    let wt = std::path::Path::new(&wt_root).join(repo);
    Ok(crate::structured_diff::commit_files_for_repo(&wt, sha).await)
}
```

### Step 4 — routes (api/sessions.rs)
```rust
pub async fn commits_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    match st.engine.commit_graph(&id).await {
        Ok(repos) => Json(json!({ "repos": repos })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct RepoQuery { pub repo: Option<String> }

pub async fn commit_files_route(
    State(st): State<AppState>,
    Path((id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Response {
    if st.engine.get(&id).await.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let repo = q.repo.unwrap_or_default();
    match st.engine.commit_files(&id, &repo, &sha).await {
        Ok(files) => Json(json!({ "files": files })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}
```
Register in `api/mod.rs`:
```rust
.route("/api/sessions/{id}/commits", get(sessions::commits_route))
.route("/api/sessions/{id}/commits/{sha}/files", get(sessions::commit_files_route))
```
Add `mod structured_diff;` to `main.rs`. Add a route test asserting 404 on unknown id + a happy-path commits round-trip on a seeded git repo (reuse `seed_demo_repo` pattern but create a session with `repos:["demo"]` + a worktree). Run: `cargo test` → pass.

### Step 5 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: structured diff + commit graph/files routes (T4)"`

---

## T5 — workflows: list_workflows + read_workflow_agent + real workflow routes

**Files:** `src/workflows.rs` (new), `src/api/sessions.rs` (real `workflows_route` + new agent route), `src/api/mod.rs` (register), `src/main.rs` (`mod workflows;`).

**Interfaces:**
```rust
#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct WorkflowAgent {
    #[serde(rename = "agentId")] pub agent_id: String,
    pub label: String,
    pub state: String,
    pub model: String,
    #[serde(rename = "phaseTitle", skip_serializing_if = "Option::is_none")] pub phase_title: Option<String>,
    #[serde(rename = "promptPreview", skip_serializing_if = "Option::is_none")] pub prompt_preview: Option<String>,
    #[serde(rename = "resultPreview", skip_serializing_if = "Option::is_none")] pub result_preview: Option<String>,
}
#[derive(serde::Serialize, Clone, Debug)]
pub struct WorkflowPhase {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub detail: Option<String>,
}
#[derive(serde::Serialize, Clone, Debug)]
pub struct WorkflowRun {
    #[serde(rename = "runId")] pub run_id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub summary: Option<String>,
    #[serde(rename = "agentCount", skip_serializing_if = "Option::is_none")] pub agent_count: Option<i64>,
    #[serde(rename = "createdAt")] pub created_at: i64,
    pub phases: Vec<WorkflowPhase>,
    pub agents: Vec<WorkflowAgent>,
    pub logs: Vec<String>,
}

pub fn list_workflows(base: &std::path::Path) -> Vec<WorkflowRun>;
pub fn read_workflow_agent(base: &std::path::Path, run_id: &str, agent_id: &str) -> String;
```

### Step 1 — failing test (workflows.rs)
Mirror the structure of the engine's `has_active_workflow` tests (which already build `projects/<slug>/<sid>/...`). Cover: completed-summary parse, in-flight journal synth, dedupe (summary wins), agent-transcript task/output formatting, path-traversal rejection.
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-wf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn sid_dir(base: &PathBuf) -> PathBuf {
        let d = base.join("projects").join("slug").join("sid");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_a_completed_summary_run() {
        let b = base();
        let sd = sid_dir(&b);
        let wf = sd.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("wf_1.json"), serde_json::json!({
            "runId":"wf_1","workflowName":"build","status":"done","createdAt":1234,
            "phases":[{"title":"plan"}],
            "workflowProgress":[{"type":"workflow_agent","agentId":"a1","label":"A","state":"done","model":"opus"}]
        }).to_string()).unwrap();
        let runs = list_workflows(&b);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "wf_1");
        assert_eq!(runs[0].name, "build");
        assert_eq!(runs[0].status, "done");
        assert_eq!(runs[0].created_at, 1234);
        assert_eq!(runs[0].agents.len(), 1);
        assert_eq!(runs[0].agents[0].agent_id, "a1");
        assert_eq!(runs[0].phases[0].title, "plan");
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn synthesizes_a_running_run_from_meta_and_journal() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_2");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("agent-a1.meta.json"), "{}").unwrap();
        std::fs::write(run.join("agent-a2.meta.json"), "{}").unwrap();
        std::fs::write(run.join("journal.jsonl"),
            "{\"agentId\":\"a1\",\"type\":\"result\",\"result\":\"ok\"}\n").unwrap();
        let runs = list_workflows(&b);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.run_id, "wf_2");
        assert_eq!(r.status, "running");
        assert_eq!(r.agent_count, Some(2));
        let a1 = r.agents.iter().find(|a| a.agent_id == "a1").unwrap();
        assert_eq!(a1.state, "done");
        assert_eq!(a1.result_preview.as_deref(), Some("ok"));
        let a2 = r.agents.iter().find(|a| a.agent_id == "a2").unwrap();
        assert_eq!(a2.state, "running");
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn summary_wins_over_live_journal_for_same_runid() {
        let b = base();
        let sd = sid_dir(&b);
        std::fs::create_dir_all(sd.join("workflows")).unwrap();
        std::fs::write(sd.join("workflows").join("dup.json"),
            serde_json::json!({"runId":"dup","status":"done"}).to_string()).unwrap();
        let live = sd.join("subagents").join("workflows").join("dup");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("agent-x.meta.json"), "{}").unwrap();
        let runs = list_workflows(&b);
        assert_eq!(runs.iter().filter(|r| r.run_id == "dup").count(), 1);
        assert_eq!(runs[0].status, "done");   // summary, not "running"
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn read_agent_formats_task_and_output_and_rejects_traversal() {
        let b = base();
        let sd = sid_dir(&b);
        let run = sd.join("subagents").join("workflows").join("wf_9");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("agent-a.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"do it\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n").unwrap();
        let t = read_workflow_agent(&b, "wf_9", "a");
        assert!(t.contains("**Task**"));
        assert!(t.contains("do it"));
        assert!(t.contains("**Output**"));
        assert!(t.contains("done"));
        assert!(t.contains("---"));
        // traversal rejected
        assert_eq!(read_workflow_agent(&b, "../etc", "a"), "");
        assert_eq!(read_workflow_agent(&b, "wf_9", "a/b"), "");
        // missing → empty
        assert_eq!(read_workflow_agent(&b, "nope", "a"), "");
        std::fs::remove_dir_all(&b).ok();
    }
}
```
Run: `cargo test workflows` → fails.

### Step 2 — implement (workflows.rs)
Port `workflows.ts` faithfully. Key helpers: `project_dirs`, `run_name`, `run_created_at`, `read_running_run`, `text_of`, `safe_segment`. Implementation sketch (fill bodies per the Global Constraints):
```rust
use std::path::{Path, PathBuf};

const WORKFLOW_TERMINAL: [&str; 8] =
    ["done","complete","completed","failed","error","killed","cancelled","canceled"];

fn project_dirs(base: &Path) -> Vec<PathBuf> {
    let projects = base.join("projects");
    let mut out = Vec::new();
    let Ok(slugs) = std::fs::read_dir(&projects) else { return out; };
    for slug in slugs.flatten() {
        let Ok(sids) = std::fs::read_dir(slug.path()) else { continue; };
        for sid in sids.flatten() {
            let sd = sid.path();
            if sd.join("workflows").exists() || sd.join("subagents").join("workflows").exists() {
                out.push(sd);
            }
        }
    }
    out
}

fn safe_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c=='_' || c=='.' || c=='-')
}

fn text_of(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() { return s.to_string(); }
    if let Some(arr) = content.as_array() {
        return arr.iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>().join("\n");
    }
    String::new()
}

fn round_ms(secs_f: f64) -> i64 { secs_f.round() as i64 }

fn mtime_ms(p: &Path) -> Option<i64> {
    let md = std::fs::metadata(p).ok()?;
    let mt = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(mt.as_millis() as i64)
}

fn run_name(dir: &Path, run_id: &str) -> String {
    let suffix = format!("-{run_id}.js");
    if let Ok(entries) = std::fs::read_dir(dir.join("workflows").join("scripts")) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(&suffix) { return n[..n.len()-suffix.len()].to_string(); }
        }
    }
    "workflow".to_string()
}

fn run_created_at(dir: &Path, run_id: &str, fallback: &Path) -> i64 {
    let suffix = format!("-{run_id}.js");
    if let Ok(entries) = std::fs::read_dir(dir.join("workflows").join("scripts")) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(&suffix) {
                if let Some(ms) = mtime_ms(&e.path()) { return ms; }
            }
        }
    }
    mtime_ms(fallback).unwrap_or(0)
}

fn read_running_run(dir: &Path, run_dir: &Path, run_id: &str) -> Option<WorkflowRun> {
    let mut ids: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(stripped) = n.strip_prefix("agent-").and_then(|s| s.strip_suffix(".meta.json")) {
                ids.push(stripped.to_string());
            }
        }
    } else { return None; }
    ids.sort();
    let mut done = std::collections::HashSet::new();
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let jf = run_dir.join("journal.jsonl");
    if let Ok(text) = std::fs::read_to_string(&jf) {
        for line in text.split('\n') {
            if line.trim().is_empty() { continue; }
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
            let id = o.get("agentId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id.is_empty() { continue; }
            if !ids.contains(&id) { ids.push(id.clone()); }
            if o.get("type").and_then(|t| t.as_str()) == Some("result") {
                done.insert(id.clone());
                if let Some(r) = o.get("result").and_then(|r| r.as_str()) { results.insert(id, r.to_string()); }
            }
        }
    }
    if ids.is_empty() { return None; }
    let agents = ids.iter().enumerate().map(|(i, id)| WorkflowAgent {
        agent_id: id.clone(),
        label: format!("agent {}", i + 1),
        state: if done.contains(id) { "done".into() } else { "running".into() },
        model: String::new(),
        result_preview: results.get(id).cloned(),
        ..Default::default()
    }).collect::<Vec<_>>();
    Some(WorkflowRun {
        run_id: run_id.to_string(), name: run_name(dir, run_id), status: "running".into(),
        summary: None, agent_count: Some(ids.len() as i64),
        created_at: run_created_at(dir, run_id, run_dir),
        phases: vec![], agents, logs: vec![],
    })
}

pub fn list_workflows(base: &Path) -> Vec<WorkflowRun> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in project_dirs(base) {
        let wf_dir = dir.join("workflows");
        if let Ok(entries) = std::fs::read_dir(&wf_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".json") { continue; }
                let Ok(text) = std::fs::read_to_string(e.path()) else { continue; };
                let Ok(d) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
                let run_id = d.get("runId").and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.trim_end_matches(".json").to_string());
                if seen.contains(&run_id) { continue; }
                seen.insert(run_id.clone());
                let agents = d.get("workflowProgress").and_then(|v| v.as_array()).map(|arr| arr.iter()
                    .filter(|a| a.get("type").and_then(|t| t.as_str()) == Some("workflow_agent"))
                    .map(|a| WorkflowAgent {
                        agent_id: a.get("agentId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        label: a.get("label").and_then(|v| v.as_str()).unwrap_or("agent").to_string(),
                        state: a.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        model: a.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        phase_title: a.get("phaseTitle").and_then(|v| v.as_str()).map(String::from),
                        prompt_preview: a.get("promptPreview").and_then(|v| v.as_str()).map(String::from),
                        result_preview: a.get("resultPreview").and_then(|v| v.as_str()).map(String::from),
                    }).collect::<Vec<_>>()).unwrap_or_default();
                let phases = d.get("phases").and_then(|v| v.as_array()).map(|arr| arr.iter().map(|p| WorkflowPhase {
                    title: p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    detail: p.get("detail").and_then(|v| v.as_str()).map(String::from),
                }).collect::<Vec<_>>()).unwrap_or_default();
                let created_at = d.get("createdAt").or_else(|| d.get("startedAt"))
                    .and_then(|v| v.as_i64()).filter(|&n| n != 0)
                    .unwrap_or_else(|| run_created_at(&dir, &run_id, &e.path()));
                out.push(WorkflowRun {
                    run_id,
                    name: d.get("workflowName").and_then(|v| v.as_str()).unwrap_or("workflow").to_string(),
                    status: d.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    summary: d.get("summary").and_then(|v| v.as_str()).map(String::from),
                    agent_count: d.get("agentCount").and_then(|v| v.as_i64()),
                    created_at, phases, agents,
                    logs: d.get("logs").and_then(|v| v.as_array()).map(|a| a.iter()
                        .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect())
                        .unwrap_or_default(),
                });
            }
        }
        let sub_dir = dir.join("subagents").join("workflows");
        if let Ok(entries) = std::fs::read_dir(&sub_dir) {
            for e in entries.flatten() {
                let run_id = e.file_name().to_string_lossy().into_owned();
                if seen.contains(&run_id) { continue; }
                if !e.path().is_dir() { continue; }
                if let Some(run) = read_running_run(&dir, &e.path(), &run_id) {
                    seen.insert(run_id);
                    out.push(run);
                }
            }
        }
    }
    out
}

pub fn read_workflow_agent(base: &Path, run_id: &str, agent_id: &str) -> String {
    if !safe_segment(run_id) || !safe_segment(agent_id) { return String::new(); }
    let mut found: Option<PathBuf> = None;
    for dir in project_dirs(base) {
        let cand = dir.join("subagents").join("workflows").join(run_id).join(format!("agent-{agent_id}.jsonl"));
        if cand.exists() { found = Some(cand); break; }
    }
    let Some(f) = found else { return String::new(); };
    let Ok(text) = std::fs::read_to_string(&f) else { return String::new(); };
    let (mut input, mut output): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for line in text.split('\n') {
        if line.trim().is_empty() { continue; }
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
        match o.get("type").and_then(|t| t.as_str()) {
            Some("user") => { let t = text_of(o.get("message").and_then(|m| m.get("content")).unwrap_or(&serde_json::Value::Null)); if !t.is_empty() { input.push(t); } }
            Some("assistant") => { let t = text_of(o.get("message").and_then(|m| m.get("content")).unwrap_or(&serde_json::Value::Null)); if !t.is_empty() { output.push(t); } }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if !input.is_empty() { parts.push(format!("**Task**\n\n{}", input.join("\n\n"))); }
    if !output.is_empty() { parts.push(format!("**Output**\n\n{}", output.join("\n\n"))); }
    parts.join("\n\n---\n\n")
}
```
Run: `cargo test workflows` → pass.

### Step 3 — wire routes (api/sessions.rs)
Replace the Phase 5 stub body of `workflows_route` (after the no-worktree guard) with the real payload, and add the agent route:
```rust
// inside workflows_route, after the no-worktree empty guard:
let base = std::path::Path::new(s.worktree_path.as_deref().unwrap()).join(".claude-config");
let runs = crate::workflows::list_workflows(&base);
Json(json!({ "workflows": runs })).into_response()
```
```rust
pub async fn workflow_agent_route(
    State(st): State<AppState>,
    Path((id, run_id, agent_id)): Path<(String, String, String)>,
) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Some(wt) = s.worktree_path.as_deref().filter(|p| !p.is_empty()) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let base = std::path::Path::new(wt).join(".claude-config");
    let transcript = crate::workflows::read_workflow_agent(&base, &run_id, &agent_id);
    Json(json!({ "transcript": transcript })).into_response()
}
```
Register:
```rust
.route("/api/sessions/{id}/workflows/{runId}/agents/{agentId}", get(sessions::workflow_agent_route))
```
Add `mod workflows;` to `main.rs`. Add a route test (seed a session with a worktree + a `.claude-config/projects/slug/sid/workflows/wf.json`, assert the route returns the run). The existing `workflows_no_worktree_returns_empty_not_500` test still passes. Run: `cargo test` → pass.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: workflows list + agent transcript routes (T5)"`

---

## T6 — repos (local+remote) + skills + routes

**Files:** `src/repos.rs` (+ `list_repos`, `list_remote_repos`), `src/skills.rs` (new), `src/api/misc.rs` (+ routes), `src/api/mod.rs` (register), `src/main.rs` (`mod skills;`).

**Interfaces:**
```rust
// repos.rs
pub fn list_repos(src_root: &std::path::Path) -> Vec<String>;
pub fn list_remote_repos(git_org: &str, gh_fn: Option<&dyn Fn(&str) -> Option<String>>) -> Vec<String>;
// skills.rs
#[derive(serde::Serialize, Clone, Debug)]
pub struct SkillInfo { pub name: String, pub description: String }
pub fn list_skills(skills_dir: &std::path::Path) -> Vec<SkillInfo>;
```

### Step 1 — failing tests
`repos.rs` add:
```rust
#[test]
fn list_repos_returns_sorted_git_dirs_only() {
    let src = tmp();
    std::fs::create_dir_all(src.join("zeta").join(".git")).unwrap();
    std::fs::create_dir_all(src.join("alpha").join(".git")).unwrap();
    std::fs::create_dir_all(src.join("notrepo")).unwrap(); // no .git
    assert_eq!(list_repos(&src), vec!["alpha".to_string(), "zeta".to_string()]);
    assert!(list_repos(&src.join("missing")).is_empty());
}

#[test]
fn list_remote_repos_parses_and_sorts_and_is_lenient() {
    let gh = |_org: &str| -> Option<String> { Some(r#"[{"name":"b"},{"name":"a"}]"#.to_string()) };
    assert_eq!(list_remote_repos("org", Some(&gh)), vec!["a".to_string(), "b".to_string()]);
    let bad = |_o: &str| -> Option<String> { None };
    assert!(list_remote_repos("org", Some(&bad)).is_empty());
}
```
`skills.rs`:
```rust
#[test]
fn lists_skills_from_frontmatter_sorted() {
    let dir = tmp();
    let make = |name: &str, fm: &str| {
        let d = dir.join(name); std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), fm).unwrap();
    };
    make("zskill", "---\nname: zed\ndescription: last one\n---\nbody");
    make("askill", "---\nname: alpha\ndescription: first\n---\nbody");
    std::fs::create_dir_all(dir.join("nomd")).unwrap(); // no SKILL.md → skipped
    let out = list_skills(&dir);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].name, "alpha");
    assert_eq!(out[0].description, "first");
    assert_eq!(out[1].name, "zed");
    assert!(list_skills(&dir.join("missing")).is_empty());
}
```
Run: `cargo test repos`, `cargo test skills` → fail.

### Step 2 — implement
```rust
// repos.rs
pub fn list_repos(src_root: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(src_root) else { return vec![]; };
    let mut out: Vec<String> = entries.flatten()
        .filter(|e| e.path().is_dir() && e.path().join(".git").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn default_gh_list(org: &str) -> Option<String> {
    let o = std::process::Command::new("gh")
        .args(["repo", "list", org, "--json", "name", "-L", "200"]).output().ok()?;
    if !o.status.success() { return None; }
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

pub fn list_remote_repos(git_org: &str, gh_fn: Option<&dyn Fn(&str) -> Option<String>>) -> Vec<String> {
    let raw = match gh_fn { Some(f) => f(git_org), None => default_gh_list(git_org) };
    let Some(raw) = raw else { return vec![]; };
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else { return vec![]; };
    let mut names: Vec<String> = arr.iter()
        .filter_map(|r| r.get("name").and_then(|n| n.as_str()).map(String::from)).collect();
    names.sort();
    names
}
```
```rust
// skills.rs
pub fn list_skills(skills_dir: &std::path::Path) -> Vec<SkillInfo> {
    let Ok(entries) = std::fs::read_dir(skills_dir) else { return vec![]; };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let md = e.path().join("SKILL.md");
        if !md.exists() { continue; }
        let Ok(text) = std::fs::read_to_string(&md) else { continue; };
        let entry_name = e.file_name().to_string_lossy().into_owned();
        let (mut name, mut description) = (entry_name.clone(), String::new());
        if let Some(fm) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) {
            for line in fm.0.lines() {
                if let Some(v) = line.strip_prefix("name:") { name = v.trim().to_string(); }
                else if let Some(v) = line.strip_prefix("description:") { description = v.trim().to_string(); }
            }
            if name.is_empty() { name = entry_name; }
        }
        out.push(SkillInfo { name, description });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
```
> **Gotcha (frontmatter parse):** the TS uses regex `^---\n([\s\S]*?)\n---` and `^name:\s*(.+)$`/`^description:\s*(.+)$` with the `m` flag. The Rust version above matches a leading `---\n ... \n---` block and scans lines; equivalent for the real SKILL.md format. If a description value itself spans `name:`-looking content the behavior could differ, but real frontmatter is single-line — acceptable parity.

### Step 3 — routes (api/misc.rs)
```rust
pub async fn repos_route(State(st): State<AppState>) -> Json<serde_json::Value> {
    let local = crate::repos::list_repos(&st.config.src_root);
    let remote = crate::repos::list_remote_repos(&st.config.git_org, None);
    Json(json!({ "local": local, "remote": remote }))
}
pub async fn skills_route(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(crate::skills::list_skills(&st.config.skills_dir)).unwrap())
}
```
Register `.route("/api/repos", get(misc::repos_route))` and `.route("/api/skills", get(misc::skills_route))`. (`skills_dir` is added to `Config` in T9 — if T9 isn't done yet, temporarily derive it as `config.claude_config_base.join("skills")`; T9 makes it a real field. Order T9 before this route wiring if you prefer; the helper functions + their tests don't need config.)
Add `mod skills;` to `main.rs`. Run `cargo test` → pass.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: repos local/remote + skills + routes (T6)"`

---

## T7 — groups store + GET/PUT routes

**Files:** `src/groups.rs` (new), `src/api/misc.rs` (+ routes), `src/api/mod.rs` (register), `src/main.rs` (`mod groups;`).

**Interfaces:**
```rust
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Group { pub name: String, pub repos: Vec<String>, pub skills: Vec<String> }
pub fn list_groups(path: &std::path::Path) -> Vec<Group>;
pub fn save_groups(path: &std::path::Path, groups: &serde_json::Value) -> std::io::Result<Vec<Group>>;
```

### Step 1 — failing test
```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp_file() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-grp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("groups.json")
    }
    #[test]
    fn missing_file_is_empty() { assert!(list_groups(&tmp_file()).is_empty()); }
    #[test]
    fn save_normalizes_dedupes_and_round_trips() {
        let p = tmp_file();
        let input = serde_json::json!([
            {"name":"  a  ","repos":["r1", 7],"skills":["s1"]},
            {"name":"","repos":[]},                       // dropped (empty name)
            {"name":"a","repos":["r2"]},                  // dedupe by name (last wins)
            {"repos":["x"]}                               // dropped (no name)
        ]);
        let saved = save_groups(&p, &input).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "a");
        assert_eq!(saved[0].repos, vec!["r2".to_string()]);
        let listed = list_groups(&p);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "a");
    }
    #[test]
    fn malformed_file_is_empty() {
        let p = tmp_file();
        std::fs::write(&p, "{not an array}").unwrap();
        assert!(list_groups(&p).is_empty());
    }
}
```
Run: `cargo test groups` → fails.

### Step 2 — implement
```rust
use crate::atomic_write::write_file_atomic;

fn normalize(g: &serde_json::Value) -> Option<Group> {
    let name = g.get("name").and_then(|n| n.as_str())?.trim().to_string();
    if name.is_empty() { return None; }
    let str_vec = |key: &str| g.get(key).and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Some(Group { name, repos: str_vec("repos"), skills: str_vec("skills") })
}

pub fn list_groups(path: &std::path::Path) -> Vec<Group> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![]; };
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };
    arr.iter().filter_map(normalize).collect()
}

pub fn save_groups(path: &std::path::Path, groups: &serde_json::Value) -> std::io::Result<Vec<Group>> {
    let mut by_name: indexmap::IndexMap<String, Group> = indexmap::IndexMap::new();
    if let Some(arr) = groups.as_array() {
        for g in arr { if let Some(n) = normalize(g) { by_name.insert(n.name.clone(), n); } }
    }
    let clean: Vec<Group> = by_name.into_values().collect();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    write_file_atomic(path, &serde_json::to_string_pretty(&clean).unwrap())?;
    Ok(clean)
}
```
> **Dependency note:** `indexmap` preserves insertion order for the dedupe-last-wins map (matching TS `Map`). Add `indexmap = "2"` to `[dependencies]`. (A plain `HashMap` + a separate order `Vec` also works if you want to avoid the dep; `indexmap` is the clean parity.) For dedupe-last-wins with `IndexMap`, `insert` updates the value but keeps the **first** position — which matches the TS `Map.set` (TS Map keeps first insertion order, updates value). Verified parity.

### Step 3 — routes (api/misc.rs)
```rust
pub async fn groups_get(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(crate::groups::list_groups(&st.config.groups_path)).unwrap())
}
pub async fn groups_put(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if !v.is_array() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"array of groups required"}))).into_response();
    }
    match crate::groups::save_groups(&st.config.groups_path, &v) {
        Ok(g) => Json(serde_json::to_value(g).unwrap()).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}
```
Register `.route("/api/groups", get(misc::groups_get).put(misc::groups_put))`. Add `mod groups;` to `main.rs`. Run `cargo test` → pass.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: groups store + routes (T7)"`

---

## T8 — templates store + resolve_prompt + GET/PUT/start routes

**Files:** `src/templates.rs` (new), `src/api/misc.rs` (+ routes), `src/api/mod.rs` (register), `src/main.rs` (`mod templates;`).

**Interfaces:**
```rust
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Template {
    pub name: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    #[serde(rename = "promptBody")] pub prompt_body: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub vars: Vec<String>,
}
pub fn list_templates(path: &std::path::Path) -> Vec<Template>;
pub fn save_templates(path: &std::path::Path, templates: &serde_json::Value) -> std::io::Result<Vec<Template>>;
pub fn resolve_prompt(prompt_body: &str, vars: &std::collections::HashMap<String, String>) -> String;
```

### Step 1 — failing test
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn tmp_file() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-tpl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("templates.json")
    }
    #[test]
    fn save_drops_invalid_and_round_trips() {
        let p = tmp_file();
        let input = serde_json::json!([
            {"name":"t1","promptBody":"hi {{x}}","model":"opus"},
            {"name":"t1","promptBody":"newer"},          // dedupe last wins
            {"name":"bad"},                              // no promptBody → dropped
            {"promptBody":"no name"}                     // no name → dropped
        ]);
        let saved = save_templates(&p, &input).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].prompt_body, "newer");
        assert_eq!(list_templates(&p).len(), 1);
    }
    #[test]
    fn resolve_substitutes_known_and_keeps_unknown() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Zhefu".to_string());
        assert_eq!(resolve_prompt("hi {{name}}, {{missing}}", &vars), "hi Zhefu, {{missing}}");
    }
}
```
Run: `cargo test templates` → fails.

### Step 2 — implement
```rust
use crate::atomic_write::write_file_atomic;

fn normalize(t: &serde_json::Value) -> Option<Template> {
    let name = t.get("name").and_then(|n| n.as_str())?.trim().to_string();
    if name.is_empty() { return None; }
    let prompt_body = t.get("promptBody").and_then(|p| p.as_str())?.to_string();
    let str_vec = |key: &str| t.get(key).and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let opt_str = |key: &str| t.get(key).and_then(|v| v.as_str()).map(String::from);
    Some(Template {
        name, repos: str_vec("repos"), skills: str_vec("skills"), prompt_body,
        model: opt_str("model"), effort: opt_str("effort"), mode: opt_str("mode"),
        vars: str_vec("vars"),
    })
}

pub fn list_templates(path: &std::path::Path) -> Vec<Template> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![]; };
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };
    arr.iter().filter_map(normalize).collect()
}

pub fn save_templates(path: &std::path::Path, templates: &serde_json::Value) -> std::io::Result<Vec<Template>> {
    let mut by_name: indexmap::IndexMap<String, Template> = indexmap::IndexMap::new();
    if let Some(arr) = templates.as_array() {
        for t in arr { if let Some(n) = normalize(t) { by_name.insert(n.name.clone(), n); } }
    }
    let clean: Vec<Template> = by_name.into_values().collect();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    write_file_atomic(path, &serde_json::to_string_pretty(&clean).unwrap())?;
    Ok(clean)
}

pub fn resolve_prompt(prompt_body: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let re = regex::Regex::new(r"\{\{(\w+)\}\}").unwrap();
    re.replace_all(prompt_body, |c: &regex::Captures| {
        let name = &c[1];
        match vars.get(name) { Some(v) => v.clone(), None => format!("{{{{{name}}}}}") }
    }).into_owned()
}
```

### Step 3 — routes (api/misc.rs)
```rust
pub async fn templates_get(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(crate::templates::list_templates(&st.config.templates_path)).unwrap())
}
pub async fn templates_put(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if !v.is_array() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"array of templates required"}))).into_response();
    }
    match crate::templates::save_templates(&st.config.templates_path, &v) {
        Ok(t) => Json(serde_json::to_value(t).unwrap()).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct TemplateStartBody {
    pub name: Option<String>,
    pub vars: Option<std::collections::HashMap<String, String>>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
}
pub async fn templates_start(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: TemplateStartBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let Some(name) = b.name.filter(|n| !n.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"name required"}))).into_response();
    };
    let templates = crate::templates::list_templates(&st.config.templates_path);
    let Some(tpl) = templates.into_iter().find(|t| t.name == name) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("template '{name}' not found")}))).into_response();
    };
    let prompt = crate::templates::resolve_prompt(&tpl.prompt_body, &b.vars.unwrap_or_default());
    let meta = crate::engine::SubmitMeta {
        model: b.model.or(tpl.model),
        effort: b.effort.or(tpl.effort),
        mode: b.mode.or(tpl.mode),
    };
    match st.engine.submit_session(tpl.repos, tpl.skills, prompt, std::collections::HashMap::new(), meta) {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}
```
Register:
```rust
.route("/api/templates", get(misc::templates_get).put(misc::templates_put))
.route("/api/templates/start", post(misc::templates_start))
```
Add `mod templates;` to `main.rs`. Add a route test: `templates_start` with no name → 400; unknown name → 404. Run `cargo test` → pass.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: templates store + start route (T8)"`

---

## T9 — config additions + `/api/devices` route

**Files:** `src/config.rs` (+ fields), `src/api/misc.rs` (+ devices route), `src/api/mod.rs` (register).

### Step 1 — failing test (config.rs)
```rust
#[test]
fn phase6_path_defaults_and_overrides() {
    let c = Config::load(env_of(&[("HOME", "/home/u")]));
    assert_eq!(c.skills_dir.to_str().unwrap(), "/home/u/.claude/skills");
    assert_eq!(c.groups_path.to_str().unwrap(), "/home/u/.agentic-dev/groups.json");
    assert_eq!(c.templates_path.to_str().unwrap(), "/home/u/.agentic-dev/templates.json");
    assert_eq!(c.device_token_path.to_str().unwrap(), "/home/u/.agentic-dev/device.json");
    assert_eq!(c.upload_max_bytes, 64 * 1024 * 1024);
    let c2 = Config::load(env_of(&[("HOME","/home/u"),("AGENTIC_UPLOAD_MAX_BYTES","10")]));
    assert_eq!(c2.upload_max_bytes, 10);
}
```
Run: `cargo test config` → fails (no such fields).

### Step 2 — implement (config.rs)
Add to `Config`:
```rust
pub skills_dir: PathBuf,
pub groups_path: PathBuf,
pub templates_path: PathBuf,
pub device_token_path: PathBuf,
pub upload_max_bytes: usize,
```
In `load`, after computing `data_dir`:
```rust
skills_dir: get("AGENTIC_SKILLS_DIR").map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from(&home).join(".claude").join("skills")),
groups_path: get("AGENTIC_GROUPS_PATH").map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from(&data_dir).join("groups.json")),
templates_path: get("AGENTIC_TEMPLATES_PATH").map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from(&data_dir).join("templates.json")),
device_token_path: get("AGENTIC_DEVICE_TOKEN_PATH").map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from(&data_dir).join("device.json")),
upload_max_bytes: get("AGENTIC_UPLOAD_MAX_BYTES").and_then(|v| v.parse().ok())
    .unwrap_or(64 * 1024 * 1024),
```
> Now that `skills_dir` exists, change the T6 `skills_route` to use `st.config.skills_dir`.

### Step 3 — devices route (api/misc.rs)
```rust
#[derive(serde::Deserialize, Default)]
pub struct DeviceBody { pub token: Option<String> }
pub async fn devices_post(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let b: DeviceBody = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let token = b.token.unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"token required"}))).into_response();
    }
    match crate::push::save_device_token(&st.config.device_token_path, token) {
        Ok(rec) => Json(json!({ "ok": true, "registeredAt": rec.registered_at })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}
```
Register `.route("/api/devices", post(misc::devices_post))`. Add a route test: no token → 400; valid token → `{ok:true, registeredAt:<n>}` and `load_device_token` round-trips. Run `cargo test` → pass.

### Step 4 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: config paths + /api/devices (T9)"`

---

## T10 — file / upload / outbox routes

**Files:** `src/api/sessions.rs` (+ helpers + 3 routes), `src/api/mod.rs` (register), `Cargo.toml` (axum `multipart` feature — added in T2).

**Interfaces (private helpers in sessions.rs):**
```rust
fn session_cwd(s: &crate::store::Session) -> std::path::PathBuf;
fn safe_path(s: &crate::store::Session, rel: &str) -> Option<std::path::PathBuf>;
fn mime_for(path: &std::path::Path) -> &'static str;
```

### Step 1 — failing test
```rust
#[tokio::test]
async fn file_route_serves_a_worktree_file_with_length_and_mime() {
    let st = test_state().await;
    let id = "s-file";
    let wt = st.config.worktrees_root.join(id);
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("hello.txt"), "hi there").unwrap();
    st.store.create(crate::store::CreateInput {
        id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
        worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
        ..Default::default()
    }).await.unwrap();
    let resp = crate::api::app(st.clone()).oneshot(
        Request::get(format!("/api/sessions/{id}/file?path=hello.txt"))
            .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
    assert_eq!(resp.headers().get("content-length").unwrap(), "8");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"hi there");
}

#[tokio::test]
async fn file_route_rejects_escape_and_missing() {
    let st = test_state().await;
    let id = "s-esc";
    let wt = st.config.worktrees_root.join(id);
    std::fs::create_dir_all(&wt).unwrap();
    st.store.create(crate::store::CreateInput {
        id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
        worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
        ..Default::default()
    }).await.unwrap();
    // missing path param → 400
    let (s, b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/file"))
        .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b, json!({"error":"path required"}));
    // escape attempt → 404
    let (s2, _b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/file?path=../../etc/passwd"))
        .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
    assert_eq!(s2, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn outbox_lists_floored_mtimes() {
    let st = test_state().await;
    let id = "s-ob";
    let wt = st.config.worktrees_root.join(id);
    let ob = wt.join("outbox");
    std::fs::create_dir_all(&ob).unwrap();
    std::fs::write(ob.join("report.md"), "x").unwrap();
    st.store.create(crate::store::CreateInput {
        id: id.into(), repos: vec![], skills: vec![], prompt: "p".into(),
        worktree_path: Some(wt.to_string_lossy().into_owned()), branch: Some("b".into()),
        ..Default::default()
    }).await.unwrap();
    let (s, b) = oneshot_req(st.clone(), Request::get(format!("/api/sessions/{id}/outbox"))
        .header("authorization", auth(&st)).body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    let files = b["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "outbox/report.md");
    assert_eq!(files[0]["name"], "report.md");
    assert!(files[0]["mtime"].is_i64());
}
```
Run: `cargo test sessions` → fails.

### Step 2 — implement helpers + routes (sessions.rs)
```rust
use crate::store::Session;

fn session_cwd(s: &Session) -> std::path::PathBuf {
    let root = std::path::PathBuf::from(s.worktree_path.clone().unwrap_or_default());
    if s.repos.len() == 1 { root.join(&s.repos[0]) } else { root }
}

fn safe_path(s: &Session, rel: &str) -> Option<std::path::PathBuf> {
    let wt = s.worktree_path.as_deref()?;
    let base = std::fs::canonicalize(wt).ok()?;
    let candidate = session_cwd(s).join(rel);
    let full = std::fs::canonicalize(&candidate).ok()?;  // needs the file to exist
    if full == base || full.starts_with(&base) { Some(full) } else { None }
}

const MIME_TABLE: &[(&str, &str)] = &[
    ("png","image/png"),("jpg","image/jpeg"),("jpeg","image/jpeg"),("gif","image/gif"),
    ("webp","image/webp"),("bmp","image/bmp"),("svg","image/svg+xml"),("pdf","application/pdf"),
    ("txt","text/plain"),("md","text/markdown"),("json","application/json"),("csv","text/csv"),
    ("html","text/html"),("mp4","video/mp4"),("webm","video/webm"),("mp3","audio/mpeg"),("wav","audio/wav"),
];
fn mime_for(path: &std::path::Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).unwrap_or_default();
    MIME_TABLE.iter().find(|(k, _)| *k == ext).map(|(_, v)| *v).unwrap_or("application/octet-stream")
}

#[derive(Deserialize, Default)]
pub struct FileQuery { pub path: Option<String> }

pub async fn file_route(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<FileQuery>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Some(rel) = q.path.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"path required"}))).into_response();
    };
    let Some(full) = safe_path(&s, &rel) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Ok(meta) = std::fs::metadata(&full) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    if !meta.is_file() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    }
    let Ok(bytes) = tokio::fs::read(&full).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let filename = full.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    use axum::http::header;
    axum::response::Response::builder()
        .header(header::CONTENT_TYPE, mime_for(&full))
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{filename}\""))
        .header(header::CONTENT_LENGTH, meta.len())
        .body(Body::from(bytes)).unwrap()
}

pub async fn upload_route(State(st): State<AppState>, Path(id): Path<String>, mut multipart: axum::extract::Multipart) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    if s.worktree_path.as_deref().unwrap_or("").is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"session has no worktree"}))).into_response();
    }
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f, _ => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    let raw_name = field.file_name().unwrap_or("upload").to_string();
    let stem = std::path::Path::new(&raw_name).file_name().and_then(|n| n.to_str()).unwrap_or("upload");
    let re = regex::Regex::new(r"[^\w.\-]+").unwrap();
    let mut safe = re.replace_all(stem, "_").into_owned();
    if safe.is_empty() { safe = "upload".into(); }
    let dir = session_cwd(&s).join("uploads");
    if std::fs::create_dir_all(&dir).is_err() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cannot create uploads dir"}))).into_response();
    }
    let dest = dir.join(&safe);
    let data = match field.bytes().await {
        Ok(b) => b, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no file"}))).into_response(),
    };
    if data.len() as u64 > st.config.upload_max_bytes as u64 {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error":"file too large"}))).into_response();
    }
    if tokio::fs::write(&dest, &data).await.is_err() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"write failed"}))).into_response();
    }
    Json(json!({ "path": format!("uploads/{safe}") })).into_response()
}

pub async fn outbox_route(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(s) = st.engine.get(&id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let Some(wt) = s.worktree_path.as_deref().filter(|p| !p.is_empty()) else {
        return Json(json!({"files": []})).into_response();
    };
    let mut dirs: Vec<(std::path::PathBuf, String)> = vec![(session_cwd(&s).join("outbox"), "outbox".to_string())];
    if s.repos.len() > 1 {
        for repo in &s.repos {
            dirs.push((std::path::Path::new(wt).join(repo).join("outbox"), format!("{repo}/outbox")));
        }
    }
    let mut files = Vec::new();
    for (dir, rel) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue; };
        for e in entries.flatten() {
            if !e.path().is_file() { continue; }
            let name = e.file_name().to_string_lossy().into_owned();
            let mtime = std::fs::metadata(e.path()).ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)   // already integer ms (floor)
                .unwrap_or(0);
            files.push(json!({ "path": format!("{rel}/{name}"), "name": name, "mtime": mtime }));
        }
    }
    Json(json!({ "files": files })).into_response()
}
```
Register:
```rust
.route("/api/sessions/{id}/file", get(sessions::file_route))
.route("/api/sessions/{id}/upload", post(sessions::upload_route))
.route("/api/sessions/{id}/outbox", get(sessions::outbox_route))
```
> **Gotchas:**
> - `Body` is `axum::body::Body` — `use axum::body::Body;` at the top of sessions.rs (already imported via `Bytes` — add `Body`).
> - The TS streams the file (`createReadStream`); reading the whole file into memory here is fine for the image/preview sizes the client downloads, and the **`content-length` header is the load-bearing parity detail** (the Ktor client hangs without it). Keep it. For very large files a future task can stream via `tokio_util::io::ReaderStream`; not needed for parity now.
> - `safe_path` uses `canonicalize` (= realpath) which requires the path to exist — matches the TS `realpathSync` "only call once the file exists" rule. Missing file → `None` → 404.
> - axum multipart needs the `multipart` feature on the `axum` dep (added in T2). The over-cap check here is post-read (TS streams + truncates); for parity on the error *response* (413 + `file too large`) this is equivalent — only the partial-file-cleanup detail differs (we never wrote it). Acceptable.

Run `cargo test` → pass.

### Step 3 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: file/upload/outbox routes (T10)"`

---

## T11 — engine push hook + main.rs wiring (end-to-end)

**Files:** `src/engine.rs` (fire push on exit), `src/main.rs` (install `push_fn` closure + usage seam), `src/api/mod.rs`/`test_support.rs` (thread new AppState fields — already done incrementally; this task confirms `cargo test` is green end-to-end and adds the engine-exit push test).

**Push hook design (parity with `push.test.ts` "Engine push on session exit"):**
The Rust engine holds `push_fn: Option<PushFn>` where `PushFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>`. The engine builds the **PushPayload JSON** at exit and calls `push_fn(payload_json)`. The closure installed in `main.rs` loads the device token + creds and calls `send_push` (spawned, fire-and-forget). Tests inject a `push_fn` that records the payload — mirroring TS `pushSendFn`.

### Step 1 — failing test (engine.rs tests)
First extend the existing test harness so a `push_fn` can be injected. In `engine.rs` `mod tests`, add a field to `EngineOverrides` and thread it through `make_engine`:
```rust
// in struct EngineOverrides { ... }
    push_fn: Option<PushFn>,
// in make_engine's EngineConfig { ... } replace `push_fn: None,` with:
    push_fn: overrides.push_fn,
```
Then add the test (reusing `make_engine`, `tmp`, and the existing `wait_for_status`-style loop already present in the engine tests):
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fires_push_fn_on_session_exit() {
    use std::sync::{Arc, Mutex};
    let src = tmp().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let b = bodies.clone();
    let push_fn: PushFn = Arc::new(move |v| b.lock().unwrap().push(v));
    let engine = make_engine(&src, EngineOverrides {
        max_concurrent: Some(1),
        push_fn: Some(push_fn),
        ..Default::default()
    }).await;
    // repos empty → pure-skill scratch session, runs immediately under the fake-claude fixture.
    let id = engine.submit_session(vec![], vec![], "go".into(),
        std::collections::HashMap::new(), SubmitMeta::default()).unwrap();
    // Wait for the session to reach "done" (poll the store like the other engine tests).
    for _ in 0..250 {
        if engine.get(&id).await.map(|s| s.status) == Some("done".into()) { break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Give the fire-and-forget push a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let got = bodies.lock().unwrap();
    assert!(!got.is_empty(), "push_fn must fire on exit");
    assert_eq!(got[0]["sessionId"], id);
    assert_eq!(got[0]["status"], "done");
}
```
> `SubmitMeta` and `PushFn` are in scope inside `mod tests` via the `use super::*;` at the top of the test module. `submit_session`'s 4th arg is the env `HashMap` (use `std::collections::HashMap::new()`), 5th is `SubmitMeta`. Run: `cargo test fires_push_fn` → fails (the hook is still commented out).

### Step 2 — implement the hook (engine.rs)
Replace the commented-out hook near the `engineExit` emit (currently `// Phase 6: push_fn hook ...`) with:
```rust
// Phase 6: fire the finish-line push hook. The closure (installed in main.rs) loads the
// device token + creds and sends FCM; tests inject a recorder. Fire-and-forget — must not
// block re-pump. We pass the PushPayload as JSON (parity with TS pushSendFn body shape).
if let Some(ref push_fn) = self.0.cfg.push_fn {
    let cost = cur.cost_usd;
    let payload = serde_json::json!({
        "sessionId": id,
        "status": status,
        "isError": status != "done",
        "errorText": cur.error,
        "costUsd": cost,
    });
    push_fn(payload);
}
```
> Note: `cur` is the pre-exit session row; `status` is the final status computed above; `cur.error`/`cur.cost_usd` are the finalized values. This matches the TS call site (`isError: status !== "done"`, `errorText: s.error`, `costUsd: s.costUsd`).

### Step 3 — main.rs: install the real push_fn + usage seam
In `main.rs`, before building `EngineConfig`, build the push closure:
```rust
let device_token_path = config.device_token_path.clone();
let push_fn: Option<crate::engine::PushFn> = Some(std::sync::Arc::new(move |payload: serde_json::Value| {
    let device_token_path = device_token_path.clone();
    // Load token + creds fresh each fire (parity with TS pushDeviceTokenFn/pushCredsFn closures).
    let token = crate::push::load_device_token(&device_token_path).map(|r| r.token);
    let creds = crate::push::load_fcm_creds(|k| std::env::var(k).ok());
    let pp = crate::push::PushPayload {
        session_id: payload.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        status: payload.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        is_error: payload.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
        error_text: payload.get("errorText").and_then(|v| v.as_str()).map(String::from),
        cost_usd: payload.get("costUsd").and_then(|v| v.as_f64()),
    };
    // Fire-and-forget on the tokio runtime; failures are swallowed inside send_push.
    tokio::spawn(async move {
        crate::push::send_push(&pp, token.as_deref(), creds.as_ref(), None).await;
    });
}));
```
Set `push_fn` in the `EngineConfig` (replace the `push_fn: None` line). Init the usage cache + (None) usage_fn in `AppState`:
```rust
usage_cache: Arc::new(Mutex::new(state::UsageCache::default())),
usage_inflight: Arc::new(tokio::sync::Mutex::new(())),
usage_fn: None,
```
> `mod` additions to `main.rs` (collect them): `atomic_write`, `push`, `usage`, `structured_diff`, `workflows`, `skills`, `groups`, `templates`.

### Step 4 — green the whole suite
Update `test_support.rs` + `api/mod.rs::test_state` + the engine's `push.test`-style test harness to construct the new `AppState` fields (`usage_cache`, `usage_inflight`, `usage_fn: None`). Run the full `cargo test` — everything green.

### Step 5 — commit
`git -C <repo> add -A && git -C <repo> commit -m "rust phase6: engine push hook + main.rs wiring (T11)"`

---

## Done-criteria (this phase)

`cargo test` green with new coverage for: atomic write; FCM device store + send_push no-op rules + body shape; usage fetch + cache (fresh/stale/503/single-flight); commit graph + files (real sha + working + untracked); workflows list (summary + live journal + dedupe) + agent transcript + traversal rejection; repos local/remote; skills; groups + templates normalize/dedupe + resolve_prompt; `/api/repos`, `/api/skills`, `/api/usage`, `/api/groups`, `/api/devices`, `/api/templates`, `/api/templates/start`, `/api/sessions/:id/file|upload|outbox|commits|commits/:sha/files|workflows|workflows/:runId/agents/:agentId`; engine fires push on exit. Every route the Android client uses is now served by the Rust backend.

Deferred to Phase 7: cross-impl conformance suite + real-state shadow run + systemd cutover/rollback runbook.
