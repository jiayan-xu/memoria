# Memoria × HMS 设计吸收优化方案

> 来源：`Shadow-Weave/HMS`（Holographic Memory System，MIT，建仓 2026-07-12，207⭐）。
> 目的：评估 HMS 哪些设计值得吸收进 Memoria（薄存储 / 身份总线，脑子在 agent-core），
> 并给出可落地的改造计划。结论基于 HMS 源码精读（alembic 迁移、`consolidator.py`、`organizer.py` 行号证据）。

---

## 1. 背景与边界

HMS 比其 README 暗示的**成熟得多**：80+ 个 alembic 迁移，含 consolidation、entity_resolver、audit_log、
webhooks、observations、memory_links 图、甚至 Oracle 基线。但它的本质是
**「一体化记忆 + 推理编排服务」**——把 retain → recall → answer → judge 全部耦合进同一个服务。

Memoria 的定位是**「薄存储 / 身份总线」**：存储、时序真值、namespace 权限、MCP 接口在这里，
而推理编排（脑子）在 agent-core。这个架构差异决定了吸收边界——**只吸收"总线职责内、高价值低成本"的项**。

---

## 2. HMS 实际架构（源码事实）

| 维度 | 实现 |
| --- | --- |
| 存储 | PostgreSQL + pgvector（默认），另含 Oracle 基线 |
| 租户分区 | `bank_id`（每用户/每 agent）≈ Memoria 的 `namespace` |
| 核心表 | `memory_units`（事实）、`mental_models`/`observations`（结构化模型）、`memory_links`（图边）、`chunks`（源切片）、`entities`+共现、`audit_log`、`tags[]` |
| 时间模型 | `event_date`/`occurred_start/end`（事件何时发生）↔ `mentioned_at`（何时被提及）。**无 valid_to / 无 supersede**；全部保留，观察更新走 in-place + history JSONB，矛盾则 DELETE |
| Retain | 切块 → LLM 抽取事实 → embedding → 写库 → 异步 consolidation（召回旧观察 → 单 LLM 调用 create/update/delete → 按 tags 隔离） |
| 证据账本 | `vendor_sdk/.../organizer.py` 的 `EvidenceOrganizer.organize()` 在**答案时刻**把召回组织成 `EvidenceLedgerRow(index, score, text, document_id, type, occurred, mentioned, chunk_id, entities)` + 原始 snippet + 控制清单 |
| Self-Evolution 控制 | `organizer._controls`：4 条关键词触发的启发式（非 ML） |
| 接口 | HTTP + MCP（FastMCP） |

### Self-Evolution 四条控制规则（关键可借鉴点）
1. `count_total_deduplication`：先枚举唯一事件再计数（"how many / total / count"）。
2. `relative_date_grounding`：以 `question_date` 解析相对日期（"ago / before / after / last / next / yesterday / tomorrow"）。
3. `amount_difference_calibration`：仅当两侧数据齐全才计算差值（"how much / difference / cost / spent / amount"）。
4. `current_previous_state_arbitration`：当前态取最新、历史态取最旧（"current / latest / previous / initially / before"）。

---

## 3. Memoria 当前架构（既定设计）

- **定位**：薄存储 / 身份总线；脑子在 agent-core。
- **存储**：SQLite 本地（生产库约 276 MB 单节点）。
- **分区**：`namespace` + 权限门控（`dashboard-agent` 为 admin 身份，namespace 访问门）。
- **时序真值（已落地）**：supersede 模型——不可变版本 + `valid_from`/`valid_to` + `is_latest_now` + `as_of`（`visible_as_of`）。
- **召回/上下文（已落地）**：`memory_profile`（static/dynamic）、`memory_context`（返回 prompt_block）、`memory_search_v2`、`memory_maintenance_normalize`。
- **固化（规划中）**：A1 `dream_state`；A2 `agent.rs:consolidate(ns)` 拉原料→LLM 提炼→`memory_remember`→推游标（低峰触发）；B1-B3 实体图谱（`entities`/`entity_mentions`/`entity_edges` + NER）。
- **配额**：`profile_bucket`（每 ns ≤10 次/分，admin 豁免）。
- **接口**：MCP（JSON-RPC `tools/call`），已注册全部 P0 工具。

---

## 4. 吸收决策矩阵

