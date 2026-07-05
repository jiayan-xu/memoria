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

## 快速开始

### 编译与运行

```bash
git clone https://github.com/memoria-ai/memoria.git
cd memoria
cargo build --release
./target/release/memoria-server
```

服务启动在 `http://0.0.0.0:9003`，Web 仪表盘在 `http://0.0.0.0:9003/app`。

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEMORIA_DB_PATH` | `data/memoria.db` | 主数据库路径 |
| `MEMORIA_PORT` | `9003` | 服务端口 |
| `MEMORIA_HOST` | `0.0.0.0` | 绑定地址 |
| `MEMORIA_ADMIN_KEY` | 自动生成 | 管理员令牌 |
| `MEMORIA_AUTH_DB_PATH` | `<data>/audit.db` | 审计数据库路径 |

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
| `register_agent` | 注册 Agent 身份（需管理员密钥） |
| `agent_list` | 列出已注册 Agent |
| `agent_revoke` | 吊销 Agent 令牌 |
| `audit_query` | 查询审计日志 |
| `db_stats` | 数据库统计 |
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
