# 企业级重构设计（后端 server-rs + 安卓 app）

- 日期：2026-07-11
- 状态：待用户复核
- 范围：两个仓库 —— `agentic-dev`（Rust 后端 `server-rs`）与 `agentic-dev-android`（Kotlin/Compose）
- 交付方式：增量小 PR，每个只做一件事、尽量不改行为；每个 PR 由用户评审，不自合并

> 本文档是跨两仓库的**主路线图**。后端相关 PR 从本仓库分支切出；安卓相关 PR 从 `agentic-dev-android` 分支切出。安卓侧执行时会在其 `docs/` 放一份指针引用本文件。

---

## 1. 背景与目标

两个仓库功能完整、都有 CI 和 PR 评审流程，但离"企业级工程质量"还有系统性缺口：

**后端 `server-rs`（Rust，约 3.2 万行）**
- `src/engine/` 是**扁平文件夹**，塞了 50+ 文件，没有子模块分组。
- 存在多个 god-file：`mod.rs` 3322 行、`tests.rs` 单文件 5000 行、`store.rs` 1872、`stream.rs` 1323、`delegate.rs` 1246、`skill_install.rs` 897、`providers.rs`/`sdk_runner.rs` 831。
- `src/api/` 同样偏大：`sessions.rs` 2151、`misc.rs` 1101。
- 缺工程门禁：无 `rust-toolchain.toml`、`rustfmt.toml`、clippy 配置、`cargo-deny`。工具链没钉版、lint 没强制、供应链没审计。
- 一个嵌入式 Node 组件 `sdk-bridge.mjs`（31 KB）+ 自己的 `package.json`/`package-lock.json` 直接躺在 crate 根，属于 polyglot 味道，无独立 lint/format。

**安卓 `agentic-dev-android`（Kotlin/Compose，单 `:app` 模块）**
- 分层已清晰：`data/{net,repo,log,util}`、`domain/`、`ui/<feature>/`、`di/`、`voice/`；测试充分（约 60 个测试文件）。
- 但是工程卫生有缺口：**无版本目录**（`libs.versions.toml` 缺失，依赖全内联钉在 `build.gradle.kts`）、**无静态检查**（detekt/ktlint）、单模块（无 `:core`/`:feature` 拆分）、手写 DI。

**目标**：在不牺牲现有行为与刻意的兼容性钉版前提下，把两个仓库的**目录结构、依赖治理、代码质量门禁、企业级横向配套**系统性补齐，全部通过一串可评审的小 PR 落地。

---

## 2. 范围

### 纳入（用户已确认全选）
1. 后端结构重整（`engine/` 归组 + 拆 god-files）
2. 质量门禁（后端 Rust 工具链/lint/供应链；安卓版本目录/静态检查）
3. 依赖更新（**保守**：只补丁/小版本，守住刻意钉版）
4. 企业级横向补齐（Dependabot、CODEOWNERS、SECURITY、CONTRIBUTING、ARCHITECTURE、模板、CI 加固）
5. **安卓拆多模块**（`:app` → `:core:*` + `:feature:*`，用户确认现在就做）

### 明确不纳入
- 破坏性大版本依赖升级（Ktor 2→3、Rust edition 2021→2024、Compose/material3 换线、AGP 大升）—— 留待单独评估。
- 重写业务逻辑 / 改功能行为。
- 引入新 DI 框架（Hilt）作为独立目标——拆模块时若自然需要再评估，不作为强制项。
- 触碰 `CLAUDE.md` 工作流约定。

### 用户已拍板的三个决策
- **后端结构大改顺序**：排最后、单独做（缩短与并发会话的冲突窗口）。
- **rustfmt 全量格式化**：单独一个纯格式化 PR 先落地，之后 CI 才开 `fmt --check`。
- **安卓多模块**：现在就拆。

---

## 3. 指导原则

