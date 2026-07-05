//! Web API 端点 — 替代旧版 Python server.py 的 /stats /graph /decay_timeline /api/memories
//!
//! 与 React SPA (web/ 目录) 直接对接，JSON 结构与旧版 Python 保持一致。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;
use serde_json::{json, Value};

use crate::storage::SqlitePool;

/// App state shared across web API handlers
pub struct WebApiState {
    pub pool: SqlitePool,
}

/// 构建 Web API 路由
pub fn build_web_api_routes(state: Arc<WebApiState>) -> Router {
    Router::new()
        .route("/stats", get(api_stats))
        .route("/graph", get(api_graph))
        .route("/decay_timeline", get(api_decay_timeline))
        .route("/api/memories", get(api_list_memories))
        .route("/api/memories/{id}", get(api_get_memory))
        .route("/api/relations", get(api_list_relations))
        .with_state(state)
}

// ── /stats ──
#[derive(Deserialize)]
pub struct StatsQuery {
    namespace: Option<String>,
}

async fn api_stats(
    State(state): State<Arc<WebApiState>>,
    Query(q): Query<StatsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let ns = q.namespace.as_deref().unwrap_or("default");
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);
    let hot: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories WHERE tier='hot' AND namespace=?1", [ns], |r| r.get(0))
        .unwrap_or(0);
    let warm: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories WHERE tier='warm' AND namespace=?1", [ns], |r| r.get(0))
        .unwrap_or(0);
    let cold: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories WHERE tier='cold' AND namespace=?1", [ns], |r| r.get(0))
        .unwrap_or(0);
    let decisions: i64 = conn
        .query_row("SELECT COUNT(*) FROM decisions", [], |r| r.get(0))
        .unwrap_or(0);
    let relations: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_relations", [], |r| r.get(0))
        .unwrap_or(0);

    let db_size: f64 = std::fs::metadata(
        std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| String::new())
    ).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);

    Ok(Json(json!({
        "sessions": sessions,
        "messages": messages,
        "source_count": 0,
        "memories": { "hot": hot, "warm": warm, "cold": cold },
        "decisions": decisions,
        "relations": relations,
        "dream": {
            "light": { "last_run": null, "processed": 0 },
            "deep": { "last_run": null, "processed": 0 },
            "rem": { "last_run": null, "processed": 0 },
        },
        "db_size_mb": (db_size * 100.0).round() / 100.0,
    })))
}

// ── /graph ──
#[derive(Deserialize)]
pub struct GraphQuery {
    namespace: Option<String>,
}

