//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::QueryCache;
use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
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
        None => return Ok(vec![]), // No cached embedding — skip semantic signal
    };

    // Search HNSW index.
    // 关键修复（2026-07-26）：HNSW 是全局索引、无 namespace 维度（语义检索 B2 修复说明）。
    // 若直接按 limit(=primary_limit) 取「全局 top-k」再按 ns 过滤，跨 ns 向量会占满名额，
    // 导致目标 ns 内排名靠前（但全局排名靠后）的 gold 被砍掉 → 语义漏召（实测 ~67%）。
    // 故先按远大于 limit 的窗口过取全局候选，再按 ns 过滤，保证目标 ns 拿到足够语义候选。
    let cap = hnsw.len();
    let overfetch = (limit as usize)
        .saturating_mul(20)
        .max(2048)
        .min(cap.max(1));
    let results = hnsw.search_with_ef(&vector, overfetch, overfetch)?;
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
    if allowed.is_empty() {
        return Ok(out);
    }

    // P3-0 修复：语义结果此前 content 恒为空（只带 memory_id），
    // 经 rrf_merge 首次插入即锁定空正文，导致「仅被语义命中」的记忆在 fusion 后丢失正文，
    // benchmark 拼上下文时得不到内容、答案必错。此处按 allowed id 批量回取 content 补齐。
    let mut contents: HashMap<String, String> = HashMap::new();
    if let Some(p) = pool {
        let ids: Vec<&String> = allowed.iter().collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, content FROM memories WHERE id IN ({})",
            placeholders
        );
        if let Ok(conn) = p.get() {
            if let Ok(mut stmt) = conn.prepare(&sql) {
                if let Ok(rows) = stmt.query_map(
                    rusqlite::params_from_iter(ids.iter().map(|s| *s)),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                ) {
                    for r in rows.flatten() {
                        contents.insert(r.0, r.1);
                    }
                }
            }
        }
    }

    for (memory_id, distance) in results {
        let score = 1.0 - distance; // Convert cosine distance to similarity
        // P0 防御：丢弃非有限 / 非正的分数。零向量在 DistCosine 下 distance≈0 → score≈1.0，
        // 会伪造「完美匹配」污染召回；add() 已拦截退化向量入索引，此处为双保险。
        if score.is_finite() && score > 0.0 && allowed.contains(&memory_id) {
            let content = contents.get(&memory_id).cloned().unwrap_or_default();
            out.push(SignalResult {
                memory_id,
                content,
                score: score as f64,
                source: "hnsw_semantic".to_string(),
            });
        }
    }
    // 恢复「每通道贡献 limit 条」设计：过取后按 ns 过滤，再截断到本 ns 内 top-limit，
    // 避免把上千条跨 ns 候选灌进融合（既保平衡，又确保 gold 在正确的本 ns top 内）。
    out.truncate(limit as usize);
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
