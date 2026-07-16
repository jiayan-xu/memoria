# Memoria × HMS 设计吸收优化方案

> 来源：`Shadow-Weave/HMS`（Holographic Memory System，MIT，建仓 2026-07-12，207⭐）。
> 目的：评估 HMS 哪些设计值得吸收进 Memoria（薄存储 / 身份总线，脑子在 agent-core），
> 并给出可落地的改造计划。

---

## Decisions locked（O1–O6，已决议）

> **状态**：已锁定。Phase A **按本表重验收**；此前以 `event_time` 列为主路径的交付视为偏离，**已纠偏**。

| ID | 决议 | 约束 |
|----|------|------|
| **O1** | P0 `entities=[]` **硬空**；**P1 开 JOIN** | P0 ledger 不 JOIN；Phase B / P1 经 `entity_mentions`×`entities` 回填；`MEMORIA_LEDGER_JOIN_ENTITIES=0` 可回滚 |
| **O2** | tags 过渡双轨时间；**不加/不依赖** `event_time` 作为 **写入**主路径 | 已有 `event_time` 列可保留只读兼容；新写入与解析走 tags；加列属 M1.2 可选且未拍板 |
| **O3** | occurred tag 格式：`occurred:YYYY-MM-DD` | 写入 `tags` JSON 数组；召回解析优先此 tag |
| **O4** | Self-Evolution 护栏在 **agent-core** | 凡 `search_memory`→knowledge 路径都挂；**不要**依赖 Memoria 响应里的 `guardrails` 当唯一注入 |
| **O5** | **无** cross-encoder | 允许轻量共现 / 启发式 rerank；不引新重依赖 |
| **O6** | ledger **只**挂 `memory_context` | `memory_profile` / 默认 `memory_search_v2`（含 `memory_recall`）**不**强制 enrich；可选开关默认关 |

---

## 1. 背景与边界

Memoria = 薄存储 / 身份总线；脑子在 agent-core。只吸收总线职责内、高价值低成本项。保留 supersede，不吸收 HMS「矛盾即 DELETE」。

---

## 4. 吸收决策矩阵（按 O1–O6）

| HMS 设计 | 判定 | 落地 |
| --- | --- | --- |
| 双轨时间 | **吸收（tags）** | O2/O3：`occurred:YYYY-MM-DD`；`event_time` 列只读兼容 |
| 类型化账本 | **吸收** | O6：仅 `memory_context`；O1：P0 `[]` → P1 JOIN |
| Self-Evolution | **吸收** | O4：agent-core knowledge 路径 |
| 共现 / 轻量 rerank | **吸收（Phase B）** | O5：启发式共现加成；**不上** cross-encoder |
| DELETE 矛盾模型 | **不吸收** | 冲突仍 supersede |
| `text_signals` | **战略 P2** | 本轮不做 |

---

## 7. Phase A 验收（O1–O6）

| ID | 验收标准 |
|----|----------|
| O1 | P0 ledger `entities` 在无 mention 时为 `[]` |
| O2 | 不以 `event_time` 列为写入主路径 |
| O3 | `occurred:YYYY-MM-DD` tag → `occurred` 字段 |
| O4 | agent-core 挂护栏到全部 search_memory→knowledge |
| O5 | 无 cross-encoder |
| O6 | ledger 仅 `memory_context`；profile / 默认 search_v2 不强制 |

另：`prompt_block` 兼容；双树 = **源码 + 二进制**。

---

## 8. Phase B 验收（P1 里程碑）

> 战略/可选与必做冲突时，**以本表为准**。`text_signals`（P2）与 `event_time` 加列（M1.2，未拍板）**不纳入本轮**。

| ID | 里程碑 | 验收标准 |
|----|--------|----------|
| **B1 / M1.1** | 双轨时间 tags→`occurred` | 带 `occurred:` tag → ledger.`occurred` 非空日期；无 tag 回退 `mentioned`/`valid_from`（Phase A 已落地，本轮回归） |
| **B2 / M1.2** | （可选）`event_time` 加列 | **本轮不做**（O2：≥1 迭代流量后再议） |
| **B3 / O1-P1** | ledger JOIN entities | 有 `entity_mentions` 时 `entities` 含 `{entity_id,name,entity_type}`；无 mention 仍 `[]`；可用 env 关闭 JOIN |
| **B4 / M1.3a** | 实体共现增强 | consolidate NER 后对同记忆共现实体补 `related_to` 边（agent-core）；不新建图库 |
| **B5 / M1.3b** | 轻量启发式 rerank | hybrid 检索后共现加成重排；**无** cross-encoder / 无新 crate |
| **B6 / M1.3c** | 冲突仍 supersede | pattern 冲突走 `supersedes_id`；旧行可 `include_superseded`/`as_of` 见；无 DELETE 当真值 |

**P2 未做（标明）**：`text_signals` 评估备忘录（M2.1）— 战略搁置。

---

## 9. 实施状态

### Phase A（纠偏已落地 / 2026-07-16）

- `ledger.rs`：occurred 优先 tags；旧列只读兜底
- `memory_context`：唯一默认账本；不强塞 `guardrails`
- `memory_search_v2`：默认简洁；可选 `enrich_ledger=true`
- agent-core：`self_evolution` 挂 knowledge 注入
- 测试：`p0_hms_absorption` 6/6

### Phase B（已完成 / 2026-07-16）

**已落地：**
- `ledger.rs`：P1 JOIN `entity_mentions`×`entities`；`MEMORIA_LEDGER_JOIN_ENTITIES=0` 可回滚
- `search/cooccur.rs` + `hybrid.rs`：轻量共现启发式 rerank（O5）
- agent-core `consolidate`：NER 后同记忆共现补 `related_to` 边
- 冲突路径：supersede 单测覆盖；不引入 DELETE 矛盾处理
- 双树：open → 运维 `memoria-core/src` 同步；release 冷启短验

**测试：** `p0_hms_absorption` 6/6；`p1_hms_phase_b` 6/6。

**未做（P2/战略）：** `text_signals`；`event_time` 加列（M1.2）。

**已本地 commit**（未 push）。

---

## 10. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-16 | Decisions locked O1–O6；Phase A 纠偏落地 |
| 2026-07-16 | Phase B：JOIN entities + 共现启发式 rerank + consolidate 共现回填 |
