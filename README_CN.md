# 🧠 Memoria

> AI Agent 的独立记忆中心。Rust 构建。MCP 原生。零外部依赖。

AI Agent 每次重启就忘了你是谁？Memoria 是一个独立记忆服务——对话、决策、偏好——统一管理，跨所有 AI 工具共享。

**不绑定任何软件。只为你服务。**

**[English](README.md)** | **[演进日志](EVOLUTION.md)**

---

## 为什么需要 Memoria？

每个 AI 产品都有自己的记忆孤岛。从 Claude 切到 DeepSeek？上下文没了。从 ChatGPT 切到本地模型？从头开始。

Memoria 解决这个问题——它是**记忆层**，不是某个 AI 客户端的功能。任何兼容 MCP 的 Agent 都可以接入，共享同一份记忆。

```
Agent (Claude Desktop / Jan / OpenClaw / ...)
    │
    ▼  MCP 协议 (JSON-RPC over HTTP)
┌─────────────────────────────────────┐
│         Memoria (:9003)             │
│  ┌─────────┐ ┌──────┐ ┌──────────┐  │
│  │ SQLite  │ │ FTS5 │ │  HNSW    │  │
│  │(结构化)  │ │(全文) │ │(向量)    │  │
│  └─────────┘ └──────┘ └──────────┘  │
│  ┌─────────────────────────────────┐│
│  │  5 信号混合搜索 (RRF 融合)       ││
│  │  关键词+语义+时间+重要性+分类     ││
│  └─────────────────────────────────┘│
│  ┌─────────────────────────────────┐│
│  │  认证 + 审计 + 命名空间隔离       ││
│  └─────────────────────────────────┘│
└─────────────────────────────────────┘
```

## 功能特性

### 🔍 5 信号混合搜索
- **关键词** — FTS5 全文检索（jieba-rs 中文分词）
- **语义** — HNSW 向量搜索（hnsw_rs）
- **时间** — 时间衰减加权
- **重要性** — 记忆优先级评分（1-5）
- **分类** — 意图分类过滤
- **RRF 融合** — 倒数排名融合，综合 5 个信号

### 🔐 身份认证与审计
- **命名空间隔离** — 多租户数据分离
- **名牌令牌认证** — SHA-256 令牌认证
- **按周分区审计日志** — 自动轮转，90 天保留
- **独立审计数据库** — 与主库无锁竞争

### 🤝 A2A Agent 通信
- Agent 间消息路由
- 审批工作流与任务协调
- 跨 Agent 知识共享

### 🌐 Web 仪表盘
- 搜索、时间线浏览、图谱可视化
- CRUD API：增删改查、导入导出、备份

## 性能基准

| 指标 | Python (原版) | Rust | 提升 |
|------|:-:|:-:|:-:|
| 平均搜索延迟 | 410ms | 112ms | **3.7 倍** |
| P50 搜索延迟 | 182ms | 99ms | **1.8 倍** |
| 零结果率 | 32.2% | 0% | **清零** |

> *测量环境：x86_64 Linux，Rust release 构建，2026-07。Python 列仅为迁移前的基线对照。*

## 测试与 CI

- `cargo test` 在 GitHub Actions 三平台（ubuntu / windows / macos）全绿（`.github/workflows/ci.yml`）。
- 截至 2026-07-13：**41 个集成 + 单元测试**，覆盖核心搜索、配额（P2-2）、实体图谱（P2-3）、导入导出（P2-4）。

## 快速开始

### 编译与运行

```bash
git clone https://github.com/jiayan-xu/memoria.git
cd memoria
cargo build --release
./target/release/memoria-server
```

服务默认仅监听本机回环 `http://127.0.0.1:9003`（安全默认）。
Web 仪表盘：`http://127.0.0.1:9003/app`。
如需暴露到局域网，设置 `MEMORIA_HOST=0.0.0.0`（自担风险）。

### Docker（回环部署）

```bash
cp .env.example .env          # 编辑填入 MEMORIA_ADMIN_KEY
docker compose up -d --build
```

