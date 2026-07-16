# Memoria：Memory Profile / 图语义 / 检索默认 — 详细设计

> **同步说明**：本文由运维树脱敏同步至 memoria-open（2026-07-16）。本机绝对路径已改为占位符；不含密钥。


| 字段 | 值 |
|------|-----|
| **状态** | Revised |
| **日期** | 2026-07-15 |
| **性质** | 设计文档（只谈怎么落地；**本稿不改业务代码**） |
| **适用范围** | Memoria 运维树 `memoria-core` + 开源树 `memoria-open`；联调面 agent-core / PFAiX |
| **前置裁决** | Memoria = Agent **记忆/身份总线（薄存储）**；**脑子在 agent-core**；不抄 Supermemory 整包 |
| **对照基线** | 现网 MCP/表结构（见 §3）；Supermemory 仅借 API/语义，不借产品形态 |
| **双树** | 运维：`<ops-memoria-root>`（含内部文档，`.NO_PUSH`）；公开：`…/memoria-open` → `jiayan-xu/memoria` |
| **修订说明** | 对照审查结论修订：拍板 as_of/valid_to 方案 (A)；修正 keyword/dynamic/配额/insight 事实错误；统一 P0/P1 边界与闭区间时序语义 |

---

## 0. 一页摘要

**目标**：在不改变「薄存储」定位的前提下，补齐会话开场可注入的 **`memory_profile(ns)`**、默认只看当前 tip 的 **`isLatest`（`superseded_by IS NULL`）**、以及 MCP 三件套 **`memory` / `recall` / `context`**；P1 再划清记忆检索 vs 文档 hybrid、TTL 临时事实、记忆边 `updates|extends|derives`（derives→Dream）。

**不做**：Drive/Gmail 连接器、多模态抽取核心化、Memoria 内置自动抽事实主循环、LongMemEval 刷榜、图数据库、把 RAG 文档库并进 Memoria。

**已有可复用**：`memory_user_prefs` / 遗留 `memory_recent_decisions`（`decisions` 表）/ `superseded_by` + `memory_dedup_chain` / `valid_from|valid_to` + `as_of` / 5 信号 `hybrid_search` / Dream 游标 + agent-core `consolidate` / entity 图谱。

**缺口（本稿要设计）**：Profile 合成出口；检索默认过滤 superseded；supersede 事务内必写 `valid_to`（方案 A，支撑 as_of）；显式 `supersedes_id`（P0）与边枚举 `updates|extends|derives`（P1）；`context` 会话注入；TTL 与 supersede 共用 `valid_to` 的优先级。

---

## 1. 目标与非目标

### 1.1 目标

| 优先级 | 目标 | 成功判据（摘要） |
|--------|------|------------------|
| **P0** | `memory_profile(ns)` 返回 `static` + `dynamic`，供会话开头一次注入 | 单次 MCP 调用 <100ms 量级（本地 SQLite）；agent-core 不必自行拼 prefs / memories(decision\|fact\|pattern) |
| **P0** | **`superseded_by` 默认过滤** + **显式 `supersedes_id`** | 默认搜索不返回已被 supersede 的旧事实；写入可显式指定被取代 id；`as_of` / `include_superseded` 可取历史 |
| **P0** | MCP UX：`memory` / `recall` / `context`（或等价别名） | `context` = Profile 主动灌入；`recall` = 按需检索；`memory` = 写入/更新语义清晰 |
| **P1** | 记忆边枚举：`updates` / `extends` / `derives`（严格 P1，非 P0） | schema CHECK 扩展；矛盾更新挂 `updates`；巩固产物挂 `derives` |
| **P1** | 划清 `memories` vs `hybrid` 检索边界 | 文档/知识库 RAG 不进 Memoria 核心路径；契约写死 |
| **P1** | 临时事实 TTL（日期型过期） | 写入可设 `valid_to` / `ttl`；到期后默认检索不可见（复用 as_of）；与 supersede stamp 共用列见 §9 |
| **P2** | 插件矩阵 / MemoryBench 刷榜 / 一键全家桶 | **仅文档提及**，不排期强推 |

> **P0 / P1 边界（硬对齐）**：P0 只保证「列指针过滤 + 显式 supersedes_id + Profile/context」。`updates|extends|derives` 边枚举与近义边改名属 **P1**，不得写成 P0 验收项。

### 1.2 非目标（硬约束）

| 不做 | 原因 |
|------|------|
| 第二个 Supermemory（连接器 + 整包 RAG + 多模态核心） | 出记忆/身份总线边界；OCR/联单在 dashboard |
| Memoria 内嵌「零配置自动抽事实」主循环 | 脑子在 agent-core；抽事实留在 `consolidate` |
| 追 LongMemEval / 公开刷榜 KPI | 已否决 |
| 引入 Neo4j / 云向量硬依赖 | 保持 SQLite + FTS5 + 本地 HNSW |
| 壳（Jan/PFAiX）直连 Memoria 拼 Profile | 见 agent-core `SHELL_ENGINE_BOUNDARY.md` |
| 把业务文档库吞进 `memories` 表当 RAG | 边界见 §7 |
| 新增库列 `kind` / `superseded_at` | 投影字段走 tags/category；时序戳复用 `valid_to`（方案 A） |

### 1.3 定位一句话

