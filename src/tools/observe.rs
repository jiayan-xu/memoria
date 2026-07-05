//! Rust memory_observe implementation.
//! Content is stored as-is (no prefix), matching Python side.

use crate::storage::SqlitePool;
use uuid::Uuid;

pub fn observe(
    pool: &SqlitePool,
    dialog: &str,
    _role: &str,
    source: &str,
    _session_id: &str,
    namespace: &str,
) -> Result<String, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mem_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    conn.execute(
        "INSERT INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor)
         VALUES (?, ?, ?, ?, 'observation', 0.5, 0, ?, 'warm', 2, 1.0)",
        rusqlite::params![mem_id, namespace, source, dialog, now],
    ).map_err(|e| format!("insert: {}", e))?;

    Ok(mem_id)
}
