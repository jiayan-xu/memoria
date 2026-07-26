# Memoria 打磨方案报告（2026-07-25 · v2 修订版）

> 依据：① GitHub 记忆层 2026 最新动态调研；② 本仓 `memoria-open`（`main` @ `5350f09`）源码实测地图；③ 用户评审修订（P0×3 / P1×5 / P2×3）。
> 目标：在**不重造已有强项**前提下，落地四项打磨（拆为 F3 / F1a / F1b / F2 / F4 五步）。
> 状态：**本报告为修订规格，尚未改动任何代码**。

---

## 0. 前置已闭环（上下文）

| 项 | 状态 | 备注 |
|---|---|---|
| 摄入侧三滤 `intake_filter` | ✅ 已落地 | agent-core 侧，opt-in，三子开关默认全开 |
| firehose 捕获静默失败修复 | ✅ 已修复 | `observe_filtered` 改用 admin 鉴权客户端（`X-Agent-Id: admin`） |
| GitHub 调研 + 本仓对照 | ✅ 完成 | 见 §1 / §2 |

---

## 1. GitHub 2026 记忆层动态（调研结论）

| 项目 | 近况 | 可借鉴 |
|---|---|---|
| **Mem0** (≈48–60k⭐, Apache-2.0) | v3 重写：LoCoMo 71→91.6、LongMemEval 67.8→**93.4** | ADD-only 非破坏更新；时间推理（valid_from/valid_to 永不删）；混合召回（语义+BM25+实体 boost）；**访问衰减**（常用/近期命中自动上浮，public score 钳 [0,1]） |
| **Zep / Graphiti** (≈27k⭐) | 时序知识图谱，双时间窗 valid_from/valid_to；矛盾=失效旧边而非覆盖；混合（语义+BM25+图遍历 k 跳按 embedding 重排），P95<200ms | 双时态 + 图遍历重排 |
| **Letta / MemGPT** (≈24k⭐) | 分层记忆（core=RAM / archival=磁盘，agent 自管）；Context Repositories（git 版记忆）；Conversations API 跨 agent 共享；**sleep-time 空闲巩固** | 分层 + 空闲巩固（已对齐 memoria `dream_state`） |
| **新锐** | PLUR（YAML engram 可读可审计）；MemPalace（MIT，LongMemEval 96.6%，本地 SQLite+Chroma，19 MCP 工具）；SuperMemory；Cognee（30+ 连接器） | 可读可审计 / MCP 原生 / 生命周期钩子自动捕获 |

**趋势总括（6 条）**：① 时间推理 + 非破坏更新；② 混合召回（语义+BM25+图+实体）；③ 访问/新鲜度衰减重排；④ 空闲巩固（dream/sleep-time）；⑤ MCP 原生 + 生命周期钩子自动捕获；⑥ 可读可审计。

---

## 2. Memoria 现状对照（强项不重造 / 差距即 F1–F4）

**已有强项（本次不碰）**：
- 混合召回：`jieba` BM25（FTS5）+ HNSW 向量（**1024d**，Qwen3-VL-Embedding-8B / SiliconFlow `:8777`）
- `memory_graph` 实体链（entities / entity_edges / entity_mentions）
- `dream_state` 空闲巩固（对齐 Letta sleep-time）
- `evolution_log` 演进/失效历史（可回滚，H5 可逆）
- `decay` 工具 + `importance`（**temporal 通道已有 recall 频率加成雏形**，见 §3.2 P0#1）
- `valid_from/valid_to/superseded_by` 双时态取代框架（已存在，F2 复用）
- 刚落地的 `intake_filter` 源头降噪

**真正差距（= 四项打磨，拆五步）**：
- F3 召回可解释性（输出层缺失）
- F1 访问频率/新鲜度加权 → **F1a 召回回写链路（缺失）** + **F1b 跨通道重排融合**
- F2 时间有效性元数据（旧事实"降权但仍可答历史"未实现）
- F4 摄入压缩蒸馏（`raw_ref` 列已建但未在写入路径填充）

---

## 3. 打磨方案（逐项：动机 / 改动坐标 / 具体改动 / 风险 / 验证）

