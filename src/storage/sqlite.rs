//! SQLite connection pool + schema initialization.
//!
//! Phase 2: Rust-Only mode — Rust now owns the schema.
//! `init_core_tables()` creates all business tables that Python used to create.

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::time::Duration;

pub type SqlitePool = Pool<SqliteConnectionManager>;

/// Create a new SQLite connection pool.
/// Opens the DB at `db_path` with WAL mode + foreign keys + busy timeout.
pub fn create_pool(db_path: &str, pool_size: u32) -> Result<SqlitePool, String> {
    let manager = SqliteConnectionManager::file(db_path);
    Pool::builder()
        .max_size(pool_size)
        .max_lifetime(Some(Duration::from_secs(3600)))
        .connection_timeout(Duration::from_secs(10))
        .build(manager)
        .map_err(|e| format!("failed to create pool: {}", e))
}

/// Initialize PRAGMAs: WAL mode + foreign keys + busy timeout.
pub fn init_schema(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )
    .map_err(|e| format!("pragma: {}", e))?;
    Ok(())
}

/// Create ALL core business tables (replaces Python server.py's init_db).
/// Safe to call on existing DB — uses IF NOT EXISTS throughout.
pub fn init_core_tables(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;

    conn.execute_batch("
        -- Sessions table
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            file_path TEXT UNIQUE,
            model TEXT,
            started_at TEXT,
            message_count INTEGER DEFAULT 0,
            indexed_at TEXT
        );

        -- Messages table
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT REFERENCES sessions(id),
            role TEXT CHECK(role IN ('user','assistant','system','tool')),
            content TEXT,
            tokens INTEGER DEFAULT 0,
            seq INTEGER,
            timestamp TEXT
        );

        -- Messages FTS5 (virtual table)
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            content,
            content='messages',
            content_rowid='id'
        );

        -- Memories table (with namespace)
        CREATE TABLE IF NOT EXISTS memories (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            source TEXT,
            content TEXT,
            category TEXT,
            confidence REAL DEFAULT 0.5,
            recall_count INTEGER DEFAULT 0,
            last_recalled TEXT,
            created_at TEXT,
            promoted_at TEXT,
            tier TEXT DEFAULT 'warm' CHECK(tier IN ('hot','warm','cold')),
            evidence TEXT,
            importance INTEGER DEFAULT 3,
            decay_factor REAL DEFAULT 1.0,
            tags TEXT DEFAULT '[]',
            valid_from TEXT DEFAULT (datetime('now')),
            valid_to TEXT
        );

        -- Memories FTS5 (virtual table)
        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            content, namespace,
            content='memories',
            content_rowid='rowid'
        );

        -- User preferences
        CREATE TABLE IF NOT EXISTS user_prefs (
            key TEXT PRIMARY KEY,
            value TEXT,
            evidence TEXT,
            confidence REAL DEFAULT 0.5,
            updated_at TEXT,
            namespace TEXT NOT NULL DEFAULT 'default'
        );

        -- Decisions table
        CREATE TABLE IF NOT EXISTS decisions (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            topic TEXT,
            decision TEXT,
            rationale TEXT,
            context TEXT,
            session_id TEXT,
            created_at TEXT
        );

        -- Decisions FTS5 (virtual table)
        CREATE VIRTUAL TABLE IF NOT EXISTS decisions_fts USING fts5(
            content,
            content='decisions',
            content_rowid='rowid'
        );

        -- Dream state (decay/consolidation tracker, per (phase, namespace))
        -- phase: 'consolidate' | 'entity_extract' | 'decay' | 'graph'
        -- cursor_ts: 幂等游标，已处理到的最大 memories.created_at
        CREATE TABLE IF NOT EXISTS dream_state (
            phase TEXT NOT NULL,
            namespace TEXT NOT NULL DEFAULT 'default',
            last_run TEXT,
            cursor_ts TEXT,
            runs INTEGER DEFAULT 0,
            items_out INTEGER DEFAULT 0,
            sessions_processed INTEGER DEFAULT 0,
            PRIMARY KEY (phase, namespace)
        );

        -- Memory relations (edges between memories)
        CREATE TABLE IF NOT EXISTS memory_relations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            namespace TEXT NOT NULL DEFAULT 'default',
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            relation_type TEXT NOT NULL CHECK(relation_type IN (
                'same_entity','chronological','semantic_related',
                'updates','extends','derives'
            )),
            weight REAL DEFAULT 0.5,
            evidence TEXT,
            created_at TEXT DEFAULT (datetime('now')),
            valid_from TEXT DEFAULT (datetime('now')),
            valid_to TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_rel_source ON memory_relations(source_id);

        -- P1-3：向量持久表（embedding 权威存储，跨重启可用）。
        -- id = 记忆 id（content SHA-256 前 16 位）；vector 以 DIM×f32 little-endian BLOB 存储（DIM 见 src/vector/hnsw.rs，当前 1024）。
        -- HNSW 索引在启动时从本表重建，避免 QueryCache 进程内丢失导致近义去重弱化。
        CREATE TABLE IF NOT EXISTS memory_vectors (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            vector BLOB NOT NULL,
            updated_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_vec_ns ON memory_vectors(namespace);
        CREATE INDEX IF NOT EXISTS idx_rel_target ON memory_relations(target_id);
        CREATE INDEX IF NOT EXISTS idx_rel_namespace ON memory_relations(namespace);

        -- Decay log
        CREATE TABLE IF NOT EXISTS decay_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id TEXT,
            old_tier TEXT,
            new_tier TEXT,
            old_decay REAL,
            new_decay REAL,
            reason TEXT,
            logged_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_decay_log_time ON decay_log(logged_at DESC);

        -- Entity knowledge graph (B phase)
        CREATE TABLE IF NOT EXISTS entities (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            entity_type TEXT NOT NULL CHECK(entity_type IN ('person','system','tool','concept','org','project','location','event','other')),
            name TEXT NOT NULL,
            aliases TEXT DEFAULT '[]',
            summary TEXT,
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_entity_ns_type ON entities(namespace, entity_type);
        CREATE INDEX IF NOT EXISTS idx_entity_name ON entities(name);

        CREATE TABLE IF NOT EXISTS entity_mentions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id TEXT NOT NULL REFERENCES entities(id),
            memory_id TEXT NOT NULL REFERENCES memories(id),
            context TEXT,
            namespace TEXT NOT NULL DEFAULT 'default',
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_mention_entity ON entity_mentions(entity_id);
        CREATE INDEX IF NOT EXISTS idx_mention_memory ON entity_mentions(memory_id);
        CREATE INDEX IF NOT EXISTS idx_mention_ns ON entity_mentions(namespace);

        CREATE TABLE IF NOT EXISTS entity_edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            namespace TEXT NOT NULL DEFAULT 'default',
            source_entity_id TEXT NOT NULL REFERENCES entities(id),
            target_entity_id TEXT NOT NULL REFERENCES entities(id),
            relation_type TEXT NOT NULL,
            weight REAL DEFAULT 1.0,
            evidence TEXT,
            created_at TEXT DEFAULT (datetime('now')),
            valid_from TEXT DEFAULT (datetime('now')),
            valid_to TEXT,
            UNIQUE(namespace, source_entity_id, target_entity_id, relation_type)
        );
        CREATE INDEX IF NOT EXISTS idx_edge_source ON entity_edges(source_entity_id);
        CREATE INDEX IF NOT EXISTS idx_edge_target ON entity_edges(target_entity_id);
        CREATE INDEX IF NOT EXISTS idx_edge_ns ON entity_edges(namespace);

        -- Performance indexes (P0 fix: 2026-07-03)
        CREATE INDEX IF NOT EXISTS idx_mem_ns ON memories(namespace);
        CREATE INDEX IF NOT EXISTS idx_mem_ns_tier ON memories(namespace, tier);
        CREATE INDEX IF NOT EXISTS idx_mem_ns_created ON memories(namespace, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_msg_session ON messages(session_id);

        -- FTS5 sync triggers for memories
        CREATE TRIGGER IF NOT EXISTS mem_ai AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(rowid, content, namespace)
            VALUES (new.rowid, new.content, new.namespace);
        END;
        CREATE TRIGGER IF NOT EXISTS mem_ad AFTER DELETE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, namespace)
            VALUES ('delete', old.rowid, old.content, old.namespace);
        END;
        CREATE TRIGGER IF NOT EXISTS mem_au AFTER UPDATE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, namespace)
            VALUES ('delete', old.rowid, old.content, old.namespace);
            INSERT INTO memories_fts(rowid, content, namespace)
            VALUES (new.rowid, new.content, new.namespace);
        END;
    ").map_err(|e| format!("create tables: {}", e))?;

    // 折叠增量迁移，确保任何调用方（含集成测试）拿到完整 schema（P0-2 根治：
    // 此前仅 main.rs 显式调迁移，测试 bootstrap 缺 actor 等列导致 eval 测试 panic）。
    // 所有迁移均幂等（列/表存在则跳过），与 main.rs 显式调用重复调用安全。
    migrate_dream_state_ns(pool)?;
    migrate_temporal(pool)?;
    migrate_extract_fields(pool)?;
    migrate_evolution(pool)?;
    migrate_memory_relation_types(pool)?;
    migrate_access_count(pool)?;
    migrate_hype_vectors(pool)?;

    Ok(())
}