> **Memoria**：带 NS/鉴权的记忆与身份总线（存、检、图、巩固游标）。  
> **agent-core**：推理、路由、红线、Dream LLM、会话注入编排。  
> **壳**：只经引擎，不碰 `:9003`。

---

## 2. 与现有代码的映射表（已有 → 扩展）

路径以运维树为准；开源树 `memoria-open/src/…` 结构同构。

| 能力 / 概念 | 现状（真实路径） | 本稿扩展 |
|-------------|------------------|----------|
| 混合检索 | `memoria-core/src/search/hybrid.rs`：`hybrid_search`；MCP `memory_search` / `memory_search_v2`（`mcp_server.rs`） | 默认加 `superseded_by IS NULL`；参数 `include_superseded` / 显式 `as_of`；**graph_expand 之后**统一再滤 |
| 时序真值 | `memories.valid_from/valid_to`；`migrate_temporal`（`storage/sqlite.rs`）；检索后过滤 `valid_at`（闭区间） | TTL 写入快捷参数；**supersede 事务内必写 `valid_to=now`**（方案 A） |
| 近义 supersede | `tools/remember.rs`：只写 `superseded_by` + tier=cold，**不写 `valid_to`**；边 `same_entity` | P0：补 stamp `valid_to`；显式 `supersedes_id`；P1：边改 `updates` |
| 去重链 | MCP `memory_dedup_chain` | 保留；Profile/context 不展开全链 |
| 用户偏好 | `tools/prefs.rs` + MCP `memory_user_prefs`（category=`preference`，tags∈hard_rule\|pref\|style） | 作为 Profile.`static` 主源 |
| 近期决策 | MCP `memory_recent_decisions` → **`decisions` 表**（非 memories） | Profile.`dynamic` **主源改为 memories**（§5.2）；该 MCP/表作遗留旁路 |
| 巩固 | Memoria：`dream_state_*` / `memory_fetch_unconsolidated`；**LLM 在** `agent-core/src/agent.rs::consolidate` | Dream 写出 pattern 时挂 **derives** 边（P1） |
| 衰减 | `tools/decay.rs` + MCP `memory_decay` | 与 TTL 正交：decay=权重；TTL=有效性 |
| 实体边 | `tools/graph.rs`：`RELATION_TYPES`（uses/depends_on/…）；表 `entity_edges` | **实体边保持现有枚举**；本稿三类边优先落在 **记忆关系** `memory_relations` |
| 记忆关系 | `memory_relations` CHECK：`same_entity\|chronological\|semantic_related` | 迁移扩展：`updates\|extends\|derives`（及保留旧值）；**一律 snake_case** |
| 会话注入 | agent-core `rephrase_and_confirm` / `search_memory` 临时拼 knowledge | 改为优先调 `memory_context` / `memory_profile` |
| 身份 / NS | `auth.rs`、`get_allowed_ns`、三级 NS | Profile/context **必须** `check_ns_access` |
| 配额 | `quota.rs`：write / search / backup；admin 豁免已有 | profile/context 走独立 `profile_bucket`（§14.1 Q3） |
| 导出 | `tools/imp_exp.rs`：`memories` 列清单**缺** `superseded_by` | **必须**补列（M5） |

**Supermemory → Memoria 映射（借语义不借整包）**

| Supermemory | Memoria |
|-------------|---------|
| `profile.static` / `profile.dynamic` | `memory_profile` → `static` / `dynamic` |
| Updates + isLatest | `memory_relations.updates`（P1）+ `superseded_by`（P0）+ 默认检索过滤 |
| MCP memory/recall/context | 本设计 §8 三件套（可别名到现有工具） |
| Hybrid = 记忆+文档 | **不在 Memoria 内做文档 RAG**；`mode=memories` 为语义默认 |
| Auto forget（日期） | `valid_to` / TTL + as_of |
| containerTag | 已有 namespace（个人/项目容器） |
| 连接器 / 多模态核心 | **明确不学** |

---

## 3. 现状能力快照（对照真实代码）

### 3.1 存储（`storage/sqlite.rs`）

- **`memories`**：内容、category、importance、tier、decay_factor、tags、**valid_from/valid_to**、迁移列 **superseded_by**。**无 `kind` 列、无 `superseded_at` 列**。
- **`memory_relations`**：记忆间边（当前 CHECK 三值）；含 valid_*。
- **`entities` / `entity_mentions` / `entity_edges`**：实体图谱（受控 `RELATION_TYPES`）。
- **`dream_state`**：phase ∈ consolidate | entity_extract | decay | graph；cursor 幂等。
- **`user_prefs`**：遗留表；新偏好走 `memories`（prefs.rs 注释已写明）。
- **`decisions`**：遗留决策表；`memory_recent_decisions` 读此表，**与 memories 无自动同步**。

### 3.2 检索（`search/hybrid.rs` + `search/keyword.rs`）

5 信号 RRF：keyword / semantic / temporal / importance / category；可选 2-hop `graph_expand`；**as_of 在 expand+dedup 之后后置过滤**。  
**keyword 现网行为**（`keyword.rs` 实现，非文件头注释）：只查 **`memories_fts`**，无结果时 **`memories.content LIKE`**；**不搜** `messages` / `decisions`。  
**缺口**：未按 `superseded_by IS NULL` 过滤；supersede 路径未 stamp `valid_to` → **无法用 as_of 还原「T 时刻 tip」**（故方案 A）；graph_expand 邻居也未单独做 isLatest（依赖最终统一过滤，见 §6.4 / M1）。

