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

/// #R49 bug/medium：DDL 执行带 **SQLITE_BUSY/SQLITE_LOCKED 退避重试**——并发首启时
/// 另一进程持写锁（如大库孤儿 DELETE）超过 busy_timeout，本进程 DDL 立即 BUSY；
/// 同连接读锁升级写锁失败会报 LOCKED（#R51 bug/low：此前只匹配 DatabaseBusy，
/// LOCKED 会立即终止重试并 .expect 硬失败启动）。main.rs/mcp_server.rs 用 .expect()，
/// 硬失败会让并发首启部署直接 panic 中止（清理分支已软降级，DDL 硬失败是同类
/// 风险）。3 次退避（attempt=1,2,3 → **500ms/1s/1.5s**，合计 3s——#R52
/// documentation/low：`500 * attempt` 的乘积是 500/1000/1500，文档按实际值写），
/// 仍失败才传播。
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(se, _)
            if se.code == rusqlite::ErrorCode::DatabaseBusy
                || se.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// #R53 bug/high：`duplicate column name`（SQLITE_ERROR 19 的扩展码 2001）判定——
/// check-then-act 竞态下对端进程刚提交同一 ALTER 时本进程 ALTER 报此错，按
/// "已应用"幂等处理（不中止启动）。
fn is_duplicate_column(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(se, _) => {
            se.extended_code == 2001 && se.code == rusqlite::ErrorCode::ConstraintViolation
        }
        _ => false,
    }
}

