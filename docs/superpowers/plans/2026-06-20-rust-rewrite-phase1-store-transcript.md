# Rust Rewrite — Phase 1 (Store + Incremental Transcript Model) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `server-rs/`, add (1) an async sqlx `Store` at byte-for-byte parity with `server/engine/store.ts` (open an existing DB, same schema/migration/ordering, same `rowToSession` fallbacks) and (2) the in-memory incremental rendered-transcript model (`RenderedProjection` + `TranscriptCache`) — the head-of-line-blocking fix — with no HTTP wiring yet. Endpoints land in Phase 5.

**Architecture:** `store.rs` opens the existing sqlite DB (same schema/migration/ordering/fallbacks) and exposes async CRUD + log append. `transcript.rs` holds a per-session append-only **rendered** projection with its own file byte-cursor (incremental `sync`; cold full read via `spawn_blocking`; byte-correct UTF-8 carry; file-rotation reset), serving O(1) cursor / O(delta) catch-up / O(window) reads, behind an LRU `TranscriptCache`. `AppState` carries both.

**Tech Stack:** Rust (edition 2021), tokio 1 (`full`), sqlx 0.8 (`runtime-tokio` + `sqlite`, default-features off; **runtime `query`/`query_as` — NO compile-time `query!` macros** to avoid live-DB-at-build-time friction), serde/serde_json 1, thiserror 1.

**Parity bar:** Behavioral parity with the TS reference (`server/engine/store.ts`, `server/api/renderedLog.ts`, `server/engine/types.ts`) is the bar. A Rust process must open a DB created by the TS server and round-trip a session unchanged; the rendered projection must equal `filterRendered(readLog)` for any fixture log so existing clients' rendered-coordinate cursors stay correct.

## Global Constraints

Copy these exact parity values/field names verbatim into the implementation; do not paraphrase.

- **Crate:** `agentic-dev/server-rs/`. Run all `cargo` commands from there. **Commit only — NEVER push.** End every commit message with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **sqlite pragmas (verbatim, in this order):** `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = NORMAL`, `PRAGMA busy_timeout = 5000`. Open URL `sqlite://<db_path>?mode=rwc`, `max_connections(1)` (single writer, matches the TS `better-sqlite3` single connection).
- **Initial schema `COLUMNS` (verbatim from `store.ts`, plus `seq INTEGER`):**
  `id TEXT PRIMARY KEY, repo TEXT, prompt TEXT, worktreePath TEXT, branch TEXT, claudeSessionId TEXT, status TEXT, costUsd REAL, exitCode INTEGER, error TEXT, errorKind TEXT, createdAt INTEGER, startedAt INTEGER, endedAt INTEGER, baseSha TEXT, worktreeState TEXT, repos TEXT, skills TEXT, baseShas TEXT, model TEXT, effort TEXT, mode TEXT, seq INTEGER`
- **`ADDED_COLUMNS` (verbatim name+decl, idempotent ALTER loop):**
  `("baseSha","TEXT"), ("worktreeState","TEXT DEFAULT 'live'"), ("repos","TEXT"), ("skills","TEXT"), ("baseShas","TEXT"), ("model","TEXT"), ("effort","TEXT"), ("mode","TEXT"), ("errorKind","TEXT"), ("lastUserMessageAt","INTEGER")`
- **Migration backfill (verbatim):** `UPDATE sessions SET lastUserMessageAt = createdAt WHERE lastUserMessageAt IS NULL`.
- **`create()` parity (`store.ts` lines 86-110):** `repos = input.repos ?? (input.repo ? [input.repo] : [])`; `repo = repos[0] ?? ""` (the `repo` **column** is `""` when empty, **not NULL**); `baseShas = input.baseShas ?? (input.repo ? {[input.repo]: input.baseSha ?? null} : {})`; `baseSha = repo in baseShas ? baseShas[repo] : (input.baseSha ?? null)`; `createdAt = lastUserMessageAt = Date.now()`; `status = "pending"`; `worktreeState = "live"`; `mode` is stored **RAW** (no normalization on write); `seq` is a per-`Store` counter (`this.seq++`).
- **`list()` ordering (verbatim):** `SELECT * FROM sessions ORDER BY COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC`.
- **`rowToSession` fallbacks (verbatim from `store.ts` lines 156-175):**
  - `repos = safeJson(r.repos, r.repo ? [r.repo] : [])` — empty-string `r.repo` is falsy → `[]` not `[""]`.
  - `skills = safeJson(r.skills, [])`.
  - `baseShas = safeJson(r.baseShas, r.repo && r.baseSha != null ? {[r.repo]: r.baseSha} : {})`.
  - `mode = normalizeMode(r.mode)` — normalization happens on **READ** (`"ultracode"|"ultra" → "ultracode"`, else `null`).
  - `lastUserMessageAt = r.lastUserMessageAt ?? r.createdAt`.
  - `baseSha = r.baseSha ?? (repos[0] != null ? baseShas[repos[0]] ?? null : null)`.
  - `worktreeState = r.worktreeState ?? "live"`.