### 3.3 写入（`tools/remember.rs`）

- 精确去重 / 近义 cosine>0.92 → 写 `superseded_by` + `tier='cold'`，边类型现为 `same_entity`；**不写 `valid_to`**。
- 时间戳格式：`chrono` 输出 `%Y-%m-%dT%H:%M:%S`（含 `T`）；部分 SQLite `DEFAULT (datetime('now'))` 为空格分隔 → 需清洗归一（Q1）。
- 支持 `valid_from` / `valid_to` 入参。

### 3.4 Dream 边界（已裁决，与 ROADMAP 一致）

- Memoria：哑工具（fetch_unconsolidated、dream_state、remember 落库）。
- agent-core：`consolidate(ns)` 调 LLM 提炼 pattern + NER；夜间巡检 / `POST /api/admin/consolidate`。

### 3.5 agent-core 注入现状

- 复述阶段 `search_memory` 取 top3 拼 system prompt（`agent.rs`）。
- **无** Profile 稳定块；偏好需另调 `memory_user_prefs`（多数路径未统一）。

---

## 4. 架构与数据流

```
┌─────────────┐     HTTP :9753      ┌──────────────────┐
│ Jan / PFAiX │ ──────────────────► │    agent-core    │  ← 脑子：LLM / 红线 / consolidate
│   (壳)      │   禁止直连 :9003    │                  │
└─────────────┘                     │  会话开场:       │
                                    │  memory_context  │
                                    │  / memory_profile│
                                    └────────┬─────────┘
                                             │ MCP JSON-RPC
                                             │ X-Agent-Id / X-Agent-Key
                                             ▼
                                    ┌──────────────────┐
                                    │ Memoria :9003    │  ← 薄存储：NS / 鉴权 / 检索 / 图 / 游标
                                    │ memoria-server   │
                                    └────────┬─────────┘
                                             │
                          ┌──────────────────┼──────────────────┐
                          ▼                  ▼                  ▼
                     SQLite memories    HNSW / FTS5      dream_state
                     + relations        hybrid_search    (cursor only)
```

### 4.1 会话开场（P0 目标流）

1. 壳 → agent-core chat / 复述入口。  
2. agent-core 对当前 `allowed_ns[0]`（或会话绑定 ns）调用 **`memory_context`**（内部 = profile + 可选轻量 recall）。  
3. 将返回的 markdown/JSON 块写入 system prompt「## 记忆档案」。  
4. 对话中按需 **`memory_recall`**（默认 isLatest）；写入走 **`memory`** / 现有 `memory_remember`（Updates 规则见 §6）。

### 4.2 矛盾更新流

1. agent-core（或工具调用）判定「新事实覆盖旧事实」。  
2. 调用 remember / memory：写入新记忆；**同事务内**：旧记忆 `superseded_by=新id`，且 **必写** `valid_to=now`（方案 A，见 §6.3 / §9）。  
3. P1：插入 `memory_relations(source=旧, target=新, relation_type='updates')`。  
4. 默认检索只见新 tip；`memory_dedup_chain` / `as_of` 可见历史时序真值。

### 4.3 Dream / Derives 流

1. agent-core `consolidate` ← `memory_fetch_unconsolidated`。  
2. LLM 产出 pattern → `memory_remember(category=pattern, tags+=auto_consolidated)`。  
3. P1：对源 observation/memory ids 写 **derives** 边（source=原料，target=pattern）。  
4. `dream_state_update` 推进 cursor（已有）。

---

## 5. Memory Profile 设计

### 5.1 职责

`memory_profile(namespace)`：**只读合成视图**，不新建「第二套偏好存储」。  
合成规则在 Memoria（保证多客户端一致）；**何时注入、如何截断进 prompt** 由 agent-core 决定。

### 5.2 static / dynamic 定义

| 块 | 含义 | 数据源（按优先级） | 建议条数上限 |
|----|------|-------------------|--------------|
| **static** | 稳定身份/硬规则/长期偏好 | ① `category=preference` 且 tag=`hard_rule` → `pref` → `style`（复用 prefs.rs）② 可选 `category=identity` / tags 含 `profile_static` 的 fact | hard_rule 全量；其余 top N（默认 12） |
| **dynamic** | 近期仍有效的动态事实/决策/模式 | **主源：`memories`**，满足：`category IN ('decision','fact','pattern')` **或** tags JSON 含等价标签 `decision`/`fact`/`pattern`；且 **当前 tip**（`superseded_by IS NULL`）且 as_of=now 有效；按 `created_at`/`importance` 取 top N。次选：近 7 日高 importance 且同过滤 | 默认 15；可配置 |

**遗留旁路（不作为 Profile 主源）**：

| 组件 | 现状 | 本稿态度 |
|------|------|----------|
| `decisions` 表 | 独立表；与 memories 无同步 | **保留表结构**，不强制迁移；新决策应写入 `memories(category=decision)` |
| MCP `memory_recent_decisions` | `prefs.rs::recent_decisions` 读 `decisions` | **兼容保留**；Profile / context **不调用**；文档标注 legacy；P2 可改为代理到 memories 查询或标 deprecated |

**排除**：`superseded_by IS NOT NULL`；`valid_to` 使 now 落在区间外；空 content；纯 `observation`（除非 promote）；insight（见 §10.1 / Q5）。