async fn api_graph(
    State(state): State<Arc<WebApiState>>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Value>, StatusCode> {
    let ns = q.namespace.as_deref().unwrap_or("default");
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // 读取 memories
    let mut stmt = conn.prepare(
        "SELECT id, content, category, tier, confidence, recall_count, importance, decay_factor, created_at
         FROM memories WHERE tier != 'forgotten' AND namespace = ?1
         ORDER BY CASE tier WHEN 'hot' THEN 0 WHEN 'warm' THEN 1 ELSE 2 END, decay_factor DESC
         LIMIT 200"
    ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut nodes = Vec::new();
    let mut node_data = Vec::new();

    let rows = stmt.query_map([ns], |r| {
        let id: String = r.get(0)?;
        let content: String = r.get(1)?;
        let category: String = r.get(2)?;
        let tier: String = r.get(3)?;
        let decay: f64 = r.get::<_, f64>(7).unwrap_or(1.0);
        let importance: i32 = r.get::<_, i32>(8).unwrap_or(3);
        Ok((id, content, category, tier, decay, importance))
    }).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for row in rows {
        if let Ok((id, content, category, tier, decay, importance)) = row {
            let cat_color = match category.as_str() {
                "candidate" => "#94a3b8",
                "decision" => "#3b82f6",
                "preference" => "#f59e0b",
                "constraint" => "#ef4444",
                "lesson" => "#8b5cf6",
                "fact" => "#10b981",
                "correction" => "#ec4899",
                _ => "#6b7280",
            };
            let tier_size = match tier.as_str() {
                "hot" => 24,
                "warm" => 16,
                "cold" => 10,
                _ => 8,
            };
            let label = if content.chars().count() > 60 {
                let truncated: String = content.chars().take(60).collect();
                format!("{}...", truncated)
            } else {
                content.clone()
            };
            let opacity = 0.3 + (decay as f32) * 0.7;

            nodes.push(json!({
                "id": id,
                "label": label,
                "title": content.chars().take(200).collect::<String>(),
                "group": tier,
                "category": category,
                "color": cat_color,
                "size": tier_size,
                "opacity": opacity,
                "decay": (decay * 100.0).round() / 100.0,
                "importance": importance,
            }));
            node_data.push((id, content));
        }
    }

    // Jaccard 边计算（简化版：关键词提取器使用常用分词）
    fn extract_keywords(text: &str) -> Vec<String> {
        let mut words = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        // 2-4字中文词组
        for i in 0..chars.len().saturating_sub(1) {
            if chars[i] >= '\u{4e00}' && chars[i] <= '\u{9fff}' {
                for len in 2..=4.min(chars.len() - i) {
                    let w: String = chars[i..i+len].iter().collect();
                    if w.chars().all(|c| c >= '\u{4e00}' && c <= '\u{9fff}') {
                        words.push(w);
                    }
                }
            }
        }
        words.sort();
        words.dedup();
        words
    }

    let kw_map: Vec<(String, Vec<String>)> = node_data.iter()
        .map(|(id, content)| (id.clone(), extract_keywords(content)))
        .collect();

    let mut edges = Vec::new();
    for i in 0..kw_map.len() {
        for j in (i+1)..kw_map.len() {
            let kwi = &kw_map[i].1;
            let kwj = &kw_map[j].1;
            if kwi.is_empty() || kwj.is_empty() { continue; }

            let intersection: usize = kwi.iter().filter(|w| kwj.contains(w)).count();
            let union: usize = kwi.len() + kwj.len() - intersection;
            if union == 0 { continue; }

            let jaccard = intersection as f64 / union as f64;
            if jaccard >= 0.25 {
                edges.push(json!({
                    "from": kw_map[i].0,
                    "to": kw_map[j].0,
                    "value": (jaccard * 100.0).round() / 100.0,
                    "title": format!("相似度: {}%", (jaccard * 100.0).round() as i32),
                }));
            }
        }
    }
    edges.sort_by(|a, b| {
        b.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0)
            .partial_cmp(&a.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    edges.truncate(300);

    let hot_count = nodes.iter().filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("hot")).count();
    let warm_count = nodes.iter().filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("warm")).count();
    let cold_count = nodes.iter().filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("cold")).count();

    Ok(Json(json!({
        "nodes": nodes,
        "edges": edges,
        "summary": {
            "total_nodes": nodes.len(),
            "total_edges": edges.len(),
            "hot": hot_count,
            "warm": warm_count,
            "cold": cold_count,
        },
    })))
}

// ── /decay_timeline ──
#[derive(Deserialize)]
pub struct DecayQuery {
    days: Option<i64>,
    namespace: Option<String>,
}

async fn api_decay_timeline(
    State(state): State<Arc<WebApiState>>,
    Query(q): Query<DecayQuery>,
) -> Result<Json<Vec<Value>>, StatusCode> {
    let _ns = q.namespace.as_deref().unwrap_or("default");
    let _days = q.days.unwrap_or(90);
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let mut stmt = conn.prepare(
        "SELECT memory_id, old_tier, new_tier, old_decay, new_decay, reason, logged_at
         FROM decay_log ORDER BY logged_at DESC LIMIT 500"
    ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut results = Vec::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3).unwrap_or(0.5),
            r.get::<_, f64>(4).unwrap_or(0.5),
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
        ))
    }).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for row in rows {
        if let Ok((mid, old_tier, new_tier, old_decay, new_decay, reason, date)) = row {
            results.push(json!({
                "memory_id": mid,
                "old_tier": old_tier,
                "new_tier": new_tier,
                "old_decay": (old_decay * 10000.0).round() / 10000.0,
                "new_decay": (new_decay * 10000.0).round() / 10000.0,
                "reason": reason,
                "date": date,
            }));
        }
    }

    Ok(Json(results))
}

// ── /api/memories ──
#[derive(Deserialize)]
pub struct MemoriesQuery {
    namespace: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    tier: Option<String>,
    category: Option<String>,
    q: Option<String>,
}

