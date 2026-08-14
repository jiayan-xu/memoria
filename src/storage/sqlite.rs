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
            conn.execute_batch(&format!(
                "ALTER TABLE memories ADD COLUMN {} {};",
                col, ctype
            ))
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

/// #R58 maintainability/high：**共享 commit helper**（改名 commit_tx——名字如实地
/// 描述"只 commit、回滚由 Drop 兜底"的行为，原 commit_or_rollback 承诺显式回滚路径
/// 误导读者去"修复"一个不可能形态）。四处 commit（updated_at/trigger/
/// cleanup-noop/cleanup）此前各自重复"commit 失败 + 显式 ROLLBACK"模式。
/// #R57 实证修正（推翻 R25 评论 10 的"drop-guard 已清除"说法）：rusqlite 0.32.1 的
/// `Transaction` Drop 语义（transaction.rs:242-247 finish_）用 **is_autocommit()** 判定
/// 而非 committed 标志——commit 失败时事务仍活跃（is_autocommit=false），Drop 按
/// 默认 `DropBehavior::Rollback` **自动执行回滚**；"commit 失败会带写锁还池"不成立，
/// R25 加的显式 ROLLBACK 是画蛇添足（且与 `&mut conn` 借用冲突，helper 无法取
/// conn 引用）。commit 成功则 is_autocommit=true，Drop no-op。helper 只收口错误
/// 消息格式，防四处文案漂移。
fn commit_tx(tx: rusqlite::Transaction, label: &str) -> Result<(), String> {
    tx.commit().map_err(|e| format!("{label}: {e}"))
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
    // #R68 bug/low：**只 DatabaseBusy 重试**——非共享缓存模式下 SQLITE_LOCKED 是
    // 同连接冲突（未终结语句/事务内读→写升级），等待不能解决，重试只烧 ~3s
    // 启动延迟（500ms/1s/1.5s）后照样失败；WARN 文案 "busy/locked" 会误导部署
    // 停滞诊断。LOCKED 按真实错误传播。
    matches!(
        e,
        rusqlite::Error::SqliteFailure(se, _)
            if se.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// #R53 bug/high：`duplicate column name` 判定——SQLite 的 ALTER ADD COLUMN 重复列
/// 报 **SQLITE_ERROR（主码 1）+ 消息含 "duplicate column name"**；libsqlite3-sys 把
/// 未显式映射的码（含 SQLITE_ERROR=1）归为 `ErrorCode::Unknown`（extended_code
/// 2001 不存在——合法 SQLITE_CONSTRAINT 子码是 275/531/787/1043/1299/1555/1811/
/// 2067/2323/2579）。此前按 extended_code==2001 匹配永不命中——check-then-act
/// 竞态下对端进程刚提交同一 ALTER 时本进程仍 panic（守卫形同虚设）。按
/// Unknown 主码 + 消息判定，消息匹配"已应用"幂等处理。
/// #R56/#R58 实证（驳斥"应匹配 ErrorCode::Error"的说法）：rusqlite 0.32.1 的
/// `ErrorCode` 是 libsqlite3-sys 的 re-export（rusqlite/src/lib.rs:79 `pub use
/// crate::ffi::ErrorCode`），该枚举**没有 Error 变体**；`Error::new` 的 `_ =>`
/// catch-all 分支把 SQLITE_ERROR(1)（未在 match 中显式列出）归入 `Unknown`。
/// #R58 已按 Cargo.lock 实际锁定的 libsqlite3-sys **0.30.1** 复核
/// （libsqlite3-sys-0.30.1/src/error.rs:67-92 `impl Error::new`）：映射与 0.28.0
/// 完全一致（SQLITE_ERROR 无显式分支 → Unknown），heuristic 成立。`ErrorCode::Error`
/// 无法编译（变体不存在）。BEGIN IMMEDIATE 事务仍是主防线，本守卫只作窗口兜底，
/// 依赖英文消息 'duplicate column name' 的 best-effort 语义如实记录。
fn is_duplicate_column(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(se, msg) => {
            se.code == rusqlite::ErrorCode::Unknown
                && msg
                    .as_deref()
                    .map_or(false, |m| m.contains("duplicate column name"))
        }
        _ => false,
    }
}

fn execute_batch_retry(conn: &rusqlite::Connection, sql: &str, label: &str) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        match conn.execute_batch(sql) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) && attempt < 3 => {
                attempt += 1;
                // #R55 maintainability/low：重试前 WARN——静默重试在并发首启锁竞争
                // 下表现为数秒启动停滞零诊断，部署死锁难排查（与 updated_at BEGIN
                // 循环同款）。
                // #R69 other/low：**措辞与实现一致**——is_busy 只匹配 DatabaseBusy
                // （SQLITE_BUSY），SQLITE_LOCKED 不重试；此前 "busy/locked" 让运维
                // 误以为 LOCKED 也会重试（启动停滞排查时误导归因）。
                eprintln!(
                    "[Memoria] WARN: {label} busy (attempt {attempt}/3), retrying in {}ms: {e}",
                    500 * attempt
                );
                std::thread::sleep(std::time::Duration::from_millis(500 * attempt as u64));
            }
            Err(e) => return Err(format!("{label}: {}", e)),
        }
    }
}