1. **一 PR 一事、尽量不改行为**：结构调整只做"搬移 + `re-export`/facade 保持导入路径"，逻辑零改动，diff 好读、回滚安全。
2. **门禁用棘轮方式**：clippy/detekt 首次开启若报存量告警，不一次性全修（否则变巨型 PR），先生成 baseline，CI 只卡新增，存量另行清理。
3. **加法先行、结构大改殿后**：低冲突的加法类 PR（门禁、文档、版本目录、依赖）先落地快速见效；高冲突的结构/拆模块大改放最后，单独做且做完尽快合。
4. **守住刻意钉版**：迁版本目录与依赖升级时，逐条保留现有兼容性注释（Ktor 2.x、Compose 1.9.x、material3 1.4.0-alpha18、AGP 8.7.2、Kotlin 2.0.21、lifecycle 2.8.7 的 lint 禁用等）。
5. **每个 PR 自带验证**：后端 `cargo test` + `cargo clippy`；安卓 `./gradlew test lint detekt`。绿了才请评审。

---

## 4. 分工作线设计

### 4.1 后端质量门禁（PR B1 + 格式化 PR B0）

新增 4 个配置 + 接入现有 `ci.yml`：

- `rust-toolchain.toml`（repo 根）：钉某个 stable channel + `components = ["rustfmt","clippy","rust-src"]`（+ 需要的 targets）。让每个开发者/CI/worktree 用同一编译器，同时确立 MSRV 基线。
- `server-rs/rustfmt.toml`：团队默认（`max_width=100`、`imports_granularity="Crate"`、`group_imports="StdExternalCrate"` 等）。
- `server-rs/Cargo.toml` 加 `[lints]`：clippy 分级启用（`all` + 选择性 `pedantic`），CI 里 `-D warnings`。存量告警用 `#[allow]` 局部豁免或分批清，避免一次性大改。
- `deny.toml`（cargo-deny）：licenses / advisories / bans 三类检查，接入供应链审计。
- CI 接入：在 `ci.yml` 增加 `fmt --check`、`clippy -D warnings`、`cargo-deny check` 步骤（用钉 SHA 的 action）。

**顺序**：先落 **B0** 纯 `cargo fmt` 全量格式化 PR（机械 diff、快速合），再落 **B1** 门禁（此时 `fmt --check` 才能通过）。

### 4.2 后端依赖（PR B4，保守）
- 逐条 `cargo update` 到兼容的补丁/小版本，不动大版本（axum 0.8、sqlx 0.8、reqwest 0.12、rustls 0.23 等锁在当前主次版本线内）。
- 每次升级后 `cargo test` + `cargo clippy` 验证。

### 4.3 后端嵌入 Node 桥（PR B5）
- 把 `sdk-bridge.mjs` + `package.json` + `package-lock.json` 收进子目录 `server-rs/sdk-bridge/`（或 crate 外的 `tools/sdk-bridge/`，取决于构建/打包引用方式，实施时确认 `Makefile`/`main.rs` 里的路径）。
- 加 `prettier` + `eslint`（flat config）+ `.editorconfig`，纳入 CI 和 Dependabot npm 生态。

### 4.4 后端结构重整（阶段三，PR B2 + B3，最后单独做）

**目标形态**：`engine/mod.rs` 收成薄 facade（只放跨模块共享类型 + `pub use <sub>::*` 保持导入路径），50+ 扁平文件收进约 10 个内聚子模块。

建议分组（最终边界在 B2 实施时用耦合分析校验，可能微调）：

| 子模块 | 归入的现有文件（示意） |
|---|---|
| `lifecycle/` | lifecycle, auto_resume, recover, resume_gate, watchdog, status, transition, groups |
| `runtime/` | spawner, runner, sdk_runner |
| `transcript/` | transcript, native_transcript, transcript_filter, tailer, usage |
| `streaming/` | stream（并拆分，见 B3） |
| `store/` | store（并拆分，见 B3）, atomic_write |
| `config/` | global_settings, user_config, templates, session_guide |
| `providers/` | providers, router, litellm |
| `workflows/` | workflows, delegate（并拆分，见 B3） |
| `vcs/` | worktree, repos, diff, structured_diff |
| `skills/` | skills, skill_install, plugins, plugin_cli, components |
| `title/` | title, title_client |
| `notify/` | push |
| `error/` | error, classify_error |
| `mentions/` | mentions, search |

