//! RRF (Reciprocal Rank Fusion) merger + 2-hop graph expansion.
//!
//! score(item) = sum( w_m / (K + rank_m) ) for m in {keyword, semantic, temporal, importance, category}

use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use std::collections::HashMap;

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
    /// A（两阶段重排）：语义通道原始 cosine 相似度（0..1，越大越相关）。
    pub sem_cos: Option<f64>,
    /// A（两阶段重排）：关键词通道原始 BM25 幅度（FTS rank 取反，越大越相关）。
    pub kw_bm25: Option<f64>,
    /// PR4（Phase A 演化）：最近演化时间戳；None=待演化/脏标记。
    pub evolved_at: Option<String>,
    /// PR4：该 tip 是否尚未演化（evolved_at IS NULL）。recall 可据此降权/标注。
    pub pending_evolution: bool,
    /// F3（可解释性）：主导匹配通道（signal_scores 中幅度最大的通道，复用 channel_of 归一）。
    pub primary_channel: Option<String>,
    /// F3：各通道归一化 RRF 幅度（通道名 -> 幅度），与 signal_scores 同源。
    pub channel_scores: HashMap<String, f64>,
    /// F1b：召回命中计数（来自 memories.access_count），供跨通道频率加权。
    pub access_count: i64,
    /// F1b：最近召回时间（来自 memories.last_recalled），供新鲜度加权。
    pub last_recalled: Option<String>,
    /// F2：时间有效性状态（current / superseded / expired），None=未标注。
    pub time_status: Option<String>,
}

/// RRF 融合过程中累加的原始信号幅度（供两阶段重排使用）。
struct Agg {
    rrf: f64,
    content: String,
    source: String,
    sigs: Vec<(String, f64)>,
    sem: Option<f64>,
    kw: Option<f64>,
}

/// Merge multiple ranked signal lists using RRF.
pub fn rrf_merge(signals: &[Vec<SignalResult>], weights: &[f64], k: f64) -> Vec<FusedResult> {
    let mut score_map: HashMap<String, Agg> = HashMap::new();

    for (signal_idx, results) in signals.iter().enumerate() {
        let weight = weights.get(signal_idx).copied().unwrap_or(1.0);
        for (rank, result) in results.iter().enumerate() {
            let rrf = weight / (k + rank as f64 + 1.0);
            let ch = channel_of(&result.source);
            let entry = score_map.entry(result.memory_id.clone()).or_insert_with(|| Agg {
                rrf: 0.0,
                content: result.content.clone(),
                source: result.source.clone(),
                sigs: Vec::new(),
                sem: None,
                kw: None,
            });
            entry.rrf += rrf;
            // 记录贡献通道（粗粒度），供评测的通道贡献度量使用
            if let Some(existing) = entry.sigs.iter_mut().find(|(n, _)| n == &ch) {
                existing.1 += rrf;
            } else {
                entry.sigs.push((ch.clone(), rrf));
            }
            // 捕获原始幅度（用于 A 两阶段重排）：sem=cosine、kw=BM25 幅度
            match ch.as_str() {
                "semantic" => {
                    if entry.sem.is_none() || result.score > entry.sem.unwrap() {
                        entry.sem = Some(result.score);
                    }
                }
                "keyword" => {
                    let mag = -result.score; // FTS rank 为负，取反得正幅度
                    if entry.kw.is_none() || mag > entry.kw.unwrap() {
                        entry.kw = Some(mag);
                    }
                }
                _ => {}
            }
        }
    }

    let mut fused: Vec<FusedResult> = score_map
        .into_iter()
        .map(|(memory_id, a)| {
            let sigs = a.sigs;
            let primary_channel = sigs
                .iter()
                .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(n, _)| n.clone());
            let channel_scores: std::collections::HashMap<String, f64> =
                sigs.iter().cloned().collect();
            FusedResult {
                memory_id,
                content: a.content,
                rrf_score: a.rrf,
                source: a.source,
                signal_scores: sigs,
                sem_cos: a.sem,
                kw_bm25: a.kw,
                evolved_at: None,
                pending_evolution: false,
                primary_channel,
                channel_scores,
                access_count: 0,
                last_recalled: None,
                time_status: None,
            }
        })
        .collect();

    fused.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fused
}

/// 将信号 source 映射为粗粒度通道名，用于通道贡献度量。
fn channel_of(source: &str) -> String {
    if source.contains("keyword") || source.contains("like") {
        "keyword".to_string()
    } else if source.contains("semantic") {
        "semantic".to_string()
    } else if source.contains("temporal") {
        "temporal".to_string()
    } else if source.contains("importance") {
        "importance".to_string()
    } else if source.contains("category") {
        "category".to_string()
    } else {
        source.to_string()
    }
}