**dynamic 可测伪 SQL（摘要）**：

```sql
SELECT id, content, category, importance, tags, created_at
FROM memories
WHERE namespace = ?
  AND superseded_by IS NULL
  AND (valid_from IS NULL OR valid_from <= ?)   -- ? = as_of，默认 now，ISO 含 T
  AND (valid_to   IS NULL OR valid_to   >= ?)
  AND (
    category IN ('decision', 'fact', 'pattern')
    OR tags LIKE '%"decision"%'
    OR tags LIKE '%"fact"%'
    OR tags LIKE '%"pattern"%'
  )
  AND NOT (                          -- insight 过滤，见 §10.1
    tags LIKE '%"insight"%'
    OR tags LIKE '%"auto_insight"%'
  )
ORDER BY importance DESC, created_at DESC
LIMIT ?;
```

### 5.3 输出形状

见 §10.1。同时提供：

- `static_text` / `dynamic_text`：已排好的 markdown，方便直接拼 prompt。  
- `generated_at`、`namespace`、`is_latest_applied=true`。

### 5.4 性能与缓存（建议）

- P0：无跨请求缓存，SQLite 点查即可。  
- P1：同 `(ns, agent_id)` 短 TTL 缓存（如 30–60s），remember/updates 时失效。  
- 不阻塞在 embedding；Profile **不跑** 全量 hybrid（dynamic 用 SQL 时间/重要性排序为主）。

---

## 6. 图语义：updates / extends / derives

### 6.1 分层

| 图 | 表 | 边语义 | 本稿动作 |
|----|-----|--------|----------|
| **记忆图** | `memory_relations` | 记忆↔记忆的演化/派生 | **P1**：扩展枚举（snake_case） |
| **实体图** | `entity_edges` | 世界实体关系 | 保持现有 `RELATION_TYPES`；**不**用 updates 污染实体层 |

### 6.2 记忆边语义（**严格 P1**）

落库与 API 一律 **snake_case**（禁止 `Updates`/`Extends`/`Derives` 混用）：

| relation_type | 含义 | 写入时机 | 检索影响 |
|---------------|------|----------|----------|
| **updates** | 新记忆取代旧记忆的当前真值 | 显式 supersede；近义去重可升级为 updates（替代仅 `same_entity`） | 旧点当前 tip=false（靠 `superseded_by` + `valid_to`） |
| **extends** | 补充/细化，不否定旧事实 | agent-core 或工具声明 | 新旧均可为 tip |
| **derives** | 由原料巩固/归纳得到 | Dream `consolidate` 写 pattern 时 | 原料与产物均可检索；产物可标 `category=pattern` |

兼容：保留 `same_entity` / `chronological` / `semantic_related` 为遗留值；近义自动边 **新写优先 `updates`**（迁移策略见 §11）。对外 JSON 若收到旧 PascalCase，服务端归一为 snake_case 后落库。

### 6.3 与 `superseded_by` 的关系（方案 A）

- **权威 tip 指针**：`memories.superseded_by`（单列；链可用 `memory_dedup_chain`）。  
- **权威时序戳**：复用 **`valid_to`**，**不新增 `superseded_at`**（现网无该列；加列无额外信息，徒增迁移）。  
- **边**：`updates`（P1）作为可查询、可带 evidence/weight 的一等关系。  
- **写入事务（P0 必做，可测）**：同事务内必须同时：
  1. `UPDATE` 旧行：`superseded_by = 新id`，`tier = 'cold'`，**`valid_to = stamp_to`（§9 优先级）**；  
  2. `INSERT` 新行；  
  3. P1：插 `memory_relations(..., 'updates')`。  
  禁止只改 `superseded_by` 不 stamp `valid_to`（现网 near_dup 路径属此缺口，实现阶段一并修）。

### 6.4 `isLatest` / `as_of` 定义（可测伪代码）

**时间比较语义（全文统一）**：闭区间 `[valid_from, valid_to]`；缺失端点视为无界。ISO-8601 **一律含字面量 `T`**（例：`2026-07-15T13:30:00`）；比较用字典序（与现网 `valid_at` 一致）。

```text
fn valid_at(vf, vt, T) -> bool:
  from_ok = (vf is NULL) OR (vf <= T)
  to_ok   = (vt is NULL) OR (vt >= T)
  return from_ok AND to_ok

# 默认检索 / Profile / context（当前 tip）
fn is_latest_now(row, now) -> bool:
  return row.superseded_by IS NULL
     AND valid_at(row.valid_from, row.valid_to, now)

# as_of=T：时序真值（还原 T 时刻仍有效的记忆，含当时 tip 与已被后来 supersede 但仍在窗口内的行）
fn visible_as_of(row, T) -> bool:
  return valid_at(row.valid_from, row.valid_to, T)
  # 注意：此处故意不看 superseded_by。
  # 因方案 A 在 supersede 时必写 valid_to，故「T 之后才被取代」的行在 T 仍 valid_at=true。
```

| 模式 | 过滤 | 用途 |
|------|------|------|
| 默认 / `include_superseded=false` 且无历史意图 | `is_latest_now` | 当前 tip |
| `include_superseded=true` | 仅 `valid_at(..., now)`（或不过滤 tip） | 调试/链展开 |
| `as_of=T` | **仅** `visible_as_of`（valid_*） | 还原 T 时刻有效集合 |

