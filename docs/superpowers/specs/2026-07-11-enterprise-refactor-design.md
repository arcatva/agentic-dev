# 企业级重构设计（后端 server-rs + 安卓 app）

- 日期：2026-07-11
- 状态：待用户复核（已含对抗性审查修订，见 §9）
- 范围：两个仓库 —— `agentic-dev`（Rust 后端 `server-rs`）与 `agentic-dev-android`（Kotlin/Compose）
- 交付方式：增量小 PR，每个只做一件事、尽量不改行为；每个 PR 由用户评审，不自合并

> 本文档是跨两仓库的**主路线图**。后端相关 PR 从本仓库分支切出；安卓相关 PR 从 `agentic-dev-android` 分支切出。安卓侧执行时会在其 `docs/` 放一份指针引用本文件。

---

## 1. 背景与目标

两个仓库功能完整、都有 CI 和 PR 评审流程，但离"企业级工程质量"还有系统性缺口。以下事实均经代码核实。

**后端 `server-rs`（Rust，约 3.2 万行）**
- `src/engine/` 是**扁平文件夹**，塞了 50+ 文件，无子模块分组。
- god-file：`mod.rs` 3322 行、`tests.rs` 单文件 5000 行、`store.rs` 1872、`stream.rs` 1323、`delegate.rs` 1246、`skill_install.rs` 897、`providers.rs`/`sdk_runner.rs` 831；`src/api/sessions.rs` 2151、`misc.rs` 1101。
- 缺工程门禁：无 `rust-toolchain.toml`、`rustfmt.toml`、clippy 配置、`cargo-deny`。全库仅 **2 处 `#[allow]`**；冷跑 `cargo clippy`（仅 `clippy::all`）约 **39 条告警 / 19+ 种 lint**，加 `pedantic` 约 **761 条**。→ clippy 门禁策略必须重新设计（见 §4.1）。
- 数据层用 sqlx **运行时** `query(...)`（43 处），**无编译期宏**，故拆 `store.rs` 不受 `DATABASE_URL`/离线数据牵制；但**无 `migrations/` 目录**，schema 内联创建——本身是缺口。
- 嵌入式 Node 组件 `sdk-bridge.mjs`（557 行）+ `package.json`/`package-lock.json` 躺在 crate 根，无独立 lint/format、**无自身测试**（现有测试只用 `fake-sdk-bridge-*.sh` 验证 Rust 侧）。

**安卓 `agentic-dev-android`（Kotlin/Compose，单 `:app` 模块）**
- 分层已清晰（`data/{net,repo,log,util}`、`domain/`、`ui/<feature>/`、`di/`、`voice/`），测试约 60 个。
- 缺工程卫生：无版本目录（`libs.versions.toml`），无静态检查（detekt/ktlint），单模块，手写 DI。
- **DI 是硬耦合**：`di/ViewModels.kt` 与 `AgenticMessagingService.kt` 都 `applicationContext as AgenticApp` 取 `container`，`appContainer()` 在 **16 个文件**里被调用（几乎每个 feature 包）。→ 直接拆模块会造成 `:feature:* → :app` 反向依赖、Gradle 循环（见 §4.8）。
- **共享测试假件**：`FakeAgenticApi/FakeLanScanner/FakeSettingsStore` 在 `app/src/test`，被 **20 个测试**引用。→ 需 `:core:testing` 模块。
- **供应商原生库**：`app/src/main/jniLibs/arm64-v8a/` 提交了 `libonnxruntime.so`(25.8 MB) + `libsherpa-onnx-jni.so`(4.7 MB)，**无版本记录/校验和/许可声明/来源说明**（sherpa-onnx=Apache-2.0，onnxruntime=MIT，均需随应用附带署名）。
- **API 契约手工对齐**：后端 `api/` 约 52 处路由 ↔ 安卓 `data/net/Models.kt`(639 行)，无 OpenAPI/codegen/契约测试。
- release 构建 `isMinifyEnabled = false`（无 R8/资源压缩/混淆规则）。

