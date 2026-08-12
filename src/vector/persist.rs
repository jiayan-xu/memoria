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
    put_vector_into(pool, id, namespace, vector, "memory_vectors")
}

/// V1（2026-08-12）：写入/覆盖某记忆的 HyPE 问句向量（与 `put_stored_vector` 对称，
/// 落 `memory_hype_vectors` 表）。同款退化/维度防御；共享 `put_vector_into` 实现。
pub fn put_hype_stored_vector(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
) -> Result<(), String> {
    put_vector_into(pool, id, namespace, vector, "memory_hype_vectors")
}

/// 共享实现：校验后写入 `table`（须含 id/namespace/vector/updated_at 列）。
///
/// `table` 仅接受两个内部常量（白名单 dispatch，杜绝字符串插值注入面）；
/// 用 `ON CONFLICT(id) DO UPDATE` 而非 INSERT OR REPLACE——REPLACE 会整行删除重建，
/// 把 `memory_hype_vectors.question`（离线脚本写入的假设问句）静默抹成 NULL。
fn put_vector_into(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
    table: &str,
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    // P0 防御：拒绝退化（零 / 非有限）向量落库。历史嵌入失败的写入会在 memory_vectors
    // 留下全 0 向量，污染 HNSW 语义召回（零向量被 DistCosine 误判为完美匹配）。
    // 调用方均 `let _ =` 忽略返回值，故记忆仍照常写入、仅缺语义向量（退化为 keyword-only）。
    let norm_sq: f64 = vector.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    if !norm_sq.is_finite() || norm_sq == 0.0 {
        return Err(format!("{table}: degenerate (zero/NaN) vector rejected"));
    }
    // 写入时校验维度：错误长度的向量落库后会被 rebuild 静默跳过（仅 stderr 告警），
    // 造成"API 接受但索引永远不含"的死行——fail fast 使写入/读取路径一致。
    // 注意：退化检查在前、维度检查在后——零值且错长度的向量报"degenerate"而非
    // "dimension mismatch"，是刻意为之（退化更根本，先拒绝）。
    if vector.len() != DIM {
        return Err(format!(
            "{table}: dimension mismatch: expected {}, got {}",
            DIM,
            vector.len()
        ));
    }
    match table {
        "memory_vectors" => conn
            .execute(
                "INSERT INTO memory_vectors (id, namespace, vector) VALUES (?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET vector=excluded.vector",
                rusqlite::params![id, namespace, encode_vector(vector)],
            )
            .map(|_| ())
            .map_err(|e| format!("put_stored_vector: {}", e))?,
        "memory_hype_vectors" => conn
            .execute(
                "INSERT INTO memory_hype_vectors (id, namespace, vector, updated_at) \
                 VALUES (?, ?, ?, datetime('now')) \
                 ON CONFLICT(id) DO UPDATE SET vector=excluded.vector, \
                                                updated_at=excluded.updated_at",
                rusqlite::params![id, namespace, encode_vector(vector)],
            )
            .map(|_| ())
            .map_err(|e| format!("put_hype_stored_vector: {}", e))?,
        _ => return Err(format!("put_vector_into: unknown table {table}")),
    }
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
    rebuild_from_table(pool, hnsw, "memory_vectors", "content")
}

/// V1（2026-08-12）：从 `memory_hype_vectors` 表重建 HyPE 问句向量 HNSW 索引。
///
/// 与 `rebuild_hnsw_from_store` 平行（共享 `rebuild_from_table` 实现），喂给**独立的 HyPE
/// HNSW 实例**（内容索引与问句索引分离，因 HnswIndex 按 id 去重、同一 memory_id 只能有
/// 一条向量）。调用方（main.rs/lib.rs）在启动时对两个索引分别 rebuild；`semantic_search`
/// 双路搜索后按 memory_id 取 max 合并。
pub fn rebuild_hype_hnsw_from_store(pool: &SqlitePool, hnsw: &HnswIndex) -> Result<usize, String> {
    rebuild_from_table(pool, hnsw, "memory_hype_vectors", "hype")
}

/// 共享实现：从 `table`（须含 id/vector 列）读取全部向量并加入 HNSW。
///
/// `label` 用于错误/告警前缀（区分 content/hype，便于日志定位）。
/// 统计并告警被跳过的行（解码失败 / 维度 ≠ DIM），使索引健康度可观测——
/// 数据损坏不再被静默吞掉（#R32 other/low：flatten 丢弃的行错误 + 维度不符行无计数）。
/// 表名**白名单 dispatch**（非字符串插值）：两个调用方只传内部常量，杜绝未来
/// 非受控输入进入 SQL 的注入面（#R33 security/low）。
fn rebuild_from_table(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
    table: &str,
    label: &str,
) -> Result<usize, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let table_sql = match table {
        "memory_vectors" => "SELECT id, vector FROM memory_vectors",
        "memory_hype_vectors" => "SELECT id, vector FROM memory_hype_vectors",
        _ => return Err(format!("{label}: unknown rebuild table {table}")),
    };
    let mut stmt = conn
        .prepare(table_sql)
        .map_err(|e| format!("prepare {}: {}", label, e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| format!("query {}: {}", label, e))?;

    let mut entries: Vec<VectorEntry> = Vec::new();
    let mut skipped = 0usize;
    for row in rows {
        match row {
            Ok((id, blob)) => {
                let v = decode_vector(&blob);
                if v.len() == DIM {
                    entries.push(VectorEntry { id, vector: v });
                } else {
                    skipped += 1;
                }
            }
            Err(e) => {
                skipped += 1;
                // 注意：decode_vector 本身不可失败（长度不符走 Ok 分支的跳过路径），
                // 此 Err 分支捕获的是 query_map 的行读取/列转换/SQLite 迭代错误。
                eprintln!("[persist] {label} row read/iteration failed: {e}");
            }
        }
    }
    if skipped > 0 {
        eprintln!(
            "[persist] {label}: {skipped} row(s) skipped (decode failure or dim != {DIM})"
        );
    }

    if entries.is_empty() {
        return Ok(0);
    }
    hnsw.add(&entries)
}
