# 设计：彻底移除遗留 TS 后端（切到 Rust-only）

**日期:** 2026-06-22
**目标:** 删除已被 Rust 重写（`server-rs/`）取代的遗留 TypeScript 后端（`server/`），让仓库变成 Rust-only 且内部一致（构建、部署、文档、钩子都不再指向 TS）。
**前置事实:** Rust 后端已完成并通过切换验证，见 `docs/superpowers/runbooks/2026-06-21-rust-backend-cutover-runbook.md`。本设计是该切换的收尾——删掉旧实现。

---

## 1. 背景

- `server/`（TypeScript，Fastify + better-sqlite3）是旧后端；`server-rs/`（Rust，axum/tokio）是新后端，二者读写同一份磁盘状态（`~/.agentic-dev/db.sqlite`、日志、json）。
- 生产每轮对话通过一个极小的 Node 桥 `server-rs/sdk-bridge.mjs` 调用 `@anthropic-ai/claude-agent-sdk`（这是 AskUserQuestion 能在同一轮内暂停等待的原因）。这是删除 TS 后唯一剩下的 Node 代码。
- **生产 Rust 代码完全不依赖 TS。** 唯一的关联是**测试**：Rust 测试复用了 `server/test/fixtures/` 里的 10 个 `fake-claude*.sh`（语言无关的 bash 假 `claude`）。

## 2. 决策记录（已与用户确认）

1. **范围 = 全面切换清理**：删源码 + 搬/删测试替身 + 改部署/构建配置 + 更新文档 + 修钩子 + 清注释。
2. **根目录 Node 工程一并删除**：删 `package.json` + `yarn.lock` + 本地 `node_modules/`；桥从 `server-rs/node_modules` 解析 SDK（部署机在 `server-rs/` 跑一次 `npm install`，切换手册 3.1b 已记载）。
3. **假脚本搬迁到 server-rs，保留全部测试**。最初选的是“删脚本 + 删依赖测试”，但 Task 1 实测发现：删 fixtures 会连带删 **75** 个测试（整套引擎集成测试），因为 `make_engine`/`engine_from`/`base_spec` 等测试工厂自身就用 fixture——“一个假 claude 本质就是一个 fixture”。据此改为**搬迁**：`git mv` 10 个 `fake-claude*.sh` 到 `server-rs/tests/fixtures/`，改约 5 处路径引用；Rust 测试数保持 **259** 不变。
4. **清除所有 parity 注释**：server-rs 里所有引用已删 `server/*.ts` 的注释全部改写或删除。

## 3. 要删除的内容（纯遗留 TS 后端）

| 路径 | 说明 |
|---|---|
| `server/` 整个目录 | engine/、api/、index.ts、test/ 全删。**例外**：`test/fixtures/*.sh`（10 个）先 `git mv` 到 `server-rs/tests/fixtures/`（见 §4）；`test/fixtures/gitrepo.ts`（TS 专用）随 `server/` 删除 |
| `tsconfig.json` | TS 编译配置 |
| `vitest.config.ts` | TS 测试配置 |
| `package.json` | 根 Node 工程清单（脚本全是 TS；SDK 依赖改由 server-rs 提供） |
| `yarn.lock` | TS 依赖锁文件 |
| `scripts/dev.sh` | `tsx server/index.ts` 启动脚本；删后 `scripts/` 若空则一并删 |
| 本地 `node_modules/`（非 git） | git 已忽略、本 worktree 不存在；部署机手动 `rm -rf` |

## 4. 测试替身搬迁（决策 3 — 保留全部测试）

把 `server/test/fixtures/` 里的 10 个 `fake-claude*.sh` 搬到 `server-rs/tests/fixtures/`，再把 4 个 Rust 文件里指向旧路径的引用改到新位置。**所有测试保留，`cargo test` 仍 259 绿。**

1. `git mv server/test/fixtures/<each>.sh server-rs/tests/fixtures/`（git mv 保留可执行位；搬完 `test -x` 确认）。10 个文件：`fake-claude.sh`、`fake-claude-agentresult.sh`、`fake-claude-crash.sh`、`fake-claude-echoargs.sh`、`fake-claude-echoenv.sh`、`fake-claude-error.sh`、`fake-claude-noinit.sh`、`fake-claude-ratelimit.sh`、`fake-claude-stream.sh`、`fake-claude-stream-crash2.sh`。
2. 改 4 处路径引用，把 `env!("CARGO_MANIFEST_DIR")` 后面的 `.join("../server/test/fixtures")` 改成 `.join("tests/fixtures")`（`CARGO_MANIFEST_DIR` = `server-rs/`，新路径即 `server-rs/tests/fixtures`）：
   - `src/api/test_support.rs`（`test_state_with_fixture`，约 68 行）
   - `src/engine/spawner.rs`（约 340 行）
   - `src/engine/runner.rs`（约 193 行）
   - `src/engine/tests.rs`（约 20 行）
3. `gitrepo.ts` 不搬（TS 专用辅助，server-rs 不用），随 `server/` 在 §3 删除。
4. 验证：`cd server-rs && cargo test` 全绿（259）；`grep -rn "server/test/fixtures" server-rs/` 无残留（旧的 `../server/...` 引用已全部改掉）。

