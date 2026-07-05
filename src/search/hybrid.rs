//! 统一搜索入口 — 整合 5 信号（keyword + semantic + temporal + importance + category）
//!
//! 替代 lib.rs hybrid_search 和 mcp_server.rs 中各自维护的搜索逻辑。

use crate::search::{self, SignalResult, FusedResult};
use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache};

/// 执行 5 信号融合搜索，返回 RRF 排序结果
pub fn hybrid_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    max_results: u32,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
) -> Result<Vec<FusedResult>, String> {
    let fts_limit = max_results * 3;
    let mut signals: Vec<Vec<SignalResult>> = Vec::new();
    let mut weights: Vec<f64> = Vec::new();

    // S1: Keyword (FTS5 + LIKE)
    if let Ok(kw) = search::keyword::keyword_search(pool, query, namespace, fts_limit) {
        if !kw.is_empty() { signals.push(kw); weights.push(1.0); }
    }

    // S2: Semantic (HNSW vector)
    if let (Some(hnsw), Some(qc)) = (hnsw, query_cache) {
        if let Ok(sem) = search::semantic::semantic_search(query, namespace, fts_limit, Some(hnsw), Some(qc)) {
            if !sem.is_empty() { signals.push(sem); weights.push(1.0); }
        }
    }

    // S3: Temporal (recency bias)
    if let Ok(temp) = search::temporal::temporal_search(pool, namespace, fts_limit) {
        if !temp.is_empty() { signals.push(temp); weights.push(1.0); }
    }

    // S4: Importance (recall count + decay)
    if let Ok(imp) = search::importance::importance_search(pool, namespace, fts_limit) {
        if !imp.is_empty() { signals.push(imp); weights.push(1.0); }
    }

    // S5: Category (query intent match)
    if let Ok(cat) = search::importance::category_search(pool, query, namespace, max_results) {
        if !cat.is_empty() { signals.push(cat); weights.push(0.5); }
    }

    let mut fused = if signals.is_empty() {
        Vec::new()
    } else {
        search::rrf::rrf_merge(&signals, &weights, 60.0)
    };

    // 2-hop graph expansion
    if let Ok(expanded) = search::rrf::graph_expand(pool, &fused, 2, namespace) {
        fused.extend(expanded);
    }

    // Dedup by memory_id
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<FusedResult> = fused.into_iter()
        .filter(|r| seen.insert(r.memory_id.clone()))
        .take(max_results as usize)
        .collect();

    Ok(unique)
}