**god-file 拆分（B3）**：
- `mod.rs`(3322)：把 `Engine` 的 `impl` 块按职责切进 `lifecycle/turn.rs`、`lifecycle/queue.rs`、`lifecycle/session_admin.rs`、`lifecycle/fork.rs`；共享类型（`EngineState`、`QueueItem`、`RunningTurn` 等）进 `lifecycle/types.rs`；`mod.rs` 仅留 facade。
- `store.rs`(1872)：按"schema/迁移 / 读 / 写 / 查询"或按聚合根切分。
- `stream.rs`(1323)、`delegate.rs`(1246)：按事件解析 / 传输 / 编排切分。
- `tests.rs`(5000，单文件内联单测)：拆成与各被测模块同名的 `#[cfg(test)]` 子模块就近放置；纯端到端用例可迁 `tests/` 集成目录。

**测试组织建议**：单元测试 `#[cfg(test)] mod tests` 就近贴被测模块；跨模块/HTTP 端到端进 `server-rs/tests/`。

**风险控制**：B2/B3 每步 `cargo test` 全绿；用 `pub(crate) use` 再导出保证外部引用不变；一次 PR 只搬一组，避免超大 diff。

### 4.5 安卓版本目录（PR A1）
- 新建 `gradle/libs.versions.toml`，把 `app/build.gradle.kts` + 根 `build.gradle.kts` 的所有依赖/插件迁入 `[versions]/[libraries]/[plugins]/[bundles]`，**版本一字不改**，逐条保留兼容性注释。
- `app/build.gradle.kts` 依赖块瘦身为 `libs.xxx` 别名引用。行为零变化。

### 4.6 安卓静态检查（PR A2）
- 引入 detekt + ktlint（或 Spotless 统一驱动），为存量生成 baseline，CI 只卡新增。
- 接入 `ci.yml`（`./gradlew detekt`）。

### 4.7 安卓依赖（PR A3，保守）
- 仅在钉定的版本线内升补丁/小版本，守住 Ktor 2.x / Compose 1.9.x / material3 1.4.0-alpha18 / AGP 8.7.x / Kotlin 2.0.x / adaptive 1.2.x。
- 每次 `./gradlew test lint` 验证。

### 4.8 安卓多模块（阶段三，用户确认现在就拆）

**目标模块图**（参考 Google *Now in Android*，映射到现有分层）：

- `:app` —— `MainActivity`、`AgenticApp`、`AgenticMessagingService`、`ui/nav`、DI 装配（`di/`）。瘦身为壳。
- `:core:model` —— `domain/` 里的纯数据类型（Node、Transcript、Usage、Status…）。
- `:core:common` —— `data/util`、`Outcome`、`Polling`、`RelativeTime`、`data/log/*`（日志）。
- `:core:domain` —— `domain/` 纯变换（AskParsing、CommitGraph、SessionSearch、TranscriptReducer…）。
- `:core:network` —— `data/net/*`（AgenticApi、Ktor 实现、Models、LAN 发现、下载器、证书固定）。
- `:core:data` —— `data/repo/*`、`SettingsStore`、`SessionUiStore`。
- `:core:designsystem` —— `ui/components/*`、`Theme`、`Motion`、`Markdown`、图标资源。
- `:core:voice` —— `voice/*` + 供应商包 `com.k2fsa.sherpa.onnx` + `jniLibs`。
- `:feature:*` —— home、session、login、providers、workflow、tree、diagnostics、globalsettings、adopt、newrequest。
- `build-logic/` —— convention plugins（android-application、android-library、compose、kotlin、detekt），消除各模块 build 文件重复。

**分步交付（拆模块本身也是一串小 PR，依赖 A1 版本目录先落）**：
1. `A-mod-0`：搭 `build-logic` + convention plugins 脚手架（不搬文件，仅把 `:app` 改用约定插件）。
2. `A-mod-1`：抽 `:core:model` + `:core:common`（最底层、依赖最少）。
3. `A-mod-2`：抽 `:core:network` + `:core:data`。
4. `A-mod-3`：抽 `:core:domain` + `:core:designsystem` + `:core:voice`。
5. `A-mod-4`：抽 `:feature:*`（按功能分几个 PR）。
6. `A-mod-5`：瘦身 `:app`，收尾 DI 装配与导航。

