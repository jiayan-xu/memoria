# 🧠 Memoria

> Independent memory center for AI agents. Built in Rust. MCP-native. Zero external dependencies.

AI agents shouldn't forget you every time they restart. Memoria is a standalone memory service — conversations, decisions, preferences — unified across all your AI tools.

**Not bound to any software. Serving only you.**

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

## Quick Start

### Build & Run

```bash
git clone https://github.com/memoria-ai/memoria.git
cd memoria
cargo build --release
./target/release/memoria-server
```

Server starts at `http://0.0.0.0:9003`. Web dashboard at `http://0.0.0.0:9003/app`.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORIA_DB_PATH` | `data/memoria.db` | Main database path |
| `MEMORIA_PORT` | `9003` | Server port |
| `MEMORIA_HOST` | `0.0.0.0` | Bind address |
| `MEMORIA_ADMIN_KEY` | Auto-generated | Admin token |
| `MEMORIA_AUTH_DB_PATH` | `<data>/audit.db` | Audit database path |

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
| `register_agent` | Register agent identity (admin key) |
| `agent_list` | List registered agents |
| `agent_revoke` | Revoke agent token |
| `audit_query` | Query audit logs |
| `db_stats` | Database statistics |
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