### F3 · 召回可解释性（**建议最先做，最低风险**）

- **动机**：对齐"可读可审计"趋势。`FusedResult` 已有 `signal_scores` / `sem_cos` / `kw_bm25`，但缺一个归一化的「主匹配通道」概览。
- **改动坐标**（已实测）：
  - `src/search/rrf.rs:33-48` `FusedResult` 结构体 → 加 `primary_channel: Option<String>`、`channel_scores: HashMap<String, f64>`。
  - `src/search/rrf.rs:102-114` `rrf_merge` 构造处 → **复用 `channel_of`（`rrf.rs:126` 已有，把 source 归一为通道名）**：由 `signal_scores`（已按 `channel_of` 聚合的 RRF 幅度）取最大值通道为 `primary_channel`，并把 `signal_scores` 原样拷给 `channel_scores`。
  - `src/search/rrf.rs:185-195` `graph_expand` push 处 → `primary_channel = Some("graph_expand")`、`channel_scores = {"graph_expand": rrf_score}`。
  - `src/mcp_server.rs:1140-1150` `json!` 序列化 → 补 `primary_channel` / `channel_scores`。
- **具体改动**：纯输出层増字段，不触动排序/存储。**客户端忽略未知字段，零破坏性**。**归一逻辑复用 `channel_of`，不重复实现。**
- **风险**：低。
- **验证**：`memory_recall` 返回 JSON 每条含 `primary_channel`（如 `"semantic"`/`"keyword"`/`"graph_expand"`）与 `channel_scores`（各通道 RRF 幅度）。

---

### F1 · 访问频率 / 新鲜度加权（拆 F1a + F1b）

#### 🔴 P0#1 修正（事实偏差）
`temporal` 通道**已经**用 `recall_count` 做了频率加权：`src/search/temporal.rs:41`
```rust
let ts = ts * (1.0 + recall_count * 0.1); // recall frequency bonus
```
即「召回频率加权」在 temporal 单通道已有雏形。**F1 真正缺的是「召回命中回写链路」**（见下），而「跨通道重排融合」是把 temporal 单通道的 `recall_count*0.1` bonus **泛化到 `two_stage_rerank`**，并非从零新建。

#### 🔴 P1#5 修正（语义变更迁移成本）→ 改推荐为新增 `access_count`
若把 `recall_count` 语义从「写入/去重自增」改为「召回命中」，现有 ~3.9 万行 `recall_count` 是**脏数据**（混了写入与召回），且会破坏 `decay.rs:33` 的冷热判据（`decay_factor <= 0.1 AND recall_count < 3`）。
**决策：新增 `access_count` 列承载「召回命中计数」，`recall_count` 保留原义（写入/去重自增），二者解耦。** 这同时保护 `decay` 判据不受影响（P2 补：本报告 §7 明确提示此隐性破坏点）。

#### F1a · 召回回写链路（低风险，不动排序）

- **🔴 关键发现（实测）**：`memories` 表有 `recall_count`（INTEGER 默认 0）+ `last_recalled`（TEXT），但 grep 全仓确认二者**仅在 `remember.rs:280/325/575`（写入/去重/merge 时自增）**更新；**搜索召回返回结果后没有任何自增钩子**。F1a 补的就是这条「召回命中即计数」链路，承载到新增的 `access_count`。
- **改动坐标**：
  - **迁移**：新增 `migrate_access_count`（仿 `src/storage/sqlite.rs:477` `migrate_extract_fields` 模板，幂等 ADD COLUMN）→ 加 `access_count INTEGER DEFAULT 0`；接入 `init_core_tables` 链（`sqlite.rs:253-257` 末尾）。
  - **回写落点**：`src/mcp_server.rs:1085` `hybrid_search` 返回 `fused` 后、进 tags 过滤前，对结果中每条 `memory_id` 执行
    `UPDATE memories SET access_count = access_count + 1, last_recalled = ? WHERE id = ?`。
