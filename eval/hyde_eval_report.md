# HyDE 召回增益实证评测（A/B，2026-07-26）

## 目的
量化 HyDE（Hypothetical Document Embeddings，假设文档嵌入）在 memoria 当前部署下
（Qwen3-VL-Embedding-8B 向量 + 2-hop 图扩展 + cross-encoder 重排）的**真实端到端召回增益**。
HyDE 本职是缩小「用户问句 vs 知识库陈述」的措辞 gap——本评测验证它在本栈是否真有价值。

## 方法
- **基准构造**：从 `agent/xujiayan` 抽取 58 条有效、未被取代、长度适中的答案型记忆，
  用硅流 LLM（Qwen2.5-7B-Instruct）把每条内容改写成**自然语言问句**（刻意换措辞制造 lexical gap），
  固定存为 `hyde_queries.json`（gold = 原记忆 id，不入库）。
- **公平 A/B**：仅切换 memoria 服务端 `MEMORIA_HYDE` 环境变量（OFF vs ON），query 输入完全相同，
  其余检索 env（recall_depth=100、rerank、graph_hops=2 等）与生产完全一致。
- **指标**：
  1. 真实 `memory_recall` 全管线 **hybrid recall@k**（用户可感知口径）；
  2. semantic 通道直接命中（gold 进 top-k 且其 `signal_scores` 含 `semantic`）——隔离 HyDE 作用的向量侧；
  3. **嵌入层诊断**：直接比 `cos(gold_vec, raw_query_emb)` vs `cos(gold_vec, hyde_query_emb)`，
     绕过全管线噪声，定位 HyDE 在向量层面的净效应。

## 结果

### 1) 全管线 A/B（n=58）
| k | OFF hybrid | ON hybrid | Δhybrid | OFF sem | ON sem | Δsem |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 29.3% | 29.3% |  0.0pp | 1.7% | 0.0% | −1.7pp |
| 3 | 31.0% | 31.0% |  0.0pp | 1.7% | 0.0% | −1.7pp |
| 5 | 32.8% | 32.8% |  0.0pp | 1.7% | 0.0% | −1.7pp |
|10 | 32.8% | 34.5% | +1.7pp | 1.7% | 0.0% | −1.7pp |

→ 全管线**无显著差异**；@10 的 +1.7pp 落在重启抖动范围内（OFF/ON 各跑一次，非交错）。

### 2) 嵌入层诊断（n=58）
| 量 | 值 |
|--|--|
| avg cos(raw_query → gold) | **0.5471** |
| avg cos(hyde_query → gold) | **0.4585** |
| HyDE 更接近 gold 的比例 | 6/58（10.3%） |
| mean Δ(hyde − raw) | **−0.0886** |
| median Δ | −0.0821 |

→ HyDE 把查询向量**推离**目标答案（平均 −0.089），仅 10% 的用例因 HyDE 更靠近 gold。

## 结论
1. 在本部署下，HyDE **未带来任何召回增益**；嵌入层证据表明它反而稀释了查询-答案相似度。
2. **根因**：嵌入模型 Qwen3-VL-Embedding-8B 已擅长「问句↔答案」语义匹配（raw cos 已达 0.55），
   HyDE 改写属于**冗余**；且假设答案由较弱的 Qwen2.5-7B-Instruct 生成，措辞偏离真实存储事实，
   进一步把查询向量带偏。HyDE 的经典收益场景是**嵌入模型弱、问句-答案 gap 大**——本栈不属于该场景。
3. **代价**：HyDE 每次查询多一次 LLM 调用，延迟 ~2.7s → ~4.2s（**+~50%**）。
4. **生产建议：保持 `MEMORIA_HYDE` 关闭**（默认即关）。在换用弱嵌入模型或观测到问句-答案 gap 巨大前，不开启。
   `start_memoria_only.ps1` 第 43 行 `MEMORIA_HYDE` 维持注释状态即可。

## 复现
```bash
# 1) 构造基准（硅流 LLM 改写 58 条记忆为问句，固定存盘）
python eval/eval_hyde_recall.py --build --out eval/hyde_off.json
# 2) 当前（HyDE 关）先跑一次基线（上面 --build 已顺带跑）
# 3) 停 memoria → 注入 MEMORIA_HYDE=1 重启 → 跑 ON
python eval/eval_hyde_recall.py --out eval/hyde_on.json
# 4) 对比
python eval/eval_hyde_recall.py --compare eval/hyde_off.json eval/hyde_on.json
```
派生数据 `hyde_queries.json / hyde_off.json / hyde_on.json` 含记忆内容样本，已在 `.gitignore` 排除，不入库。
