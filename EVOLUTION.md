# 演进日志 / Evolution Log

---

## 2026-06-24 — Memoria Rust 重构 v4：全线交付

### 背景

经过 5 轮审阅迭代，完成 Memoria 核心从 Python 到 Rust（PyO3）的重构。项目代码共 **24 个文件，1723 行 Rust**，覆盖搜索、写入、衰减、图构建全部管线。

### 架构概览

```
Python server.py (瘦身版)
  └── MEMORIA_BACKEND=rust (默认)
       └── memoria_core.pyd (PyO3)
            ├── storage/  — r2d2 + rusqlite + jieba-rs FTS5
            ├── search/   — 5 信号 RRF 融合 + 2-hop 图扩展
            ├── vector/   — hnsw_rs HNSW + LRU 缓存
            └── tools/    — observe/remember/prefs/decay/graph
```

### 关键数据

| 指标 | Phase 1 (Python) | Phase 4 (Rust) | 提升 |
|------|:-:|:-:|:-:|
| 平均搜索延迟 | 410ms | 112ms | 3.7× |
| P50 搜索延迟 | 182ms | 99ms | 1.8× |
| 零结果率 | 32.2% | 0% | ↓ |
| HNSW 向量数 | 0 | 1174 | — |

### 演进要点

- **PyO3 替代 HTTP 通信**：零序列化开销，单进程共享连接池
- **hnsw_rs 替代 ChromaDB**：HNSW 原生向量索引，省去 HTTP 开销
- **jieba-rs 替代 Python jieba**：FTS5 中文分词，编译期内置
- **SHA-256 去重**：与 Python `hashlib.sha256` 兼容，双写无重复

---

## 2026-06-25 — MCP 独立服务 + 认证系统 + PyO3 可选化

### 变更

#### 独立 MCP 服务
- **`mcp_server.rs`**：独立模块，`build_app()` 返回 axum Router
- **工具**：`memory_search`/`memory_search_v2`/`memory_remember`/`memory_observe`/`register_agent`/`audit_query`/`db_stats`
- **环境变量**：`MEMORIA_DB_PATH`/`MEMORIA_PORT`/`MEMORIA_HOST`/`MEMORIA_ADMIN_KEY`

#### 名牌认证 + NS 隔离 + 审计
- **`agent_registry` 表**：`badge_token`/`namespace`/`permission`/`allowed_skills`/`expires_at`
- **`register_agent`**：自助注册（需 admin key），返回唯一令牌
- **`check_ns_access`**：精确 namespace 匹配
- **`audit_log` 表**：每次 MCP 调用自动记录，`audit_query` 可查

#### PyO3 去依赖
- **可选 feature**：`default = []`，`cargo build --no-default-features` 无 pyo3
- **`#[cfg]` 条件编译**：PyO3 绑定与纯 Rust API 分离
- **二进制**：8.5MB → 7.8MB

### 修复

| 问题 | 级别 | 修复 |
|------|:-:|------|
| Admin 认证死锁 | P0 | admin_key 明文写入 badge_token |
| NS starts_with 越权 | P0 | 改为精确匹配 `==` |
| badge_token 可预测 | P0 | getrandom 随机数 |
| INSERT OR REPLACE 覆盖 | P1 | INSERT OR IGNORE + 查询现有 |
| pyo3 硬依赖 | P1 | 改为可选 feature |

---

## 2026-06-26~29 — A2A 总线 + 技能市场

### A2A 跨 Agent 通信
- `a2a_send` / `a2a_recv` MCP 工具上线
- Agent 间消息路由，跨 namespace 通信
- 配合 A2A Bridge 服务实现异构 Agent 互通

### 技能市场
- `skill_market_search` / `skill_market_install` / `skill_market_publish` / `skill_market_info` / `skill_market_list_installed` 五个 MCP 工具
- SKILL.md 规范发布，兼容 WorkBuddy/QClaw 技能格式
- 按 agent 白名单控制技能安装

