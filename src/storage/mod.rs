pub mod fts5;
pub mod models;
pub mod sqlite;

/// HyPE 假设问句向量表名（#R69 maintainability/low：**单一事实源**——lib.rs
/// query_hype_count_cached 与 mcp_server db_stats 各自内嵌同一字面量曾构成
/// 第四处独立查询点：表 rename/迁移后 MCP 侧静默翻转为 -1 而 lib.rs 仍工作，
/// sentinel 语义漂移。表名收敛为共享常量，改表名只动一处）。
pub const MEMORY_HYPE_VECTORS_TABLE: &str = "memory_hype_vectors";

pub use sqlite::{
    SqlitePool, create_pool, init_core_tables, init_schema, migrate_dream_state_ns,
    migrate_event_time, migrate_evolution, migrate_extract_fields, migrate_memory_relation_types,
    migrate_superseded_by, migrate_temporal, migrate_user_prefs_namespace, wal_checkpoint,
};
