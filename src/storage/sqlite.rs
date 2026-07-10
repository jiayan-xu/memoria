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
         PRAGMA busy_timeout=5000;"
    ).map_err(|e| format!("pragma: {}", e))?;
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
            tags TEXT DEFAULT '[]'
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
            updated_at TEXT
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

        -- Dream state (decay/consolidation tracker)
        CREATE TABLE IF NOT EXISTS dream_state (
            phase TEXT PRIMARY KEY,
            last_run TEXT,
            sessions_processed INTEGER DEFAULT 0
        );

        -- Memory relations (edges between memories)
        CREATE TABLE IF NOT EXISTS memory_relations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            namespace TEXT NOT NULL DEFAULT 'default',
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            relation_type TEXT NOT NULL CHECK(relation_type IN ('same_entity','chronological','semantic_related')),
            weight REAL DEFAULT 0.5,
            evidence TEXT,
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_rel_source ON memory_relations(source_id);
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
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN superseded_by TEXT;",
        )
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