---

## 2026-07-01 — Python 全部退役，Rust 全线接管

### 背景

Python 组件（`server.py`、`viz_engine.py`、`capture_proxy.py`）此前仍负责 Web UI、可视化、文件监听。本轮将 Python 全部功能并入 Rust。

### 架构变更

```
旧架构：
  :9003  Rust (MCP)              :9005  Python (Web API + 静态文件)
  /mcp  /health                  /visualize  /stats  /graph  /app/

新架构：
  :9003  Rust (MCP + Web API + 静态文件)
  /mcp  /health  /stats  /graph  /decay_timeline  /api/memories  /api/relations  /app/
```

### 新增文件

| 文件 | 行数 | 职责 |
|------|:-:|------|
| `web_api.rs` | ~260 | `/stats` `/graph` `/decay_timeline` `/api/memories` CRUD `/api/relations` |
| `search/hybrid.rs` | ~68 | 5 信号统一搜索函数，消除重复代码 |

### 修复清单

| 问题 | 级别 | 修复 |
|------|:-:|------|
| Rust 不建业务表 | P0 | `init_core_tables()` 建 11 张核心表 + FTS5 + triggers |
| MCP 搜索缺 semantic + category 信号 | P0 | 补全 5 信号 |
| `memory_relations` 无 namespace 列 | P0 | 加列 + 索引 |
| `decisions` 无 namespace 列 | P0 | 加列 |
| AppState 缺 query_cache | P1 | 加字段 |
| `hybrid_search` 两处重复 | P1 | 提取公共函数 |

### 当前架构

```
memoria-server (:9003)  — Rust 独立二进制
├── MCP 协议（15 个工具）
│   ├── memory_search / memory_search_v2
│   ├── memory_remember / memory_observe
│   ├── register_agent / audit_query / db_stats
│   ├── a2a_send / a2a_recv
│   ├── agent_list / agent_revoke
│   └── skill_market_* (5 个工具)
├── Web API
│   ├── GET /stats           — 记忆统计
│   ├── GET /graph           — 记忆拓扑图
│   ├── GET /decay_timeline  — 衰减时间线
│   ├── GET /api/memories    — 记忆列表 + 过滤查询
│   ├── GET /api/memories/:id — 单条记忆
│   └── GET /api/relations   — 关系列表
├── 静态文件
│   └── /app/ → web/ 目录
└── 会话文件监听
    └── session_watcher (5s 轮询)
```

---

## 2026-07-03 — 独立审计 + 按周分表

### 背景

审计日志与主数据库混用，高频写入影响搜索性能；数据只追加不清理，长期膨胀。

### 变更

#### 审计独立数据库
- 新增 `audit.db`，与 `memoria.db` 物理分离
- `auth_pool` 独立连接池（2 连接），不跟主库争锁

#### 按周分表
- `audit_log` → `audit_log_2026W27`（ISO 周自动分区）
- `audit_log()` 自动创建当周表并写入
- `init_auth_tables()` 启动时创建当周表 + 清理超 90 天分区
- `audit_query` UNION ALL 跨分区查询
- `db_stats` 跨分区统计审计行数

### 审计容量估算

| 场景 | 日均行数 | 年增长 |
|------|:-:|:-:|
| 3 agents | ~1,500 | ~3MB |
| 20 agents | ~20,000 | ~40MB |
| 50 agents | ~100,000 | ~200MB |

---

## 2026-07-03 — P0 索引修复

### 修复
新增 6 个数据库索引，解决搜索性能瓶颈：
- `idx_mem_ns` — memories 按命名空间查询
- `idx_mem_ns_tier` — memories 按命名空间 + 层级查询
- `idx_mem_ns_created` — memories 按命名空间 + 时间排序
- `idx_msg_session` — messages 按会话查询
- `idx_decay_log_time` — 衰减日志按时间查询
- `idx_audit_time` — 审计日志按时间查询