仅本机 `127.0.0.1:9003` 可访问，不暴露到网络。详见 `docker-compose.yml` 与 `docs/ROADMAP.md`。

### 配置与示例

- 所有环境变量见 [`.env.example`](.env.example)（占位符，无真实密钥）。
- MCP 客户端配置样例见 [`examples/`](examples/)：`claude-desktop.json` / `cursor.json` / `python-minimal-client.py`。

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEMORIA_DB_PATH` | `data/memoria.db` | 主数据库路径 |
| `MEMORIA_PORT` | `9003` | 服务端口 |
| `MEMORIA_HOST` | `127.0.0.1` | 绑定地址（默认回环） |
| `MEMORIA_ADMIN_KEY` | 自动生成 | 管理员令牌（生产环境务必显式设置） |
| `MEMORIA_AUTH_DB_PATH` | `<data>/audit.db` | 审计数据库路径 |
| `MEMORIA_BACKUP_DIR` | `data/backups` | GFS 备份目录 |
| `MEMORIA_BACKUP_INTERVAL_HOURS` | `24` | 备份间隔（小时） |
| `MEMORIA_WORKER_THREADS` | `4` | 异步工作线程数 |
| `MEMORIA_MAX_BLOCKING_THREADS` | `512` | 最大阻塞线程数 |
| `MEMORIA_NEAR_DUP_ENABLED` | `true` | 近义去重（P1-3） |
| `MEMORIA_NEAR_DUP_THRESHOLD` | `0.92` | 去重余弦阈值 |
| `MEMORIA_QUOTA_WRITES_PER_DAY` | `1000` | 每命名空间每日写入限额（P2-2） |
| `MEMORIA_QUOTA_SEARCHES_PER_MIN` | `120` | 每命名空间每分钟搜索限额（P2-2） |
| `MEMORIA_QUOTA_BACKUPS_PER_HOUR` | `10` | 每命名空间每小时备份限额（P2-2） |
| `MEMORIA_DREAM_COOLDOWN_DEFAULT` | `300` | Dream 巩固冷却秒数（P1-4） |
| `MEMORIA_DREAM_COOLDOWN_DECAY` | `60` | decay 阶段冷却秒数 |
| `AGENT_CORE_LOG` / `RUST_LOG` | `info` | 日志级别（P2-1 tracing） |

### MCP 客户端配置

将 Memoria 添加到任何兼容 MCP 的客户端：

```json
{
  "mcpServers": {
    "memoria": {
      "url": "http://127.0.0.1:9003/mcp",
      "transport": "http"
    }
  }
}
```

### MCP 工具列表

| 工具 | 说明 |
|------|------|
| `memory_search` | 关键词 + 语义混合搜索 |
| `memory_search_v2` | 5 信号 RRF 融合搜索 |
| `memory_remember` | 存储记忆（SHA-256 去重） |
| `memory_observe` | 存储低优先级观察 |
| `memory_user_prefs` | 查询用户偏好块 |
| `memory_recent_decisions` | 最近决策记录 |
| `memory_export` | 流式导出某命名空间数据（P2-4） |
| `memory_import` | 幂等导入数据到命名空间（P2-4） |
| `memory_migration_manifest` | 跨机迁移包校验和清单（admin，P2-4） |
| `memory_quota_status` | 当前配额用量与上限（P2-2） |
| `memory_backup` / `memory_backup_list` | GFS 备份触发 / 列出 |
| `memory_health` | 完整健康检查报告 |
| `memory_decay` | 运行衰减循环 |
| `memory_graph` | 构建记忆关系图 |
| `memory_dedup_chain` | 查询某条记忆的 superseded 链 |
| `memory_merge` | 合并两条近义记忆（admin） |
| `memory_fetch_unconsolidated` | 拉取未巩固原料供夜间巩固 |
| `dream_state_get` / `dream_state_update` | 巩固游标状态（P1-4） |
| `entity_upsert` / `entity_add_mention` / `entity_add_edge` | 实体图谱写入（P2-3） |
| `entity_search` | 实体搜索（含 mention 上下文，P2-3） |
| `register_agent` / `agent_list` / `agent_revoke` | Agent 注册（需管理员密钥） |
| `register_user` / `login_user` | 本地账号登录 |
| `import_install_memories` | 迁移命名空间（admin） |
| `get_allowed_ns` | 返回调用者授权的命名空间列表 |
| `audit_query` / `db_stats` | 审计日志查询 / 数据库统计 |
| `a2a_send` / `a2a_recv` | A2A 消息收发 |
| `skill_market_*` | 技能市场（5 个工具） |

## 技术栈

| 组件 | 技术 |
|------|------|
| 语言 | Rust（2021 edition） |
| Web 框架 | axum + tower-http |
| 结构化存储 | SQLite + r2d2 连接池 |
| 全文搜索 | FTS5 + jieba-rs |
| 向量搜索 | hnsw_rs（HNSW） |
| 混合排序 | RRF 5 信号融合 |
| 协议 | MCP（JSON-RPC over HTTP） |
| 二进制大小 | ~8 MB（release, stripped） |

## 系统要求

- **操作系统**: Windows 10+ / Linux / macOS
- **内存**: 空闲 ≥ 64 MB，负载 ≥ 256 MB
- **磁盘**: ≥ 100 MB（不含数据库）
- **Rust 工具链**: 仅编译时需要

## 项目结构

```
memoria/
├── src/
│   ├── main.rs              # 二进制入口
│   ├── lib.rs               # 库（可选 PyO3 绑定）
│   ├── mcp_server.rs        # MCP JSON-RPC 处理
│   ├── auth.rs              # 身份认证 + 审计 + 按周分区
│   ├── web_api.rs           # HTTP API + 静态文件服务
│   ├── session_watcher.rs   # 会话生命周期跟踪
│   ├── search/
│   │   ├── rrf.rs           # 5 信号 RRF 融合 + 图扩展
│   │   ├── keyword.rs       # FTS5 关键词搜索
│   │   ├── semantic.rs      # HNSW 语义搜索
│   │   ├── temporal.rs      # 时间衰减
│   │   ├── importance.rs    # 重要性评分
│   │   └── hybrid.rs        # 搜索编排
│   ├── storage/
│   │   ├── sqlite.rs        # 连接池 + Schema 初始化
│   │   ├── fts5.rs          # jieba-rs 分词器
│   │   └── models.rs        # 数据模型
│   ├── vector/
│   │   ├── hnsw.rs          # HNSW 索引封装
│   │   └── embedding.rs     # Embedding 客户端 + LRU 缓存
│   └── tools/
│       ├── remember.rs      # 记忆存储
│       ├── observe.rs       # 观察存储
│       ├── prefs.rs         # 用户偏好
│       ├── decay.rs         # 记忆衰减
│       └── graph.rs         # 关系图谱
├── web/                     # Web 仪表盘（静态 HTML/CSS/JS）
├── Cargo.toml
├── Cargo.lock
└── README.md
```

## 参考文献

### 论文
- **MAGMA** (ACL 2026) — 多图记忆架构，RRF 融合
- **Reciprocal Rank Fusion** (Cormack et al., SIGIR 2009) — 排序融合
- **HNSW** (Malkov & Yashunin, 2016) — 近似最近邻搜索

### 项目
- [hnsw-rs](https://github.com/marco-apostoli/hnsw-rs) — Rust HNSW 实现
- [jieba-rs](https://github.com/messense/jieba-rs) — 中文分词
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite 绑定
- [axum](https://github.com/tokio-rs/axum) — Rust Web 框架

### 同类项目对比
| 系统 | 与 Memoria 的区别 |
|------|------------------|
| Mem0 | 内存层，需外部向量数据库；Memoria 内置 HNSW + SQLite |
| MemGPT | 管理 LLM 窗口的虚拟上下文；Memoria 专注持久化记忆 |
| LangChain Memory | 框架锁定；Memoria 是协议级独立服务 |

## 许可证

MIT