**统一过滤落点（含 graph_expand）**：`hybrid_search` 在 **RRF → graph_expand → dedup 之后**，对最终候选统一应用上表过滤（M1）。禁止 expand 邻居绕过 isLatest/valid。

---

## 7. 检索边界：`memories` vs `hybrid`

### 7.1 裁决

| mode | 含义 | Memoria 内？ |
|------|------|--------------|
| **`memories`（语义默认）** | 仅 `memories` 上的人设/事实/模式检索 | ✅ |
| **`hybrid`（Memoria 语义）** | 现有 5 信号 RRF（keyword+semantic+temporal+importance+category）+ 可选 graph_expand | ✅ 且 **这就是今天的 `hybrid_search`** |
| **文档 Hybrid（Supermemory 式 记忆+文件）** | 记忆 ∪ Drive/PDF/知识库 chunk | ❌ **不在 Memoria**；落在 dashboard / 业务 MCP / 壳侧检索 |

命名防混：

- 对外文档写清：Memoria `hybrid` = **多信号记忆融合**，不是「记忆+企业文档」。  
- 若未来 MCP 增加 `mode` 参数：`memories` | `hybrid`（默认 `hybrid` 保持现状行为，但 **一律 isLatest**）；**禁止**出现 `documents` 模式除非另立服务。

### 7.2 keyword 信号（对照现网）

`search/keyword.rs` **实现**（以代码为准；文件头「搜 messages/decisions」为过时注释）：

1. `memories_fts` MATCH（jieba 分词）  
2. 若空：`memories.content LIKE ? ESCAPE '\\'`  

**不搜索** `messages` / `messages_fts` / `decisions` / `decisions_fts`。  
因此：无需再为「排除 messages 污染」开专项开关；`mode=memories` 与现网 keyword 行为已对齐。若未来有人把 messages 加回 keyword，须同步改本文并加回归测试。

---

## 8. MCP 契约：memory / recall / context

原则：**少造平行工具**；优先别名 + 薄封装，permissions 矩阵必须登记（`permissions.rs`；由 **CI/单测** 双向覆盖，见 M4）。

### 8.1 推荐落地形态

| 对外名（UX） | 实现策略 | 行为 |
|--------------|----------|------|
| **`memory_context`** | **新建**（P0） | = `memory_profile` + 可选 `query` 时追加 top-k recall；专为会话开场；计入 `profile_bucket` |
| **`memory_profile`** | **新建**（P0） | 纯 static+dynamic 合成；计入 `profile_bucket` |
| **`memory_recall`** | **别名** → `memory_search_v2`（或薄包装） | 默认 isLatest；透传 `as_of`/`max_results`/`tags`；走 search 配额 |
| **`memory`** | **别名/包装** → `memory_remember` | 增加可选 `supersedes_id` / `relation=updates\|extends`；TTL 快捷参数 |

保留原名：`memory_search`、`memory_search_v2`、`memory_remember`、`memory_user_prefs` 等，避免破坏现有 agent-core 白名单。

### 8.2 agent-core 联调（文档约定，非本稿改码）

- 复述/开场：`memory_context` 优先于手搓 `search_memory` top3。  
- 工具列表：把 `memory_context` / `memory_profile` / `memory_recall` 纳入 memoria 工具白名单（`agent.rs` 中 memoria_tools 列表处扩展）。  
- 壳仍不直连。

### 8.3 Authz

| 工具 | min_role | ns_policy |
|------|----------|-----------|
| memory_profile / memory_context / memory_recall | Agent | NamespaceArg + `check_ns_access` |
| memory（写） | Agent | NamespaceArg；`supersedes_id` 目标必须同 ns（见 §10.4） |

### 8.4 `supersedes_id` 失败模式（与 §10.4 一致）

| HTTP/RPC 语义 | 条件 | 说明 |
|---------------|------|------|
| **404** | 目标 id 不存在 | `supersedes_id` 指向未知记忆 |
| **403** | 跨 namespace / 无 ns 权限 | 目标在别的 ns 或调用方不可见 |
| **409** | 非 tip（`superseded_by IS NOT NULL`） | 禁止 supersede 非链尾；应指向当前 tip |
| **409** | 自指（`supersedes_id ==` 新 id / 同 content 自环） | 禁止 |
| **409** | 并发：事务内发现目标刚被他人 supersede | 重试或返回冲突；以 tip 校验为准 |

---

## 9. TTL 临时事实与 Decay

| 机制 | 作用 | 实现要点 |
|------|------|----------|
| **TTL / valid_to** | 「明天考试」类事实到期后不再为真 | 写入：`ttl_seconds` 或 `expires_at` → 填 `valid_to`；默认检索 as_of=now 自动不可见 |
| **decay** | 长期降权、冷归档 | 保持 `memory_decay`；**不**代替 TTL |
| **supersede stamp** | 被新事实取代时截断有效期 | **必写**（非可选）：见下方优先级 |

### 9.1 共用 `valid_to` 的优先级（supersede vs 业务 TTL）

同一列服务两义，写入规则：

```text
# supersede 事务内计算旧行 valid_to（stamp_to）
now = utc_now_iso_T()   # 含 T，与 remember 一致

if old.valid_to IS NOT NULL AND old.valid_to < now:
  stamp_to = old.valid_to      # 已过期：不回拨、不延长
else:
  stamp_to = now               # 截断未来 TTL；或填补 NULL → 关闭开放区间
```