**目标**：在不牺牲现有行为与刻意兼容性钉版前提下，把两仓库的**目录结构、依赖治理、代码质量门禁、企业级横向配套**系统性补齐，全部通过一串可评审的小 PR 落地。

---

## 2. 范围

### 纳入（用户已确认全选）
1. 后端结构重整（`engine/` 归组 + 拆 god-files）
2. 质量门禁（后端 Rust 工具链/lint/供应链；安卓版本目录/静态检查）
3. 依赖更新（**保守**：只补丁/小版本，守住刻意钉版）
4. 企业级横向补齐（Dependabot、CODEOWNERS、SECURITY、CONTRIBUTING、ARCHITECTURE、模板、CI 加固）
5. **安卓拆多模块**（`:app` → `:core:*` + `:feature:*`，用户确认现在就做）
6. （对抗性审查新增）API 契约防漂移、jniLibs 许可/来源治理、sqlx 迁移纪律、sdk-bridge 测试

### 明确不纳入 / 单独评估
- 破坏性大版本依赖升级（Ktor 2→3、Rust edition 2021→2024、Compose/material3 换线、AGP 大升）——单独评估。
- 重写业务逻辑 / 改功能行为。**例外（已接受的必要逻辑改动）**：安卓 DI 解耦（§4.8 的 `A-mod-DI`）不是纯搬移，是为打破 `AgenticApp` 强转循环而做的一次受控重写，test-first。
- 引入 Hilt：**不作为目标**。DI 解耦用"把容器下沉到 `:core:di` + 中性访问器"解决，不引 Hilt。
- 安卓 R8/minify 开启：**单独评估 PR**（需为 kotlinx-serialization/Ktor/Firebase 补 keep 规则，风险独立），不并入结构改造。
- 触碰 `CLAUDE.md` 工作流约定。

### 用户已拍板的三个决策
- **后端结构大改顺序**：排最后、单独做。
- **rustfmt 全量格式化**：单独一个纯格式化 PR 先落地，之后 CI 才开 `fmt --check`。
- **安卓多模块**：现在就拆。

---

## 3. 指导原则

1. **一 PR 一事、优先不改行为**：结构调整以"搬移 + `re-export`/facade 保持导入路径"为主，逻辑零改动。唯一的例外是安卓 DI 解耦（§4.8），它是明确标注、test-first 的受控逻辑改动。
2. **门禁落地要区分工具能力**：
   - **detekt（安卓）有原生 baseline** → 首开生成 baseline，CI 只卡新增，存量另清。
   - **clippy（Rust）无 baseline** → 不能"只卡新增"。策略改为：先落 `B1b` 清掉 39 条 `clippy::all` 存量告警（或对个别 lint 加 crate 级 `#![allow]` 并记为技术债），**清完再把 CI 翻成 `-D warnings`**。`pedantic` 不整体开启（761 条会淹没信号），只精选少数高价值 lint 显式启用。
3. **加法先行、结构大改殿后**：低冲突加法类 PR 先落地；高冲突结构/拆模块大改放最后、单独做、做完尽快合。
4. **守住刻意钉版**：迁版本目录/升依赖逐条保留现有兼容性注释（Ktor 2.x、Compose 1.9.x、material3 1.4.0-alpha18、AGP 8.7.2、Kotlin 2.0.21、lifecycle 2.8.7 lint 禁用等）。
5. **保护 API 契约缝**：后端 `api/` 与安卓 `Models.kt` 是全计划回归风险最高处。动它的 PR（`B3`、`A-mod-2`）必须在 `C1`（契约测试/schema，见 §4.10）落地之后进行，或与之同 PR。
6. **每个 PR 自带验证**：后端 `cargo test` + `cargo clippy`；安卓 `./gradlew test lint detekt`。绿了才请评审。

---

## 4. 分工作线设计