---

## 2026-07-04 — Agent-Core 完整审计 + 12 项修复

### Agent-Core 审计
7 个源文件，20 项问题（3 P0、7 P1、10 P2），复审后 P0 全部清零。

### 12 项修复
- `mcp_client` 超时处理
- `chat_stream` failover
- `tools_cache` 缓存
- `SkillClassifier` 配置驱动
- `canonical_ordered_stringify` 去重
- SQL 注入增强
- system_prompt 可配置
- LRU 会话历史
- 其他 5 项

### 代码健壮性
- 11 处生产代码 `unwrap` 清除
- warning 降至 0

### 并发优化
- `Mutex<Option<AgentCore>>` → `RwLock<Option<Arc<AgentCore>>>`
- 锁持有从 chat 周期缩短至微秒级
- 支持 20-30 并发用户

---

## 2026-07-05 — 开源仓库准备（方案 A）

### 决策
采用方案 A：新建干净仓库，只提取 Rust 源码 + 配置 + Web 前端，零历史包袱。

### 完成
- 40 个文件，8503 行，单次干净 commit
- `cargo check` 零 warning 零 error
- 中英文双版 README
- GitHub Actions CI（三平台矩阵）
- MIT License
- 零个人数据泄露

### Cargo.toml 优化
- edition 从 `2024` 改为 `2021`（稳定版）
- PyO3 改为可选依赖（`default = []`）
- `crate-type` 从 `["cdylib", "rlib"]` 改为 `["rlib"]`

---

## 2026-07-08 — 开源隐私与安全整改

### 背景
开源后发现工作区残留密钥与绝对路径，按隐私红线统一清理。

### 变更
- **密钥轮换**：`MEMORIA_ADMIN_KEY` 改为随机值；agent API key 在提供方注销旧值并换发新 key
- **托盘脚本去硬编码**：`start_tray.ps1` / `start_agentcore_tray.ps1` 改用 `$PSScriptRoot` 与 `$env:AGENT_CORE_DIR`，不再写死 `C:\Users\<user>\...`
- **内部文档移出公开树**：24 份内部审查/设计文档 `git rm --cached`（本地保留），补全 `.gitignore`
- **`.env` 纳入忽略**：密钥一律走环境变量 / `.env`（gitignore），代码只读 `std::env::var(...)`
- **历史扫描**：全量 `git rev-list --all` 扫描，旧明文密钥已在提供方注销（惰性死串），无需改写历史

### 当前状态
| 项 | 状态 |
|------|------|
| 工作区密钥/绝对路径 | ✅ 已清除 |
| admin key 轮换 | ✅ |
| agent API key 换发 | ✅ |
| 公开历史改写 | ⛔ 免做（旧 key 已注销） |

---

## 2026-07-10 — 死锁根治 + 自噬清理 + spawn_blocking 全程隔离

### 背景
恢复生产后发现 memoria-server 占 60–190% CPU，MCP 工具调用（尤其 register_agent）稳定 40s 超时。经 tokio-console 线程栈精确定位，锁定两类根因：① 同一 spawn_blocking 闭包内对 auth_pool 连续两次同步写（dispatch 写 + 尾部 audit_log）在 WAL 串行写下死等；② 所有同步阻塞点（authenticate/audit_log/dispatch/health_check）直接跑在 async worker 上，慢查询即耗尽 worker。

