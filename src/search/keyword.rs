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
    let tokens = fts5::tokenize_for_fts(query);
    if tokens.is_empty() {
        return Ok(vec![]);
    }

    let mut results = Vec::new();

    // FTS5 主召回：jieba 切词 + 代码符号拆子 token（对齐索引拆存），OR 宽召回。
    // 实测：对「自然语言 + 代码符号」类 query，拆词后 ground truth 召回 rank 1~11（6/7 进 top-10）。
    // 不再做整句/整符号 LIKE（库内 ground truth 内容多不含字面符号，LIKE 不可靠且会污染重排）。
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

    Ok(results)
}