- **`safeJson` parity:** malformed/empty JSON in a TEXT column degrades to the documented fallback, **never panics**.
- **`now_ms()` parity:** `Date.now()` (ms since epoch); must **never panic** (`.unwrap_or_default()`, not `.unwrap()`).
- **`appendLog` parity:** append `line + "\n"` to `<logDir>/<id>.jsonl`; the blocking write runs in `spawn_blocking` so it never blocks the async executor.
- **Rendered filter — single source of truth (`renderedLog.ts` line 7, verbatim prefixes):** keep lines starting with exactly `{"type":"agentic_prompt"`, `{"type":"stream_event"`, `{"type":"assistant"`, `{"type":"result"`; drop all others. `filter_rendered` falls back to the **raw** lines when filtering yields nothing.
- **Cursor stays in rendered coordinates:** `RenderedProjection::count()` == number of rendered lines == the `since` the client counts. **Equivalence invariant (mandatory test):** `projection.rendered == filter_rendered(complete_lines_of_file)`.
- **Byte/UTF-8 discipline:** the partial-line carry is `Vec<u8>` (raw bytes); decode to `&str` only at `b'\n'` boundaries — never mid-character — so a multibyte char (CJK, emoji) split across a read boundary is never corrupted. A partial final line (no trailing `\n`) is held in the carry and excluded from `rendered`; it converges on the next `sync`. `byte_offset` advances by **bytes actually read** (`buf.len()`), never by a metadata snapshot. File shrink (rotation) resets the projection and re-reads from byte 0.
- **CPU discipline:** the cold/whole-file read inside `sync` runs in `tokio::task::spawn_blocking`; incremental new-byte reads run inline. No CPU-heavy work on the async executor.
- **Cache:** `AGENTIC_TRANSCRIPT_CACHE_BYTES` env, default `256 * 1024 * 1024` (256 MiB); LRU eviction by summed `bytes()`; never evict the just-inserted id.
- **Out of scope (later phases):** HTTP endpoints for the windowed transcript (Phase 5); engine/spawn/stream/queue/watchdog/workflows/push (Phases 3-6); `rawToRenderedOffset` (the projection's O(1) `count()` replaces it). All `cargo test` green before each commit.

## File Structure

- `server-rs/Cargo.toml` — deps already include `sqlx` + `thiserror`; no change needed in Phase 1.
- `server-rs/src/store.rs` — `Store`, `Session`, `CreateInput`, `SessionPatch`, `StoreError`, `row_to_session`, `now_ms`, `normalize_mode`, `safe_json_vec`, `safe_json_map`.
- `server-rs/src/transcript.rs` — `is_rendered`, `filter_rendered`, `RenderedProjection`, `Window`, `TranscriptCache`.
- `server-rs/src/config.rs` — `transcript_cache_bytes` field + `AGENTIC_TRANSCRIPT_CACHE_BYTES`.
- `server-rs/src/state.rs` — `AppState` gains `store: Arc<Store>` + `transcript: Arc<TranscriptCache>`.
- `server-rs/src/main.rs` — `mod store; mod transcript;`; build `Store` + cache into state.
- `server-rs/src/api/mod.rs` — async `test_state()` builds a `Store` + `TranscriptCache` (test-only change).

---

### Task 1: `Store` — sqlite parity (schema, migration, CRUD, log append)

**Files:**
- Modify: `server-rs/Cargo.toml` (confirm `sqlx` + `thiserror` deps), `server-rs/src/main.rs` (add `mod store;`)
- Create: `server-rs/src/store.rs`
- Test: inline `#[cfg(test)]` in `src/store.rs`

**Interfaces (exact Rust signatures):**
```rust
pub enum StoreError { Sqlx(sqlx::Error), Io(std::io::Error) }      // thiserror

pub struct Session { /* serde-renamed fields mirroring types.ts — see Step 4 */ }
pub struct CreateInput { /* id, prompt, repos, skills, worktree_path, branch, model, effort, mode, repo, base_sha, base_shas */ }
pub struct SessionPatch { /* Option-wrapped updatable fields */ }

impl Store {
    pub async fn open(db_path: impl AsRef<Path>, log_dir: impl AsRef<Path>) -> Result<Store, StoreError>;
    pub fn log_path(&self, id: &str) -> PathBuf;
    pub async fn append_log(&self, id: &str, line: &str) -> Result<(), StoreError>;
    pub async fn create(&self, input: CreateInput) -> Result<Session, StoreError>;
    pub async fn get(&self, id: &str) -> Result<Option<Session>, StoreError>;
    pub async fn list(&self) -> Result<Vec<Session>, StoreError>;
    pub async fn update(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError>;
}
fn row_to_session(r: &SqliteRow) -> Session;
```

- [ ] **Step 1: Confirm deps in `Cargo.toml`** (already present; verify exactly):
```toml
sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio", "sqlite"] }
thiserror = "1"
```

- [ ] **Step 2: Write failing tests in `src/store.rs`** (mirror the `store.test.ts` intents: roundtrip + `lastUserMessageAt == createdAt`, list ordering, legacy-DB migration+backfill, append-log).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AO};

    // Per-call unique counter: the Rust test harness runs tests concurrently in-process, so a
    // pid-only temp path collides (one test's remove_dir_all wipes another mid-run).
    static CTR: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> PathBuf {
        let n = CTR.fetch_add(1, AO::SeqCst);
        let p = std::env::temp_dir().join(format!("agentic-store-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn create_get_roundtrip_and_lastusermessageat_eq_createdat() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let s = store.create(CreateInput { id: "s1".into(), repos: vec!["demo".into()], prompt: "do x".into(), ..Default::default() }).await.unwrap();
        assert_eq!(s.last_user_message_at, s.created_at);
        let got = store.get("s1").await.unwrap().unwrap();
        assert_eq!(got.id, "s1");
        assert_eq!(got.repos, vec!["demo".to_string()]);
        assert_eq!(got.status, "pending");
    }

    #[tokio::test]
    async fn list_orders_by_last_user_message_at_desc() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let a = store.create(CreateInput { id: "a".into(), prompt: "a".into(), ..Default::default() }).await.unwrap();
        store.create(CreateInput { id: "b".into(), prompt: "b".into(), ..Default::default() }).await.unwrap();
        // default: newest-created first (b before a)
        assert_eq!(store.list().await.unwrap().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["b", "a"]);
        // bump a past b
        store.update("a", SessionPatch { last_user_message_at: Some(a.created_at + 10_000), ..Default::default() }).await.unwrap();
        assert_eq!(store.list().await.unwrap().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn opens_a_legacy_db_and_backfills_last_user_message_at() {
        // Simulate a DB created by an older TS store: sessions table WITHOUT lastUserMessageAt.
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, createdAt INTEGER, seq INTEGER)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq) VALUES ('old','done','p',12345,0)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let store = Store::open(path, dir.join("logs")).await.unwrap();
        let s = store.get("old").await.unwrap().unwrap();
        assert_eq!(s.created_at, 12345);
        assert_eq!(s.last_user_message_at, 12345); // backfilled
    }

    #[tokio::test]
    async fn append_log_writes_one_line_per_call() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        store.append_log("s1", "{\"type\":\"x\"}").await.unwrap();
        store.append_log("s1", "{\"type\":\"y\"}").await.unwrap();
        let content = std::fs::read_to_string(store.log_path("s1")).unwrap();
        assert_eq!(content, "{\"type\":\"x\"}\n{\"type\":\"y\"}\n");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test store::`
Expected: FAIL — `Store`/`Session`/`CreateInput`/`SessionPatch` not found (compile error).

- [ ] **Step 4: Implement `src/store.rs`** (parity with `store.ts`; `create()` writes the `repo` column as `repos[0] ?? ""`, stores raw `mode`, and persists `baseSha`/`baseShas`; `row_to_session` replicates the fallbacks; `append_log` runs off-executor; `now_ms` never panics).

```rust
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("sqlite error: {0}")] Sqlx(#[from] sqlx::Error),
    #[error("io error: {0}")] Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Session {
    pub id: String,
    pub repo: Option<String>,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    #[serde(rename = "worktreePath")] pub worktree_path: Option<String>,
    pub branch: Option<String>,
    #[serde(rename = "claudeSessionId")] pub claude_session_id: Option<String>,
    pub status: String,
    #[serde(rename = "costUsd")] pub cost_usd: Option<f64>,
    #[serde(rename = "exitCode")] pub exit_code: Option<i64>,
    pub error: Option<String>,
    #[serde(rename = "errorKind")] pub error_kind: Option<String>,
    #[serde(rename = "createdAt")] pub created_at: i64,
    #[serde(rename = "startedAt")] pub started_at: Option<i64>,
    #[serde(rename = "endedAt")] pub ended_at: Option<i64>,
    #[serde(rename = "lastUserMessageAt")] pub last_user_message_at: i64,
    #[serde(rename = "baseSha")] pub base_sha: Option<String>,
    #[serde(rename = "baseShas")] pub base_shas: std::collections::HashMap<String, Option<String>>,
    #[serde(rename = "worktreeState")] pub worktree_state: String,
}

/// Input for creating a new session. Fields added here match TS `create()` input. Callers that
/// use `..Default::default()` continue to compile — new fields all have `Option`/`Vec` defaults.
#[derive(Default)]
pub struct CreateInput {
    pub id: String,
    pub prompt: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Raw mode string (e.g. `"ultra"`, `"ultracode"`). Stored as-is; normalization happens
    /// only on read in `row_to_session`. (Fix #5: TS `create()` persists raw `input.mode`.)
    pub mode: Option<String>,
    /// Override for the `repo` column. When `None`, derived from `repos.first()` (TS: `repos[0] ?? ""`).
    pub repo: Option<String>,
    /// Optional base SHA for the primary repo.
    pub base_sha: Option<String>,
    /// Optional per-repo base SHAs. Defaults to `{}`.
    pub base_shas: std::collections::HashMap<String, Option<String>>,
}

/// A partial update. Only `Some` fields are written. Mirrors the TS `UPDATABLE` set for the fields
/// Phase 1 needs; extend as later phases require.
#[derive(Default)]
pub struct SessionPatch {
    pub status: Option<String>,
    pub claude_session_id: Option<Option<String>>,
    pub cost_usd: Option<Option<f64>>,
    pub exit_code: Option<Option<i64>>,
    pub error: Option<Option<String>>,
    pub error_kind: Option<Option<String>>,
    pub started_at: Option<Option<i64>>,
    pub ended_at: Option<Option<i64>>,
    pub last_user_message_at: Option<i64>,
    pub prompt: Option<String>,
}

const COLUMNS_DDL: &str = "id TEXT PRIMARY KEY, repo TEXT, prompt TEXT, worktreePath TEXT, branch TEXT, \
  claudeSessionId TEXT, status TEXT, costUsd REAL, exitCode INTEGER, error TEXT, errorKind TEXT, \
  createdAt INTEGER, startedAt INTEGER, endedAt INTEGER, baseSha TEXT, worktreeState TEXT, repos TEXT, \
  skills TEXT, baseShas TEXT, model TEXT, effort TEXT, mode TEXT, seq INTEGER";