每步 `./gradlew assembleDebug test` 全绿；测试随对应源码迁到对应模块的 `src/test`。

### 4.9 企业级横向补齐（PR X1-backend / X1-android，各一）

两仓库各补（按仓库定制内容）：
- `.github/dependabot.yml`：后端 = cargo + npm（sdk-bridge）+ github-actions；安卓 = gradle + github-actions。
- `CODEOWNERS`、`SECURITY.md`（写清 HMAC 鉴权 + 自签 TLS 模型 + 漏洞上报渠道，**不含任何密钥**）、`CONTRIBUTING.md`、`ARCHITECTURE.md`（高层总览 + 图；后端复用/提炼现有 `docs/internals.md`）。
- `.github/pull_request_template.md`、`.github/ISSUE_TEMPLATE/`、`.editorconfig`。
- CI 加固：Actions 钉 commit SHA、加最小 `permissions:`、加 `concurrency:` 取消过期跑。

**"你可能漏的"补充项**（并入 X1 或独立小 PR）：供应链 secret 扫描（gitleaks / GitHub secret scanning）、可选 SBOM、gradle wrapper checksum 校验、统一日志字段 + 收敛错误分类（后端 `classify_error.rs` 已有雏形）、可选 Conventional Commits/PR 校验、可选覆盖率报告。

---

## 5. PR 路线图（约 16–18 个小 PR，三阶段）

**阶段一 · 加法类（低冲突，先落地）**
- `B0` 后端全量 `cargo fmt`
- `B1` 后端质量门禁（toolchain/rustfmt/clippy/deny + CI）
- `A1` 安卓版本目录 `libs.versions.toml`
- `A2` 安卓静态检查 detekt+ktlint + CI
- `X1-backend` / `X1-android` 企业级文档与配置（各一）

**阶段二 · 依赖（保守）**
- `B4` 后端 cargo 补丁/小版本
- `A3` 安卓补丁/小版本

**阶段三 · 结构大改（单独小心做，做完尽快合）**
- `B5` 后端 sdk-bridge 收目录 + lint
- `B2` engine/ 归组到子模块（facade 保留导入路径）
- `B3` 拆 god-files（mod/store/stream/delegate/tests）
- `A-mod-0` → `A-mod-5` 安卓拆多模块（一串 PR）

阶段一/二内部 PR 相互独立、可并行推进与评审；阶段三串行、每个做完尽快合以缩短冲突窗口。

---

## 6. 风险与冲突管理

- **并发会话冲突**：阶段三的 `B2`/`B3` 和 `A-mod-*` 会大面积移动文件，与任何并发改动冲突面大。缓解：排最后、单独 PR、做完立即请评审合入；若 rebase 冲突，按 `CLAUDE.md` 约定停下交用户裁决，不强行解。
- **门禁卡红**：clippy/detekt 首开可能大量告警 → 用 baseline/棘轮，不阻塞。
- **依赖升级回归**：保守策略 + 每步跑测试；任何构建/测试变红即回退该条。
- **兼容性钉版被误升**：迁版本目录/升依赖时逐条核对现有注释，钉版线不动。
- **sdk-bridge 路径引用**：移动前先 grep `Makefile`/`main.rs`/部署脚本里的引用，同 PR 一并改。

---

## 7. 验证策略

| 仓库 | 每个 PR 必过 |
|---|---|
| server-rs | `cargo build` + `cargo test` + `cargo clippy -D warnings` +（B1 后）`cargo fmt --check` + `cargo deny check` |
| android | `./gradlew assembleDebug test lint` +（A2 后）`./gradlew detekt` |

结构类 PR 额外要求：diff 里除文件移动/`use` 路径调整外无逻辑改动（评审时可核对）。

---

## 8. 交付与评审

- 后端 PR 从本会话 `agentic/<session>` 分支切出，`gh pr create` targeting `agentic-dev` 默认分支；安卓同理 targeting `agentic-dev-android` 默认分支。
- 不自合并、不推默认分支；每个 PR 由用户评审批准。
- 冲突交用户裁决。
