//! 增量 semantic_related 边维护（闭环 Phase 1b）。
//!
//! 离线脚本 `tools/offline/build_semantic_edges.py` 负责存量全量补边；
//! 本模块负责**新记忆落库后即时补边**，使图不滞后（review 第 7 条空缺）。
//!
//! 关键约束：
//! - HNSW 是全局索引、无 namespace 维度（见 semantic.rs:13）→ 必须按 ns 回查过滤，杜绝跨租户连边。
//! - 双向插边（新记忆↔存量近邻），让新记忆即时入图、也被近邻可达。
//! - 幂等：重算时清掉该记忆的旧 semantic 出边；对每条近邻精确 upsert 反向入边。
//! - 失败静默（return Ok(0)），不影响 remember 主链路。

use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use rusqlite::params_from_iter;
use std::collections::HashMap;

fn env_usize_def(key: &str, default: usize) -> usize {
    match std::env::var(key) {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(default),
        Err(_) => default,
    }
}

fn env_f32_def(key: &str, default: f32) -> f32 {
    match std::env::var(key) {
        Ok(v) => v.trim().parse::<f32>().unwrap_or(default),
        Err(_) => default,
    }
}

/// 新记忆落库后，基于其向量在 HNSW 中找同 ns 近邻，补 semantic_related 双向边。
///
/// 返回成功插入/保留的边数（双向计 2）；任何异常返回 Ok(0) 不冒泡。
pub fn upsert_semantic_edges_for(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
    id: &str,
    namespace: &str,
    vec: &[f32],
) -> Result<usize, String> {
    let k = env_usize_def("MEMORIA_SEMANTIC_INCR_K", 12);
    let threshold = env_f32_def("MEMORIA_SEMANTIC_INCR_THRESHOLD", 0.60);
    let cap = env_usize_def("MEMORIA_SEMANTIC_INCR_CAP", 8);
    if k == 0 {
        return Ok(0);
    }

    // P0 防御：落库原始向量退化（NaN / 全零）→ 在 HNSW 中与任意记忆 distance≈0
    // → sim≈1.0 ≥ threshold → 误建 cap 个双向 semantic_related 边污染图谱。
    // add() 已拦退化向量入索引，此处对「增量补边」的入参向量同样守卫。
    let norm_sq: f32 = vec.iter().map(|&x| x * x).sum();
    if !norm_sq.is_finite() || norm_sq <= 0.0 {
        return Ok(0);
    }

    let near = hnsw.search(vec, k).unwrap_or_default();
    if near.is_empty() {
        return Ok(0);
    }

    // HNSW 全局索引无 ns 维度 → 回查 memories 表按 ns 过滤（防跨租户连边）
    let ns_map: HashMap<String, String> = {
        let conn0 = pool.get().map_err(|e| format!("pool: {}", e))?;
        let ids: Vec<&String> = near.iter().map(|(nid, _)| nid).collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, namespace FROM memories WHERE id IN ({})",
            placeholders
        );
        let mut stmt = conn0
            .prepare(&sql)
            .map_err(|e| format!("prepare ns: {}", e))?;
        let rows = stmt
            .query_map(
                params_from_iter(ids.iter().map(|s| *s)),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| format!("query ns: {}", e))?;
        rows.flatten().collect()
    };

    // 过滤同 ns、相似度达标、按 sim 降序取 cap 条
    let mut edges: Vec<(String, f32)> = near
        .iter()
        .filter(|(nid, _)| nid.as_str() != id)
        .filter(|(nid, _)| {
            ns_map
                .get(nid.as_str())
                .map(|ns| ns == namespace)
                .unwrap_or(false)
        })
        .map(|(nid, dist)| (nid.clone(), 1.0 - *dist))
        .filter(|(_, sim)| *sim >= threshold)
        .collect();
    edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    edges.truncate(cap);

    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    // 清掉该记忆旧的 semantic 出边（重算用）
    conn.execute(
        "DELETE FROM memory_relations WHERE relation_type='semantic_related' AND source_id=?",
        rusqlite::params![id],
    )
    .map_err(|e| format!("delete out: {}", e))?;

    let mut inserted = 0usize;
    for (nid, sim) in &edges {
        let w = ((*sim * 100.0).round() / 100.0) as f64;
        // 正向 (id -> nid)
        conn.execute(
            "DELETE FROM memory_relations WHERE relation_type='semantic_related' AND source_id=? AND target_id=?",
            rusqlite::params![id, nid],
        )
        .map_err(|e| format!("del fwd: {}", e))?;
        conn.execute(
            "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence) VALUES (?,?,?,'semantic_related',?,'incremental')",
            rusqlite::params![namespace, id, nid, w],
        )
        .map_err(|e| format!("ins fwd: {}", e))?;
        // 反向 (nid -> id)
        conn.execute(
            "DELETE FROM memory_relations WHERE relation_type='semantic_related' AND source_id=? AND target_id=?",
            rusqlite::params![nid, id],
        )
        .map_err(|e| format!("del rev: {}", e))?;
        conn.execute(
            "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence) VALUES (?,?,?,'semantic_related',?,'incremental')",
            rusqlite::params![namespace, nid, id, w],
        )
        .map_err(|e| format!("ins rev: {}", e))?;
        inserted += 2;
    }
    Ok(inserted)
}
