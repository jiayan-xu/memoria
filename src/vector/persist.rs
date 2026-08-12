//! P1-3 向量持久化层。
//!
//! embedding 模型运行在 Python / 调用方，Rust 只接收并存储向量。
//! `memory_vectors` 表是 embedding 的**权威持久存储**：
//! - `remember` 拿到向量（query_cache 优先、其次本表）跑近义去重，并把新向量落表 + 增量加入 HNSW；
//! - 启动时从本表重建 HNSW，使近义去重在重启后依然可靠（不再依赖进程内 QueryCache 与 .bin 快取）。

use crate::storage::SqlitePool;
use crate::vector::{DIM, HnswIndex, VectorEntry};

/// 将 `Vec<f32>` 编码为 little-endian BLOB。
pub fn encode_vector(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for &x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

/// 从 little-endian BLOB 解码为 `Vec<f32>`。
pub fn decode_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 读取某记忆的持久向量（按记忆 id）。无则返回 None。
pub fn get_stored_vector(pool: &SqlitePool, id: &str) -> Option<Vec<f32>> {
    let conn = pool.get().ok()?;
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT vector FROM memory_vectors WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .ok()?;
    let v = decode_vector(&blob);
    if v.len() == DIM { Some(v) } else { None }
}

/// 写入/覆盖某记忆的持久向量（INSERT OR REPLACE）。
pub fn put_stored_vector(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    // P0 防御：拒绝退化（零 / 非有限）向量落库。历史嵌入失败的写入会在 memory_vectors
    // 留下全 0 向量，污染 HNSW 语义召回（零向量被 DistCosine 误判为完美匹配）。
    // 调用方均 `let _ =` 忽略返回值，故记忆仍照常写入、仅缺语义向量（退化为 keyword-only）。
    let norm_sq: f64 = vector.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    if !norm_sq.is_finite() || norm_sq == 0.0 {
        return Err("put_stored_vector: degenerate (zero/NaN) vector rejected".into());
    }
    conn.execute(
        "INSERT OR REPLACE INTO memory_vectors (id, namespace, vector) VALUES (?, ?, ?)",
        rusqlite::params![id, namespace, encode_vector(vector)],
    )
    .map_err(|e| format!("put_stored_vector: {}", e))?;
    Ok(())
}

/// 查询某记忆所属 namespace（用于批量落表时补全维度）。
pub fn lookup_namespace(pool: &SqlitePool, id: &str) -> Option<String> {
    let conn = pool.get().ok()?;
    conn.query_row(
        "SELECT namespace FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .ok()
}

/// 从 `memory_vectors` 表重建 HNSW 索引（启动权威路径）。
///
/// `HnswIndex::add` 内部按 id 去重，因此即使 .bin 已加载也能安全增量补齐；
/// 返回实际加入的向量条数。
pub fn rebuild_hnsw_from_store(pool: &SqlitePool, hnsw: &HnswIndex) -> Result<usize, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn
        .prepare("SELECT id, vector FROM memory_vectors")
        .map_err(|e| format!("prepare: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| format!("query: {}", e))?;

    let mut entries: Vec<VectorEntry> = Vec::new();
    for row in rows.flatten() {
        let (id, blob) = row;
        let v = decode_vector(&blob);
        if v.len() == DIM {
            entries.push(VectorEntry { id, vector: v });
        }
    }

    if entries.is_empty() {
        return Ok(0);
    }
    hnsw.add(&entries)
}

/// V1（2026-08-12）：从 `memory_hype_vectors` 表重建 HyPE 问句向量 HNSW 索引。
///
/// 与 `rebuild_hnsw_from_store` 平行，喂给**独立的 HyPE HNSW 实例**（内容索引与问句索引
/// 分离，因 HnswIndex 按 id 去重、同一 memory_id 只能有一条向量）。调用方（main.rs/lib.rs）
/// 在启动时对两个索引分别 rebuild；`semantic_search` 双路搜索后按 memory_id 取 max 合并。
pub fn rebuild_hype_hnsw_from_store(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
) -> Result<usize, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn
        .prepare("SELECT id, vector FROM memory_hype_vectors")
        .map_err(|e| format!("prepare hype: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| format!("query hype: {}", e))?;

    let mut entries: Vec<VectorEntry> = Vec::new();
    for row in rows.flatten() {
        let (id, blob) = row;
        let v = decode_vector(&blob);
        if v.len() == DIM {
            entries.push(VectorEntry { id, vector: v });
        }
    }

    if entries.is_empty() {
        return Ok(0);
    }
    hnsw.add(&entries)
}