- **🔴 P1#4 修正（并发/事务风险）→ 异步批外提交**：
  - **不阻塞召回响应**：用 `std::thread::spawn`（或 `tokio::spawn`，取决于 handler 是否 async）带**克隆的 pool** 单事务批量 UPDATE（一次事务多语句，N=max_results ≤ 50，开销极小）。
  - **只读/配额拒绝时跳过**：若 `quota_gate` 已拒绝（本就不在结果路径）或 `auth.role` 为只读，不回写，避免「召回成功但计数失败」不一致。
  - **WAL 抖动**：高频小 UPDATE 触发 checkpoint 抖动 → 调 `PRAGMA wal_autocheckpoint`（如 2000）或复用独立写连接；`access_count` 走新列，与 `recall_count` 读写解耦，互不放大锁竞争。
- **风险**：低（新列 + 异步批量；不动排序）。
- **验证**：连续 `memory_recall` 同查询 → DB 中 `access_count` 递增、`last_recalled` 刷新；`recall_count` 不变（保留写入语义）；`decay.rs` 冷热判据不受影响。

#### F1b · 跨通道重排融合（中风险，env 可调权重）

- **改动坐标**：
  - **取数**：`src/search/hybrid.rs` 内 fetch 每条候选的 `access_count` + `last_recalled`（并入现有 `is_latest_now` 查询块 `hybrid.rs:117-154`，把这两列一起取回），填入 `FusedResult` 新字段 `access_count: i64` / `last_recalled: Option<String>`。
  - **融合**：`src/search/hybrid.rs:230` `two_stage_rerank` 叠加 recency + freq 分量：
    - `freq_n = access_count / (access_count + K_freq)`，`K_freq = MEMORIA_FREQ_K`（默认 10）；
    - `recency_n = exp(-λ · age_hours)`，`λ = MEMORIA_RECENCY_LAMBDA`（默认 0.01，age 由 `last_recalled` 与 now 之差算）；
    - `final = w_rrf·rrf_n + w_sem·sem_n + w_kw·kw_n + w_freq·freq_n + w_rec·recency_n`，`w_freq = MEMORIA_RERANK_W_FREQ`（默认 0.1）、`w_rec = MEMORIA_RERANK_W_REC`（默认 0.1）。
  - 权重默认值与现有 `MEMORIA_RERANK_W_RRF/SEM/KW` 风格一致，均 env 可覆盖；权重为 0 时退化为现状。
- **风险**：中。融合项默认仅 0.1，对基线排序扰动小；需 `eval/` 回归确认 recall@10 不降（见 §5）。
- **验证**：常用/近期记忆在混合分中上浮；`eval/eval_recall_corrected.py` 同进程复测 recall@10 不降（基线见 §5）。

---

### F2 · 时间有效性元数据（旧事实降权但仍可答历史）

- **动机**：对齐 Mem0 temporal & Zep 双时态。当前 `hybrid_search`（`hybrid.rs:117-154`）默认 `is_latest_now`（`superseded_by IS NULL` + 当前有效）直接**过滤掉**旧事实。
- **🔴 P0#2 修正（事实偏差）**：`hybrid.rs:24` **已有 `include_superseded: bool` 参数**，`true` 时「跳过整段 isLatest/visible_as_of 过滤，返回全部行」。故 **F2 = `include_superseded` 的降权版**，不是新参数——避免参数膨胀。
- **改动坐标（复用 + 降权）**：
  - **复用 `include_superseded` + 加降权系数**：将其语义从「全收」改为「当前真值为主 + 被取代但仍有效(valid_at(now))的历史真值降权补回」，不再一刀切返回全部。
    - 默认（`include_superseded=false`）：维持 `is_latest_now`，行为不变。
    - `include_superseded=true`：① 先取 `is_latest_now` 主集（`time_status="current"`）；② 再取 `superseded_by IS NOT NULL AND valid_at(now)` 的历史真值，标 `time_status="superseded"`，`rrf_score *= MEMORIA_HISTORY_DOWNWEIGHT`（见下）。
  - **复用已有框架**：`remember.rs` 内 `compute_stamp_to_boundary`（取代边界）+ `apply_supersede_in_tx`（标 `superseded_by` + 关 `valid_to`）已完整；过滤块 `hybrid.rs:143-151` 改为「先分桶（current/superseded），再按参数决定保留 + 降权」，而非当前一刀切 `retain`。
  - **`FusedResult` 加 `time_status: Option<String>`**（`current`/`superseded`/`expired`，默认 `None`），`rrf_merge`/`graph_expand` 填；`mcp_server.rs:1140` json! 补。
  - **🔴 P1#6 修正（魔法数 → env）**：降权系数 `rrf_score *= MEMORIA_HISTORY_DOWNWEIGHT`（默认 0.5，env 可覆盖），与 F1 权重风格一致。
