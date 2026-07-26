# OPTIMIZATION_RAGFLOW_ABSORPTION

> 目标栈：`agent-core`（Rust 运行时）+ `Memoria`（SQLite 记忆/向量检索总线）
> 研究对象：RAGFlow（InfiniFlow，github.com/infiniflow/ragflow），浅克隆于 `ragflow-probe@d2a769c`
> 研究方式：只读精读 + 代码事实 vs 宣传对照（同 HMS / OPENCLAW 两轮）
> 实施状态：**研究完成 / 待开工**

---

## 1. 边界与定位对照

| 维度 | RAGFlow | Memoria + agent-core |
|---|---|---|
| 定位 | 端到端**文档型** RAG 引擎（PDF/Word/表格/扫描件 → 问答） | **记忆/身份总线**（对话记忆、自进化、TTC、可观测），非文档库 |
| 语言 | Python 为主 + React 前端 + Docker 全家桶 | Rust（memoria / agent-core）+ 系统 Python（embed_server） |
| 存储 | Elasticsearch / Infinity（向量+全文）+ MySQL + MinIO + Redis | SQLite + HNSW（自建，1024d）+ FTS5（jieba BM25） |
| 检索融合 | 引擎内 `weighted_sum`（向量主导 0.001/1），**无 RRF** | **RRF**（自建 rrf.rs）+ cross-encoder 重排（已落地） |
| 重排 | cross-encoder（bge-reranker-v2-m3 等），**默认关闭** | cross-encoder（bge-reranker-v2-m3，**已默认开启**，@10=57.5%） |
| 图 | GraphRAG：LLM 抽取实体/关系 + 索引期预计算 2-hop | graph_expand：内存中 `memory_relations` 双向 BFS 2-hop（已落地） |
| 层次摘要 | RAPTOR：聚类 + LLM 逐层摘要树（默认关闭） | 无 |
| 文档理解 | DeepDoc：版面识别 + OCR + 表格结构（需 HF 权重下载） | 不适用（记忆为结构化文本） |

**结论**：RAGFlow 是「重文档解析」型 RAG，Memoria 是「轻量记忆检索」型。可直接借鉴的是**检索/重排/图/RAPTOR 的算法范式**，而非其文档解析栈（DeepDoc 对记忆场景无价值）。

---

## 2. 核心子系统代码事实（file:line）

### 2.1 分块 `rag/app/*`
- 范式：`chunk()` → `naive_merge()`（贪心 token 窗口，`rag/nlp/__init__.py:1157`）或 `tokenize_table()`（**每 10 行一批**，`__init__.py:464`）。
- 真正语义单元保留仅：`qa`（Q+A 成对）、`laws`（标题层级树）、`presentation`（slide）、`tag`（行）。
- `naive/book/paper/manual/table` 本质是「DeepDoc 块 + 贪心 token 窗口」，可切在段落中间。
- 层级分块：`split_with_pattern()` `__init__.py:365`，`tokenize_chunks()` `:391`。

### 2.2 检索与融合 `rag/nlp/search.py`
- 入口 `Dealer.search()` `:134`。
- 词项召回：`FulltextQueryer.question()` `query.py:42` 构建 `MatchTextExpr`（带 `term_weight` + 同义词），`minimum_should_match=0.3` —— **非经典 BM25**（无 corpus IDF）。
- 向量召回：`get_vector()` `:199`。
- 融合：`FusionExpr("weighted_sum", topk, {"weights":"0.001,1"})` `:210` —— **向量主导，词项分权重仅 0.001**。
- 全仓无 RRF（仅 SVG base64 巧合子串）。

### 2.3 重排 `rag/llm/rerank_model.py`
- 基类 `Base.similarity()` `:36` 把分数 **min-max 归一到 [0,1]** `:62`。
- 全部为 **cross-encoder / 托管 API**（Jina/CoHere/Voyage/QWen/SILICONFLOW/本地 `HuggingfaceRerank` `:547`，默认 `BAAI/bge-reranker-v2-m3` `:578`）。**无 LLM-as-reranker**。
- 调用链：`Dealer.retrieval()` `:549` → `rerank_by_model()` `:494`；最终分 `sim = 0.7*tksim + 0.3*vtsim + rank_fea` `:519`（**重排器仅 30%**，70% 来自本地词项相似度）。
- 默认关闭：仅当 `dialog.rerank_id` 存在才构造 `rerank_mdl`（`dialog_service.py:360-399`）。

