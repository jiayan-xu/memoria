//! Rust memory_remember implementation.
//! Phase 2.5: SQLite INSERT with SHA-256 dedup (compatible with Python side).
//! Phase P0: 近义重复检测 — HNSW cosine > 0.92 → 旧记忆标记 superseded_by。
//! Returns the memory ID (existing or new).

use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache, VectorEntry};
use rusqlite::Connection;
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
    pub action: String, // "created" | "updated_exact" | "superseded_near_dup" | "superseded_explicit"
    pub superseded_ids: Vec<String>,
    pub similarities: Vec<f32>,
}

/// §9.1：supersede 时计算旧行 valid_to（stamp_to）。
/// - 旧 valid_to 已过期（< now）→ 保留，不回拨
/// - 否则 → stamp 为 now（截断未来 TTL 或关闭开放区间）
pub fn compute_stamp_to(old_valid_to: Option<&str>, now: &str) -> String {
    match old_valid_to {
        Some(vt) if !vt.is_empty() && vt < now => vt.to_string(),
        _ => now.to_string(),
    }
}

/// 归一记忆边 relation（snake_case）；显式 supersede 默认 updates。
fn normalize_memory_relation(raw: Option<&str>) -> Result<&'static str, String> {
    let s = raw.unwrap_or("updates").trim().to_ascii_lowercase();
    match s.as_str() {
        "updates" | "update" => Ok("updates"),
        "extends" | "extend" => Ok("extends"),
        "derives" | "derive" => Ok("derives"),
        "same_entity" => Ok("same_entity"),
        "chronological" => Ok("chronological"),
        "semantic_related" => Ok("semantic_related"),
        other => Err(format!(
            "400: invalid relation '{}'; allowed: updates|extends|derives|same_entity|chronological|semantic_related",
            other
        )),
    }
}