async fn api_list_memories(
    State(state): State<Arc<WebApiState>>,
    Query(q): Query<MemoriesQuery>,
) -> Result<Json<Value>, StatusCode> {
    let ns = q.namespace.as_deref().unwrap_or("default");
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let mut sql = "SELECT id, content, category, tier, importance, decay_factor, recall_count, created_at, namespace FROM memories WHERE namespace = ?1".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(ns.to_string())];

    if let Some(ref t) = q.tier {
        sql.push_str(" AND tier = ?2");
        params.push(Box::new(t.clone()));
    }
    if let Some(ref cat) = q.category {
        let idx = params.len() + 1;
        sql.push_str(&format!(" AND category = ?{}", idx));
        params.push(Box::new(cat.clone()));
    }
    if let Some(ref query) = q.q {
        if !query.is_empty() {
            let idx = params.len() + 1;
            sql.push_str(&format!(" AND (content LIKE ?{} OR id LIKE ?{})", idx, idx));
            params.push(Box::new(format!("%{}%", query)));
        }
    }

    sql.push_str(" ORDER BY CASE tier WHEN 'hot' THEN 0 WHEN 'warm' THEN 1 ELSE 2 END, decay_factor DESC LIMIT ?");
    let limit_idx = params.len() + 1;
    sql.push_str(&limit_idx.to_string());
    params.push(Box::new(limit));

    sql.push_str(" OFFSET ?");
    let offset_idx = params.len() + 1;
    sql.push_str(&offset_idx.to_string());
    params.push(Box::new(offset));

    let mut stmt = conn.prepare(&sql).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        let id: String = r.get(0)?;
        let content: String = r.get(1)?;
        let category: String = r.get(2)?;
        let tier: String = r.get(3)?;
        let importance: i32 = r.get::<_, i32>(4).unwrap_or(3);
        let decay: f64 = r.get::<_, f64>(5).unwrap_or(1.0);
        let recall: i32 = r.get::<_, i32>(6).unwrap_or(0);
        let created: String = r.get::<_, String>(7).unwrap_or_default();
        let ns_val: String = r.get::<_, String>(8).unwrap_or_default();
        Ok((id, content, category, tier, importance, decay, recall, created, ns_val))
    }).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let memories: Vec<Value> = rows.filter_map(|r| r.ok()).map(|(id, content, category, tier, importance, decay, recall, created, ns_val)| {
        json!({
            "id": id,
            "content": content,
            "category": category,
            "tier": tier,
            "importance": importance,
            "decay_factor": (decay * 100.0).round() / 100.0,
            "recall_count": recall,
            "created_at": created,
            "namespace": ns_val,
        })
    }).collect();

    Ok(Json(json!({"memories": memories, "total": memories.len()})))
}

// ── /api/memories/:id ──
async fn api_get_memory(
    State(state): State<Arc<WebApiState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let result = conn.query_row(
        "SELECT id, content, category, tier, importance, decay_factor, recall_count, created_at, namespace FROM memories WHERE id = ?1",
        [&id],
        |r| {
            let id: String = r.get(0)?;
            let content: String = r.get(1)?;
            let category: String = r.get(2)?;
            let tier: String = r.get(3)?;
            let importance: i32 = r.get::<_, i32>(4).unwrap_or(3);
            let decay: f64 = r.get::<_, f64>(5).unwrap_or(1.0);
            let recall: i32 = r.get::<_, i32>(6).unwrap_or(0);
            let created: String = r.get::<_, String>(7).unwrap_or_default();
            let ns: String = r.get::<_, String>(8).unwrap_or_default();
            Ok(json!({
                "id": id, "content": content, "category": category, "tier": tier,
                "importance": importance, "decay_factor": decay, "recall_count": recall,
                "created_at": created, "namespace": ns,
            }))
        }
    );
    match result {
        Ok(memory) => Ok(Json(memory)),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

// ── /api/relations ──
async fn api_list_relations(
    State(state): State<Arc<WebApiState>>,
) -> Result<Json<Value>, StatusCode> {
    let conn = state.pool.get().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut stmt = conn.prepare(
        "SELECT id, source_id, target_id, relation_type, weight, created_at FROM memory_relations LIMIT 200"
    ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let relations: Vec<Value> = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "source_id": r.get::<_, String>(1)?,
            "target_id": r.get::<_, String>(2)?,
            "relation_type": r.get::<_, String>(3)?,
            "weight": r.get::<_, f64>(4).unwrap_or(0.0),
            "created_at": r.get::<_, String>(5).unwrap_or_default(),
        }))
    }).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .filter_map(|r| r.ok())
    .collect();

    Ok(Json(json!({"relations": relations, "total": relations.len()})))
}
