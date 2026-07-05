//! Rust memory_graph — 记忆关系边自动构建。
//! Phase 3: 基于 content 相似性和时间顺序建立关系边。

use crate::storage::SqlitePool;

/// Build relation edges between memories in a namespace.
/// Uses content hash prefix matching and temporal proximity.
/// Returns (same_entity, chronological, semantic) counts.
pub fn build_graph(
    pool: &SqlitePool,
    namespace: &str,
    batch_size: u32,
) -> Result<(u32, u32, u32), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    // Get candidate memories
    let mut stmt = conn.prepare(
        "SELECT id, content, created_at FROM memories WHERE namespace = ? AND tier IN ('hot','warm') ORDER BY created_at DESC LIMIT ?"
    ).map_err(|e| format!("prepare: {}", e))?;

    let rows = stmt.query_map(rusqlite::params![namespace, batch_size], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    }).map_err(|e| format!("query: {}", e))?;

    let candidates: Vec<(String, String, Option<String>)> = rows.flatten().collect();
    let mut same_entity = 0u32;
    let mut chronological = 0u32;
    let mut _semantic = 0u32;

    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            let (id_a, content_a, _) = &candidates[i];
            let (id_b, content_b, _) = &candidates[j];

            // Same entity: content has matching key phrases (skip generic prefixes)
            if content_a.len() > 20 && content_b.len() > 20 {
                // Use the first non-generic 15 characters
                let a_prefix: String = content_a.chars().skip_while(|c| *c == '[' || *c == '(').take(15).collect();
                let b_prefix: String = content_b.chars().skip_while(|c| *c == '[' || *c == '(').take(15).collect();
                if a_prefix == b_prefix {
                    conn.execute(
                        "INSERT OR IGNORE INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence, created_at)
                         VALUES (?, ?, ?, 'same_entity', 0.8, 'auto_graph', ?)",
                        rusqlite::params![namespace, id_a, id_b, now],
                    ).ok();
                    same_entity += 1;
                }
            }

            // Chronological: same day creation
            if let (Some(da), Some(db)) = (&candidates[i].2, &candidates[j].2) {
                if da.len() >= 10 && db.len() >= 10 && &da[..10] == &db[..10] {
                    conn.execute(
                        "INSERT OR IGNORE INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence, created_at)
                         VALUES (?, ?, ?, 'chronological', 0.5, 'auto_graph', ?)",
                        rusqlite::params![namespace, id_a, id_b, now],
                    ).ok();
                    chronological += 1;
                }
            }
        }
    }

    Ok((same_entity, chronological, _semantic))
}