// (name, decl) — columns added after the initial schema; added idempotently (parity with ADDED_COLUMNS).
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("baseSha", "TEXT"), ("worktreeState", "TEXT DEFAULT 'live'"), ("repos", "TEXT"), ("skills", "TEXT"),
    ("baseShas", "TEXT"), ("model", "TEXT"), ("effort", "TEXT"), ("mode", "TEXT"), ("errorKind", "TEXT"),
    ("lastUserMessageAt", "INTEGER"),
];

/// Returns the current time as milliseconds since UNIX epoch. Never panics (mirrors TS `Date.now()`).
fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64 }

fn normalize_mode(m: Option<String>) -> Option<String> {
    match m.as_deref() { Some("ultracode") | Some("ultra") => Some("ultracode".into()), _ => None }
}
fn safe_json_vec(raw: Option<String>, fallback: Vec<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or(fallback)
}
fn safe_json_map(raw: Option<String>) -> std::collections::HashMap<String, Option<String>> {
    raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub struct Store {
    pool: SqlitePool,
    log_dir: PathBuf,
    seq: AtomicI64,
}

impl Store {
    pub async fn open(db_path: impl AsRef<Path>, log_dir: impl AsRef<Path>) -> Result<Store, StoreError> {
        let db_path = db_path.as_ref();
        if let Some(parent) = db_path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::create_dir_all(log_dir.as_ref())?;
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await?;
        sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
        sqlx::query("PRAGMA synchronous = NORMAL").execute(&pool).await?;
        sqlx::query("PRAGMA busy_timeout = 5000").execute(&pool).await?;
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS sessions ({COLUMNS_DDL})")).execute(&pool).await?;
        // migrate: add any missing ADDED_COLUMNS, then backfill lastUserMessageAt
        let have: Vec<String> = sqlx::query("PRAGMA table_info(sessions)").fetch_all(&pool).await?
            .iter().map(|r| r.get::<String, _>("name")).collect();
        for (name, decl) in ADDED_COLUMNS {
            if !have.iter().any(|h| h == name) {
                sqlx::query(&format!("ALTER TABLE sessions ADD COLUMN {name} {decl}")).execute(&pool).await?;
            }
        }
        sqlx::query("UPDATE sessions SET lastUserMessageAt = createdAt WHERE lastUserMessageAt IS NULL").execute(&pool).await?;
        Ok(Store { pool, log_dir: log_dir.as_ref().to_path_buf(), seq: AtomicI64::new(0) })
    }

    pub fn log_path(&self, id: &str) -> PathBuf { self.log_dir.join(format!("{id}.jsonl")) }

    /// Append a single line (with trailing `\n`) to the session's log file.
    /// The blocking file write runs in `spawn_blocking` so it never blocks the async executor.
    pub async fn append_log(&self, id: &str, line: &str) -> Result<(), StoreError> {
        let path = self.log_path(id);
        let line = line.to_string();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(f, "{line}")?;
            Ok(())
        }).await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;
        Ok(())
    }

    pub async fn create(&self, input: CreateInput) -> Result<Session, StoreError> {
        let now = now_ms();
        // TS `create()`: `repo` column gets `repos[0] ?? ""` (NOT null). Fix #4.
        let repo_col: String = input.repo
            .unwrap_or_else(|| input.repos.first().cloned().unwrap_or_default());
        // Store repo as Some("") rather than None to match TS behavior; row_to_session normalizes.
        let repo_opt: Option<String> = Some(repo_col.clone());
        let base_shas = input.base_shas;
        let s = Session {
            id: input.id,
            // repo field in Session: None when empty string (TS stores "" but we expose None for empty).
            repo: if repo_col.is_empty() { None } else { Some(repo_col.clone()) },
            repos: input.repos.clone(),
            skills: input.skills.clone(),
            prompt: input.prompt,
            model: input.model,
            effort: input.effort,
            // Fix #5: store raw mode, do NOT normalize on create. Normalization happens on read.
            mode: input.mode,
            worktree_path: input.worktree_path,
            branch: input.branch,
            claude_session_id: None,
            status: "pending".into(),
            cost_usd: None,
            exit_code: None,
            error: None,
            error_kind: None,
            created_at: now,
            started_at: None,
            ended_at: None,
            last_user_message_at: now,
            base_sha: input.base_sha.clone(),
            base_shas: base_shas.clone(),
            worktree_state: "live".into(),
        };
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        sqlx::query("INSERT INTO sessions (id,repo,repos,skills,prompt,worktreePath,branch,claudeSessionId,status,costUsd,exitCode,error,errorKind,createdAt,startedAt,endedAt,lastUserMessageAt,baseSha,baseShas,worktreeState,model,effort,mode,seq) \
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&s.id).bind(&repo_opt).bind(serde_json::to_string(&s.repos).unwrap()).bind(serde_json::to_string(&s.skills).unwrap())
            .bind(&s.prompt).bind(&s.worktree_path).bind(&s.branch).bind(&s.claude_session_id).bind(&s.status)
            .bind(s.cost_usd).bind(s.exit_code).bind(&s.error).bind(&s.error_kind).bind(s.created_at)
            .bind(s.started_at).bind(s.ended_at).bind(s.last_user_message_at).bind(&s.base_sha)
            .bind(serde_json::to_string(&s.base_shas).unwrap()).bind(&s.worktree_state)
            .bind(&s.model).bind(&s.effort).bind(&s.mode).bind(seq)
            .execute(&self.pool).await?;
        Ok(s)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| row_to_session(&r)))
    }

    pub async fn list(&self) -> Result<Vec<Session>, StoreError> {
        let rows = sqlx::query("SELECT * FROM sessions ORDER BY COALESCE(lastUserMessageAt, createdAt) DESC, seq DESC")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_session).collect())
    }

    pub async fn update(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError> {
        let mut sets: Vec<&str> = Vec::new();
        // Build the SET list + bind in the same order.
        macro_rules! col { ($opt:expr, $sql:literal) => { if $opt.is_some() { sets.push($sql); } } }
        col!(patch.status, "status = ?"); col!(patch.claude_session_id, "claudeSessionId = ?");
        col!(patch.cost_usd, "costUsd = ?"); col!(patch.exit_code, "exitCode = ?");
        col!(patch.error, "error = ?"); col!(patch.error_kind, "errorKind = ?");
        col!(patch.started_at, "startedAt = ?"); col!(patch.ended_at, "endedAt = ?");
        col!(patch.last_user_message_at, "lastUserMessageAt = ?"); col!(patch.prompt, "prompt = ?");
        if sets.is_empty() { return Ok(()); }
        let sql = format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = &patch.status { q = q.bind(v); }
        if let Some(v) = &patch.claude_session_id { q = q.bind(v); }
        if let Some(v) = &patch.cost_usd { q = q.bind(v); }
        if let Some(v) = &patch.exit_code { q = q.bind(v); }
        if let Some(v) = &patch.error { q = q.bind(v); }
        if let Some(v) = &patch.error_kind { q = q.bind(v); }
        if let Some(v) = &patch.started_at { q = q.bind(v); }
        if let Some(v) = &patch.ended_at { q = q.bind(v); }
        if let Some(v) = &patch.last_user_message_at { q = q.bind(v); }
        if let Some(v) = &patch.prompt { q = q.bind(v); }
        q.bind(id).execute(&self.pool).await?;
        Ok(())
    }
}