### 4.1 后端质量门禁（B0 格式化 → B1 门禁 → B1b clippy 清债）
- `B0`：全库 `cargo fmt`（机械 diff、快速合）。
- `B1`：新增 `rust-toolchain.toml`（钉 stable + `rustfmt/clippy/rust-src` 组件，确立 MSRV）、`rustfmt.toml`、`Cargo.toml` `[lints]`（clippy 存量告警对应的 lint 先设 `warn`；`fmt --check` 此时可入 CI 门禁）、`deny.toml`。CI 加 `fmt --check`。
- `B1b`：清掉 39 条 `clippy::all` 告警（真修，不是掩盖；确需保留的极少数用局部 `#[allow]` 并注明原因）。清完把 CI 的 clippy 翻成 **`-D warnings`** 作为合并门禁。
- **cargo-deny 分级**：`licenses` + `bans` 作合并门禁；**`advisories` 不作硬门禁**（RustSec 新增公告会无差别红掉无关 PR）——放**定时任务**（每日 `schedule:`）或 `continue-on-error: true` 的独立 job，只提醒不拦合并。

### 4.2 后端依赖（B4，保守）
- 逐条 `cargo update` 到兼容补丁/小版本，不动大版本；每次 `cargo test` + `cargo clippy` 验证。

### 4.3 后端嵌入 Node 桥（B5）
- 把 `sdk-bridge.mjs` + `package.json`/`package-lock.json` 收进子目录（移动前先 grep `Makefile`/`main.rs`/`deploy`/`scripts` 里的路径引用，同 PR 一并改）。
- 加 `prettier` + `eslint`(flat config) + `.editorconfig`，纳入 CI + Dependabot npm。
- **加最小 smoke 测试**：给 bridge 至少一条"能起、能握手、错误路径"的 Node 测试，别让它只在生产里被跑到。

### 4.4 后端结构重整（阶段三，B2 归组 + B3 拆分，最后单独做）
`engine/mod.rs` 收成薄 facade（共享类型 + `pub use <sub>::*` 保持导入路径），50+ 文件收进约 10–13 个内聚子模块。

建议分组（最终边界在 B2 实施时用耦合分析校验，可能微调）：

| 子模块 | 现有文件（示意） |
|---|---|
| `lifecycle/` | lifecycle, auto_resume, recover, resume_gate, watchdog, status, transition, groups |
| `runtime/` | spawner, runner, sdk_runner |
| `transcript/` | transcript, native_transcript, transcript_filter, tailer, usage |
| `streaming/` | stream（拆分，见 B3） |
| `store/` | store（拆分，见 B3）, atomic_write |
| `config/` | global_settings, user_config, templates, session_guide |
| `providers/` | providers, router, litellm |
| `workflows/` | workflows, delegate（拆分，见 B3） |
| `vcs/` | worktree, repos, diff, structured_diff |
| `skills/` | skills, skill_install, plugins, plugin_cli, components |
| `title/` | title, title_client |
| `notify/` | push |
| `error/` | error, classify_error |
| `search/` `mentions/` | search, mentions |

**god-file 拆分（B3）**：
- `mod.rs`(3322)：`Engine` 的 `impl` 按职责切进 `lifecycle/{turn,queue,session_admin,fork}.rs`；共享类型进 `lifecycle/types.rs`；`mod.rs` 仅留 facade。
- `store.rs`(1872)：按 schema/迁移 / 读 / 写 / 查询切分（sqlx 运行时查询，无编译期宏牵制）。
- `stream.rs`(1323)、`delegate.rs`(1246)：按事件解析 / 传输 / 编排切分。
- `tests.rs`(5000 内联单测)：**先核对它引用了哪些私有内部项**（B2 前用 grep 摸清），拆成与被测模块同名的 `#[cfg(test)]` 子模块就近放置；纯端到端用例迁 `tests/`。
- **可见性风险处理**：move 前 grep `pub(crate)`/`pub(super)`/`pub(in ...)`，跨子模块移动后按需把私有项提升为 `pub(crate)`，保证 `tests.rs` 与相邻模块仍可见；每步 `cargo test` + `cargo build` 全绿。

