# A+C 搜索召回增强 — 实现与验证报告

> 目标：手动实现 ① **A = 两阶段召回+重排**（向量宽召回 top-50 → 重排精到 top-10 降误召）② **C = 混合 BM25**（加权 RRF 融合）。
> 仓库：`memoria-open`（`src/search/`、`src/storage/fts5.rs`、`src/mcp_server.rs`）
> 验证命名空间：`agent/xujiayan`（占库 94%）

## 改动清单

| 文件 | 改动 |
|---|---|
| `search/rrf.rs` | `FusedResult` 新增 `sem_cos`/`kw_bm25` 两字段；`rrf_merge` 累加时按通道捕获 cosine/BM25 原始幅度；`graph_expand` 邻居补 `None`。 |
| `search/hybrid.rs` | 新增 `two_stage_rerank()`（归一化 RRF + cosine + BM25 加权混合，再 `take(max_results)`）；顶部可配 env：`MEMORIA_RECALL_DEPTH`(50)、`MEMORIA_WEIGHT_KEYWORD/SEMANTIC`、`MEMORIA_RERANK_W_RRF/SEM/KW`(默认 0.4/0.3/0.3)；主信号宽召回深度 = `max(50, max_results*3)`。 |
| `mcp_server.rs` | **关键 bug 修复**：查询嵌入注入条件原漏 `memory_recall`（只 `memory_search`/`memory_search_v2`）→ recall 语义通道恒空；已补齐。recall 输出 JSON 新增 `signal_scores`/`sem_cos`/`kw_bm25`。 |
| `storage/fts5.rs` | `tokenize_for_fts()` 经 4 次迭代最终修复：对每个 jieba token 用双引号包裹（`"agent-core"`），避免连字符破坏 FTS5 `MATCH` 语法。 |
| `search/keyword.rs` | 改用 `fts5::tokenize_for_fts`（显式 ` OR ` 连接，宽召回）。 |

## C 的根因（4 次迭代）

1. 空格连接多词 = AND（FTS5 本机构建）→ 改用 ` OR `。
2. 误判索引用 unicode61 逐字切 → 实测 `MATCH '权 限'=0` 证伪（索引实为 jieba 整词）。
3. **真根因**：带连字符的 token（如 `agent-core`）触发 `no such column: core` 语法错，整条 FTS 查询失败、keyword 通道恒空 → 逐 token 双引号包裹解决。

## 验证结果（修复前后 `kw_filled` = 含 BM25 幅度的结果数 / 10）

| 查询 | 修复前 | 修复后 |
|---|---|---|
| `agent-core 权限 两层 门控 校验` | 0/10 | **1/10** |
| `记忆演化 evolution_log 回滚` | 0/10 | **2/10** |
| `PFAiX 智浦助手 构建 发布` | 1/10 | **8/10** |
| `HNSW 向量 召回 覆盖度 回填` | 2/10 | **3/10** |

语义通道（`sem_cos`≈0.6–0.78）全程激活；`source` 带 `;rerank2` 标记证明两阶段重排已生效。
单测 `cargo test --lib`：**15 passed / 0 failed**。

## 可调环境变量（免重编译微调）

- `MEMORIA_RERANK_W_RRF` / `MEMORIA_RERANK_W_SEM` / `MEMORIA_RERANK_W_KW`：两阶段重排三轴权重（默认 0.4 / 0.3 / 0.3）。
- `MEMORIA_RECALL_DEPTH`：语义/关键词宽召回深度（默认 50）。
- `MEMORIA_WEIGHT_KEYWORD` / `MEMORIA_WEIGHT_SEMANTIC`：RRF 一阶主信号权重。

## 运维

- 重编前按纪律停 `memoria-server.exe`（释放 exe 锁）；仅直起 memoria（不碰 agent-core/bridge）。
- 验证脚本：`eval/validate_recall_bm25.py`（读 `.env` 的 `MEMORIA_ADMIN_KEY` 不打印，POST `/mcp` tools/call memory_recall，逐条打印 `kw_bm25`/`sem_cos`/`signal_scores`）。