/// Convert a SQLite row to a `Session`, replicating all TS `rowToSession` fallbacks.
fn row_to_session(r: &SqliteRow) -> Session {
    let repo: Option<String> = r.try_get("repo").ok().flatten()
        // Treat empty string as absent (TS `r.repo` is falsy for "").
        .filter(|s: &String| !s.is_empty());
    let base_sha_col: Option<String> = r.try_get("baseSha").ok().flatten();

    // Fix #3/#6: replicate TS `rowToSession` baseShas/baseSha/repos fallbacks exactly.
    //
    // TS: `baseShas = r.baseShas ?? (r.repo && r.baseSha != null ? {[r.repo]: r.baseSha} : {})`
    let base_shas: std::collections::HashMap<String, Option<String>> = {
        let raw: Option<String> = r.try_get("baseShas").ok().flatten();
        let parsed = raw.and_then(|s| serde_json::from_str(&s).ok());
        parsed.unwrap_or_else(|| {
            // Fallback: if repo non-empty AND baseSha non-null → {repo: baseSha}, else {}
            match (&repo, &base_sha_col) {
                (Some(rep), Some(sha)) => {
                    let mut m = std::collections::HashMap::new();
                    m.insert(rep.clone(), Some(sha.clone()));
                    m
                }
                _ => std::collections::HashMap::new(),
            }
        })
    };

    // TS repos fallback: `r.repos ?? (r.repo ? [r.repo] : [])` — empty string is falsy → []
    let repos: Vec<String> = {
        let raw: Option<String> = r.try_get("repos").ok().flatten();
        safe_json_vec(raw, repo.as_ref().map(|r| vec![r.clone()]).unwrap_or_default())
    };

    // TS: `baseSha = r.baseSha ?? (repos[0] != null ? baseShas[repos[0]] ?? null : null)`
    let base_sha: Option<String> = base_sha_col.clone().or_else(|| {
        repos.first().and_then(|r0| base_shas.get(r0).and_then(|v| v.clone()))
    });

    let created_at: i64 = r.try_get("createdAt").unwrap_or(0);
    Session {
        id: r.try_get("id").unwrap_or_default(),
        repo: repo.clone(),
        repos,
        skills: safe_json_vec(r.try_get("skills").ok().flatten(), vec![]),
        prompt: r.try_get("prompt").unwrap_or_default(),
        model: r.try_get("model").ok().flatten(),
        effort: r.try_get("effort").ok().flatten(),
        // Normalization happens on READ (not on create/write).
        mode: normalize_mode(r.try_get("mode").ok().flatten()),
        worktree_path: r.try_get("worktreePath").ok().flatten(),
        branch: r.try_get("branch").ok().flatten(),
        claude_session_id: r.try_get("claudeSessionId").ok().flatten(),
        status: r.try_get("status").unwrap_or_default(),
        cost_usd: r.try_get("costUsd").ok().flatten(),
        exit_code: r.try_get("exitCode").ok().flatten(),
        error: r.try_get("error").ok().flatten(),
        error_kind: r.try_get("errorKind").ok().flatten(),
        created_at,
        started_at: r.try_get("startedAt").ok().flatten(),
        ended_at: r.try_get("endedAt").ok().flatten(),
        last_user_message_at: r.try_get::<Option<i64>, _>("lastUserMessageAt").ok().flatten().unwrap_or(created_at),
        base_sha,
        base_shas,
        worktree_state: r.try_get::<Option<String>, _>("worktreeState").ok().flatten().unwrap_or_else(|| "live".into()),
    }
}
```

Add `mod store;` to `src/main.rs`.

- [ ] **Step 5: Run tests + build**

Run: `cargo test store:: && cargo build`
Expected: 4 store tests PASS; build clean (a `safe_json_map`/`repo`/`base_sha` unused warning is acceptable — exercised in T5).

- [ ] **Step 6: Commit (commit-only, NEVER push)**

```bash
git add Cargo.toml Cargo.lock src/store.rs src/main.rs
git commit -m "feat(rs): sqlx Store at parity with store.ts (schema, migration, CRUD)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: `RenderedProjection` — incremental rendered transcript (byte-correct)

**Files:**
- Create: `server-rs/src/transcript.rs`
- Modify: `server-rs/src/main.rs` (add `mod transcript;`)
- Test: inline `#[cfg(test)]` in `src/transcript.rs`

**Interfaces (exact Rust signatures):**
```rust
pub fn is_rendered(line: &str) -> bool;
pub fn filter_rendered(raw: &[String]) -> Vec<String>;
pub struct Window { pub start: usize, pub lines: Vec<String>, pub total: usize }

impl RenderedProjection {
    pub fn new() -> Self;
    pub fn count(&self) -> usize;        // rendered-coordinate cursor
    pub fn raw_count(&self) -> u64;
    pub fn byte_offset(&self) -> u64;
    pub fn bytes(&self) -> usize;
    pub fn rendered_clone(&self) -> Vec<String>;
    pub async fn sync(&mut self, path: &Path) -> std::io::Result<()>;
    pub fn window_tail(&self, limit: usize) -> Window;
    pub fn range(&self, before: usize, limit: usize) -> &[String];
    pub fn slice_from(&self, since: usize) -> &[String];
}
```

