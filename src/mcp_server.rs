//! MCP 协议服务端 — 符合 MCP 规范
//!
//! 独立模块，由 main.rs 调用 build_app() 启动。

use axum::{
    extract::State,
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use chrono;

use memoria_core::auth::{self, AuthResult};
use memoria_core::search;
use memoria_core::storage;
use memoria_core::tools;

/// 不认识的 MCP 工具调用转发到 A2A Bridge
const BRIDGE_TOOLS: &[&str] = &[
    "cross_agent_query",
    "system_status",
    "panel_discuss",
    "reasonix_dispatch",
    "continue_task",
    "auto_route",
];

/// 应用状态
pub struct AppState {
    pub pool: storage::SqlitePool,
    pub auth_pool: storage::SqlitePool,
    pub hnsw: Arc<memoria_core::vector::HnswIndex>,
    pub query_cache: Arc<memoria_core::vector::QueryCache>,
    pub admin_key: String,
    pub bridge_url: String,
    pub http_client: reqwest::Client,
}

/// 构建 axum Router
pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/health", get(health_check))
        .with_state(state)
}

/// 健康检查
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","service":"memoria","version":"0.2.0"}))
}

/// MCP 主入口
async fn handle_mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let params = body.get("params").and_then(|p| p.as_object()).cloned().unwrap_or_default();

    let agent_id = headers.get("x-agent-id")
        .and_then(|v| v.to_str().ok()).unwrap_or("anonymous");
    let agent_key = headers.get("x-agent-key")
        .and_then(|v| v.to_str().ok()).unwrap_or("");

    let auth_result = auth::authenticate(&state.auth_pool, agent_id, agent_key).ok();

    let result = match method {
        "initialize" => rpc_ok(&id, serde_json::json!({
            "protocolVersion": "2026-07-28",
            "serverInfo": {"name": "memoria", "version": "0.2.0"},
            "capabilities": {"tools": {}},
        })),
        "tools/list" => rpc_ok(&id, serde_json::json!({"tools": tools_list()})),
        "tools/call" => handle_tool_call(&state, &params, &id, &auth_result, agent_id, agent_key).await,
        _ => rpc_error(&id, -32601, &format!("Method not found: {}", method)),
    };
    Json(result)
}

/// MCP 工具模式定义
fn tools_list() -> Vec<serde_json::Value> {
    let mut tools = vec![
        serde_json::json!({"name": "memory_search", "description": "搜索记忆"}),
        serde_json::json!({"name": "memory_search_v2", "description": "多信号融合搜索"}),
        serde_json::json!({"name": "memory_remember", "description": "记录一条记忆"}),
        serde_json::json!({"name": "memory_observe", "description": "记录观察（低优先级）"}),
        serde_json::json!({"name": "register_agent", "description": "注册Agent（需要Admin key）"}),
        serde_json::json!({"name": "audit_query", "description": "查询审计日志"}),
        serde_json::json!({"name": "db_stats", "description": "数据库统计"}),
        serde_json::json!({"name": "a2a_send", "description": "向另一个Agent发送消息"}),
        serde_json::json!({"name": "a2a_recv", "description": "接收发给自己的消息"}),
        serde_json::json!({"name": "agent_list", "description": "列出已注册的Agent（需要Admin key）"}),
        serde_json::json!({"name": "agent_revoke", "description": "撤销Agent令牌（需要Admin key）"}),
    ];
    // Bridge 转发工具
    for name in BRIDGE_TOOLS {
        let desc = match *name {
            "cross_agent_query" => "向另一个Agent提问",
            "system_status" => "检查各Agent连接状态",
            "panel_discuss" => "多Agent圆桌讨论",
            "reasonix_dispatch" => "派发编码任务给Reasonix",
            "continue_task" => "继续一个等待输入的任务",
            "auto_route" => "动态路由查询到最佳Agent",
            _ => "Bridge 转发工具",
        };
        tools.push(serde_json::json!({"name": name, "description": desc}));
    }
    // Skill Market 工具
    tools.push(serde_json::json!({"name": "skill_market_search", "description": "搜索技能市场中的可用技能"}));
    tools.push(serde_json::json!({"name": "skill_market_info", "description": "查看技能详细信息"}));
    tools.push(serde_json::json!({"name": "skill_market_publish", "description": "发布技能到市场（需要Admin key或同namespace）"}));
    tools.push(serde_json::json!({"name": "skill_market_install", "description": "安装技能到指定Agent（需要Admin key或同namespace管理权）"}));
    tools.push(serde_json::json!({"name": "skill_market_list_installed", "description": "查询指定Agent已安装的技能列表"}));
    tools
}