/// F1a：为 `memories` 增加 `access_count` 列（召回命中计数）。
/// 与 `recall_count`（写入/去重自增）解耦，避免污染历史指标与 `decay` 冷热判据（依赖 recall_count<3）。
/// 幂等：列已存在则跳过。
pub fn migrate_access_count(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'access_count'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has == 0 {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN access_count INTEGER DEFAULT 0;")
            .map_err(|e| format!("add memories.access_count: {}", e))?;
        println!("[Memoria] Migration: added memories.access_count column");
    }
    Ok(())
}

/// Run WAL checkpoint (PASSIVE mode).
pub fn wal_checkpoint(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .map_err(|e| format!("checkpoint: {}", e))
}

/// 迁移：添加 superseded_by 列到 memories 表（P0: 近义重复检测）
/// SQLite 不支持 ADD COLUMN IF NOT EXISTS，需要先检查
pub fn migrate_superseded_by(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;

    // 检查 superseded_by 列是否已存在
    let has_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'superseded_by'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if has_column == 0 {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN superseded_by TEXT;")
            .map_err(|e| format!("add superseded_by: {}", e))?;
        println!("[Memoria] Migration: added superseded_by column to memories");
    }

    // 列确保存在后再建索引（P0 fix: 2026-07-06 近义重复检测）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_mem_superseded ON memories(superseded_by) WHERE superseded_by IS NOT NULL;",
    )
    .map_err(|e| format!("superseded index: {}", e))?;

    Ok(())
}

