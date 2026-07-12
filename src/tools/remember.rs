//! Rust memory_remember implementation.
//! Phase 2.5: SQLite INSERT with SHA-256 dedup (compatible with Python side).
//! Phase P0: 近义重复检测 — HNSW cosine > 0.92 → 旧记忆标记 superseded_by。
//! Returns the memory ID (existing or new).

use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache, VectorEntry};
use sha2::{Digest, Sha256};

/// 近义去重开关 / 阈值 / top-k 均可通过环境变量覆盖（P1-3 可配）。
/// 默认：开近义、余弦阈值 0.92、HNSW 候选 top-k 10。
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

fn near_dup_topk() -> usize {
    std::env::var("MEMORIA_NEAR_DUP_TOPK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10)
}

/// Remember result with dedup info
#[derive(Debug, Default)]
pub struct RememberResult {
  pub id: String,
  pub action: String, // "created" | "updated_exact" | "superseded_near_dup"
  pub superseded_ids: Vec<String>,
  pub similarities: Vec<f32>,
}

/// Remember a durable memory with SHA-256 dedup (compatible with Python).
/// 原始接口 — 不做近义检测，向后兼容。
pub fn remember(
    pool: &SqlitePool,
    content: &str,
    category: &str,
    importance: i64,
    source: &str,
    namespace: &str,
    tags: &str,
) -> Result<String, String> {
    let result = remember_with_dedup(pool, content, category, importance, source, namespace, tags, None, None)?;
    Ok(result.id)
}

/// 带近义重复检测的 remember
///
/// 流程：
/// 1. SHA-256 精确去重（已有逻辑）
/// 2. 新记忆插入后，如果 query_cache 中有 content 的 embedding，
///    用 HNSW 搜索 top-N，cosine > 0.92 的标记为 superseded
/// 3. 被标记的记忆保留（不删除），但 superseded_by 指向新记忆
pub fn remember_with_dedup(
    pool: &SqlitePool,
    content: &str,
    category: &str,
    importance: i64,
    source: &str,
    namespace: &str,
    tags: &str,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
) -> Result<RememberResult, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // SHA-256 hash matching Python's hashlib.sha256(content.encode()).hexdigest()[:16]
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize())[..16].to_string();

    // Use content_hash as memory ID for dedup
    let mem_id = content_hash.clone();

    // Check if already exists (exact duplicate)
    let existing: Result<String, _> = conn.query_row(
        "SELECT id FROM memories WHERE id = ?",
        rusqlite::params![mem_id],
        |row| row.get(0),
    );

    if let Ok(_existing_id) = existing {
        // 精确重复 — boost importance and merge tags
        let tags_safe = if tags.is_empty() || tags == "[]" { String::new() } else { tags.to_string() };
        conn.execute(
            "UPDATE memories SET importance = MAX(importance, ?), confidence = MAX(confidence, 0.8),
             recall_count = recall_count + 1, last_recalled = ? WHERE id = ?",
            rusqlite::params![importance, chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(), mem_id],
        ).map_err(|e| format!("update: {}", e))?;
        if !tags_safe.is_empty() {
            let _ = conn.execute(
                "UPDATE memories SET tags = ? WHERE id = ? AND tags = '[]'",
                rusqlite::params![tags_safe, mem_id],
            );
        }
        return Ok(RememberResult {
            id: mem_id,
            action: "updated_exact".to_string(),
            ..Default::default()
        });
    }

    // Insert new memory
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let tags_safe = if tags.is_empty() || tags == "[]" { "[]".to_string() } else { tags.to_string() };
    conn.execute(
        "INSERT INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor, tags)
         VALUES (?, ?, ?, ?, ?, 0.8, 0, ?, 'hot', ?, 1.0, ?)",
        rusqlite::params![mem_id, namespace, source, content, category, now, importance, tags_safe],
    ).map_err(|e| format!("insert: {}", e))?;

    // ── 近义重复检测（P1-3：可配 + 向量持久化兜底）──
    let mut superseded_ids = Vec::new();
    let mut similarities = Vec::new();

    if near_dup_enabled() {
        if let (Some(hnsw_idx), Some(qc)) = (hnsw, query_cache) {
            // 向量来源：query_cache 优先；其次 memory_vectors 持久表（重启后仍可用，
            // 解决了 QueryCache 进程内、重启后近义去重弱化的问题）。
            // Python / 调用方在调 memory_remember 前通过 cache_query_vector 提供新内容向量。
            let query_vector: Option<Vec<f32>> = qc.get(content)
                .or_else(|| crate::vector::persist::get_stored_vector(pool, &mem_id));

            if let Some(qv) = query_vector {
                let threshold = near_dup_threshold();
                let topk = near_dup_topk();
                // 搜索 top-k 结果（排除自身）
                if let Ok(results) = hnsw_idx.search(&qv, topk) {
                    for (candidate_id, distance) in &results {
                        // 跳过自身
                        if *candidate_id == mem_id {
                            continue;
                        }

                        // cosine distance → similarity
                        let similarity = 1.0 - distance;

                        if similarity > threshold {
                            // 验证候选记忆是否在同一 namespace 且未被 superseded
                            let valid: Option<(String, Option<String>)> = conn
                                .query_row(
                                    "SELECT id, superseded_by FROM memories WHERE id = ? AND namespace = ?",
                                    rusqlite::params![candidate_id, namespace],
                                    |row| Ok((row.get(0)?, row.get(1)?)),
                                )
                                .ok();

                            if let Some((cid, existing_superseded)) = valid {
                                // 只标记未被 superseded 的记忆
                                if existing_superseded.is_none() {
                                    let _ = conn.execute(
                                        "UPDATE memories SET superseded_by = ?, tier = 'cold'
                                         WHERE id = ?",
                                        rusqlite::params![mem_id, cid],
                                    );
                                    // 记录关系边
                                    let _ = conn.execute(
                                        "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence)
                                         VALUES (?, ?, ?, 'same_entity', ?, 'near_dup_detection')",
                                        rusqlite::params![
                                            namespace,
                                            cid,
                                            mem_id,
                                            ((similarity * 100.0).round() / 100.0)
                                        ],
                                    );
                                    superseded_ids.push(cid);
                                    similarities.push(similarity);
                                }
                            }
                        } else {
                            // HNSW 结果按距离排序，低于阈值后可以 break
                            break;
                        }
                    }
                }

                // P1-3：把新向量持久化 + 增量加入 HNSW。
                // 之前从不把新记忆向量加入索引，导致后续近义漏标；现在落表并从表重建，
                // 重启后索引仍包含该向量，近义链可靠。
                let _ = crate::vector::persist::put_stored_vector(pool, &mem_id, namespace, &qv);
                let _ = hnsw_idx.add(&[VectorEntry { id: mem_id.clone(), vector: qv }]);
            }
        }
    }

    let action = if superseded_ids.is_empty() {
        "created".to_string()
    } else {
        "superseded_near_dup".to_string()
    };

    Ok(RememberResult {
        id: mem_id,
        action,
        superseded_ids,
        similarities,
    })
}