- [ ] **Step 1: Write failing tests in `src/transcript.rs`** (mirror `renderedLog.test.ts` intent — filter keeps rendered/drops noise with fallback; equivalence; incremental sync; window/range/slice/count; missing file).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AO};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn write_lines(path: &std::path::Path, lines: &[&str]) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for l in lines { writeln!(f, "{l}").unwrap(); }
    }
    fn tmpfile(name: &str) -> std::path::PathBuf {
        let n = CTR.fetch_add(1, AO::SeqCst);
        let p = std::env::temp_dir().join(format!("agentic-tx-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p); p
    }

    const RENDERED: [&str; 2] = ["{\"type\":\"agentic_prompt\",\"text\":\"hi\"}", "{\"type\":\"assistant\"}"];
    const NOISE: [&str; 2] = ["{\"type\":\"tool_result\"}", "{\"type\":\"system\"}"];

    #[test]
    fn filter_keeps_rendered_drops_noise_with_fallback() {
        let all: Vec<String> = RENDERED.iter().chain(NOISE.iter()).map(|s| s.to_string()).collect();
        assert_eq!(filter_rendered(&all), RENDERED.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // empty-after-filter fallback: returns the raw lines
        let only_noise: Vec<String> = NOISE.iter().map(|s| s.to_string()).collect();
        assert_eq!(filter_rendered(&only_noise), only_noise);
    }

    #[tokio::test]
    async fn sync_is_incremental_and_matches_filter_rendered() {
        let path = tmpfile("inc");
        write_lines(&path, &[RENDERED[0], NOISE[0], RENDERED[1]]);
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 2); // 2 rendered, 1 noise dropped
        assert_eq!(p.raw_count(), 3);
        // equivalence invariant: split on '\n', filter empty (matches TS readLog split/filter)
        let raw_bytes = std::fs::read(&path).unwrap();
        let whole: Vec<String> = raw_bytes.split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert_eq!(p.rendered_clone(), filter_rendered(&whole));
        // append more; a second sync reads only the new bytes
        let before = p.byte_offset();
        write_lines(&path, &[NOISE[1], RENDERED[0]]);
        p.sync(&path).await.unwrap();
        assert!(p.byte_offset() > before);
        assert_eq!(p.count(), 3);
    }

    #[tokio::test]
    async fn window_range_slice_count() {
        let path = tmpfile("win");
        let many: Vec<String> = (0..10).map(|i| format!("{{\"type\":\"assistant\",\"i\":{i}}}")).collect();
        write_lines(&path, &many.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 10);
        let w = p.window_tail(3);
        assert_eq!((w.start, w.total, w.lines.len()), (7, 10, 3));
        assert_eq!(p.range(7, 4).len(), 4);   // [3,7)
        assert_eq!(p.range(2, 5).len(), 2);   // [0,2) clamped
        assert_eq!(p.slice_from(8).len(), 2); // [8,10)
        assert_eq!(p.slice_from(100).len(), 0);
    }

    #[tokio::test]
    async fn missing_file_is_empty() {
        let mut p = RenderedProjection::new();
        p.sync(std::path::Path::new("/no/such/file.jsonl")).await.unwrap();
        assert_eq!(p.count(), 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test transcript::tests`
Expected: FAIL — items not found (compile error).

- [ ] **Step 3: Implement `src/transcript.rs` (projection part)** — byte-correct sync: `carry: Vec<u8>`, decode only at `b'\n'` boundaries, advance `byte_offset` by bytes actually read, reset+re-read on file shrink (rotation).

```rust
use std::path::Path;
use tokio::task::spawn_blocking;

const RENDERED_PREFIXES: [&str; 4] = [
    "{\"type\":\"agentic_prompt\"", "{\"type\":\"stream_event\"", "{\"type\":\"assistant\"", "{\"type\":\"result\"",
];

pub fn is_rendered(line: &str) -> bool {
    RENDERED_PREFIXES.iter().any(|p| line.starts_with(p))
}

/// Rendered view; falls back to the raw lines if filtering yields nothing (parity with renderedLog.ts).
pub fn filter_rendered(raw: &[String]) -> Vec<String> {
    let filtered: Vec<String> = raw.iter().filter(|l| is_rendered(l)).cloned().collect();
    if filtered.is_empty() { raw.to_vec() } else { filtered }
}

pub struct Window { pub start: usize, pub lines: Vec<String>, pub total: usize }

/// Per-session append-only rendered projection with its own file byte cursor.
///
/// # Equivalence invariant
///
/// After every `sync()` that reads complete lines (i.e. lines terminated by `'\n'`),
/// `self.rendered == filter_rendered(complete_lines_of_file)`.
///
/// # Deliberate divergence from TS `readLog`
///
/// The TS `readLog` does `readFileSync().split("\n").filter(l => l.length > 0)`, which
/// INCLUDES a non-empty final line that lacks a trailing `\n` (a half-written in-flight event).
/// We intentionally do NOT include that partial line in `rendered`: a half-written line is an
/// in-flight event that should be delivered via the live stream, not the cursor-indexed rendered
/// list. The cursor converges on the next `sync()` once the `\n` lands. This is safe because the
/// Android client's rendered-coordinate `since` cursor only advances past lines that appear in
/// `rendered`, so a partial line never corrupts the cursor — it simply appears in the next sync
/// window.
///
/// # UTF-8 multibyte safety
///
/// The partial-line carry is `Vec<u8>` (raw bytes). We decode to `&str` only at newline
/// (`b'\n'`) boundaries — never on a partial byte buffer — so a multibyte UTF-8 character
/// (CJK, emoji) split across a read boundary is never corrupted.
pub struct RenderedProjection {
    rendered: Vec<String>,
    byte_offset: u64,
    raw_count: u64,
    /// Partial trailing bytes of the last incomplete line (no `\n` yet). Raw bytes — decoded
    /// only when a `\n` arrives.
    carry: Vec<u8>,
    bytes: usize,    // approx memory = sum of rendered line lengths
}

impl RenderedProjection {
    pub fn new() -> Self {
        RenderedProjection {
            rendered: Vec::new(),
            byte_offset: 0,
            raw_count: 0,
            carry: Vec::new(),
            bytes: 0,
        }
    }

    pub fn count(&self) -> usize { self.rendered.len() }
    pub fn raw_count(&self) -> u64 { self.raw_count }
    pub fn byte_offset(&self) -> u64 { self.byte_offset }
    pub fn bytes(&self) -> usize { self.bytes }
    pub fn rendered_clone(&self) -> Vec<String> { self.rendered.clone() }

    /// Read only the new bytes `[byte_offset, EOF)` and ingest complete (newline-terminated)
    /// lines. Missing file = no-op.
    ///
    /// If the file has shrunk (rotation), the projection is reset and re-read from the start so a
    /// rotated log re-hydrates instead of freezing.
    ///
    /// `byte_offset` is advanced by the number of bytes ACTUALLY read, not by a metadata snapshot.
    pub async fn sync(&mut self, path: &Path) -> std::io::Result<()> {
        let path = path.to_path_buf();
        let offset = self.byte_offset;
        // Returns Ok(None) = file not found; Ok(Some((buf, rotated))) otherwise.
        // `rotated = true` means the file shrank and `buf` is the full file from byte 0.
        let read = spawn_blocking(move || -> std::io::Result<Option<(Vec<u8>, bool)>> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            let size = f.metadata()?.len();
            if size < offset {
                // Rotation: re-read whole file from the start.
                let mut buf = Vec::with_capacity(size as usize);
                f.read_to_end(&mut buf)?;
                return Ok(Some((buf, true)));
            }
            if size == offset {
                return Ok(Some((Vec::new(), false)));
            }
            f.seek(SeekFrom::Start(offset))?;
            let to_read = size - offset;
            let mut buf = Vec::with_capacity(to_read as usize);
            f.take(to_read).read_to_end(&mut buf)?;
            Ok(Some((buf, false)))
        }).await.expect("spawn_blocking join")?;

        let Some((buf, rotated)) = read else { return Ok(()) };

        if rotated {
            // Reset projection state; re-process from scratch below.
            self.rendered.clear();
            self.carry.clear();
            self.raw_count = 0;
            self.bytes = 0;
            self.byte_offset = 0;
        }

        if buf.is_empty() {
            // No new bytes (file at same size, or rotation resulted in empty file).
            return Ok(());
        }

        // Append new bytes to the byte carry, then split on b'\n'.
        // Decode only at newline boundaries — never on a partial byte buffer — so a multibyte
        // UTF-8 character (CJK, emoji) split across a read boundary is never corrupted.
        let mut carry = std::mem::take(&mut self.carry);
        carry.extend_from_slice(&buf);

        let mut start = 0;
        for i in 0..carry.len() {
            if carry[i] == b'\n' {
                let line_bytes = &carry[start..i];
                if !line_bytes.is_empty() {
                    // Safe to decode: we are at a complete line boundary.
                    let line = String::from_utf8_lossy(line_bytes);
                    self.raw_count += 1;
                    if is_rendered(&line) {
                        self.bytes += line.len();
                        self.rendered.push(line.into_owned());
                    }
                }
                start = i + 1;
            }
        }
        // Keep the trailing bytes after the last '\n' as the new byte carry.
        self.carry = carry[start..].to_vec();

        // Advance byte_offset by the number of bytes actually read (not by metadata snapshot).
        self.byte_offset += buf.len() as u64;
        Ok(())
    }

    pub fn window_tail(&self, limit: usize) -> Window {
        let start = self.rendered.len().saturating_sub(limit);
        Window { start, lines: self.rendered[start..].to_vec(), total: self.rendered.len() }
    }
    pub fn range(&self, before: usize, limit: usize) -> &[String] {
        let before = before.min(self.rendered.len());
        let start = before.saturating_sub(limit);
        &self.rendered[start..before]
    }
    pub fn slice_from(&self, since: usize) -> &[String] {
        &self.rendered[since.min(self.rendered.len())..]
    }
}
```

Add `mod transcript;` to `src/main.rs`.

- [ ] **Step 4: Run tests + build**

Run: `cargo test transcript::tests && cargo build`
Expected: all 4 projection tests PASS; build clean.

- [ ] **Step 5: Commit (commit-only, NEVER push)**

```bash
git add src/transcript.rs src/main.rs
git commit -m "feat(rs): incremental rendered-transcript projection (O(delta) byte-correct sync)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: `TranscriptCache` — LRU byte budget + hydrate + concurrent-get consistency

**Files:**
- Modify: `server-rs/src/transcript.rs` (append `TranscriptCache` + a `cache_tests` module)
- Test: inline `#[cfg(test)] mod cache_tests` in `src/transcript.rs`

**Interfaces (exact Rust signatures):**
```rust
impl TranscriptCache {
    pub fn new(budget_bytes: usize) -> Self;
    pub fn drop_session(&self, id: &str);
    pub async fn with<R>(
        &self,
        id: &str,
        path: &std::path::Path,
        f: impl FnOnce(&RenderedProjection) -> R,
    ) -> std::io::Result<R>;
}
```
`with` syncs the projection to EOF (the big cold read inside `sync` is off the map lock and off the executor), runs `f` against it, and returns `f`'s owned result so the projection borrow never escapes the lock.

- [ ] **Step 1: Write failing tests in `src/transcript.rs`** (hydrate→count; LRU evict over a tiny budget then re-hydrate; concurrent get of a cold session is consistent).

```rust
#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering as AO};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn tmpfile(name: &str) -> std::path::PathBuf {
        let n = CTR.fetch_add(1, AO::SeqCst);
        let p = std::env::temp_dir().join(format!("agentic-cache-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p); p
    }
    fn seed(path: &std::path::Path, n: usize) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for i in 0..n { writeln!(f, "{{\"type\":\"assistant\",\"i\":{i}}}").unwrap(); }
    }

    #[tokio::test]
    async fn hydrates_then_serves_count() {
        let path = tmpfile("c1"); seed(&path, 5);
        let cache = TranscriptCache::new(1_000_000);
        let total = cache.with("s1", &path, |p| p.count()).await.unwrap();
        assert_eq!(total, 5);
    }

    #[tokio::test]
    async fn evicts_lru_over_budget_then_rehydrates() {
        let a = tmpfile("ca"); seed(&a, 50);
        let b = tmpfile("cb"); seed(&b, 50);
        let cache = TranscriptCache::new(200); // tiny budget → only one fits
        let n1 = cache.with("a", &a, |p| p.count()).await.unwrap();
        let n2 = cache.with("b", &b, |p| p.count()).await.unwrap();
        assert_eq!((n1, n2), (50, 50));
        // "a" was evicted by "b"; re-access re-hydrates from file to the same count
        assert_eq!(cache.with("a", &a, |p| p.count()).await.unwrap(), 50);
    }

    #[tokio::test]
    async fn concurrent_get_of_cold_session_is_consistent() {
        let path = tmpfile("cc"); seed(&path, 20);
        let cache = Arc::new(TranscriptCache::new(1_000_000));
        let mut hs = Vec::new();
        for _ in 0..8 {
            let c = cache.clone(); let p = path.clone();
            hs.push(tokio::spawn(async move { c.with("s", &p, |pr| pr.count()).await.unwrap() }));
        }
        for h in hs { assert_eq!(h.await.unwrap(), 20); }
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test cache_tests`
Expected: FAIL — `TranscriptCache` not found (compile error).

- [ ] **Step 3: Implement `TranscriptCache` in `src/transcript.rs`** (append below the projection).

```rust
use std::collections::HashMap;
use std::sync::Mutex;

struct Entry { proj: RenderedProjection, last_access: u64 }

/// Per-session rendered projections, LRU-evicted by a byte budget. The big cold read inside
/// `RenderedProjection::sync` is off the executor (spawn_blocking); the map lock is only held for the
/// short in-memory bookkeeping, never across the sync's await.
pub struct TranscriptCache {
    map: Mutex<HashMap<String, Entry>>,
    budget: usize,
    tick: std::sync::atomic::AtomicU64,
}

impl TranscriptCache {
    pub fn new(budget_bytes: usize) -> Self {
        TranscriptCache { map: Mutex::new(HashMap::new()), budget: budget_bytes, tick: std::sync::atomic::AtomicU64::new(0) }
    }

    pub fn drop_session(&self, id: &str) { self.map.lock().unwrap().remove(id); }

    /// Sync the session's projection to current EOF, then run `f` against it and return f's result.
    /// `f` returns an owned value so the projection borrow never escapes the lock.
    pub async fn with<R>(&self, id: &str, path: &std::path::Path, f: impl FnOnce(&RenderedProjection) -> R) -> std::io::Result<R> {
        // Take the projection out (or create) so the big sync runs without holding the map lock.
        let mut proj = {
            let mut m = self.map.lock().unwrap();
            m.remove(id).map(|e| e.proj).unwrap_or_else(RenderedProjection::new)
        };
        proj.sync(path).await?;
        let out = f(&proj);
        let now = self.tick.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut m = self.map.lock().unwrap();
        m.insert(id.to_string(), Entry { proj, last_access: now });
        // evict LRU while over budget (never evict the just-inserted id)
        let mut total: usize = m.values().map(|e| e.proj.bytes()).sum();
        while total > self.budget && m.len() > 1 {
            if let Some(victim) = m.iter().filter(|(k, _)| k.as_str() != id).min_by_key(|(_, e)| e.last_access).map(|(k, _)| k.clone()) {
                if let Some(e) = m.remove(&victim) { total -= e.proj.bytes(); }
            } else { break; }
        }
        Ok(out)
    }
}
```

Note on the concurrent-cold case: two concurrent `with` calls for the same cold id each take the (absent) projection out and build it from the file (idempotent — both read the same content to the same count); the last writer wins in the map. The test asserts every observer sees the correct count, which satisfies correctness without a per-id async lock. (A future optimization can add in-flight dedup; not required for correctness — flagged, not a gap.)

- [ ] **Step 4: Run tests + build**

Run: `cargo test transcript:: && cargo build`
Expected: all projection + cache tests PASS; build clean.

- [ ] **Step 5: Commit (commit-only, NEVER push)**

```bash
git add src/transcript.rs
git commit -m "feat(rs): TranscriptCache (LRU byte budget + rehydrate)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Wire `Config::transcript_cache_bytes` + carry `Store` + `TranscriptCache` in state

**Files:**
- Modify: `server-rs/src/config.rs` (add `transcript_cache_bytes`), `src/state.rs` (carry `store` + `transcript`), `src/main.rs` (build them), `src/api/mod.rs` (async `test_state`)
- Test: inline `#[cfg(test)]` in `src/config.rs`

**Interfaces (exact Rust signatures):**
```rust
pub struct Config { /* ...existing... */ pub transcript_cache_bytes: usize }

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub throttle: Arc<Mutex<LoginThrottle>>,
    pub store: Arc<Store>,
    pub transcript: Arc<TranscriptCache>,
}
```

- [ ] **Step 1: Write the failing config test in `src/config.rs`** (mirror `config.test.ts` intent for the new env).

```rust
    #[test]
    fn transcript_cache_bytes_default_and_override() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.transcript_cache_bytes, 256 * 1024 * 1024);
        let c2 = Config::load(env_of(&[("HOME", "/home/u"), ("AGENTIC_TRANSCRIPT_CACHE_BYTES", "1048576")]));
        assert_eq!(c2.transcript_cache_bytes, 1_048_576);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test config::tests::transcript_cache_bytes_default_and_override`
Expected: FAIL — field missing (compile error).

- [ ] **Step 3: Add the field in `src/config.rs`**

In `struct Config` add: `pub transcript_cache_bytes: usize,`. In `Config::load`, add (after `auth_secret`):
```rust
            transcript_cache_bytes: get("AGENTIC_TRANSCRIPT_CACHE_BYTES").and_then(|v| v.parse().ok()).unwrap_or(256 * 1024 * 1024),
```

- [ ] **Step 4: Carry `Store` + cache in `AppState` (`src/state.rs`)**

```rust
use std::sync::{Arc, Mutex};
use crate::config::Config;
use crate::throttle::LoginThrottle;
use crate::store::Store;
use crate::transcript::TranscriptCache;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub throttle: Arc<Mutex<LoginThrottle>>,
    pub store: Arc<Store>,
    pub transcript: Arc<TranscriptCache>,
}
```

- [ ] **Step 5: Build `Store` + cache in `main.rs`**

Ensure `mod store;` and `mod transcript;` are declared, then in `main()`:
```rust
    let config = Arc::new(Config::load(|k| std::env::var(k).ok()));
    let store = Arc::new(store::Store::open(config.db_path.clone(), config.log_dir.clone()).await.expect("open store"));
    let transcript = Arc::new(transcript::TranscriptCache::new(config.transcript_cache_bytes));
    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState { config: config.clone(), throttle: Arc::new(Mutex::new(LoginThrottle::default())), store, transcript };
```

- [ ] **Step 6: Make `api/mod.rs::test_state()` async and build a `Store` + cache** (test-only; every api test that calls it becomes `app(test_state().await)`).

```rust
    fn c_budget() -> usize { 64 * 1024 * 1024 }

    async fn test_state() -> AppState {
        let mut c = Config::load(|_| None);
        c.password = "pw".into();
        c.auth_secret = "s3cret".into();
        let dir = std::env::temp_dir().join(format!("agentic-apitest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = crate::store::Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        AppState {
            config: Arc::new(c),
            throttle: Arc::new(Mutex::new(LoginThrottle::default())),
            store: Arc::new(store),
            transcript: Arc::new(crate::transcript::TranscriptCache::new(c_budget())),
        }
    }
```

- [ ] **Step 7: Run the whole suite + build**

Run: `cargo test && cargo build`
Expected: ALL tests pass (store + transcript + cache + config + api + auth + throttle); `cargo build` clean (only expected unused-until-Phase-5 warnings: `AppState.store`/`transcript`, `drop_session`, `for_test`).

- [ ] **Step 8: Commit (commit-only, NEVER push)**

```bash
git add src/config.rs src/state.rs src/main.rs src/api/mod.rs
git commit -m "feat(rs): carry Store + TranscriptCache in config/state

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Parity-hardening (code-review fixes #1-#11) + regression tests

This task closes the gap between the first-pass code and exact TS parity, found in review. Each fix is a behavioral parity correction with a dedicated regression test mirroring the precise TS rule. If T1-T4 were already implemented with the parity-correct code shown above (the byte-correct `sync`, raw-mode `create`, full `row_to_session` fallbacks, async `append_log`, per-test counters), this task only **adds the regression tests** and confirms green; otherwise it applies both the code and the tests.

**Files:**
- Modify: `server-rs/src/transcript.rs` (fixes #1, #2, #7, #10 + 3 regression tests), `src/store.rs` (fixes #3, #4, #5, #6, #8, #9 + 3 regression tests), `src/api/mod.rs` (fix #11: per-call unique temp dir)

**Fix map (each = a behavioral parity rule):**

| # | Finding | Rule | Regression test |
|---|---------|------|-----------------|
| #1 | UTF-8 multibyte carry | `carry` is `Vec<u8>`; decode only at `b'\n'` | `multibyte_char_split_across_reads_decodes_correctly` |
| #2 | Partial final line | excluded from `rendered`; converges on next `sync` | `partial_final_line_excluded_then_converges` |
| #3 | `baseShas` fallback | `{repo: baseSha}` when `baseShas` column NULL and repo+baseSha set | `row_to_session_base_shas_fallback_from_repo_and_base_sha` |
| #4 | `create()` repo column | `repos[0] ?? ""` (not NULL) | covered by roundtrip + `repos_fallback_*` |
| #5 | `create()` raw mode | store raw `input.mode`; normalize on read | `create_stores_raw_mode_get_normalizes` |
| #6 | `repos` fallback | empty-string repo → `[]` not `[""]` | `repos_fallback_empty_repo_yields_empty_vec` |
| #7 | `byte_offset` advance | by `buf.len()`, not `metadata().len()` | covered by `sync_is_incremental_*` |
| #8 | `now_ms` no panic | `.unwrap_or_default()` | (no separate test) |
| #9 | `append_log` off-executor | `spawn_blocking` | covered by `append_log_writes_one_line_per_call` |
| #10 | Equivalence oracle | split on `b'\n'`, filter empty (not `lines()`) | `partial_final_line_excluded_then_converges` |
| #11 | API test isolation | per-call unique temp dir | applies to all api tests |

- [ ] **Step 1: Write the failing regression tests in `src/transcript.rs`** (append to `mod tests`).

```rust
    /// Regression for #2/#10: a partial final line (no trailing '\n') is NOT included in
    /// `rendered`. Once the '\n' is appended, count() converges and equals filter_rendered of the
    /// complete lines. The cursor never corrupts.
    #[tokio::test]
    async fn partial_final_line_excluded_then_converges() {
        let path = tmpfile("partial");
        // Write a complete rendered line followed by a partial line (no trailing '\n').
        let complete = "{\"type\":\"assistant\",\"text\":\"done\"}";
        let partial_prefix = "{\"type\":\"assistant\",\"text\":\"in-fli"; // no '\n'
        {
            let mut f = std::fs::OpenOptions::new().create(true).write(true).open(&path).unwrap();
            write!(f, "{complete}\n{partial_prefix}").unwrap();
        }
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        // Only the complete line is in rendered; the partial is in carry.
        assert_eq!(p.count(), 1, "partial line must not be in rendered");
        assert_eq!(p.rendered_clone()[0], complete);

        // Now complete the partial line.
        let rest = "ght\"}";
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{rest}\n").unwrap();
        }
        p.sync(&path).await.unwrap();
        // Both lines now complete — count must equal filter_rendered of the full file.
        assert_eq!(p.count(), 2, "after nl, both lines must be rendered");
        let raw_bytes = std::fs::read(&path).unwrap();
        let whole: Vec<String> = raw_bytes.split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert_eq!(p.rendered_clone(), filter_rendered(&whole),
            "cursor must converge: projection == filter_rendered(complete lines)");
    }

    /// Regression for #1/#10: a multibyte UTF-8 character split across two sync() calls must
    /// decode without corruption (no U+FFFD replacement characters).
    #[tokio::test]
    async fn multibyte_char_split_across_reads_decodes_correctly() {
        let path = tmpfile("multibyte");
        // "你好" in UTF-8 is 6 bytes: [0xE4,0xBD,0xA0,0xE5,0xA5,0xBD].
        // We split the first sync() INSIDE the second character's bytes, then append the rest + '\n'.
        let line_prefix = "{\"type\":\"assistant\",\"text\":\""; // ASCII prefix
        let cjk = "你好";
        let line_suffix = "\"}";
        let full_line = format!("{line_prefix}{cjk}{line_suffix}");

        let full_bytes: Vec<u8> = format!("{full_line}\n").into_bytes();
        // Split after the full prefix + first 4 bytes of CJK (cuts 你(3 bytes) + 1 byte of 好).
        let split_at = line_prefix.len() + 4;
        assert!(split_at < full_bytes.len(), "split must be within the line");

        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&full_bytes[..split_at]).unwrap();
        }
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 0, "no complete line before nl");

        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&full_bytes[split_at..]).unwrap();
        }
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 1, "complete line must be counted after nl");
        let decoded = &p.rendered_clone()[0];
        assert!(decoded.contains("你好"), "multibyte char must decode correctly, got: {decoded:?}");
        assert!(!decoded.contains('\u{FFFD}'), "must not contain replacement char, got: {decoded:?}");
    }

    /// Regression for file rotation: if the file shrinks (rotated), projection resets and
    /// re-reads from the start.
    #[tokio::test]
    async fn file_rotation_resets_and_rehydrates() {
        let path = tmpfile("rotate");
        write_lines(&path, &[RENDERED[0], RENDERED[1]]);
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 2);
        let old_offset = p.byte_offset();
        assert!(old_offset > 0);

        // Simulate rotation: replace file with a new shorter one.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{}", RENDERED[0]).unwrap();
        }
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 1, "after rotation, count must reflect new file");
    }
