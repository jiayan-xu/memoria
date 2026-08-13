# PR 流程规定（Pull Request Workflow）

> 适用仓库：GitHub `jiayan-xu/memoria`（本地 canonical 见 AGENTS.md）
> 更新日期：2026-08-13
> 状态：**强制执行**（pre-push hook 已机械拦截直推默认分支）
>
> 默认分支是 **`main`**（不是 `master`）。本文示例一律走 `main`。

---

## 1. 为什么必须走 PR

**背景**：GitHub 的 `required status checks` 只对 **PR 合并**强制，**不拦截直接 push 到默认分支**——
直接 `git push origin main` 永远放行（仅提示 `Bypassed rule violations`），CI 红代码照样进主分支。

**目的**：让 `ocr-review` + `gitleaks` + `build` 门禁在代码进入 **main** **之前**拦截，而不是事后检查。

---

## 2. 铁律（P0）

1. **禁止直接 push 到 `main`**（pre-push hook 已拦截，`--no-verify` 绕过被记录为违规）。
2. **所有改动必须走 PR**：feature 分支 → push → 开 PR → CI 通过 → 合入 main。
3. **PR 合入时 required checks 必须全绿**（build×3 + ocr-review + gitleaks，共 5 个），否则合并按钮灰掉。
4. **禁止 force push 到 main / 删除分支**（分支保护已开，hook 也拦）。
5. **开 PR 后必须前台轮询进展**（`gh pr checks --watch`）。禁止开完 PR 就结束对话、把「等 CI」甩给用户。绿了立刻汇报；红了立刻进入修复闭环。

---

## 3. 标准操作流程

### 3.1 新建 feature 分支（从最新 main 切出）

```bash
git fetch origin
git checkout -b feat/<简述> origin/main   # 如 feat/write-intent-cap
```

### 3.2 开发 + 本地验证（和以前一样）

```bash
cargo check          # 编译
cargo test --lib     # 测试全绿
git diff | grep -E "(api_key|admin_key|token|secret|password)...C:\\\\Users"   # 密钥扫描，无输出=干净
git add <files>
git commit -m "feat(xxx): 一句话" -m "- 要点列表"
```

### 3.3 推送 feature 分支（hook 放行：非默认分支）

```bash
git push origin feat/<简述>
# hook 只审查该分支相对 origin/main 的增量，有评论会拦截
```

### 3.4 开 PR + 前台轮询 CI

```bash
# 用 gh CLI 或网页：feat/<简述> → main
gh pr create --base main --title "..." --body "..."

# 前台阻塞轮询该 PR 的 required checks（不要 gh run list --limit 1，会误拿并发会话）
gh pr checks --watch
```

预估：ocr-review + gitleaks 约 1–3 分钟；build×3 约 5–15 分钟。watch 结束前不要结束对话。

PR 页面确认（5 个 required checks）：
- `build` (ubuntu / macos / windows) ✅
- `ocr-review` ✅
- `gitleaks` ✅
- 有红灯 → 立刻提取意见 → 修 → push 同一 feat 分支（PR 自动刷新）→ **再前台 watch**，直到全绿

### 3.5 合并

```bash
gh pr merge --merge   # 或网页点 Merge
```

---

## 4. 异常情况

| 情况 | 处理 |
|---|---|
| **hook 拦了但确实要直推**（如紧急回滚） | `git push --no-verify`，但**必须在 PR 描述/commit 里说明理由**，事后补 PR |
| **PR CI 卡在 in_progress 很久** | 前台继续 `gh pr checks --watch`；看 Actions 日志区分 runner 排队 vs API 超时。给时间预估，不要把等待甩给用户 |
| **只想改文档/注释** | 同样走 PR（小改动用 `feat/docs-*` 分支；hook 只放行 `feat/*`，`docs/` 前缀会被拦） |
| **多人协作** | PR 里 @ 协作者 review；main 只有维护者能合并 |

---

## 5. hook 行为说明

`.githooks/pre-push`（`git config core.hooksPath .githooks` 激活后）：

| 推送到 | 行为 |
|---|---|
| `main`（默认分支） | **BLOCKED**：要求走 PR（本文件第 3 节） |
| 其他分支（`feat/*` 等） | 放行 + 跑 ocr-review 审查（OCR_GATE=1 时有评论即拦截） |
| `gitee` 私有镜像 | BLOCKED（开源内容永不进私有镜像，见 AGENTS.md） |
| 删除分支 / force push | BLOCKED |

---

## 6. 与旧流程的差异

| | 旧（2026-08-07 前） | 新（本文件） |
|---|---|---|
| main 直推 | ✅ 允许（hook 只验分支名） | ❌ 禁止（hook 拦截） |
| 门禁时机 | push 后事后检查 | PR 合并前强制 |
| feature 分支 | 不允许存在 | ✅ 正常流程 |
| 紧急绕过 | — | `--no-verify` + 说明理由 |
