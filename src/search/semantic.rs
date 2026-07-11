//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use crate::QueryCache;
use std::collections::{HashMap, HashSet};

/// Semantic search via HNSW vector similarity.
/// Python must call cache_query_vector() first to provide the query embedding.
///
/// `pool` 用于按调用者 namespace 回查 `memories` 表，过滤 HNSW 全局索引返回的跨租户记忆
/// （B2 修复：HNSW 无 namespace 维度，原实现完全忽略 ns 导致跨租户记忆泄露）。
pub fn semantic_search(
    query: &str,
    namespace: &str,
    limit: u32,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    pool: Option<&SqlitePool>,
) -> Result<Vec<SignalResult>, String> {
    let hnsw = match hnsw {
        Some(h) => h,
        None => return Ok(vec![]),
    };

    let cache = match query_cache {
        Some(c) => c,
        None => return Ok(vec![]),
    };

    // Get cached embedding from Python (must have been cached via cache_query_vector)
    let vector = match cache.get(query) {
        Some(v) => v,
        None => return Ok(vec![]),  // No cached embedding — skip semantic signal
    };

    // Search HNSW index
    let results = hnsw.search(&vector, limit as usize)?;
    if results.is_empty() {
        return Ok(vec![]);
    }

    // HNSW 是全局索引，无 namespace 维度。按调用者 ns 回查 memories 表，
    // 仅保留归属当前 ns 的记忆，杜绝跨租户泄露。无 pool 时无法过滤，保守返回空。
    let allowed: HashSet<String> = match pool {
        Some(p) => match lookup_namespaces(p, &results) {
            Ok(map) => map
                .into_iter()
                .filter(|(_, ns)| ns == namespace)
                .map(|(id, _)| id)
                .collect(),
            Err(_) => return Ok(vec![]),
        },
        None => return Ok(vec![]),
    };

    let mut out = Vec::with_capacity(allowed.len());
    for (memory_id, distance) in results {
        let score = 1.0 - distance; // Convert cosine distance to similarity
        if score > 0.0 && allowed.contains(&memory_id) {
            out.push(SignalResult {
                memory_id,
                content: String::new(),
                score: score as f64,
                source: "hnsw_semantic".to_string(),
            });
        }
    }
    Ok(out)
}

/// 批量回查 memory_id 的 namespace（单条 IN 查询，避免 N+1）。
fn lookup_namespaces(
    pool: &SqlitePool,
    results: &[(String, f32)],
) -> Result<HashMap<String, String>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let ids: Vec<&String> = results.iter().map(|(id, _)| id).collect();
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, namespace FROM memories WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {}", e))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter().map(|s| *s)), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query: {}", e))?;
    let mut map = HashMap::new();
    for row in rows.flatten() {
        map.insert(row.0, row.1);
    }
    Ok(map)
}
