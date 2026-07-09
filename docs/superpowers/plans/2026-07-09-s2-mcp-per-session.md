# S2 — MCP Per-Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add per-session MCP enumeration, hiding, and ad-hoc injection to the Rust backend, mirroring the existing `hiddenPlugins` pattern exactly.

**Architecture:** `McpServerDef` lives in `engine/store.rs` (same file as `Session`/`CreateInput`); two new session fields (`hiddenMcpServers`, `extraMcpServers`) thread through store → mod → runner → sdk_runner → sdk-bridge.mjs; `components.rs` gains a `list_user_mcp_servers()` helper that reads `~/.claude.json`.

**Tech Stack:** Rust (tokio, sqlx, serde_json), Node.js MJS bridge.

## Global Constraints

- Engine (`server-rs/src/engine/`) must NOT import axum. No new crate deps.
- DB change must be additive: new nullable TEXT columns; old sessions still load.
- `cargo test && cargo build` must stay green before each commit.
- Commit locally only; no push, no PR, no Codex. Each commit message ends with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- `McpServerDef` field `type` serializes as `"type"` on the wire (use `#[serde(rename = "type")]`).
- All serde on `McpServerDef` uses camelCase or explicit renames; `#[serde(default)]` on the two new `Session` fields.
- Validation: `name` non-empty; exactly one of `command` (stdio) or `url` (http/sse) present; reject 400 otherwise.

---

### Task 1: `McpServerDef` struct + `list_user_mcp_servers` in `store.rs` and `components.rs`

**Files:**
- Modify: `server-rs/src/engine/store.rs` (add `McpServerDef` struct after imports, before `Session`)
- Modify: `server-rs/src/engine/components.rs` (add `list_user_mcp_servers` helper + call it in `list_components`)

**Interfaces:**
- Produces: `pub struct McpServerDef { name, command, args, env, transport, url, headers }` — fully public, re-used by api layer.
- Produces: `pub fn list_user_mcp_servers(claude_config_base: &Path) -> Vec<ComponentInfo>` — reads `<claude_config_base>/../.claude.json`.

- [ ] **Step 1: Write failing tests for `list_user_mcp_servers`**

In `server-rs/src/engine/components.rs` test module, add:

```rust
#[test]
fn lists_user_mcp_servers_from_claude_json() {
    let base = tmp();
    // The helper reads <config_base>/../.claude.json
    let parent = base.parent().unwrap().to_path_buf();
    std::fs::write(parent.join(".claude.json"),
        r#"{"mcpServers":{"my-server":{"command":"npx","args":["my-mcp"]},"web-server":{"type":"http","url":"https://example.com/mcp"}}}"#
    ).unwrap();

    let out = list_user_mcp_servers(&base);
    assert_eq!(out.len(), 2);
    let s1 = out.iter().find(|c| c.id == "my-server").unwrap();
    assert_eq!(s1.kind, "mcp");
    assert_eq!(s1.source, "user");
    assert!(s1.global_enabled);
    let s2 = out.iter().find(|c| c.id == "web-server").unwrap();
    assert_eq!(s2.kind, "mcp");
}

#[test]
fn list_user_mcp_servers_missing_file_returns_empty() {
    let base = tmp();
    let out = list_user_mcp_servers(&base);
    assert!(out.is_empty());
}

#[test]
fn list_components_includes_mcp_servers() {
    let base = tmp();
    let skills = base.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let parent = base.parent().unwrap().to_path_buf();
    std::fs::write(parent.join(".claude.json"),
        r#"{"mcpServers":{"test-mcp":{"command":"node","args":["server.js"]}}}"#
    ).unwrap();
    let out = list_components(&base, &skills);
    let mcp = out.iter().find(|c| c.kind == "mcp" && c.id == "test-mcp").unwrap();
    assert_eq!(mcp.source, "user");
    assert!(mcp.global_enabled);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev engine::components::tests 2>&1 | tail -20
```

