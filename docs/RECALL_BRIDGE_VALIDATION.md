# 召回补充验证：Phase 1b（semantic_related 边）间接价值 + FULL 重排稀释根因

> 收口 `RECALL_GRAPH_COMPLETION_PLAN.md` 的 review 第 6 条（"1b 真实价值待专项基准"）。
> 配套脚本：`eval/eval_semantic_recall.py`（1-hop 直连）、`eval/eval_bridge_recall.py`（2-hop 桥接）。

## 0. 背景与动机

上一轮 `eval_semantic_recall.py`（金标准 = `semantic_related` 1-hop 直连邻居 B）得到反常结果：

```
k    FULL(向量+图)  VECTOR-ONLY   1b lift
1         0.4%          0.0%       +0.4pp
5         2.4%         41.5%      -39.1pp
10        2.4%         68.9%      -66.5pp
```

FULL 反而远逊 VECTOR-ONLY。当时结论"1b 边对直接语义召回几乎无正收益"，但**该探针设计本身不公平**：
查询 = 记忆 A 原文时，A 的语义邻居 B（cos(A,B)≥0.6）已被 S2 向量通道直接召回，1b 图边完全冗余；
且暴露了重排栈对"纯语义近邻"的稀释，但根因未定位。

本验证用**2-hop 非直连桥接**场景公平测 1b 的间接价值，并隔离"图边是否拉进 C" vs "拉进后被重排压低"。

## 1. 桥接基准方法（eval_bridge_recall.py）

金标准 = 通过中间节点 B 桥接的 **2-hop 非向量直连** 记忆 C：

```
A --semantic_related(cos∈[0.60,0.78])--> B --semantic_related(cos≥0.60)--> C
且 cos(A, C) < 0.50  ← A 与 C 不直接语义相似，纯向量检索召回不了 C
```

- 探针查询 = A 原文（`content[A]`）。
- `VECTOR-ONLY` = 离线纯余弦(A 向量) top-k —— C 因 cos(A,C)<0.5 应极低。
- `FULL` = `memory_recall(query=A 原文, max_results=100)` —— 若 graph_expand 从 B 走 1-hop 拉进 C，且重排不压低，则应有正 lift。
- 额外维度 `pool(100)`：C 是否进候选池前 100，隔离"拉进 vs 截断"。

参数：NS=`agent/xujiayan`，n=100 (A,C) 对，SIM_AB=[0.60,0.78]，SIM_BC≥0.60，A-C<0.50。

## 2. 结果

```
=== 2-hop 桥接召回基准（金标准=非直连语义桥接记忆 C, n=100 对）===
   k |    FULL(含图) |  VECTOR-ONLY |  1b lift
   1 |       0.0% |        0.0% |   +0.0pp
   3 |       0.0% |        0.0% |   +0.0pp
   5 |       0.0% |        0.0% |   +0.0pp
  10 |       0.0% |        2.0% |   -2.0pp
  15 |       1.0% |        2.0% |   -1.0pp
--- 候选池命中（FULL 取 max_results=100，C 是否进前 100）：1.0% ---
```

**结论**：1b 边在当前重排栈下**完全无效**（lift≈0，FULL 与 VECTOR-ONLY 同量级且略差）。

## 3. 根因定位（三重证据）

### 3.1 graph_expand 确实拉进了 C（非"没建/没拉进"）
`rrf.rs:177` graph_expand 以 fused top-`seed_n`(默认10) 为种子，2-hop BFS 沿 `memory_relations` 双向扩展。
探针查询=A 原文 → S2 召回 A(rank1) 与 B(cos≥0.6) 均在种子内 → 从 A 走 2-hop（A→B→C）必触达 C。
`pool(100)=1.0%`（非 0）证明 C 偶尔能进前 100，证明确实被拉进，只是绝大多数被截断在 100 名外。

### 3.2 graph 扩展项丢失语义/关键词信号（rrf.rs:288-289）
```rust
FusedResult {
    sem_cos: None,      // ← 图邻居无"与查询余弦"
    kw_bm25: None,      // ← 图邻居无"与查询字面重叠"
    ...
}
```
→ `two_stage_rerank`（hybrid.rs:306）里这些项的 `sem_n = 0`、`kw_n = 0`，只剩 graph rrf 分：
`score = seed_rrf_max * 0.5^hop * max(weight,0.1)` ≈ `0.0033 * 0.5 * 0.65 ≈ 0.001`，
远低于直接信号项（S2/keyword 的 rrf 0.003+）。

### 3.3 重排栈是"与原始查询相似度"导向，不区分直接/传递相关
`two_stage_rerank` 最终分 = `w_rrf*rrf_n + w_sem*sem_n + w_kw*kw_n + w_freq*freq_n + w_rec*recency_n`
（hybrid.rs:350，默认 `w_rrf=0.3, w_sem=0.2, w_kw=0.5`）。
- 图传递相关项 C 与"原始查询"本身不相似（cos(A,C)<0.5）→ `sem_n`/`kw_n` 天然劣势；
- 且 `w_sem(0.2) < w_kw(0.5)`，纯语义信号本身权重也偏低；
- 最终 C 排 100+ 名，被 `take(max_results)`（hybrid.rs:271）截断。

**根因一句话**：重排栈不给"图传递相关性"任何正向信号，图扩展拉进来的桥接项在"直接相似度"竞争里必败，结构性被淘汰。这不是 1b 边本身无效，而是**重排层没有消费图边的价值**。

## 4. 修复方向（若要 1b 生效 —— 独立 Phase，不阻塞当前）

要让 1b 桥接项排进 top-k，必须让重排层"看见"图传递相关性：