// ── RPC helpers ──

fn rpc_ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_ok_text(id: &serde_json::Value, text: &str) -> serde_json::Value {
    rpc_ok(id, serde_json::json!({"content": [{"type": "text", "text": text}]}))
}

fn rpc_error(id: &serde_json::Value, code: i32, msg: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

// ── Tool dispatch ──

async fn handle_tool_call(
    state: &Arc<AppState>,
    params: &serde_json::Map<String, serde_json::Value>,
    id: &serde_json::Value,
    auth_result: &Option<AuthResult>,
    agent_id: &str,
    _agent_key: &str,
) -> serde_json::Value {
    let tool = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let empty_args = serde_json::Map::new();
    let safe_args = if args.is_empty() { &empty_args } else { &args };

    let auth = match auth_result {
        Some(a) => a,
        None => {
            auth::audit_log(&state.auth_pool, agent_id, tool, &format!("{:?}", args), false);
            return rpc_error(id, -32001, "Authentication failed. Send X-Agent-Id and X-Agent-Key headers.");
        }
    };

    let ns = safe_args.get("namespace").and_then(|v| v.as_str()).unwrap_or("default");
    if !auth::check_ns_access(auth, ns) {
        auth::audit_log(&state.auth_pool, agent_id, tool, &format!("{:?}", args), false);
        return rpc_error(id, -32002, &format!("Namespace '{}' not authorized.", ns));
    }

    // Bridge 工具 → 异步转发
    if BRIDGE_TOOLS.contains(&tool) {
        let text = forward_to_bridge(state, tool, safe_args).await;
        let allowed = !text.contains(r#""error""#);
        auth::audit_log(&state.auth_pool, agent_id, tool, &format!("{:?}", args), allowed);
        return rpc_ok_text(id, &text);
    }

    let text = dispatch(state, tool, safe_args, auth);
    let allowed = !text.contains(r#""error""#);
    auth::audit_log(&state.auth_pool, agent_id, tool, &format!("{:?}", args), allowed);

    rpc_ok_text(id, &text)
}

fn dispatch(
    state: &Arc<AppState>,
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    _auth: &AuthResult,
) -> String {
    let ns = args.get("namespace").and_then(|v| v.as_str()).unwrap_or("default");

    match tool {
        "memory_search" | "memory_search_v2" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
            let _tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or("L2");
            let tags_filter: Option<Vec<String>> = args.get("tags").and_then(|v| {
                // tags 可以是 JSON 数组 ["a","b"] 或 JSON 字符串 "[\"a\",\"b\"]"
                if let Some(arr) = v.as_array() {
                    Some(arr.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                } else if let Some(s) = v.as_str() {
                    // 尝试作为 JSON 数组字符串解析
                    Some(serde_json::from_str::<Vec<String>>(s).unwrap_or_default())
                } else {
                    None
                }
            });

            let fts_limit = max_results * 3;
            let mut signals: Vec<Vec<search::SignalResult>> = Vec::new();
            let mut weights: Vec<f64> = Vec::new();

            // S1: Keyword
            if let Ok(kw) = search::keyword::keyword_search(&state.pool, query, ns, fts_limit) {
                if !kw.is_empty() { signals.push(kw); weights.push(1.0); }
            }
            // S2: Semantic (HNSW)
            if let Ok(sem) = search::semantic::semantic_search(query, ns, fts_limit, Some(&state.hnsw), Some(&state.query_cache)) {
                if !sem.is_empty() { signals.push(sem); weights.push(1.0); }
            }
            // S3: Temporal
            if let Ok(temp) = search::temporal::temporal_search(&state.pool, ns, fts_limit) {
                if !temp.is_empty() { signals.push(temp); weights.push(1.0); }
            }
            // S4: Importance
            if let Ok(imp) = search::importance::importance_search(&state.pool, ns, fts_limit) {
                if !imp.is_empty() { signals.push(imp); weights.push(1.0); }
            }
            // S5: Category
            if let Ok(cat) = search::importance::category_search(&state.pool, query, ns, max_results as u32) {
                if !cat.is_empty() { signals.push(cat); weights.push(0.5); }
            }

            let mut fused = if signals.is_empty() { vec![] } else { search::rrf::rrf_merge(&signals, &weights, 60.0) };
            if let Ok(expanded) = search::rrf::graph_expand(&state.pool, &fused, 2, ns) { fused.extend(expanded); }

            // Tags 过滤（如果有）
            let filtered: Vec<search::rrf::FusedResult> = if let Some(ref tags) = tags_filter {
                if tags.is_empty() {
                    fused
                } else {
                    fused.into_iter().filter(|r| {
                        matches_memory_tags(&state.pool, &r.memory_id, tags)
                    }).collect()
                }
            } else {
                fused
            };

            let results: Vec<serde_json::Value> = filtered.iter().take(max_results as usize).map(|r| {
                serde_json::json!({"memory_id": r.memory_id, "content": truncate(&r.content, 2000), "rrf_score": r.rrf_score, "source": r.source})
            }).collect();
            serde_json::to_string(&serde_json::json!({"status":"ok","total_results":filtered.len(),"results":results})).unwrap_or_default()
        },
        "memory_remember" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let cat = args.get("category").and_then(|v| v.as_str()).unwrap_or("fact");
            let imp = args.get("importance").and_then(|v| v.as_i64()).unwrap_or(3);
            let src = args.get("source").and_then(|v| v.as_str()).unwrap_or("mcp");
            // tags: 支持 JSON 数组 ["a","b"] 或 JSON 字符串 "[\"a\",\"b\"]"
            let tags = if let Some(arr) = args.get("tags").and_then(|v| v.as_array()) {
                serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string())
            } else if let Some(s) = args.get("tags").and_then(|v| v.as_str()) {
                // 已经是 JSON 字符串，确保格式正确
                if s.is_empty() || s == "[]" { "[]".to_string() } else { s.to_string() }
            } else {
                "[]".to_string()
            };
            match tools::remember::remember(&state.pool, content, cat, imp, src, ns, &tags) {
                Ok(id) => format!(r#"{{"status":"remembered","id":"{}"}}"#, id),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_observe" => {
            let dialog = args.get("dialog").and_then(|v| v.as_str()).unwrap_or("");
            let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let src = args.get("source").and_then(|v| v.as_str()).unwrap_or("mcp");
            let sid = args.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
            match tools::observe::observe(&state.pool, dialog, role, src, sid, ns) {
                Ok(id) => format!(r#"{{"status":"observed","id":"{}"}}"#, id),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "register_agent" => {
            let new_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            let display_name = args.get("display_name").and_then(|v| v.as_str()).unwrap_or(new_id);
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if admin_key != state.admin_key {
                return r#"{"status":"error","message":"Invalid admin key"}"#.to_string();
            }
            let default_ns = format!("agent/{}", new_id);
            let ns = args.get("namespace").and_then(|v| v.as_str()).unwrap_or(&default_ns);
            match auth::register_agent(&state.auth_pool, new_id, display_name, &[ns], "user") {
                Ok(badge) => serde_json::to_string(&serde_json::json!({"status":"registered","badge":badge})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "audit_query" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
            match state.auth_pool.get() {
                Ok(conn) => {
                    // 收集所有 audit_log_2026Wxx 表，UNION ALL 查询
                    let tables: Vec<String> = conn
                        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%' ORDER BY name DESC")
                        .and_then(|mut stmt| stmt.query_map([], |row| row.get::<_, String>(0))
                            .map(|rows| rows.filter_map(|r| r.ok()).collect()))
                        .unwrap_or_default();
                    if tables.is_empty() {
                        return serde_json::to_string(&serde_json::json!({"status":"ok","logs":[]})).unwrap_or_default();
                    }
                    let union_sql: String = tables.iter()
                        .map(|t| format!("SELECT agent_id, tool, params, allowed, timestamp FROM {}", t))
                        .collect::<Vec<_>>()
                        .join(" UNION ALL ");
                    let full_sql = format!("SELECT * FROM ({}) AS all_logs ORDER BY timestamp DESC LIMIT ?", union_sql);
                    let mut stmt = match conn.prepare(&full_sql) {
                        Ok(s) => s, Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
                    };
                    let rows = stmt.query_map(rusqlite::params![limit], |row| {
                        Ok(serde_json::json!({"agent_id":row.get::<_,String>(0)?,"tool":row.get::<_,String>(1)?,"params":row.get::<_,String>(2)?,"allowed":row.get::<_,i32>(3)?,"timestamp":row.get::<_,String>(4)?}))
                    });
                    let items: Vec<serde_json::Value> = match rows {
                        Ok(r) => r.flatten().collect(),
                        Err(_) => vec![],
                    };
                    serde_json::to_string(&serde_json::json!({"status":"ok","logs":items})).unwrap_or_default()
                },
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
        },
        "db_stats" => {
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            let auth_conn = match state.auth_pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            let tables = ["memories","messages","sessions","decisions","user_prefs",
                          "memory_relations","decay_log","dream_state"];
            let auth_tables = ["agent_registry"];
            let mut m = serde_json::Map::new();
            for t in &tables {
                let c: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", t),[],|r|r.get(0)).unwrap_or(0);
                m.insert(t.to_string(), serde_json::Value::Number(c.into()));
            }
            for t in &auth_tables {
                let c: i64 = auth_conn.query_row(&format!("SELECT COUNT(*) FROM {}", t),[],|r|r.get(0)).unwrap_or(0);
                m.insert(t.to_string(), serde_json::Value::Number(c.into()));
            }
            // 审计总行数（跨分区）
            let audit_count: i64 = auth_conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%'")
                .and_then(|mut stmt| {
                    let tables: Vec<String> = stmt.query_map([], |row| row.get::<_, String>(0))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default();
                    let mut total: i64 = 0;
                    for t in tables {
                        if let Ok(c) = auth_conn.query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get::<_, i64>(0)) {
                            total += c;
                        }
                    }
                    Ok(total)
                })
                .unwrap_or(0);
            m.insert("audit_log_total".to_string(), serde_json::Value::Number(audit_count.into()));
            m.insert("hnsw_vectors".to_string(), serde_json::Value::Number((state.hnsw.len() as i64).into()));
            serde_json::to_string(&serde_json::json!({"status":"ok","stats":m})).unwrap_or_default()
        },
        "a2a_send" => {
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
            let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            if to.is_empty() { return format!(r#"{{"status":"error","message":"missing 'to'"}}"#); }
            match state.pool.get() {
                Ok(conn) => {
                    let _ = conn.execute(
                        "INSERT INTO memories (id, namespace, source, content, category, confidence, created_at, tier, importance)
                         VALUES (?, ?, ?, ?, 'a2a_message', 1.0, datetime('now'), 'hot', 2)",
                        rusqlite::params![format!("a2a_{}", uuid::Uuid::new_v4()), format!("agent/{}", to),
                                          format!("agent:{}", _auth.agent_id),
                                          format!("[{}] {}", subject, body)],
                    );
                    format!(r#"{{"status":"sent","to":"{}"}}"#, to)
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "a2a_recv" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            match state.pool.get() {
                Ok(conn) => {
                    let result = (|| -> Result<String, String> {
                        let mut stmt = conn.prepare(
                            "SELECT id, source, content, created_at FROM memories
                             WHERE namespace = ? AND category = 'a2a_message'
                             ORDER BY created_at DESC LIMIT ?"
                        ).map_err(|e| format!("prepare: {}", e))?;
                        let rows: Vec<serde_json::Value> = stmt.query_map(
                            rusqlite::params![format!("agent/{}", _auth.agent_id), limit],
                            |row| Ok(serde_json::json!({"id":row.get::<_,String>(0)?,"from":row.get::<_,String>(1)?,"content":row.get::<_,String>(2)?,"time":row.get::<_,String>(3)?}))
                        ).map_err(|e| format!("query: {}", e))?.flatten().collect();
                        Ok(serde_json::to_string(&serde_json::json!({"status":"ok","messages":rows})).unwrap_or_default())
                    })();
                    match result {
                        Ok(s) => s,
                        Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
                    }
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "agent_list" => {
            match state.auth_pool.get() {
                Ok(conn) => {
                    let result = (|| -> Result<String, String> {
                        let mut stmt = conn.prepare(
                            "SELECT agent_id, display_name, namespace, permission, created_at FROM agent_registry ORDER BY created_at"
                        ).map_err(|e| format!("prepare: {}", e))?;
                        let rows: Vec<serde_json::Value> = stmt.query_map([], |row| Ok(serde_json::json!({
                            "agent_id":row.get::<_,String>(0)?,"display_name":row.get::<_,String>(1)?,
                            "namespace":row.get::<_,String>(2)?,"permission":row.get::<_,String>(3)?,
                            "registered_at":row.get::<_,String>(4)?
                        }))).map_err(|e| format!("query: {}", e))?.flatten().collect();
                        Ok(serde_json::to_string(&serde_json::json!({"status":"ok","agents":rows})).unwrap_or_default())
                    })();
                    match result {
                        Ok(s) => s,
                        Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
                    }
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "agent_revoke" => {
            let target = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() { return format!(r#"{{"status":"error","message":"missing agent_id"}}"#); }
            match state.auth_pool.get() {
                Ok(conn) => {
                    let n = conn.execute("DELETE FROM agent_registry WHERE agent_id = ?", rusqlite::params![target])
                        .unwrap_or(0);
                    format!(r#"{{"status":"revoked","agent_id":"{}","deleted":{}}}"#, target, n)
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "skill_market_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let category = args.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            let caller_ns = _auth.allowed_ns.first().map(|s| s.as_str()).unwrap_or("default");
            match state.auth_pool.get() {
                Ok(conn) => {
                    let like = format!("%{}%", query);
                    let mut rows: Vec<serde_json::Value> = Vec::new();
                    let sql = if category.is_empty() {
                        "SELECT name, description, version, author, category, confidence, install_count, visibility
                         FROM skill_catalog WHERE is_active = 1 AND (visibility = 'public' OR visibility = ?)
                         AND (name LIKE ? OR description LIKE ?) ORDER BY install_count DESC, confidence DESC LIMIT ?"
                    } else {
                        "SELECT name, description, version, author, category, confidence, install_count, visibility
                         FROM skill_catalog WHERE is_active = 1 AND (visibility = 'public' OR visibility = ?)
                         AND category = ? AND (name LIKE ? OR description LIKE ?) ORDER BY install_count DESC, confidence DESC LIMIT ?"
                    };
                    if let Ok(mut stmt) = conn.prepare(sql) {
                        if category.is_empty() {
                            if let Ok(iter) = stmt.query_map(rusqlite::params![caller_ns, like, like, max_results], |row| {
                                Ok(serde_json::json!({"name": row.get::<_,String>(0)?, "description": row.get::<_,String>(1)?,
                                    "version": row.get::<_,String>(2)?, "author": row.get::<_,String>(3)?,
                                    "category": row.get::<_,String>(4)?, "confidence": row.get::<_,f64>(5)?,
                                    "install_count": row.get::<_,i64>(6)?, "visibility": row.get::<_,String>(7)?}))
                            }) {
                                for r in iter.flatten() { rows.push(r); }
                            }
                        } else {
                            if let Ok(iter) = stmt.query_map(rusqlite::params![caller_ns, category, like, like, max_results], |row| {
                                Ok(serde_json::json!({"name": row.get::<_,String>(0)?, "description": row.get::<_,String>(1)?,
                                    "version": row.get::<_,String>(2)?, "author": row.get::<_,String>(3)?,
                                    "category": row.get::<_,String>(4)?, "confidence": row.get::<_,f64>(5)?,
                                    "install_count": row.get::<_,i64>(6)?, "visibility": row.get::<_,String>(7)?}))
                            }) {
                                for r in iter.flatten() { rows.push(r); }
                            }
                        }
                    }
                    serde_json::to_string(&serde_json::json!({"status":"ok","results":rows})).unwrap_or_default()
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "skill_market_info" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() { return r#"{"status":"error","message":"missing name"}"#.to_string(); }
            match state.auth_pool.get() {
                Ok(conn) => {
                    let row = conn.query_row(
                        "SELECT name, description, version, author, category, visibility,
                                steps, dependencies, confidence, install_count, checksum, source, published_at
                         FROM skill_catalog WHERE name = ? AND is_active = 1",
                        rusqlite::params![name],
                        |row| Ok(serde_json::json!({"name": row.get::<_,String>(0)?,
                            "description": row.get::<_,String>(1)?, "version": row.get::<_,String>(2)?,
                            "author": row.get::<_,String>(3)?, "category": row.get::<_,String>(4)?,
                            "visibility": row.get::<_,String>(5)?, "steps": row.get::<_,String>(6)?,
                            "dependencies": row.get::<_,String>(7)?, "confidence": row.get::<_,f64>(8)?,
                            "install_count": row.get::<_,i64>(9)?, "checksum": row.get::<_,String>(10)?,
                            "source": row.get::<_,String>(11)?, "published_at": row.get::<_,String>(12)?}))
                    );
                    match row {
                        Ok(skill) => serde_json::to_string(&serde_json::json!({"status":"ok","skill":skill})).unwrap_or_default(),
                        Err(_) => format!(r#"{{"status":"error","message":"skill '{}' not found"}}"#, name),
                    }
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "skill_market_publish" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() { return r#"{"status":"error","message":"missing name"}"#.to_string(); }
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            let visibility = args.get("visibility").and_then(|v| v.as_str()).unwrap_or("public");
            let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let version = args.get("version").and_then(|v| v.as_str()).unwrap_or("1.0.0");
            let author = args.get("author").and_then(|v| v.as_str()).unwrap_or("");
            let category = args.get("category").and_then(|v| v.as_str()).unwrap_or("general");
            let steps = args.get("steps").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
            let dependencies = args.get("dependencies").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
            let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("manual");

            // 权限检查：admin_key 或同 namespace
            if admin_key != state.admin_key {
                // 非 admin：检查 namespace 是否匹配 visibility
                if visibility.starts_with("tenant/") {
                    let vis_ns = &visibility[7..]; // "tenant/finance" → "finance"
                    let caller_ns = _auth.allowed_ns.first().map(|s| s.as_str()).unwrap_or("");
                    if !caller_ns.contains(vis_ns) {
                        return r#"{"status":"error","message":"no permission to publish to this visibility"}"#.to_string();
                    }
                } else if visibility != "public" {
                    return r#"{"status":"error","message":"invalid visibility"}"#.to_string();
                }
            }

            match state.auth_pool.get() {
                Ok(conn) => {
                    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                    conn.execute(
                        "INSERT INTO skill_catalog (name, description, version, author, category, visibility,
                         steps, dependencies, confidence, checksum, source, published_at, updated_at, published_by)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0.5, '', ?, ?, ?, ?)
                         ON CONFLICT(name) DO UPDATE SET
                         description=excluded.description, version=excluded.version,
                         steps=excluded.steps, dependencies=excluded.dependencies,
                         updated_at=excluded.updated_at, is_active=1",
                        rusqlite::params![name, description, version, author, category, visibility,
                                          steps, dependencies, source, now, now, _auth.agent_id],
                    ).ok();
                    format!(r#"{{"status":"published","name":"{}","visibility":"{}"}}"#, name, visibility)
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "skill_market_install" => {
            let skill_name = args.get("skill_name").and_then(|v| v.as_str()).unwrap_or("");
            let target_agent = args.get("target_agent").and_then(|v| v.as_str()).unwrap_or("");
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if skill_name.is_empty() || target_agent.is_empty() {
                return r#"{"status":"error","message":"missing skill_name or target_agent"}"#.to_string();
            }

            // 权限检查：admin_key 或同 namespace
            if admin_key != state.admin_key {
                // 非 admin：只能给同 namespace 的 Agent 安装
                let caller_ns = _auth.allowed_ns.first().map(|s| s.as_str()).unwrap_or("");
                let target_ns = format!("agent/{}", target_agent);
                if !caller_ns.contains(&target_ns) && !caller_ns.contains("admin") {
                    return r#"{"status":"error","message":"no permission to install on this agent"}"#.to_string();
                }
            }

            match state.auth_pool.get() {
                Ok(conn) => {
                    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                    conn.execute(
                        "INSERT OR IGNORE INTO agent_skill_whitelist (agent_id, skill_name, installed_at, installed_by)
                         VALUES (?, ?, ?, ?)",
                        rusqlite::params![target_agent, skill_name, now, _auth.agent_id],
                    ).ok();
                    // 增加 install_count
                    conn.execute("UPDATE skill_catalog SET install_count = install_count + 1 WHERE name = ?",
                        rusqlite::params![skill_name]).ok();
                    format!(r#"{{"status":"installed","skill":"{}","target_agent":"{}"}}"#, skill_name, target_agent)
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "skill_market_list_installed" => {
            let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            if agent_id.is_empty() { return r#"{"status":"error","message":"missing agent_id"}"#.to_string(); }
            match state.auth_pool.get() {
                Ok(conn) => {
                    let mut rows: Vec<serde_json::Value> = Vec::new();
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT skill_name, installed_at, installed_by FROM agent_skill_whitelist
                         WHERE agent_id = ? AND is_active = 1 ORDER BY installed_at DESC"
                    ) {
                        if let Ok(iter) = stmt.query_map(rusqlite::params![agent_id], |row| {
                            Ok(serde_json::json!({"skill_name": row.get::<_,String>(0)?,
                                "installed_at": row.get::<_,String>(1)?,
                                "installed_by": row.get::<_,String>(2)?}))
                        }) {
                            for r in iter.flatten() { rows.push(r); }
                        }
                    }
                    serde_json::to_string(&serde_json::json!({"status":"ok","skills":rows})).unwrap_or_default()
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        _ => format!(r#"{{"error":"Unknown tool: {}"}}"#, tool),
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len { return s.to_string(); }
    let mut end = max_len;
    while !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

/// 将不识别的工具调用转发到 A2A Bridge (:9000)
async fn forward_to_bridge(
    state: &Arc<AppState>,
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": args,
        }
    });

    match state.http_client
        .post(&state.bridge_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    {
        Ok(resp) => {
            match resp.json::<serde_json::Value>().await {
                Ok(val) => {
                    val.get("result")
                        .and_then(|r| serde_json::to_string(r).ok())
                        .unwrap_or_else(|| r#"{"error":"empty bridge response"}"#.to_string())
                }
                Err(e) => format!(r#"{{"error":"bridge parse: {}"}}"#, e),
            }
        }
        Err(e) => format!(r#"{{"error":"bridge unreachable ({}): {}"}}"#, state.bridge_url, e),
    }
}

/// 检查 memory_id 的记录是否包含所有指定标签
fn matches_memory_tags(pool: &storage::SqlitePool, memory_id: &str, tags: &[String]) -> bool {
    if tags.is_empty() {
        return true;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return true,
    };
    let tags_str: String = match conn.query_row(
        "SELECT tags FROM memories WHERE id = ?",
        rusqlite::params![memory_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(t) => t,
        Err(_) => return true,  // 无标签记录不拦截
    };
    // tags 存为 JSON 数组 ["a","b"]，检查每个请求标签是否在其中
    tags.iter().all(|tag| tags_str.contains(&format!("\"{}\"", tag)))
}