| 场景 | 结果 |
|------|------|
| 无 TTL（`valid_to=NULL`）+ supersede | `valid_to=now` |
| 业务 TTL 在未来 + supersede | `valid_to=now`（**supersede 优先于未到期 TTL**） |
| 业务 TTL 已过期 + supersede | 保持已过期的 `valid_to` |
| 仅设 TTL、无 supersede | 按写入参数设 `valid_to`，不碰 `superseded_by` |

P1 清扫（可选）：定时把 `valid_to < now` 且冷数据的 tier 标 cold；**不物理删除**（导出/审计需要）。

---

## 10. JSON 契约（草案）

### 10.1 `memory_profile`

**请求**

```json
{
  "namespace": "org/changshu/dept/ops/proj/gufei",
  "static_limit": 12,
  "dynamic_limit": 15,
  "as_of": null
}
```

**响应**

```json
{
  "status": "ok",
  "namespace": "org/changshu/dept/ops/proj/gufei",
  "generated_at": "2026-07-15T13:30:00Z",
  "is_latest_applied": true,
  "static": [
    {
      "id": "mem_…",
      "kind": "hard_rule",
      "content": "称呼用户为老大；默认简体中文",
      "importance": 5,
      "tags": ["hard_rule"],
      "category": "preference"
    }
  ],
  "dynamic": [
    {
      "id": "mem_…",
      "kind": "decision",
      "content": "Memoria 定位为薄存储；脑子在 agent-core",
      "importance": 4,
      "created_at": "2026-07-15T13:00:00Z",
      "category": "decision",
      "tags": ["decision"]
    }
  ],
  "static_text": "## 稳定偏好\n- …\n",
  "dynamic_text": "## 近期动态\n- …\n"
}
```

**`kind` 约定（无库列）**：

- **落库**：只用现有 `category` + `tags`；**禁止** `ALTER` 加 `kind`。  
- **投影**：响应里的 `kind` 为派生字段，优先级示例：  
  - tags 含 `hard_rule`/`pref`/`style` → 对应 kind  
  - 否则 `category`（decision/fact/pattern/preference/…）  
  - tags 含 `insight` / `auto_insight` → 投影 `kind=insight`  
- **insight 过滤 SQL**（Profile dynamic / prompt_block 默认排除）：

```sql
-- 排除条件（与 §5.2 一致）
AND NOT (
  tags LIKE '%"insight"%'
  OR tags LIKE '%"auto_insight"%'
)
-- 显式 recall 不强制排除；调用方可自行加 tags 过滤
```

### 10.2 `memory_context`

**请求**

```json
{
  "namespace": "default",
  "query": "可选：本轮用户首句，用于追加 recall",
  "recall_k": 3,
  "include_profile": true
}
```

**响应**

```json
{
  "status": "ok",
  "namespace": "default",
  "profile": { "static": [], "dynamic": [], "static_text": "…", "dynamic_text": "…" },
  "recall": [
    {
      "memory_id": "…",
      "content": "…",
      "rrf_score": 0.03,
      "source": "hnsw_semantic",
      "is_latest": true
    }
  ],
  "prompt_block": "## 记忆档案\n…\n## 相关回忆\n…"
}
```

### 10.3 `memory_recall`（包装 search_v2）

**请求增量字段**

```json
{
  "query": "用户住在哪",
  "max_results": 5,
  "as_of": null,
  "include_superseded": false,
  "mode": "hybrid"
}
```

默认：`include_superseded=false` → 应用 `is_latest_now`（§6.4）。  
`as_of` 非空 → 应用 `visible_as_of`（仅 valid_*）。

### 10.4 `memory` / remember 增量

```json
{
  "content": "用户现居旧金山",
  "category": "fact",
  "importance": 4,
  "tags": "[\"profile_dynamic\"]",
  "supersedes_id": "old_mem_id_nyc",
  "relation": "updates",
  "ttl_seconds": null,
  "expires_at": null,
  "valid_from": null,
  "valid_to": null
}
```

**成功响应增量**

```json
{
  "status": "remembered",
  "id": "new_mem_id",
  "action": "superseded_explicit",
  "superseded": ["old_mem_id_nyc"],
  "relation": "updates"
}
```

**`supersedes_id` 失败响应（须可测）**

| status | code | 场景 |
|--------|------|------|
| error | 404 | 目标不存在 |
| error | 403 | 跨 ns / 无权限 |
| error | 409 | 非 tip、自指、并发冲突 |

近义自动路径可继续返回现有 `action=superseded_near_dup`；P0 起同样 **必 stamp `valid_to`**；P1 起边类型写 `updates`。

### 10.5 derives（Dream 写出，P1）

```json
{
  "content": "周末入厂量显著低于工作日",
  "category": "pattern",
  "tags": "[\"pattern\",\"auto_consolidated\"]",
  "derived_from": ["obs_id_1", "obs_id_2"],
  "relation": "derives"
}
```

Memoria 对 `derived_from` 批量插 `memory_relations`；agent-core consolidate 负责填 ids。

---

## 11. 迁移与兼容