/// P0+ 吸收 HMS：为 `memories` 增加 `event_time` 列（事件「发生」时刻），
/// 与 `valid_from`（记忆「被断言/认知」时刻）区分，支撑新旧状态区分 / 相对日期落地。
/// 幂等：列已存在则跳过。event_time 缺省 NULL（召回时以 valid_from 兜底为 occurred）。
pub fn migrate_event_time(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;

    let has_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'event_time'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if has_column == 0 {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN event_time TEXT;")
            .map_err(|e| format!("add event_time: {}", e))?;
        println!("[Memoria] Migration: added event_time column to memories");
    }

    Ok(())
}

/// P0 迁移：为 `user_prefs` 增加 `namespace` 列（跨租户隔离，B3 修复）。
/// 幂等：列已存在则跳过。
pub fn migrate_user_prefs_namespace(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    let has_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('user_prefs') WHERE name = 'namespace'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_column == 0 {
        conn.execute_batch(
            "ALTER TABLE user_prefs ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default';",
        )
        .map_err(|e| format!("add user_prefs.namespace: {}", e))?;
        println!("[Memoria] Migration: added namespace column to user_prefs");
    }
    Ok(())
}

/// 暗知识层 A1 迁移：`dream_state` 由 PK(phase) 升级为复合 PK(phase, namespace)，
/// 并补 cursor_ts / runs / items_out 列（夜间巩固幂等游标与审计）。
/// SQLite 不支持直接改 PK。`dream_state` 历史上从未被任何代码写入（空表），
/// 故检测到旧结构（无 namespace 列）时直接 DROP + 按新结构重建，无数据损失。
/// 幂等：已是新结构（含 namespace 列）则跳过。
pub fn migrate_dream_state_ns(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    let has_ns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('dream_state') WHERE name = 'namespace'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_ns == 0 {
        conn.execute_batch(
            "ALTER TABLE dream_state RENAME TO _dream_state_old;
             CREATE TABLE dream_state (
                 phase TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT 'default',
                 last_run TEXT,
                 cursor_ts TEXT,
                 runs INTEGER DEFAULT 0,
                 items_out INTEGER DEFAULT 0,
                 sessions_processed INTEGER DEFAULT 0,
                 PRIMARY KEY (phase, namespace)
             );
             INSERT INTO dream_state(phase, namespace, last_run, sessions_processed)
                 SELECT phase, 'default', last_run, sessions_processed FROM _dream_state_old;
             DROP TABLE _dream_state_old;",
        )
        .map_err(|e| format!("migrate dream_state: {}", e))?;
        println!(
            "[Memoria] Migration: rebuilt dream_state with (phase, namespace) composite PK (preserved old rows)"
        );
    }
    Ok(())
}