### 修复清单
| 问题 | 级别 | 修复 |
|------|:-:|------|
| register_agent 40s 死锁 | P0 | 闭包尾部 `auth::audit_log` 改为 fire-and-forget `spawn_audit`（与 get_allowed_ns 一致），消除同池连续两次同步写死等 |
| 同步阻塞占满 async worker | P0 | `authenticate`/`audit_log`/`dispatch`/`health_check` 全部 `spawn_blocking` 隔离，WAL 下 SQLite 串行写不再饿死 worker |
| auth_pool 漏 init_schema | P0 | 补 WAL + busy_timeout；连接池 auth_pool 2→16、pool 4→16 |
| observe() 无去重 | P1 | content_hash UNIQUE + INSERT OR IGNORE（与 remember.rs 一致），消除重复 INSERT |
| WATCH_DIRS 自噬 14GB | P0 | 从监视目录移除 agent 自身 sessions（14GB/1164 文件），仅留 reasonix/sessions；Memoria 不再「观察自身输出」 |
| 页面/工具强鉴权卡启动 | P1 | tools/list、initialize 不再强制鉴权 |

### 诊断脚手架
- 可选 `diag` feature（`console-subscriber`，需 `RUSTFLAGS="--cfg tokio_unstable"`）暴露 tokio 任务级栈，`:6669` console 端口；默认关闭，不影响生产构建。
- 结论：当前代码无 busy-loop，空闲 0% CPU，死锁随干净版根治。

### 验证
- `/health` 1.5ms；`tools/list` 3.7ms；register_agent **21ms**（原 40s 超时）；agent-core `/v1/chat/completions` 端到端 **2.9s** 真实回复。

---

## 2026-07-11 — 本地账密登录体系 + 跨租户命名空间隔离（B2/B3）

### 背景
在多用户/多安装实例场景下，需要「个人登录身份」而非仅靠随机 install_id 归属记忆；同时排查发现两处跨租户泄露隐患：① HNSW 语义索引是全局的、无 namespace 维度，`semantic_search` 原实现完全忽略调用者 ns，会把其他租户的记忆一并召回；② `user_prefs` 表缺 namespace 列，偏好数据跨租户共享。此外脱敏逻辑存在一处死循环，是此前 Memoria CPU 飙高的根因之一。

### 变更清单
| 项 | 级别 | 变更 |
|------|:-:|------|
| 本地账密注册/登录 | P0 | `auth.rs` 新增 `register_user`/`login_user`（SHA256(password‖user_id)），登录回传 `badge_token` 供客户端后续鉴权 |
| 新增 MCP 工具 | P0 | `mcp_server.rs` 暴露 `register_user`/`login_user`/`import_install_memories`（记忆命名空间迁移，仅改 namespace 列——id=内容哈希与 ns 无关，无需重建索引） |
| 语义搜索跨租户泄露 | P0 | `search/semantic.rs` 按调用者 ns 回查 `memories` 表过滤 HNSW 结果（单条 IN 查询避免 N+1）；无 pool 时保守返回空。`search/hybrid.rs` 透传 pool |
| user_prefs 跨租户共享 | P0 | `storage/{mod,sqlite}.rs` + `tools/prefs.rs`：`user_prefs` 加 `namespace` 列并做迁移（B3 隔离） |
| 脱敏死循环 | P0 | `auth.rs` 修复脱敏逻辑死循环（此前 CPU 飙高根因之一） |
| runtime 线程限制 | P1 | `main.rs` 显式构建 runtime 限制 worker 线程数，避免线程膨胀 |
| 文件 IO 隔离 | P1 | `session_watcher.rs` 用 `spawn_blocking` 隔离文件读取，不阻塞 async worker |

### 验证
- 账密登录链路：`register_user` → `login_user` 回传 badge → 客户端带 badge 鉴权通过；命名空间默认 `agent/{user_id}`（可选覆盖）。
- 跨租户隔离：语义检索仅返回归属当前 ns 的记忆；`user_prefs` 按 ns 隔离。
- 记忆迁移：`import_install_memories` 换设备/登录后归并旧记忆，仅改 namespace 列，无索引重建。

---

## 2026-07-11 (evening) — 实体三表 + NER MCP 工具 + graph.rs 重写（B1+B2+B3）