| HMS 设计 | 判定 | 优先级 | 落地成本 | 在 Memoria 的落地位置 |
| --- | --- | --- | --- | --- |
| 事件时间 vs 提及时间双轨（`occurred_*`/`mentioned_at`） | **吸收** | P0+ | 低（加列） | `memories` 新增 `event_time`（发生时刻），与 `valid_from`（断言时刻）区分 |
| 类型化证据账本召回（`EvidenceLedgerRow`） | **吸收** | P0+ | 低（改造 context 输出） | `memory_context` 由 prompt_block 升级为结构化账本：每行带 `type / occurred / mentioned / source_ref / entities / score` |
| Self-Evolution 控制规则（4 条启发式） | **吸收** | P0+ | 极低（prompt 护栏） | 在 `memory_context` / agent-core compose 步加入确定性护栏 |
| 类型化信号字段 `text_signals` | 战略 | P1 | 低 | `memories` 增 `signals`（JSON：numeric/date/update）；也可由 content+tags 派生 |
| 成熟 consolidation 质量技术（rerank/共现/矛盾即删） | 战略 | P1 | 中 | 吸收质量手法（rerank、共现回填、冲突检测），但**矛盾处理保留 supersede** 而非 DELETE |
| 多角色 LLM 编排（core/recall/retain/answer/judge） | 不吸收 | — | — | 「脑子」职责，HMS 耦合进记忆服务；Memoria 刻意分离，脑子在 agent-core |
| observations / directives / reflections / oracle | 不吸收 | — | — | 高阶 agent 认知构造，属 agent-core 范畴 |
| 换 PostgreSQL + pgvector 引擎 | 不吸收 | — | — | Memoria 是 SQLite 本地单节点（运维优势）；换引擎是复杂度倒退 |
| 出站 webhooks / 基准复现 lab | 不吸收 | — | — | 出站动作已由 agent-core 边界门控；lab 是研究工具，非产品 |

### 关键反直觉点
HMS 用「in-place 更新 + history + 矛盾即删」；Memoria 的 **supersede（不可变版本 + `valid_to` + `is_latest_now` + `as_of`）在生产可审计性 / 时序回溯上更干净**。**不要回退到 HMS 的更新-删除模型**——这是 Memoria 相对 HMS 的优势，应保留而非对齐。

---

## 5. 优先改造（落地草拟）

### ① 双轨时间：新增 `event_time`
```sql
-- 参考既有 migrate_superseded_by 的幂等写法
ALTER TABLE memories ADD COLUMN event_time TEXT;  -- 事情发生时刻，与 valid_from(断言时刻) 区分
```
召回时 `memory_context` 同时输出 `occurred`(event_time) 与 `mentioned`(valid_from)，让 agent-core
能判断「这件事何时发生 vs 我何时知道」——直接对应 HMS 研究的「新旧状态区分」失败模式。

### ② `memory_context` 升级为类型化证据账本
```text
返回结构由纯 prompt_block →
[{ index, score, text, source_ref:"ns:id", type, occurred, mentioned, entities:[...] }, ...] + raw_snippets
```
复用 HMS `EvidenceLedgerRow` 字段语义，但数据源来自 Memoria 既有 supersede + 实体图谱。
无需改存储，只改 context 组装逻辑（P0-3 已留口子）。

### ③ recall 侧 Self-Evolution 护栏
```text
在 compose / context 注入前，按问题关键词触发：
count_dedup / relative_date_ground / amount_diff_calib / current_vs_prev_arbitration
```
纯规则、零 LLM 调用。直接搬 HMS `organizer._controls` 的四条启发式，作为 prompt 侧 checklist 注记。

**吸收顺序建议**：②③ 同为 `memory_context` 改造，可合并为一次 P0+ 迭代；
① 加列需伴随幂等迁移。三项都不触及 supersede 时序真值内核，风险可控。

---

## 6. 不吸收的边界（为什么）

- **职责边界**：Memoria 是总线，agent-core 是脑子。HMS 把 retain→recall→answer→judge 全塞进一个服务，
  是其「可复现研究框架」定位使然；Memoria 的生产约束要求分离（安全边界、namespace 权限、两层门控）。
- **引擎选择**：SQLite 本地是 Memoria 的运维优势（单文件、约 276MB、零运维）。PostgreSQL+pgvector 的
  分布式/高并发能力对当前场景是过度设计。
