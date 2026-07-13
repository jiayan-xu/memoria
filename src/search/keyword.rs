//! FTS5 keyword search signal (S1).
//! Searches memories_fts, messages_fts, and decisions_fts via jieba-rs tokenization.

use crate::storage::{SqlitePool, fts5};

/// A single search result from any signal.
#[derive(Debug, Clone)]
pub struct SignalResult {
    pub memory_id: String,
    pub content: String,
    pub score: f64,
    pub source: String,
}

/// Keyword signal: search all FTS5 tables and return ranked results.
pub fn keyword_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    limit: u32,
) -> Result<Vec<SignalResult>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let tokens = fts5::tokenize(query);
    if tokens.is_empty() {
        return Ok(vec![]);
    }

    let mut results = Vec::new();

    // 1. Search memories_fts
    let mem_sql = "
        SELECT m.rowid, m.id, m.content, f.rank
        FROM memories_fts f
        JOIN memories m ON f.rowid = m.rowid
        WHERE memories_fts MATCH ? AND m.namespace = ?
        ORDER BY f.rank
        LIMIT ?";
    if let Ok(mut stmt) = conn.prepare(mem_sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![tokens, namespace, limit], |row| {
            Ok(SignalResult {
                memory_id: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
                score: row.get::<_, f64>(3)?,
                source: "fts5_keyword".to_string(),
            })
        }) {
            for row in rows.flatten() {
                results.push(row);
            }
        }
    }

    // 2. FTS5 fallback if no results (LIKE query, with ESCAPE for %/_)
    if results.is_empty() {
        let like_sql = "SELECT rowid, id, content FROM memories WHERE content LIKE ? ESCAPE '\\' AND namespace = ? LIMIT ?";
        let escaped = query.replace('%', "\\%").replace('_', "\\_");
        let like_q = format!("%{}%", escaped);
        if let Ok(mut stmt) = conn.prepare(like_sql) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![like_q, namespace, limit], |row| {
                Ok(SignalResult {
                    memory_id: row.get::<_, String>(1)?,
                    content: row.get::<_, String>(2)?,
                    score: 0.5,
                    source: "like_fallback".to_string(),
                })
            }) {
                for row in rows.flatten() {
                    results.push(row);
                }
            }
        }
    }

    Ok(results)
}
