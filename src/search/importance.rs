//! Importance signal (S4) and Category match signal (S5).

use super::keyword::SignalResult;
use crate::storage::SqlitePool;

/// Importance signal: rank by (importance × decay_factor / 5).
pub fn importance_search(
    pool: &SqlitePool,
    namespace: &str,
    limit: u32,
) -> Result<Vec<SignalResult>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    let sql = "
        SELECT rowid, id, content, importance, decay_factor, tier, confidence, category
        FROM memories
        WHERE tier IN ('hot','warm') AND namespace = ?
        ORDER BY (importance * decay_factor) DESC
        LIMIT ?";

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
    let rows = stmt
        .query_map(rusqlite::params![namespace, limit], |row| {
            let importance: f64 = row.get::<_, f64>(3).unwrap_or(3.0);
            let decay: f64 = row.get::<_, f64>(4).unwrap_or(0.5);
            let imp_score = importance * decay / 5.0;

            Ok(SignalResult {
                memory_id: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
                score: imp_score,
                source: "importance_signal".to_string(),
            })
        })
        .map_err(|e| format!("query: {}", e))?;

    let mut results: Vec<SignalResult> = rows.flatten().collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(results)
}

/// Category match signal: match query against known categories.
pub fn category_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    limit: u32,
) -> Result<Vec<SignalResult>, String> {
    let categories = [
        ("decision", "decision"),
        ("preference", "preference"),
        ("constraint", "constraint"),
        ("lesson", "lesson"),
        ("fact", "fact"),
        ("candidate", "candidate"),
        ("决定", "decision"),
        ("偏好", "preference"),
        ("约束", "constraint"),
        ("教训", "lesson"),
        ("事实", "fact"),
    ];

    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut results = Vec::new();

    for (keyword, category) in &categories {
        if query.contains(keyword) {
            let sql = "SELECT rowid, id, content FROM memories WHERE category = ? AND tier IN ('hot','warm') AND namespace = ? LIMIT ?";
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) =
                    stmt.query_map(rusqlite::params![category, namespace, limit], |row| {
                        Ok(SignalResult {
                            memory_id: row.get::<_, String>(1)?,
                            content: row.get::<_, String>(2)?,
                            score: 1.0,
                            source: "category_match".to_string(),
                        })
                    })
                {
                    for row in rows.flatten() {
                        results.push(row);
                    }
                }
            }
        }
    }

    Ok(results)
}
