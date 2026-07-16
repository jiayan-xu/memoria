# Memoria × HMS 设计吸收优化方案

> 来源：`Shadow-Weave/HMS`（Holographic Memory System，MIT，建仓 2026-07-12，207⭐）。
> 目的：评估 HMS 哪些设计值得吸收进 Memoria（薄存储 / 身份总线，脑子在 agent-core），
> 并给出可落地的改造计划。

---

## Decisions locked（O1–O6，已决议）

> **状态**：已锁定。Phase A **按本表重验收**；此前以 `event_time` 列为主路径的交付视为偏离，**已纠偏**。

| ID | 决议 | 约束 |
|----|------|------|
| **O1** | P0 `entities=[]` **硬空** | ledger 不 JOIN `entities`/`entity_mentions`；实体回填属后续阶段 |
| **O2** | tags 过渡双轨时间；**不加/不依赖** `event_time` 作为 P0 **写入**路径 | 已有 `event_time` 列可保留只读兼容；新写入与解析走 tags |
| **O3** | occurred tag 格式：`occurred:YYYY-MM-DD` | 写入 `tags` JSON 数组；召回解析优先此 tag |
| **O4** | Self-Evolution 护栏在 **agent-core** | 凡 `search_memory`→knowledge 路径都挂；**不要**依赖 Memoria 响应里的 `guardrails` 当唯一注入 |
| **O5** | **无** cross-encoder | Phase A 禁止引入；rerank 属 Phase B 议题 |
| **O6** | ledger **只**挂 `memory_context` | `memory_profile` / 默认 `memory_search_v2`（含 `memory_recall`）**不**强制 enrich；可选开关默认关 |

---

## 1. 背景与边界

Memoria = 薄存储 / 身份总线；脑子在 agent-core。只吸收总线职责内、高价值低成本项。保留 supersede，不吸收 HMS「矛盾即 DELETE」。

---

## 4. 吸收决策矩阵（按 O1–O6）

| HMS 设计 | 判定 | 落地 |
| --- | --- | --- |
| 双轨时间 | **吸收（tags）** | O2/O3：`occurred:YYYY-MM-DD`；`event_time` 列只读兼容 |
| 类型化账本 | **吸收** | O6：仅 `memory_context`；O1：`entities=[]` |
| Self-Evolution | **吸收** | O4：agent-core knowledge 路径 |
| cross-encoder / DELETE 矛盾模型 | **不吸收（Phase A）** | O5；矛盾走 supersede |

---

## 7. Phase A 验收（O1–O6）

| ID | 验收标准 |
|----|----------|
| O1 | ledger `entities` 恒 `[]` |
| O2 | 不以 `event_time` 列为 P0 写入主路径 |
| O3 | `occurred:YYYY-MM-DD` tag → `occurred` 字段 |
| O4 | agent-core 挂护栏到全部 search_memory→knowledge |
| O5 | 无 cross-encoder |
| O6 | ledger 仅 `memory_context`；profile / 默认 search_v2 不强制 |

另：`prompt_block` 兼容；双树 = **源码 + 二进制**。

---

## 8. 实施状态（纠偏已落地 / 2026-07-16）

旧「event_time 主路径已发布」表述**废止**。

**已落地：**
- `ledger.rs`：occurred 优先 tags；`entities` 硬 `[]`；旧列只读兜底
- `memory_context`：唯一默认账本（`recall`/`ledger`）；不强塞 `guardrails`
- `memory_search_v2`：默认简洁 results；可选 `enrich_ledger=true`
- `memory_remember`：`event_time` deprecate → 映射 `occurred:` tag
- agent-core：`self_evolution` 挂两处 knowledge 注入
- 双树源码同步 + release 冷启短验通过

**测试：** `p0_hms_absorption` 6/6；agent-core `self_evolution` 2/2。

**未 commit**（待确认）。Phase B 仍后续。