| 步骤 | 内容 | 风险 |
|------|------|------|
| **M1** | `hybrid_search`：**RRF → graph_expand → dedup 之后**统一加默认 `superseded_by IS NULL` + valid_at(now)；`include_superseded` / `as_of` 按 §6.4 | 旧客户端依赖「搜到历史」→ 用 `include_superseded`；expand 邻居不得漏滤 |
| M2 | `ALTER` 放宽 `memory_relations.relation_type` CHECK，加入 `updates\|extends\|derives`（SQLite 常需重建表） | 需备份；走现有 backup / 测试 |
| M3 | 近义边：新写入用 `updates`；旧 `same_entity` 可读映射为 updates | 双写兼容一期（默认） |
| **M4** | 注册 MCP 工具 + `permissions.rs`；**由 CI/`cargo test`（`permissions.rs` 内双向覆盖单测）强制登记** | **非**启动 hard-fail；缺登记 = 测试红，合入拦截 |
| **M5** | `imp_exp`：`memories` 导出列清单 **必须含 `superseded_by`**（现网缺失）；枚举文档更新；导出不包含 computed `is_latest`/`kind` | 缺列会导致迁移丢 tip 链 |
| M6 | 开源树 `memoria-open` 同步；运维树内部审查文档勿推 | 双树纪律 |
| **M7** | 历史 near_dup 行：可选批处理补 `valid_to`（有 `superseded_by` 且 `valid_to IS NULL` 者，用 `COALESCE(相关新行 created_at, updated 推断)` 回填） | 无回填则 as_of 对旧链仍不准；与方案 A 新写入正交 |

**数据后门**：已有 `superseded_by` 的行无需回填边即可被默认 isLatest 过滤；边可懒回填（P2）。as_of 精确还原依赖 `valid_to` stamp（新写入强制；旧数据见 M7）。

**错误数据 / 格式**：现存 `valid_to='1970-01-01…'` 哨兵、以及 `datetime('now')` 空格格式 vs `T` 格式混用，会导致字典序比较误杀或漏滤 → 见 §14.1 Q1。

---

## 12. 分阶段验收

### 12.1 P0 验收清单

- [ ] `memory_profile`：同 ns 返回非空 static（有 preference 时）+ dynamic（来自 **memories** decision/fact/pattern，非 decisions 表）；跨 ns IDOR 拒绝。  
- [ ] `memory_context`：`prompt_block` 可被 agent-core 单测/手工注入；计入 `profile_bucket`。  
- [ ] 默认 `memory_search_v2`：被 supersede 的旧记忆**不出现**；`include_superseded=true` 出现。  
- [ ] **显式 `supersedes_id`**：旧行 `superseded_by` 已设，且 **`valid_to` 已 stamp**；404/403/409 失败模式覆盖。  
- [ ] `as_of`：仅用 valid_*；与 `tests/as_of.rs` / §6.4 伪代码一致（**不**要求与「当前 tip」同一谓词）。  
- [ ] graph_expand 邻居同样被 isLatest/valid 过滤。  
- [ ] permissions 矩阵单测通过；`cargo test` 相关新增用例绿。  
- [ ] **不**把 `updates|extends|derives` 枚举迁移列为 P0 必过项。

### 12.2 P1 验收清单

- [ ] 文档与 schema 明确：`hybrid` ≠ 文档 RAG。  
- [ ] `ttl_seconds` / `expires_at` → `valid_to`；到期后默认召回不可见；与 supersede stamp 优先级符合 §9.1。  
- [ ] `memory_relations` 支持 `updates|extends|derives`；非法类型拒绝；对外 snake_case。  
- [ ] consolidate 写出 pattern 时带 derives（agent-core 联调一条 E2E）。  
- [ ] 近义去重新边为 updates（或双写一期，文档说明）。  
- [ ] export 含 `superseded_by`（M5）。

### 12.3 P2（提及，不验收强推）

- 插件市场矩阵、MemoryBench 对外 KPI、Supermemory 式连接器、本地「全家桶」安装器功能面抄满。  
- 实体层是否镜像 updates（默认否）。  
- Profile 跨 ns 聚合视图（默认否，防串租户）。  
- `memory_recent_decisions` 正式 deprecated 或代理到 memories。

---

## 13. 风险表

| ID | 风险 | 级别 | 缓解 |
|----|------|------|------|
| R1 | 默认 isLatest 改变检索召回，下游 prompt 变「短」 | P0 | 发布说明；保留 include_superseded；评测集对比 |
| R2 | `valid_to` 脏数据 / 格式混用导致全空或漏滤 | P0 | Q1 清洗+格式归一；监控零结果率 |
| R3 | Profile 把 insight 塞进 dynamic 污染人设 | P1 | tags 过滤；无 kind 列 |
| R4 | SQLite CHECK 迁移失败 | P1 | 备份 + 重建表脚本；先 staging |
| R5 | agent-core 未改仍走旧 search，P0 体感不达标 | P0 | 联调清单强制 context；兼容旧路径 |
| R6 | 配额：context 每会话调用被 search 限额误伤 | P1 | `profile_bucket` + admin 豁免（Q3） |
| R7 | 理念漂移：又往 Memoria 塞抽事实 LLM | P0 | 代码评审对照 §1.2；consolidate 留在 agent-core |
| R8 | 双树不同步导致开源缺工具 | P1 | 脱敏同步（Q8） |
| R9 | 旧数据未 stamp `valid_to`，as_of 还原不准 | P0 | 方案 A 新写入强制；M7 可选回填 |

---

## 14. 开放问题与已决议

### 14.1 已决议（2026-07-15）