/// P0/P1：扩展 `memory_relations.relation_type` CHECK，加入 `updates|extends|derives`。
/// SQLite 无法 ALTER CHECK，需重建表；幂等：新 CHECK 已含 updates 则跳过。
pub fn migrate_memory_relation_types(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_relations'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    if sql.is_empty() {
        return Ok(());
    }
    if sql.contains("'updates'") {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE memory_relations RENAME TO _memory_relations_old;
         CREATE TABLE memory_relations (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             namespace TEXT NOT NULL DEFAULT 'default',
             source_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             relation_type TEXT NOT NULL CHECK(relation_type IN (
                 'same_entity','chronological','semantic_related',
                 'updates','extends','derives'
             )),
             weight REAL DEFAULT 0.5,
             evidence TEXT,
             created_at TEXT DEFAULT (datetime('now')),
             valid_from TEXT DEFAULT (datetime('now')),
             valid_to TEXT
         );
         INSERT INTO memory_relations(
             id, namespace, source_id, target_id, relation_type, weight, evidence,
             created_at, valid_from, valid_to
         )
         SELECT id, namespace, source_id, target_id, relation_type, weight, evidence,
                created_at, valid_from, valid_to
         FROM _memory_relations_old;
         DROP TABLE _memory_relations_old;
         CREATE INDEX IF NOT EXISTS idx_rel_source ON memory_relations(source_id);
         CREATE INDEX IF NOT EXISTS idx_rel_target ON memory_relations(target_id);
         CREATE INDEX IF NOT EXISTS idx_rel_namespace ON memory_relations(namespace);",
    )
    .map_err(|e| format!("migrate memory_relations types: {}", e))?;
    println!("[Memoria] Migration: memory_relations CHECK extended with updates|extends|derives");
    Ok(())
}

/// P1-5 迁移：为 `memories` / `memory_relations` / `entity_edges` 三表补充
/// `valid_from` / `valid_to` 列（轻量时序真值 / as_of 查询）。
/// 幂等：列已存在则跳过；新库由 CREATE TABLE 直接带列，本迁移仅覆盖旧库。
pub fn migrate_temporal(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    for table in ["memories", "memory_relations", "entity_edges"] {
        for col in ["valid_from", "valid_to"] {
            let has: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = '{}'",
                        table, col
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if has == 0 {
                if col == "valid_from" {
                    conn.execute_batch(&format!(
                        "ALTER TABLE {} ADD COLUMN valid_from TEXT DEFAULT (datetime('now'));",
                        table
                    ))
                    .map_err(|e| format!("add {}.{}: {}", table, col, e))?;
                } else {
                    conn.execute_batch(&format!("ALTER TABLE {} ADD COLUMN valid_to TEXT;", table))
                        .map_err(|e| format!("add {}.{}: {}", table, col, e))?;
                }
                println!("[Memoria] Migration: added {}.{} column", table, col);
            }
        }
    }
    // 为 as_of 过滤建立索引（memories 高频查询）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_mem_valid ON memories(namespace, valid_from);",
    )
    .map_err(|e| format!("temporal index: {}", e))?;
    Ok(())
}

