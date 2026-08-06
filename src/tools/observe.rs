//! Rust memory_observe implementation.
//! Content is stored as-is (no prefix), matching Python side.
//! 2026-08-06 治理配套：写入前近义去重（与 remember_with_dedup 同机制）——HNSW 近邻
//! 余弦 > MEMORIA_NEAR_DUP_THRESHOLD（默认 0.92）时复用已有记忆（recall_count+1），
//! 不落新行。杜绝 consolidate/观察流对相似变体的重复堆积（07-23 1236 条 pattern 教训）。

use crate::storage::SqlitePool;
use crate::tools::compress::distill;
use crate::vector::{HnswIndex, QueryCache};
use sha2::{Digest, Sha256};

fn near_dup_enabled() -> bool {
    match std::env::var("MEMORIA_NEAR_DUP_ENABLED") {
        Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")),
        Err(_) => true,
    }
}

fn near_dup_threshold() -> f32 {
    std::env::var("MEMORIA_NEAR_DUP_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(0.92)
}

pub fn observe(
    pool: &SqlitePool,
    dialog: &str,
    _role: &str,
    source: &str,
    _session_id: &str,
    namespace: &str,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
) -> Result<String, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // SHA-256 dedup key (matches remember.rs / Python _hash_content())
    let mut hasher = Sha256::new();
    hasher.update(dialog.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize())[..16].to_string();
    let mem_id = content_hash; // id == content_hash → identical content is ignored on re-observe

    // 近义去重（2026-08-06）：写入前查 HNSW 近邻，相似度 > 阈值则复用已有记忆。
    // 向量来源：写入侧嵌入注入的 query_cache，或该内容在 memory_vectors 的既有向量。
    if near_dup_enabled() {
        if let (Some(hnsw_idx), Some(qc)) = (hnsw, query_cache) {
            let qv = qc
                .get(dialog)
                .or_else(|| crate::vector::persist::get_stored_vector(pool, &mem_id));
            if let Some(qv) = qv {
                let threshold = near_dup_threshold();
                if let Ok(results) = hnsw_idx.search(&qv, 10) {
                    for (candidate_id, distance) in &results {
                        if candidate_id == &mem_id {
                            continue;
                        }
                        let similarity = 1.0 - distance;
                        if similarity > threshold {
                            // 提升已有记忆热度并复用其 id，避免相似变体堆积
                            let _ = conn.execute(
                                "UPDATE memories SET recall_count = recall_count + 1 \
                                 WHERE id = ?1 AND namespace = ?2",
                                rusqlite::params![candidate_id, namespace],
                            );
                            return Ok(candidate_id.clone());
                        }
                    }
                }
            }
        }
    }

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let (content, raw_ref) = distill(dialog);
    conn.execute(
        "INSERT OR IGNORE INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor, raw_ref)
         VALUES (?, ?, ?, ?, 'observation', 0.5, 0, ?, 'warm', 2, 1.0, ?)",
        rusqlite::params![mem_id, namespace, source, content, now, raw_ref],
    )
    .map_err(|e| format!("insert: {}", e))?;

    Ok(mem_id)
}
