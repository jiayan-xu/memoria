//! MCP 协议服务端 — 符合 MCP 规范
//!
//! 独立模块，由 main.rs 调用 build_app() 启动。

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
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
    pub db_path: String,
    pub backup_dir: String,
    pub vec_index_path: String,
}

/// 构建 axum Router
pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/health", get(health_check))
        .route("/health/full", get(health_check_full))
        .with_state(state)
}

/// 健康检查（简化版，向后兼容）
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","service":"memoria","version":"0.2.0"}))
}

/// 健康检查（完整版 — P0: 启动自检）
async fn health_check_full(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // P2-4 修复：/health/full 返回完整健康报告（含 schema 检查），需 admin 校验
    let admin_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !auth::ct_eq(admin_key, &state.admin_key) {
        return Err(StatusCode::FORBIDDEN);
    }
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        memoria_core::health::run_health_check(
            &st.pool,
            &st.auth_pool,
            &st.hnsw,
            &st.db_path,
        )
    }).await.unwrap_or_else(|_| memoria_core::health::HealthReport {
        overall: "fail".to_string(),
        hard_checks: vec![],
        soft_checks: vec![],
        timestamp: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    let overall = report.overall.clone();
    Ok(Json(serde_json::json!({
        "status": overall,
        "service": "memoria",
        "version": "0.2.0",
        "report": report,
    })))
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

    let result = match method {
        "initialize" => {
            // MCP 协议版本协商：回显客户端请求的版本（memoria-core 仅用基础 tools 能力，
            // 任何版本均兼容），缺省回退到广泛支持的 2024-11-05，避免客户端因版本过新拒绝握手。
            let requested = params.get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            rpc_ok(&id, serde_json::json!({
                "protocolVersion": requested,
                "serverInfo": {"name": "memoria", "version": "0.2.0"},
                "capabilities": {"tools": {}},
            }))
        }
        "tools/list" => rpc_ok(&id, serde_json::json!({"tools": tools_list()})),
        "tools/call" => handle_tool_call(&state, &params, &id, agent_id, agent_key).await,
        _ => rpc_error(&id, -32601, &format!("Method not found: {}", method)),
    };
    Json(result)
}

/// MCP 工具模式定义
///
/// 每个工具必须包含合法的 `inputSchema`（JSON Schema 对象），
/// 否则 MCP 客户端（如 WorkBuddy）的 schema 校验会拒绝整个 tools/list 响应。
fn tools_list() -> Vec<serde_json::Value> {
    // 构造工具条目：name + description + 符合 MCP 规范的 inputSchema（必须存在且为 object）
    let tool = |name: &str, description: &str, props: serde_json::Value| -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": props,
            },
        })
    };

    let mut tools = vec![
        tool("memory_search", "搜索记忆", serde_json::json!({
            "query": {"type": "string", "description": "搜索关键词（必填）"},
            "max_results": {"type": "number", "description": "最大返回结果数", "default": 5},
            "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串如 [\"a\",\"b\"]"}
        })),
        tool("memory_search_v2", "多信号融合搜索", serde_json::json!({
            "query": {"type": "string", "description": "搜索关键词（必填）"},
            "max_results": {"type": "number", "description": "最大返回结果数", "default": 5},
            "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串"}
        })),
        tool("memory_remember", "记录一条记忆", serde_json::json!({
            "content": {"type": "string", "description": "记忆内容（必填）"},
            "category": {"type": "string", "description": "类别，默认 fact"},
            "importance": {"type": "number", "description": "重要度 1-5，默认 3"},
            "source": {"type": "string", "description": "来源，默认 mcp"},
            "tags": {"type": "string", "description": "标签 JSON 数组字符串"}
        })),
        tool("memory_observe", "记录观察（低优先级）", serde_json::json!({
            "dialog": {"type": "string", "description": "对话/观察内容"},
            "role": {"type": "string", "description": "角色，默认 user"},
            "source": {"type": "string", "description": "来源，默认 mcp"},
            "session_id": {"type": "string", "description": "会话 ID"}
        })),
        tool("register_agent", "注册Agent（需要Admin key）", serde_json::json!({
            "agent_id": {"type": "string", "description": "新 Agent ID"},
            "display_name": {"type": "string", "description": "显示名"},
            "admin_key": {"type": "string", "description": "Admin Key"},
            "namespace": {"type": "string", "description": "命名空间"}
        })),
        tool("register_user", "注册个人登录账号（本地账密）：user_id + password，命名空间默认 agent/{user_id}（可选 namespace 覆盖）", serde_json::json!({
            "user_id": {"type": "string", "description": "登录用户名（唯一）"},
            "display_name": {"type": "string", "description": "显示名"},
            "password": {"type": "string", "description": "登录口令"},
            "namespace": {"type": "string", "description": "可选：命名空间覆盖（逗号分隔多个）"}
        })),
        tool("login_user", "个人账号登录：校验 user_id + password，成功回传 badge_token 供客户端后续鉴权", serde_json::json!({
            "user_id": {"type": "string", "description": "登录用户名"},
            "password": {"type": "string", "description": "登录口令"}
        })),
        tool("import_install_memories", "迁移记忆命名空间（需 Admin key）：把 from_ns 下记忆整体移到 to_ns（换设备/登录后归并旧记忆）。id=内容哈希与 ns 无关，故仅改 namespace，无需重建索引。", serde_json::json!({
            "from_ns": {"type": "string", "description": "源命名空间"},
            "to_ns": {"type": "string", "description": "目标命名空间"},
            "admin_key": {"type": "string", "description": "Admin Key"}
        })),
        tool("get_allowed_ns", "返回当前调用者自身的命名空间授权列表（供 agent-core 按项目过滤 MCP 工具）", serde_json::json!({})),
        tool("audit_query", "查询审计日志", serde_json::json!({
            "limit": {"type": "number", "description": "返回条数，默认 50"}
        })),
        tool("db_stats", "数据库统计", serde_json::json!({})),
        tool("a2a_send", "向另一个Agent发送消息", serde_json::json!({
            "to": {"type": "string", "description": "目标 Agent ID（必填）"},
            "subject": {"type": "string", "description": "主题"},
            "body": {"type": "string", "description": "正文"}
        })),
        tool("a2a_recv", "接收发给自己的消息", serde_json::json!({
            "limit": {"type": "number", "description": "返回条数，默认 10"}
        })),
        tool("agent_list", "列出已注册的Agent（需要Admin key）", serde_json::json!({
            "admin_key": {"type": "string", "description": "Admin Key"}
        })),
        tool("agent_revoke", "撤销Agent令牌（需要Admin key）", serde_json::json!({
            "agent_id": {"type": "string", "description": "目标 Agent ID（必填）"},
            "admin_key": {"type": "string", "description": "Admin Key"}
        })),
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
        tools.push(tool(name, desc, serde_json::json!({})));
    }
    // Skill Market 工具
    tools.push(tool("skill_market_search", "搜索技能市场中的可用技能", serde_json::json!({
        "query": {"type": "string", "description": "搜索关键词"},
        "category": {"type": "string", "description": "技能分类"},
        "max_results": {"type": "number", "description": "最大结果数，默认 10"}
    })));
    tools.push(tool("skill_market_info", "查看技能详细信息", serde_json::json!({
        "name": {"type": "string", "description": "技能名（必填）"}
    })));
    tools.push(tool("skill_market_publish", "发布技能到市场（需要Admin key或同namespace）", serde_json::json!({
        "name": {"type": "string", "description": "技能名（必填）"},
        "admin_key": {"type": "string", "description": "Admin Key"},
        "visibility": {"type": "string", "description": "可见性 public/tenant/<ns>"},
        "description": {"type": "string", "description": "描述"},
        "version": {"type": "string", "description": "版本，默认 1.0.0"},
        "author": {"type": "string", "description": "作者"},
        "category": {"type": "string", "description": "分类，默认 general"},
        "steps": {"type": "string", "description": "步骤 JSON 数组字符串"},
        "dependencies": {"type": "string", "description": "依赖 JSON 数组字符串"},
        "source": {"type": "string", "description": "来源，默认 manual"}
    })));
    tools.push(tool("skill_market_install", "安装技能到指定Agent（需要Admin key或同namespace管理权）", serde_json::json!({
        "skill_name": {"type": "string", "description": "技能名（必填）"},
        "target_agent": {"type": "string", "description": "目标 Agent ID（必填）"},
        "admin_key": {"type": "string", "description": "Admin Key"}
    })));
    tools.push(tool("skill_market_list_installed", "查询指定Agent已安装的技能列表", serde_json::json!({
        "agent_id": {"type": "string", "description": "Agent ID（必填）"}
    })));
    // Phase 3 工具（decay/graph/prefs）
    tools.push(tool("memory_decay", "运行衰减循环（降低旧记忆权重）", serde_json::json!({})));
    tools.push(tool("memory_graph", "构建记忆关系图", serde_json::json!({
        "batch_size": {"type": "number", "description": "批大小，默认 50"}
    })));
    tools.push(tool("memory_user_prefs", "获取用户偏好设置", serde_json::json!({})));
    tools.push(tool("memory_recent_decisions", "获取最近的决策记录", serde_json::json!({
        "limit": {"type": "number", "description": "返回条数，默认 10"}
    })));
    // P0 工具（backup/health/dedup）
    tools.push(tool("memory_backup", "手动触发数据库备份（GFS 轮转）", serde_json::json!({})));
    tools.push(tool("memory_backup_list", "列出所有备份文件", serde_json::json!({})));
    tools.push(tool("memory_health", "完整健康检查报告", serde_json::json!({})));
    tools.push(tool("memory_dedup_chain", "查询某条记忆的 superseded 链", serde_json::json!({
        "memory_id": {"type": "string", "description": "记忆 ID（必填）"}
    })));
    tools.push(tool("memory_merge", "手动合并两条近义记忆（需 Admin key）", serde_json::json!({
        "keep_id": {"type": "string", "description": "保留的记忆 ID（必填）"},
        "merge_id": {"type": "string", "description": "被合并的记忆 ID（必填）"},
        "admin_key": {"type": "string", "description": "Admin Key"}
    })));
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
    agent_id: &str,
    agent_key: &str,
) -> serde_json::Value {
    let tool = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let empty_args = serde_json::Map::new();
    let safe_args = if args.is_empty() { &empty_args } else { &args };

    // 鉴权：同步 SQLite 查询，必须隔离到阻塞线程池，否则会占住 async worker
    // 导致整服务冻结（initialize/tools/list 全卡）。
    let auth_result = authenticate_async(state, agent_id, agent_key).await;

    let auth = match auth_result {
        Some(a) => a,
        None => {
            spawn_audit(state, agent_id, tool, &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)), false);
            return rpc_error(id, -32001, "Authentication failed. Send X-Agent-Id and X-Agent-Key headers.");
        }
    };

    // 自省工具：仅返回调用者自身命名空间授权，无需命名空间参数，跳过 ns 门控
    if tool == "get_allowed_ns" {
        let text = serde_json::json!({
            "agent_id": auth.agent_id,
            "allowed_ns": auth.allowed_ns,
        }).to_string();
        spawn_audit(state, agent_id, tool, &format!("agent_id={}", auth.agent_id), true);
        return rpc_ok_text(id, &text);
    }

    let ns = safe_args.get("namespace").and_then(|v| v.as_str()).unwrap_or("default");
    if !auth::check_ns_access(&auth, ns) {
        spawn_audit(state, agent_id, tool, &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)), false);
        return rpc_error(id, -32002, &format!("Namespace '{}' not authorized.", ns));
    }

    // Bridge 工具 → 异步转发（网络 I/O，留在 async worker，正确 yield）
    if BRIDGE_TOOLS.contains(&tool) {
        let text = forward_to_bridge(state, tool, safe_args).await;
        let allowed = !text.contains(r#""error""#);
        spawn_audit(state, agent_id, tool, &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)), allowed);
        return rpc_ok_text(id, &text);
    }

    // P0 修复：dispatch 内含同步重计算（FTS5 / HNSW 语义检索等）+ audit_log 同步写，
    // 全部隔离到阻塞线程池，避免占住 async worker 导致整服务冻结。
    let st = state.clone();
    let tool_owned = tool.to_string();
    let args_owned = safe_args.clone();
    let auth_owned = auth.clone();
    let agent_id_owned = agent_id.to_string();
    let text = match tokio::task::spawn_blocking(move || {
        let text = dispatch(&st, &tool_owned, &args_owned, &auth_owned);
        let allowed = !text.contains(r#""error""#);
        // P0 修复（终极）：闭包内若 dispatch 已对 auth_pool 写过（如 register_agent），
        // 再同步调 auth::audit_log 写同一池 → WAL 串行写下第二次写死等第一次写锁释放，
        // 导致 spawn_blocking 的 .await 卡到超时（实测 40s）。改为 fire-and-forget，
        // 审计在独立阻塞线程异步写，与 get_allowed_ns / 鉴权失败分支保持一致，不阻塞响应。
        // 审计参数改用标准 JSON（而非 Rust Debug 格式）：Debug 串如
        // `Object({"api_key": String("...")})` 会让 sanitize_params 走非 JSON 分支，
        // 其 value 提取在 `:` 后第一个空格截断 → 只把空格打码、真实密钥明文落库。
        // 改成 JSON 后走 sanitize_json_value 分支，按值正确打码（secret 卫生）。
        spawn_audit(&st, &agent_id_owned, &tool_owned, &serde_json::to_string(&args_owned).unwrap_or_else(|_| format!("{:?}", args_owned)), allowed);
        text
    }).await {
        Ok(t) => t,
        Err(_) => "{\"error\":\"dispatch task panicked\"}".to_string(),
    };

    rpc_ok_text(id, &text)
}

