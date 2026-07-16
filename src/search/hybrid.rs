//! 统一搜索入口 — 整合 5 信号（keyword + semantic + temporal + importance + category）
//!
//! 替代 lib.rs hybrid_search 和 mcp_server.rs 中各自维护的搜索逻辑。

use crate::search::{self, FusedResult, SignalResult};
use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache};

/// 执行 5 信号融合搜索，返回 RRF 排序结果
///
/// `as_of`: P1-5 轻量时序真值。
/// - `Some("2026-01-02T00:00:00")` → `visible_as_of`：仅返回该时刻「有效」的记忆
///   （valid_from <= as_of 且 (valid_to IS NULL 或 valid_to >= as_of)），不查 superseded_by（时序真值优先）。
/// - `None`（默认）→ `is_latest_now`：superseded_by IS NULL 且当前(now)有效（§14.1 Q2）。
/// `include_superseded=true` 跳过整段 isLatest/visible_as_of 过滤（含时序），返回全部行。
pub fn hybrid_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    max_results: u32,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    as_of: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<FusedResult>, String> {
    let fts_limit = max_results * 3;
    let mut signals: Vec<Vec<SignalResult>> = Vec::new();
    let mut weights: Vec<f64> = Vec::new();

    // S1: Keyword (FTS5 + LIKE)
    if let Ok(kw) = search::keyword::keyword_search(pool, query, namespace, fts_limit) {
        if !kw.is_empty() {
            signals.push(kw);
            weights.push(1.0);
        }
    }

    // S2: Semantic (HNSW vector)
    if let (Some(hnsw), Some(qc)) = (hnsw, query_cache) {
        if let Ok(sem) = search::semantic::semantic_search(
            query,
            namespace,
            fts_limit,
            Some(hnsw),
            Some(qc),
            Some(pool),
        ) {
            if !sem.is_empty() {
                signals.push(sem);
                weights.push(1.0);
            }
        }
    }

    // S3: Temporal (recency bias)
    if let Ok(temp) = search::temporal::temporal_search(pool, namespace, fts_limit) {
        if !temp.is_empty() {
            signals.push(temp);
            weights.push(1.0);
        }
    }

    // S4: Importance (recall count + decay)
    if let Ok(imp) = search::importance::importance_search(pool, namespace, fts_limit) {
        if !imp.is_empty() {
            signals.push(imp);
            weights.push(1.0);
        }
    }

    // S5: Category (query intent match)
    if let Ok(cat) = search::importance::category_search(pool, query, namespace, max_results) {
        if !cat.is_empty() {
            signals.push(cat);
            weights.push(0.5);
        }
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
    let mut unique: Vec<FusedResult> = fused
        .into_iter()
        .filter(|r| seen.insert(r.memory_id.clone()))
        .collect();

    // P0 + P1-5: isLatest / visible_as_of 过滤（§14.1 Q2）。
    // 统一在 dedup 后、take 前执行；graph_expand 邻居因已并入 unique 一并覆盖。
    // 规则：
    //  - include_superseded=true → 不过滤（返回全部，含历史被取代行）
    //  - as_of=Some(T)          → 仅 visible_as_of（valid_* 有效），不查 superseded_by（时序真值优先）
    //  - as_of=None（默认 tip） → is_latest_now：superseded_by IS NULL 且当前(now)有效
    if !include_superseded && !unique.is_empty() {
        if let Ok(conn) = pool.get() {
            let ids: Vec<String> = unique.iter().map(|r| r.memory_id.clone()).collect();
            let ph = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, superseded_by, valid_from, valid_to FROM memories WHERE id IN ({})",
                ph
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let info: std::collections::HashMap<
                    String,
                    (Option<String>, Option<String>, Option<String>),
                > = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<String>>(3)?,
                            ),
                        ))
                    })
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default();
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                unique.retain(|r| match info.get(&r.memory_id) {
                    Some((sup, vf, vt)) => match as_of {
                        Some(t) => valid_at(vf.as_deref(), vt.as_deref(), t), // visible_as_of
                        None => {
                            sup.is_none() && valid_at(vf.as_deref(), vt.as_deref(), now.as_str())
                        } // is_latest_now
                    },
                    None => false,
                });
            }
        }
    }

    // Phase B / M1.3：轻量共现启发式 rerank（O5：无 cross-encoder）
    // 在 take 前对候选池加成重排，再截断到 max_results。
    search::cooccur::rerank_by_cooccurrence(pool, namespace, query, &mut unique);
    // P2 / M2.1：text_signals 数字/日期重叠加成（可 MEMORIA_TEXT_SIGNALS_RERANK=0 关闭）
    search::text_signals::rerank_by_text_signals(query, &mut unique);

    let unique: Vec<FusedResult> = unique.into_iter().take(max_results as usize).collect();

    Ok(unique)
}

/// P1-5: 判断记忆在 `as_of` 时刻是否有效。
/// 有效区间：[valid_from, valid_to]，端点闭合。任一端点缺失按「无界」处理。
/// 注意：valid_from/valid_to 为固定格式 ISO-8601 字符串，字典序即时间序，可直接比较。
fn valid_at(valid_from: Option<&str>, valid_to: Option<&str>, as_of: &str) -> bool {
    let from_ok = match valid_from {
        None => true,
        Some(v) => v <= as_of,
    };
    let to_ok = match valid_to {
        None => true,
        Some(v) => v >= as_of,
    };
    from_ok && to_ok
}
