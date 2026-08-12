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
///
/// `hype_hnsw`（V1 2026-08-12）：HyPE 假设问句索引（可选）。传入时双路搜索——同一 query
/// 向量分别匹配「内容向量」（hnsw）与「问句向量」（hype_hnsw），按 memory_id 取两路
/// cosine 最大值合并。问句向量与 query 同为「问句式」表达，缩小措辞 gap（IEEE Access
/// 2025 HyPE）。两路结果分别过 ns 过滤后合并；任一索引缺失则退化为单路。
pub fn semantic_search(
    query: &str,
    namespace: &str,
    limit: u32,
    hnsw: Option<&HnswIndex>,
    hype_hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    pool: Option<&SqlitePool>,
) -> Result<Vec<SignalResult>, String> {
    let cache = match query_cache {
        Some(c) => c,
        None => return Ok(vec![]),
    };

    // Get cached embedding from Python (must have been cached via cache_query_vector)
    let vector = match cache.get(query) {
        Some(v) => v,
        None => return Ok(vec![]), // No cached embedding — skip semantic signal
    };

    // P0 防御：查询向量退化（NaN / 全零）则无法产生有效语义信号，提前返回空集。
    // 全零向量在 DistCosine 下与任意记忆 distance≈0 → score≈1.0，会灌入 limit 条垃圾；
    // NaN 则 score 非有限。add() 已拦退化向量入索引，此处对「查询向量」做双保险。
    let norm_sq: f32 = vector.iter().map(|&x| x * x).sum();
    if !norm_sq.is_finite() || norm_sq <= 0.0 {
        return Ok(vec![]);
    }

    // 收集两路候选：content 路（hnsw）+ hype 路（hype_hnsw），按 memory_id 合并取 max。
    // HNSW 是全局索引、无 namespace 维度（语义检索 B2 修复说明）：
    // 先按远大于 limit 的窗口过取全局候选，再按 ns 过滤，保证目标 ns 拿到足够语义候选。
    let mut best: HashMap<String, f64> = HashMap::new(); // memory_id -> max cosine
    if let Some(h) = hnsw {
        let cap = h.len();
        let overfetch = (limit as usize)
            .saturating_mul(20)
            .max(2048)
            .min(cap.max(1));
        if let Ok(results) = h.search_with_ef(&vector, overfetch, overfetch) {
            for (memory_id, distance) in results {
                let score = 1.0 - distance as f64;
                if score.is_finite() && score > 0.0 {
                    let e = best.entry(memory_id).or_insert(0.0);
                    if score > *e {
                        *e = score;
                    }
                }
            }
        }
    }
    if let Some(h) = hype_hnsw {
        let cap = h.len();
        let overfetch = (limit as usize)
            .saturating_mul(20)
            .max(2048)
            .min(cap.max(1));
        if let Ok(results) = h.search_with_ef(&vector, overfetch, overfetch) {
            for (memory_id, distance) in results {
                let score = 1.0 - distance as f64;
                if score.is_finite() && score > 0.0 {
                    let e = best.entry(memory_id).or_insert(0.0);
                    if score > *e {
                        *e = score;
                    }
                }
            }
        }
    }
    if best.is_empty() {
        return Ok(vec![]);
    }

    // HNSW 是全局索引，无 namespace 维度。按调用者 ns 回查 memories 表，
    // 仅保留归属当前 ns 的记忆，杜绝跨租户泄露。无 pool 时无法过滤，保守返回空。
    let ids: Vec<&String> = best.keys().collect();
    let allowed: HashSet<String> = match pool {
        Some(p) => match lookup_namespaces(p, &ids) {
            Ok(map) => map
                .into_iter()
                .filter(|(_, ns)| ns == namespace)
                .map(|(id, _)| id)
                .collect(),
            Err(_) => return Ok(vec![]),
        },
        None => return Ok(vec![]),
    };
    if allowed.is_empty() {
        return Ok(vec![]);
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

    let mut out: Vec<SignalResult> = Vec::with_capacity(allowed.len());
    for memory_id in &allowed {
        if let Some(score) = best.get(memory_id) {
            let content = contents.get(memory_id).cloned().unwrap_or_default();
            out.push(SignalResult {
                memory_id: memory_id.clone(),
                content,
                score: *score,
                source: "hnsw_semantic".to_string(),
            });
        }
    }
    // 按合并后分数降序（保持原语义：语义通道按相似度排序进入融合）。
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    // 恢复「每通道贡献 limit 条」设计：过取后按 ns 过滤，再截断到本 ns 内 top-limit，
    // 避免把上千条跨 ns 候选灌进融合（既保平衡，又确保 gold 在正确的本 ns top 内）。
    out.truncate(limit as usize);
    Ok(out)
}

/// 批量回查 memory_id 的 namespace（单条 IN 查询，避免 N+1）。
fn lookup_namespaces(
    pool: &SqlitePool,
    results: &[&String],
) -> Result<HashMap<String, String>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let ids: Vec<&String> = results.iter().map(|s| *s).collect();
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