/// 鉴权（同步 SQLite 查询隔离到阻塞线程池，不占 async worker）
async fn authenticate_async(state: &Arc<AppState>, agent_id: &str, agent_key: &str) -> Option<auth::AuthResult> {
    let st = state.clone();
    let a = agent_id.to_string();
    let k = agent_key.to_string();
    tokio::task::spawn_blocking(move || auth::authenticate(&st.auth_pool, &a, &k).ok())
        .await
        .unwrap_or(None)
}

/// 审计日志写入（同步 SQLite 写隔离到阻塞线程池，fire-and-forget 不阻塞调用方）
fn spawn_audit(state: &Arc<AppState>, agent_id: &str, tool: &str, params: &str, allowed: bool) {
    let st = state.clone();
    let aid = agent_id.to_string();
    let t = tool.to_string();
    let p = params.to_string();
    tokio::task::spawn_blocking(move || {
        auth::audit_log(&st.auth_pool, &aid, &t, &p, allowed);
    });
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
            if let Ok(sem) = search::semantic::semantic_search(query, ns, fts_limit, Some(&state.hnsw), Some(&state.query_cache), Some(&state.pool)) {
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
            // P0: 带近义重复检测的 remember
            match tools::remember::remember_with_dedup(
                &state.pool, content, cat, imp, src, ns, &tags,
                Some(&state.hnsw), Some(&state.query_cache),
            ) {
                Ok(result) => {
                    if result.action == "superseded_near_dup" && !result.superseded_ids.is_empty() {
                        let pairs: Vec<String> = result.superseded_ids.iter().zip(result.similarities.iter())
                            .map(|(id, sim)| format!("{{\"id\":\"{}\",\"similarity\":{}}}", id, (sim * 100.0).round() / 100.0))
                            .collect();
                        format!(
                            r#"{{"status":"remembered","id":"{}","action":"{}","superseded":[{}]}}"#,
                            result.id, result.action, pairs.join(",")
                        )
                    } else {
                        format!(r#"{{"status":"remembered","id":"{}","action":"{}"}}"#, result.id, result.action)
                    }
                },
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
            if !auth::ct_eq(admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"Invalid admin key"}"#.to_string();
            }
            let default_ns = format!("agent/{}", new_id);
            let ns = args.get("namespace").and_then(|v| v.as_str()).unwrap_or(&default_ns);
            match auth::register_agent(&state.auth_pool, new_id, display_name, &[ns], "user") {
                Ok(badge) => serde_json::to_string(&serde_json::json!({"status":"registered","badge":badge})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "register_user" => {
            let user_id = args.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let display_name = args.get("display_name").and_then(|v| v.as_str()).unwrap_or(user_id);
            let password = args.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if user_id.is_empty() || password.is_empty() {
                return r#"{"status":"error","message":"user_id 与 password 必填"}"#.to_string();
            }
            // 口令强度最低要求：长度 >= 6（内部可信 LAN，仅作基本防线）
            if password.len() < 6 {
                return r#"{"status":"error","message":"口令至少 6 位"}"#.to_string();
            }
            let ns_override = args.get("namespace").and_then(|v| v.as_str());
            match auth::register_user(&state.auth_pool, user_id, display_name, password, ns_override) {
                Ok(badge) => serde_json::to_string(&serde_json::json!({
                    "status": "registered",
                    "agent_id": badge.agent_id,
                    "namespace": badge.namespace
                })).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "login_user" => {
            let user_id = args.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let password = args.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if user_id.is_empty() || password.is_empty() {
                return r#"{"status":"error","message":"user_id 与 password 必填"}"#.to_string();
            }
            match auth::login_user(&state.auth_pool, user_id, password) {
                Ok(badge) => serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "agent_id": badge.agent_id,
                    "display_name": badge.display_name,
                    "namespace": badge.namespace,
                    "badge_token": badge.badge_token
                })).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "import_install_memories" => {
            let from_ns = args.get("from_ns").and_then(|v| v.as_str()).unwrap_or("");
            let to_ns = args.get("to_ns").and_then(|v| v.as_str()).unwrap_or("");
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !auth::ct_eq(admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"Invalid admin key"}"#.to_string();
            }
            if from_ns.is_empty() || to_ns.is_empty() || from_ns == to_ns {
                return r#"{"status":"error","message":"from_ns / to_ns 必填且不能相同"}"#.to_string();
            }
            match state.pool.get() {
                Ok(conn) => {
                    // 迁移仅改 namespace 列：memory id = SHA256(content) 与 ns 无关，
                    // 全局唯一 PK；HNSW 按 id 索引、FTS 索引 content，均不含 ns，
                    // 故移动 ns 后向量/全文索引仍指向同一条记忆，无需重建。
                    let n1 = conn.execute(
                        "UPDATE memories SET namespace=?1 WHERE namespace=?2",
                        rusqlite::params![to_ns, from_ns],
                    ).unwrap_or(0);
                    let n2 = conn.execute(
                        "UPDATE memory_relations SET namespace=?1 WHERE namespace=?2",
                        rusqlite::params![to_ns, from_ns],
                    ).unwrap_or(0);
                    // user_prefs（B3 已加 namespace 列）一并迁移；若无该列则忽略。
                    let n3 = conn.execute(
                        "UPDATE user_prefs SET namespace=?1 WHERE namespace=?2",
                        rusqlite::params![to_ns, from_ns],
                    ).unwrap_or(0);
                    format!(
                        r#"{{"status":"ok","memories_moved":{},"relations_moved":{},"prefs_moved":{}}}"#,
                        n1, n2, n3
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"pool: {}"}}"#, e),
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
            // P0-M1 修复：校验目标 Agent 的命名空间属于调用者授权范围，
            // 防止任意已认证 agent 向任意 namespace 注入消息（跨租户/跨项目消息投毒）。
            // 调用者自身（to == 自己）或 admin(*) 始终允许；其余必须落在 allowed_ns 内。
            let target_ns = format!("agent/{}", to);
            let self_msg = to == _auth.agent_id;
            if !self_msg && !auth::check_ns_access(_auth, &target_ns) {
                return format!(r#"{{"status":"error","message":"无权向该 Agent 发送消息（超出命名空间授权范围）"}}"#);
            }
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
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !auth::ct_eq(admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
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
            // 只有 admin 可以吊销 agent
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !auth::ct_eq(admin_key, &state.admin_key) {
                return format!(r#"{{"status":"error","message":"admin key required"}}"#);
            }
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
            if !auth::ct_eq(admin_key, &state.admin_key) {
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
            if !auth::ct_eq(admin_key, &state.admin_key) {
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
        // ── Phase 3 工具 ──
        "memory_decay" => {
            match tools::decay::run_decay(&state.pool, ns) {
                Ok((processed, cold)) => format!(r#"{{"status":"ok","processed":{},"cold":{}}}"#, processed, cold),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_graph" => {
            let batch_size = args.get("batch_size").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
            match tools::graph::build_graph(&state.pool, ns, batch_size) {
                Ok((entity, chrono_cnt)) => format!(r#"{{"status":"ok","same_entity":{},"chronological":{}}}"#, entity, chrono_cnt),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_user_prefs" => {
            match tools::prefs::user_prefs(&state.pool, &ns) {
                Ok(prefs) => {
                    let items: Vec<serde_json::Value> = prefs.into_iter().map(|(k,v,c)| {
                        serde_json::json!({"key": k, "value": v, "confidence": c})
                    }).collect();
                    serde_json::to_string(&serde_json::json!({"status":"ok","prefs":items})).unwrap_or_default()
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_recent_decisions" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            match tools::prefs::recent_decisions(&state.pool, ns, limit) {
                Ok(decisions) => {
                    let items: Vec<serde_json::Value> = decisions.into_iter().map(|(id, content, ts)| {
                        serde_json::json!({"id": id, "content": content, "time": ts})
                    }).collect();
                    serde_json::to_string(&serde_json::json!({"status":"ok","decisions":items})).unwrap_or_default()
                },
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        // ── P0 工具：备份 / 健康 / 去重 ──
        "memory_backup" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            // 手动触发备份
            match memoria_core::backup::perform_backup(
                &state.pool, &state.db_path, &state.backup_dir,
                Some(&state.vec_index_path),
            ) {
                Ok(r) => format!(
                    r#"{{"status":"ok","backup_path":"{}","size_mb":{},"integrity_ok":{},"rotation_deleted":{},"tier":"{}"}}"#,
                    r.backup_path, r.db_size_bytes / 1048576, r.integrity_ok, r.rotation_deleted, r.tier
                ),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_backup_list" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            match memoria_core::backup::list_backups(&state.backup_dir) {
                Ok(v) => serde_json::to_string(&serde_json::json!({"status":"ok","backups":v})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_health" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            let report = memoria_core::health::run_health_check(
                &state.pool, &state.auth_pool, &state.hnsw, &state.db_path,
            );
            serde_json::to_string(&serde_json::json!({"status":"ok","report":report})).unwrap_or_default()
        },
        "memory_dedup_chain" => {
            let memory_id = args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
            if memory_id.is_empty() {
                return r#"{"status":"error","message":"missing memory_id"}"#.to_string();
            }
            // P1-2 修复：校验该记忆所属 NS 对调用者可见（防跨 NS 读取 superseded 链 / IDOR）
            let mem_ns: String = match state.pool.get() {
                Ok(conn) => conn.query_row(
                    "SELECT namespace FROM memories WHERE id = ?1",
                    rusqlite::params![memory_id],
                    |r| r.get::<_, String>(0),
                ).unwrap_or_default(),
                Err(_) => return r#"{"status":"error","message":"db error"}"#.to_string(),
            };
            if !auth::check_ns_access(_auth, &mem_ns) {
                return r#"{"status":"error","message":"namespace not authorized"}"#.to_string();
            }
            match tools::remember::get_supersession_chain(&state.pool, memory_id) {
                Ok(chain) => serde_json::to_string(&serde_json::json!({"status":"ok","superseded":chain})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_merge" => {
            let admin_key_val = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !auth::ct_eq(admin_key_val, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            let keep_id = args.get("keep_id").and_then(|v| v.as_str()).unwrap_or("");
            let merge_id = args.get("merge_id").and_then(|v| v.as_str()).unwrap_or("");
            if keep_id.is_empty() || merge_id.is_empty() {
                return r#"{"status":"error","message":"missing keep_id or merge_id"}"#.to_string();
            }
            match tools::remember::merge_memories(&state.pool, keep_id, merge_id) {
                Ok(()) => format!(r#"{{"status":"merged","keep":"{}","merged":"{}"}}"#, keep_id, merge_id),
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
        Err(_) => return false,
    };
    let tags_str: String = match conn.query_row(
        "SELECT tags FROM memories WHERE id = ?",
        rusqlite::params![memory_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(t) => t,
        Err(_) => return false,  // 无标签记录→不匹配（P2-4 安全加固）
    };
    // tags 存为 JSON 数组 ["a","b"]，检查每个请求标签是否在其中
    tags.iter().all(|tag| tags_str.contains(&format!("\"{}\"", tag)))
}
