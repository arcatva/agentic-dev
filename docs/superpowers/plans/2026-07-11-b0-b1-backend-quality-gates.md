# B0 + B1 — 后端质量门禁 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / executing-plans. Steps use `- [ ]`.
>
> 本仓库（agentic-dev）走自己的 PR 流程（见仓库 CLAUDE.md「auto-merge after Codex review」）：提交前用 `delegate` 对抗验证 → 开 PR → Codex 审 → 逐条处理 → `gh pr merge --rebase` 自合并（硬底线是能编译）。B0 与 B1 是**两个独立 PR**，B0 先合、B1 基于新 master。

**Goal:** 给 Rust 后端建立格式化与静态检查/供应链门禁，且不因存量告警阻塞后续 PR。

**Tech Stack:** Rust 1.96（系统工具链，无 rustup）、cargo、rustfmt、clippy、cargo-deny（CI 内装）。CI = `.github/workflows/ci.yml`。

## Global Constraints
- edition 保持 `2021`；不改任何业务逻辑；不动 `sdk-bridge.mjs`。
- **clippy 首期只 `warn`、不 `-D warnings`**（全库仅 2 处 `#[allow]`、冷跑 39 条 all / 761 条 pedantic；`-D` 留给后续 B1b 清债后再开）。
- cargo-deny：`licenses` + `bans` + `sources` 作门禁；`advisories` **不拦合并**（放独立定时/`continue-on-error`）。
- 合并硬底线：`cd server-rs && cargo build` 通过。
- CI 现状（务必先读）：单 job `test`，`actions/checkout@v4`(persist-credentials:false) → `dtolnay/rust-toolchain@stable` → `Swatinem/rust-cache@v2`(workspaces: server-rs) → `cargo test`(working-directory: server-rs)。

---

## PR B0 — 全量格式化（含 rustfmt.toml）

> 顺序要点：`rustfmt.toml` 必须**先于**格式化存在，否则 B1 再改格式规则会导致二次重排。故 B0 = 加 rustfmt.toml + 用它跑 `cargo fmt`。仅用 **stable** rustfmt 选项（系统工具链非 nightly，import 分组等 unstable 选项在 stable 上会被忽略，避免本地与 CI 不一致）。

### Task B0.1: 加 `server-rs/rustfmt.toml`（stable 选项）
**Files:** Create `server-rs/rustfmt.toml`
- [ ] **Step 1:** 写入（全部为 stable 选项）：
```toml
# rustfmt 团队配置（stable 选项，系统工具链即可校验）。
# import 分组/合并等 unstable 选项需 nightly，暂不启用以保证 stable 上本地与 CI 一致。
edition = "2021"
max_width = 100
newline_style = "Unix"
use_small_heuristics = "Default"
```
- [ ] **Step 2:** 无需构建，进入下一 Task。

### Task B0.2: 全量格式化并提交
- [ ] **Step 1: 跑格式化**
Run: `cd server-rs && cargo fmt`
- [ ] **Step 2: 确认仅格式变化、可编译（合并硬底线）**
Run: `cd server-rs && cargo build 2>&1 | tail -5`
Expected: 编译通过。
- [ ] **Step 3: 确认 fmt 幂等（再跑 --check 应干净）**
Run: `cd server-rs && cargo fmt -- --check; echo "EXIT=$?"`
Expected: `EXIT=0`（已格式化，无残余）。
- [ ] **Step 4:（可选但建议）测试仍绿**
Run: `cd server-rs && cargo test 2>&1 | tail -15`
Expected: 全过（tests 走 fake bridge，无 API 成本）。
- [ ] **Step 5: 对抗验证（仓库规则，提交前）**
用 `delegate` 派 1–2 个 worker（`model` 留空、给 `title`+`phase`），角度：「这个 diff 是否**只有**格式化、有无任何语义/token 变化被 rustfmt 意外引入」。确认纯格式化后再提交。
- [ ] **Step 6: 提交**
```bash
git add server-rs/rustfmt.toml server-rs/src
git commit -m "style(server-rs): add rustfmt.toml and apply cargo fmt

Formatting-only pass under a stable rustfmt config. No logic changes;
cargo build passes. Precedes the quality-gate PR (B1) so fmt --check is green.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
- [ ] **Step 7: 开 PR → Codex → 处理意见 → `gh pr merge --rebase`**（按仓库流程）。

---

## PR B1 — 门禁配置 + CI 接入（基于已合入 B0 的 master）

### Task B1.1: `rust-toolchain.toml`（钉工具链）
**Files:** Create `server-rs/rust-toolchain.toml`
- [ ] **Step 1:** 写入：
```toml
# 钉工具链，确保每个开发者/CI/worktree 用同一编译器；确立 MSRV 基线。
# 有意升级时改 channel。CI 与本地保持一致。
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
```

### Task B1.2: `Cargo.toml` 加 `[lints]`（clippy 只 warn）
**Files:** Modify `server-rs/Cargo.toml`（在 `[package]` 之后、`[dependencies]` 之前插入）
- [ ] **Step 1:** 插入：
```toml
[lints.rust]
unused_must_use = "warn"

