//! Web API 端点 — 替代旧版 Python server.py 的 /stats /graph /decay_timeline /api/memories
//!
//! 与 React SPA (web/ 目录) 直接对接，JSON 结构与旧版 Python 保持一致。

use axum::{
    Json, Router,
    extract::{Extension, Multipart, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::auth::{self, AuthResult};
use crate::document::{self, DEFAULT_DEPT_NS};
use crate::storage::SqlitePool;

/// App state shared across web API handlers
pub struct WebApiState {
    pub pool: SqlitePool,
    pub auth_pool: SqlitePool,
    /// 文档二进制根目录（通常为 data/，其下 documents/…）
    pub doc_dir: PathBuf,
}

fn json_err(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": detail.into() }))).into_response()
}

/// Auth middleware: 校验 X-Agent-Id / X-Agent-Key
async fn auth_middleware(
    State(state): State<Arc<WebApiState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let agent_id = request
        .headers()
        .get("X-Agent-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let badge_token = request
        .headers()
        .get("X-Agent-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if agent_id.is_empty() || badge_token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let auth = auth::authenticate(&state.auth_pool, &agent_id, &badge_token)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    // P1-1：注入认证结果。注意：必须在 next.run 前插入；且勿在持有 headers 借用时插入。
    request.extensions_mut().insert(auth);
    Ok(next.run(request).await)
}

/// 构建 Web API 路由（所有路由需要 X-Agent-Id / X-Agent-Key 认证）
pub fn build_web_api_routes(state: Arc<WebApiState>) -> Router {
    Router::new()
        .route("/stats", get(api_stats))
        .route("/graph", get(api_graph))
        .route("/decay_timeline", get(api_decay_timeline))
        // 单条 CRUD 一律走 query ?id=（/{id} 动态段在本服务 merge+layer 下 handler 不执行，见证据）
        .route(
            "/api/memories",
            get(api_list_memories)
                .put(api_update_memory)
                .delete(api_delete_memory),
        )
        .route("/api/relations", get(api_list_relations))
        .route(
            "/api/documents",
            get(api_list_documents).post(api_upload_document),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

// ── /api/documents ──

#[derive(Deserialize)]
pub struct DocumentsQuery {
    namespace: Option<String>,
    limit: Option<i64>,
}

/// 列出文档清单（memory_type=document 且 parent_id 为空）
async fn api_list_documents(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<DocumentsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let ns = q
        .namespace
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_DEPT_NS);
    if !auth::check_ns_access(&auth, ns) {
        return Err(StatusCode::FORBIDDEN);
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, content, raw_ref, created_at, source, tags
             FROM memories
             WHERE namespace = ?1
               AND memory_type = 'document'
               AND (parent_id IS NULL OR parent_id = '')
             ORDER BY created_at DESC
             LIMIT ?2",
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = stmt
        .query_map(rusqlite::params![ns, limit], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "content": r.get::<_, String>(1)?,
                "raw_ref": r.get::<_, Option<String>>(2)?,
                "created_at": r.get::<_, Option<String>>(3)?,
                "source": r.get::<_, Option<String>>(4)?,
                "tags": r.get::<_, Option<String>>(5)?,
            }))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut docs = Vec::new();
    for row in rows.flatten() {
        docs.push(row);
    }
    Ok(Json(json!({
        "namespace": ns,
        "count": docs.len(),
        "documents": docs,
        "default_dept_ns": DEFAULT_DEPT_NS,
    })))
}

/// multipart: file + namespace(可选，默认固废部门 ns)
async fn api_upload_document(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    mut multipart: Multipart,
) -> Response {
    let mut namespace = DEFAULT_DEPT_NS.to_string();
    let mut filename = String::new();
    let mut content_type: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, format!("multipart: {e}")),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "namespace" {
            if let Ok(v) = field.text().await {
                let t = v.trim().to_string();
                if !t.is_empty() {
                    namespace = t;
                }
            }
            continue;
        }
        if name == "file" || name == "document" {
            filename = field
                .file_name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "upload.bin".into());
            content_type = field.content_type().map(|m| m.to_string());
            match field.bytes().await {
                Ok(b) => bytes = Some(b.to_vec()),
                Err(e) => return json_err(StatusCode::BAD_REQUEST, format!("read file: {e}")),
            }
            continue;
        }
        // 忽略其它字段
        let _ = field.bytes().await;
    }

    let Some(bytes) = bytes else {
        return json_err(StatusCode::BAD_REQUEST, "缺少 file 字段（PDF/DOCX）");
    };
    if !auth::check_ns_access(&auth, &namespace) {
        return json_err(
            StatusCode::FORBIDDEN,
            format!("无权限写入命名空间 {namespace}"),
        );
    }
    let Some(kind) = document::detect_kind(&filename, content_type.as_deref()) else {
        return json_err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "仅支持 .pdf / .docx / .xlsx / .xls",
        );
    };

    let pool = state.pool.clone();
    let doc_dir = state.doc_dir.clone();
    let actor = auth.agent_id.clone();
    let ns = namespace.clone();
    let fname = filename.clone();
    let kind_owned = kind.to_string();

    let result = tokio::task::spawn_blocking(move || {
        document::ingest_bytes(
            &pool,
            &doc_dir,
            &ns,
            &fname,
            &kind_owned,
            &bytes,
            &actor,
        )
    })
    .await;

    match result {
        Ok(Ok(out)) => Json(json!({
            "status": "ok",
            "doc_id": out.doc_id,
            "namespace": out.namespace,
            "filename": out.filename,
            "kind": out.kind,
            "raw_ref": out.raw_ref,
            "chars": out.chars,
            "chunk_count": out.chunk_count,
            "manifest_id": out.manifest_id,
            "chunk_ids": out.chunk_ids,
        }))
        .into_response(),
        Ok(Err(e)) => json_err(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── /stats ──
#[derive(Deserialize)]
pub struct StatsQuery {
    namespace: Option<String>,
}

async fn api_stats(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<StatsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let ns = q.namespace.as_deref().unwrap_or("default");
    if !auth::check_ns_access(&auth, ns) {
        return Err(StatusCode::FORBIDDEN);
    }
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);
    let hot: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tier='hot' AND namespace=?1",
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let warm: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tier='warm' AND namespace=?1",
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let cold: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tier='cold' AND namespace=?1",
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let decisions: i64 = conn
        .query_row("SELECT COUNT(*) FROM decisions", [], |r| r.get(0))
        .unwrap_or(0);
    let relations: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_relations", [], |r| r.get(0))
        .unwrap_or(0);

    let db_size: f64 =
        std::fs::metadata(std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| String::new()))
            .map(|m| m.len() as f64 / 1048576.0)
            .unwrap_or(0.0);

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
    /// 采样上限：?limit=N 控制返回节点数（边上限=节点上限×3）。默认 200，硬顶 5000，
    /// 防止把全库十几万点一次性灌进 vis.Network 卡死浏览器。
    limit: Option<i64>,
    /// 视图：`value`（默认）= pattern/高价值类；`all` = 含 observation
    include: Option<String>,
}

/// 图谱默认高价值类（排除海量 observation 噪声）
fn graph_value_category(cat: &str) -> bool {
    matches!(
        cat,
        "pattern"
            | "fact"
            | "decision"
            | "preference"
            | "constraint"
            | "lesson"
            | "correction"
            | "infrastructure"
            | "note"
            | "report"
    )
}

const GRAPH_VALUE_SQL: &str = "category IN ('pattern','fact','decision','preference','constraint','lesson','correction','infrastructure','note','report')";

async fn api_graph(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Value>, StatusCode> {
    // 主数据 ns；旧前端 default 几乎无边，统一映射到 agent/xujiayan
    let ns_raw = q.namespace.as_deref().unwrap_or("agent/xujiayan");
    let ns = if ns_raw.is_empty() || ns_raw == "default" {
        "agent/xujiayan"
    } else {
        ns_raw
    };
    if !auth::check_ns_access(&auth, ns) {
        return Err(StatusCode::FORBIDDEN);
    }
    let value_only = !matches!(
        q.include.as_deref().unwrap_or("value"),
        "all" | "full" | "observation"
    );
    // 图谱采样上限：默认 200 节点，边上限 = 节点上限×3，均设硬顶防浏览器卡死。
    let raw_limit = q.limit.unwrap_or(200).clamp(10, 5000);
    let node_cap: usize = raw_limit as usize;
    let edge_cap: usize = (raw_limit as usize * 3).clamp(10, 15000);

    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // ── 默认：先按高价值记忆种子节点，再挂真边（避免 observation 淹没图谱）──
    let mut pending_edges: Vec<(String, String, String, f64)> = Vec::new();
    let mut id_order: Vec<String> = Vec::new();
    let mut id_set: HashSet<String> = HashSet::new();

    let seed_sql = if value_only {
        format!(
            "SELECT id FROM memories
             WHERE tier != 'forgotten' AND namespace = ?1 AND {GRAPH_VALUE_SQL}
             ORDER BY CASE category WHEN 'pattern' THEN 0 WHEN 'decision' THEN 1 WHEN 'constraint' THEN 2
               WHEN 'preference' THEN 3 WHEN 'lesson' THEN 4 WHEN 'fact' THEN 5 ELSE 6 END,
               CASE tier WHEN 'hot' THEN 0 WHEN 'warm' THEN 1 ELSE 2 END,
               importance DESC, decay_factor DESC
             LIMIT ?2"
        )
    } else {
        "SELECT id FROM memories
         WHERE tier != 'forgotten' AND namespace = ?1
         ORDER BY CASE tier WHEN 'hot' THEN 0 WHEN 'warm' THEN 1 ELSE 2 END, decay_factor DESC
         LIMIT ?2"
            .to_string()
    };
    if let Ok(mut stmt) = conn.prepare(&seed_sql) {
        if let Ok(rows) =
            stmt.query_map(rusqlite::params![ns, node_cap as i64], |r| r.get::<_, String>(0))
        {
            for id in rows.flatten() {
                if id_set.insert(id.clone()) {
                    id_order.push(id);
                }
            }
        }
    }

    // 关系边：两端都已在种子集，或（全量模式）可扩展进图
    if let Ok(mut estmt) = conn.prepare(
        "SELECT source_id, target_id, relation_type, COALESCE(weight, 0.5)
         FROM memory_relations
         WHERE namespace = ?1
           AND (
             valid_to IS NULL
             OR valid_to = ''
             OR valid_to < '1971-01-01'
             OR valid_to > datetime('now')
           )
         ORDER BY weight DESC, id DESC
         LIMIT 12000",
    ) {
        if let Ok(erows) = estmt.query_map([ns], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3).unwrap_or(0.5),
            ))
        }) {
            for row in erows.flatten() {
                let (src, tgt, rtype, weight) = row;
                if src == tgt {
                    continue;
                }
                if value_only {
                    // 仅保留两端都在高价值种子集内的边
                    if !id_set.contains(&src) || !id_set.contains(&tgt) {
                        continue;
                    }
                } else {
                    for id in [&src, &tgt] {
                        if id_set.len() < node_cap && id_set.insert((*id).clone()) {
                            id_order.push((*id).clone());
                        }
                    }
                    if !id_set.contains(&src) || !id_set.contains(&tgt) {
                        continue;
                    }
                }
                pending_edges.push((src, tgt, rtype, weight));
                if pending_edges.len() >= edge_cap {
                    break;
                }
            }
        }
    }

    // 截到 node_cap
    if id_order.len() > node_cap {
        let keep: HashSet<String> = id_order.iter().take(node_cap).cloned().collect();
        id_set = keep;
        id_order.truncate(node_cap);
    }

    let mut nodes = Vec::new();
    let mut node_data = Vec::new();
    let node_ids = id_set;
    for id in &id_order {
        let row = conn.query_row(
            "SELECT content, category, tier, decay_factor, importance
             FROM memories WHERE id = ?1 AND namespace = ?2 AND tier != 'forgotten'",
            rusqlite::params![id, ns],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3).unwrap_or(1.0),
                    r.get::<_, i32>(4).unwrap_or(3),
                ))
            },
        );
        let Ok((content, category, tier, decay, importance)) = row else {
            continue;
        };
        if value_only && !graph_value_category(&category) {
            continue;
        }
        let cat_color = match category.as_str() {
            "candidate" => "#94a3b8",
            "decision" => "#3b82f6",
            "preference" => "#f59e0b",
            "constraint" => "#ef4444",
            "lesson" => "#8b5cf6",
            "fact" => "#10b981",
            "correction" => "#ec4899",
            "pattern" => "#06b6d4",
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
            "tier": tier,
            "category": category,
            "color": cat_color,
            "size": tier_size,
            "opacity": opacity,
            "decay": (decay * 100.0).round() / 100.0,
            "importance": importance,
        }));
        node_data.push((id.clone(), content));
    }

    // 真边：两端都在节点集内
    let mut edges = Vec::new();
    let mut edge_seen: HashSet<(String, String, String)> = HashSet::new();
    for (src, tgt, rtype, weight) in pending_edges {
        if !node_ids.contains(&src) || !node_ids.contains(&tgt) {
            continue;
        }
        let (a, b) = if src <= tgt {
            (src.clone(), tgt.clone())
        } else {
            (tgt.clone(), src.clone())
        };
        if !edge_seen.insert((a, b, rtype.clone())) {
            continue;
        }
        let w = weight.clamp(0.05, 1.0);
        edges.push(json!({
            "from": src,
            "to": tgt,
            "source": src,
            "target": tgt,
            "value": (w * 100.0).round() / 100.0,
            "weight": (w * 100.0).round() / 100.0,
            "relation_type": rtype,
            "title": format!("{} · {:.0}%", rtype, w * 100.0),
        }));
        if edges.len() >= edge_cap {
            break;
        }
    }

    // 真边过少时用 Jaccard 相似度补边（标注为相似度，避免图谱完全散点）
    if edges.len() < 20 {
        fn extract_keywords(text: &str) -> Vec<String> {
            let mut words = Vec::new();
            let chars: Vec<char> = text.chars().collect();
            for i in 0..chars.len().saturating_sub(1) {
                if chars[i] >= '\u{4e00}' && chars[i] <= '\u{9fff}' {
                    for len in 2..=4.min(chars.len() - i) {
                        let w: String = chars[i..i + len].iter().collect();
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
        let kw_map: Vec<(String, Vec<String>)> = node_data
            .iter()
            .map(|(id, content)| (id.clone(), extract_keywords(content)))
            .collect();
        for i in 0..kw_map.len() {
            for j in (i + 1)..kw_map.len() {
                let kwi = &kw_map[i].1;
                let kwj = &kw_map[j].1;
                if kwi.is_empty() || kwj.is_empty() {
                    continue;
                }
                let intersection: usize = kwi.iter().filter(|w| kwj.contains(w)).count();
                let union: usize = kwi.len() + kwj.len() - intersection;
                if union == 0 {
                    continue;
                }
                let jaccard = intersection as f64 / union as f64;
                if jaccard >= 0.2 {
                    let src = &kw_map[i].0;
                    let tgt = &kw_map[j].0;
                    if edge_seen.insert((src.clone(), tgt.clone(), "similarity".into())) {
                        edges.push(json!({
                            "from": src,
                            "to": tgt,
                            "source": src,
                            "target": tgt,
                            "value": (jaccard * 100.0).round() / 100.0,
                            "weight": (jaccard * 100.0).round() / 100.0,
                            "relation_type": "similarity",
                            "title": format!("相似度: {}%", (jaccard * 100.0).round() as i32),
                        }));
                    }
                }
            }
        }
    }
    edges.sort_by(|a, b| {
        b.get("value")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .partial_cmp(&a.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    edges.truncate(edge_cap);

    let hot_count = nodes
        .iter()
        .filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("hot"))
        .count();
    let warm_count = nodes
        .iter()
        .filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("warm"))
        .count();
    let cold_count = nodes
        .iter()
        .filter(|n| n.get("group").and_then(|g| g.as_str()) == Some("cold"))
        .count();

    // 真实总量（不受采样上限影响），用于界面区分"显示数 vs 全库数"
    let total_mem: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1 AND tier != 'forgotten'",
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_value: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM memories WHERE namespace = ?1 AND tier != 'forgotten' AND {GRAPH_VALUE_SQL}"
            ),
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_rel: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_relations WHERE namespace = ?1 AND (valid_to IS NULL OR valid_to = '' OR valid_to < '1971-01-01' OR valid_to > datetime('now'))",
            [ns],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(Json(json!({
        "nodes": nodes,
        "edges": edges,
        "summary": {
            "view": if value_only { "value" } else { "all" },
            "total_nodes": nodes.len(),
            "total_edges": edges.len(),
            "total_memories": if value_only { total_value } else { total_mem },
            "total_memories_all": total_mem,
            "total_value_memories": total_value,
            "total_relations": total_rel,
            "hot": hot_count,
            "warm": warm_count,
            "cold": cold_count,
        },
    })))
}

// ── /decay_timeline ──
async fn api_decay_timeline(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
) -> Result<Json<Vec<Value>>, StatusCode> {
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // P1-1 修复：NS 过滤。admin/通配看全部；否则仅返回可访问 NS 的记忆衰减历史
    // （decay_log 无 namespace 列，需 JOIN memories）
    let is_admin = auth.role == "admin" || auth.allowed_ns.iter().any(|n| n == "*");
    let (sql, params): (String, Vec<String>) = if is_admin {
        (
            "SELECT memory_id, old_tier, new_tier, old_decay, new_decay, reason, logged_at
             FROM decay_log ORDER BY logged_at DESC LIMIT 500"
                .to_string(),
            vec![],
        )
    } else if auth.allowed_ns.is_empty() {
        (
            "SELECT memory_id, old_tier, new_tier, old_decay, new_decay, reason, logged_at
             FROM decay_log WHERE 0=1"
                .to_string(),
            vec![],
        )
    } else {
        let placeholders: Vec<String> = (1..=auth.allowed_ns.len())
            .map(|i| format!("?{}", i))
            .collect();
        (
            format!(
                "SELECT dl.memory_id, dl.old_tier, dl.new_tier, dl.old_decay, dl.new_decay, dl.reason, dl.logged_at
                 FROM decay_log dl JOIN memories m ON dl.memory_id = m.id
                 WHERE m.namespace IN ({}) ORDER BY dl.logged_at DESC LIMIT 500",
                placeholders.join(",")
            ),
            auth.allowed_ns.clone(),
        )
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect();

    let mut results = Vec::new();
    let rows = stmt
        .query_map(param_refs.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3).unwrap_or(0.5),
                r.get::<_, f64>(4).unwrap_or(0.5),
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
    /// 精确按 id 取单条（规避部分部署上 `/api/memories/{id}` 路由未命中）
    id: Option<String>,
}

async fn api_list_memories(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<MemoriesQuery>,
) -> Result<Json<Value>, StatusCode> {
    // 精确 id：走与详情相同的加载逻辑，供图谱/前端兜底
    if let Some(ref id) = q.id {
        let id = id.trim();
        if !id.is_empty() {
            return match load_memory_by_id(&state, &auth, id) {
                Ok(v) => Ok(Json(v)),
                Err(code) => Err(code),
            };
        }
    }
    let ns = q.namespace.as_deref().unwrap_or("default");
    if !auth::check_ns_access(&auth, ns) {
        return Err(StatusCode::FORBIDDEN);
    }
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

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

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt
        .query_map(param_refs.as_slice(), |r| {
            let id: String = r.get(0)?;
            let content: String = r.get(1)?;
            let category: String = r.get(2)?;
            let tier: String = r.get(3)?;
            let importance: i32 = r.get::<_, i32>(4).unwrap_or(3);
            let decay: f64 = r.get::<_, f64>(5).unwrap_or(1.0);
            let recall: i32 = r.get::<_, i32>(6).unwrap_or(0);
            let created: String = r.get::<_, String>(7).unwrap_or_default();
            let ns_val: String = r.get::<_, String>(8).unwrap_or_default();
            Ok((
                id, content, category, tier, importance, decay, recall, created, ns_val,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let memories: Vec<Value> = rows
        .filter_map(|r| r.ok())
        .map(
            |(id, content, category, tier, importance, decay, recall, created, ns_val)| {
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
            },
        )
        .collect();

    Ok(Json(json!({"memories": memories, "total": memories.len()})))
}

fn load_memory_by_id(
    state: &WebApiState,
    auth: &AuthResult,
    id: &str,
) -> Result<Value, StatusCode> {
    let id = id.trim();
    if id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // P1-1：先取所属 NS 再授权（防 IDOR）
    let mem_ns: String = conn
        .query_row("SELECT namespace FROM memories WHERE id = ?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| {
            eprintln!("[api_get_memory] miss id={:?} len={} err={}", id, id.len(), e);
            StatusCode::NOT_FOUND
        })?;
    if !auth::check_ns_access(auth, &mem_ns) {
        return Err(StatusCode::FORBIDDEN);
    }
    conn.query_row(
        "SELECT id, content, category, tier, importance, decay_factor, recall_count, created_at, namespace FROM memories WHERE id = ?1",
        [id],
        |r| {
            let id: String = r.get(0)?;
            let content: String = r.get(1)?;
            let category: String = r.get(2).unwrap_or_default();
            let tier: String = r.get(3).unwrap_or_else(|_| "warm".to_string());
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
        },
    )
    .map_err(|e| {
        eprintln!("[api_get_memory] row map fail id={:?} err={}", id, e);
        StatusCode::NOT_FOUND
    })
}

// ── PUT /api/memories/:id ──
#[derive(Deserialize)]
pub struct UpdateMemoryBody {
    content: Option<String>,
    category: Option<String>,
    tier: Option<String>,
    importance: Option<i32>,
}

#[derive(Deserialize)]
pub struct MemoryIdQuery {
    id: String,
}

async fn api_update_memory(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<MemoryIdQuery>,
    Json(body): Json<UpdateMemoryBody>,
) -> Result<Json<Value>, StatusCode> {
    let id = q.id.trim();
    if id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mem_ns: String = conn
        .query_row("SELECT namespace FROM memories WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !auth::check_ns_access(&auth, &mem_ns) {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(ref content) = body.content {
        conn.execute(
            "UPDATE memories SET content = ?1 WHERE id = ?2",
            rusqlite::params![content, id],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    if let Some(ref category) = body.category {
        conn.execute(
            "UPDATE memories SET category = ?1 WHERE id = ?2",
            rusqlite::params![category, id],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    if let Some(ref tier) = body.tier {
        let allowed = ["hot", "warm", "cold"];
        if !allowed.contains(&tier.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        conn.execute(
            "UPDATE memories SET tier = ?1 WHERE id = ?2",
            rusqlite::params![tier, id],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    if let Some(importance) = body.importance {
        if !(1..=5).contains(&importance) {
            return Err(StatusCode::BAD_REQUEST);
        }
        conn.execute(
            "UPDATE memories SET importance = ?1 WHERE id = ?2",
            rusqlite::params![importance, id],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    Ok(Json(json!({"status": "ok", "id": id})))
}

/// P2-5：删除须带二次确认头，防止仪表盘误触 / CSRF 风格单请求删除。
/// 客户端须发送：`X-Confirm: delete-memory`；id 走 query：`DELETE /api/memories?id=`
async fn api_delete_memory(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
    Query(q): Query<MemoryIdQuery>,
    request: Request,
) -> Result<Json<Value>, StatusCode> {
    let id = q.id.trim();
    if id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let confirm = request
        .headers()
        .get("X-Confirm")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if confirm != "delete-memory" {
        return Err(StatusCode::PRECONDITION_REQUIRED);
    }

    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mem_ns: String = conn
        .query_row("SELECT namespace FROM memories WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !auth::check_ns_access(&auth, &mem_ns) {
        return Err(StatusCode::FORBIDDEN);
    }

    let n = conn
        .execute("DELETE FROM memories WHERE id = ?1", [id])
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if n == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    let _ = conn.execute(
        "DELETE FROM memory_relations WHERE source_id = ?1 OR target_id = ?1",
        [id],
    );

    Ok(Json(json!({"status": "deleted", "id": id})))
}

// ── /api/relations ──
async fn api_list_relations(
    State(state): State<Arc<WebApiState>>,
    Extension(auth): Extension<AuthResult>,
) -> Result<Json<Value>, StatusCode> {
    let conn = state
        .pool
        .get()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // P1-1 修复：NS 过滤。admin/通配看全部；否则仅返回可访问 NS 的关系边
    let is_admin = auth.role == "admin" || auth.allowed_ns.iter().any(|n| n == "*");
    let (sql, params): (String, Vec<String>) = if is_admin {
        (
            "SELECT id, source_id, target_id, relation_type, weight, created_at FROM memory_relations LIMIT 200".to_string(),
            vec![],
        )
    } else if auth.allowed_ns.is_empty() {
        (
            "SELECT id, source_id, target_id, relation_type, weight, created_at FROM memory_relations WHERE 0=1".to_string(),
            vec![],
        )
    } else {
        let placeholders: Vec<String> = (1..=auth.allowed_ns.len())
            .map(|i| format!("?{}", i))
            .collect();
        (
            format!(
                "SELECT id, source_id, target_id, relation_type, weight, created_at
                 FROM memory_relations WHERE namespace IN ({}) LIMIT 200",
                placeholders.join(",")
            ),
            auth.allowed_ns.clone(),
        )
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect();
    let relations: Vec<Value> = stmt
        .query_map(param_refs.as_slice(), |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "source_id": r.get::<_, String>(1)?,
                "target_id": r.get::<_, String>(2)?,
                "relation_type": r.get::<_, String>(3)?,
                "weight": r.get::<_, f64>(4).unwrap_or(0.0),
                "created_at": r.get::<_, String>(5).unwrap_or_default(),
            }))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(
        json!({"relations": relations, "total": relations.len()}),
    ))
}
