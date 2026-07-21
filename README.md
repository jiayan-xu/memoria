# 🧠 Memoria

> Independent memory center for AI agents. Built in Rust. MCP-native. Zero external dependencies.

AI agents shouldn't forget you every time they restart. Memoria is a standalone memory service — conversations, decisions, preferences — unified across all your AI tools.

**Not bound to any software. Serving only you.**

**[中文文档](README_CN.md)** | **[Evolution Log](EVOLUTION.md)**

---

## Why Memoria?

Every AI product has its own memory silo. Switch from Claude to DeepSeek? Your context is gone. Switch from ChatGPT to a local model? Start from scratch.

Memoria fixes this by being **the memory layer**, not a feature of any particular AI client. Any MCP-compatible agent can plug in and share the same memory.

```
Agent (Claude Desktop / Jan / OpenClaw / ...)
    │
    ▼  MCP Protocol (JSON-RPC over HTTP)
┌─────────────────────────────────────┐
│         Memoria (:9003)             │
│  ┌─────────┐ ┌──────┐ ┌──────────┐  │
│  │ SQLite  │ │ FTS5 │ │  HNSW    │  │
│  │(structured)│(full-text)│(vector)│  │
│  └─────────┘ └──────┘ └──────────┘  │
│  ┌─────────────────────────────────┐│
│  │  5-Signal Hybrid Search (RRF)   ││
│  │  Keyword+Semantic+Temporal      ││
│  │  +Importance+Category           ││
│  └─────────────────────────────────┘│
│  ┌─────────────────────────────────┐│
│  │  Auth + Audit + Namespace       ││
│  └─────────────────────────────────┘│
└─────────────────────────────────────┘
```

## Features

### 🔍 5-Signal Hybrid Search
- **Keyword** — FTS5 full-text (jieba-rs Chinese tokenization)
- **Semantic** — HNSW vector search (hnsw_rs)
- **Temporal** — Time decay weighting
- **Importance** — Memory priority scoring (1-5)
- **Category** — Intent classification filter
- **RRF Fusion** — Reciprocal Rank Fusion across all 5 signals

### 🧠 Semantic Search (optional)
- The **Semantic** signal (HNSW vector search) is **off by default**. When `MEMORIA_EMBEDDING_URL` is empty, Memoria silently degrades to keyword-only fusion (FTS5 + temporal + importance + category) — see runtime `/health` (`embed` → `warn: 语义检索降级为 FTS/时间信号`).
- **Capability gap:** with embeddings off you lose semantic / paraphrase recall — queries that don't share keywords with stored memories may return nothing. With embeddings on, Memoria gains true semantic recall across rephrasings and synonyms. (For a concrete example, try querying a stored memory with different wording before vs after enabling embeddings.)
- **Wiring:** start the bundled embed server and point Memoria at it:
  ```bash
  python embed_server.py                       # listens 127.0.0.1:8777/embed
  # then set in .env:
  MEMORIA_EMBEDDING_URL=http://127.0.0.1:8777/embed
  ```
  The embed server (sentence_transformers, offline CPU, model `shibing624/text2vec-base-chinese`) is documented in `embed_server.py`. It is loopback-only and optional; Memoria runs fully without it.

### 🔐 Identity & Audit
- **Namespace isolation** — Multi-tenant data separation
- **Badge token auth** — SHA-256 token-based authentication
- **Weekly partitioned audit logs** — Auto-rotating, 90-day retention
- **Independent audit DB** — No lock contention with main DB

### 🤝 A2A Agent Communication
- Agent-to-Agent message routing
- Approval workflows & task coordination
- Cross-agent knowledge sharing

### 🌐 Web Dashboard
- Search, timeline browse, graph visualization
- CRUD API: create, read, update, delete, import, export, backup

## Performance

| Metric | Python (original) | Rust | Improvement |
|--------|:-:|:-:|:-:|
| Avg search latency | 410ms | 112ms | **3.7x** |
| P50 search latency | 182ms | 99ms | **1.8x** |
| Zero-result rate | 32.2% | 0% | **—** |

> *Measured on x86_64 Linux, Rust release build, 2026-07. The Python column is the
> pre-Rust baseline for relative comparison only.*