> 注：`server-rs/tests/` 是 cargo 集成测试目录，cargo 只把 `tests/*.rs` 当测试目标编译；`tests/fixtures/` 子目录下的 `.sh` 不会被当成测试 crate，安全。
>
> 背景（为何不删）：最初决策是删脚本 + 删依赖测试，但实测删 fixtures 会连带删 75 个测试（整套引擎集成测试），因为 `make_engine`/`engine_from`/`base_spec` 等测试工厂自身就用 fixture。搬迁代价极小且零覆盖损失，故改为搬迁。

## 5. 要改接线的配置（原本指向 TS）

| 文件 | 改动 |
|---|---|
| `deploy/agentic-dev.service` | `ExecStart=` 从 `tsx server/index.ts` 改为 `…/server-rs/target/release/agentic-dev-server`（与切换手册 drop-in 一致）；注释/PATH 说明同步更新 |
| `README.md` | “Run/Test” 从 `bash scripts/dev.sh` / `yarn test` 改为 `cargo run --release`（或直接 run 二进制）/ `cargo test`；说明桥的 `npm install`（在 server-rs） |
| `deploy/README.md` | `yarn install`/`yarn test` → `cargo build --release` + `cd server-rs && npm install`；部署步骤对齐切换手册 |

## 6. 文档更新

| 文件 | 改动 |
|---|---|
| `agentic-dev/CLAUDE.md` | Layout：`server/engine`、`server/api` → `server-rs/src/engine`、`server-rs/src/api`；Rules：`yarn test` → `cargo test`、删“用 yarn 不用 npm”、假 claude 路径改 `server-rs/tests/fixtures/fake-claude.sh`、引擎免 Fastify → 免 axum |
| `docs/internals.md` | 所有 `server/...ts`、`sdkRunner.ts`、`yarn test`/`tsc` 引用改到 server-rs/cargo；保留架构事实，去掉 TS 专有说法 |
| `docs/errorkind-stop-reason.md` | `server/engine/types.ts`、`classifyError.ts` → `server-rs/src/engine/{error.rs, classify_error.rs}` |
| `docs/streaming-only-refactor.md` | “`yarn test` + `tsc` 绿” → “`cargo test` 绿” |

## 7. 钩子修正

- `session-hooks/guard.sh` 规则 #3 现在无条件拦截 `npm install`。删 TS 后，bridge 的 `cd server-rs && npm install` 是正当流程，会被误伤。改为：仅当命令所在工程存在 `yarn.lock` 时才拦（或直接移除该规则）。**默认：移除该规则**，因为仓库已无 yarn 工程。

## 8. 清除 parity 注释（决策 4）

scrub server-rs 中所有引用已删 `server/*.ts` 的注释，逐处改写为不指向已删文件的说法（或删除）。已知点（非穷举，实现时以 `grep -rn "server/" server-rs/src server-rs/sdk-bridge.mjs` 为准）：

- `src/lib.rs`（2 处 “parity with server/...”）
- `src/api/{mod.rs, auth.rs, config.rs}`、`src/engine/{session_guide.rs, workflows.rs, tests.rs}` 等的 “Mirrors/parity server/*.ts”
- `server-rs/sdk-bridge.mjs` 顶部 “This file mirrors server/engine/sdkRunner.ts; keep them in sync.” —— sdkRunner.ts 将不存在，改写为自描述（说明它是 Rust 服务器的 per-turn claude 传输，不再有可同步的对象）。

## 9. 不改动

`server-rs/`（除注释与被删测试）、`claude-global/`、`skills/`、`session-hooks/`（除规则 #3）、`setup.sh`、Android 仓库（仅通过 HTTP 通信，无源码耦合）。

## 10. 验证（完成判据）

1. `cd server-rs && cargo build --release` 成功。
2. `cd server-rs && cargo test` 全绿（数量保持 259，搬迁后零覆盖损失）。
3. `grep -rn "server/" server-rs/ deploy/ README.md docs/ *.md` 无“活引用”——例外：`server-rs/tests/fixtures/` 是搬迁后的合法路径，引用它不算遗留（仅不允许指向已删的 `server/...ts`）。
4. `grep -rn "tsx\|yarn\|vitest\|tsconfig\|server/index.ts" .`（排除 server-rs 第三方、其他仓库 skills）无遗留。
5. `deploy/agentic-dev.service` 的 `ExecStart` 指向 Rust 二进制。

## 11. 部署注意（非本仓改动，提醒用户在部署机执行）

- 在 `server-rs/` 跑 `npm install`（落地 `@anthropic-ai/claude-agent-sdk`），否则桥找不到 SDK。
- systemd 指向 Rust 二进制（仓库 unit 已改；若部署机用的是切换手册的 drop-in `10-rust.conf`，两者一致，无冲突）。
- 删根目录 `node_modules` 是本地操作（git 已忽略）。
- **回滚代价提示**：删除 TS 后端后，切换手册第 4 节“重启旧二进制”式回滚不再可用；回滚需 `git revert` 本次删除提交。
