# LongMemEval 完整集回归报告

> 日期: 2026-08-16
> 数据: `xiaowu0162/longmemeval-cleaned` → `longmemeval_s_cleaned.json`（500 问，完整集）
> 性质: HY3 执行单 §7 未完成项 —— LongMemEval 完整集作回归（H4：回归指标，非产品 KPI，不为刷榜改架构）
> Harness: `eval/longmemeval/longmemeval_eval.py`（复现见下）

## 0. 一句话结论

memoria 在 LongMemEval 完整集（500 问）上的**证据命中率（answer session 进 top-8）= 96.8%**，
LLM-as-judge 平均分 **7.05 / 10**（pass@8 = 59.6%）。检索侧（memoria 召回）与作答侧（Qwen2.5-72B）
分离统计：命中率衡量 memoria 本身，均分衡量「召回 + 作答」端到端。

> 执行日期：2026-08-16 22:14 – 2026-08-17 00:27（含 2 次服务端挂起自愈重启，resume 续跑无丢题）

## 1. 评测方法（与 LoCoMo 评测对齐，见 eval/locomo/locomo_eval.py）

1. **灌库**：每问一个命名空间 `longmemeval/<question_id>`，其 haystack 会话（38-62 个/问，含
   sharegpt/ultrachat 填充会话）按 5000 字符分块，每块一条 `memory_remember`：
   - content 头部带 `[会话日期] [session_id] (part k/n)`（续块可自证归属，供证据命中统计）；
   - `raw_ref` 占位绕开 distill 压缩（评测口径 = 原文召回；压缩是独立变量，另测）；
   - tags: `longmemeval:<qid>` / `session:<sid>` / `occurred:<date>`。
2. **检索**：`memory_search_v2`（prod 配置：recall_depth=100、cross-encoder rerank pool=100、
   graph hops=0），top-8。
3. **证据命中**：检索结果中是否出现 `answer_session_ids` 任一 session 的块（经 web API 拉全量
   内容后解析块头）。隔离「检索」与「作答」两个环节。
4. **作答**：检索到的 8 个块**完整内容** + 问题 → Qwen2.5-72B-Instruct 生成答案
   （服务端 search 响应截断 2000 字符，答案句常在块中后部，故经 `/api/memories?id=` 取全量）。
5. **Judge**：0-10 宽松打分（与 LoCoMo 同 prompt；10=完全正确，8-9=小偏差，5-7=部分正确，
   0-4=错误/幻觉/应答 UNKNOWN），Qwen2.5-72B-Instruct。
6. **聚合**：总体均分、pass@8、证据命中率、按 `question_type` 与能力域（映射见下）。

### 能力域映射（cleaned 数据无 capability 字段，按官方任务命名近似映射）

| question_type | 能力域 | n |
|---|---|---|
| single-session-user | Information Extraction | 70 |
| single-session-assistant | Information Extraction | 56 |
| multi-session | Multi-Session Reasoning | 133 |
| knowledge-update | Knowledge Updates | 78 |
| temporal-reasoning | Temporal Reasoning | 133 |
| single-session-preference | Abstraction & Reasoning | 30 |

## 2. 复现

```bash
# 1) 下载数据（277MB，放脚本同目录或设 LME_DATA_PATH）
curl -L -o longmemeval_s_cleaned.json \
  https://hf-mirror.com/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json
# 2) 跑（全量 500 问；--limit-questions N 可调试；断点自动 resume）
python longmemeval_eval.py --ingest-workers 4
# 3) 清理评测命名空间（备份 + 删除）
python clean_lme.py --confirm --backup
```

环境: memoria-server :9003（prod 配置）+ embed :8777（Qwen3-VL-Embedding-8B, 1024d）；
SiliconFlow key 读 `SILICONFLOW_API_KEY` / `MEMORIA_JARVIS_BADGE`（兜底 `~/agent-core/.env`）。

## 3. 结果（全量 500 问）

### 3.1 总体