## Testing & CI

- `cargo test` passes on all platforms (ubuntu / windows / macos) via GitHub Actions (`.github/workflows/ci.yml`).
- As of 2026-07-13: **41 integration + unit tests** covering core search, quota (P2-2), entity graph (P2-3), and import/export (P2-4).

## Quick Start

### Build & Run

```bash
git clone https://github.com/jiayan-xu/memoria.git
cd memoria
cargo build --release
./target/release/memoria-server
```

服务默认仅监听本机回环 `http://127.0.0.1:9003`（安全默认）。
Web 仪表盘：`http://127.0.0.1:9003/app`。
如需暴露到局域网，设置 `MEMORIA_HOST=0.0.0.0`（自担风险）。

### Docker (loopback)

```bash
cp .env.example .env          # 编辑填入 MEMORIA_ADMIN_KEY
docker compose up -d --build
```

仅本机 `127.0.0.1:9003` 可访问，不暴露到网络。详见 `docker-compose.yml` 与 `docs/ROADMAP.md`。

### Config & Examples

- 所有环境变量见 [`.env.example`](.env.example)（占位符，无真实密钥）。
- MCP 客户端配置样例见 [`examples/`](examples/)：`claude-desktop.json` / `cursor.json` / `python-minimal-client.py`。

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_DB_PATH` | `data/memoria.db` | Main database path |
| `MEMORIA_PORT` | `9003` | Server port |
| `MEMORIA_HOST` | `127.0.0.1` | Bind address (loopback by default) |
| `MEMORIA_ADMIN_KEY` | (required) | Admin token; refuse to start if unset/empty |
| `MEMORIA_AUTH_DB_PATH` | `<data>/audit.db` | Audit database path |
| `MEMORIA_BACKUP_DIR` | `data/backups` | GFS backup directory |
| `MEMORIA_BACKUP_INTERVAL_HOURS` | `24` | Backup interval |
| `MEMORIA_WORKER_THREADS` | `4` | Async worker threads |
| `MEMORIA_MAX_BLOCKING_THREADS` | `512` | Max blocking threads |
| `MEMORIA_NEAR_DUP_ENABLED` | `true` | Near-duplicate dedup (P1-3) |
| `MEMORIA_NEAR_DUP_THRESHOLD` | `0.92` | Dedup cosine threshold |
| `MEMORIA_QUOTA_WRITES_PER_DAY` | `1000` | Write quota per ns/day (P2-2) |
| `MEMORIA_QUOTA_SEARCHES_PER_MIN` | `120` | Search quota per ns/min (P2-2) |
| `MEMORIA_QUOTA_BACKUPS_PER_HOUR` | `10` | Backup quota per ns/hour (P2-2) |
| `MEMORIA_DREAM_COOLDOWN_DEFAULT` | `300` | Dream cooldown seconds (P1-4) |
| `MEMORIA_DREAM_COOLDOWN_DECAY` | `60` | Decay-phase cooldown seconds |
| `AGENT_CORE_LOG` / `RUST_LOG` | `info` | Log level (P2-1 tracing) |
| `MEMORIA_EMBEDDING_URL` | (empty) | Embed server URL; if empty, semantic search degrades to FTS-only (optional). See "Semantic Search" above. |

### MCP Client Configuration

Add Memoria to any MCP-compatible client:

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

### MCP Tools

| Tool | Description |
|------|-------------|
| `memory_search` | Keyword + semantic hybrid search |
| `memory_search_v2` | 5-signal RRF fusion search |
| `memory_remember` | Store memory (SHA-256 dedup) |
| `memory_observe` | Store low-priority observation |
| `memory_user_prefs` | Query user preference block |
| `memory_recent_decisions` | Recent decision records |
| `memory_export` | Streamed JSONL export of a namespace (P2-4) |
| `memory_import` | Idempotent import into a namespace (P2-4) |
| `memory_migration_manifest` | Cross-machine migration checksum manifest (admin, P2-4) |
| `memory_quota_status` | Current quota usage & limits (P2-2) |
| `memory_backup` / `memory_backup_list` | GFS backup trigger / list |
| `memory_health` | Full health check report |
| `memory_decay` | Run decay loop |
| `memory_graph` | Build memory relation graph |
| `memory_dedup_chain` | Query superseded chain of a memory |
| `memory_merge` | Merge two near-duplicate memories (admin) |
| `memory_fetch_unconsolidated` | Fetch raw observations for nightly consolidation |
| `dream_state_get` / `dream_state_update` | Consolidation cursor state (P1-4) |
| `entity_upsert` / `entity_add_mention` / `entity_add_edge` | Entity graph write (P2-3) |
| `entity_search` | Entity search (incl. mention context, P2-3) |
| `register_agent` / `agent_list` / `agent_revoke` | Agent registry (admin key) |
| `register_user` / `login_user` | Local account login |
| `import_install_memories` | Migrate a namespace (admin) |
| `get_allowed_ns` | Return caller's authorized namespaces |
| `audit_query` / `db_stats` | Audit log query / DB stats |
| `a2a_send` / `a2a_recv` | A2A messaging |
| `skill_market_*` | Skill marketplace (5 tools) |

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (2021 edition) |
| Web framework | axum + tower-http |
| Structured storage | SQLite + r2d2 connection pool |
| Full-text search | FTS5 + jieba-rs |
| Vector search | hnsw_rs (HNSW) |
| Hybrid ranking | RRF 5-signal fusion |
| Protocol | MCP (JSON-RPC over HTTP) |
| Binary size | ~8 MB (release, stripped) |

## System Requirements

- **OS**: Windows 10+ / Linux / macOS
- **RAM**: ≥ 64 MB idle, ≥ 256 MB under load
- **Disk**: ≥ 100 MB (excluding database)
- **Rust toolchain**: Only needed for building

## Project Structure

```
memoria/
├── src/
│   ├── main.rs              # Binary entry point
│   ├── lib.rs               # Library (optional PyO3 bindings)
│   ├── mcp_server.rs        # MCP JSON-RPC handler
│   ├── auth.rs              # Identity + audit + weekly partitioning
│   ├── web_api.rs           # HTTP API + static file serving
│   ├── session_watcher.rs   # Session lifecycle tracking
│   ├── search/
│   │   ├── rrf.rs           # 5-signal RRF fusion + graph expansion
│   │   ├── keyword.rs       # FTS5 keyword search
│   │   ├── semantic.rs      # HNSW semantic search
│   │   ├── temporal.rs      # Time decay
│   │   ├── importance.rs    # Importance scoring
│   │   └── hybrid.rs        # Search orchestration
│   ├── storage/
│   │   ├── sqlite.rs        # Connection pool + schema init
│   │   ├── fts5.rs          # jieba-rs tokenizer
│   │   └── models.rs        # Data models
│   ├── vector/
│   │   ├── hnsw.rs          # HNSW index wrapper
│   │   └── embedding.rs     # Embedding client + LRU cache
│   └── tools/
│       ├── remember.rs      # Memory storage
│       ├── observe.rs       # Observation storage
│       ├── prefs.rs         # User preferences
│       ├── decay.rs         # Memory decay
│       └── graph.rs         # Relation graph
├── web/                     # Web dashboard (static HTML/CSS/JS)
├── Cargo.toml
├── Cargo.lock
└── README.md
```

## References

### Papers
- **MAGMA** (ACL 2026) — Multi-graph memory architecture, RRF fusion
- **Reciprocal Rank Fusion** (Cormack et al., SIGIR 2009) — Ranking fusion
- **HNSW** (Malkov & Yashunin, 2016) — Approximate nearest neighbor search

### Projects
- [hnsw-rs](https://github.com/marco-apostoli/hnsw-rs) — Rust HNSW implementation
- [jieba-rs](https://github.com/messense/jieba-rs) — Chinese segmentation
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite bindings
- [axum](https://github.com/tokio-rs/axum) — Rust web framework

### Comparisons
| System | vs Memoria |
|--------|-----------|
| Mem0 | In-memory layer, needs external vector DB; Memoria ships HNSW + SQLite |
| MemGPT | Virtual context management for LLM windows; Memoria focuses on persistent memory |
| LangChain Memory | Framework-locked; Memoria is protocol-level independent service |

## License

MIT