/// #R56 bug/medium：可选表 DDL 的**软降级包装**——execute_batch_retry 的失败（含
/// 重试耗尽后的非 busy 错误）只 WARN 不传播：调用方（migrate_hype_vectors →
/// init_core_tables → main.rs/mcp_server.rs .expect()）硬失败会中止整个启动，
/// 而这三张附加表缺失时核心功能不受影响（见调用处注释），下次启动重试即可。
fn ddl_soft(conn: &rusqlite::Connection, sql: &str, label: &str) {
    if let Err(e) = execute_batch_retry(conn, sql, label) {
        eprintln!("[Memoria] WARN: {label} failed (soft-degraded, will retry next start): {e}");
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
///
/// #R60 maintainability/low **控制流总览**（各段详细 rationale 见段内 #R 注释）：
/// ① busy_timeout PRAGMA（软处理）→ ② memory_vectors.updated_at 补列（只读快速路径 +
/// BEGIN 退避重试 + 必要列验证传播，不软降级）→ ③ 三张可选表 DDL（ddl_soft 软降级）+
/// schema 契约检查 → ④ 触发器重建（版本 flag 门控 + 软降级）→ ⑤ 孤儿向量清理
/// （hype 段独立 HYPE_FLAG / vec 段 REFUSED_FLAG，删除需 OPT_IN+FORCE 双 env，
/// 软降级 + 下次重试）。整体设计目标：并发首启存活（软降级）+ 数据安全（外部向量
/// 不误删）+ 幂等（migration_flags 门控）。
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
    // #R55 performance/low：**只读快速路径**——新库的基表 schema 已声明该列，先在
    // 无事务下查列存在（pragma_table_info 只读、busy_timeout 已设），存在即跳过整个
    // BEGIN IMMEDIATE 块（多进程首启/滚动重启下避免每次启动一次写锁获取与跨进程
    // 串行化，与本迁移的 flag 门控设计目标一致）。缺列才进事务路径；check-then-act
    // 竞态由 `duplicate column name` 幂等判定兜底（对端先提交同一 ALTER 的窗口）。
    // #R53 bug/high：**check + ALTER 包进 BEGIN IMMEDIATE 事务**——pragma_table_info
    // 读与 ALTER 分开执行存在 check-then-act 竞态：两进程同时见列缺失，第一个提交
    // ALTER 后第二个报 `duplicate column name`（SQLITE_ERROR，is_busy 不重试）→
    // .expect 中止启动。事务内重读（首个提交后看到列已存在 → noop 提交），且
    // 读失败**传播**（事务内 BEGIN 已按 busy_timeout 等待写锁，读失败即真实 DB 问题，
    // 不再 unwrap_or(0) 掩蔽成"列缺失"）。`duplicate column name` 另作"已应用"处理
    // （对端刚提交的窗口，幂等语义）。
    // #R54 bug/high：整个块 **BEGIN BUSY 退避重试**——并发首启时对端持写锁（大库
    // 孤儿 DELETE 可超 5s busy_timeout，或对端自身的 ALTER）会让本进程 BEGIN 直接
    // DatabaseBusy 并 ? 传播到 .expect 中止启动（本函数唯一既无 execute_batch_retry
    // 也无软降级的 DDL 路径）。退避与 execute_batch_retry 同款（3 次 500/1000/1500ms）。
    // #R55 bug/high：ALTER 默认值用**常量 `''`**——SQLite 的 ALTER TABLE ADD COLUMN
    // 禁止非常量默认表达式（`datetime('now')` 是函数调用，报 "Cannot add a column with
    // non-constant default" SQLITE_ERROR），恰在此迁移要服务的存量库上每次必现且
    // 不是 duplicate-column 分支 → 传播到 .expect 中止启动。常量默认即可：4 列
    // upsert 每次写入都会 SET updated_at=datetime('now')，存量行的 '' 只占位。
    // #R58 注释统一（修正 #R55 与 #R57 的矛盾）：rusqlite 0.32.1 Transaction 的
    // Drop 用 is_autocommit() 判定（transaction.rs finish_），commit 失败时事务仍
    // 活跃 → 默认 DropBehavior::Rollback **自动回滚**，不存在"带写锁还池"；
    // 事务内语句失败（`?` 提前返回）同样由 Drop 回滚。见 commit_tx helper doc。
    {
        // 只读快速路径：新库基表已含该列（常见情形）→ 零写锁。
        let fast_has_upd: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memory_vectors') WHERE name='updated_at'",
                [],
                |r| r.get(0),
            )
            // #R66 maintainability/low：读失败**可见**（BUSY/坏连接被误判为列缺失
            // 触发无谓写事务——事务路径安全，但诊断应点名真实错误）。
            .unwrap_or_else(|e| {
                eprintln!(
                    "[Memoria] WARN: check memory_vectors.updated_at (fast path) failed, treating as missing: {e}"
                );
                0
            });
        if fast_has_upd == 0 {
            let mut attempt = 0;
            loop {
                // BEGIN 失败携带原始 rusqlite 错误供 busy 判定；其余错误只带消息
                // （重试只对 BEGIN 的锁竞争有意义——事务内语句失败是真实 DB 问题）。
                let r: Result<(), (String, Option<rusqlite::Error>)> = (|| {
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .map_err(|e| (format!("begin updated_at tx: {}", e), Some(e)))?;
                    let has_upd: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM pragma_table_info('memory_vectors') WHERE name='updated_at'",
                            [],
                            |r| r.get(0),
                        )
                        .map_err(|e| (format!("check memory_vectors.updated_at: {}", e), None))?;
                    if has_upd == 0 {
                        if let Err(e) = tx.execute_batch(
                            "ALTER TABLE memory_vectors ADD COLUMN updated_at TEXT DEFAULT ''",
                        ) {
                            // #R53 bug/high：`duplicate column name` = 对端进程刚提交了
                            // 同一 ALTER（并发首启窗口）——按"已应用"处理，不中止启动。
                            // #R66 maintainability/low：**重查列存在**（消息/错误码
                            // 无关幂等）——is_duplicate_column 依赖 libsqlite3-sys
                            // 版本映射 + 英文消息，升级/变体可能静默失效；列现在
                            // 存在即视为已应用。
                            let col_now: i64 = tx
                                .query_row(
                                    "SELECT COUNT(*) FROM pragma_table_info('memory_vectors') WHERE name='updated_at'",
                                    [],
                                    |r| r.get(0),
                                )
                                .unwrap_or(0);
                            if col_now > 0 || is_duplicate_column(&e) {
                                eprintln!(
                                    "[Memoria] Migration: memory_vectors.updated_at added by peer process"
                                );
                            } else {
                                // #R58 bug/medium：**携带原始错误（Some(e)）供 busy
                                // 重试**——BEGIN IMMEDIATE 只获取 RESERVED 锁，ALTER
                                // 需要 EXCLUSIVE 锁升级；并发首启时对端事务中（或长
                                // 读事务）可让 ALTER 报 SQLITE_BUSY 即使 BEGIN 已成功。
                                // 此前归 None 立即软降级：列永不补、旧库 4 列 upsert
                                // 运行时报 no such column。busy/locked 的 ALTER 错误
                                // 走重试循环（其余错误仍即时返回）。
                                return Err((
                                    format!("add memory_vectors.updated_at: {}", e),
                                    Some(e),
                                ));
                            }
                        } else {
                            eprintln!(
                                "[Memoria] Migration: added memory_vectors.updated_at column"
                            );
                        }
                    }
                    // #R58 maintainability/low：共享 commit_tx 的 Drop 回滚语义
                    // （commit 失败后事务仍活跃，Drop 自动回滚）。
                    // #R64 bug/medium：**手写 commit 携带原始错误（Some(e)）**——
                    // commit_tx 已把错误字符串化无法还原 rusqlite::Error；WAL 下
                    // COMMIT 可 BUSY（auto-checkpoint 与并发读者竞态），此前归 None
                    // 立即终止（列验证失败 → .expect 中止部署，并发首启场景唯一
                    // 未保护的写路径）；busy COMMIT 走重试循环。
                    if let Err(e) = tx.commit() {
                        return Err((format!("commit updated_at tx: {e}"), Some(e)));
                    }
                    Ok(())
                })();
                match r {
                    Ok(()) => break,
                    Err((_msg, Some(e))) if is_busy(&e) && attempt < 3 => {
                        attempt += 1;
                        // #R55 maintainability/low：重试前 WARN——静默重试在并发首启
                        // 锁竞争下表现为数秒启动停滞零诊断，部署死锁难排查。
                        // #R69 other/low：措辞与实现一致（is_busy 只匹配 DatabaseBusy）。
                        eprintln!(
                            "[Memoria] WARN: updated_at migration busy (attempt {attempt}/3), retrying in {}ms: {e}",
                            500 * attempt
                        );
                        std::thread::sleep(std::time::Duration::from_millis(500 * attempt as u64));
                    }
                    Err((msg, _)) => {
                        // #R57 bug/medium：**软降级曾在此 break**——#R60 bug/medium
                        // 推翻：updated_at 是**写路径必要列**（persist vector_table!
                        // 的 4 列 upsert 引用它，无该列则每次写入报 no such column；
                        // 生产调用方 let _ 丢弃 Result，失败只以限流 WARN 呈现）。
                        // 与 ddl_soft 的**可选**附加表（缺表只是 HyPE 功能降级）
                        // 不同，此处软降级 = 核心写路径静默损坏而进程"健康"。
                        // 重试耗尽后**验证列实际存在**：对端进程可能已成功 ALTER
                        // （本进程失败仅因锁竞争）→ 通过；否则传播错误（启动失败
                        // 显式暴露，运维可重试）。
                        let has_upd_after: i64 = conn
                            .query_row(
                                "SELECT COUNT(*) FROM pragma_table_info('memory_vectors') \
                                 WHERE name='updated_at'",
                                [],
                                |r| r.get(0),
                            )
                            // #R69 bug/medium：**读失败传播而非掩蔽**——unwrap_or(0)
                            // 会把瞬态读错误（并发首启 busy_timeout 后的 SQLITE_BUSY、
                            // 坏池连接）误判为"列缺失"：此检查在硬失败决策路径上，
                            // 误报产出误导性 `updated_at missing after retries` 并
                            // 经 .expect() 中止启动——与同函数内事务中该读错误
                            // 传播的原则矛盾。只有确认的 0 才算缺失。
                            .map_err(|e| {
                                format!("verify memory_vectors.updated_at after retries: {e}")
                            })?;
                        if has_upd_after == 0 {
                            return Err(format!(
                                "memory_vectors.updated_at missing after retries (core write path requires it): {msg}"
                            ));
                        }
                        eprintln!(
                            "[Memoria] updated_at migration retries exhausted but column present (peer applied it): {msg}"
                        );
                        break;
                    }
                }
            }
        }
    }
    // #R49 bug/medium：DDL 用 BUSY 退避重试（见 execute_batch_retry）——并发首启时
    // 另一进程持写锁可能让本进程 DDL 立即 SQLITE_BUSY，.expect 硬失败会 panic 中止
    // 部署。
    // #R56 bug/medium：三张**可选/附加**表的 DDL **软降级**——非 busy 失败（SQLITE_
    // FULL 磁盘满 / IOERR / 权限）经 `?` 传播到 init_core_tables 的 .expect() 会硬
    // 中止启动，与本迁移其余分支（PRAGMA / 触发器重建 / 孤儿清理均软降级 + 下次
    // 重试）不一致；核心 memories/memory_vectors 无此三张表仍可运行（hype rebuild
    // 已有 or_default 兜底、flag 读取已有软处理）。失败 WARN + 下次启动重试。
    ddl_soft(
        &conn,
        "CREATE TABLE IF NOT EXISTS memory_hype_vectors (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL DEFAULT 'default',
            question TEXT,
            vector BLOB NOT NULL,
            updated_at TEXT DEFAULT (datetime('now'))
        )",
        "create memory_hype_vectors",
    );
    ddl_soft(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_hype_ns ON memory_hype_vectors(namespace)",
        "create idx_hype_ns",
    );
    // migration_flags 表（先建——触发器版本门控与下方孤儿清理共用；key-value 语义
    // 隔离，不复用 health.rs 的 user_version 位（#R40 maintainability/low：位复用会
    // 在"按版本号写 user_version"的未来路径上被静默清除或碰撞）。软降级同
    // memory_hype_vectors（#R56 bug/medium）：缺表时 flag 读取软处理为未置位、
    // 触发器/清理段各自软降级，下次启动重试。
    ddl_soft(
        &conn,
        "CREATE TABLE IF NOT EXISTS migration_flags (
            flag TEXT PRIMARY KEY,
            applied_at TEXT DEFAULT (datetime('now'))
        )",
        "create migration_flags",
    );

    // #R58 maintainability/low：**schema 契约检查**——ddl_soft 吞掉所有失败（schema
    // typo/权限/磁盘满），若此后无验证，CREATE 结果与消费方（persist vector_table!
    // upsert id/namespace/vector/updated_at；rebuild 读 id/vector；离线脚本写 question）
    // 的漂移会让 HyPE 每启动静默失效而进程照常"健康"（孤儿清理段也查询该表）。轻量
    // 列集校验：期望列缺一即 WARN（不阻断——flag/重建路径已有软处理，缺失只影响
    // 4 列 upsert 与双路检索，可下次启动修复后自愈）。
    {
        // #R60 maintainability/low：期望列含 **question**——离线 HyPE builder
        // （build_hype_vectors.py 插入 id/namespace/question/vector/updated_at）依赖
        // 它；此前漏检会让未来 schema 变更静默破坏离线写入路径。
        let expect: [&str; 5] = ["id", "namespace", "question", "vector", "updated_at"];
        // #R64 maintainability/low：**先查表存在**——DDL 软降级（ddl_soft 失败）时
        // pragma_table_info 对不存在的表返回 0 行，把"DDL 失败"误报为"5 列全部
        // schema 漂移"（运维查无此 schema 变更）；缺表与列漂移是不同根因，
        // 诊断必须区分（缺表 = 瞬时 DDL 失败，下次启动自愈）。
        let table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_hype_vectors'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if table_exists == 0 {
            eprintln!(
                "[Memoria] WARN: memory_hype_vectors table absent (DDL soft-degraded; will retry next start) - HyPE feature degraded"
            );
        } else {
            let have: Vec<String> =
                match conn.prepare("SELECT name FROM pragma_table_info('memory_hype_vectors')") {
                    Ok(mut stmt) => stmt
                        .query_map([], |r| r.get::<_, String>(0))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default(),
                    Err(_) => vec![],
                };
            let missing: Vec<&str> = expect
                .iter()
                .copied()
                .filter(|c| !have.iter().any(|h| h.as_str() == *c))
                .collect();
            if !missing.is_empty() {
                eprintln!(
                    "[Memoria] WARN: memory_hype_vectors schema drift - missing columns {missing:?} (HyPE feature degraded until next successful migration)"
                );
            }
        }
    }

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
    // #R64 bug/medium：fallback body 的**独立版本标记**——置位表示"曾安装过
    // content-only body"；gate 检查它 → 下次启动（hype 表可能已恢复）重新评估
    // 并升级 full body。升级成功后清除（同 refused flag 模式）。
    const TRIGGER_FALLBACK_VERSION: &str = "trigger_mem_ad_vec_fallback_v1";
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
    let trig_fallback_done = conn
        .query_row(
            "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
            rusqlite::params![TRIGGER_FALLBACK_VERSION],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        // #R68 other/low：读失败可见（与 trig_done 同款 WARN）——静默按未置位
        // 会让 fallback→full 升级被瞬时 BUSY 跳过（幂等，但自愈延迟不可见）。
        .unwrap_or_else(|e| {
            eprintln!("[Memoria] WARN: check trigger fallback flag failed (treated as unset): {e}");
            false
        });
    // #R64 bug/medium：fallback 置位 → 重新评估（hype 表可能已恢复，需升级 full
    // body）；升级成功路径清除 fallback flag。
    // #R66/#R67 bug/high：**表存在检查进 gate**——full body 触发器在表缺失时
    // （手动 DROP/部分恢复/损坏）让每次 DELETE memories 报 no such table（核心
    // 删除路径被可选表硬前置）；缺表 → 降级 content-only body + fallback flag。
    // #R67 修正：原实现把降级检查放 `trig_done && !trig_fallback_done` 内——它是
    // gate 的精确否定，**永不执行**（不可达降级路径）。
    let hype_present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_hype_vectors'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // 进入条件：触发器未装 / fallback 已置位（需升级）/ v2 已装但表缺失（需降级）。
    if !trig_done || trig_fallback_done || (trig_done && hype_present == 0) {
        if trig_done && !trig_fallback_done && hype_present == 0 {
            eprintln!(
                "[Memoria] WARN: memory_hype_vectors absent though trigger v2 installed - downgrading trigger to content-only body"
            );
        }
        // #R49 bug/medium：触发器重建**软降级**——BEGIN IMMEDIATE 在并发首启大库
        // 清理持锁时可能 BUSY（busy_timeout 超时）；触发器缺失只影响未来 memories
        // 删除的联动清理（孤儿可下次启动补，或孤儿清理段兜底），不阻断启动。
        // 事务内所有语句失败时 Transaction Drop 自动回滚（#R43）。
        let trig = (|| -> Result<(), String> {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| format!("begin trigger tx: {}", e))?;
            // #R69 bug/high（R41 短路回归修复）：**DROP TRIGGER 移到短路判断之后**——
            // 此前 DROP 在闭包开头无条件执行、fallback 短路（表仍缺 + fallback 已装）
            // 在其后提交：DROP 已执行而 CREATE 被跳过 → mem_ad_vec 触发器永久消失
            // （hype 表缺失期间），DELETE FROM memories 不再级联清理 memory_vectors，
            // 孤儿行每启动 rebuild 重入 HNSW——正是迁移要防的无界泄漏。
            // #R63 bug/medium：**触发器 body 按 hype 表存在性分支**——SQLite 在
            // 触发时（而非 CREATE 时）解析 body 的表名：memory_hype_vectors 的 DDL
            // 是 ddl_soft（可能软降级缺表），无条件引用它会让之后**每次** DELETE
            // FROM memories 报 `no such table: memory_hype_vectors`——核心级联清理
            // 与可选表耦合。缺表时用单表 body（memory_vectors 清理不受影响）。
            let hype_table_exists: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_hype_vectors'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check memory_hype_vectors existence: {}", e))?;
            // #R69 performance/low：**fallback 持久态短路**——表仍缺失 + fallback
            // body 已装（trig_fallback_done）时，content-only body 已是当前版本且
            // 未变：重建 DROP+CREATE 是纯浪费的写锁 DDL 序列化（每启动一次，直到
            // 表恢复），正是版本 flag 设计（#R46）要避免的。此时跳过 DROP+CREATE
            // 与 flag 写入直接提交（幂等）；表一旦恢复，`trig_fallback_done` 使
            // gate 仍进入（`trig_fallback_done` 恒 true），hype_table_exists > 0
            // 分支照常升级 full body——自愈路径不受影响。
            if hype_table_exists == 0 && trig_fallback_done {
                commit_tx(tx, "commit trigger tx (fallback unchanged)")?;
                return Ok(());
            }
            // 到达此处必然重建（升级 full body 或首次安装 fallback）：先 DROP
            // 旧 body（#R69 bug/high：DROP 不得早于短路判断——见上方注释）。
            tx.execute_batch("DROP TRIGGER IF EXISTS mem_ad_vec")
                .map_err(|e| format!("drop mem_ad_vec trigger: {}", e))?;
            // #R66：降级路径（版本已置位 + 表缺失）——hype_table_exists==0 时走
            // fallback body + fallback flag（下段分支已按表存在分流）。
            if hype_table_exists > 0 {
                tx.execute_batch(
                    "CREATE TRIGGER mem_ad_vec AFTER DELETE ON memories BEGIN
                        DELETE FROM memory_vectors WHERE id = old.id;
                        DELETE FROM memory_hype_vectors WHERE id = old.id;
                    END",
                )
                .map_err(|e| format!("create mem_ad_vec trigger: {}", e))?;
                // full body 升级成功：清除 fallback 标记（下次启动 gate 不再进入）。
                tx.execute(
                    "DELETE FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![TRIGGER_FALLBACK_VERSION],
                )
                .map_err(|e| format!("clear trigger fallback flag: {}", e))?;
            } else {
                // #R64 bug/medium：fallback body 置**独立 flag**——同 TRIGGER_VERSION
                // 会让后续启动（hype 表已恢复）跳过触发器段、full body 永不安装
                // （WARN 承诺的自愈落空，DELETE 静默漏清 hype 孤儿）。
                eprintln!(
                    "[Memoria] WARN: memory_hype_vectors missing (DDL soft-degraded); trigger installed with memory_vectors-only body - full body will be installed next start when table exists"
                );
                tx.execute_batch(
                    "CREATE TRIGGER mem_ad_vec AFTER DELETE ON memories BEGIN
                        DELETE FROM memory_vectors WHERE id = old.id;
                    END",
                )
                .map_err(|e| format!("create mem_ad_vec trigger (content-only): {}", e))?;
                tx.execute(
                    "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                    rusqlite::params![TRIGGER_FALLBACK_VERSION],
                )
                .map_err(|e| format!("set trigger fallback version flag: {}", e))?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                rusqlite::params![TRIGGER_VERSION],
            )
            .map_err(|e| format!("set trigger version flag: {}", e))?;
            // #R58 maintainability/low：共享 commit_tx（见 helper doc）。
            commit_tx(tx, "commit trigger tx")?;
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
    // #R69 bug/high：**dry-run 审计模式**——orphan 判定是"无 memories 行即垃圾"，
    // 但写侧不强制该不变量（add_vectors/put_stored_vector 接受任意 id，外部注册/
    // 暂存向量是合法数据态）；即使双 env 齐备，删除也是永久性销毁。dry_run 时
    // 打印**全部**待删 id 清单（而非仅 5 行样本）供人工确认，且不执行 DELETE、
    // 不置任何完成 flag（下次启动重新评估）。运维流程：先 dry-run 审清单 →
    // 确认无误后去掉 dry_run 再启动执行删除。
    let dry_run = std::env::var("MEMORIA_ORPHAN_DRY_RUN")
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
    // #R54 bug/low：refused 态不再外层读取——事务内重读（refused2，与完成 flag
    // 同款并发语义）；外层快照会在对端提交 REFUSED 的窗口内过期。
    // #R54 performance/medium：hype 段**独立完成 flag**——此前 refused（memory_vectors
    // 拒绝）时 CLEANUP_FLAG 不置位、gate 每次启动都进，hype 段的 EXISTS+COUNT 相关
    // 扫描（大表 O(n)）在持写锁事务里每启动重跑一遍（#R49 拒绝态持久化的初衷就是
    // 避免这个）。HYPE_FLAG = "hype 段已评估（删除或拒绝）"；force 时仍重新评估。
    const HYPE_FLAG: &str = "hype_orphan_cleanup_v1";
    let hype_done = flag_set(&conn, HYPE_FLAG);
    // #R52 bug/medium：外层 gate 由完成 flag 门控（任一未完成即进）——refused 态
    // 只跳过 memory_vectors 段的评估（事务内判定），不再跳过整个清理。
    // force gate 保留（覆盖已置位标记的逃生通道，#R50/#R51）。
    // #R55 performance/medium：**refused 态外层 gate 跳过**——refused 后 CLEANUP_FLAG
    // 故意不置位（#R49 语义：拒绝态需重新评估的机会），但每次启动都重新
    // BEGIN IMMEDIATE + flag 检查 + no-op commit 是在多进程部署里每次启动获取一次
    // 写锁（可阻塞到 busy_timeout），正是 #R49 声称避免的。refused 且两段完成
    // （hype 段独立 HYPE_FLAG 已置位）时整分支跳过；refused 但 hype 段未评估时仍需
    // 进入（hype 段有独立 flag，refused 不跳过它）。force 时一律进入。
    // 逻辑：`(!already && !refused_done) || !hype_done`——refused_done 只压掉
    // `!already`（vec 段拒绝态），hype 未完成仍进。
    let refused_done = flag_set(&conn, REFUSED_FLAG);
    // #R69 bug/medium：**dry-run 必须绕过 gate**——WARN 文案指引运维
    // "restart with MEMORIA_ORPHAN_DRY_RUN=1 to list every id"，但默认（无 env）
    // refuse 运行已置位 HYPE_FLAG + REFUSED_FLAG：此后 `(!already && !refused_
    // done) || !hype_done` 恒 false，清理块整体跳过、审计输出为零——逃生通道
    // 只在任何 refuse 评估持久化前有效，照文案操作反而拿不到清单。`|| dry_run`
    // 使审计总是可请求；dry-run 分支自身不写任何 flag（见 cleanup 闭包内
    // dry_ran 处理），下次启动状态不变。
    if (!already && !refused_done) || !hype_done || (force_cleanup && clean_vectors) || dry_run {
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
            // 事务内重读**两个完成 flag**（#R52 bug/medium：并发首启时对端进程可能在
            // 本进程 gate 检查后、BEGIN 生效前完成——任一完成即短路）。
            // #R56 bug/high：短路必须**两段 flag 都在**（`done2 == 2`）——`done2 > 0`
            // 把"任一完成 flag 存在"当"两段都完成"，但 gate 进入恰恰因为至少一段未
            // 完成。两个可达坏例：① refused 后状态为 HYPE_FLAG+REFUSED_FLAG（CLEANUP_
            // FLAG 未置位），运维按 WARN 指引删除 refused 行后下次启动进入 gate
            // （!already && !refused_done），done2==1 短路 no-op——memory_vectors 段
            // 永不重评、恢复路径静默死；② 存量库已有 CLEANUP_FLAG 但无 HYPE_FLAG
            // （升级/部分部署）：gate 经 !hype_done 进入，done2==1 短路——hype 段
            // 永不执行、其 flag 永不置位，孤儿 hype 行每启动重入 HNSW 且每次启动
            // 仍获取写锁 no-op。逐段 gate 自己的 flag 语义即 `done2 == 2`。
            let done2: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM migration_flags WHERE flag IN (?1, ?2)",
                    rusqlite::params![CLEANUP_FLAG, HYPE_FLAG],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check cleanup flags (tx): {}", e))?;
            // #R69 bug/high：**dry-run 绕过 done2 短路**——双 flag 已置（典型
            // post-refusal 态：hype 段阈值拒绝 + vec 段干净完成）时，gate 仅因
            // `|| dry_run` 进入，但本短路先触发：no-op commit、零审计输出——
            // 运维按 WARN 指引"restart with MEMORIA_ORPHAN_DRY_RUN=1 to list
            // every id"拿到空输出，hype 孤儿永久不可审计。dry-run 必须继续
            // 走各段评估打印清单（各段 dry-run 分支自身不写 flag，见下）。
            if done2 == 2 && !(force_cleanup && clean_vectors) && !dry_run {
                // #R58 maintainability/low：共享 commit_tx（见 helper doc）。
                commit_tx(tx, "commit cleanup tx (noop)")?;
                return Ok(());
            }
            // #R54 bug/low：refused 态**事务内重读**（与完成 flag 一致）——对端进程
            // 可能在本进程 gate 检查后、BEGIN 生效前提交 REFUSED_FLAG；用外层快照
            // 会基于过期拒绝重跑 memory_vectors 评估（浪费全扫、可能覆盖刚持久化
            // 的拒绝态）。
            let refused2: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![REFUSED_FLAG],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check refused flag (tx): {}", e))?;
            // #R47 performance/medium：空表短路用 EXISTS(LIMIT 1)（全 COUNT 扫描换首行
            // 命中即停），避免持写锁做无谓全表扫描。
            // #R42/#R44 performance/medium：**单条** DELETE（一次相关扫描）——分批
            // LIMIT 1000 在同一个 BEGIN IMMEDIATE 事务内不降低持锁时间（每批全表扫描）。
            // #R43 maintainability/low：SQL 规范化 + 错误消息内嵌 SQL 片段便于审计。
            // #R54 bug/medium：hype 段与 memory_vectors **统一守卫**——put_hype_
            // stored_vector 是公开 API 接受任意 id（与 put_stored_vector 镜像），
            // 外部工具可能在对应 memories 行之前/独立 stage hype 行（分批导入/部分
            // 写入/外部 id）；"hype 行必为垃圾"的不变量未在写侧强制。默认只报告
            // 不删除（opt-in MEMORIA_ORPHAN_CLEANUP_VECTORS=1），阈值/force 语义
            // 与 memory_vectors 段一致。
            const ORPHAN_REFUSE_THRESHOLD: i64 = 5000;
            // force/opt-in env 在 gate 之前已读取（#R50 bug/medium，见函数上方）。
            // #R69 bug/high：**dry-run 审计标记**——任一表段进入 dry-run 删除分支时
            // 置位：跳过 DELETE、跳过全部完成 flag（HYPE_FLAG/CLEANUP_FLAG/REFUSED_
            // FLAG 都不写），事务照常提交，下次启动重新评估。dry-run 语义 =
            // 审计预览，不固化任何"已处理"状态。
            let mut refused = false;
            // ── hype 段（HYPE_FLAG 未评估，或 force 覆盖）──
            // #R55 bug/medium：force 时**重新评估**——HYPE_FLAG 置位后（含 refused
            // 路径）`hype_done2 == 0` 恒假，hype 段对 force env 组合（FORCE+OPT_IN）
            // 静默 no-op，与 memory_vectors 段（refused2 > 0 && !force 才跳过）语义
            // 不一致，文档承诺的"force 覆盖重新评估"对 hype 表失效；孤儿 hype 行
            // 持续存在并在每次 rebuild 重入 HNSW。
            let hype_done2: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![HYPE_FLAG],
                    |r| r.get(0),
                )
                .map_err(|e| format!("check hype flag (tx): {}", e))?;
            // #R69 bug/medium：**dry-run 强制重新评估**——refuse 运行已置 HYPE_FLAG，
            // 不带 `|| dry_run` 时审计请求被 flag 短路（与 gate 处 #R69 同款问题）；
            // dry-run 分支自身不写 flag（段尾 !dry_ran 门），下次启动状态不变。
            if hype_done2 == 0 || (force_cleanup && clean_vectors) || dry_run {
                // #R65 bug/medium：**表存在检查先行**——hype 表 DDL 是 ddl_soft（可
                // 软降级缺表），直接 query 会 no such table 中止整个清理事务
                // （vec 段被连带，与触发器段的缺表保护一致）。
                let h_tbl: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_hype_vectors'",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("check memory_hype_vectors table: {}", e))?;
                if h_tbl == 0 {
                    eprintln!(
                        "[Memoria] WARN: memory_hype_vectors absent (DDL soft-degraded); skipping hype orphan segment"
                    );
                    // #R68 performance/medium：缺表也置 HYPE_FLAG——不置位会让
                    // hype_done 恒 false，gate 每启动重进 BEGIN IMMEDIATE + 对
                    // memory_vectors 跑 COUNT NOT EXISTS 相关扫描（#R49/#R55 要
                    // 避免的写锁成本）；表未来出现时由 force 或手动清 flag 重扫。
                    // #R69 bug/high：**dry-run 不写 HYPE_FLAG**——审计模式违反
                    // "不固化任何状态"契约；且表未来出现（如离线脚本重建行、
                    // 其 memories 已删）时 hype_done 恒 true、hype 孤儿段永久
                    // 跳过且无重复 WARN（孤儿持续重入 HNSW rebuild）。
                    if !dry_run {
                        tx.execute(
                            "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                            rusqlite::params![HYPE_FLAG],
                        )
                        .map_err(|e| format!("set hype flag (table absent): {}", e))?;
                    }
                } else {
                    let h_exists: i64 = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM memory_hype_vectors LIMIT 1)",
                            [],
                            |r| r.get(0),
                        )
                        .map_err(|e| format!("check memory_hype_vectors rows: {}", e))?;
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
                            // #R69 bug/high：WARN 文案**中性化**——此前 "are BOTH required
                            // to clean" 读起来像操作指引，运维照做即触发永久删除，但
                            // add_vectors/put_hype_stored_vector 接受任意 id、外部注册/
                            // 暂存向量是合法数据态（"无 memories 行 ⇒ 垃圾"并非写侧
                            // 强制不变量）。现在只陈述事实 + 提示 dry-run 审计通道。
                            if !clean_vectors {
                                eprintln!(
                                "[Memoria] WARN: {orphans_hype} orphan memory_hype_vectors rows (no matching memories row; may be legit staged/external hype vectors). Cleanup is opt-in and disabled by default. To inspect before any decision, restart with MEMORIA_ORPHAN_DRY_RUN=1 to list every id (nothing will be deleted)."
                            );
                                // #R69 bug/high：dry-run 时**不写任何 flag**——refused
                                // 态也会跳过后续启动评估；dry-run 是纯审计预览，不固化
                                // "已拒绝/已处理"状态（下次启动重新评估）。此前这里
                                // 只注释不置位（hype 段拒绝由 HYPE_FLAG 记录），dry-run
                                // 时 HYPE_FLAG 同样不置（见段尾 !dry_ran 门）。
                                if dry_run {
                                    }
                                // #R57 bug/low：hype 段拒绝**不置 refused**——REFUSED_FLAG
                                // 语义 = memory_vectors 段拒绝（见下）；hype 拒绝态由
                                // HYPE_FLAG 记录（段完成=删除或拒绝）。混用会让 vec 段
                                // 干净完成也被标记 refused、下次启动 WARN 误导归因。
                            } else if orphans_hype > ORPHAN_REFUSE_THRESHOLD && !force_cleanup {
                                eprintln!(
                                "[Memoria] WARN: {orphans_hype} orphan memory_hype_vectors rows ABOVE safety threshold {ORPHAN_REFUSE_THRESHOLD}; cleanup refused (count too large to be safely attributable to staged/external writes; inspect with MEMORIA_ORPHAN_DRY_RUN=1)"
                            );
                                if dry_run {
                                    }
                            } else if !force_cleanup {
                                // #R60 bug/high：**单 OPT_IN 只报告不删**——"无 memories 行
                                // ⇒ 垃圾"非强制不变量（外部/暂存向量是合法态）；删除执行
                                // 必须 OPT_IN + FORCE 双 env 显式确认（清单审计 + 阈值只
                                // 缓解可见性/数量，不修正分类正确性）。
                                eprintln!(
                                "[Memoria] WARN: {orphans_hype} orphan memory_hype_vectors rows (no matching memories row; may be legit staged/external hype vectors); not deleting. MEMORIA_ORPHAN_DRY_RUN=1 lists every id without deleting."
                            );
                                if dry_run {
                                    }
                            } else {
                                // #R57 bug/medium：**删除前打印完整清单**——opt-in 时 DELETE
                                // 永久销毁所有无 memories 行的向量行，但 add_vectors/
                                // put_stored_vector 接受任意 id、lookup_namespace 回退
                                // default、vector_search 不 join memories：外部注册向量是
                                // 合法数据态，阈值只限数量不限分类正确性。删除前列出
                                // **全部** id（#R69 bug/high：此前 LIMIT 5 样本在 >5 行时
                                // 无法审计"删了什么"）使操作可审计。
                                // #R69 bug/high：**dry-run 分支**——MEMORIA_ORPHAN_DRY_RUN=1
                                // 时打印完整 id 清单但跳过 DELETE 且不置完成 flag（下次
                                // 启动重新评估）；运维先审计清单、确认无误后再去掉
                                // dry-run 重启执行删除。
                                let audit: String = tx
                                .query_row(
                                    "SELECT COALESCE(group_concat(id, char(10)), '(none)') FROM (SELECT id FROM memory_hype_vectors \
                                     WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id))",
                                    [],
                                    |r| r.get(0),
                                )
                                .map_err(|e| format!("audit hype orphans: {e}"))?;
                                if dry_run {
                                    eprintln!(
                                    "[Memoria] DRY-RUN: {orphans_hype} orphan memory_hype_vectors rows would be deleted (nothing deleted; rerun without MEMORIA_ORPHAN_DRY_RUN to execute):\n{audit}"
                                );
                                    } else {
                                    eprintln!(
                                    "[Memoria] Migration: deleting {orphans_hype} orphan memory_hype_vectors rows:\n{audit}"
                                );
                                    const DEL_HYPE: &str = "DELETE FROM memory_hype_vectors \
                                    WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_hype_vectors.id)";
                                    tx.execute(DEL_HYPE, []).map_err(|e| {
                                    format!("clean memory_hype_vectors orphans: {e} [SQL: {DEL_HYPE}]")
                                })?;
                                }
                            }
                        }
                    }
                    // #R54 performance/medium：段完成（删除或拒绝）都置 HYPE_FLAG——
                    // 评估过即跳过后续启动的持锁重扫，force 覆盖重新评估。
                    // #R57 bug/low：hype 拒绝态**只由 HYPE_FLAG 记录**（不再写
                    // REFUSED_FLAG——该 flag 语义 = memory_vectors 段拒绝，混用会让
                    // vec 段干净完成也被标记拒绝、下次启动 WARN 误导归因）。
                    // #R69 bug/high：**dry-run 时不置 HYPE_FLAG**——置位会让后续启动
                    // 跳过评估（dry-run 语义 = 审计后待决定，不应固化"已处理"状态）。
                    // #R69 bug/medium：**dry_ran 门控不足**——表存在但空/无孤儿时
                    // dry_ran 保持 false、此写入在事务内执行、尾部 `if dry_run` 提前
                    // 返回把事务提交——审计运行仍固化 HYPE_FLAG（此后孤儿行出现时
                    // hype 段永久跳过、无重复 WARN）；与表缺失分支的 !dry_run 守卫
                    // 对称，统一 `!dry_run`（dry_ran 已删除——dry_run 单条件覆盖，
                    // 见 R44 maint/low 死代码清理）。
                    if !dry_run {
                        tx.execute(
                            "INSERT OR IGNORE INTO migration_flags (flag) VALUES (?1)",
                            rusqlite::params![HYPE_FLAG],
                        )
                        .map_err(|e| format!("set hype flag: {}", e))?;
                    }
                }
            }
            // ── memory_vectors 段 ──
            // #R48 bug/high（数据安全）：**孤儿 COUNT 必须有**——DELETE 无差别销毁
            // 所有"无 memories 行的向量行"，但代码库并不强制该不变量：add_vectors
            // （Python bindings，lib.rs:280）接受任意调用方 id、persist::lookup_namespace
            // 对无 memories 行回退 'default'、vector_search 不 join memories——外部注册
            // 向量/待重导入/合成 id 是合法数据态。
            // #R49 bug/high：memory_vectors 孤儿删除 **opt-in**——默认只报告不删除，
            // 设 MEMORIA_ORPHAN_CLEANUP_VECTORS=1 才删（仍受阈值与
            // MEMORIA_FORCE_ORPHAN_CLEANUP=1 约束）。
            // #R51 maintainability/medium：此前 refused（refused2）→ 本段跳过——
            // 非 force 时保持拒绝态（不再重扫评估）；force 时重新评估。
            // #R69 bug/medium：**dry-run 绕过 refused 短路**——refuse 运行已置
            // REFUSED_FLAG，不带 `&& !dry_run` 时审计请求只看到 "previously
            // refused; skipping" 而无清单（与 gate 处 #R69 同款问题）；dry-run
            // 强制重扫评估、打印完整清单（其分支不写 refused/CLEANUP flag）。
            if refused2 > 0 && !(force_cleanup && clean_vectors) && !dry_run {
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
                        // #R69 bug/high：WARN 文案**中性化**（同 hype 段）——不读作
                        // 操作指引；提示 dry-run 审计通道（完整 id 清单）。
                        if !clean_vectors {
                            eprintln!(
                                "[Memoria] WARN: {orphans_vec} orphan memory_vectors rows (no matching memories row; may be legit external vectors via add_vectors). Cleanup is opt-in and disabled by default. To inspect before any decision, restart with MEMORIA_ORPHAN_DRY_RUN=1 to list every id (nothing will be deleted)."
                            );
                            // #R69 bug/high：dry-run 时**不置 refused**——审计预览
                            // 不固化"已拒绝"状态（否则后续启动跳过评估，运维无法
                            if dry_run {
                            } else {
                                refused = true;
                            }
                        } else if orphans_vec > ORPHAN_REFUSE_THRESHOLD && !force_cleanup {
                            eprintln!(
                                "[Memoria] WARN: {orphans_vec} orphan memory_vectors rows ABOVE safety threshold {ORPHAN_REFUSE_THRESHOLD}; cleanup refused (count too large to be safely attributable to external writes; inspect with MEMORIA_ORPHAN_DRY_RUN=1)"
                            );
                            if dry_run {
                            } else {
                                refused = true;
                            }
                        } else if !force_cleanup {
                            // #R60 bug/high：单 OPT_IN 只报告不删（同 hype 段理由）。
                            eprintln!(
                                "[Memoria] WARN: {orphans_vec} orphan memory_vectors rows (no matching memories row; may be legit external vectors via add_vectors); not deleting. MEMORIA_ORPHAN_DRY_RUN=1 lists every id without deleting."
                            );
                            if dry_run {
                            } else {
                                refused = true;
                            }
                        } else {
                            // #R57 bug/medium：删除前打印**完整清单**（同 hype 段——
                            // opt-in 时 ≤5000 的合法外部向量也会被永久销毁，清单
                            // 使操作可审计；此前 LIMIT 5 样本在 >5 行时无法审计）。
                            // #R69 bug/high：dry-run 分支（打印清单、跳过 DELETE、
                            // 不置完成 flag，下次启动重新评估）。
                            let audit: String = tx
                                .query_row(
                                    "SELECT COALESCE(group_concat(id, char(10)), '(none)') FROM (SELECT id FROM memory_vectors \
                                     WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id))",
                                    [],
                                    |r| r.get(0),
                                )
                                .map_err(|e| format!("audit vec orphans: {e}"))?;
                            if dry_run {
                                eprintln!(
                                    "[Memoria] DRY-RUN: {orphans_vec} orphan memory_vectors rows would be deleted (nothing deleted; rerun without MEMORIA_ORPHAN_DRY_RUN to execute):\n{audit}"
                                );
                            } else {
                                eprintln!(
                                    "[Memoria] Migration: deleting {orphans_vec} orphan memory_vectors rows:\n{audit}"
                                );
                                const DEL_VEC: &str = "DELETE FROM memory_vectors \
                                    WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = memory_vectors.id)";
                                tx.execute(DEL_VEC, []).map_err(|e| {
                                    format!("clean memory_vectors orphans: {e} [SQL: {DEL_VEC}]")
                                })?;
                            }
                        }
                    }
                }
            }
            // #R69 bug/high：**dry-run 时不写任何 flag**——HYPE_FLAG（hype 段）
            // 与 REFUSED/CLEANUP（vec 段）都跳过：dry-run 是审计预览，不固化
            // "已处理/已拒绝"状态，下次启动重新评估（去掉 dry-run 后执行删除
            // 才会置位）。直接提交空事务。
            // #R69 bug/medium：**dry-run 无条件提前返回**——此前以 `dry_ran` 门控：
            // 两表均无孤儿时 dry_ran 保持 false、落入尾部 flag 块写入 CLEANUP_FLAG
            // 并清 REFUSED_FLAG——审计运行仍改持久状态，违反 #R69 契约。
            if dry_run {
                commit_tx(tx, "commit cleanup tx (dry-run)")?;
                return Ok(());
            }
            // #R69 maintainability/low：**删除 dry_ran 死代码**——dry_ran 只在
            // dry_run 分支置位，而上方 `if dry_run` 无条件提前返回先于此处，
            // 此分支不可达（R44 指出：保留为"防御性"会模糊 flag 写入纪律）。
            // 置位标记（事务内，提交后对并发进程可见）：正常完成（含"无孤儿"）→
            // CLEANUP_FLAG；阈值/opt-in 拒绝（refused）→ REFUSED_FLAG——持久化拒绝态
            // 使后续启动跳过 memory_vectors 段评估（不再持写锁重扫相关扫描，
            // #R49 performance/medium；hype 段由 HYPE_FLAG 独立跳过）。
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
                // #R65 bug/medium：**拒绝时清 CLEANUP_FLAG**——升级路径（存量库已
                // 有 CLEANUP_FLAG、gate 经 !hype_done 进入）上 vec 段拒绝会双 flag
                // 并存：下次启动 gate `!already && !refused_done` 恒 false，删除
                // refused 行的逃生通道失效（只剩双 env force）；两 flag 互斥，
                // refused 记录 = 重新评估机会（#R49/#R55 语义）。
                tx.execute(
                    "DELETE FROM migration_flags WHERE flag = ?1",
                    rusqlite::params![CLEANUP_FLAG],
                )
                .map_err(|e| format!("clear cleanup flag on refuse: {}", e))?;
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
            // #R58 maintainability/low：共享 commit_tx（Drop 自动回滚，见 helper doc）。
            commit_tx(tx, "commit cleanup tx")?;
            Ok(())
        })();
        if let Err(e) = cleanup {
            // #R48 bug/medium：清理失败不阻断启动（软降级，flag 未置位 → 下次重试）。
            eprintln!("[Memoria] WARN: orphan cleanup skipped (will retry next start): {e}");
        }
    }
    Ok(())
}