- **风险**：中。改动过滤逻辑须小心不破坏默认 `is_latest_now` 语义（回归测试重点）。
- **验证**：`memory_recall` 默认只见当前真值；`include_superseded=true` 时旧事实降权（`×0.5`）出现在尾部并带 `time_status="superseded"`；历史查询（`as_of=旧时刻` + `include_superseded=true`）可答出当时真值。

---

### F4 · 摄入压缩蒸馏

- **动机**：对话流水类记忆原文冗长，既占存储又稀释检索。对齐"摄入压缩"。
- **现状（实测）**：`observe.rs:24` / `remember.rs:366` INSERT 原样存；`raw_ref` 列已存在（`migrate_extract_fields` 建立）但**写入路径未填充**（observe 不传；remember 仅外部显式传 `raw_ref` 才存）。
- **改动坐标（守 H1/H2，Memoria 不调 LLM 做摘要）**：
  - **启发式压缩**：超长内容做"首段 + 关键句"抽取，压缩后存 `content`，**原文进 `raw_ref`**。
    - 落点 1：`src/tools/observe.rs:24` INSERT 前 `let (content, raw_ref) = distill(dialog)`。
    - 落点 2：`src/tools/remember.rs:366` 新写 INSERT 前——仅当 `raw_ref` 参数为 `None` 且 `content` 超阈值时压缩并填 `raw_ref`（外部已传则不二次压缩）。
  - **🔴 P1#7 修正（中文阈值 + 分句 + 关键句定义）**：
    - **阈值分语言**：`MEMORIA_DISTILL_MAX_CHARS`（默认 1200，拉丁文为主）/`MEMORIA_DISTILL_MAX_CHARS_CN`（默认 600，中文为主，≈1200 token）——按内容是否以中文为主（阈值按字符数计，但中文单独设更低上限避免 token 爆炸）。
    - **中文分句复用 `jieba`**（仓库已依赖，`keyword.rs` 在用）：以 jieba 分词结果切句 + 提取高频词所在句作为「关键句」；退化方案用标点（`。！？；\n` / `.!?`）切句。
    - **「关键句」定义**：取首句（摘要）+ 含最高频 jieba 实词的句子（最多 K=3 句）+ 末句（结论）；拼接后若仍超目标长度，截断到目标。确保压缩后仍可被 BM25/语义命中（保留实体与关键词）。
    - **适用范围**：仅压缩 `category='observation'` / source 含 `dialog` 的流水，不压缩 `remember` 的高优事实（避免失真）。
  - **可选 LLM 钩子（不默认开）**：预留 `MEMORIA_DISTILL_LLM_URL` env，配置则走外部蒸馏；缺省走启发式。
- **风险**：中。须保证压缩后不丢检索关键 token。
- **验证**：超长 `memory_observe` → DB `content` 压缩版、`raw_ref` 存原文；短内容原样；压缩后仍可 `memory_recall` 召回。

---

## 4. 实施顺序与节奏（按评审修订：F1 拆 F1a/F1b）

风险递增，每步独立 `build → 脱离式重启 → 场景验证`：

1. **F3 可解释性**（纯输出层，零破坏）
2. **F1a 召回回写**（新列 `access_count` + 异步批量回写，不动排序）
3. **F1b 跨通道重排**（融合 recency+freq，env 可调权重）
4. **F2 时间有效性**（复用 `include_superseded` + 降权分桶，保默认语义）
5. **F4 摄入压缩**（启发式 + `raw_ref` 回填，中文阈值 + jieba）

