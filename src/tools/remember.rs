//! Rust memory_remember implementation.
//! Phase 2.5: SQLite INSERT with SHA-256 dedup (compatible with Python side).
//! Phase P0: 近义重复检测 — HNSW cosine > 0.92 → 旧记忆标记 superseded_by。
//! Returns the memory ID (existing or new).

use crate::storage::SqlitePool;
use crate::tools::compress::distill;
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

/// §9.1（PR3 双时态补洞）：supersede 时计算旧行 valid_to（stamp_to）。
///
/// 统一更新路径：旧 tip 的失效点 = 新事实的生效起点（new_valid_from，即 DESIGN Profile
/// 「旧 valid_to = 新 valid_from」），而非墙钟 `now` —— 否则 `as_of=now` 仍会命中已被
/// 取代的旧事实（端点闭合导致旧 tip 在 `now` 时刻仍“有效”）。
/// 边界规则：
/// - 提供 new_valid_from（非空）→ 以它为候选边界；
/// - 旧 valid_to 已存在且早于候选边界（旧事实本就更早结束）→ 保留旧值，不回拨/不扩展；
/// - 否则 → stamp 为候选边界（new_valid_from 或 `now` 兜底）。
/// 语义：旧事实在「新事实开始生效」那一刻停止为真，且绝不把旧事实有效期拉长或回拨。
pub fn compute_stamp_to_boundary(
    old_valid_to: Option<&str>,
    now: &str,
    new_valid_from: Option<&str>,
) -> String {
    let boundary = match new_valid_from {
        Some(b) if !b.is_empty() => b,
        _ => now,
    };
    match old_valid_to {
        Some(ovt) if !ovt.is_empty() && ovt < boundary => ovt.to_string(),
        _ => boundary.to_string(),
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
/// `new_valid_from`：新事实的生效起点；旧 tip 的 valid_to 据此边界关闭（PR3 双时态补洞）。
fn apply_supersede_in_tx(
    conn: &Connection,
    new_id: &str,
    target_id: &str,
    namespace: &str,
    now: &str,
    relation_type: &str,
    evidence: &str,
    new_valid_from: Option<&str>,
) -> Result<(), String> {
    let old_vt: Option<String> = conn
        .query_row(
            "SELECT valid_to FROM memories WHERE id = ?",
            rusqlite::params![target_id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    let stamp_to = compute_stamp_to_boundary(old_vt.as_deref(), now, new_valid_from);
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
        pool, content, category, importance, source, namespace, tags, None, None, valid_from,
        valid_to, None, None, None, None, None, None,
    )?;
    Ok(result.id)
}

/// **Deprecated (O2)**：不再作为 P0 写入主路径。保留函数以免外部调用方编译断裂；
/// 调用方应改用 tags `occurred:YYYY-MM-DD`。本函数仍写旧列（只读兼容遗留数据），
/// 但 MCP `memory_remember` 已不再调用。
pub fn set_event_time(pool: &SqlitePool, memory_id: &str, event_time: &str) -> Result<(), String> {
    eprintln!(
        "[Memoria] WARN: set_event_time deprecated (O2); prefer tags occurred:YYYY-MM-DD (id={})",
        memory_id
    );
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let _ = conn.execute(
        "UPDATE memories SET event_time = ? WHERE id = ?",
        rusqlite::params![event_time, memory_id],
    );
    Ok(())
}

/// 三路统一的向量持久化+索引 helper（此前复制粘贴的 if/else-if 链让失败语义
/// 漂移，抽单点防未来只改一路）：put 失败短路 add（防 memory-only 向量）；
/// add 失败记录并注明重启 rebuild 自愈。
/// #R69 documentation/medium（自相矛盾注释重写）：本函数返回 ()，不传播任何
/// 状态——semantic_related 边的门控由 edge_refresh 统一负责（#R67/#R68 契约）：
/// **以持久向量存在为准**（put 成功即建边，add 失败由重启 rebuild 对齐）；
/// 此前注释残留"返回 put+add 是否都成功（edge 门控）"与 "semantic_related 边仅
/// put+add 都成功才建" 均已过时（edge_refresh 在 add 失败时仍建边），误导
/// 维护者依赖不存在的返回值或错误假设 add 失败阻断建边。
fn persist_and_index(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
    id: &str,
    ns: &str,
    qv: &[f32],
) {
    let put_ok = match crate::vector::persist::put_stored_vector(pool, id, ns, qv) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[remember] put_stored_vector failed for {id}: {e}");
            false
        }
    };
    if put_ok
        && let Err(e) = hnsw.add(&[VectorEntry {
            id: id.to_string(),
            vector: qv.to_vec(),
        }])
    {
        // put 成功 add 失败：向量已持久化但内存索引缺失；外层
        // get_stored_vector 守卫会让后续 remember 跳过 add（发散到重启，
        // #R65：长期运行服务可能持续缺失——重启 rebuild 对齐权威表自愈）。
        eprintln!("[remember] hnsw add failed for {id}: {e} (index rebuild at next start reconciles)");
    }
}

/// #R66：semantic_related 边的**幂等刷新**——is_none 守卫之外统一调用（已存在
/// 向量的记忆也应重算邻接，删除旧出边 + 按当前 HNSW 邻域重算）；失败可见。
/// #R67/#R68 门控契约：**以持久向量存在为准**——put 失败（无持久向量）跳过建边
/// （防 dangling edges）；put 成功但 add 失败（向量持久、内存索引缺失）仍建边
/// （add 失败由下次启动 rebuild 对齐；此时边基于持久向量邻域，重启后一致）。
fn edge_refresh(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
    id: &str,
    ns: &str,
    qv: &[f32],
) {
    // #R69 performance/low：轻量存在性探测（SELECT 1，不解码全向量）——此前
    // get_stored_vector 拉 BLOB + 解码成 DIM 长 Vec<f32> 只为判存在；此路径在
    // near_dup_enabled 且存在候选向量时每 remember_with_dedup 都执行。
    if !crate::vector::persist::stored_vector_exists(pool, id) {
        eprintln!(
            "[remember] edge_refresh skipped for {id}: no persisted vector (put/add failed earlier)"
        );
        return;
    }
    if let Err(e) = crate::search::semantic_edges::upsert_semantic_edges_for(pool, hnsw, id, ns, qv)
    {
        eprintln!("[remember] upsert_semantic_edges failed for {id}: {e}");
    }
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
    actor: Option<&str>,
    memory_type: Option<&str>,
    parent_id: Option<&str>,
    raw_ref: Option<&str>,
) -> Result<RememberResult, String> {
    let relation_type = normalize_memory_relation(relation)?;
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

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
    // P2.2c：写入前把确定性 text_signals 落到 tags（O2：occurred 仍走 tags，不写 event_time 列）
    let occurred_for_signals = crate::tools::ledger::parse_occurred_tag(&tags_safe);
    let tags_safe = crate::search::text_signals::merge_signal_tags(
        &tags_safe,
        content,
        occurred_for_signals.as_deref(),
    );

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
                valid_from,
            )?;
            tx.commit().map_err(|e| format!("commit: {}", e))?;

            if near_dup_enabled() {
                if let (Some(hnsw_idx), Some(qv)) = (hnsw, &candidate_vector) {
                    if crate::vector::persist::get_stored_vector(pool, &mem_id).is_none() {
                        // #R63 maintainability/medium：**与 updated 路径同款失败
                        // 处理**——put 失败短路 add（瞬态 BUSY 留 memory-only 向量、
                        // 重启后消失）；失败可见（低频异常直接 eprintln）。
                        // #R65：共享 helper（put 失败短路 add）。
                        persist_and_index(pool, hnsw_idx, &mem_id, namespace, qv);
                    }
                    // #R66 bug/medium：edge 刷新在 is_none 守卫**之外**——已存在
                    // 向量的记忆也需重算邻接（幂等维护）；移入 helper 且只在
                    // is_none 内调用会让存量记忆的边永不刷新。
                    edge_refresh(pool, hnsw_idx, &mem_id, namespace, qv);
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
                    // #R61 maintainability/medium：**失败可见**（put 失败短路 add，
                    // 见 persist_and_index doc）；#R65：共享 helper。
                    persist_and_index(pool, hnsw_idx, &mem_id, namespace, qv);
                }
                // #R66：edge 刷新在 is_none 之外（幂等维护，同 superseded 路）。
                edge_refresh(pool, hnsw_idx, &mem_id, namespace, qv);
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

    let (content_to_store, raw_ref_to_store) = match raw_ref {
        Some(r) => (content.to_string(), Some(r.to_string())),
        None => distill(content),
    };
    tx.execute(
        "INSERT INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor, tags, valid_from, valid_to,
         actor, memory_type, parent_id, raw_ref)
         VALUES (?, ?, ?, ?, ?, 0.8, 0, ?, 'hot', ?, 1.0, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            mem_id,
            namespace,
            source,
            content_to_store,
            category,
            now,
            importance,
            tags_safe,
            valid_from_val,
            valid_to_val,
            actor,
            memory_type,
            parent_id,
            raw_ref_to_store
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
                                    let stamp_to = compute_stamp_to_boundary(
                                        old_vt.as_deref(),
                                        &now,
                                        Some(valid_from_val),
                                    );
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
            Some(valid_from_val),
        )?;
        superseded_ids.push(target_id.to_string());
        explicit_superseded = true;
    }

    tx.commit().map_err(|e| format!("commit: {}", e))?;

    // 向量持久化在事务外（非 tip 权威）；失败不回滚记忆写入
    // #R64 maintainability/medium：创建路径与 exact/superseded 路径同款失败处理
    // （put 失败短路 add；edge upsert 仅 put+add 成功时跑——无向量的记忆不该有
    // 图边，且失败可见）。
    if near_dup_enabled() {
        if let (Some(hnsw_idx), Some(qv)) = (hnsw, candidate_vector.as_ref()) {
            // #R65：共享 helper（put 失败短路 add）。
            persist_and_index(pool, hnsw_idx, &mem_id, namespace, qv);
            // #R66：edge 刷新（创建路径无存量向量——helper 后直接刷新，三路统一）。
            edge_refresh(pool, hnsw_idx, &mem_id, namespace, qv);
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
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
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
    // 保留事实（keep_id）的生效起点作为被合并事实的失效边界：合并即「旧事实在 keep 生效时失效」。
    let keep_vf: Option<String> = tx
        .query_row(
            "SELECT valid_from FROM memories WHERE id = ?",
            rusqlite::params![keep_id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    let stamp_to = compute_stamp_to_boundary(old_vt.as_deref(), &now, keep_vf.as_deref());

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
