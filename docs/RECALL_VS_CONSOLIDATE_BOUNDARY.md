# 召回（Recall）vs 整合（Consolidate/Dream）— 职责边界

> 版本: 2026-08-14（B3，`docs/OPTIMIZATION_OPENCLAW_ABSORPTION.md` Phase B 文档化项）
> 依据: HY3 执行单（`MEMORIA_INTEL_HY3_EXEC.md`）H1/H2 硬约束 + 现网代码事实。
> 目的: 明确「什么时候走召回、什么时候走整合」，防止把整合逻辑搬进热路径、或把召回能力拆进批处理。

---

## 0. 一句话

**召回是只读热路径（用户问 → 立刻答），整合是批处理（夜间/空闲 → 记忆自我精炼）。两者只共享同一存储，不共享执行时机。** Memoria 是薄存储（H1/H2）：检索与存取在 memoria，演化与护栏的「脑子」在 agent-core。

---

## 1. 召回侧（读取热路径）

| 入口 | 用途 | 代码路径 |
|---|---|---|
| `memory_context` | 会话开场画像注入（静态+动态 profile，`as_of` 可选） | `tools/profile.rs`（`memory_profile`） |
| `memory_recall` | 语义回忆（当前真值，自动过滤 superseded/过期） | `mcp_server.rs` → hybrid |
| `memory_search` / `memory_search_v2` | 显式检索（可 `as_of` 历史窗口、`enrich_ledger`、`include_superseded`） | `mcp_server.rs` → hybrid |

融合引擎（`src/search/hybrid.rs`）：
- 5 信号：keyword(FTS5) + semantic(HNSW) + temporal + importance + category → RRF 融合
- 2-hop 图扩展（`rrf.rs`：`semantic_related` > `updates` > `same_entity` > `chronological`，类型配额 + weight 降序）
- `text_signals` 数字/日期重叠加成（`search/text_signals.rs`，`MEMORIA_TEXT_SIGNALS_RERANK=0` 可关）
- 时序过滤：`as_of=None` → `is_latest_now`（当前真值）；`as_of=T` → `visible_as_of`（`valid_from/valid_to` 窗口）
- 演化脏标记：`evolved_at IS NULL` 候选可降权/标注（PR4）

**原则：召回路径不写库、不调 LLM、不触发演化。** 任何「读取时顺便提炼/整合」的诉求都进 §3，不回读路径。

---

## 2. 写路径（入口，非整合）

| 入口 | 用途 | 备注 |
|---|---|---|
| `memory_observe` | 轻量观察（对话流水） | 可走 `distill` 压缩，原文进 `raw_ref` |
| `memory_remember` | 持久记忆（含去重） | agent-core 侧可挂**写前提取门**（PR2，`AGENT_MEMORY_EXTRACT`）→ 1 raw → N 原子事实 |

**原则：写路径允许确定性预处理（去重、提取门、text_signals tags），禁止同步 LLM 演化。** `remember_with_dedup` 内不调 LLM（H1）。

---

## 3. 整合侧（批处理 / 夜间）

| 环节 | 执行者 | 内容 | 触发 |
|---|---|---|---|
| `consolidate` | agent-core | 消化 observation 积压 → 提炼/合并 | 空闲 tick + 夜间 patrol |
| `memory_evolve` | agent-core → memoria | LLM 决定是否更新旧记忆 → 写 `evolved_context`/`link_count` + `evolution_log` | consolidate 批处理内（`agent.rs`，`agent_memory_evolve_enabled()` 默认开） |
| `memory_evolve_auto` | memoria 内置 | 确定性「提升式」演化（`memoria:builtin-auto`，零 LLM，幂等只处理 `evolved_at IS NULL`） | 周期/事件（曾由 DSH 维护插件、现由 agent-core 夜间 patrol 调度） |
| `meta_evolution`（L2） | agent-core | 用 `evolution_log` 负样本 + `experience_memo` 评估演化质量并自改进 | 夜间 patrol，`[meta_evolution]` 显式开启，cooldown 限流 |
| `memory_decay` / `memory_backup` | agent-core | 衰减 + GFS 备份 | 夜间 patrol（`memoria_maintenance()`，02:00-04:59 窗口） |
| Dream / 暗知识层 | agent-core | 空闲 tick / 夜间 consolidation round-robin | `bootstrap.rs` |

夜间编排（`agent-core/src/bootstrap.rs`，02:00-04:59 本地）：
`consolidate → meta_evolution → memoria_maintenance(decay+backup)`，结果记 `dream health`。

**原则：整合只发生在批处理/夜间；受限流与开关保护（防写风暴）；所有变更可回滚（`evolution_log.old_value` / `evolution_rollback` → `rolled_back` 负样本）。**

---

## 4. 边界规则（调用方必读）

1. **用户提问 → 只走召回**：`memory_context` / `memory_recall` / `memory_search`。禁止在提问处理中调用 `memory_evolve` / `consolidate`。
2. **写入 → 允许提取门，禁止演化**：写前可用 `AGENT_MEMORY_EXTRACT` 提取门压缩成原子事实；演化交给批处理。
3. **整合 → 只进批处理队列**：`consolidate` / `memory_evolve` / `meta_evolution` 只由夜间 patrol 或显式管理端点（`/api/meta-evolution/run`）触发。
4. **观测整合结果**：用 `evolution_log_query`（只读）采样演化历史；用 `/health.dream` 看夜间运行摘要。
5. **不确定时**：优先「降级 + 可观测」，勿默默吞失败（HY3 §11-3）。

---

## 5. 与 OpenClaw 吸收的关系

OpenClaw 的「recall-only + dreaming 整合」与 Memoria 的「`memory_context`/`recall` vs `consolidate`/Dream」同构（见 `OPTIMIZATION_OPENCLAW_ABSORPTION.md` §4）。本文件即该对照的落地文档：**不拆模块、不改架构，只固定边界语义**。

---

## 6. 违反边界的典型反模式（禁止）

- 在 `memory_search` 内部触发 LLM 摘要/演化 → 热路径 QPS 线性恶化（HY3 验收项，禁止）。
- 把 `consolidate` 搬进 memoria 主循环 → 违背「Memoria 薄存储，脑子在 agent-core」。
- 读取时写库（如查询时顺便改 tags）→ 破坏只读语义与并发安全。