### 4.5 安卓版本目录（A1）
- 新建 `gradle/libs.versions.toml`，迁入所有依赖/插件，**版本一字不改、保留兼容性注释**；`app/build.gradle.kts` 瘦身为别名引用。行为零变化。

### 4.6 安卓静态检查（A2，依赖 A1）
- detekt（有 baseline）+ ktlint/Spotless，为存量生成 baseline，CI 只卡新增；接入 `ci.yml`。

### 4.7 安卓依赖（A3，保守）
- 仅在钉定版本线内升补丁/小版本；每次 `./gradlew test lint` 验证。

### 4.8 安卓多模块（阶段三，用户确认现在就拆）

> 修订要点：拆模块**不是纯搬移**。先解决 DI 循环与共享测试件两个硬阻断，模块图新增 `:core:di` 与 `:core:testing`。

**目标模块图**：
- `:app` —— `MainActivity`、`AgenticApp`、`AgenticMessagingService`、`ui/nav`、导航装配。壳。
- `:core:di` —— `AppContainer` + 中性访问器（见 `A-mod-DI`）。打破 `AgenticApp` 强转循环的关键模块。
- `:core:model` —— `domain/` 纯数据类型。
- `:core:common` —— `data/util`、`Outcome`、`Polling`、`RelativeTime`、`data/log/*`。
- `:core:domain` —— `domain/` 纯变换（AskParsing、CommitGraph、SessionSearch、TranscriptReducer…）。
- `:core:network` —— `data/net/*`。
- `:core:data` —— `data/repo/*`、`SettingsStore`、`SessionUiStore`。
- `:core:designsystem` —— `ui/components/*`、`Theme`、`Motion`、`Markdown`、图标资源。
- `:core:voice` —— `voice/*` + `com.k2fsa.sherpa.onnx` + `jniLibs`（+ 许可/来源治理，见下）。
- `:core:testing` —— 共享 `Fake*`（`test`/`testFixtures`），供 20 个测试复用。
- `:feature:*` —— home、session、login、providers、workflow、tree、diagnostics、globalsettings、adopt、newrequest。
- `build-logic/` —— convention plugins（android-application/library、compose、kotlin、detekt），消除 build 文件重复。

**分步交付（一串小 PR，A1 先落）**：
1. `A-mod-DI`（**受控逻辑改动，test-first，先做**）：把 `AppContainer` 下沉、用中性访问器（`Application` 接口 / `androidx.startup` Initializer / CompositionLocal）取代 16 处 `applicationContext as AgenticApp`，`AgenticMessagingService` 一并改。改完 App 单模块内仍应全绿，再谈拆分。
2. `A-mod-0`：`build-logic` + convention plugins 脚手架（`:app` 改用约定插件，不搬业务文件）。
3. `A-mod-1`：抽 `:core:model` + `:core:common` + `:core:testing`。
4. `A-mod-2`：抽 `:core:network` + `:core:data`（**须在 §4.10 `C1` 契约测试之后**）。
5. `A-mod-3`：抽 `:core:domain` + `:core:designsystem` + `:core:di` + `:core:voice`。
6. `A-mod-4`：抽 `:feature:*`（按功能分几个 PR）。注意 `ui/nav/AppNav.kt` 的跨功能导航——feature 间不得相互直依赖，导航路由集中在 `:app` 或 `:core` 的 nav 契约层，避免 `:feature:*` 环。
7. `A-mod-5`：瘦身 `:app` 收尾。

每步 `./gradlew assembleDebug test` 全绿；测试随源码迁到对应模块。