### 2.4 GraphRAG `rag/graphrag/`
- 查询：`KGSearch.retrieval()` `:139` → LLM `query_rewrite` `:161` → 初选实体/关系 → 沿预存 `n_hop_ents` 路径累加 `sim/(2+i)` `:180-186`，按 `sim*pagerank` 取 top6 `:220-221`。
- 构建：实体/关系抽取**依赖 LLM**（`utils.py:344/366`）；`n_neighbor(graph, node, n_hop=2)` `:803` 默认 2-hop，写 `n_hop_with_weight` 入实体 chunk。
- 接入：`dialog_service.py:784` `if use_kg` → `kg_retriever.retrieval()`，结果 `insert(0, ck)` 置顶。

### 2.5 RAPTOR 层次摘要 `rag/advanced_rag/knowlege_compile/raptor.py`
- 类 `RecursiveAbstractiveProcessing4TreeOrganizedRetrieval` `:165`；建树 `clustering()` `:328`（GMM+UMAP 或 AHC）。
- 摘要 `_summarize_texts()` `:384` 调 LLM 生成每层聚类摘要并重新 embed；经典树 `:854` 或 PSI 超边树 `:806`。
- 查询期：`retrieval_by_children()` `search.py:902` 把命中摘要块展开到叶子原文。
- 默认关闭：`use_raptor` 默认 False（`task_executor.py:1469`）。

### 2.6 主检索链路
`dialog_service.async_chat` `:712-798` → `Dealer.retrieval()` `search.py:549` → `search()`（融合）→ `rerank_by_model` → 阈值过滤 → `retrieval_by_children`（RAPTOR 展开）→ 可选 `use_kg` → `kb_prompt` 组装。

---

## 3. 宣传 vs 代码事实（防误吸收）

| 宣传口径 | 代码事实 | 证据 |
|---|---|---|
| "深度文档理解 / 10 种布局识别" | 标签恰 10 类 `layout_recognizer.py:34-46`；但权重需 HF 下载（`snapshot_download("InfiniFlow/deepdoc")` `:62`）或远程 `DEEPDOC_URL` | `layout_recognizer.py:34-66` |
| "BM25 + 向量 + 混合召回" | 非经典 BM25（加权词项匹配）；融合 `weighted_sum` 权重 `0.001,1` 向量主导；**无 RRF** | `search.py:192-211`、`query.py:42` |
| "Rerank 提升召回" | 全 cross-encoder，无 LLM 重排；最终分 `0.7*tksim + 0.3*vtsim`；**默认关闭** | `rerank_model.py:32,547`；`search.py:494-519` |
| "语义单元 / 表格整体保留" | `naive_merge` 贪心 token 窗口；表格每 10 行一批拆块；仅 qa/laws/presentation/tag 真语义保留 | `__init__.py:1157,464` |
| "GraphRAG" | 真实实现，但 LLM 抽取 + 索引期预计算 2-hop，需 `use_kg` 开关 | `graphrag/search.py:139`、`utils.py:803` |
| "RAPTOR" | 真实完整实现，但默认关闭、每层摘要耗 LLM | `raptor.py:165,750` |

---

## 4. 与 Memoria / agent-core 当前架构对照

| 能力 | Memoria 现状（2026-07-26） | RAGFlow 现状 | 差距/机会 |
|---|---|---|---|
| 重排 | ✅ cross-encoder bge-reranker-v2-m3，**默认开** @10=57.5% | cross-encoder，默认关，权重仅 0.3 | **我们已领先**：RAGFlow 反而压低 reranker 权重；我们全量 rerank 更激进有效 |
| 图扩展 | ✅ `memory_relations` 双向 2-hop BFS（已落地） | LLM 抽取 + 预计算 2-hop + pagerank | 我们轻量；缺 pagerank 排序与 LLM 实体抽取 |
| 混合召回 | ✅ RRF（向量+BM25+graph） | weighted_sum（向量主导）无 RRF | **RRF 更稳**，保持 |
| 层次摘要 | ❌ 无 | RAPTOR 树（默认关） | 可借鉴：对"高层/综述型"查询补召回 |
| 文档解析 | 不适用 | DeepDoc | 不吸收（记忆为文本） |
| HyDE | ✅ embed_server `hyde`（默认关，待生产 A/B） | 无原生 HyDE | 我们独有 |

