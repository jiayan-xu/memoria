//! Knowledge graph — entity-based relation edges and graph queries.
//! Phase B: replaces the old heuristic prefix-matching `build_graph`.
//!
//! The engine stores edges in `entity_edges` table (inserted by NER / manual tools).
//! This module reads from that table to return graph data for visualization.

use crate::storage::SqlitePool;

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

/// Export full graph as JSON payload (nodes + edges) for the frontend.
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
    let nodes: Vec<serde_json::Value> = stmt.query_map(
        rusqlite::params![namespace],
        |r| Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?,
            "type": r.get::<_, String>(1)?,
            "label": r.get::<_, String>(2)?,
            "aliases": r.get::<_, Option<String>>(3)?,
            "summary": r.get::<_, Option<String>>(4)?,
            "created_at": r.get::<_, Option<String>>(5)?,
        })),
    ).map_err(|e| format!("query nodes: {}", e))?
    .flatten().collect();

    // Edges
    let mut stmt2 = conn.prepare(
        "SELECT source_entity_id, target_entity_id, relation_type, weight, evidence
         FROM entity_edges WHERE namespace = ?1
         ORDER BY weight DESC"
    ).map_err(|e| format!("prep edges: {}", e))?;
    let edges: Vec<serde_json::Value> = stmt2.query_map(
        rusqlite::params![namespace],
        |r| Ok(serde_json::json!({
            "source": r.get::<_, String>(0)?,
            "target": r.get::<_, String>(1)?,
            "relation": r.get::<_, String>(2)?,
            "weight": r.get::<_, f64>(3)?,
            "evidence": r.get::<_, Option<String>>(4)?,
        })),
    ).map_err(|e| format!("query edges: {}", e))?
    .flatten().collect();

    Ok(serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "stats": {"node_count": nodes.len(), "edge_count": edges.len()}
    }))
}