**jniLibs 许可/来源治理（并入 `A-mod-3` 的 `:core:voice` 或独立小 PR）**：
- 加 `jniLibs/README.md` 记录 `libonnxruntime.so`/`libsherpa-onnx-jni.so` 的**上游版本 + SHA256 + 获取/编译来源**。
- 加 `THIRD-PARTY-NOTICES`（sherpa-onnx Apache-2.0 + onnxruntime MIT 署名）。
- 评估：用 Gradle 任务**按校验和拉取** .so，替代把 30 MB 二进制提交进 git（减仓库膨胀 + 可核验来源）。

### 4.9 企业级横向补齐（X1-backend / X1-android，各一）
- `.github/dependabot.yml`：后端 = cargo + npm(sdk-bridge) + github-actions；安卓 = gradle + github-actions。**配 grouping**（按生态/周批），避免 PR 洪水。
- `CODEOWNERS`、`SECURITY.md`（写清 HMAC 鉴权 + 自签 TLS 模型 + 漏洞上报渠道，**不含任何密钥**）、`CONTRIBUTING.md`、`ARCHITECTURE.md`（复用/提炼后端 `docs/internals.md`）。
- `.github/pull_request_template.md`、`ISSUE_TEMPLATE/`、`.editorconfig`。
- **CI 加固**：Actions 钉 commit SHA；加最小 `permissions:`——但 **`release.yml` 需要 `contents: write`**（发布 release/上传产物），须逐 workflow 单独给权，不能一刀切只读（实施时读 `release.yml` 确认所需权限）；加 `concurrency:` 取消过期跑。
- 补充项：secret 扫描（gitleaks / GitHub secret scanning）、gradle wrapper checksum 校验、可选 SBOM、可选 Conventional Commits/覆盖率。

### 4.10 API 契约防漂移（C1，跨仓库，**排在 B3/A-mod-2 之前**）
- 后端加**契约快照测试**：对约 52 条路由的响应形状生成 JSON fixtures / JSON-Schema（或最小 OpenAPI），集成测试断言 handler 输出符合快照。
- 安卓侧用**同一份 fixtures** 反序列化进 `Models.kt` 类型做测试，形成两端共享的契约。
- 目的：让后续动 `api/`（B3）和 `data/net`（A-mod-2）时，任何请求/响应结构漂移立刻被测试抓到。这是全计划回归风险最高缝的护栏。

### 4.11 后端 sqlx 迁移纪律（并入 B3 或独立小 PR）
- 现无 `migrations/`、schema 内联创建。拆 `store.rs` 时把建表/演进收敛到 `migrations/`（sqlx migrate 或显式版本化），并记录既有 schema 为初始迁移，避免拆分过程中 schema 语义丢失。

---

## 5. PR 路线图（约 20 个小 PR，三阶段）

**阶段一 · 加法类（低冲突，先落地）**
- `B0` 后端全量 `cargo fmt`
- `B1` 后端门禁（toolchain/rustfmt/deny + clippy 存量设 warn + `fmt --check` 入 CI）
- `B1b` 后端 clippy 清 39 条存量告警 → CI 翻 `-D warnings`
- `A1` 安卓版本目录
- `A2` 安卓静态检查 detekt+ktlint + CI
- `C1` API 契约快照测试（两仓库，护栏；早落）
- `X1-backend` / `X1-android` 企业级文档与配置（各一）

**阶段二 · 依赖（保守）**
- `B4` 后端 cargo 补丁/小版本
- `A3` 安卓补丁/小版本

**阶段三 · 结构大改（单独小心做，做完尽快合；均在 C1 之后）**
- `B5` 后端 sdk-bridge 收目录 + lint + smoke 测试
- `B2` engine/ 归组（facade 保留导入路径）
- `B3` 拆 god-files（mod/store/stream/delegate/tests）+ sqlx 迁移收敛（§4.11）
- `A-mod-DI`（DI 解耦，受控逻辑改动，先于其它 A-mod）
- `A-mod-0…5` 安卓拆多模块（含 jniLibs 治理）

