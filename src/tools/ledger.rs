//! Ledger enrichment — 吸收 HMS 的「类型化证据账本」思想（Phase A / P0+）。
//!
//! 把 hybrid_search 返回的 `FusedResult` 富化为结构化「证据账本行」，每行携带：
//! `type`(=category) / `occurred`(=event_time 或兜底 valid_from) / `mentioned`(=valid_from) /
//! `source_ref`(=`namespace:id`) / `entities`(该记忆提及的实体)。
//! 这与 HMS `EvidenceLedgerRow` 字段语义对齐，但数据源来自 Memoria 既有 supersede + 实体图谱，
//! 不改动存储层，只在召回组装处批量回查。

use crate::search::rrf::FusedResult;
use crate::storage::SqlitePool;
use serde_json::json;
use std::collections::HashMap;

/// 单条记忆的轻量元数据（批量回查用）。
struct MemMeta {
    category: String,
    valid_from: String,
    event_time: String,
}

/// 批量回查 memories 的 (category, valid_from, event_time)。
/// 用一条 `id IN (...)` 查询替代逐条查询，控制 DB 往返。
fn fetch_memory_meta(
    pool: &SqlitePool,
    ids: &[String],
) -> HashMap<String, MemMeta> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, category, valid_from, event_time FROM memories WHERE id IN ({})",
        ph
    );
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        ))
    });
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            out.insert(
                row.0.clone(),
                MemMeta {
                    category: row.1,
                    valid_from: row.2,
                    event_time: row.3,
                },
            );
        }
    }
    out
}

/// 批量回查每个 memory 提及的实体（name/type），按 memory_id 分组。
fn fetch_entities_for_memories(
    pool: &SqlitePool,
    ids: &[String],
    namespace: &str,
) -> HashMap<String, Vec<serde_json::Value>> {
    let mut out: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT m.memory_id, e.entity_type, e.name
         FROM entities e
         JOIN entity_mentions m ON m.entity_id = e.id
         WHERE m.memory_id IN ({}) AND m.namespace = e.namespace AND m.namespace = ?",
        ph
    );
    let mut params: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    params.push(&namespace as &dyn rusqlite::ToSql);
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        ))
    });
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            out.entry(row.0).or_default().push(json!({
                "entity_type": row.1,
                "name": row.2,
            }));
        }
    }
    out
}

/// 把召回结果富化为类型化证据账本。
///
/// 返回的每条 JSON 同时保留旧字段（`memory_id` / `content` / `rrf_score` / `source`，
/// 供既有 `prompt_block` 与 agent-core 消费）与新增账本字段，向后兼容。
pub fn enrich_ledger(
    pool: &SqlitePool,
    namespace: &str,
    fused: &[FusedResult],
) -> Vec<serde_json::Value> {
    if fused.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = fused.iter().map(|f| f.memory_id.clone()).collect();
    let meta = fetch_memory_meta(pool, &ids);
    let entities_map = fetch_entities_for_memories(pool, &ids, namespace);

    fused
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let m = meta.get(&f.memory_id);
            let category = m.map(|x| x.category.clone()).unwrap_or_default();
            let valid_from = m.map(|x| x.valid_from.clone()).unwrap_or_default();
            let event_time = m.map(|x| x.event_time.clone()).unwrap_or_default();
            // occurred：事件发生时刻；缺省以 valid_from（断言时刻）兜底
            let occurred = if event_time.is_empty() {
                valid_from.clone()
            } else {
                event_time
            };
            let entities = entities_map
                .get(&f.memory_id)
                .cloned()
                .unwrap_or_default();
            json!({
                "index": i + 1,
                "memory_id": f.memory_id,
                "content": f.content,
                "rrf_score": f.rrf_score,
                "source": f.source,
                "type": category,
                "occurred": occurred,
                "mentioned": valid_from,
                "source_ref": format!("{}:{}", namespace, f.memory_id),
                "entities": entities,
                "is_latest": true,
            })
        })
        .collect()
}