构建：`cd memoria-open && cargo build --release`（产物 `target/release/memoria-server.exe`）。
重启：先停运行实例（避免文件锁），脱离式起 `memoria-server.exe` 并注入 `MEMORIA_DB_PATH` / `MEMORIA_WEB_DIR` 等 env（参考 `start_both_tray.ps1`）。

---

## 5. 验收清单（回归口径）

- [ ] `cargo build --release` 通过，二进制生成。
- [ ] **基线（务必不低于）**：recall@10 ≈ **63.9%（3 次重启均值，区间 61.7~66.7）**、@5≈56.4%、@1≈23.9%（来源 `eval/recall_rate_report.md`，方法学铁律：同进程复测或多次重启取均值，单次对比因 HNSW 重建抖动 ±3~5pp 不可信）。
- [ ] F3：`memory_recall` 返回含 `primary_channel` + `channel_scores`（复用 `channel_of`）。
- [ ] F1a：连续召回后 DB `access_count` 递增、`last_recalled` 刷新；`recall_count` 不变；`decay.rs` 冷热判据不受影响。
- [ ] F1b：常用/近期记忆上浮；`eval/eval_recall_corrected.py` 同进程复测 recall@10 ≥ 63.9%（不退化）。
- [ ] F2：默认只见当前真值；`include_superseded=true` 旧事实降权（`×MEMORIA_HISTORY_DOWNWEIGHT`）补回并标 `time_status`。
- [ ] F4：超长 observe 落 `raw_ref`、短内容原样；中文 600 字阈值生效；压缩后仍可召回。
- [ ] 混合召回整体 recall@10 不退化（对照 §5 基线）。

---

## 6. 不重造清单（强项，本次不碰）

混合 BM25(jieba)+HNSW(1024d) 召回 / `memory_graph` / `dream_state` 空闲巩固 / `evolution_log` / `decay`+`importance`（含 temporal `recall_count*0.1` bonus）/ `valid_from|valid_to|superseded_by` 双时态框架 / `intake_filter` 源头降噪。

---

## 7. 集中默认值表（所有可调参数）

| 参数 | 默认 | 含义 | 归属 |
|---|---|---|---|
| `MEMORIA_HISTORY_DOWNWEIGHT` | `0.5` | F2 旧事实降权系数 | F2 |
| `MEMORIA_FREQ_K` | `10` | F1b 频率饱和常数（freq_n = acc/(acc+K)） | F1b |
| `MEMORIA_RECENCY_LAMBDA` | `0.01` | F1b 新鲜度衰减率（recency_n = exp(-λ·age_h)) | F1b |
| `MEMORIA_RERANK_W_FREQ` | `0.1` | F1b 频率分量权重 | F1b |
| `MEMORIA_RERANK_W_REC` | `0.1` | F1b 新鲜度分量权重 | F1b |
| `MEMORIA_DISTILL_MAX_CHARS` | `1200` | F4 拉丁文压缩阈值（字符） | F4 |
| `MEMORIA_DISTILL_MAX_CHARS_CN` | `600` | F4 中文压缩阈值（字符） | F4 |
| `MEMORIA_DISTILL_LLM_URL` | 空（关） | F4 可选 LLM 蒸馏端点 | F4 |

> 注：`MEMORIA_RERANK_W_RRF=0.3 / W_SEM=0.2 / W_KW=0.5`（已固化默认，保留 env 覆盖）沿用不变。

**F1 语义决策（评审 P1#5）**：`access_count` = 新增「召回命中计数」；`recall_count` 保留「写入/去重自增」原义。二者解耦，保护 `decay.rs:33` 冷热判据（依赖 `recall_count < 3`）不被污染。

---

## 8. 开放问题（已据评审收敛，余 1 项待确认）

1. **F1b 是否默认开启权重**：`w_freq/w_rec` 默认 0.1（轻微上浮，对基线 recall@10 影响应在抖动内）。是否保持默认开启，还是默认 0（纯退化、最简回归）？——**建议默认 0.1**（方向正确小幅改善，可调到 0）。
2. （其余开放问题已在 §7 默认值表中集中给出推荐默认，无需逐条确认。）

> 确认上述任一项后我即按 §4 顺序落地；未确认项采用上表「推荐默认」先实现，并在提交说明中标注。
