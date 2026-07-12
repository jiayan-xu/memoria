//! Knowledge graph — entity-based relation edges and graph queries.
//! Phase B: replaces the old heuristic prefix-matching `build_graph`.
//!
//! The engine stores edges in `entity_edges` table (inserted by NER / manual tools).
//! This module reads from that table to return graph data for visualization.

use crate::storage::SqlitePool;
use serde_json::json;
use std::collections::HashMap;

/// 受控关系类型枚举（P2-3）。
/// 边类型必须是其中之一，防止关系类型爆炸 / 垃圾关系污染图谱。
pub const RELATION_TYPES: &[&str] = &[
    "related_to", "uses", "depends_on", "mentions", "similar_to",
    "part_of", "belongs_to", "contains", "member_of", "works_at",
    "located_in", "founded_by", "owns", "authors", "collaborates_with",
    "antagonist_of", "spawned_by", "triggers",
];

/// 校验关系类型是否在受控枚举内。
pub fn is_valid_relation_type(s: &str) -> bool {
    RELATION_TYPES.contains(&s)
}

/// 返回受控关系类型列表（逗号分隔），用于错误提示与工具 schema。
pub fn relation_type_list() -> String {
    RELATION_TYPES.join(", ")
}

/// Legacy entry point (backward compatible with `memory_graph` MCP tool).
/// Now reads from `entity_edges` + `entities` instead of heuristic matches.
/// Returns (nodes_count, edges_count).
pub fn build_graph(
    pool: &SqlitePool,
    namespace: &str,
    _batch_size: u32,  // kept for backward compat
) -> Result<(u32, u32), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // Count existing entity edges
    let edges: u32 = conn.query_row(
        "SELECT COUNT(*) FROM entity_edges WHERE namespace = ?1",
        rusqlite::params![namespace],
        |r| r.get::<_, u32>(0),
    ).map_err(|e| format!("count edges: {}", e))?;

    let nodes: u32 = conn.query_row(
        "SELECT COUNT(*) FROM entities WHERE namespace = ?1",
        rusqlite::params![namespace],
        |r| r.get::<_, u32>(0),
    ).map_err(|e| format!("count nodes: {}", e))?;

    Ok((nodes, edges))
}

/// Search entities by name / aliases / summary / mention-context,
/// returning per-entity `mentions_count`. All matches are namespace-scoped.
///
/// This is the testable core behind the `entity_search` MCP tool.
pub fn search_entities(
    pool: &SqlitePool,
    namespace: &str,
    query: &str,
    entity_type: Option<&str>,
    max_results: i64,
) -> Result<Vec<serde_json::Value>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let like = format!("%{}%", query);

    // mention-context subquery lets a query hit entities that never appear in
    // their own name/summary but are referenced in a memory's surrounding text.
    let mention_hit = "e.id IN (SELECT entity_id FROM entity_mentions WHERE namespace = ?1 AND context LIKE ?p)";

    let rows: Vec<serde_json::Value> = if let Some(et) = entity_type {
        let sql = format!(
            "SELECT e.id, e.entity_type, e.name, e.aliases, e.summary,
                    (SELECT COUNT(*) FROM entity_mentions m WHERE m.entity_id = e.id AND m.namespace = ?1) AS mc
             FROM entities e
             WHERE e.namespace = ?1 AND e.entity_type = ?2
               AND (e.name LIKE ?3 OR e.aliases LIKE ?3 OR e.summary LIKE ?3
                    OR {mention})
             ORDER BY mc DESC, e.name LIMIT ?4",
            mention = mention_hit.replace("?p", "?3"),
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("prep: {}", e))?;
        stmt.query_map(rusqlite::params![namespace, et, like, max_results], |r| {
            Ok(entity_row(r))
        }).map_err(|e| format!("query: {}", e))?
        .flatten().collect()
    } else {
        let sql = format!(
            "SELECT e.id, e.entity_type, e.name, e.aliases, e.summary,
                    (SELECT COUNT(*) FROM entity_mentions m WHERE m.entity_id = e.id AND m.namespace = ?1) AS mc
             FROM entities e
             WHERE e.namespace = ?1
               AND (e.name LIKE ?2 OR e.aliases LIKE ?2 OR e.summary LIKE ?2
                    OR {mention})
             ORDER BY mc DESC, e.name LIMIT ?3",
            mention = mention_hit.replace("?p", "?2"),
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("prep: {}", e))?;
        stmt.query_map(rusqlite::params![namespace, like, max_results], |r| {
            Ok(entity_row(r))
        }).map_err(|e| format!("query: {}", e))?
        .flatten().collect()
    };

    Ok(rows)
}