/// PR1（Phase B 前置）：为 `memories` 增加提取压缩元数据列
/// `actor` / `memory_type` / `parent_id` / `raw_ref`（均为可空 TEXT）。
/// 这些列由 agent-core 的写入前门提取（Phase B）填充；Memoria 仅哑存储，
/// 不在 remember_with_dedup 内调 LLM（遵守 H1/H2）。旧行 NULL 视为
/// agent_inferred / declarative，读取时兜底。幂等：列已存在则跳过。
pub fn migrate_extract_fields(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    for col in ["actor", "memory_type", "parent_id", "raw_ref"] {
        let has: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = '{}'",
                    col
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has == 0 {
            conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {} TEXT;", col))
                .map_err(|e| format!("add memories.{}: {}", col, e))?;
            println!("[Memoria] Migration: added memories.{} column", col);
        }
    }
    Ok(())
}

/// PR4（Phase A 演化）：为 `memories` 增加演化写回元数据列
/// `evolved_context` / `evolved_at` / `link_count`（可空），并建 `evolution_log` 表。
/// 演化认知在 agent-core 的 Dream/consolidate（批处理），Memoria 仅哑存储（守 H1/H2）。
/// 回滚依据 `evolution_log.old_value` 恢复旧值，不依赖 DROP 列（H5：可逆）。幂等。
pub fn migrate_evolution(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    // 新增 3 列（evolved_at NULL = 待演化/脏标记；link_count 默认 NULL）
    for (col, ctype) in [
        ("evolved_context", "TEXT"),
        ("evolved_at", "TEXT"),
        ("link_count", "INTEGER"),
    ] {
        let has: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = '{}'",
                    col
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has == 0 {
            conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {} {};", col, ctype))
                .map_err(|e| format!("add memories.{}: {}", col, e))?;
            println!("[Memoria] Migration: added memories.{} column", col);
        }
    }
    // evolution_log：演化变更审计，old_value 可回滚
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS evolution_log (
            id TEXT PRIMARY KEY,
            new_id TEXT,
            target_id TEXT NOT NULL,
            change_type TEXT,
            old_value TEXT,
            new_value TEXT,
            model TEXT,
            created_at TEXT DEFAULT (datetime('now')),
            namespace TEXT NOT NULL DEFAULT 'default'
        )",
    )
    .map_err(|e| format!("create evolution_log: {}", e))?;
    // 存量 evolution_log 表补 namespace 列（幂等，防 P1-③ 跨租户泄露）
    let has_ns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('evolution_log') WHERE name = 'namespace'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_ns == 0 {
        conn.execute_batch(
            "ALTER TABLE evolution_log ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default';",
        )
        .map_err(|e| format!("add evolution_log.namespace: {}", e))?;
    }
    Ok(())
}

