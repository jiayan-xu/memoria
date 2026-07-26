# 召回图补全方案（修订版 v2 · 吸收代码审查）

> 取代 v1。`v1` 有三处 P0 过时/危险，已按审查逐条修订（每条均对实时库/源码核实）。
> 基线：A+C 管线 @10 = **57.5%**（reranker 已开），候选上限（top-100 命中）**75.8%**。
> 关联：`docs/OPTIMIZATION_RAGFLOW_ABSORPTION.md`（Phase A 的「LLM 抽取」以本方案取代）。

---

## 0. 审查修正摘要（已逐条核实）

| # | v1 问题 | 核实事实 | 修订 |
|---|---|---|---|
| P0-1 | 称 graph_expand「未用边 weight，Phase1 要折入」 | `rrf.rs:259` 已 `seed_rrf_max * hop_factor * weight.abs().max(0.1)` | 删「折入 weight」；Phase1 重点=补边 + fanout 采样顺序 |
| P0-2 | 未提 fanout 截断无序 | `rrf.rs:232-244` `LIMIT {fanout}` **无 ORDER BY** | Phase1a 必加 `ORDER BY weight DESC` + 类型配额 |
| P0-3 | Phase0 DELETE「经空实体的边」表述危险 | `source/target` 是**记忆 id** 非实体 id；空 hub 仅触 **350** 记忆 / ≈**75** same_entity（总 2149） | cooccur 去空名（主）+ same_entity **整表重建**（非含糊 DELETE） |
| P0-8 | 写入防护坐标错（2821） | 空名来自 `entity_upsert`（`mcp_server.rs:2799` 的 `INSERT INTO entities`） | 防护移到 upsert 处拒空名 |
| P1-4 | 未写 namespace 隔离 | 全库 `V@V.T` 会跨 `agent/xujiayan`↔`jarvis` 连边 | 按 namespace 分块建边 |
| P1-5 | 未提 chronological 稀释 | `chronological` 4035 = 边表 **60%** | fanout 类型配额（semantic≤6 / same_entity≤4 / chrono≤2） |
| P1-6 | `57.5→63~68%` 乐观无对照 | — | 强制消融 + 固定 env + DB 快照，以实测为准 |
| P1-7 | 仅离线脚本 | 图会再滞后 | 增量：observe/remember → HNSW top-k 或异步队列 |
| P2 | 称「1 hub」 | 实测 **4 hub**（`ent:agentxujiayan_` 5865 / `ent:agentjarvis_` 68 / 两 0-mention） | 改 4 hub；RAPTOR 与「零 LLM」叙事分离；阈值先网格扫描 |

---

## 1. 修订后落地顺序

1. **Phase 0**：cooccur 去空名 + 写入拒空名(upsert) + same_entity 整表重建
2. **Phase 1a**：`graph_expand` 邻居 `ORDER BY weight DESC` + 类型配额（**补边前必做**）
3. **Phase 1b**：按 namespace 离线补 `semantic_related`
4. **Phase 2**：idf 共现提质
5. 增量同步 + 评测消融 → 再谈 Phase 3(RAPTOR) / LLM 抽取

---

## Phase 0（必做前置）

### 0.1 cooccur 去空名（主收益）
`src/search/cooccur.rs:98` `load_memory_entities` 的 SQL 加空名实体排除：
```rust
"SELECT memory_id, entity_id FROM entity_mentions \
 WHERE namespace = ?1 AND memory_id IN ({}) \
 AND entity_id NOT IN (SELECT id FROM entities WHERE name = '')",
```
空名 mention 占 93%，是共现污染主源；运行时重排先净化。

### 0.2 写入拒空名（防护移到 upsert）
`mcp_server.rs:2791` `let name = args.get("name")...unwrap_or("")` 处：若 `name.trim().is_empty()` 直接 `return` 错误（不插 `entities`、也不插 `entity_mentions`）。同时 `entity_id` 由 name 派生的路径一并跳过。
> 注：`mcp_server.rs:2821` 是 `entity_add_mention` 插入点，但空名根因在 **上游 upsert（2799）**，只在 mention 处堵不够。

