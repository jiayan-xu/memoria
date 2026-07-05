//! RRF (Reciprocal Rank Fusion) merger + 2-hop graph expansion.
//!
//! score(item) = sum( w_m / (K + rank_m) ) for m in {keyword, semantic, temporal, importance, category}

use std::collections::HashMap;
use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;

/// RRF weights (default, can be overridden by intent).
pub struct RrfWeights {
    pub keyword: f64,
    pub semantic: f64,
    pub temporal: f64,
    pub importance: f64,
    pub category: f64,
    pub k: f64,
}

impl Default for RrfWeights {
    fn default() -> Self {
        Self {
            keyword: 1.0,
            semantic: 1.0,
            temporal: 1.0,
            importance: 1.0,
            category: 0.5,
            k: 60.0,
        }
    }
}

/// A fused result after RRF merge.
#[derive(Debug, Clone)]
pub struct FusedResult {
    pub memory_id: String,
    pub content: String,
    pub rrf_score: f64,
    pub source: String,
    pub signal_scores: Vec<(String, f64)>,
}

/// Merge multiple ranked signal lists using RRF.
pub fn rrf_merge(
    signals: &[Vec<SignalResult>],
    weights: &[f64],
    k: f64,
) -> Vec<FusedResult> {
    let mut score_map: HashMap<String, (f64, String, String)> = HashMap::new();

    for (signal_idx, results) in signals.iter().enumerate() {
        let weight = weights.get(signal_idx).copied().unwrap_or(1.0);
        for (rank, result) in results.iter().enumerate() {
            let rrf = weight / (k + rank as f64 + 1.0);
            let entry = score_map.entry(result.memory_id.clone());
            let (current_score, _, _) = entry.or_insert((0.0, result.content.clone(), result.source.clone()));
            *current_score += rrf;
        }
    }

    let mut fused: Vec<FusedResult> = score_map.into_iter().map(|(memory_id, (rrf_score, content, source))| {
        FusedResult {
            memory_id,
            content,
            rrf_score,
            source,
            signal_scores: vec![],
        }
    }).collect();

    fused.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));
    fused
}

/// 2-hop graph expansion via memory_relations table.
pub fn graph_expand(
    pool: &SqlitePool,
    results: &[FusedResult],
    _max_hops: u32,
    namespace: &str,
) -> Result<Vec<FusedResult>, String> {
    if results.is_empty() {
        return Ok(vec![]);
    }

    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut expanded = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = results.iter()
        .map(|r| r.memory_id.clone()).collect();

    for result in results.iter().take(5) {
        // Bidirectional graph expansion with content from memories table
        let hop_sql = "
            SELECT r.neighbor_id, r.weight, r.relation_type, m.content
            FROM (
                SELECT target_id AS neighbor_id, weight, relation_type
                FROM memory_relations WHERE source_id = ? AND namespace = ?
                UNION
                SELECT source_id AS neighbor_id, weight, relation_type
                FROM memory_relations WHERE target_id = ? AND namespace = ?
            ) r
            LEFT JOIN memories m ON r.neighbor_id = m.id
            LIMIT 10";
        if let Ok(mut stmt) = conn.prepare(hop_sql) {
                if let Ok(rows) = stmt.query_map(rusqlite::params![result.memory_id, namespace, result.memory_id, namespace], |row| {
                let target_id: String = row.get(0)?;
                let weight: f64 = row.get(1)?;
                let rel_type: String = row.get(2)?;
                let content: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                Ok((target_id, weight, rel_type, content))
            }) {
                for row in rows.flatten() {
                    let (target_id, weight, _rel_type, content) = row;
                    if seen_ids.insert(target_id.clone()) {
                        expanded.push(FusedResult {
                            memory_id: target_id,
                            content,
                            rrf_score: result.rrf_score * 0.5 * weight,
                            source: format!("graph_expand_{}", _rel_type),
                            signal_scores: vec![],
                        });
                    }
                }
            }
        }
    }

    Ok(expanded)
}
