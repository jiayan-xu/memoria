# 发版纪律（RELEASING）

> memoria 的版本管理规范。目标：让每个版本都可复现、可回溯、可审计。
> 原则一句话：**「bump 版本号 + 打 tag + 写 CHANGELOG」三步必须绑定同一次合并**，
> 缺一不可。

## 为什么要有这个文件

历史教训（2026-08-10 审计）：
- `Cargo.toml` 版本号在涨（0.2.0 -> 0.3.0），但**从未打 tag、从未写 CHANGELOG**，
  123 commits、几个 PR 之后，没有任何一个 tag 能指回某个可复现的发布点。
- memoria 有**硬门禁（HY3 G1-G4）**，可回溯性缺失在安全敏感库里不可接受。

## 版本号规则（SemVer）

- 格式 `X.Y.Z`，字段在 `Cargo.toml` 的 `[package] version`。
  - **X（major）**：破坏性 / 不兼容改动。
  - **Y（minor）**：新增功能。
  - **Z（patch）**：bug 修复。
- 当前基线：`0.3.0`。

## 发版三步（每次合并前完成）

1. **bump 版本号**：改 `Cargo.toml` 的 `version`，提交信息用
   `chore(release): bump memoria X.Y.Z -> X.Y.Z+1`。
2. **打 tag**：`git tag vX.Y.Z`（annotated tag，附发版说明），push 时 `--tags`。
3. **写 CHANGELOG**：在 `CHANGELOG.md` 顶部新增 `## YYYY-MM-DD — vX.Y.Z` 条目，
   按主题压缩本版本关键变更。

> ⚠️ 三步必须落在**同一个 feature 分支**上一起合入，禁止散落在不同 commit。

## 发版触发时机

- **必须发版**：非补丁级功能（新能力 / 安全边界改动 / 门禁改动）合入时。
- **建议发版**：累积 ≥ 20 个 commit 或跨一个自然周时。
- **延迟发版**：纯内部重构、无行为变化时，可攒到下一个功能版一起 bump。

## 分支与 PR 纪律（本仓库硬约束，见 docs/PR_PROCESS.md）

- 本仓库为**公开 GitHub 仓库**（`jiayan-xu/memoria`，默认分支 `main`）。
- **强制 PR 流程**：所有改动经 feature 分支（`feat/*`）提 PR 合入，禁止直推 main。
- 发版 commit 只含 `Cargo.toml` / `CHANGELOG.md` / `docs/*` 等元数据文件，
  **不得**混入业务代码改动。
- **禁止**在 commit 中带入密钥 / 隐私 / 绝对路径（`C:\Users\user\...`）。
- tag 是**不可变**锚点：打错 tag 只能删除重建，禁止 `--force` 覆盖已有 tag。

## 现有 tag 锚点

- `v0.2.0` = `7245341`（2026-07-05 初始发布）
- `v0.3.0` = `f4428bf`（2026-07-21 bump，当前 HEAD 基线）