fn execute_batch_retry(
    conn: &rusqlite::Connection,
    sql: &str,
    label: &str,
) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        match conn.execute_batch(sql) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) && attempt < 3 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(500 * attempt as u64));
            }
            Err(e) => return Err(format!("{label}: {}", e)),
        }
    }
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
    // #R43 bug/medium：busy_timeout 必须在**本函数所有 DDL/读-写操作之前**设置——
    // 连接来自池（create_pool 无 per-connection init），迁移可能拿到默认
    // busy_timeout=0 的连接；下方的 pragma_table_info 读与 ALTER/CREATE DDL 在
    // 并发首启场景下会立即 SQLITE_BUSY（#R53 bug/medium：has_upd 读此前跑在
    // busy_timeout=0 上，读失败被 unwrap_or(0) 掩蔽成"列缺失"→ 无端 ALTER →
    // 竞态下 duplicate column name 中止启动）。统一函数入口设置（幂等，覆盖
    // 池连接状态）。
    // #R52 bug/medium：PRAGMA 失败**软处理**（WARN 后继续）——PRAGMA 本身不依赖表
    // 存在、失败仅极端场景（连接已坏），硬失败会让 .expect 中止部署；后续 DDL 仍有
    // execute_batch_retry 兜底 BUSY。
    if let Err(e) = conn.execute_batch("PRAGMA busy_timeout = 5000;") {
        eprintln!("[Memoria] WARN: set busy_timeout failed (continuing): {e}");
    }
    // #R52 bug/low：存量库的 memory_vectors 可能缺 updated_at 列（旧 schema 无此列、
    // 无历史 ALTER 迁移）——补齐后所有写入统一 4 列 upsert（当前 schema 部署的时间戳
    // 不再因 3 列 upsert 陈旧；imp_exp 导出该列，陈旧时间戳会出现在导出数据里）。
    // #R53 bug/high：**check + ALTER 包进 BEGIN IMMEDIATE 事务**——pragma_table_info
    // 读与 ALTER 分开执行存在 check-then-act 竞态：两进程同时见列缺失，第一个提交
    // ALTER 后第二个报 `duplicate column name`（SQLITE_ERROR，is_busy 不重试）→
    // .expect 中止启动。事务内重读（首个提交后看到列已存在 → noop 提交），且
    // 读失败**传播**（事务内 BEGIN 已按 busy_timeout 等待写锁，读失败即真实 DB 问题，
    // 不再 unwrap_or(0) 掩蔽成"列缺失"）。`duplicate column name` 另作"已应用"处理
    // （对端刚提交的窗口，幂等语义）。
    {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("begin updated_at tx: {}", e))?;
        let has_upd: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memory_vectors') WHERE name='updated_at'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| format!("check memory_vectors.updated_at: {}", e))?;
        if has_upd == 0 {
            if let Err(e) = tx.execute_batch(
                "ALTER TABLE memory_vectors ADD COLUMN updated_at TEXT DEFAULT (datetime('now'))",
            ) {
                // #R53 bug/high：`duplicate column name` = 对端进程刚提交了同一 ALTER
                // （并发首启窗口）——按"已应用"处理，不中止启动。
                if is_duplicate_column(&e) {
                    eprintln!("[Memoria] Migration: memory_vectors.updated_at added by peer process");
                } else {
                    return Err(format!("add memory_vectors.updated_at: {}", e));
                }
            } else {
                eprintln!("[Memoria] Migration: added memory_vectors.updated_at column");
            }
        }
        tx.commit()
            .map_err(|e| format!("commit updated_at tx: {}", e))?;
    }
    // #R49 bug/medium：DDL 用 BUSY 退避重试（见 execute_batch_retry）——并发首启时
    // 另一进程持写锁可能让本进程 DDL 立即 SQLITE_BUSY，.expect 硬失败会 panic 中止
    // 部署。
    execute_batch_retry(
        &conn,
        "CREATE TABLE IF NOT EXISTS memory_hype_vectors (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            question TEXT,
            vector BLOB NOT NULL,
            updated_at TEXT DEFAULT (datetime('now'))
        )",
        "create memory_hype_vectors",
    )?;
    execute_batch_retry(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_hype_ns ON memory_hype_vectors(namespace)",
        "create idx_hype_ns",
    )?;
    // migration_flags 表（先建——触发器版本门控与下方孤儿清理共用；key-value 语义
    // 隔离，不复用 health.rs 的 user_version 位（#R40 maintainability/low：位复用会
    // 在"按版本号写 user_version"的未来路径上被静默清除或碰撞）。
    execute_batch_retry(
        &conn,
        "CREATE TABLE IF NOT EXISTS migration_flags (
            flag TEXT PRIMARY KEY,
            applied_at TEXT DEFAULT (datetime('now'))
        )",
        "create migration_flags",
    )?;

    // 删除联动：memories 行删除时清理两个向量表。注意：memory_vectors 同样存在孤儿
    // 问题（历史行为），一并覆盖。
    // #R44 maintainability/low：`DROP TRIGGER IF EXISTS` + `CREATE TRIGGER` 而非仅
    // `IF NOT EXISTS`——固定名 + IF NOT EXISTS 会让存量库永远保留**旧 body**：未来
    // 修改联动体（新增第三张向量表/修 bug）对新库生效、对已部署库静默失效（CREATE
    // 被跳过），孤儿预防契约无法演进。
    // #R45 other/low：DROP+CREATE 包进 BEGIN IMMEDIATE 事务——避免并发进程在
    // DROP 与 CREATE 之间读到"触发器不存在"的中间态（DELETE 瞬间漏清向量表）。
    // busy_timeout 已在函数入口设置，事务竞争按 busy 等待而非失败。
    // #R46 maintainability/medium：重建按 **body 版本**门控（migration_flags 记录
    // 已应用的 body 版本）——无条件每次启动重建会在多进程部署中让每个进程每次启动
    // 都做写锁 DDL（串行化启动），且滚动升级期间新旧版本进程交替启动会互相覆盖
    // 触发器（级联契约随版本振荡，新增向量表的清理在新版下次启动前丢失）。版本名即
    // body 标识：修改 body 时改版本名（v2→v3），存量库首次看到新名才 DROP+CREATE。
    // #R47 maintainability/low 已知残余限制：版本 flag 只防**同版本**重跑，不防
    // **版本回退**——旧二进制只查自己的 v2 flag：新二进制已装 v3 body 的库上，旧
    // 进程看 v2 未置位会 DROP+CREATE 旧 body 覆盖 v3，契约回退到下次新版本重启。
    // 仅在未来出现第二个 body 版本时才实质化；届时可加降级守卫（对比已安装的最大
    // 触发器版本）或接受"回退 = 契约回退到旧版"的语义（与二进制回退一致）。
    const TRIGGER_VERSION: &str = "trigger_mem_ad_vec_v2";
    // #R52 bug/medium：trig_done 读取**软处理**（WARN + 视为未置位）——并发首启时
    // 对端进程提交中可能让本进程此读 SQLITE_BUSY（busy_timeout 后）；硬失败会让
    // .expect 中止部署（与触发器重建本身软降级、flag_set 软读取的意图一致）。
    // 读失败=0 只会让触发器重建多跑一次（幂等 + 事务 + 版本 flag 幂等），安全。
    let trig_done = conn
        .query_row(
            "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
            rusqlite::params![TRIGGER_VERSION],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or_else(|e| {
            eprintln!("[Memoria] WARN: check trigger version flag failed (treated as unset): {e}");
            false
        });
    if !trig_done {
        // #R49 bug/medium：触发器重建**软降级**——BEGIN IMMEDIATE 在并发首启大库
        // 清理持锁时可能 BUSY（busy_timeout 超时）；触发器缺失只影响未来 memories
        // 删除的联动清理（孤儿可下次启动补，或孤儿清理段兜底），不阻断启动。
        // 事务内所有语句失败时 Transaction Drop 自动回滚（#R43）。
        let trig = (|| -> Result<(), String> {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| format!("begin trigger tx: {}", e))?;
            tx.execute_batch("DROP TRIGGER IF EXISTS mem_ad_vec")
                .map_err(|e| format!("drop mem_ad_vec trigger: {}", e))?;
            tx.execute_batch(
                "CREATE TRIGGER mem_ad_vec AFTER DELETE ON memories BEGIN
                    DELETE FROM memory_vectors WHERE id = old.id;
                    DELETE FROM memory_hype_vectors WHERE id = old.id;
                END",
            )
            .map_err(|e| format!("create mem_ad_vec trigger: {}", e))?;
            tx.execute(
                "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                rusqlite::params![TRIGGER_VERSION],
            )
            .map_err(|e| format!("set trigger version flag: {}", e))?;
            tx.commit()
                .map_err(|e| format!("commit trigger tx: {}", e))?;
            Ok(())
        })();
        if let Err(e) = trig {
            eprintln!("[Memoria] WARN: trigger (re)creation skipped (will retry next start): {e}");
        }
    }
    // 一次性清理**存量**孤儿行（#R36 maintainability/low）：触发器只挡未来，
    // 历史删除留下的 memory_vectors 孤儿（无对应 memories 行）仍会被启动 rebuild
    // 重新加入 HNSW，浪费索引内存/存储。迁移内清理彻底闭环。
    // #R37 bug/medium：两表在此刻**必然存在**（刚 CREATE），query_row 失败即真实 DB
    // 问题（BUSY/schema 异常）——unwrap_or(0) 会静默跳过清理、迁移假成功、孤儿
    // 每启动重入 HNSW。必须传播。
    // #R37 performance/medium：清理用 migration_flags 门控——`COUNT(*) NOT EXISTS`
    // 是全表相关扫描 + 可能的大 DELETE，若每次启动都跑会拖慢启动并持写锁。
    // #R38 other/low：门控 + 清理 + 置位必须在单个 BEGIN IMMEDIATE 事务内——
    // init_core_tables 可能被 CLI/server/库引擎三进程并发首次启动调用，check-then-act
    // 会让多个进程同时跑全表扫描/DELETE（写锁争用），且 DELETE 后崩溃会留下未置位
    // 标记导致下次重复清理。BEGIN IMMEDIATE 使第二进程阻塞到首个提交后看到已置位。
    // #R38 maintainability/low：库代码（Python bindings 宿主可达）用 eprintln 不污染 stdout。
    // #R43 bug/high：事务用 rusqlite 的 transaction_with_behavior(Immediate)——Transaction
    // 在 Drop 时自动回滚，任何 `?` 提前返回或 panic 都不会把"仍持有写锁的连接"还给池
    // （r2d2 无 test_on_check_out，归还后后续查询会静默跑在未提交事务内、写锁不释放）。
    // #R43 performance/medium：持写锁做全表 COUNT+DELETE 相关扫描可能超 busy_timeout——
    // 空表直接短路（最常见的首次启动场景），减少持锁时间。
    // #R46 maintainability/low：flag 按**清理集**版本化——`orphan_vector_cleanup_v1`
    // 置位后清理分支永久跳过；未来新增第三张向量表并扩展本清理时，必须用新 flag 名
    // （如 v2）注册，否则已迁移部署的清理扩展被静默禁用（新表孤儿每启动重入 HNSW）。
    const CLEANUP_FLAG: &str = "orphan_vector_cleanup_v1";
    // #R49 performance/medium：**refused 态持久化**——阈值拒绝/opt-in 未开时若每次
    // 启动都重跑 `COUNT ... NOT EXISTS` 相关扫描（持写锁），大孤儿集（稳定的合法
    // 外部向量态）会让每次启动变慢并阻塞并发写者。refused 标记置位后清理分支整体
    // 跳过，直到运维处理（删除 migration_flags 中的 refused 行后重启，或设
    // MEMORIA_FORCE_ORPHAN_CLEANUP=1 + MEMORIA_ORPHAN_CLEANUP_VECTORS=1 强制）。
    // #R50 bug/medium：force/opt-in env 在 **gate 之前**读取——若只在清理闭包内读，
    // REFUSED_FLAG 置位后 gate（refused_done != 0）直接跳过、env 永不生效（文档承诺
    // 的逃生通道静默失效，唯一出路变成手删 migration_flags 行）。force 时进入清理
    // 分支（覆盖 refused 态），成功后再清除 refused 标记。
    const REFUSED_FLAG: &str = "orphan_vector_cleanup_refused_v1";
    let force_cleanup = std::env::var("MEMORIA_FORCE_ORPHAN_CLEANUP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let clean_vectors = std::env::var("MEMORIA_ORPHAN_CLEANUP_VECTORS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // #R50 bug/low：flag 读取软降级（BUSY/DB 故障时按"未置位"处理）——硬失败会让
    // 并发首启在 .expect 处 panic（与 DDL/清理的软降级意图一致）；读失败=0 只会让
    // 清理多跑一次（清理本身软降级 + 事务内重读 + 幂等），安全。
    let flag_set = |conn: &rusqlite::Connection, flag: &str| -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
            rusqlite::params![flag],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or_else(|e| {
            eprintln!("[Memoria] WARN: check migration flag {flag} failed (treated as unset): {e}");
            false
        })
    };
    let already = flag_set(&conn, CLEANUP_FLAG);
    let refused_done = flag_set(&conn, REFUSED_FLAG);
    // #R52 bug/medium：外层 gate **只由 CLEANUP_FLAG 门控**——refused 态只应跳过
    // memory_vectors 段（事务内判定），此前 `!refused_done` 会把整个清理（含 hype
    // 孤儿清理）跳过：用户仅忘记设 opt-in env 就会永久禁用 hype 清理（hype 行按
    // 注释必为垃圾、无合法态），唯一出路是手删 migration_flags 行或 force env 对。
    // force gate 保留（覆盖已置位标记的逃生通道，#R50/#R51）。
    if !already || (force_cleanup && clean_vectors) {
        // 清理**软降级**（#R48 bug/medium）：BEGIN IMMEDIATE 阻塞超 busy_timeout（并发
        // 首启大库 DELETE 可超 5s）、或清理中任何 DB 错误，**不阻断启动**——main.rs/
        // mcp_server.rs 用 .expect()，硬失败会让并发首启部署直接 panic 中止。失败仅
        // log + 不置位 flag（下次启动重试）；孤儿的影响只是索引内存/重建耗时，可后补。
        // 正常完成（含"无孤儿"）才置位。
        let cleanup = (|| -> Result<(), String> {
            // #R43 bug/medium：rusqlite transaction_with_behavior(Immediate)——Transaction
            // Drop 时自动回滚，任何 `?` 提前返回或 panic 都不会把"仍持有写锁的连接"还给
            // 池（r2d2 无 test_on_check_out，归还后后续查询会静默跑在未提交事务内）。
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| format!("begin cleanup tx: {}", e))?;
            // 事务内重读**CLEANUP_FLAG**（#R52 bug/medium：并发首启时对端进程可能在
            // 本进程 gate 检查后、BEGIN 生效前完成清理——只查 CLEANUP_FLAG；refused
            // 不再参与短路（refused 只门控 memory_vectors 段，hype 段始终执行）。
            let already2: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![CLEANUP_FLAG],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check cleanup flag (tx): {}", e))?;
            // #R51 bug/high：force 时**不能短路**——force gate（force_cleanup &&
            // clean_vectors）存在的唯一意义就是覆盖已置位的 CLEANUP/REFUSED 标记；
            // 此处 `already2 != 0` 无条件 return 会让逃生通道永久 no-op（refused 态
            // 一旦持久化，普通 gate 不再进入、force gate 进入又被短路——死局，只能
            // 手删 migration_flags 行）。非 force 时保留短路（对端进程已完成）。
            if already2 != 0 && !(force_cleanup && clean_vectors) {
                tx.commit()
                    .map_err(|e| format!("commit cleanup tx (noop): {}", e))?;
                return Ok(());
            }
            // #R47 performance/medium：空表短路用 EXISTS(LIMIT 1)（全 COUNT 扫描换首行
            // 命中即停），避免持写锁做无谓全表扫描。
            // #R42/#R44 performance/medium：**单条** DELETE（一次相关扫描）——分批
            // LIMIT 1000 在同一个 BEGIN IMMEDIATE 事务内不降低持锁时间（每批全表扫描）。
            // #R43 maintainability/low：SQL 规范化 + 错误消息内嵌 SQL 片段便于审计。
            // #R51 maintainability/medium：**hype 段与 memory_vectors 段分离**——
            // memory_hype_vectors 可由离线脚本重写（lib.rs/mcp_server.rs 文档），但
            // 内容始终派生自 memories（问句向量），无 memories 行的 hype 行必为孤儿
            // 垃圾、无合法态——不需要 opt-in/阈值；且 refused（仅由 memory_vectors
            // 触发）不得禁用 hype 清理（此前共享 REFUSED_FLAG 让 memory_vectors 的
            // 拒绝永久跳过两表清理）。
            let h_exists: i64 = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM memory_hype_vectors LIMIT 1)",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check memory_hype_vectors exists: {}", e))?;
            if h_exists > 0 {
                let orphans_hype: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM memory_hype_vectors \
                         WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("count memory_hype_vectors orphans: {}", e))?;
                if orphans_hype > 0 {
                    const DEL_HYPE: &str = "DELETE FROM memory_hype_vectors \
                        WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id)";
                    tx.execute(DEL_HYPE, []).map_err(|e| {
                        format!("clean memory_hype_vectors orphans: {e} [SQL: {DEL_HYPE}]")
                    })?;
                    eprintln!("[Memoria] Migration: removed {orphans_hype} orphan memory_hype_vectors rows");
                }
            }
            // #R48 bug/high（数据安全）：memory_vectors 的**孤儿 COUNT 必须有**——
            // DELETE 无差别销毁所有"无 memories 行的向量行"，但代码库并不强制该
            // 不变量：add_vectors（Python bindings，lib.rs:280）接受任意调用方 id、
            // persist::lookup_namespace 对无 memories 行回退 'default'、vector_search
            // 不 join memories——外部注册向量/待重导入/合成 id 是合法数据态。
            // #R49 bug/high：memory_vectors 孤儿删除 **opt-in**——默认只报告不删除，
            // 设 MEMORIA_ORPHAN_CLEANUP_VECTORS=1 才删（仍受阈值与
            // MEMORIA_FORCE_ORPHAN_CLEANUP=1 约束）。
            // #R51 maintainability/medium：此前 refused（refused_done）→ 本段跳过——
            // 非 force 时保持拒绝态（不再重扫评估）；force 时重新评估。
            const ORPHAN_REFUSE_THRESHOLD: i64 = 5000;
            // force/opt-in env 在 gate 之前已读取（#R50 bug/medium，见函数上方）。
            let mut refused = false;
            if refused_done && !(force_cleanup && clean_vectors) {
                // #R51：WARN 文案明确**两个 env 都要**（此前只提示 FORCE，而 gate
                // 还要求 OPT_IN——误导运维）。
                eprintln!(
                    "[Memoria] WARN: memory_vectors orphan cleanup previously refused; skipping (set MEMORIA_ORPHAN_CLEANUP_VECTORS=1 AND MEMORIA_FORCE_ORPHAN_CLEANUP=1 to force, or delete migration_flags row '{REFUSED_FLAG}')"
                );
                refused = true;
            } else {
                let v_exists: i64 = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM memory_vectors LIMIT 1)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("check memory_vectors exists: {}", e))?;
                if v_exists > 0 {
                    let orphans_vec: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM memory_vectors \
                             WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id)",
                            [],
                            |r| r.get(0),
                        )
                        .map_err(|e| format!("count memory_vectors orphans: {}", e))?;
                    if orphans_vec > 0 {
                        if !clean_vectors {
                            eprintln!(
                                "[Memoria] WARN: {orphans_vec} orphan memory_vectors rows (possible legit external vectors via add_vectors); NOT deleting - set MEMORIA_ORPHAN_CLEANUP_VECTORS=1 to enable (MEMORIA_FORCE_ORPHAN_CLEANUP=1 bypasses the {ORPHAN_REFUSE_THRESHOLD} threshold)"
                            );
                            refused = true;
                        } else if orphans_vec > ORPHAN_REFUSE_THRESHOLD && !force_cleanup {
                            eprintln!(
                                "[Memoria] WARN: {orphans_vec} orphan memory_vectors rows ABOVE safety threshold {ORPHAN_REFUSE_THRESHOLD}; refusing auto-delete (set MEMORIA_FORCE_ORPHAN_CLEANUP=1 to force)"
                            );
                            refused = true;
                        } else {
                            const DEL_VEC: &str = "DELETE FROM memory_vectors \
                                WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id)";
                            tx.execute(DEL_VEC, []).map_err(|e| {
                                format!("clean memory_vectors orphans: {e} [SQL: {DEL_VEC}]")
                            })?;
                            eprintln!("[Memoria] Migration: removed {orphans_vec} orphan memory_vectors rows");
                        }
                    }
                }
            }
            // 置位标记（事务内，提交后对并发进程可见）：正常完成（含"无孤儿"）→
            // CLEANUP_FLAG；阈值/opt-in 拒绝（refused）→ REFUSED_FLAG——持久化拒绝态
            // 使后续启动跳过整个清理（不再持写锁重扫相关扫描，#R49 performance/medium）。
            // #R50 bug/medium：**强制成功运行后清除 REFUSED_FLAG**——force gate
            // （force_cleanup && clean_vectors）会无视 refused 态进入清理，若不清除
            // 旧 refused 标记，后续普通启动（无 env）仍被 refused 挡住；清除后
            // 正常状态由 CLEANUP_FLAG 覆盖。
            if refused {
                tx.execute(
                    "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                    rusqlite::params![REFUSED_FLAG],
                )
                .map_err(|e| format!("set refused flag: {}", e))?;
            } else {
                tx.execute(
                    "DELETE FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![REFUSED_FLAG],
                )
                .map_err(|e| format!("clear refused flag: {}", e))?;
                tx.execute(
                    "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                    rusqlite::params![CLEANUP_FLAG],
                )
                .map_err(|e| format!("set cleanup flag: {}", e))?;
            }
            // #R43 bug/medium：commit 失败时 Transaction 的 Drop 自动回滚——连接不会
            // 带着未提交事务还池。
            tx.commit()
                .map_err(|e| format!("commit cleanup tx: {}", e))?;
            Ok(())
        })();
        if let Err(e) = cleanup {
            // #R48 bug/medium：清理失败不阻断启动（软降级，flag 未置位 → 下次重试）。
            eprintln!("[Memoria] WARN: orphan cleanup skipped (will retry next start): {e}");
        }
    }
    Ok(())
}