---

## 5. 吸收决策矩阵

### ✅ 已对齐 / 验证（无需再动）
- **cross-encoder 重排（bge-reranker-v2-m3）**：RAGFlow 默认模型恰与我们用的一致，且我们已实现并验证 @10=57.5%。**决策：保持，不再吸收 RAGFlow 的 0.3 降权写法**（我们更激进有效）。

### 🟡 部分吸收（有增量价值，需改造）
1. **RAPTOR 层次摘要树** — 对"综述/高层"查询补召回（当前 reranker 在 100 候选上截断，顶层摘要可进入候选池）。
   - 落地点：memoria 摄入侧（`compress.rs` / `observe.rs`）对长记忆生成一层摘要块并链接原文；检索时命中摘要块 `retrieval_by_children` 式展开。
   - 成本：索引期 LLM 调用（agent-core 已有 LLM 池），可离线/异步。
   - 验收：对"总结一下我最近在做什么"类查询 @10 提升。
2. **GraphRAG 的 pagerank 排序 + LLM 实体抽取** — 增强 `memory_relations` 权重。
   - 落地点：graph_expand 邻居按 pagerank（或关系重要度）排序而非 `decay^hop` 平权；后台任务用 LLM 从对话抽取实体关系补 `memory_relations`。
   - 成本：LLM 抽取管线 + 图收敛；需防噪声（对齐 intake_filter）。
3. **分块原语 `naive_merge` / `tokenize_table`** — 仅当 memoria 增加「文档摄入」场景时有用（当前记忆已是结构化文本）。暂不吸收。

### ❌ 不吸收
- **DeepDoc 版面识别 / OCR / 表格结构** — 需 HF 权重下载 + 外接推理，与记忆场景无关；合规与运维成本高。
- **weighted_sum 融合（向量主导 0.001/1）** — 劣于我们现有 RRF，且弱化 BM25 词项信号。
- **rerank 0.3 降权写法** — 与实测相悖（我们全量 rerank 更优）。
- **RAGFlow 的 MySQL/ES/MinIO/Redis 全家桶** — 与 SQLite 单体架构冲突。

---

## 6. 落地方案（Phase，待用户确认）

### Phase A — GraphRAG 增强（低成本、高相关）
- A1：`graph_expand` 邻居按关系权重/pagerank 排序（替换平权 `decay^hop`）。
- A2：后台 `relation_extractor` 用 agent-core LLM 池从对话抽取实体/关系，写入 `memory_relations`（受 intake_filter 治理）。
- 验收：图 2-hop 在"桥接节点"查询上 @3/@5 再 +2~3pp（当前已 +2.5pp）。

### Phase B — RAPTOR 层次摘要（中成本）
- B1：摄入侧对长记忆（>600 字）生成 1 层摘要块，`raw_ref` 链原文。
- B2：检索命中摘要块时展开到叶子（仿 `retrieval_by_children`）。
- 验收：综述型查询 @10 提升，且无幻觉（摘要块带溯源）。

---

## 7. 不吸收清单（写进 AGENTS.md / 注释，防后人误抄）
- 不引入 DeepDoc / OCR / 表格结构识别（记忆场景无文档解析需求）。
- 不改用 `weighted_sum` 融合，保持 RRF。
- 不采用 rerank 0.3 降权，保持全量 cross-encoder 重排。
- 不引入 ES/Infinity/MySQL 存储栈。

---

## 8. 实施状态
- ✅ 浅克隆 `ragflow-probe@d2a769c`，只读精读完成。
- ✅ 代码事实 vs 宣传对照已建立。
- ✅ 决策矩阵已产出（✅对齐 / 🟡部分 / ❌不吸收）。
- ⏸ 落地 Phase A/B 待用户确认（当前 memoria 召回已通过 reranker 达 @10=57.5%，优先级可后置）。