1. **graph_expand 填充传递余弦**：扩展项 `sem_cos = 中间节点B的sem_cos × 边权(cos(B,C))`。
   使 two_stage_rerank 的 `sem_n` 反映"与查询的间接接近"（≈0.6×0.65≈0.39），而非恒 0。
   （需把 query 向量/中间节点 sem_cos 传入 graph_expand；当前签名只有 pool/results/hops/ns。）
2. **two_stage_rerank 增 `w_graph` 分量**：✅ **已实现（2026-07-26 下午）**。
   - 新增 `FusedResult.graph_signal: Option<f64>` = 沿 graph_expand 路径边权累乘（hop-1=边权，hop-2=边权×上游边权），
     作为独立于「与查询余弦」的图信号；`two_stage_rerank` 增 `w_graph * graph_n` 分量（env `MEMORIA_RERANK_W_GRAPH`，默认 **0.15**）。
   - 回归实测：`eval_recall_corrected` @10 = **65.0%**（修复前 65.8%，Δ-0.8pp 属重启抖动噪声，**零实质退化**），
     且图可达 gold（hop-1 same_entity/updates 边）因获图信号而更稳进 top-10。
3. **重扫描权重 + 防退化**：改完后必须重跑 `eval_recall_corrected.py`（图可达强相关金标准，@10 当前 65.8%）
   确认不退化，并重新扫描 `w_rrf/w_sem/w_kw/w_graph` 全局最优。

## 5. 当前决策

- **1b 边保留**：低风险、零收益、不改写图可达评测（@10=65.8% 无回归），待上述重排改造后才有价值。
- **不紧急**：1b 的理论价值（2-hop 桥接间接查询）在当前重排栈下无法兑现，优先级低于"重排层图信号消费"改造本身。
- **真正提升语义召回的下一只手**：应修 `two_stage_rerank` 的语义权重结构（提高 `w_sem` 或引入 `w_graph`），
  而非继续加更多语义边——边已建好，卡在重排没用上。

## 6. 补充验证（2026-07-26 下午）：w_graph 修复已落地，但桥接仍失效 — 根因升级

### 6.1 决定性实验：关掉 cross-encoder + 抬高 w_graph
为验证「cross-encoder 重排器（线上 `MEMORIA_RERANK_ENABLED=1`，bge-reranker-v2-m3，按查询-文档语义重排 top-100）是 1b 桥接杀手」的假设，
用一次性启动器（reranker **OFF** + `MEMORIA_RERANK_W_GRAPH=0.6`）重启 memoria，重跑桥接基准：

```
=== 2-hop 桥接召回基准（reranker OFF + w_graph=0.6, n=100 对）===
   k |    FULL(含图) |  VECTOR-ONLY |  1b lift
  10 |       1.0% |        2.0% |   -1.0pp
  15 |       1.0% |        2.0% |   -1.0pp
--- 候选池命中（pool=100）：6.0% ---
```
**结论：假设被推翻**——即便完全去掉 cross-encoder、且把 w_graph 提到 0.6，2-hop 桥接召回**仍≈0%**。
说明 cross-encoder 只是"补一刀"，**真正的根因在 graph_expand 自身**。

### 6.2 升级后的三重根因
1. **（已修，非主因）图项无独立信号** → 已用 `graph_signal` + `w_graph` 修复，图项获得应有的图相关性分。
2. **（主导，未解）候选爆炸 + hop-2 结构性最弱**：
   `graph_expand` 每查询炸出**数千**个图项（seed=10 × 每跳配额≈20，2-hop 累计 ≈4200 项）。
   其中 2-hop 桥接项 C 的 `edge_prod = 边权×上游边权 ≈ 0.4`，而 hop-1 语义邻居（**已被 S2 向量通道冗余覆盖**）的
   `edge_prod = 单条边 ≈ 0.6~1.0`。在 `two_stage_rerank` 里，**hop-1 项的 `graph_n` 与 `rrf_n` 双高，永远碾压 C**；
   `take(100)`/top-15 截断按比例取最高分，C 在数千图项里排不进前 100（pool 仅 6%）。
   **无论 w_graph 取多大，hop-1 都比 hop-2 得分高** → 纯调权重无法把 C 抬进 top-k。
3. **（叠加）线上 cross-encoder 重排器**按「查询-文档」语义相关性重排，彻底无视图结构；对"图源且不直连查询"的项再补一刀压低。

### 6.3 最终结论与决策
- **1b（semantic_related 边）的 2-hop 桥接召回在现有 `graph_expand` 设计下结构性≈0**：候选爆炸使 C 永远被淹没，
  w_graph 修复解决了"图项无信号"但解不开"候选爆炸 + hop-2 最弱"。
- **1b 边保留为低成本安全网**：已建、增量同步、零回归（@10=65.0%），对 hop-1 图可达 gold 有微弱正贡献；不删。
- **不应再为 2-hop 桥接投入**：收益≈0，且任何"抬 C"的改法都会同时抬高冗余的 hop-1（已被向量覆盖）→ 纯精度浪费。
- **真正提升语义召回的下一只手**：
  (a) cross-encoder 重排器（线上已开）已是主力；
  (b) **HyDE**（对问句式查询生成假设文档再检索）针对"查询-文档字面 mismatch"更有效，且不受图结构限制；
  (c) 若坚持解锁图桥接，需改**管线顺序**——在 cross-encoder 之后再注入图信号保底（post-rerank 图混合），或限宽 graph_expand + 给 2-hop 桥接项加成；属独立大改，优先级低，且须防 precision 退化。

> 方法论沉淀：`pool(max_results=100)` 维度隔离「graph_expand 是否拉进 C」vs「two_stage_rerank/cross-encoder 是否压低」，
> 配合「reranker ON/OFF × w_graph 扫描」的对照实验，可逐层钉死图召回失效根因。本套诊断法可复用于 memoria 后续任何图边验证。