[lints.clippy]
# 首期只 warn：存量告警不阻塞合并；清债后（B1b）在 CI 开 -D warnings。
all = { level = "warn", priority = -1 }
```
- [ ] **Step 2: 确认仍编译**
Run: `cd server-rs && cargo build 2>&1 | tail -5` → 通过。

### Task B1.3: `deny.toml`（cargo-deny）
**Files:** Create `server-rs/deny.toml`
- [ ] **Step 1:** 写入一个最小可用配置：`[licenses]` 允许常见许可（MIT/Apache-2.0/BSD/ISC/Unicode-3.0 等）、`[bans]` 基本、`[sources]` 仅允许 crates.io、`[advisories]` 设为不因新公告失败（版本化后由定时 job 检查）。具体许可清单在实施时用 `cargo deny check licenses 2>&1` 的报告补齐到「allow」列表，直到 licenses 通过。

### Task B1.4: CI 接入
**Files:** Modify `.github/workflows/ci.yml`
- [ ] **Step 1:** 把 toolchain 步骤改为装 rustfmt+clippy 并新增 lint job（保持既有 `test` job 不变）：
  - 将 `dtolnay/rust-toolchain@stable` 改为固定版 + 组件：
    ```yaml
    - uses: dtolnay/rust-toolchain@master
      with:
        toolchain: 1.96.0
        components: rustfmt, clippy
    ```
  - 新增独立 `lint` job：`cargo fmt --check`、`cargo clippy --all-targets`（**不加 -D**）、`EmbarkStudios/cargo-deny-action`（`command: check licenses bans sources`）。
  - 新增 `permissions: contents: read`（最小权限）与 `concurrency`（取消过期跑）。**不动 `release.yml`**（它需要 `contents: write`）。
- [ ] **Step 2: 本地能跑的都验证**
Run: `cd server-rs && cargo fmt --check; echo fmt=$?; cargo clippy --all-targets 2>&1 | tail -5`
Expected: fmt 干净；clippy 有告警但退出 0（未加 -D）。
- [ ] **Step 3: 对抗验证（delegate，提交前）**：角度「CI 改动是否会误伤 release.yml / 是否引入会拦合并的门禁 / deny.toml 许可清单是否漏项」。
- [ ] **Step 4: 提交 → PR → Codex → 处理 → 自合并**
```bash
git add server-rs/rust-toolchain.toml server-rs/Cargo.toml server-rs/deny.toml .github/workflows/ci.yml
git commit -m "ci(server-rs): pin toolchain, add clippy(warn)/rustfmt/cargo-deny gates

toolchain 1.96.0; Cargo [lints] clippy=warn (not deny — backlog cleared in B1b);
deny.toml licenses+bans+sources gate, advisories non-blocking; new lint job with
least-privilege permissions. release.yml untouched.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## 后续（不在本批）
- **B1b**：清 39 条 `clippy::all` 存量告警 → CI 的 clippy 加 `-D warnings` 转为合并门禁。
- **B4/B5/B2/B3**：见 §4 路线图，各自出计划。

## Self-Review（对照 spec §4.1）
- 覆盖：格式化（B0）、toolchain/rustfmt/clippy-warn/deny + CI（B1）、clippy 无 baseline 用两步（B1 warn → B1b deny）、advisories 非拦截、release.yml 不动、最小权限。✅
- 无占位；clippy 只 warn 的约束在 Global Constraints 显式化。✅