/// V1（2026-08-12）：HyPE 假设问句向量表（写入侧增强，双向量检索）。
///
/// HyPE（Hypothetical Prompt Embeddings, IEEE Access 2025）：写入时用 LLM 为该记忆生成
/// 「用户会怎么问」的假设问句，嵌入问句并与内容向量**并列**存储；检索时 query 同时匹配
/// 内容向量与问句向量（question-question 匹配），缩小「问句式 query vs 陈述式记忆」的
/// 措辞 gap。本表存问句向量（id=记忆 id，与 memory_vectors 平行），由 semantic_search
/// 双路合并（取 max）。幂等迁移；HNSW 由 persist::rebuild_hype_hnsw_from_store 重建。
/// 清理（#R35 maintainability/low）：memories 删除时经触发器同步删本表与 memory_vectors
/// 的行——否则孤儿向量行在每次启动 rebuild 时被重新加入 HNSW，永久浪费索引内存/存储
/// （web_api/mcp_server 的 DELETE 只删 memories，不碰向量表）。
pub fn migrate_hype_vectors(pool: &SqlitePool) -> Result<(), String> {
    let mut conn = pool.get().map_err(|e| format!("pool get: {}", e))?;
    // #R43 bug/medium：busy_timeout 必须在**本函数所有 DDL/写操作之前**设置——连接来自
    // 池（create_pool 无 per-connection init），迁移可能拿到默认 busy_timeout=0 的连接，
    // 上面的 CREATE TABLE/INDEX/TRIGGER 与 migration_flags DDL 在并发首启场景下会立即
    // SQLITE_BUSY（本迁移正是为三进程并发首启设计的）。统一函数入口设置（幂等，覆盖
    // 池连接状态）。
    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(|e| format!("set busy_timeout: {}", e))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_hype_vectors (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            question TEXT,
            vector BLOB NOT NULL,
            updated_at TEXT DEFAULT (datetime('now'))
        )",
    )
    .map_err(|e| format!("create memory_hype_vectors: {}", e))?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_hype_ns ON memory_hype_vectors(namespace)",
    )
    .map_err(|e| format!("create idx_hype_ns: {}", e))?;
    // 删除联动：memories 行删除时清理两个向量表。注意：memory_vectors 同样存在孤儿
    // 问题（历史行为），一并覆盖。
    // #R44 maintainability/low：`DROP TRIGGER IF EXISTS` + `CREATE TRIGGER` 而非仅
    // `IF NOT EXISTS`——固定名 + IF NOT EXISTS 会让存量库永远保留**旧 body**：未来
    // 修改联动体（新增第三张向量表/修 bug）对新库生效、对已部署库静默失效（CREATE
    // 被跳过），孤儿预防契约无法演进。DROP+CREATE 每次启动重放（DDL 微秒级，幂等），
    // 保证 body 恒为当前代码定义；并发首启各进程执行同一 body，最终一致。
    conn.execute_batch("DROP TRIGGER IF EXISTS mem_ad_vec")
        .map_err(|e| format!("drop mem_ad_vec trigger: {}", e))?;
    conn.execute_batch(
        "CREATE TRIGGER mem_ad_vec AFTER DELETE ON memories BEGIN
            DELETE FROM memory_vectors WHERE id = old.id;
            DELETE FROM memory_hype_vectors WHERE id = old.id;
        END",
    )
    .map_err(|e| format!("create mem_ad_vec trigger: {}", e))?;
    // 一次性清理**存量**孤儿行（#R36 maintainability/low）：触发器只挡未来，
    // 历史删除留下的 memory_vectors 孤儿（无对应 memories 行）仍会被启动 rebuild
    // 重新加入 HNSW，浪费索引内存/存储。迁移内清理彻底闭环。
    // #R37 bug/medium：两表在此刻**必然存在**（刚 CREATE），query_row 失败即真实 DB
    // 问题（BUSY/schema 异常）——unwrap_or(0) 会静默跳过清理、迁移假成功、孤儿
    // 每启动重入 HNSW。必须传播。
    // #R37 performance/medium：清理用 `PRAGMA user_version` 位标记门控——`COUNT(*) NOT IN`
    // 是全表相关扫描 + 可能的大 DELETE，若每次启动都跑会拖慢启动并持写锁。
    // user_version 已有值只增不减；用位 0x1000 标记"孤儿清理已执行"。
    // #R38 other/low：门控 + 清理 + 置位必须在单个 BEGIN IMMEDIATE 事务内——
    // init_core_tables 可能被 CLI/server/库引擎三进程并发首次启动调用，check-then-act
    // 会让多个进程同时跑全表扫描/DELETE（写锁争用），且 DELETE 后崩溃会留下未置位
    // 标记导致下次重复清理。BEGIN IMMEDIATE 使第二进程阻塞到首个提交后看到已置位。
    // #R38 maintainability/low：库代码（Python bindings 宿主可达）用 eprintln 不污染 stdout。
    // #R39 bug/high：事务体内所有错误路径必须 ROLLBACK——raw execute_batch 开启的事务
    // 池不知情，`?` 提前返回会把"仍持有写锁的事务"还给连接池：后续查询静默跑在事务内、
    // 写锁不释放（其他写者 SQLITE_BUSY）。闭包返回 Result，错误统一 rollback 后传播。
    // #R39 performance/medium：持写锁做全表 COUNT+DELETE 相关扫描可能超 busy_timeout——
    // 空表直接短路（最常见的首次启动场景），减少持锁时间。
    // #R40 maintainability/low：门控用**独立 migration_flags 表**而非 user_version 位复用——
    // health.rs 把 user_version 当纯 schema 版本号写（EXPECTED_SCHEMA_VERSION=2），位
    // 复用会在任何"按版本号写 user_version"的未来路径上被静默清除（重跑昂贵清理）或
    // 与升高的期望版本碰撞。key-value 表语义隔离、无耦合。
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_flags (
            flag TEXT PRIMARY KEY,
            applied_at TEXT DEFAULT (datetime('now'))
        )",
    )
    .map_err(|e| format!("create migration_flags: {}", e))?;
    const CLEANUP_FLAG: &str = "orphan_vector_cleanup_v1";
    let already: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
            rusqlite::params![CLEANUP_FLAG],
            |r| r.get(0),
        )
        .map_err(|e| format!("check cleanup flag: {}", e))?;
    if already == 0 {
        // #R43 bug/medium：改用 rusqlite 的 transaction_with_behavior(TransactionBehavior::
        // Immediate) 替代手动 BEGIN/COMMIT/ROLLBACK——Transaction 在 Drop 时自动回滚，
        // 任何 `?` 提前返回或 panic 都不会把"仍持有写锁的连接"还给池（r2d2 无
        // test_on_check_out，归还后后续查询会静默跑在未提交事务内、写锁不释放，其他写者
        // SQLITE_BUSY）。错误路径免疫未来编辑/panic，与模块其余代码风格一致。
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("begin cleanup tx: {}", e))?;
        // 事务内重读标记（首个提交后此处看到已置位 → 跳过清理）。
        let already2: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
                rusqlite::params![CLEANUP_FLAG],
                |r| r.get(0),
            )
            .map_err(|e| format!("check cleanup flag (tx): {}", e))?;
        if already2 == 0 {
            // 空表短路：向量表无行时无需扫描（避免持写锁的全表相关扫描）。
            let v_count: i64 = tx
                .query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))
                .map_err(|e| format!("count memory_vectors rows: {}", e))?;
            if v_count > 0 {
                let orphans_vec: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM memory_vectors \
                         WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("count memory_vectors orphans: {}", e))?;
                if orphans_vec > 0 {
                    // #R42/#R44 performance/medium：**单条** DELETE（一次相关扫描）——
                    // 分批 LIMIT 1000 在同一个 BEGIN IMMEDIATE 事务内**不降低**持锁时间：
                    // NOT EXISTS 无索引可走，每批都是全表扫描，总工作量 O(批数 × 表大小)
                    // 而非 O(孤儿数)；大库（孤儿恰在此累积）上启动被拖慢，并发首启的
                    // 另一进程可能超 5000ms busy_timeout 而 SQLITE_BUSY 中止启动
                    // （main.rs .expect 直接 panic）。真正释放锁需要每批独立事务，
                    // 复杂度不值——单条 DELETE 一次扫描即最优。
                    // #R43 maintainability/low：SQL 规范化（去掉填充空白）+ 错误消息
                    // 内嵌 SQL 片段，便于真实库上失败时审计定位。
                    const DEL_VEC: &str = "DELETE FROM memory_vectors \
                        WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id)";
                    tx.execute(DEL_VEC, []).map_err(|e| {
                        format!("clean memory_vectors orphans: {e} [SQL: {DEL_VEC}]")
                    })?;
                    eprintln!("[Memoria] Migration: removed {orphans_vec} orphan memory_vectors rows");
                }
            }
            let h_count: i64 = tx
                .query_row("SELECT COUNT(*) FROM memory_hype_vectors", [], |r| r.get(0))
                .map_err(|e| format!("count memory_hype_vectors rows: {}", e))?;
            if h_count > 0 {
                let orphans_hype: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM memory_hype_vectors \
                         WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("count memory_hype_vectors orphans: {}", e))?;
                if orphans_hype > 0 {
                    // #R44 performance/medium：单条 DELETE（同 DEL_VEC 理由，见上）。
                    const DEL_HYPE: &str = "DELETE FROM memory_hype_vectors \
                        WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id)";
                    tx.execute(DEL_HYPE, []).map_err(|e| {
                        format!("clean memory_hype_vectors orphans: {e} [SQL: {DEL_HYPE}]")
                    })?;
                    eprintln!("[Memoria] Migration: removed {orphans_hype} orphan memory_hype_vectors rows");
                }
            }
            // 置位标记：插入 migration_flags 行（事务内，提交后对并发进程可见）。
            tx.execute(
                "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                rusqlite::params![CLEANUP_FLAG],
            )
            .map_err(|e| format!("set cleanup flag: {}", e))?;
        }
        // #R43 bug/medium：commit 失败时 Transaction 的 Drop 自动回滚——连接不会带着
        // 未提交事务还池（手动 COMMIT 失败还需手动 ROLLBACK 的双失败路径已消除）。
        tx.commit()
            .map_err(|e| format!("commit cleanup tx: {}", e))?;
    }
    Ok(())
}