| ID | 最终决策 | 关键约束 |
|----|----------|----------|
| **Q1** | 脏值清洗 **+ ISO 格式归一** | 执行前必须 `memory_backup`；哨兵：`valid_to <= '1970-01-02'`（含空格/`T` 两种写法）→ NULL；把 `YYYY-MM-DD HH:MM:SS` **归一为** `YYYY-MM-DDTHH:MM:SS`（补 `T`）；审计日志记行数+样本 id；上线后监控零结果率（R2） |
| **Q2** | **方案 (A)**：as_of 用 valid_* 时序；默认 tip 用 `superseded_by IS NULL` | supersede **事务内必写 `valid_to`**（优先复用列，不加 `superseded_at`）；默认检索 = `is_latest_now`；`as_of=T` = **仅** `visible_as_of`（§6.4）。删除「与 tip 判定同源」的错误表述 |
| **Q3** | `profile_bucket` + admin 豁免 | 新增配额 kind=`profile`：每 ns **≤10 次/分钟**（`memory_profile`/`memory_context`）；超限 429 + retry-after；**admin 角色豁免**（与现网 `quota.rs` admin 豁免一致）；**不**再设「开场必调不受限」旁路（避免配额被架空） |
| **Q4** | keyword 现状已足够 | 现网只搜 memories_fts+LIKE；无需再开「排除 messages」开关；若回归引入 messages，再开 issue |
| **Q5** | insight 默认不进 dynamic | **无库列 `kind`**；落库用 tags（`insight`/`auto_insight`）+ category；响应投影 kind；默认不进 dynamic / prompt_block；可经 `memory_recall` 召回 |
| **Q6** | 近义边双写一期 | 新写 `updates`，旧 `same_entity` 可读映射；一期后可只写 updates |
| **Q7** | static 允许可选 `category=identity` | 非必须；先复用 preference |

### 14.2 仍待拍板

| ID | 问题 | 建议默认 | 需老大拍板 |
|----|------|----------|------------|
| **Q8** | 公开仓是否同步本文全文？ | 脱敏后同步到 `memoria-open/docs/`（去本机绝对路径）；确认无密钥；运维树内部审查文档不推 | **是** |

---

## 15. 建议落地顺序（实现阶段，非本稿）

1. supersede 路径补 `valid_to` stamp（方案 A）+ 检索默认 isLatest + graph_expand 后统一过滤 + 测试。  
2. `memory_profile` + `memory_context` + `profile_bucket` + permissions 单测登记。  
3. remember 显式 `supersedes_id`（含 404/403/409）+ export 补 `superseded_by`。  
4. agent-core 开场改调 context。  
5. P1：TTL 快捷参数、关系枚举 `updates|extends|derives`、derives←consolidate、Q1 清洗脚本。

---

## 16. 参考路径索引

| 资源 | 路径 |
|------|------|
| 本设计 | `memoria/docs/DESIGN_MEMORY_PROFILE_AND_GRAPH.md`（运维树） |
| 公开路线图 | `memoria/docs/ROADMAP.md` / `memoria-open/docs/ROADMAP.md` |
| 优化总案 | `memoria/docs/OPTIMIZATION_PLAN_2026-07-12.md` |
| **agent-core 运维树**（与 memoria 并列） | `<agent-core-root>` |
| 壳-引擎边界 | `<agent-core-root>/docs/SHELL_ENGINE_BOUNDARY.md` |
| agent-core 优化（记忆外置） | `<agent-core-root>/docs/OPTIMIZATION_PLAN_2026-07-11.md` |
| 混合检索 | `memoria-core/src/search/hybrid.rs` |
| keyword（仅 memories） | `memoria-core/src/search/keyword.rs` |
| 偏好 / 遗留 decisions | `memoria-core/src/tools/prefs.rs` |
| 近义 supersede | `memoria-core/src/tools/remember.rs` |
| 导出（须补 superseded_by） | `memoria-core/src/tools/imp_exp.rs` |
| 配额（admin 豁免已有） | `memoria-core/src/quota.rs` |
| 权限矩阵单测 | `memoria-core/src/permissions.rs` |
| 实体边枚举 | `memoria-core/src/tools/graph.rs` |
| 巩固编排 | `<agent-core-root>/src/agent.rs`（`consolidate`） |
| PFAiX 兼容 | `jan/docs/COMPAT_MATRIX.md` |

> 上表含本机并列路径便于运维对照；同步公开仓时按 Q8 脱敏为仓库相对路径。勿写入密钥。

---

## 修订记录

| 日期 | 版本 | 说明 |
|------|------|------|
| 2026-07-15 | Draft | 首稿：Profile / isLatest / MCP 三件套 / TTL / Updates·Extends·Derives；对照现网代码路径 |
| 2026-07-15 | Draft | 老大拍板 Q1/Q2/Q3/Q5/Q8 初版（后经审查发现 Q2/Q3/Q5 与现网矛盾，见下条） |
| 2026-07-15 | **Revised** | 对照审查修订：方案 A（supersede 必写 valid_to）；重写 §6.4 as_of≠tip；dynamic 主源改 memories；keyword 按现网改写；Q3=profile_bucket+admin；insight 无 kind 列；P0/P1 边界对齐；export 必含 superseded_by；graph_expand 后统一过滤；边类型 snake_case；M4=CI 单测；时序闭区间+`T` 归一；开放问题仅留 Q8 |