Expected: FAIL — `list_user_mcp_servers` not defined.

- [ ] **Step 3: Add `McpServerDef` to `store.rs`**

After the existing `use` imports at the top of `store.rs`, before the `StoreError` enum, add:

```rust
/// MCP server definition for per-session ad-hoc injection.
/// Supports stdio (`command`+`args`+`env`) and http/sse (`url`+`type`+`headers`) transports.
/// Serde uses the field names verbatim (camelCase where needed via rename).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct McpServerDef {
    pub name: String,
    // stdio transport:
    #[serde(skip_serializing_if = "Option::is_none")] pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub env: Option<std::collections::BTreeMap<String, String>>,
    // http/sse transport:
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")] pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub headers: Option<std::collections::BTreeMap<String, String>>,
}
```

- [ ] **Step 4: Add `list_user_mcp_servers` to `components.rs`**

Replace the stub comment line `// MCP: no user/project mcpServers today...` with:

```rust
    for c in list_user_mcp_servers(config_base) {
        out.push(c);
    }
```

And add the function before `list_components`:

```rust
/// Read user-scope MCP servers from `<config_base>/../.claude.json` → `mcpServers`.
/// Returns one `ComponentInfo{kind:"mcp"}` per named server. Missing file → empty vec.
pub fn list_user_mcp_servers(config_base: &Path) -> Vec<ComponentInfo> {
    let claude_json = config_base.parent().unwrap_or(config_base).join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&claude_json) else { return vec![]; };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![]; };
    let Some(obj) = val.get("mcpServers").and_then(|v| v.as_object()) else { return vec![]; };
    obj.keys()
        .filter(|name| !name.is_empty())
        .map(|name| ComponentInfo {
            kind: "mcp".into(),
            id: name.clone(),
            name: name.clone(),
            description: String::new(),
            source: "user".into(),
            global_enabled: true,
        })
        .collect()
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev engine::components::tests 2>&1 | tail -20
```

Expected: all component tests pass.

- [ ] **Step 6: Full suite + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test 2>&1 | tail -10 && cargo build 2>&1 | tail -5
```

Expected: green + compiles.

- [ ] **Step 7: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add server-rs/src/engine/store.rs server-rs/src/engine/components.rs
git commit -m "$(cat <<'EOF'
feat(s2): add McpServerDef struct and list_user_mcp_servers enumeration

Reads ~/.claude.json → mcpServers and surfaces them as kind="mcp"
ComponentInfo entries in GET /api/global-settings.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Thread `hidden_mcp_servers` + `extra_mcp_servers` through store, SubmitMeta, CreateInput

**Files:**
- Modify: `server-rs/src/engine/store.rs` — `Session`, `CreateInput`, `ADDED_COLUMNS`, INSERT, `row_to_session`
- Modify: `server-rs/src/engine/mod.rs` — `SubmitMeta`, `submit_session` CreateInput build, `fork_session` CreateInput build

**Interfaces:**
- Consumes: `McpServerDef` from Task 1.
- Produces: `Session::hidden_mcp_servers: Vec<String>`, `Session::extra_mcp_servers: Vec<McpServerDef>` — persisted to DB; `CreateInput` + `SubmitMeta` gain same two fields.

- [ ] **Step 1: Write failing store round-trip test**

In `store.rs` test module, add after existing tests:

```rust
#[tokio::test]
async fn roundtrip_mcp_session_fields() {
    let dir = tmp();
    let store = Store::open(dir.join("db.sqlite"), dir.join("logs")).await.unwrap();
    let extra = vec![
        McpServerDef {
            name: "my-mcp".into(),
            command: Some("npx".into()),
            args: Some(vec!["my-server".into()]),
            ..Default::default()
        },
        McpServerDef {
            name: "web-mcp".into(),
            url: Some("https://example.com/mcp".into()),
            transport: Some("http".into()),
            ..Default::default()
        },
    ];
    let hidden = vec!["unwanted-mcp".into()];
    store.create(CreateInput {
        id: "mcp1".into(),
        prompt: "test mcp fields".into(),
        extra_mcp_servers: extra.clone(),
        hidden_mcp_servers: hidden.clone(),
        ..Default::default()
    }).await.unwrap();

    let got = store.get("mcp1").await.unwrap().unwrap();
    assert_eq!(got.extra_mcp_servers, extra);
    assert_eq!(got.hidden_mcp_servers, hidden);
}