### 0.3 same_entity 整表重建（非含糊 DELETE）
离线脚本 `tools/offline/rebuild_same_entity.py`：
- 先 `sqlite3 .backup` 快照；
- `DELETE FROM memory_relations WHERE relation_type='same_entity'`（全清，来源已被污染）；
- 从**干净 mention**（排除空名实体）按 namespace 重算：同 ns 内共享 **≥2 个真实实体**的记忆对 → 插 `same_entity`，
  `weight = 共现实体数 / (1 + log(实体总频度))`（idf 降权常见实体如 `agent-core`/`memoria`）；
- 实现 SQL：`entity_mentions m1 JOIN m2 ON m1.entity_id=m2.entity_id AND m1.memory_id<m2.memory_id WHERE m1.entity_id NOT IN(空名) GROUP BY 对 HAVING COUNT(*)>=2`；
- 每记忆出边 cap ≤20，`weight<0.1` 丢弃；幂等。
- 实测空 hub 仅触 ≈75 条 same_entity，重建后边更干净、无幽灵星型。

**验证（Phase 0 消融）**：固定 `reranker 开 / GRAPH_HOPS=2 / SEED=10 / FANOUT=10`，`eval_recall_corrected.py` 两跑 → @10 应不降（可能微升），`@1/@3` 观察噪声下降。

---

## Phase 1a（fanout 有序 + 类型配额，补边前必做）

`src/search/rrf.rs:232-244` 邻居查询改为带排序与配额：
```sql
SELECT r.neighbor_id, r.weight, r.relation_type, m.content
FROM (
    SELECT target_id AS neighbor_id, weight, relation_type
    FROM memory_relations WHERE source_id = ? AND namespace = ?
    UNION
    SELECT source_id AS neighbor_id, weight, relation_type
    FROM memory_relations WHERE target_id = ? AND namespace = ?
) r
LEFT JOIN memories m ON r.neighbor_id = m.id
WHERE r.weight > 0
ORDER BY
  CASE r.relation_type
    WHEN 'semantic_related' THEN 0
    WHEN 'updates' THEN 1
    WHEN 'same_entity' THEN 2
    ELSE 3 END,
  r.weight DESC
LIMIT {fanout}
```
- 保证高权 `semantic_related`/`updates` 优先进 2-hop；`chronological` 自然靠后。
- **类型配额**：每种子节点的 fanout 内截断累积（semantic_related≤6、same_entity≤4、updates 全留、chronological≤2），防版本链稀释。
- **不改计分公式**：`rrf.rs:259` 已用 `weight` 计分，本阶段只改「哪些边被选中」，避免重复改造。

---

## Phase 1b（按 namespace 补 semantic_related，主收益）

离线 `tools/offline/build_semantic_edges.py`：
- **按 namespace 分块**（`agent/xujiayan` / `jarvis` 等互不连边）；
- 每块：`SELECT id, vector FROM memory_vectors WHERE namespace=?` → 解包 `(N,1024)` float32；
- 行归一化 `M = V @ V.T`（N≈5024，~100MB，numpy 数十秒），每行 top-k(k=8) 排除自身，`cosine > 阈值` 建边；
- **阈值先网格扫描**（0.50 / 0.55 / 0.60 / 0.62 × k=6/8/10）在 eval 集选默认（已知向量错存 87 例，偏高更稳）；
- 插入 `semantic_related`，`weight=cosine`，`evidence='cos@<model>'`；每记忆出边 cap ≤8；
- 估算总边 ≈ 2 万条，写入秒级。

**增量（P1-7）**：`observe`/`remember` 写新向量后，异步队列（或写入后立即）对该 id 做 numpy/HNSW top-k 近邻插入 `semantic_related`；复用 `record_recall_access` 同范式（admin 鉴权），与 firehose 捕获一致，避免图再滞后。

---

## Phase 2（idf 共现提质）
在 Phase 0.3 重建基础上，weight 用 idf（已含于 0.3 公式）。与 Phase 1b 互补：
共现抓「同上下文」、嵌入抓「同义/近义」，共同拓宽召回网。

