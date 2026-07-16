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
| `text_signals` | **吸收（P2 最小切片）** | M2.1：读时抽取 + ledger 显式化 + 检索加成；见 §8.1 |

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

| ID | 里程碑 | 验收标准 |
|----|--------|----------|
| **B1 / M1.1** | 双轨时间 tags→`occurred` | 带 `occurred:` tag → ledger.`occurred` 非空日期；无 tag 回退 `mentioned`/`valid_from` |
| **B2 / M1.2** | （可选）`event_time` 加列 | **不做**（O2） |
| **B3 / O1-P1** | ledger JOIN entities | 有 mention 时 `entities` 非空；无 mention `[]` |
| **B4 / M1.3a** | 实体共现增强 | agent-core consolidate 共现补边 |
| **B5 / M1.3b** | 轻量启发式 rerank | cooccur 加成；无 cross-encoder |
| **B6 / M1.3c** | 冲突仍 supersede | 无 DELETE 当真值 |

---

## 8.1 Phase P2 验收（text_signals / M2.1）

> HMS ledger 中的 numeric / date / update signals。Memoria 薄存储：**读时确定性抽取**，不新表、不写 `event_time` 主路径、不引 LLM/cross-encoder。

### 目标（P2.1 已做）

| ID | 验收标准 |
|----|----------|
| **P2.1a** | `memory_context` ledger 每行含 `text_signals: { numbers, dates, update_markers }` |
| **P2.1b** | `occurred`（来自 tags，O3）并入 `text_signals.dates`；**不**写 `event_time` 列 |
| **P2.1c** | hybrid 检索：query 与正文数字/日期重叠 → 小幅 `rrf_score` 加成；`source` 可含 `text_signals` |
| **P2.1d** | `MEMORIA_TEXT_SIGNALS_RERANK=0` 可关闭检索加成（ledger 抽取仍保留） |

### 非目标（P2.2 未做）

- retain/consolidate 时用 LLM 抽取信号并持久化
- ~~agent-core Self-Evolution 消费 `text_signals` 做 dedup/校准护栏~~ → **P2.2b 已做（agent-core）**
- ~~相对日期解析（「上周三」→ 绝对日）~~ → **P2.2a 已做（读时）**
- `event_time` 加列（M1.2，仍不做）
- tags 持久化信号（P2.2 未做）

### 实现锚点

- `src/search/text_signals.rs`：抽取 + rerank
- `src/tools/ledger.rs`：`enrich_ledger` 挂 `text_signals`
- `src/search/hybrid.rs`：cooccur 之后调用 rerank
- 测试：`p2_hms_text_signals`

---

## 9. 实施状态

### Phase A（纠偏已落地 / 2026-07-16）

- `ledger.rs`：occurred 优先 tags；旧列只读兜底
- `memory_context`：唯一默认账本；不强塞 `guardrails`
- `memory_search_v2`：默认简洁；可选 `enrich_ledger=true`
- agent-core：`self_evolution` 挂 knowledge 注入
- 测试：`p0_hms_absorption` 6/6

### Phase B（已完成 / 2026-07-16）

- P1 JOIN entities + cooccur rerank + consolidate 共现
- 测试：`p0_hms_absorption` 6/6；`p1_hms_phase_b` 6/6

### Phase P2（最小切片 / 2026-07-16）

**P2.1 已落地：**
- `search/text_signals.rs`：numbers / dates / update_markers 读时抽取
- `ledger.rs`：ledger 行挂 `text_signals`（仅 `memory_context` / O6）
- `hybrid.rs`：数字/日期 query 重叠加成；env 可关 rerank
- 双树：open → 运维 `memoria-core/src` 同步；release 冷启短验

**测试：** `p2_hms_text_signals` 3/3；Phase A/B 回归保持。

**P2.2 最小切片（2026-07-16）：**
- **P2.2a** `text_signals` 相对中文日期读时解析（上周三/昨天 → `YYYY-MM-DD`；不写 `event_time`）
- **P2.2b** agent-core Self-Evolution 消费 ledger `text_signals` 生成 dedup/校准护栏

**P2.2 未做：** LLM retain 抽取、tags 持久化信号。

**M1.2 未做：** `event_time` 加列。

**已本地 commit**（未 push）。

---

## 10. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-16 | Decisions locked O1–O6；Phase A 纠偏落地 |
| 2026-07-16 | Phase B：JOIN entities + 共现启发式 rerank |
| 2026-07-16 | Phase P2.1：text_signals 抽取 + ledger + 检索加成（M2.1 最小切片） |