/// Map a result row (id, entity_type, name, aliases, summary, mentions_count) to JSON.
fn entity_row(r: &rusqlite::Row) -> serde_json::Value {
    json!({
        "id": r.get::<_, String>(0).unwrap_or_default(),
        "entity_type": r.get::<_, String>(1).unwrap_or_default(),
        "name": r.get::<_, String>(2).unwrap_or_default(),
        "aliases": r.get::<_, Option<String>>(3).unwrap_or(None),
        "summary": r.get::<_, Option<String>>(4).unwrap_or(None),
        "mentions_count": r.get::<_, i64>(5).unwrap_or(0),
    })
}

/// Export full graph as JSON payload (nodes + edges) for the frontend.
/// Each node carries its `mentions` (memory evidence) so the UI can drill
/// down from an entity to the memories that reference it (P2-3).
pub fn export_graph(
    pool: &SqlitePool,
    namespace: &str,
) -> Result<serde_json::Value, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // Nodes
    let mut stmt = conn.prepare(
        "SELECT id, entity_type, name, aliases, summary, created_at
         FROM entities WHERE namespace = ?1
         ORDER BY name"
    ).map_err(|e| format!("prep nodes: {}", e))?;
    let nodes_rows: Vec<(String, String, String, Option<String>, Option<String>, Option<String>)> = stmt.query_map(
        rusqlite::params![namespace],
        |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        )),
    ).map_err(|e| format!("query nodes: {}", e))?
    .flatten().collect();

    // Mentions (all for the namespace, grouped in Rust to avoid N+1 queries)
    let mut mstmt = conn.prepare(
        "SELECT entity_id, memory_id, context FROM entity_mentions
         WHERE namespace = ?1 ORDER BY id DESC"
    ).map_err(|e| format!("prep mentions: {}", e))?;
    let mentions: Vec<(String, String, Option<String>)> = mstmt.query_map(
        rusqlite::params![namespace],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)),
    ).map_err(|e| format!("query mentions: {}", e))?
    .flatten().collect();

    let mut mentions_by_entity: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for (eid, mid, ctx) in mentions {
        let arr = mentions_by_entity.entry(eid).or_default();
        if arr.len() < 20 {
            arr.push(json!({ "memory_id": mid, "context": ctx }));
        }
    }

    let nodes: Vec<serde_json::Value> = nodes_rows.into_iter().map(|(id, et, name, al, su, ca)| {
        json!({
            "id": id,
            "type": et,
            "label": name,
            "aliases": al,
            "summary": su,
            "created_at": ca,
            "mentions": mentions_by_entity.get(&id).cloned().unwrap_or_default(),
        })
    }).collect();

    // Edges
    let mut stmt2 = conn.prepare(
        "SELECT source_entity_id, target_entity_id, relation_type, weight, evidence
         FROM entity_edges WHERE namespace = ?1
         ORDER BY weight DESC"
    ).map_err(|e| format!("prep edges: {}", e))?;
    let edges: Vec<serde_json::Value> = stmt2.query_map(
        rusqlite::params![namespace],
        |r| Ok(json!({
            "source": r.get::<_, String>(0)?,
            "target": r.get::<_, String>(1)?,
            "relation": r.get::<_, String>(2)?,
            "weight": r.get::<_, f64>(3)?,
            "evidence": r.get::<_, Option<String>>(4)?,
        })),
    ).map_err(|e| format!("query edges: {}", e))?
    .flatten().collect();

    Ok(json!({
        "nodes": nodes,
        "edges": edges,
        "stats": {"node_count": nodes.len(), "edge_count": edges.len()}
    }))
}