---

## Phase 3 / LLM 抽取（后置，与「零 LLM」叙事分离）

- **RAPTOR** 仅解决综述型查询，单独叙事，不挂「零 LLM」主线。
- **LLM 抽取** 仅 `importance>=4` 可选通道，写 `semantic_related`（带 `evidence`）或新增 `llm_related` 类型，受 `intake_filter` 治理；**不内联、不默认、不进 firehose 实时捕获**。

---

## 验证（统一，强制消融）

固定：`reranker 开` / `MEMORIA_GRAPH_HOPS=2` / `SEED=10` / `FANOUT=10` / **DB 快照**。
- **A**：仅 Phase 0
- **B**：Phase 0 + 1a + 1b
- **C**：+ Phase 2
每次 `eval_recall_corrected.py`（两跑取稳）+ `eval_recall_pool.py`。

**判据**：B 的 @10 ≥ **60%**（以实测为准，不预设 63~68）；无回归（@1 不降）。

---

## 风险与回滚

| 风险 | 缓解 |
|---|---|
| 边膨胀拖慢 graph_expand | 出边 cap（semantic≤8 / same_entity≤20）；weight 阈值裁剪 |
| 空实体再生 | 0.2 写入拒空名兜底 |
| 向量错存误连（已知 87 例同向量不同内容） | 阈值上调 0.62 + 仅加边不合并（可审） |
| DB 误操作 | 每阶段前 `.backup` 快照；边插入幂等（先 DELETE 同 type 再插） |
| chronological 稀释 | 1a 类型配额兜底 |

---

## 5. 执行结果（2026-07-26 实测消融）

固定 env：reranker 开 / `GRAPH_HOPS=2` / `SEED=10` / `FANOUT=10` / DB 已快照（`.same_entity_rebuild_bak`、`.semantic_rebuild_bak`）。

| 阶段 | @1 | @3 | @5 | @10 | 候选池@100 |
|---|---|---|---|---|---|
| A: Phase 0（净化 + same_entity 重建 394 边） | 25.8% | 46.7% | 55.0% | **65.0%** | 65.8% |
| B: +1a（ORDER BY + 类型配额） | 25.8% | 46.7% | 55.0% | 65.0% | 65.8% |
| C: +1b（semantic_related 12,148 边） | 25.8% | 46.7% | 55.0% | 65.0% | 65.8% |
| 基线（reranker 开、图未净化） | 22.5% | 37.5% | 45.0% | 57.5% | 75.8%（污染假上限） |

- **Phase 0 是唯一显著收益**：@10 57.5→65.0（+7.5pp）。根因是空实体 hub（5865 mention）把共现图污染成巨型星型；clean 后 same_entity 2149→394 条干净边 + cooccur 重排去空名，信号纯度大升。
- **75.8% 候选池是污染假上限**：旧池靠空实体 hub 把几乎所有记忆连成一片，覆盖率虚高但噪声大；去污染后真实上限 ≈65.8%。@10 反而升，因 reranker 在干净候选上更有效（57.5/75.8=76% → 65.0/65.8=99% 池内利用率）。
- **1a 中性**：为 1b 铺路，不改计分（`rrf.rs:259` 已用 `weight`），仅定序+配额，无回归。
- **1b 中性（于本评测）**：12,148 条语义边进入 2-hop，但本探针 gold=「图可达强相关邻居」，主要由 `same_entity`/`updates` 可达；余弦近邻未新增可达 gold。语义图的真实价值在「语义相似查询」（与查询共享含义但无共现实体），需另建语义查询召回基准才能体现，本图可达性评测不捕获。
- **Phase 2（idf 共现）已并入 Phase 0.3**：`rebuild_same_entity.py` 用 idf 权重，无需单独再做。

**当前总览**：纯余弦 28.3% → A+C(1-hop+变体A) 46.7% → +reranker 57.5% → +Phase0 净化 **65.0%**（真实候选上限 65.8%）。剩余差距在候选生成（图/混合）而非重排；semantic 图对语义查询的增益待专项基准验证。