```

- [ ] **Step 2: Write the failing regression tests in `src/store.rs`** (append to `mod tests`).

```rust
    /// Fix #5: `create()` stores raw mode; `get()` normalizes on read.
    #[tokio::test]
    async fn create_stores_raw_mode_get_normalizes() {
        let dir = tmp();
        let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        let created = store.create(CreateInput {
            id: "m1".into(), prompt: "p".into(), mode: Some("ultra".into()), ..Default::default()
        }).await.unwrap();
        // create() returns the raw mode.
        assert_eq!(created.mode.as_deref(), Some("ultra"), "create() must return raw mode");
        // get() normalizes on read.
        let got = store.get("m1").await.unwrap().unwrap();
        assert_eq!(got.mode.as_deref(), Some("ultracode"), "get() must normalize mode on read");
    }

    /// Fix #3/#6: a row with baseShas column NULL but repo+baseSha set reconstructs
    /// base_shas = {repo: baseSha} and base_sha = baseSha.
    #[tokio::test]
    async fn row_to_session_base_shas_fallback_from_repo_and_base_sha() {
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, baseSha TEXT, repos TEXT, \
                worktreeState TEXT, lastUserMessageAt INTEGER)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,baseSha,lastUserMessageAt) \
                VALUES ('r1','pending','p',1000,0,'myrepo','abc123',1000)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let store = Store::open(path, dir.join("logs")).await.unwrap();
        let s = store.get("r1").await.unwrap().unwrap();
        assert_eq!(s.base_shas.get("myrepo").and_then(|v| v.as_deref()), Some("abc123"),
            "base_shas must be reconstructed from repo+baseSha when baseShas column is null");
        assert_eq!(s.base_sha.as_deref(), Some("abc123"), "base_sha must be set from baseSha column");
    }

    /// Fix #4/#6: repos fallback — empty-string repo → [] not [""]
    #[tokio::test]
    async fn repos_fallback_empty_repo_yields_empty_vec() {
        let dir = tmp();
        let path = dir.join("db.sqlite");
        {
            use sqlx::sqlite::SqlitePoolOptions;
            let pool = SqlitePoolOptions::new().connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
                createdAt INTEGER, seq INTEGER, repo TEXT, lastUserMessageAt INTEGER)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq,repo,lastUserMessageAt) \
                VALUES ('r2','pending','p',1000,0,'',1000)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let store = Store::open(path, dir.join("logs")).await.unwrap();
        let s = store.get("r2").await.unwrap().unwrap();
        assert_eq!(s.repos, Vec::<String>::new(),
            "empty-string repo must yield empty repos vec (TS: r.repo is falsy for '')");
    }