/// 查询被 superseded 的记忆链
pub fn get_supersession_chain(pool: &SqlitePool, memory_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, content, category, tier, superseded_by, created_at
             FROM memories WHERE superseded_by = ? ORDER BY created_at DESC",
        )
        .map_err(|e| format!("prepare: {}", e))?;

    let rows: Vec<serde_json::Value> = stmt
        .query_map(rusqlite::params![memory_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "content": row.get::<_, String>(1)?,
                "category": row.get::<_, String>(2)?,
                "tier": row.get::<_, String>(3)?,
                "superseded_by": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })
        .map_err(|e| format!("query: {}", e))?
        .flatten()
        .collect();

    Ok(rows)
}

/// 手动合并两条近义记忆（管理员操作）
pub fn merge_memories(
    pool: &SqlitePool,
    keep_id: &str,
    merge_id: &str,
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // 将 merge_id 标记为 superseded
    conn
        .execute(
            "UPDATE memories SET superseded_by = ?, tier = 'cold' WHERE id = ?",
            rusqlite::params![keep_id, merge_id],
        )
        .map_err(|e| format!("update: {}", e))?;

    // 转移 recall_count
    let recall: i64 = conn
        .query_row(
            "SELECT recall_count FROM memories WHERE id = ?",
            rusqlite::params![merge_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if recall > 0 {
        let _ = conn.execute(
            "UPDATE memories SET recall_count = recall_count + ? WHERE id = ?",
            rusqlite::params![recall, keep_id],
        );
    }

    // 取 keep_id 所属 namespace（merge 通常同 NS）
    let ns: String = conn
        .query_row(
            "SELECT namespace FROM memories WHERE id = ?",
            rusqlite::params![keep_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "default".to_string());

    // 添加关系边（P2-1 修复：补 namespace，避免归入 default 导致 NS 隔离失效）
    let _ = conn.execute(
        "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence)
         VALUES (?, ?, ?, 'same_entity', 1.0, 'manual_merge')",
        rusqlite::params![ns, merge_id, keep_id],
    );

    Ok(())
}
