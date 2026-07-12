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
    pub hnsw_status: String,
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
    // P0-2 统一 require_admin：admin 角色（X-Agent-Id/Key）或 x-admin-key 兜底，与 P0-1 一致
    let agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok()).unwrap_or("");
    let agent_key = headers.get("x-agent-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    let legacy_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    let auth_result = auth::authenticate(&state.auth_pool, agent_id, agent_key)
        .unwrap_or_else(|_| auth::AuthResult { agent_id: String::new(), allowed_ns: Vec::new(), role: String::new() });
    if !crate::permissions::require_admin(&auth_result, legacy_key, &state.admin_key) {
        return Err(StatusCode::FORBIDDEN);
    }
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        memoria_core::health::run_health_check(
            &st.pool,
            &st.auth_pool,
            &st.hnsw,
            &st.db_path,
            &st.hnsw_status,
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
pub fn tools_list() -> Vec<serde_json::Value> {
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
            "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串如 [\"a\",\"b\"]"},
            "as_of": {"type": "string", "description": "P1-5 时序真值：仅返回该 ISO-8601 时刻有效的记忆；不传则默认 now，自动过滤已失效"}
        })),
        tool("memory_search_v2", "多信号融合搜索", serde_json::json!({
            "query": {"type": "string", "description": "搜索关键词（必填）"},
            "max_results": {"type": "number", "description": "最大返回结果数", "default": 5},
            "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串"},
            "as_of": {"type": "string", "description": "P1-5 时序真值：仅返回该 ISO-8601 时刻有效的记忆；不传则默认 now，自动过滤已失效"}
        })),
        tool("memory_remember", "记录一条记忆", serde_json::json!({
            "content": {"type": "string", "description": "记忆内容（必填）"},
            "category": {"type": "string", "description": "类别，默认 fact"},
            "importance": {"type": "number", "description": "重要度 1-5，默认 3"},
            "source": {"type": "string", "description": "来源，默认 mcp"},
            "tags": {"type": "string", "description": "标签 JSON 数组字符串"},
            "valid_from": {"type": "string", "description": "P1-5 时序真值：记忆生效起点 ISO-8601（默认插入时刻）"},
            "valid_to": {"type": "string", "description": "P1-5 时序真值：记忆失效点 ISO-8601（默认 NULL，长期有效）"}
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
    tools.push(tool("memory_user_prefs", "获取用户偏好设置（按 ns 聚合，hard_rule 优先；写入走 memory_remember，category=preference，tags∈pref|hard_rule|style）", serde_json::json!({
        "tag": {"type": "string", "description": "可选：仅返回指定类型偏好 hard_rule|pref|style"}
    })));
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
    // ── 暗知识层 A1：夜间巩固哑工具（认知在 agent-core，此处只做纯 SQL）──
    tools.push(tool("memory_fetch_unconsolidated", "取本命名空间中 created_at > since 的未巩固观察记录（供 agent-core 夜间巩固提炼；纯读取）", serde_json::json!({
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "since": {"type": "string", "description": "游标：只取 created_at 大于此 ISO8601 时间的记录，默认 1970-01-01"},
        "limit": {"type": "number", "description": "最多返回条数，默认 200"}
    })));
    tools.push(tool("dream_state_get", "读取某命名空间某阶段的巩固进度（last_run / cursor_ts）", serde_json::json!({
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "phase": {"type": "string", "description": "阶段：consolidate/entity_extract/decay/graph，默认 consolidate"}
    })));
    tools.push(tool("dream_state_update", "推进某命名空间某阶段的巩固进度游标（P1-4 限流 + cursor 校验；幂等，只写进度表）", serde_json::json!({
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "phase": {"type": "string", "description": "阶段，默认 consolidate"},
        "cursor_ts": {"type": "string", "description": "本批处理到的最大 created_at（推进游标），必填且须比现有游标新"},
        "items_out": {"type": "number", "description": "本批产出条数，默认 0"},
        "sessions_processed": {"type": "number", "description": "本批处理会话数，默认 0"}
    })));
    // ── 知识图谱 B1：实体 MCP 工具 ──
    tools.push(tool("entity_upsert", "创建或更新一个实体（幂等：同名同类型的实体仅更新别名/摘要）", serde_json::json!({
        "namespace": {"type": "string", "description": "命名空间"},
        "entity_id": {"type": "string", "description": "实体唯一 ID（建议 UUID4，留空自动生成）"},
        "entity_type": {"type": "string", "description": "类型：person/system/tool/concept/org/project/location/event/other"},
        "name": {"type": "string", "description": "实体名称（必填）"},
        "aliases": {"type": "string", "description": "别名 JSON 数组字符串，如 [\"别名1\",\"别名2\"]"},
        "summary": {"type": "string", "description": "实体摘要描述"}
    })));
    tools.push(tool("entity_add_mention", "记录实体在某条记忆中的提及", serde_json::json!({
        "entity_id": {"type": "string", "description": "实体 ID"},
        "memory_id": {"type": "string", "description": "记忆 ID"},
        "context": {"type": "string", "description": "上下文片段（实体在记忆中出现的周围的文字）"},
        "namespace": {"type": "string", "description": "命名空间，默认 default"}
    })));
    tools.push(tool("entity_add_edge", "创建实体间关系边", serde_json::json!({
        "source_entity_id": {"type": "string", "description": "源实体 ID"},
        "target_entity_id": {"type": "string", "description": "目标实体 ID"},
        "relation_type": {"type": "string", "description": "关系类型，如 uses/depends_on/mentions/part_of/similar_to"},
        "weight": {"type": "number", "description": "关系权重 0-1，默认 1.0"},
        "evidence": {"type": "string", "description": "证据来源描述"},
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "valid_from": {"type": "string", "description": "P1-5 时序真值：关系生效起点 ISO-8601（默认插入时刻）"},
        "valid_to": {"type": "string", "description": "P1-5 时序真值：关系失效点 ISO-8601（默认 NULL，长期有效）"}
    })));
    tools.push(tool("entity_search", "搜索实体（按类型/名称/关键词）", serde_json::json!({
        "query": {"type": "string", "description": "搜索关键词（搜索 name/aliases/summary）"},
        "entity_type": {"type": "string", "description": "按类型过滤，可选"},
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "max_results": {"type": "number", "description": "最大返回数，默认 20"}
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

/// P1-4：Dream 阶段 ns 限流（per-(phase, namespace) cooldown，秒）。
/// 各阶段可通过环境变量 `MEMORIA_DREAM_COOLDOWN_<PHASE>` 覆盖，默认：
///   consolidate / entity_extract / graph = 300s，decay = 60s。
fn dream_cooldown(phase: &str) -> u64 {
    let key = format!("MEMORIA_DREAM_COOLDOWN_{}", phase.to_uppercase());
    if let Ok(v) = std::env::var(&key) {
        if let Ok(secs) = v.parse::<u64>() { return secs; }
    }
    // 兜底（未设环境变量时）
    if let Ok(v) = std::env::var("MEMORIA_DREAM_COOLDOWN_DEFAULT") {
        if let Ok(secs) = v.parse::<u64>() { return secs; }
    }
    match phase {
        "decay" => 60,
        _ => 300,
    }
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

            // P1-1: 统一检索入口（与评测 harness 同一路径），替代内联 5 信号构建。
            // 生产路径传入 hnsw/query_cache（运行时语义通道可用）；CI 评测无 embedding 后端时传 None。
            // P1-5: as_of 时序真值（默认 now → 自动过滤已失效记忆）。
            let now_str = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let as_of: Option<&str> = args.get("as_of").and_then(|v| v.as_str())
                .or(Some(&now_str));
            let fused = search::hybrid::hybrid_search(
                &state.pool, query, ns, max_results,
                Some(&state.hnsw), Some(&state.query_cache), as_of,
            ).unwrap_or_default();

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
            // P1-5: 可选时序真值区间
            let valid_from = args.get("valid_from").and_then(|v| v.as_str());
            let valid_to = args.get("valid_to").and_then(|v| v.as_str());
            // P0: 带近义重复检测的 remember
            match tools::remember::remember_with_dedup(
                &state.pool, content, cat, imp, src, ns, &tags,
                Some(&state.hnsw), Some(&state.query_cache), valid_from, valid_to,
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
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
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
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
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
            // P0-1 修复：全库统计含 agent_registry / 审计总数 / HNSW 向量数，需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permissions::require_admin(&_auth, ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
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
            // P1-6：to 格式校验 — 仅允许字母数字连字符（agent-id 格式），
            // 拒绝含 ".." ".." "/" 等路径遍历/注入字符。
            if !to.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || to.len() > 64 {
                return r#"{"status":"error","message":"invalid 'to' format: agent-id only, max 64 chars"}"#.to_string();
            }
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
                    // P1-6：a2a_send 审计（记录目标 agent + subject 摘要）
                    auth::audit_log(&state.auth_pool, &_auth.agent_id, "a2a_send",
                        &format!("to={},subject={:.40}", to, subject), true);
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
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
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
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return format!(r#"{{"status":"error","message":"admin required"}}"#);
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

            // 权限检查：admin_key 或 require_admin（精确角色，非子串）
            if !auth::ct_eq(admin_key, &state.admin_key) && _auth.role != "admin" {
                if visibility.starts_with("tenant/") {
                    let vis_ns = &format!("agent/{}", &visibility[7..]); // "tenant/finance" → "agent/finance"
                    if !auth::check_ns_access(_auth, vis_ns) {
                        return r#"{"status":"error","message":"no permission to publish to this visibility"}"#.to_string();
                    }
                } else if visibility != "public" {
                    return r#"{"status":"error","message":"invalid visibility"}"#.to_string();
                }
            }

            let publish_result = match state.auth_pool.get() {
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
            };
            // P1-6：publish 审计（记录 name + visibility）
            if publish_result.contains("\"status\":\"published\"") {
                auth::audit_log(&state.auth_pool, &_auth.agent_id, "skill_market_publish",
                    &format!("name={},visibility={}", name, visibility), true);
            }
            return publish_result;
        },
        "skill_market_install" => {
            let skill_name = args.get("skill_name").and_then(|v| v.as_str()).unwrap_or("");
            let target_agent = args.get("target_agent").and_then(|v| v.as_str()).unwrap_or("");
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if skill_name.is_empty() || target_agent.is_empty() {
                return r#"{"status":"error","message":"missing skill_name or target_agent"}"#.to_string();
            }

            // 权限检查：admin_key 或 同 namespace
            if !auth::ct_eq(admin_key, &state.admin_key) {
                // 非 admin：只能给同 namespace 的 Agent 安装（使用精确角色判定，非子串）
                if _auth.role != "admin" && !auth::check_ns_access(_auth, &format!("agent/{}", target_agent)) {
                    return r#"{"status":"error","message":"no permission to install on this agent"}"#.to_string();
                }
            }

            let install_result = match state.auth_pool.get() {
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
            };
            // P1-6：install 审计（记录 skill_name + target_agent）
            if install_result.contains("\"status\":\"installed\"") {
                auth::audit_log(&state.auth_pool, &_auth.agent_id, "skill_market_install",
                    &format!("skill={},target={}", skill_name, target_agent), true);
            }
            return install_result;
        },
        "skill_market_list_installed" => {
            let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            if agent_id.is_empty() { return r#"{"status":"error","message":"missing agent_id"}"#.to_string(); }
            // P0-1 修复：NS 隔离 —— 仅自身（同 agent）或授权命名空间内的 agent 可查，
            // 防任意已认证 agent 查他人已装技能清单（信息泄露）。
            if _auth.role != "admin" && agent_id != _auth.agent_id.as_str() {
                let target_ns = format!("agent/{}", agent_id);
                if !auth::check_ns_access(_auth, &target_ns) {
                    return r#"{"status":"error","message":"no permission to view this agent's installed skills"}"#.to_string();
                }
            }
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
            match tools::graph::build_graph(&state.pool, ns, 50) {
                Ok((nodes, edges)) => format!(r#"{{"status":"ok","nodes":{},"edges":{}}}"#, nodes, edges),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "memory_user_prefs" => {
            // 可选 tag 过滤（hard_rule|pref|style），默认返回全部偏好
            let tag_filter = args.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
            match tools::prefs::user_prefs(&state.pool, &ns) {
                Ok(prefs) => {
                    let items: Vec<serde_json::Value> = prefs.into_iter()
                        .filter(|p| tag_filter.as_ref().map_or(true, |t| &p.tag == t))
                        .map(|p| serde_json::json!({
                            "key": p.key,
                            "value": p.value,
                            "importance": p.importance,
                            "tag": p.tag,
                            "confidence": p.confidence,
                            "created_at": p.created_at,
                        }))
                        .collect();
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
                &state.pool, &state.auth_pool, &state.hnsw, &state.db_path, &state.hnsw_status,
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
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key_val, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
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
        // ── 暗知识层 A1：夜间巩固哑工具（ns 门控已在 handle_tool_call 完成）──
        "memory_fetch_unconsolidated" => {
            let since = args.get("since").and_then(|v| v.as_str()).unwrap_or("1970-01-01");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as i64;
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            let mut stmt = match conn.prepare(
                "SELECT id, content, category, created_at FROM memories
                 WHERE namespace = ?1 AND created_at > ?2
                   AND (category = 'observation' OR category IS NULL)
                 ORDER BY created_at ASC LIMIT ?3",
            ) {
                Ok(s) => s, Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            let rows = stmt.query_map(rusqlite::params![ns, since, limit], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "content": r.get::<_, Option<String>>(1)?,
                    "category": r.get::<_, Option<String>>(2)?,
                    "created_at": r.get::<_, Option<String>>(3)?,
                }))
            });
            let items: Vec<serde_json::Value> = match rows {
                Ok(r) => r.flatten().collect(),
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            serde_json::to_string(&serde_json::json!({"status":"ok","count":items.len(),"items":items})).unwrap_or_default()
        },
        "dream_state_get" => {
            let phase = args.get("phase").and_then(|v| v.as_str()).unwrap_or("consolidate");
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            let row = conn.query_row(
                "SELECT last_run, cursor_ts, runs, items_out FROM dream_state
                 WHERE phase = ?1 AND namespace = ?2",
                rusqlite::params![phase, ns],
                |r| Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                )),
            );
            match row {
                Ok((last_run, cursor_ts, runs, items_out)) => serde_json::to_string(&serde_json::json!({
                    "status":"ok","phase":phase,"namespace":ns,
                    "last_run":last_run,
                    "cursor_ts":cursor_ts.unwrap_or_else(|| "1970-01-01".into()),
                    "runs":runs,"items_out":items_out
                })).unwrap_or_default(),
                // 首跑：无记录，返回 epoch 游标让其处理全量
                Err(rusqlite::Error::QueryReturnedNoRows) => serde_json::to_string(&serde_json::json!({
                    "status":"ok","phase":phase,"namespace":ns,
                    "last_run":serde_json::Value::Null,"cursor_ts":"1970-01-01","runs":0,"items_out":0
                })).unwrap_or_default(),
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
        },
        "dream_state_update" => {
            let phase = args.get("phase").and_then(|v| v.as_str()).unwrap_or("consolidate");
            let cursor_ts = args.get("cursor_ts").and_then(|v| v.as_str()).unwrap_or("");
            let items_out = args.get("items_out").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
            let sessions = args.get("sessions_processed").and_then(|v| v.as_u64()).unwrap_or(0) as i64;

            // P1-4 一：cursor_ts 非空校验（防止游标回退到 epoch）
            if cursor_ts.is_empty() {
                return r#"{"status":"error","message":"cursor_ts must be non-empty ISO-8601"}"#.to_string();
            }

            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };

            // P1-4 二：cursor_ts 必须是前进的（比现有游标新）
            let current_cursor: Option<String> = conn
                .query_row(
                    "SELECT cursor_ts FROM dream_state WHERE phase=?1 AND namespace=?2",
                    rusqlite::params![phase, ns],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            if let Some(ref prev) = current_cursor {
                if cursor_ts <= prev.as_str() {
                    return format!(r#"{{"status":"error","message":"cursor_ts must advance: new '{}' not newer than previous '{}'"}}"#, cursor_ts, prev);
                }
            }

            // P1-4 三：ns 限流（per-(phase,namespace) cooldown，防监听洪流冲库）
            let cooldown_secs = dream_cooldown(phase);
            let ok_to_proceed: bool = conn
                .query_row(
                    "SELECT (julianday('now') - julianday(COALESCE(last_run,'1970-01-01'))) * 86400 >= ?1
                     FROM dream_state WHERE phase=?2 AND namespace=?3",
                    rusqlite::params![cooldown_secs as f64, phase, ns],
                    |r| r.get(0),
                )
                .unwrap_or(true); // 首跑无记录 = 放行
            if !ok_to_proceed {
                return format!(r#"{{"status":"error","message":"rate limited: phase '{}' for ns '{}' requires {}s cooldown"}}"#, phase, ns, cooldown_secs);
            }

            // P1-4 四：推进游标（含 sessions_processed 累加）
            match conn.execute(
                "INSERT INTO dream_state(phase, namespace, last_run, cursor_ts, runs, items_out, sessions_processed)
                 VALUES(?1, ?2, datetime('now'), ?3, 1, ?4, ?5)
                 ON CONFLICT(phase, namespace) DO UPDATE SET
                   last_run=datetime('now'),
                   cursor_ts=excluded.cursor_ts,
                   runs=dream_state.runs+1,
                   items_out=dream_state.items_out+excluded.items_out,
                   sessions_processed=dream_state.sessions_processed+excluded.sessions_processed",
                rusqlite::params![phase, ns, cursor_ts, items_out, sessions],
            ) {
                Ok(_) => serde_json::to_string(&serde_json::json!({
                    "status":"ok","phase":phase,"namespace":ns,"cursor_ts":cursor_ts,
                    "runs":"(incremented)","items_out":"(accumulated)"
                })).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        // ── 知识图谱 B：实体工具 ──
        "entity_upsert" => {
            let default_id = uuid::Uuid::new_v4().to_string();
            let entity_id = args.get("entity_id").and_then(|v| v.as_str())
                .unwrap_or(&default_id)
                .to_string();
            let entity_type = args.get("entity_type").and_then(|v| v.as_str()).unwrap_or("other");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let aliases = args.get("aliases").and_then(|v| v.as_str()).unwrap_or("[]");
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            match conn.execute(
                "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   name=excluded.name, aliases=excluded.aliases, summary=excluded.summary",
                rusqlite::params![entity_id, ns, entity_type, name, aliases, summary],
            ) {
                Ok(_) => serde_json::to_string(&serde_json::json!({"status":"ok","entity_id":entity_id})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "entity_add_mention" => {
            let entity_id = args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let memory_id = args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
            let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            match conn.execute(
                "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![entity_id, memory_id, context, ns],
            ) {
                Ok(_) => serde_json::to_string(&serde_json::json!({"status":"ok"})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "entity_add_edge" => {
            let source = args.get("source_entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let target = args.get("target_entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let rtype = args.get("relation_type").and_then(|v| v.as_str()).unwrap_or("related_to");
            let weight = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let evidence = args.get("evidence").and_then(|v| v.as_str()).unwrap_or("");
            // P1-5: 可选时序真值区间
            let valid_from = args.get("valid_from").and_then(|v| v.as_str());
            let valid_to = args.get("valid_to").and_then(|v| v.as_str());
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            match conn.execute(
                "INSERT INTO entity_edges(namespace, source_entity_id, target_entity_id, relation_type, weight, evidence, valid_from, valid_to)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(namespace, source_entity_id, target_entity_id, relation_type) DO UPDATE SET
                   weight=excluded.weight, evidence=excluded.evidence",
                rusqlite::params![ns, source, target, rtype, weight, evidence, valid_from, valid_to],
            ) {
                Ok(_) => serde_json::to_string(&serde_json::json!({"status":"ok"})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        },
        "entity_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let ent_type = args.get("entity_type").and_then(|v| v.as_str());
            let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(20) as i64;
            let conn = match state.pool.get() {
                Ok(c) => c, Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            let has_type_filter = ent_type.is_some();
            let sql = if has_type_filter {
                "SELECT id, entity_type, name, aliases, summary FROM entities
                 WHERE namespace=?1 AND entity_type=?2
                   AND (name LIKE ?3 OR aliases LIKE ?3 OR summary LIKE ?3)
                 ORDER BY name LIMIT ?4"
            } else {
                "SELECT id, entity_type, name, aliases, summary FROM entities
                 WHERE namespace=?1
                   AND (name LIKE ?2 OR aliases LIKE ?2 OR summary LIKE ?2)
                 ORDER BY name LIMIT ?3"
            };
            let like = format!("%{}%", query);
            let rows = if let Some(et) = ent_type {
                conn.prepare(sql).ok().and_then(|mut st| {
                    st.query_map(rusqlite::params![ns, et, like, max_results], |r| {
                        Ok(serde_json::json!({
                            "id": r.get::<_, String>(0)?,
                            "entity_type": r.get::<_, String>(1)?,
                            "name": r.get::<_, String>(2)?,
                            "aliases": r.get::<_, Option<String>>(3)?,
                            "summary": r.get::<_, Option<String>>(4)?,
                        }))
                    }).ok().map(|r| r.flatten().collect::<Vec<_>>())
                }).unwrap_or_default()
            } else {
                conn.prepare(sql).ok().and_then(|mut st| {
                    st.query_map(rusqlite::params![ns, like, max_results], |r| {
                        Ok(serde_json::json!({
                            "id": r.get::<_, String>(0)?,
                            "entity_type": r.get::<_, String>(1)?,
                            "name": r.get::<_, String>(2)?,
                            "aliases": r.get::<_, Option<String>>(3)?,
                            "summary": r.get::<_, Option<String>>(4)?,
                        }))
                    }).ok().map(|r| r.flatten().collect::<Vec<_>>())
                }).unwrap_or_default()
            };
            serde_json::to_string(&serde_json::json!({"status":"ok","count":rows.len(),"entities":rows})).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_state() -> Arc<AppState> {
        let pool = memoria_core::storage::create_pool(":memory:", 4).expect("pool");
        memoria_core::storage::init_schema(&pool).expect("schema");
        memoria_core::storage::init_core_tables(&pool).expect("core");
        let auth_pool = memoria_core::storage::create_pool(":memory:", 4).expect("auth pool");
        memoria_core::storage::init_schema(&auth_pool).expect("auth schema");
        memoria_core::auth::init_auth_tables(&auth_pool).expect("auth tables");
        Arc::new(AppState {
            pool,
            auth_pool,
            hnsw: Arc::new(memoria_core::vector::HnswIndex::new()),
            hnsw_status: "uninitialized".to_string(),
            query_cache: Arc::new(memoria_core::vector::QueryCache::new()),
            admin_key: "test-admin-key".to_string(),
            bridge_url: "http://127.0.0.1:9000/mcp".to_string(),
            http_client: reqwest::Client::new(),
            db_path: ":memory:".to_string(),
            backup_dir: ".".to_string(),
            vec_index_path: ":memory:".to_string(),
        })
    }

    #[test]
    fn test_health_full_no_auth_is_forbidden() {
        let state = build_test_state();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let headers = axum::http::HeaderMap::new();
            let res = health_check_full(axum::extract::State(state), headers).await;
            assert!(res.is_err(), "anonymous /health/full must return 403");
        });
    }

    #[test]
    fn test_health_full_legacy_admin_key_ok() {
        let state = build_test_state();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("x-admin-key", axum::http::HeaderValue::from_static("test-admin-key"));
            let res = health_check_full(axum::extract::State(state), headers).await;
            assert!(res.is_ok(), "valid admin key must allow /health/full");
        });
    }
}