| 指标 | 值 |
|---|---|
| 总 QA | 500 |
| 评分 | 500（跳过 0） |
| 平均分（0-10） | **7.05** |
| pass@8 占比 | **59.6%**（298/500） |
| 证据命中率（answer session ∈ top-8） | **96.8%**（484/500） |
| UNKNOWN 回答数 | 83（16.6%） |

### 3.2 按 question_type / 能力域

| question_type | 能力域 | n | 平均分 | pass@8 | 命中率 | UNKNOWN |
|---|---|---|---|---|---|---|
| single-session-user | Information Extraction | 70 | 8.64 | 89% | 96% | 10 |
| single-session-assistant | Information Extraction | 56 | 9.21 | 98% | 100% | 0 |
| multi-session | Multi-Session Reasoning | 133 | 6.20 | 43% | 96% | 33 |
| knowledge-update | Knowledge Updates | 78 | 7.77 | 73% | 100% | 4 |
| temporal-reasoning | Temporal Reasoning | 133 | 5.98 | 43% | 95% | 34 |
| single-session-preference | Abstraction & Reasoning | 30 | 5.93 | 33% | 93% | 2 |

按能力域聚合：

| 能力域 | n | 平均分 | pass@8 | 命中率 |
|---|---|---|---|---|
| Information Extraction | 126 | 8.90 | 93% | 98% |
| Multi-Session Reasoning | 133 | 6.20 | 43% | 96% |
| Knowledge Updates | 78 | 7.77 | 73% | 100% |
| Temporal Reasoning | 133 | 5.98 | 43% | 95% |
| Abstraction & Reasoning | 30 | 5.93 | 33% | 93% |

### 3.3 检索侧观察

- **未命中 16/500（3.2%）** = memoria 召回短板，逐条归因（keyword/semantic/rerank 通道）；
- **命中但答 UNKNOWN 83 条** = 检索达标但作答/标注问题（top-8 块中无答案句所在块、或标注偏弱），需人工抽样核对；
- **多跳/时序类（multi-session、temporal-reasoning、single-session-preference）均分 5.9-6.2**：命中率高（93-96%）但作答侧折损明显——8 块截断上下文不足以支撑跨会话聚合推理，块粒度是后续主调节旋钮（调小更精，灌库量线性上升）；knowledge-update 命中率 100% 且 pass@8 73%，验证了「知识更新」场景 memoria 的时序召回能力。

## 4. 与 LoCoMo 的对照（方法学一致性）

| 项 | LoCoMo | LongMemEval |
|---|---|---|
| 灌库粒度 | 逐 turn | 5000 字符分块（会话级） |
| 命名空间 | locomo/<sid> | longmemeval/<qid> |
| 检索 | memory_search_v2 top-15 | memory_search_v2 top-8 |
| 证据统计 | - | answer session ∈ top-8（新增） |
| 作答模型 | Qwen2.5-7B | Qwen2.5-72B（7B 长上下文实测复读崩坏，见 §5） |
| Judge | Qwen2.5-7B | Qwen2.5-72B |

## 5. 方法学注记（已知取舍，均记录在案）

1. **分块粒度**：5000 字符/块（≈1250 token，对齐论文检索基线段落粒度）；答案句若在块尾部，
   经全量内容拉取保证 LLM 可见；块粒度是召回精度的主要调节旋钮（调小更精但灌库量线性上升）。
2. **作答模型**：7B 在 8×5000 字符上下文下出现复读/幻觉崩坏（n=1 能答、n=8 乱码，实验见
   `exp_q2_len.py`），换 72B 后 Q1/Q2 满分；评测的是 memoria 召回 + 通用 LLM 作答的组合，
   作答模型更换不影响「证据命中率」这一检索侧指标。
3. **raw_ref 占位**：绕开 distill 摄入压缩（原文入库）。distill 对长会话的压缩损失是独立变量，
   后续可加开/关 A/B。
4. **数据质量**：部分问题标注偏弱（如 assistant 举例「like Target」被当作答案证据），
   模型正确答 UNKNOWN 会被 judge 按口径记 4 分——属基准数据特性，不是系统缺陷。
5. **评测数据/结果不入库**（277MB + 派生 JSON，见 .gitignore）；报告与 harness 入库。