```

- [ ] **Step 3: Run to verify the new tests fail (or the code already satisfies them)**

Run: `cargo test`
Expected: the 6 new regression tests FAIL against first-pass code (or PASS already if T1-T4 used the parity-correct code above). If they fail, apply the corresponding code from the fix map below.

- [ ] **Step 4: Apply the parity fixes** (these are already reflected in the T1/T2 code above; this step states each verbatim so a first-pass implementation can be patched).
  - **#1 (`transcript.rs`):** `carry` field type `String` → `Vec<u8>`; the `sync` ingest loop scans bytes for `b'\n'` and `String::from_utf8_lossy` decodes each complete line only at the boundary (see T2 Step 3).
  - **#2/#10 (`transcript.rs`):** the trailing bytes after the last `b'\n'` stay in `self.carry` and are NOT pushed to `rendered`; the equivalence-oracle test splits raw bytes on `b'\n'` and filters empty (not `read_to_string().lines()`), matching TS `readLog`.
  - **#7 (`transcript.rs`):** `self.byte_offset += buf.len() as u64` (bytes actually read), never a metadata snapshot; file shrink resets state and re-reads from 0.
  - **#3 (`store.rs`):** `row_to_session` computes `base_shas` with the `{repo: baseSha}` fallback when the `baseShas` column is NULL and `repo` is non-empty and `baseSha` is non-null.
  - **#4 (`store.rs`):** `create()` writes the `repo` column as `repos.first() ?? ""` (`Some("")`, not `None`); `CreateInput` carries `repo`, `base_sha`, `base_shas`.
  - **#5 (`store.rs`):** `create()` stores raw `input.mode` (no `normalize_mode` on write); `row_to_session` applies `normalize_mode` on read.
  - **#6 (`store.rs`):** `row_to_session` filters empty-string `repo` to `None` before the `repos`/`baseShas` fallbacks, so an empty repo yields `[]`.
  - **#8 (`store.rs`):** `now_ms` uses `.unwrap_or_default()`.
  - **#9 (`store.rs`):** `append_log` wraps the blocking write in `tokio::task::spawn_blocking`; the `JoinError` is mapped to `StoreError::Io` via an intermediate `std::io::Error`.
  - **#11 (`api/mod.rs`):** add `static API_CTR: AtomicU64` and make `test_state()`'s temp dir `agentic-apitest-<pid>-<n>` so concurrent api tests don't share a dir.

For #11, update `src/api/mod.rs::test_state()`:
```rust
    use std::sync::atomic::{AtomicU64, Ordering as AO};
    /// Per-call unique counter so parallel `#[tokio::test]`s each get their own DB/WAL directory.
    static API_CTR: AtomicU64 = AtomicU64::new(0);

    async fn test_state() -> AppState {
        let n = API_CTR.fetch_add(1, AO::SeqCst);
        let mut c = Config::load(|_| None);
        c.password = "pw".into();
        c.auth_secret = "s3cret".into();
        let dir = std::env::temp_dir().join(format!("agentic-apitest-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = crate::store::Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
        AppState {
            config: Arc::new(c),
            throttle: Arc::new(Mutex::new(LoginThrottle::default())),
            store: Arc::new(store),
            transcript: Arc::new(crate::transcript::TranscriptCache::new(c_budget())),
        }
    }
```

- [ ] **Step 5: Run the whole suite + build**

Run: `cargo test && cargo build`
Expected: **31 tests pass, 0 failed** (25 from T1-T4 + 6 regression tests); build clean (only expected unused-until-Phase-5 warnings: `AppState.store`/`transcript`, `drop_session`, `for_test`, `safe_json_map`).

- [ ] **Step 6: Commit (commit-only, NEVER push)**

```bash
git add src/transcript.rs src/store.rs src/api/mod.rs
git commit -m "fix(rs): Phase 1 code-review fixes #1-#11 (byte-correct sync, store fallbacks, test isolation)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Done criteria (Phase 1)

- `cargo test` green — **31 tests, 0 failed:**
  - store: roundtrip + `lastUserMessageAt == createdAt`; list ordering; legacy-DB migration+backfill; append-log; raw-mode-create-normalize-on-read; `baseShas` fallback; empty-repo→`[]`.
  - transcript: filter+fallback; incremental sync matching `filter_rendered`; window/range/slice/count; missing-file; partial-final-line convergence; multibyte split; file rotation.
  - cache: hydrate→count; LRU evict→rehydrate; concurrent-cold consistency.
  - config: defaults; overrides; `transcript_cache_bytes` default+override. Plus the existing Phase-0 auth/throttle/api suite still green.
- `cargo build` clean (only expected unused-until-Phase-5 warnings).
- The store opens a TS-created DB and round-trips a session unchanged; the projection serves rendered windows/cursors from memory with O(delta) syncs and a byte-correct UTF-8 carry; the cache bounds memory by an LRU byte budget. No HTTP yet — endpoints land in Phase 5.

## Self-review

- **Spec coverage vs the Phase 1 design + notes:**
  - 1a sqlite store parity ✅ — schema/`ADDED_COLUMNS`/migration/backfill (T1), `COALESCE` ordering (T1), `rowToSession` fallbacks for `repos`/`baseShas`/`baseSha`/`mode`/`lastUserMessageAt`/`worktreeState` (T1 + T5 #3/#4/#5/#6), `safeJson` no-panic (T1), `create` raw-mode + `repo`=`""` + `seq` counter (T1 + T5 #4/#5), `appendLog`/`logPath` off-executor (T1 + T5 #9), open-a-TS-DB (T1 legacy-DB test).
  - 1b incremental rendered model ✅ — `RenderedProjection` fields + `is_rendered` single source + `sync` byte-cursor (T2), `window_tail`/`range`/`slice_from`/`count` (T2), equivalence invariant incl. empty-after-filter fallback (T2 + T5 #2/#10), partial-line carry + multibyte safety + rotation (T2 + T5 #1/#7), `TranscriptCache` LRU byte budget + hydrate + drop + concurrent-cold (T3), `Config::transcript_cache_bytes` + `AppState` wiring (T4). `count()` replaces `rawToRenderedOffset` on the hot path (Global Constraints).
  - Out-of-scope honored ✅ — no HTTP endpoints, no engine/spawn/stream/queue/watchdog/workflows/push; those are Phases 3-6/5.
  - Concurrent-get dedup: T3 satisfies correctness via idempotent rebuild and flags an in-flight async-dedup optimization as future work — flagged, not a gap.
- **Placeholder scan:** none — every step has complete, compile-ready Rust code or exact shell commands; no `TODO`/`unimplemented!()`/`...`/stub.
- **Type consistency:** `Store::{open,create,get,list,update,append_log,log_path}`, `CreateInput`/`SessionPatch`/`Session`, `StoreError::{Sqlx,Io}`, `row_to_session`, `now_ms`, `normalize_mode`, `safe_json_vec`/`safe_json_map`; `is_rendered`/`filter_rendered`, `RenderedProjection::{new,sync,window_tail,range,slice_from,count,bytes,raw_count,byte_offset,rendered_clone}`, `Window{start,lines,total}`, `TranscriptCache::{new,with,drop_session}`; `Config.transcript_cache_bytes`, `AppState{config,throttle,store,transcript}` — all signatures match across tasks and against the existing crate (verified `cargo test` green: 31 passed).