/// 校验 supersedes_id 目标：存在 / 同 ns / tip / 非自指。
fn validate_supersede_target(
    conn: &Connection,
    target_id: &str,
    namespace: &str,
    new_id: &str,
) -> Result<(), String> {
    if target_id == new_id {
        return Err("409: self-referencing supersede".to_string());
    }
    let target: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT namespace, superseded_by FROM memories WHERE id = ?",
            rusqlite::params![target_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    match target {
        None => Err(format!("404: supersede target not found: {}", target_id)),
        Some((t_ns, t_sup)) => {
            if t_ns != namespace {
                return Err(format!(
                    "403: supersede cross-namespace: {} not in {}",
                    target_id, namespace
                ));
            }
            if t_sup.is_some() {
                return Err(format!(
                    "409: supersede target not tip (already superseded): {}",
                    target_id
                ));
            }
            Ok(())
        }
    }
}

/// 事务内 stamp 旧 tip：superseded_by + tier=cold + §9.1 valid_to，并写记忆边。
fn apply_supersede_in_tx(
    conn: &Connection,
    new_id: &str,
    target_id: &str,
    namespace: &str,
    now: &str,
    relation_type: &str,
    evidence: &str,
) -> Result<(), String> {
    let old_vt: Option<String> = conn
        .query_row(
            "SELECT valid_to FROM memories WHERE id = ?",
            rusqlite::params![target_id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    let stamp_to = compute_stamp_to(old_vt.as_deref(), now);
    conn.execute(
        "UPDATE memories SET superseded_by = ?, tier = 'cold', valid_to = ? WHERE id = ?",
        rusqlite::params![new_id, stamp_to, target_id],
    )
    .map_err(|e| format!("supersede update: {}", e))?;
    conn.execute(
        "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence)
         VALUES (?, ?, ?, ?, 1.0, ?)",
        rusqlite::params![namespace, target_id, new_id, relation_type, evidence],
    )
    .map_err(|e| format!("supersede relation insert ({}): {}", relation_type, e))?;
    Ok(())
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
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> Result<String, String> {
    let result = remember_with_dedup(
        pool,
        content,
        category,
        importance,
        source,
        namespace,
        tags,
        None,
        None,
        valid_from,
        valid_to,
        None,
        None,
    )?;
    Ok(result.id)
}

/// P0+ 吸收 HMS: 写入「事件发生」时刻（与 valid_from 断言时刻区分）。
/// 作为 remember 后的独立 UPDATE，避免改动 remember_with_dedup 签名而牵连大量调用方。
/// event_time 缺省留 NULL（召回时以 valid_from 兜底为 occurred）。
pub fn set_event_time(
    pool: &SqlitePool,
    memory_id: &str,
    event_time: &str,
) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    conn.execute(
        "UPDATE memories SET event_time = ? WHERE id = ?",
        rusqlite::params![event_time, memory_id],
    )
    .map_err(|e| format!("set event_time: {}", e))?;
    Ok(())
}

/// 带近义重复检测的 remember
///
/// `supersedes_id`：显式取代目标；与 INSERT 同事务，失败 ROLLBACK。
/// `relation`：记忆边类型，默认 `updates`（P1 枚举）。
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
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    supersedes_id: Option<&str>,
    relation: Option<&str>,
) -> Result<RememberResult, String> {
    let relation_type = normalize_memory_relation(relation)?;
    let mut conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // SHA-256 hash matching Python's hashlib.sha256(content.encode()).hexdigest()[:16]
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize())[..16].to_string();
    let mem_id = content_hash.clone();

    let candidate_vector: Option<Vec<f32>> = if let (Some(qc), Some(_h)) = (query_cache, hnsw) {
        qc.get(content)
            .or_else(|| crate::vector::persist::get_stored_vector(pool, &mem_id))
    } else {
        None
    };

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let tags_safe = if tags.is_empty() || tags == "[]" {
        "[]".to_string()
    } else {
        tags.to_string()
    };

    // Check if already exists (exact duplicate)
    let existing: Result<String, _> = conn.query_row(
        "SELECT id FROM memories WHERE id = ?",
        rusqlite::params![mem_id],
        |row| row.get(0),
    );

    if let Ok(_existing_id) = existing {
        // 精确重复：仍须处理 supersedes_id，禁止静默跳过
        if let Some(target_id) = supersedes_id {
            validate_supersede_target(&conn, target_id, namespace, &mem_id)?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin tx: {}", e))?;
            tx.execute(
                "UPDATE memories SET importance = MAX(importance, ?), confidence = MAX(confidence, 0.8),
                 recall_count = recall_count + 1, last_recalled = ? WHERE id = ?",
                rusqlite::params![importance, now, mem_id],
            )
            .map_err(|e| format!("update: {}", e))?;
            if tags_safe != "[]" {
                let _ = tx.execute(
                    "UPDATE memories SET tags = ? WHERE id = ? AND (tags = '[]' OR tags = '')",
                    rusqlite::params![tags_safe, mem_id],
                );
            }
            apply_supersede_in_tx(
                &tx,
                &mem_id,
                target_id,
                namespace,
                &now,
                relation_type,
                "explicit_supersede_exact",
            )?;
            tx.commit().map_err(|e| format!("commit: {}", e))?;

            if near_dup_enabled() {
                if let (Some(hnsw_idx), Some(qv)) = (hnsw, &candidate_vector) {
                    if crate::vector::persist::get_stored_vector(pool, &mem_id).is_none() {
                        let _ = crate::vector::persist::put_stored_vector(pool, &mem_id, namespace, qv);
                        let _ = hnsw_idx.add(&[VectorEntry {
                            id: mem_id.clone(),
                            vector: qv.clone(),
                        }]);
                    }
                }
            }

            return Ok(RememberResult {
                id: mem_id,
                action: "superseded_explicit".to_string(),
                superseded_ids: vec![target_id.to_string()],
                similarities: vec![],
            });
        }

        // 无 supersedes_id：常规精确去重 boost
        conn.execute(
            "UPDATE memories SET importance = MAX(importance, ?), confidence = MAX(confidence, 0.8),
             recall_count = recall_count + 1, last_recalled = ? WHERE id = ?",
            rusqlite::params![importance, now, mem_id],
        )
        .map_err(|e| format!("update: {}", e))?;
        if tags_safe != "[]" {
            let _ = conn.execute(
                "UPDATE memories SET tags = ? WHERE id = ? AND (tags = '[]' OR tags = '')",
                rusqlite::params![tags_safe, mem_id],
            );
        }
        if near_dup_enabled() {
            if let (Some(hnsw_idx), Some(qv)) = (hnsw, &candidate_vector) {
                if crate::vector::persist::get_stored_vector(pool, &mem_id).is_none() {
                    let _ = crate::vector::persist::put_stored_vector(pool, &mem_id, namespace, qv);
                    let _ = hnsw_idx.add(&[VectorEntry {
                        id: mem_id.clone(),
                        vector: qv.clone(),
                    }]);
                }
            }
        }

        return Ok(RememberResult {
            id: mem_id,
            action: "updated_exact".to_string(),
            ..Default::default()
        });
    }

    // ── 新写入：显式 supersede 时先校验再开事务，失败不留脏 tip ──
    if let Some(target_id) = supersedes_id {
        validate_supersede_target(&conn, target_id, namespace, &mem_id)?;
    }

    let valid_from_val = valid_from.unwrap_or(&now);
    let valid_to_val = valid_to;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin tx: {}", e))?;

    tx.execute(
        "INSERT INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor, tags, valid_from, valid_to)
         VALUES (?, ?, ?, ?, ?, 0.8, 0, ?, 'hot', ?, 1.0, ?, ?, ?)",
        rusqlite::params![
            mem_id,
            namespace,
            source,
            content,
            category,
            now,
            importance,
            tags_safe,
            valid_from_val,
            valid_to_val
        ],
    )
    .map_err(|e| format!("insert: {}", e))?;

    let mut superseded_ids = Vec::new();
    let mut similarities = Vec::new();
    let mut explicit_superseded = false;

    // 近义重复检测（同事务内 stamp，边写 updates）
    if near_dup_enabled() {
        if let (Some(hnsw_idx), Some(_qc)) = (hnsw, query_cache) {
            let query_vector: Option<Vec<f32>> = candidate_vector.clone();
            if let Some(qv) = query_vector {
                let threshold = near_dup_threshold();
                let topk = near_dup_topk();
                if let Ok(results) = hnsw_idx.search(&qv, topk) {
                    for (candidate_id, distance) in &results {
                        if *candidate_id == mem_id {
                            continue;
                        }
                        let similarity = 1.0 - distance;
                        if similarity > threshold {
                            let valid: Option<(String, Option<String>)> = tx
                                .query_row(
                                    "SELECT id, superseded_by FROM memories WHERE id = ? AND namespace = ?",
                                    rusqlite::params![candidate_id, namespace],
                                    |row| Ok((row.get(0)?, row.get(1)?)),
                                )
                                .ok();
                            if let Some((cid, existing_superseded)) = valid {
                                if existing_superseded.is_none() {
                                    // 若显式 supersedes_id 已指向同一 id，跳过以免重复边
                                    if supersedes_id == Some(cid.as_str()) {
                                        continue;
                                    }
                                    let old_vt: Option<String> = tx
                                        .query_row(
                                            "SELECT valid_to FROM memories WHERE id = ?",
                                            rusqlite::params![cid],
                                            |r| r.get(0),
                                        )
                                        .unwrap_or(None);
                                    let stamp_to = compute_stamp_to(old_vt.as_deref(), &now);
                                    tx.execute(
                                        "UPDATE memories SET superseded_by = ?, tier = 'cold', valid_to = ?
                                         WHERE id = ?",
                                        rusqlite::params![mem_id, stamp_to, cid],
                                    )
                                    .map_err(|e| format!("near_dup supersede: {}", e))?;
                                    let weight = (similarity * 100.0).round() / 100.0;
                                    tx.execute(
                                        "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence)
                                         VALUES (?, ?, ?, 'updates', ?, 'near_dup_detection')",
                                        rusqlite::params![namespace, cid, mem_id, weight],
                                    )
                                    .map_err(|e| format!("near_dup relation insert: {}", e))?;
                                    superseded_ids.push(cid);
                                    similarities.push(similarity);
                                }
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }

    // 显式 supersedes_id（同事务）
    if let Some(target_id) = supersedes_id {
        // 并发：事务内再验 tip（可能刚被他人取代）
        validate_supersede_target(&tx, target_id, namespace, &mem_id)?;
        apply_supersede_in_tx(
            &tx,
            &mem_id,
            target_id,
            namespace,
            &now,
            relation_type,
            "explicit_supersede",
        )?;
        superseded_ids.push(target_id.to_string());
        explicit_superseded = true;
    }

    tx.commit().map_err(|e| format!("commit: {}", e))?;

    // 向量持久化在事务外（非 tip 权威）；失败不回滚记忆写入
    if near_dup_enabled() {
        if let (Some(hnsw_idx), Some(qv)) = (hnsw, candidate_vector.as_ref()) {
            let _ = crate::vector::persist::put_stored_vector(pool, &mem_id, namespace, qv);
            let _ = hnsw_idx.add(&[VectorEntry {
                id: mem_id.clone(),
                vector: qv.clone(),
            }]);
        }
    }

    let action = if superseded_ids.is_empty() {
        "created".to_string()
    } else if explicit_superseded {
        "superseded_explicit".to_string()
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
pub fn get_supersession_chain(
    pool: &SqlitePool,
    memory_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
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
pub fn merge_memories(pool: &SqlitePool, keep_id: &str, merge_id: &str) -> Result<(), String> {
    let mut conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin tx: {}", e))?;

    let old_vt: Option<String> = tx
        .query_row(
            "SELECT valid_to FROM memories WHERE id = ?",
            rusqlite::params![merge_id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    let stamp_to = compute_stamp_to(old_vt.as_deref(), &now);

    tx.execute(
        "UPDATE memories SET superseded_by = ?, tier = 'cold', valid_to = ? WHERE id = ?",
        rusqlite::params![keep_id, stamp_to, merge_id],
    )
    .map_err(|e| format!("update: {}", e))?;

    let recall: i64 = tx
        .query_row(
            "SELECT recall_count FROM memories WHERE id = ?",
            rusqlite::params![merge_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if recall > 0 {
        tx.execute(
            "UPDATE memories SET recall_count = recall_count + ? WHERE id = ?",
            rusqlite::params![recall, keep_id],
        )
        .map_err(|e| format!("recall merge: {}", e))?;
    }

    let ns: String = tx
        .query_row(
            "SELECT namespace FROM memories WHERE id = ?",
            rusqlite::params![keep_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "default".to_string());

    tx.execute(
        "INSERT INTO memory_relations (namespace, source_id, target_id, relation_type, weight, evidence)
         VALUES (?, ?, ?, 'updates', 1.0, 'manual_merge')",
        rusqlite::params![ns, merge_id, keep_id],
    )
    .map_err(|e| format!("merge relation insert: {}", e))?;

    tx.commit().map_err(|e| format!("commit: {}", e))?;
    Ok(())
}