/// 2-hop（可配置 max_hops）graph expansion via memory_relations table。
///
/// 真 BFS：以 top-N 融合结果为种子，逐跳沿 memory_relations 双向扩展至 max_hops 跳。
/// 计分：hop-h 邻居 = `seed_rrf_max * decay^h * max(weight, 0.1)`，使图邻居以足够分数进入
/// rerank 候选池（最终排序由 cross-encoder reranker 决定，图扩展只负责「拉入」而非「排序」）。
///
/// 开关（均可选 env 覆盖）：
/// - MEMORIA_GRAPH_HOPS : 覆盖传入 max_hops（0=关闭图扩展，退化为纯向量/关键词召回）。
/// - MEMORIA_GRAPH_SEED : 种子数（默认 10，原硬编码 5）。
/// - MEMORIA_GRAPH_DECAY: 每跳衰减（默认 0.5）。
/// - MEMORIA_GRAPH_FANOUT: 每个节点的邻居上限（默认 10）。
pub fn graph_expand(
    pool: &SqlitePool,
    results: &[FusedResult],
    max_hops: u32,
    namespace: &str,
) -> Result<Vec<FusedResult>, String> {
    if results.is_empty() {
        return Ok(vec![]);
    }
    // MEMORIA_GRAPH_HOPS 覆盖传入 max_hops（0=关闭图扩展）
    let cfg_hops = std::env::var("MEMORIA_GRAPH_HOPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    let max_hops = cfg_hops.unwrap_or(max_hops).min(3);
    if max_hops == 0 {
        return Ok(vec![]);
    }
    let seed_n = std::env::var("MEMORIA_GRAPH_SEED")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(10)
        .max(1) as usize;
    let decay = std::env::var("MEMORIA_GRAPH_DECAY")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.5)
        .clamp(0.05, 0.95);
    let fanout = std::env::var("MEMORIA_GRAPH_FANOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(10)
        .max(1) as usize;

    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let seed_rrf_max = results
        .iter()
        .map(|r| r.rrf_score)
        .fold(0.0_f64, f64::max)
        .max(1e-6);

    let mut expanded: Vec<FusedResult> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> =
        results.iter().map(|r| r.memory_id.clone()).collect();

    // 初始 frontier = top seed_n 种子
    let mut frontier: Vec<(String, f64)> = results
        .iter()
        .take(seed_n)
        .map(|r| (r.memory_id.clone(), r.rrf_score))
        .collect();

    for hop in 1..=max_hops {
        let mut next_frontier: Vec<(String, f64)> = Vec::new();
        let hop_factor = decay.powi(hop as i32);
        for (fid, _fscore) in &frontier {
            let hop_sql = format!(
                "SELECT r.neighbor_id, r.weight, r.relation_type, m.content
                 FROM (
                     SELECT target_id AS neighbor_id, weight, relation_type
                     FROM memory_relations WHERE source_id = ? AND namespace = ?
                     UNION
                     SELECT source_id AS neighbor_id, weight, relation_type
                     FROM memory_relations WHERE target_id = ? AND namespace = ?
                 ) r
                 LEFT JOIN memories m ON r.neighbor_id = m.id
                 LIMIT {}",
                fanout
            );
            if let Ok(mut stmt) = conn.prepare(&hop_sql) {
                if let Ok(rows) = stmt.query_map(
                    rusqlite::params![fid, namespace, fid, namespace],
                    |row| {
                        let target_id: String = row.get(0)?;
                        let weight: f64 = row.get(1)?;
                        let rel_type: String = row.get(2)?;
                        let content: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                        Ok((target_id, weight, rel_type, content))
                    },
                ) {
                    for row in rows.flatten() {
                        let (target_id, weight, rel_type, content) = row;
                        if seen_ids.insert(target_id.clone()) {
                            let score = seed_rrf_max * hop_factor * weight.abs().max(0.1);
                            expanded.push(FusedResult {
                                memory_id: target_id.clone(),
                                content,
                                rrf_score: score,
                                source: format!("graph_expand_h{}_{}", hop, rel_type),
                                signal_scores: vec![],
                                sem_cos: None,
                                kw_bm25: None,
                                evolved_at: None,
                                pending_evolution: false,
                                primary_channel: Some("graph_expand".to_string()),
                                channel_scores: {
                                    let mut m = std::collections::HashMap::new();
                                    m.insert("graph_expand".to_string(), score);
                                    m
                                },
                                access_count: 0,
                                last_recalled: None,
                                time_status: None,
                            });
                            next_frontier.push((target_id, score));
                        }
                    }
                }
            }
        }
        frontier = next_frontier;
        if frontier.is_empty() {
            break;
        }
    }

    Ok(expanded)
}