### 背景
旧 `graph.rs` 的 `build_graph` 用前缀匹配 + 同日时间线启发式生成假图谱（无真实实体概念），不足以支撑跨会话的实体级知识检索与可视化。设计文档确定实体三表方案：代理端（agent-core）通过 LLM NER 提取实体和关系，存储端（memoria）仅提供哑工具管理实体数据。

### 变更

#### B1. 实体三表 schema（`storage/sqlite.rs` + `storage/models.rs`）
- `CREATE TABLE IF NOT EXISTS` 在 `init_core_tables` 内，自动升级现有 DB：
  - **`entities`**：`id TEXT PK`（缺省自动 UUID）、`namespace`、`entity_type`（CHECK 9 类：person/system/tool/concept/org/project/location/event/other）、`name`、`aliases`、`summary`、`created_at`、`updated_at`
  - **`entity_mentions`**：`id INTEGER AUTO PK`、`entity_id TEXT FK→entities`、`memory_id TEXT FK→memories`、`context`、`namespace`、`created_at`（FK 校验确保引用完整性）
  - **`entity_edges`**：`id INTEGER AUTO PK`、`namespace`、`source_entity_id FK→entities`、`target_entity_id FK→entities`、`relation_type`、`weight REAL`、`evidence`、`created_at`
- 索引：`entities(namespace, entity_type)`、`entity_mentions(entity_id)`、`entity_edges(source_entity_id)`、`entity_edges(target_entity_id)`
- 模型：新增 `Entity` / `EntityMention` / `EntityEdge` 三个 serde struct

#### B2. 4 个实体 MCP 工具（`mcp_server.rs`）
- **`entity_upsert`**：UPSERT 实体（`name`+`namespace` 联合幂等），缺 `entity_id` 自动 UUID，支持 `summary`/`aliases` 更新
- **`entity_add_mention`**：实体→记忆 FK 关联（外键校验，内存_ID 不存在时返回 FK 错误）
- **`entity_add_edge`**：实体间关系，`(source, target, relation_type, namespace)` 唯一幂等，`weight` 保留最大值
- **`entity_search`**：按 `name`/`aliases`/`summary` LIKE 模糊匹配，可选 `entity_type` 过滤，ns 门控已接入

#### B3. graph.rs 重写（`tools/graph.rs`）
- `build_graph`：改从 `entities` 表统计计数 + `entity_edges` 表统计边数
- 新增 `export_graph`：返回 `entities` + `entity_edges` 完整 JSON（`{entities:[...], edges:[...]}`）
- `memory_graph` MCP 工具同步适应新返回格式

### 验证
- tools/list 4 实体工具注册 ✅
- entity_upsert 创建 3 实体（agent-core / memory_search / 暗知识层）✅
- entity_add_mention 关联真实记忆（FK 校验通过）✅
- entity_add_edge 建立 2 关系（calls / builds）✅
- entity_search 按名/别名/类型搜索 ✅
- memory_graph 返回 nodes=3, edges=2 ✅

---

## 2026-07-14 — `/health` 暴露 embed + 本地嵌入通道可观测

### 背景
HNSW 语义检索依赖本机 `embed_server.py`（默认 `:8777`），但公开 `/health` 不反映嵌入是否在线，托盘与 agent-core 无法区分「Memoria 起了但语义降级」。

### 变更
- **`health.rs`**：`check_embedding_endpoint`（软检查）；`run_health_check` 纳入 embedding
- **公开 `/health`**：返回 `embed.{configured,status,message,duration_ms}`；嵌入不可达时顶层 `status=degraded`
- **`/health/full` / `memory_health`**：同步带 embed 摘要
- **同批**：本地嵌入服务 + 写入侧向量（既有提交）；A2A 信封存 `content` JSON

### 验证
- Embed 在线：`GET /health` → `embed.status=pass`（model/dim 可见）
- Embed 停：`degraded` + 明确 message；硬检查仍通过，服务可起

---

*Memoria — Not bound to any software. Serving only you.*