- **版本模型**：HMS 的「in-place 更新 + history + 矛盾即删」在审计、时序回溯（`as_of`）上弱于 Memoria
  的不可变 supersede。**这是 Memoria 的优势，应保留而非对齐。**

---

## 7. 实施计划与验收

1. **Phase A（P0+，本次）**：
   - `migrate_event_time` 幂等迁移（参考 `migrate_superseded_by`）。
   - `memory_remember` 接受可选 `event_time`，写入列。
   - `memory_context` / `memory_search_v2` 召回结构补充 `occurred`/`mentioned`/`source_ref`/`entities` 字段（账本化）。
   - 新增 `self_evolution` 护栏模块，在 recall 组装时按关键词触发 4 条规则，作为注记注入。
   - 单测 + `cargo build --release` + 隔离冒烟。
2. **Phase B（P1，后续）**：
   - `signals` 类型化信号字段（或派生）。
   - consolidation 质量技术（rerank / 共现 / 冲突检测），矛盾处理仍走 supersede。
3. **验收**：`memory_context` 返回结构化账本；`as_of` 与 `event_time` 互不干扰；护栏命中时注入 checklist；
   既有 `dashboard-agent` admin 路径不受影响；生产 cutover 沿用既有双树纪律（canonical → 运维树二进制）。

---

## 8. 实施状态（Phase A 已发布）

> 本节为落地后补记，已脱敏（不含本机绝对路径 / 生产密钥）。

**已交付（canonical 树 `memoria-open`，分支 `main`）：**
- `src/storage/sqlite.rs`：`migrate_event_time()` 幂等迁移（`ALTER TABLE memories ADD COLUMN event_time TEXT`，参考 `migrate_superseded_by` 写法）；在 `lib.rs` / `main.rs` / `mcp_server.rs` 测试 bootstrap 共 4 处注册。
- `src/tools/remember.rs`：新增 `set_event_time(pool, memory_id, event_time)`，在 `memory_remember` 成功写入后独立 `UPDATE`；未改动 `remember_with_dedup` 签名（避免 28+ 调用方回归，采用「成功返回后再补写」方案）。
- `src/tools/ledger.rs`（新建）：`enrich_ledger()` 把 `FusedResult` 富化为类型化账本——每行带 `type`(=category) / `occurred`(=event_time 或兜底 valid_from) / `mentioned`(=valid_from) / `source_ref`(=`namespace:id`) / `entities` / `is_latest` / `rrf_score`。
- `src/tools/self_evolution.rs`（新建）：`guardrails(query)` 按中英文关键词触发 4 条规则注记（`COUNT_TOTAL_DEDUP` / `RELATIVE_DATE_GROUNDING` / `AMOUNT_DIFFERENCE_CALIBRATION` / `CURRENT_PREVIOUS_ARBITRATION`），纯关键词启发式，零 LLM 调用。
- `src/tools/profile.rs` 的 `memory_context`：`recall` 改用 `enrich_ledger`，返回 JSON 新增 `guardrails` 字段。
- `src/mcp_server.rs`：`memory_search_v2` 改用账本 + `guardrails`；`memory_remember` 成功写入后调 `set_event_time`；`memory_remember` / `memory` 的 JSON schema 各加 `event_time`（标注「吸收 HMS：事件「发生」时刻」）。
- `tests/p0_hms_absorption.rs`（新建，5 测试）：`event_time` 与 `valid_from` 区分、账本字段富化、护栏关键词命中、context 含 `guardrails` —— 全部通过。

**验证结论：**
- `cargo test` 全绿（含 5/5 新测试）；`cargo build --release` 成功。
- 隔离冒烟（临时库）：`memory_remember` 传 `event_time` → `memory_context` 返回 `occurred≠mentioned`、`type`/`source_ref` 正确。
- 生产 cutover（双树纪律）：新 `memoria-server.exe` + `memoria_core.dll` 覆盖进运维树 release 目录，由看门狗重启 9003；生产主库 `event_time` 列迁移零改写落地；`memory_search_v2` 在真实数据上返回结构化账本 + `guardrails`；`dashboard-agent` admin 路径正常。

**未做（保持边界）：** Phase B（`signals` 字段、consolidation 质量技术）按计划留后续；不吸收项（多角色编排 / observations / PostgreSQL / webhooks）维持原决策。