阶段一/二内部相互独立、可并行；阶段三串行、尽快合。`C1` 必须先于 `B3`、`A-mod-2`。

---

## 6. 风险与冲突管理
- **clippy 无 baseline**：靠 `B1`（存量设 warn，不拦合并）+ `B1b`（清完再 deny）分两步，避免门禁一开红全场。
- **cargo-deny advisories 误伤**：设为定时/非拦截 job。
- **安卓 DI 循环**：`A-mod-DI` 必须先行且 test-first；否则任何 `:feature:*` 抽取都会造成 `→ :app` 环、构建失败。
- **API 契约漂移**：`C1` 护栏先落，`B3`/`A-mod-2` 才动 `api/`、`data/net`。
- **并发会话冲突**：阶段三大移动排最后、单独 PR、做完立即请评审合入；rebase 冲突按 `CLAUDE.md` 停下交用户裁决。
- **CI 权限收紧误伤 release**：逐 workflow 配 `permissions`，`release.yml` 保留 `contents: write`。
- **依赖钉版被误升**：迁目录/升级逐条核对注释。
- **jniLibs 许可缺失**：`A-mod-3` 内补 README/SHA256/NOTICES。

## 7. 验证策略
| 仓库 | 每个 PR 必过 |
|---|---|
| server-rs | `cargo build` + `cargo test` + `cargo fmt --check`（B1 后）+ `cargo clippy`（**`-D warnings` 仅在 B1b 之后作门禁**）+ `cargo deny check licenses bans` |
| android | `./gradlew assembleDebug test lint` + `./gradlew detekt`（A2 后，卡新增）|

结构类 PR 额外要求：diff 除文件移动/`use`/`import` 与（DI/契约）明确标注的改动外无逻辑变化，评审可核对。

## 8. 交付与评审
- 后端 PR 从会话 `agentic/<session>` 分支切出 → `gh pr create` targeting `agentic-dev` 默认分支；安卓同理 targeting `agentic-dev-android`。
- 不自合并、不推默认分支；每个 PR 用户评审。冲突交用户裁决。

---

## 9. 对抗性审查发现与修订（红队 → 本次改动）
4 路红队从"结构可行性 / 安卓模块化 / 顺序门禁 / 企业级完整性"反驳初版，经代码核实后采纳如下：

| # | 发现（已核实） | 对规格的修订 |
|---|---|---|
| 1 | clippy 无 baseline；冷跑 39 条(all)/761 条(pedantic)，全库仅 2 处 `#[allow]` | §3.2、§4.1 拆出 `B1b` 清债后再 `-D warnings`；pedantic 不整体开 |
| 2 | 安卓 DI 强转 `AgenticApp`，16 文件调用 → 拆模块必造成 `:feature:*→:app` 环 | §4.8 新增 `:core:di` + `A-mod-DI` 受控解耦，标注为唯一逻辑改动 |
| 3 | 20 个测试共享 `Fake*` 假件，原图无处安放 | §4.8 新增 `:core:testing` 模块 |
| 4 | jniLibs 30 MB 二进制无版本/校验和/许可 | §4.8 加 README+SHA256+THIRD-PARTY-NOTICES，评估校验和拉取 |
| 5 | API 契约手工对齐（52 路由 ↔ Models.kt 639 行），B3/A-mod-2 会动它 | 新增 §4.10 `C1` 契约测试，排在 B3/A-mod-2 之前 |
| 6 | cargo-deny advisories 会无差别拦无关 PR | §4.1 advisories 改定时/非拦截 |
| 7 | CI 一刀切只读权限会破坏 release.yml | §4.9 逐 workflow 配权，release 保留 `contents: write` |
| 8 | sdk-bridge.mjs 无自身测试；无 sqlx migrations | §4.3 加 smoke 测试；§4.11 收敛迁移 |
| 9 | 安卓 release `isMinifyEnabled=false` | §2 列为单独评估 PR（需 keep 规则），不并入结构改造 |