#[tokio::test]
async fn old_session_without_mcp_columns_defaults_to_empty() {
    let dir = tmp();
    let path = dir.join("db.sqlite");
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT, prompt TEXT, \
             createdAt INTEGER, seq INTEGER)"
        ).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions (id,status,prompt,createdAt,seq) VALUES ('old2','done','p',12345,0)")
            .execute(&pool).await.unwrap();
        pool.close().await;
    }
    let store = Store::open(path, dir.join("logs")).await.unwrap();
    let s = store.get("old2").await.unwrap().unwrap();
    assert!(s.hidden_mcp_servers.is_empty());
    assert!(s.extra_mcp_servers.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev engine::store::tests::roundtrip_mcp 2>&1 | tail -20
```

Expected: compile error — `extra_mcp_servers` not a field of `CreateInput`.

- [ ] **Step 3: Add fields to `Session` struct**

In `store.rs`, after the `auto_resume_at` field in `Session` (around line 102), add:

```rust
    /// MCP server names to hide for this session (blacklist). Bridge writes them to
    /// `settings.disabledMcpjsonServers` and skips injecting any matching `extra_mcp_servers`.
    #[serde(rename = "hiddenMcpServers", default)] pub hidden_mcp_servers: Vec<String>,
    /// Extra MCP servers to inject for this session only (not persisted globally).
    /// Serialized as JSON array to `extraMcpServers` DB column; forwarded to bridge as
    /// `SDK_BRIDGE_EXTRA_MCP` env (hidden names removed).
    #[serde(rename = "extraMcpServers", default)] pub extra_mcp_servers: Vec<McpServerDef>,
```

- [ ] **Step 4: Add fields to `CreateInput`**

In `store.rs`, after `pub group_id: Option<String>,` in `CreateInput`, add:

```rust
    pub hidden_mcp_servers: Vec<String>,
    pub extra_mcp_servers: Vec<McpServerDef>,
```

- [ ] **Step 5: Add columns to `ADDED_COLUMNS`**

In `store.rs`, after the `("hiddenPlugins", "TEXT"),` line, add:

```rust
    // NULL = no MCP servers hidden/added (rows written before the column existed keep the default).
    ("hiddenMcpServers", "TEXT"),
    ("extraMcpServers", "TEXT"),
```

- [ ] **Step 6: Update INSERT statement and bindings in `Store::create`**

Find the `sqlx::query("INSERT INTO sessions (id,repo,repos,...`) line (~577). Replace the column list to add the two new columns and add two new `.bind()` calls.

Current INSERT column list ends with: `...unreadEventId,ackedEventId)`
New column list: `...unreadEventId,ackedEventId,hiddenMcpServers,extraMcpServers)`

Current query value placeholders: `VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
New: add two more `?`: `VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`

After the last bind `.bind(s.acked_event_id)`, add:
```rust
            .bind(serde_json::to_string(&s.hidden_mcp_servers)?)
            .bind(serde_json::to_string(&s.extra_mcp_servers)?)
```

Also set these fields in the `Session` struct literal (after `auto_resume_at: None`):
```rust
            hidden_mcp_servers: input.hidden_mcp_servers.clone(),
            extra_mcp_servers: input.extra_mcp_servers.clone(),
```

- [ ] **Step 7: Update `row_to_session` to read the new fields**

In the `row_to_session` function, after `auto_resume_at: ...` line, add:

```rust
        hidden_mcp_servers: safe_json_vec(r.try_get("hiddenMcpServers").ok().flatten(), vec![]),
        extra_mcp_servers: r.try_get::<Option<String>, _>("extraMcpServers").ok().flatten()
            .and_then(|s| serde_json::from_str::<Vec<McpServerDef>>(&s).ok())
            .unwrap_or_default(),
```

- [ ] **Step 8: Add fields to `SubmitMeta` in `mod.rs`**

In `engine/mod.rs`, after `pub hidden_plugins: Vec<String>,` in `SubmitMeta`, add:

```rust
    /// MCP server names to disable for this session.
    pub hidden_mcp_servers: Vec<String>,
    /// Ad-hoc MCP servers to inject for this session only.
    pub extra_mcp_servers: Vec<crate::engine::store::McpServerDef>,
```

- [ ] **Step 9: Pass through in `submit_session` CreateInput build**

In `mod.rs` `submit_session`, after `hidden_plugins: meta.hidden_plugins,` in the `CreateInput { ... }` block, add:

```rust
            hidden_mcp_servers: meta.hidden_mcp_servers,
            extra_mcp_servers: meta.extra_mcp_servers,
```

- [ ] **Step 10: Pass through in `fork_session` CreateInput build**

In `mod.rs` `fork_session`, after `hidden_plugins: src.hidden_plugins.clone(),` in the `CreateInput { ... }` block, add:

```rust
            hidden_mcp_servers: src.hidden_mcp_servers.clone(),
            extra_mcp_servers: src.extra_mcp_servers.clone(),
```

- [ ] **Step 11: Run failing tests to verify they now pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev engine::store::tests 2>&1 | tail -20
```

Expected: all store tests pass including the two new ones.

- [ ] **Step 12: Full suite + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test 2>&1 | tail -10 && cargo build 2>&1 | tail -5
```

- [ ] **Step 13: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add server-rs/src/engine/store.rs server-rs/src/engine/mod.rs
git commit -m "$(cat <<'EOF'
feat(s2): thread hiddenMcpServers + extraMcpServers through store and SubmitMeta

Adds two new DB columns (additive migration), Session/CreateInput fields,
SubmitMeta fields, and wires them through submit_session + fork_session.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Thread through `RunSpec` + env injection in `sdk_runner.rs`

**Files:**
- Modify: `server-rs/src/engine/runner.rs` — `RunSpec` gains two new fields
- Modify: `server-rs/src/engine/mod.rs` — RunSpec build site copies session fields
- Modify: `server-rs/src/engine/sdk_runner.rs` — env injection + tests

**Interfaces:**
- Consumes: `RunSpec::hidden_mcp_servers: Vec<String>`, `RunSpec::extra_mcp_servers: Vec<McpServerDef>` from Task 2.
- Produces: env `SDK_BRIDGE_HIDDEN_MCP` (JSON string array) and `SDK_BRIDGE_EXTRA_MCP` (JSON McpServerDef array, hidden entries removed).

- [ ] **Step 1: Write failing env injection test**

In `sdk_runner.rs` test module, add after `start_passes_enabled_plugins_to_bridge_env`:

```rust
#[test]
fn start_passes_mcp_envs_to_bridge() {
    let dir = tmpdir();
    let rec = dir.join("rec.txt");
    let log = dir.join("session.jsonl");
    let fake = fake_node(&dir, &rec);
    let bridge = dir.join("bridge.mjs");
    std::fs::write(&bridge, "// fake bridge").unwrap();

    let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
    let mut env = HashMap::new();
    env.insert("PATH".to_string(), std::env::var("PATH").unwrap_or_default());

    let spec = RunSpec {
        cwd: dir.to_string_lossy().into_owned(),
        env,
        log_path: log,
        hidden_mcp_servers: vec!["hidden-one".into()],
        extra_mcp_servers: vec![
            crate::engine::store::McpServerDef {
                name: "extra-mcp".into(),
                command: Some("npx".into()),
                args: Some(vec!["my-server".into()]),
                ..Default::default()
            },
            crate::engine::store::McpServerDef {
                name: "hidden-one".into(), // must be excluded from EXTRA_MCP
                command: Some("npx".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let handle = runner.start(spec);
    let mut recorded = String::new();
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        recorded = std::fs::read_to_string(&rec).unwrap_or_default();
        if recorded.contains("SDK_BRIDGE_HIDDEN_MCP") && recorded.contains("SDK_BRIDGE_EXTRA_MCP") {
            break;
        }
    }
    handle.stop();

    assert!(
        recorded.contains("SDK_BRIDGE_HIDDEN_MCP=[\"hidden-one\"]"),
        "hidden MCP names must be a JSON array; recorded={recorded}"
    );
    // extra_mcp should only contain the non-hidden "extra-mcp" entry
    assert!(
        recorded.contains("SDK_BRIDGE_EXTRA_MCP=") && recorded.contains("\"extra-mcp\""),
        "extra MCP must be set; recorded={recorded}"
    );
    assert!(
        !recorded.contains("SDK_BRIDGE_EXTRA_MCP=[") || !recorded.split("SDK_BRIDGE_EXTRA_MCP=").nth(1).unwrap_or("").contains("\"hidden-one\""),
        "hidden-one must be excluded from extra MCP; recorded={recorded}"
    );
}

#[test]
fn start_omits_mcp_envs_when_empty() {
    let dir = tmpdir();
    let rec = dir.join("rec.txt");
    let log = dir.join("session.jsonl");
    let fake = fake_node(&dir, &rec);
    let bridge = dir.join("bridge.mjs");
    std::fs::write(&bridge, "// fake bridge").unwrap();

    let runner = SdkRunner::with_node(fake, bridge.to_string_lossy());
    let mut env = HashMap::new();
    env.insert("PATH".to_string(), std::env::var("PATH").unwrap_or_default());

    let spec = RunSpec {
        cwd: dir.to_string_lossy().into_owned(),
        env,
        log_path: log,
        ..Default::default()
    };
    let handle = runner.start(spec);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let recorded = std::fs::read_to_string(&rec).unwrap_or_default();
    handle.stop();

    assert!(
        !recorded.contains("SDK_BRIDGE_HIDDEN_MCP"),
        "must not set SDK_BRIDGE_HIDDEN_MCP when empty; recorded={recorded}"
    );
    assert!(
        !recorded.contains("SDK_BRIDGE_EXTRA_MCP"),
        "must not set SDK_BRIDGE_EXTRA_MCP when empty; recorded={recorded}"
    );
}
```

- [ ] **Step 2: Add `hidden_mcp_servers` and `extra_mcp_servers` to `RunSpec`**

In `runner.rs`, after `pub enabled_plugins: std::collections::BTreeMap<String, bool>,`, add:

```rust
    /// Per-session MCP server name blacklist. Bridge writes them to `settings.disabledMcpjsonServers`.
    pub hidden_mcp_servers: Vec<String>,
    /// Per-session ad-hoc MCP server definitions (hidden names already removed by sdk_runner).
    pub extra_mcp_servers: Vec<crate::engine::store::McpServerDef>,
```

- [ ] **Step 3: Copy session values into RunSpec in `mod.rs`**

In `mod.rs`, in the `build_run_spec` method (or wherever `enabled_plugins:` is set in the RunSpec build), after `enabled_plugins: crate::engine::plugins::resolve_enabled_plugins(...)`, add:

```rust
            hidden_mcp_servers: s.hidden_mcp_servers.clone(),
            extra_mcp_servers: s.extra_mcp_servers.clone(),
```

- [ ] **Step 4: Add env injection in `sdk_runner.rs`**

In `sdk_runner.rs`, after the `enabled_plugins` block (after the `if !enabled_plugins.is_empty() { ... }` block and before `#[cfg(unix)]`), add:

```rust
        // Per-session MCP hidden list → SDK_BRIDGE_HIDDEN_MCP (JSON name array).
        let hidden_mcp: Vec<&str> = spec
            .hidden_mcp_servers
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !hidden_mcp.is_empty() {
            if let Ok(json) = serde_json::to_string(&hidden_mcp) {
                cmd.env("SDK_BRIDGE_HIDDEN_MCP", json);
            }
        }
        // Per-session extra MCP defs → SDK_BRIDGE_EXTRA_MCP (JSON array, hidden names removed).
        let hidden_set: std::collections::HashSet<&str> = hidden_mcp.iter().copied().collect();
        let extra_mcp: Vec<&crate::engine::store::McpServerDef> = spec
            .extra_mcp_servers
            .iter()
            .filter(|d| !d.name.trim().is_empty() && !hidden_set.contains(d.name.trim()))
            .collect();
        if !extra_mcp.is_empty() {
            if let Ok(json) = serde_json::to_string(&extra_mcp) {
                cmd.env("SDK_BRIDGE_EXTRA_MCP", json);
            }
        }
```

- [ ] **Step 5: Run failing tests to verify they now pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev engine::sdk_runner::tests 2>&1 | tail -20
```

Expected: all sdk_runner tests pass.

- [ ] **Step 6: Full suite + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test 2>&1 | tail -10 && cargo build 2>&1 | tail -5
```

- [ ] **Step 7: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add server-rs/src/engine/runner.rs server-rs/src/engine/mod.rs server-rs/src/engine/sdk_runner.rs
git commit -m "$(cat <<'EOF'
feat(s2): inject SDK_BRIDGE_HIDDEN_MCP and SDK_BRIDGE_EXTRA_MCP at spawn

RunSpec gains hidden_mcp_servers + extra_mcp_servers; sdk_runner sets
the env vars (hidden names removed from extra set) when non-empty.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: API layer — `CreateBody` + validation in `sessions.rs`

**Files:**
- Modify: `server-rs/src/api/sessions.rs` — `CreateBody`, validation, map to `SubmitMeta`

**Interfaces:**
- Consumes: `McpServerDef` from `crate::engine::store` (Task 1); `SubmitMeta::hidden_mcp_servers/extra_mcp_servers` (Task 2).
- Produces: HTTP 400 when `extraMcpServers` entries have empty name or wrong/missing transport.

- [ ] **Step 1: Write failing API validation test**

In `sessions.rs` test module, add:

```rust
#[tokio::test]
async fn create_session_rejects_extra_mcp_with_no_transport() {
    let st = make_state().await;
    let body = serde_json::json!({
        "prompt": "test",
        "extraMcpServers": [{"name": "bad-mcp"}]
    });
    let resp = create_session(
        State(st),
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    ).await.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_session_rejects_extra_mcp_with_empty_name() {
    let st = make_state().await;
    let body = serde_json::json!({
        "prompt": "test",
        "extraMcpServers": [{"name": "", "command": "npx"}]
    });
    let resp = create_session(
        State(st),
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    ).await.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_session_accepts_valid_extra_mcp_servers() {
    let st = make_state().await;
    let body = serde_json::json!({
        "prompt": "test",
        "extraMcpServers": [
            {"name": "stdio-mcp", "command": "npx", "args": ["server"]},
            {"name": "http-mcp", "url": "https://example.com/mcp", "type": "http"}
        ],
        "hiddenMcpServers": ["some-mcp"]
    });
    let resp = create_session(
        State(st),
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    ).await.into_response();
    assert_eq!(resp.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev api::sessions::tests::create_session_rejects_extra_mcp 2>&1 | tail -20
```

Expected: compile error — `CreateBody` has no `extra_mcp_servers` field.

- [ ] **Step 3: Add fields to `CreateBody` in `sessions.rs`**

In `sessions.rs`, after `pub staged_uploads: Option<Vec<crate::engine::StagedUpload>>,` in `CreateBody`, add:

```rust
    /// MCP server names to disable for this session.
    #[serde(rename = "hiddenMcpServers")] pub hidden_mcp_servers: Option<Vec<String>>,
    /// Ad-hoc MCP server defs for this session only. Validated: name non-empty, exactly one transport.
    #[serde(rename = "extraMcpServers")] pub extra_mcp_servers: Option<Vec<crate::engine::store::McpServerDef>>,
```

- [ ] **Step 4: Add validation + wire into `SubmitMeta` in `create_session`**

In `sessions.rs`, in `create_session` after `let Some(prompt) = b.prompt.filter(...) else { ... };`, add validation:

```rust
    let extra_mcp_servers = b.extra_mcp_servers.unwrap_or_default();
    for def in &extra_mcp_servers {
        if def.name.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"extraMcpServers: name must be non-empty"}))).into_response();
        }
        let has_stdio = def.command.is_some();
        let has_http = def.url.is_some();
        if !has_stdio && !has_http {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: must have either command (stdio) or url (http/sse)", def.name)}))).into_response();
        }
        if has_stdio && has_http {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":format!("extraMcpServers[{}]: cannot have both command and url", def.name)}))).into_response();
        }
    }
```

Then in the `SubmitMeta { ... }` block, after `staged_uploads: b.staged_uploads.unwrap_or_default(),`, add:

```rust
        hidden_mcp_servers: b.hidden_mcp_servers.unwrap_or_default(),
        extra_mcp_servers,
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test -p agentic-dev api::sessions::tests 2>&1 | tail -20
```

Expected: all session tests pass including the three new ones.

- [ ] **Step 6: Full suite + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test 2>&1 | tail -10 && cargo build 2>&1 | tail -5
```

- [ ] **Step 7: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add server-rs/src/api/sessions.rs
git commit -m "$(cat <<'EOF'
feat(s2): API CreateBody accepts hiddenMcpServers + extraMcpServers with validation

Empty name or missing transport (no command, no url) → 400. Valid defs
pass through to SubmitMeta and onward to the store.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Update `sdk-bridge.mjs` to parse and apply MCP env vars

**Files:**
- Modify: `server-rs/sdk-bridge.mjs`

**Interfaces:**
- Consumes: `SDK_BRIDGE_HIDDEN_MCP` (JSON string array), `SDK_BRIDGE_EXTRA_MCP` (JSON `McpServerDef[]`).
- Produces: `settings.disabledMcpjsonServers` array (from HIDDEN), extra `mcpServers` entries merged alongside `agentic: delegateServer` (from EXTRA, skipping hidden names).

- [ ] **Step 1: Add MCP env parsing after the `enabledPlugins` block**

In `sdk-bridge.mjs`, after the line `if (Object.keys(settings).length) extraArgs.settings = JSON.stringify(settings);` (around line 245), but actually the MCP parsing must happen BEFORE the `extraArgs.settings` serialization so we add it before that line. Find the block:

```js
if (Object.keys(settings).length) extraArgs.settings = JSON.stringify(settings);
```

And insert before it:

```js
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
```

- [ ] **Step 2: Merge `extraMcpServers` into the `mcpServers` option**

Find the line:
```js
    ...(delegateServer ? { mcpServers: { agentic: delegateServer } } : {}),
```

Replace with:
```js
    ...(delegateServer || Object.keys(extraMcpServers).length
      ? { mcpServers: { ...(delegateServer ? { agentic: delegateServer } : {}), ...extraMcpServers } }
      : {}),
```

- [ ] **Step 3: Update the boot log line to include the new env vars**

Find the boot log line section (around line 38-46):
```js
  `hidden_skills=${process.env.SDK_BRIDGE_HIDDEN_SKILLS || ""} ` +
  `enabled_plugins=${process.env.SDK_BRIDGE_ENABLED_PLUGINS || ""} ` +
```

Add after `enabled_plugins` line:
```js
  `hidden_mcp=${process.env.SDK_BRIDGE_HIDDEN_MCP || ""} ` +
  `extra_mcp_count=${extraMcpDefs.length} ` +
```

Wait — the boot log runs before extraMcpDefs is defined (at line 46 vs parsing happens later at ~245). So add only the raw env, not the parsed count:

```js
  `hidden_mcp=${process.env.SDK_BRIDGE_HIDDEN_MCP || ""} ` +
  `extra_mcp=${process.env.SDK_BRIDGE_EXTRA_MCP ? "set" : ""} ` +
```

- [ ] **Step 4: Verify the JS is syntactically valid**

```bash
node --check /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs/sdk-bridge.mjs
```

Expected: no output (syntax OK).

- [ ] **Step 5: Full Rust suite + build**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev/server-rs
cargo test 2>&1 | tail -10 && cargo build 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add server-rs/sdk-bridge.mjs
git commit -m "$(cat <<'EOF'
feat(s2): sdk-bridge parses SDK_BRIDGE_HIDDEN_MCP + SDK_BRIDGE_EXTRA_MCP

Hidden names → settings.disabledMcpjsonServers. Extra defs (hidden
excluded) → merged into mcpServers option alongside the agentic delegate
server. Defensive: bad JSON → ignored.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Write SDD report

**Files:**
- Create: `server-rs/../.superpowers/sdd/s2-report.md` (i.e., `<repo-root>/.superpowers/sdd/s2-report.md`)

- [ ] **Step 1: Write report file**

Create `.superpowers/sdd/s2-report.md` with: files changed, DB migration approach, store round-trip validation evidence, test commands + result counts, bridge-JS-only untested parts, concerns/deviations.

- [ ] **Step 2: Commit**

```bash
cd /home/arcatva/src/agentic-worktrees/7a5b3fa6-51d9-4deb-a442-c30944eee3ab/agentic-dev
git add .superpowers/sdd/s2-report.md
git commit -m "$(cat <<'EOF'
docs(s2): SDD report for MCP per-session implementation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review Against Spec

**Spec coverage:**
1. MCP enumeration `components.rs` → Task 1 ✓
2. `extraMcpServers` session field → Tasks 2+3+4 ✓
3. `hiddenMcpServers` session field → Tasks 2+3+4 ✓
4. DB columns additive migration → Task 2 ✓
5. `SDK_BRIDGE_EXTRA_MCP` env → Task 3 ✓
6. `SDK_BRIDGE_HIDDEN_MCP` env → Task 3 ✓
7. Bridge merges mcpServers → Task 5 ✓
8. Bridge sets `disabledMcpjsonServers` → Task 5 ✓
9. Hidden names excluded from extra set → Task 3 (sdk_runner filter) + Task 5 ✓
10. Tests: components enum, store round-trip, sdk_runner env, API validation → Tasks 1-4 ✓
11. Bridge JS not covered by suite (noted in spec) → Task 5, report notes it ✓
12. No new crate deps, no axum in engine → ✓ (serde_json already a dep; HashSet is std)
13. `cargo test && cargo build` green before each commit → each task ends with this check ✓

**Concerns:**
- The `tmp()` test helper in `components.rs` creates a temp dir whose PARENT is the system temp dir. The test writes `<parent>/../.claude.json` — i.e., `<tmp_parent>/.claude.json`. This will pollute the system temp dir with a `.claude.json` file. Safer to create a nested structure like `base_dir/dot_claude/` and write to `base_dir/.claude.json`. The test code in Task 1 accounts for this by using `base.parent().unwrap()`.
- The boot-log line referencing `extraMcpDefs` before it's defined requires the env-only fallback approach in Task 5 Step 3 — implemented correctly.
