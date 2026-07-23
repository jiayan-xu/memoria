//! MCP 协议服务端 — 符合 MCP 规范
//!
//! 独立模块，由 main.rs 调用 build_app() 启动。

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use chrono;
use std::sync::Arc;

use memoria_core::auth::{self, AuthResult};
use memoria_core::search;
use memoria_core::storage;
use memoria_core::tools;

/// 不认识的 MCP 工具调用转发到 A2A Bridge
const BRIDGE_TOOLS: &[&str] = &[
    "cross_agent_query",
    "system_status",
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
    /// P3-0: 查询时嵌入服务地址。空 = 禁用 HNSW 语义信号（仅 FTS5+temporal+importance+category）。
    /// 非空（如 http://127.0.0.1:8777/embed）= 启用，memory_search 会先把 query 向量注入 QueryCache。
    pub embedding_url: String,
    pub http_client: reqwest::Client,
    pub db_path: String,
    pub backup_dir: String,
    pub vec_index_path: String,
    /// P2-12：审计事件有界通道（背压）。落库 worker 在 main.rs 启动。
    pub audit_tx: tokio::sync::mpsc::Sender<AuditEvent>,
}

/// 审计事件（经有界通道异步落库，提供背压）
pub struct AuditEvent {
    pub agent_id: String,
    pub tool: String,
    pub params: String,
    pub allowed: bool,
}

/// 审计写入 worker：从有界通道取事件，隔离到阻塞线程池写 auth_pool。
/// 通道满（默认 1024）时 `spawn_audit` 的 `try_send` 丢弃该条审计（fire-and-forget，
/// 不阻塞业务响应），从而防止审计洪峰下无限 `spawn_blocking` 拖垮服务（P2-12 背压）。
pub fn spawn_audit_worker(
    pool: storage::SqlitePool,
    mut rx: tokio::sync::mpsc::Receiver<AuditEvent>,
) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let p = pool.clone();
            let _ = tokio::task::spawn_blocking(move || {
                auth::audit_log(&p, &ev.agent_id, &ev.tool, &ev.params, ev.allowed);
            })
            .await;
        }
    });
}

/// P3-0: 查询时生成 query embedding —— 修复「HNSW 语义搜索从未生效」的根因。
///
/// 独立 MCP 服务（memoria-server）在查询时调用本地嵌入服务（默认 127.0.0.1:8777/embed）
/// 将 query 文本转为向量，写入 QueryCache 后 `semantic_search` 即可参与融合排序。
///
/// 任何错误（未配置 / 服务不可用 / 超时 / 解析失败）均返回 `None` —— 优雅降级，
/// 不影响既有 FTS5 + temporal + importance + category 信号。绝不抛错、绝不阻塞主链路。
async fn embed_query(client: &reqwest::Client, url: &str, query: &str) -> Option<Vec<f32>> {
    let body = serde_json::json!({ "texts": [query] });
    let resp = client
        .post(url)
        .timeout(std::time::Duration::from_secs(5))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let arr = v.get("embeddings")?.as_array()?;
    let first = arr.first()?.as_array()?;
    let vec: Vec<f32> = first
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect();
    if vec.is_empty() {
        None
    } else {
        Some(vec)
    }
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
async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    // 公开探针：含嵌入通道摘要（无密钥），供托盘 / PFAiX / agent-core 判断语义是否在线。
    let emb = memoria_core::health::check_embedding_endpoint(&state.embedding_url);
    let semantic_ok = emb.status == "pass";
    Json(serde_json::json!({
        "status": if semantic_ok { "ok" } else { "degraded" },
        "service": "memoria",
        "version": env!("MEMORIA_BUILD_VERSION"),
        "embed": {
            "configured": !state.embedding_url.trim().is_empty(),
            "status": emb.status,
            "message": emb.message,
            "duration_ms": emb.duration_ms,
        },
    }))
}

/// 健康检查（完整版 — P0: 启动自检）
async fn health_check_full(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // P0-2 统一 require_admin：admin 角色（X-Agent-Id/Key）或 x-admin-key 兜底，与 P0-1 一致
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let agent_key = headers
        .get("x-agent-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let legacy_key = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let auth_result =
        auth::authenticate(&state.auth_pool, agent_id, agent_key).unwrap_or_else(|_| {
            auth::AuthResult {
                agent_id: String::new(),
                allowed_ns: Vec::new(),
                role: String::new(),
            }
        });
    if !crate::permissions::require_admin(&auth_result, legacy_key, &state.admin_key) {
        return Err(StatusCode::FORBIDDEN);
    }
    let st = state.clone();
    let emb_url = state.embedding_url.clone();
    let report = tokio::task::spawn_blocking(move || {
        memoria_core::health::run_health_check(
            &st.pool,
            &st.auth_pool,
            &st.hnsw,
            &st.db_path,
            &st.hnsw_status,
            &emb_url,
        )
    })
    .await
    .unwrap_or_else(|_| memoria_core::health::HealthReport {
        overall: "fail".to_string(),
        hard_checks: vec![],
        soft_checks: vec![],
        timestamp: chrono::Utc::now().to_rfc3339(),
        version: env!("MEMORIA_BUILD_VERSION").to_string(),
    });
    let overall = report.overall.clone();
    let embed_check = report
        .soft_checks
        .iter()
        .find(|c| c.name == "embedding")
        .map(|c| {
            serde_json::json!({
                "status": c.status,
                "message": c.message,
                "duration_ms": c.duration_ms,
            })
        });
    Ok(Json(serde_json::json!({
        "status": overall,
        "service": "memoria",
        "version": env!("MEMORIA_BUILD_VERSION"),
        "embed": embed_check,
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
    let params = body
        .get("params")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();

    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous");
    let agent_key = headers
        .get("x-agent-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // 联调：入站 x-trace-id（agent-core 转发），接入统一 trace 链
    let trace_id = headers
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("none")
        .to_string();

    let result = match method {
        "initialize" => {
            // MCP 协议版本协商：回显客户端请求的版本（memoria-core 仅用基础 tools 能力，
            // 任何版本均兼容），缺省回退到广泛支持的 2024-11-05，避免客户端因版本过新拒绝握手。
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            rpc_ok(
                &id,
                serde_json::json!({
                    "protocolVersion": requested,
                    "serverInfo": {"name": "memoria", "version": "0.3.0"},
                    "capabilities": {"tools": {}},
                }),
            )
        }
        "tools/list" => rpc_ok(&id, serde_json::json!({"tools": tools_list()})),
        "tools/call" => {
            handle_tool_call(&state, &params, &id, agent_id, agent_key, &trace_id).await
        }
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
        tool(
            "memory_search",
            "搜索记忆",
            serde_json::json!({
                "query": {"type": "string", "description": "搜索关键词（必填）"},
                "max_results": {"type": "number", "description": "最大返回结果数", "default": 5},
                "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串如 [\"a\",\"b\"]"},
                "as_of": {"type": "string", "description": "P1-5 时序真值：仅返回该 ISO-8601 时刻有效的记忆；不传则默认 now，自动过滤已失效"}
            }),
        ),
        tool(
            "memory_search_v2",
            "多信号融合搜索",
            serde_json::json!({
                "query": {"type": "string", "description": "搜索关键词（必填）"},
                "max_results": {"type": "number", "description": "最大返回结果数", "default": 5},
                "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串"},
                "as_of": {"type": "string", "description": "P1-5 时序真值：仅返回该 ISO-8601 时刻有效的记忆；不传则默认返回「当前真值」（superseded_by IS NULL 且未失效）"},
                "include_superseded": {"type": "boolean", "description": "P0: 是否包含已被取代的历史记忆（默认 false，仅看当前真值）"},
                "enrich_ledger": {"type": "boolean", "description": "O6：可选账本富化，默认 false；ledger 主路径在 memory_context"}
            }),
        ),
        tool(
            "memory_remember",
            "记录一条记忆",
            serde_json::json!({
                "content": {"type": "string", "description": "记忆内容（必填）"},
                "namespace": {"type": "string", "description": "命名空间（NamespaceArg 必填）：写入目标命名空间；不传将被拒绝（Namespace argument required）"},
                "category": {"type": "string", "description": "类别，默认 fact"},
                "importance": {"type": "number", "description": "重要度 1-5，默认 3"},
                "source": {"type": "string", "description": "来源，默认 mcp"},
                "tags": {"type": "string", "description": "标签 JSON 数组字符串（事件日：occurred:YYYY-MM-DD，O3）"},
                "valid_from": {"type": "string", "description": "P1-5 时序真值：记忆生效起点 ISO-8601（默认插入时刻）"},
                "valid_to": {"type": "string", "description": "P1-5 时序真值：记忆失效点 ISO-8601（默认 NULL，长期有效）"},
                "event_time": {"type": "string", "description": "DEPRECATED(O2)：勿写列；若传入则映射为 tags occurred:YYYY-MM-DD，不再 UPDATE event_time"},
                "supersedes_id": {"type": "string", "description": "P0-4: 显式取代的目标记忆 id（须同 ns 且为当前 tip）；失败返回 404/403/409"},
                "relation": {"type": "string", "description": "记忆边类型：updates|extends|derives（默认 updates）"},
                "actor": {"type": "string", "description": "PR1(Phase B 前置)：事实作者/来源主体；NULL 视为 agent_inferred"},
                "memory_type": {"type": "string", "description": "PR1：记忆类型 declarative/procedural/episodic/...；NULL 视为 declarative"},
                "parent_id": {"type": "string", "description": "PR1：原子事实挂回的原始记忆 id（1 raw→N 原子事实时用）"},
                "raw_ref": {"type": "string", "description": "PR1：原文旁路存储引用（避免整段 raw 入库）"}
            }),
        ),
        tool(
            "ingest_document",
            "部门共享文档入库（已抽文本）：写入 memory_type=document 清单+分块；默认 ns 可为 org/.../dept/...。二进制 PDF/DOCX 请用 HTTP POST /api/documents",
            serde_json::json!({
                "text": {"type": "string", "description": "已抽取的文档正文（必填）"},
                "filename": {"type": "string", "description": "原文件名，如 report.pdf / 制度.docx（必填）"},
                "namespace": {"type": "string", "description": "目标命名空间；部门共享例：org/cs-pufa-2nd-thermal/dept/gufei"}
            }),
        ),
        tool(
            "memory",
            "写入记忆（memory_remember 薄别名）：支持 supersedes_id / relation / valid_*",
            serde_json::json!({
                "content": {"type": "string", "description": "记忆内容（必填）"},
                "namespace": {"type": "string", "description": "命名空间（NamespaceArg 必填）：写入目标命名空间；不传将被拒绝（Namespace argument required）"},
                "category": {"type": "string", "description": "类别，默认 fact"},
                "importance": {"type": "number", "description": "重要度 1-5，默认 3"},
                "source": {"type": "string", "description": "来源，默认 mcp"},
                "tags": {"type": "string", "description": "标签 JSON 数组字符串（事件日用 occurred:YYYY-MM-DD，O3）"},
                "valid_from": {"type": "string", "description": "P1-5 时序真值：记忆生效起点 ISO-8601"},
                "valid_to": {"type": "string", "description": "P1-5 时序真值：记忆失效点 ISO-8601"},
                "event_time": {"type": "string", "description": "DEPRECATED(O2)：映射为 occurred: tag，不写列"},
                "supersedes_id": {"type": "string", "description": "P0-4: 显式取代的目标记忆 id"},
                "relation": {"type": "string", "description": "记忆边类型：updates|extends|derives（默认 updates）"},
                "actor": {"type": "string", "description": "PR1(Phase B 前置)：事实作者/来源主体；NULL 视为 agent_inferred"},
                "memory_type": {"type": "string", "description": "PR1：记忆类型 declarative/procedural/episodic/...；NULL 视为 declarative"},
                "parent_id": {"type": "string", "description": "PR1：原子事实挂回的原始记忆 id（1 raw→N 原子事实时用）"},
                "raw_ref": {"type": "string", "description": "PR1：原文旁路存储引用（避免整段 raw 入库）"}
            }),
        ),
        tool(
            "memory_profile",
            "会话开场注入：返回 ns 的静态偏好(static)+近期动态(dynamic)合成视图，计入 profile 配额",
            serde_json::json!({
                "namespace": {"type": "string", "description": "命名空间，默认 default"},
                "static_limit": {"type": "number", "description": "static 条数上限，默认 12"},
                "dynamic_limit": {"type": "number", "description": "dynamic 条数上限，默认 15"},
                "as_of": {"type": "string", "description": "可选 ISO-8601：按该时刻 valid_* 过滤（默认 now + tip）"}
            }),
        ),
        tool(
            "memory_context",
            "会话开场注入：memory_profile + 可选 query 追加 top-k recall，产出 prompt_block",
            serde_json::json!({
                "namespace": {"type": "string", "description": "命名空间，默认 default"},
                "query": {"type": "string", "description": "可选：本轮用户首句，用于追加 recall"},
                "recall_k": {"type": "number", "description": "recall 条数，默认 3"},
                "include_profile": {"type": "boolean", "description": "是否包含 profile，默认 true"},
                "static_limit": {"type": "number", "description": "static 条数上限，默认 12"},
                "dynamic_limit": {"type": "number", "description": "dynamic 条数上限，默认 15"},
                "as_of": {"type": "string", "description": "可选 ISO-8601：透传到 profile 过滤"}
            }),
        ),
        tool(
            "memory_recall",
            "回忆检索（别名 memory_search_v2）：默认 isLatest，走 search 配额",
            serde_json::json!({
                "query": {"type": "string", "description": "搜索关键词（必填）"},
                "max_results": {"type": "number", "description": "最大返回结果数，默认 5"},
                "tags": {"type": "string", "description": "标签过滤，JSON 数组字符串"},
                "as_of": {"type": "string", "description": "P1-5 时序真值：仅返回该 ISO-8601 时刻有效的记忆；不传则默认返回当前真值"},
                "include_superseded": {"type": "boolean", "description": "P0: 是否包含已被取代的历史记忆（默认 false）"},
                "enrich_ledger": {"type": "boolean", "description": "O6：可选账本富化，默认 false"}
            }),
        ),
        tool(
            "memory_observe",
            "记录观察（低优先级）",
            serde_json::json!({
                "dialog": {"type": "string", "description": "对话/观察内容"},
                "role": {"type": "string", "description": "角色，默认 user"},
                "source": {"type": "string", "description": "来源，默认 mcp"},
                "session_id": {"type": "string", "description": "会话 ID"}
            }),
        ),
        tool(
            "memory_quota_status",
            "查询本命名空间当前配额用量与上限（P2-2 滥用防护；写入=日限额，搜索=分钟限额，备份=小时限额）",
            serde_json::json!({
                "namespace": {"type": "string", "description": "命名空间，默认 default"}
            }),
        ),
        tool(
            "register_agent",
            "注册Agent（需要Admin key）",
            serde_json::json!({
                "agent_id": {"type": "string", "description": "新 Agent ID"},
                "display_name": {"type": "string", "description": "显示名"},
                "admin_key": {"type": "string", "description": "Admin Key"},
                "namespace": {"type": "string", "description": "命名空间"}
            }),
        ),
        tool(
            "register_user",
            "注册个人登录账号（本地账密）：user_id + password，命名空间默认 agent/{user_id}（可选 namespace 覆盖）",
            serde_json::json!({
                "user_id": {"type": "string", "description": "登录用户名（唯一）"},
                "display_name": {"type": "string", "description": "显示名"},
                "password": {"type": "string", "description": "登录口令"},
                "namespace": {"type": "string", "description": "可选：命名空间覆盖（逗号分隔多个）"}
            }),
        ),
        tool(
            "login_user",
            "个人账号登录：校验 user_id + password，成功回传 badge_token 供客户端后续鉴权",
            serde_json::json!({
                "user_id": {"type": "string", "description": "登录用户名"},
                "password": {"type": "string", "description": "登录口令"}
            }),
        ),
        tool(
            "import_install_memories",
            "迁移记忆命名空间（需 Admin key）：把 from_ns 下记忆整体移到 to_ns（换设备/登录后归并旧记忆）。id=内容哈希与 ns 无关，故仅改 namespace，无需重建索引。",
            serde_json::json!({
                "from_ns": {"type": "string", "description": "源命名空间"},
                "to_ns": {"type": "string", "description": "目标命名空间"},
                "admin_key": {"type": "string", "description": "Admin Key"}
            }),
        ),
        tool(
            "get_allowed_ns",
            "返回当前调用者自身的命名空间授权列表（供 agent-core 按项目过滤 MCP 工具）",
            serde_json::json!({}),
        ),
        tool(
            "audit_query",
            "查询审计日志",
            serde_json::json!({
                "limit": {"type": "number", "description": "返回条数，默认 50"}
            }),
        ),
        tool("db_stats", "数据库统计", serde_json::json!({})),
        tool(
            "a2a_send",
            "向另一个Agent发送消息",
            serde_json::json!({
                "to": {"type": "string", "description": "目标 Agent ID（必填）"},
                "subject": {"type": "string", "description": "主题"},
                "body": {"type": "string", "description": "正文"}
            }),
        ),
        tool(
            "a2a_recv",
            "接收发给自己的消息",
            serde_json::json!({
                "limit": {"type": "number", "description": "返回条数，默认 10"}
            }),
        ),
        tool(
            "agent_list",
            "列出已注册的Agent（需要Admin key）",
            serde_json::json!({
                "admin_key": {"type": "string", "description": "Admin Key"}
            }),
        ),
        tool(
            "agent_revoke",
            "撤销Agent令牌（需要Admin key）",
            serde_json::json!({
                "agent_id": {"type": "string", "description": "目标 Agent ID（必填）"},
                "admin_key": {"type": "string", "description": "Admin Key"}
            }),
        ),
    ];
    // Bridge 转发工具（圆桌 panel_discuss 已 native 进 agent-core，不再经 bridge）
    for name in BRIDGE_TOOLS {
        let desc = match *name {
            "cross_agent_query" => "向另一个Agent提问",
            "system_status" => "检查各Agent连接状态",
            "reasonix_dispatch" => "派发编码任务给Reasonix",
            "continue_task" => "继续一个等待输入的任务",
            "auto_route" => "动态路由查询到最佳Agent",
            _ => "Bridge 转发工具",
        };
        tools.push(tool(name, desc, serde_json::json!({})));
    }
    // Skill Market 工具
    tools.push(tool(
        "skill_market_search",
        "搜索技能市场中的可用技能",
        serde_json::json!({
            "query": {"type": "string", "description": "搜索关键词"},
            "category": {"type": "string", "description": "技能分类"},
            "max_results": {"type": "number", "description": "最大结果数，默认 10"}
        }),
    ));
    tools.push(tool(
        "skill_market_info",
        "查看技能详细信息",
        serde_json::json!({
            "name": {"type": "string", "description": "技能名（必填）"}
        }),
    ));
    tools.push(tool(
        "skill_market_publish",
        "发布技能到市场（需要Admin key或同namespace）",
        serde_json::json!({
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
        }),
    ));
    tools.push(tool(
        "skill_market_install",
        "安装技能到指定Agent（需要Admin key或同namespace管理权）",
        serde_json::json!({
            "skill_name": {"type": "string", "description": "技能名（必填）"},
            "target_agent": {"type": "string", "description": "目标 Agent ID（必填）"},
            "admin_key": {"type": "string", "description": "Admin Key"}
        }),
    ));
    tools.push(tool(
        "skill_market_list_installed",
        "查询指定Agent已安装的技能列表",
        serde_json::json!({
            "agent_id": {"type": "string", "description": "Agent ID（必填）"}
        }),
    ));
    // Phase 3 工具（decay/graph/prefs）
    tools.push(tool(
        "memory_decay",
        "运行衰减循环（降低旧记忆权重）",
        serde_json::json!({}),
    ));
    tools.push(tool(
        "memory_graph",
        "构建记忆关系图",
        serde_json::json!({
            "batch_size": {"type": "number", "description": "批大小，默认 50"}
        }),
    ));
    tools.push(tool("memory_user_prefs", "获取用户偏好设置（按 ns 聚合，hard_rule 优先；写入走 memory_remember，category=preference，tags∈pref|hard_rule|style）", serde_json::json!({
        "tag": {"type": "string", "description": "可选：仅返回指定类型偏好 hard_rule|pref|style"}
    })));
    tools.push(tool(
        "memory_recent_decisions",
        "获取最近的决策记录",
        serde_json::json!({
            "limit": {"type": "number", "description": "返回条数，默认 10"}
        }),
    ));
    // P0 工具（backup/health/dedup）
    tools.push(tool(
        "memory_backup",
        "手动触发数据库备份（GFS 轮转）",
        serde_json::json!({}),
    ));
    tools.push(tool(
        "memory_backup_list",
        "列出所有备份文件",
        serde_json::json!({}),
    ));
    tools.push(tool(
        "memory_health",
        "完整健康检查报告",
        serde_json::json!({}),
    ));
    // ── P2-4 导入导出与迁移 ──
    tools.push(tool("memory_export", "导出某命名空间的记忆/实体为流式 JSONL（认证后；大库分块防 OOM）", serde_json::json!({
        "namespace": {"type": "string", "description": "命名空间，默认 default"},
        "include_vectors": {"type": "boolean", "description": "是否包含 embedding 向量（base64），默认 false"}
    })));
    tools.push(tool("memory_import", "从 memory_export 的 JSONL 导入记忆/实体到本命名空间（认证后；INSERT OR IGNORE 幂等，重复导入不翻倍）", serde_json::json!({
        "namespace": {"type": "string", "description": "目标命名空间，默认 default"},
        "jsonl": {"type": "string", "description": "memory_export 产出的 JSONL 文本"},
        "on_conflict": {"type": "string", "description": "ignore（默认，跳过已存在）或 replace（覆盖）"}
    })));
    tools.push(tool(
        "memory_migration_manifest",
        "生成跨机迁移包清单：DB + HNSW 的 sha256 校验和与全表行数（admin；与 GFS 备份格式统一）",
        serde_json::json!({}),
    ));
    tools.push(tool(
        "memory_dedup_chain",
        "查询某条记忆的 superseded 链",
        serde_json::json!({
            "memory_id": {"type": "string", "description": "记忆 ID（必填）"}
        }),
    ));
    tools.push(tool(
        "memory_merge",
        "手动合并两条近义记忆（需 Admin key）",
        serde_json::json!({
            "keep_id": {"type": "string", "description": "保留的记忆 ID（必填）"},
            "merge_id": {"type": "string", "description": "被合并的记忆 ID（必填）"},
            "admin_key": {"type": "string", "description": "Admin Key"}
        }),
    ));
    // ── PR4（Phase A 演化）：演化写回 + 回滚（认知在 agent-core Dream/consolidate，此处哑存储）──
    tools.push(tool(
        "memory_evolve",
        "（PR4）对一条记忆施加演化：写入 evolved_context/evolved_at/link_count，并记 evolution_log（old_value 可回滚）。由 agent-core 夜间 consolidate 批处理调用，亦可手动。",
        serde_json::json!({
            "target_id": {"type": "string", "description": "被演化的记忆 id（必填）"},
            "namespace": {"type": "string", "description": "记忆所属命名空间（默认 default）"},
            "evolved_context": {"type": "string", "description": "演化合成的上下文/摘要（必填）"},
            "link_count": {"type": "number", "description": "演化后关联边数；省略则按 memory_relations 自动统计"},
            "model": {"type": "string", "description": "演化所用模型标识（记入 evolution_log）"},
            "change_type": {"type": "string", "description": "变更类型，如 context_update/links_update"}
        }),
    ));
    tools.push(tool(
        "evolution_rollback",
        "（PR4）按 evolution_log.id 回滚某次演化，恢复 old_value（evolved_context/link_count）。敏感操作，需 Admin key。",
        serde_json::json!({
            "log_id": {"type": "string", "description": "evolution_log 行 id（必填）"},
            "admin_key": {"type": "string", "description": "Admin Key"},
            "namespace": {"type": "string", "description": "记忆所属命名空间（NamespaceArg 必填）；不传将被拒绝（Namespace argument required）"}
        }),
    ));
    tools.push(tool(
        "evolution_log_query",
        "（PR5）只读查询 evolution_log 负样本（rolled_back/corrected 等），供 agent-core 元进化闭环采样。纯读取，不写库、不调 LLM。",
        serde_json::json!({
            "change_types": {"type": "array", "description": "按变更类型过滤，如 [\"rolled_back\",\"corrected\"]；空数组=不过滤"},
            "since": {"type": "string", "description": "created_at 下界 ISO8601（YYYY-MM-DDTHH:MM:SS），默认 1970-01-01"},
            "limit": {"type": "number", "description": "最多返回条数，默认 500，上限 5000"},
            "namespace": {"type": "string", "description": "命名空间（NamespaceArg 必填）；不传将被拒绝（Namespace argument required）"}
        }),
    ));
    tools.push(tool(
        "memory_evolve_auto",
        "（G2 自动演化）对命名空间内 evolved_at IS NULL 的记忆做内置提升式演化（memoria:builtin-auto，不调 LLM），写入 evolution_log 并标 evolved_at。幂等（只处理未演化），可周期/事件触发。",
        serde_json::json!({
            "namespace": {"type": "string", "description": "命名空间，默认 default"},
            "limit": {"type": "number", "description": "本次最多处理条数，默认 50，上限 2000"}
        }),
    ));
    tools.push(tool(
        "agent_registry_cleanup",
        "（G4 注册表清理）保守幂等移除 agent_registry 中死行（badge 为空 / test_ / demo_ 占位）。不删真实 agent。需 Admin key。",
        serde_json::json!({
            "admin_key": {"type": "string", "description": "Admin Key"}
        }),
    ));
    tools.push(tool(
        "memory_maintenance_normalize",
        "Q1 维护：归一 valid_from/valid_to 时间格式（补 T）+ 清洗 1970 哨兵为空。⚠️ 破坏性，调用前必须先 memory_backup（需 Admin key）",
        serde_json::json!({
            "admin_key": {"type": "string", "description": "Admin Key"}
        }),
    ));
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
        "query": {"type": "string", "description": "搜索关键词（匹配 name/aliases/summary 及记忆提及上下文 context）"},
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
    rpc_ok(
        id,
        serde_json::json!({"content": [{"type": "text", "text": text}]}),
    )
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
    trace_id: &str,
) -> serde_json::Value {
    let tool = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    // 联调：mcp.request span（与 agent-core http.request span 对接 x-trace-id 链）
    let span = tracing::info_span!("mcp.request", trace_id = %trace_id, tool = %tool, agent_id = %agent_id);
    let _guard = span.enter();
    let args = params
        .get("arguments")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let empty_args = serde_json::Map::new();
    let safe_args = if args.is_empty() { &empty_args } else { &args };

    // 鉴权：同步 SQLite 查询，必须隔离到阻塞线程池，否则会占住 async worker
    // 导致整服务冻结（initialize/tools/list 全卡）。
    let auth_result = authenticate_async(state, agent_id, agent_key).await;

    let auth = match auth_result {
        Some(a) => a,
        None => {
            spawn_audit(
                state,
                agent_id,
                tool,
                &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)),
                false,
            );
            return rpc_error(
                id,
                -32001,
                "Authentication failed. Send X-Agent-Id and X-Agent-Key headers.",
            );
        }
    };

    // 自省工具：仅返回调用者自身命名空间授权，无需命名空间参数，跳过 ns 门控
    if tool == "get_allowed_ns" {
        let text = serde_json::json!({
            "agent_id": auth.agent_id,
            "allowed_ns": auth.allowed_ns,
        })
        .to_string();
        spawn_audit(
            state,
            agent_id,
            tool,
            &format!("agent_id={}", auth.agent_id),
            true,
        );
        return rpc_ok_text(id, &text);
    }

    // P1-④ ns 门控：依据 NsPolicy 矩阵决定 namespace 来源与是否校验（去 unwrap_or("default")）
    let ns_policy = crate::permissions::matrix_lookup(tool).map(|e| e.ns_policy.clone());
    let ns: &str = match ns_policy {
        // 无命名空间概念的工具：用调用者主命名空间占位，跳过校验
        Some(crate::permissions::NsPolicy::None) => {
            auth.allowed_ns.first().map(|s| s.as_str()).unwrap_or("default")
        }
        // NamespaceArg 及其它需按 namespace 门控的变体：要求调用方显式传 namespace，缺失即拒绝
        // （防静默落到 "default" 造成跨租户访问）
        Some(_) => match safe_args.get("namespace").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                spawn_audit(
                    state,
                    agent_id,
                    tool,
                    &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)),
                    false,
                );
                return rpc_error(id, -32002, "Namespace argument required for this tool.");
            }
        },
        // 未在矩阵登记的工具：维持历史行为（默认 default + 校验），避免误伤
        None => safe_args
            .get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or("default"),
    };
    let ns_authorized = match ns_policy {
        Some(crate::permissions::NsPolicy::None) => true, // 无 ns 概念，跳过校验
        _ => auth::check_ns_access(&auth, ns),
    };
    if !ns_authorized {
        spawn_audit(
            state,
            agent_id,
            tool,
            &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)),
            false,
        );
        return rpc_error(id, -32002, &format!("Namespace '{}' not authorized.", ns));
    }

    // Bridge 工具 → 异步转发（网络 I/O，留在 async worker，正确 yield）
    if BRIDGE_TOOLS.contains(&tool) {
        let text = forward_to_bridge(state, tool, safe_args).await;
        let allowed = !text.contains(r#""error""#);
        spawn_audit(
            state,
            agent_id,
            tool,
            &serde_json::to_string(&args).unwrap_or_else(|_| format!("{:?}", args)),
            allowed,
        );
        return rpc_ok_text(id, &text);
    }

    // P3-0: 查询时注入 query embedding（在 async 上下文，早于 spawn_blocking）。
    // 这是评测报告「HNSW 未生效」的根因修复点：原 MCP 路径从不生成 query 向量，
    // 导致 semantic_search 的 QueryCache 恒为空、HNSW 那 1174 个向量永不参与融合排序。
    // 此处异步调本地嵌入服务拿到向量写入共享 QueryCache，dispatch 内的 hybrid_search
    // 即可让语义信号真正参与 RRF 融合。未配置 MEMORIA_EMBEDDING_URL 或服务不可用 → 静默降级。
    if (tool == "memory_search" || tool == "memory_search_v2") && !state.embedding_url.is_empty() {
        if let Some(q) = safe_args.get("query").and_then(|v| v.as_str()) {
            if !q.is_empty() {
                if let Some(qvec) = embed_query(&state.http_client, &state.embedding_url, q).await {
                    state.query_cache.put(q, qvec);
                }
            }
        }
    }

    // P3-0 写入侧嵌入（与查询侧对称）：memory_remember 时把 content 向量注入共享 QueryCache，
    // 使 remember_with_dedup 能正常落表 + 入 HNSW。这是「写入侧从不嵌入、HNSW 恒空」根因的
    // 另一半修复——benchmark / 独立 HTTP 部署只传 content，从不预缓存向量，导致记忆写入后
    // 索引里永远没有它的向量。此处异步调本地嵌入服务补齐，未配置/不可用则静默降级。
    if (tool == "memory_remember" || tool == "memory") && !state.embedding_url.is_empty() {
        if let Some(c) = safe_args.get("content").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                if let Some(cvec) = embed_query(&state.http_client, &state.embedding_url, c).await {
                    state.query_cache.put(c, cvec);
                }
            }
        }
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
        spawn_audit(
            &st,
            &agent_id_owned,
            &tool_owned,
            &serde_json::to_string(&args_owned).unwrap_or_else(|_| format!("{:?}", args_owned)),
            allowed,
        );
        text
    })
    .await
    {
        Ok(t) => t,
        Err(_) => "{\"error\":\"dispatch task panicked\"}".to_string(),
    };

    rpc_ok_text(id, &text)
}

/// 鉴权（同步 SQLite 查询隔离到阻塞线程池，不占 async worker）
async fn authenticate_async(
    state: &Arc<AppState>,
    agent_id: &str,
    agent_key: &str,
) -> Option<auth::AuthResult> {
    let st = state.clone();
    let a = agent_id.to_string();
    let k = agent_key.to_string();
    tokio::task::spawn_blocking(move || auth::authenticate(&st.auth_pool, &a, &k).ok())
        .await
        .unwrap_or(None)
}

/// 审计日志写入（P2-12 背压）：经有界通道发送给 worker，fire-and-forget 不阻塞调用方。
/// 通道满（默认 1024）时丢弃该条审计（审计为尽力而为，不阻塞业务响应）。
fn spawn_audit(state: &Arc<AppState>, agent_id: &str, tool: &str, params: &str, allowed: bool) {
    let ev = AuditEvent {
        agent_id: agent_id.to_string(),
        tool: tool.to_string(),
        params: params.to_string(),
        allowed,
    };
    if let Err(e) = state.audit_tx.try_send(ev) {
        eprintln!("[Memoria][audit] dropped (channel full): {}", e);
    }
}

/// P1-4：Dream 阶段 ns 限流（per-(phase, namespace) cooldown，秒）。
/// 各阶段可通过环境变量 `MEMORIA_DREAM_COOLDOWN_<PHASE>` 覆盖，默认：
///   consolidate / entity_extract / graph = 300s，decay = 60s。
fn dream_cooldown(phase: &str) -> u64 {
    let key = format!("MEMORIA_DREAM_COOLDOWN_{}", phase.to_uppercase());
    if let Ok(v) = std::env::var(&key) {
        if let Ok(secs) = v.parse::<u64>() {
            return secs;
        }
    }
    // 兜底（未设环境变量时）
    if let Ok(v) = std::env::var("MEMORIA_DREAM_COOLDOWN_DEFAULT") {
        if let Ok(secs) = v.parse::<u64>() {
            return secs;
        }
    }
    match phase {
        "decay" => 60,
        _ => 300,
    }
}

/// P2-2：结构化配额超限错误。spawn_audit 会将其记 `allowed=false`（denied），
/// 形成滥用可见性；客户端可据 `retry_after_sec` 退避。
fn quota_error_json(kind: &str, limit: u64, retry_after_sec: u64) -> String {
    serde_json::to_string(&serde_json::json!({
        "status": "error",
        "code": "quota_exceeded",
        "message": format!("namespace quota exceeded for '{}' (limit {} per window)", kind, limit),
        "kind": kind,
        "limit": limit,
        "retry_after_sec": retry_after_sec,
    }))
    .unwrap_or_default()
}

/// P2-2：配额闸门。写/搜对 admin 豁免（避免运维自锁）；备份本身已 admin 门禁，
/// 故备份配额对所有人生效（限制备份频率、防备份风暴）。返回 true=放行。
fn quota_gate(state: &Arc<AppState>, ns: &str, kind: &str, role: &str) -> Option<String> {
    if kind != memoria_core::quota::KIND_BACKUP && role == "admin" {
        return None; // admin 免写/搜配额
    }
    match memoria_core::quota::check_quota(&state.pool, ns, kind) {
        Ok(()) => None,
        Err(e) => Some(quota_error_json(kind, e.limit, e.retry_after_sec)),
    }
}

fn dispatch(
    state: &Arc<AppState>,
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    _auth: &AuthResult,
) -> String {
    let ns = args
        .get("namespace")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    match tool {
        "memory_search" | "memory_search_v2" | "memory_recall" => {
            // P2-2 配额：搜索 QPS（admin 豁免）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_SEARCH, &_auth.role)
            {
                return err;
            }
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(5) as u32;
            let tags_filter: Option<Vec<String>> = args.get("tags").and_then(|v| {
                // tags 可以是 JSON 数组 ["a","b"] 或 JSON 字符串 "[\"a\",\"b\"]"
                if let Some(arr) = v.as_array() {
                    Some(
                        arr.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect(),
                    )
                } else if let Some(s) = v.as_str() {
                    // 尝试作为 JSON 数组字符串解析
                    Some(serde_json::from_str::<Vec<String>>(s).unwrap_or_default())
                } else {
                    None
                }
            });

            // P0-2 (§14.1 Q2): 默认不传 as_of → hybrid 走 is_latest_now（superseded_by IS NULL + 当前有效），
            // 即默认检索只见「当前真值」；显式 as_of → visible_as_of（仅 valid_*，含后来被取代的历史真值）。
            // include_superseded 默认 false（可由 MCP 参数开启以查看历史）。
            let as_of: Option<&str> = args.get("as_of").and_then(|v| v.as_str());
            let include_superseded = args
                .get("include_superseded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let fused = search::hybrid::hybrid_search(
                &state.pool,
                query,
                ns,
                max_results,
                Some(&state.hnsw),
                Some(&state.query_cache),
                as_of,
                include_superseded,
            )
            .unwrap_or_default();

            // Tags 过滤（如果有）
            let filtered: Vec<search::rrf::FusedResult> = if let Some(ref tags) = tags_filter {
                if tags.is_empty() {
                    fused
                } else {
                    fused
                        .into_iter()
                        .filter(|r| matches_memory_tags(&state.pool, &r.memory_id, tags))
                        .collect()
                }
            } else {
                fused
            };

            // O6：默认不 enrich ledger；仅 enrich_ledger=true 时可选富化
            let want_ledger = args
                .get("enrich_ledger")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let results: Vec<serde_json::Value> = if want_ledger {
                tools::ledger::enrich_ledger(&state.pool, ns, &filtered)
                    .into_iter()
                    .take(max_results as usize)
                    .map(|mut row| {
                        if let Some(obj) = row.as_object_mut() {
                            let text = obj
                                .get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            obj.insert(
                                "content".to_string(),
                                serde_json::json!(truncate(&text, 2000)),
                            );
                        }
                        row
                    })
                    .collect()
            } else {
                filtered
                    .iter()
                    .take(max_results as usize)
                    .map(|r| {
                        serde_json::json!({
                            "memory_id": r.memory_id,
                            "content": truncate(&r.content, 2000),
                            "rrf_score": r.rrf_score,
                            "source": r.source,
                            "evolved_at": r.evolved_at,
                            "pending_evolution": r.pending_evolution,
                        })
                    })
                    .collect()
            };
            serde_json::to_string(&serde_json::json!({
                "status": "ok",
                "total_results": filtered.len(),
                "results": results,
            }))
            .unwrap_or_default()
        }
        "memory_remember" | "memory" => {
            // P2-2 配额：写入限流（admin 豁免）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_WRITE, &_auth.role) {
                return err;
            }
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let cat = args
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("fact");
            let imp = args.get("importance").and_then(|v| v.as_i64()).unwrap_or(3);
            let src = args.get("source").and_then(|v| v.as_str()).unwrap_or("mcp");
            // tags: 支持 JSON 数组 ["a","b"] 或 JSON 字符串 "[\"a\",\"b\"]"
            let mut tags = if let Some(arr) = args.get("tags").and_then(|v| v.as_array()) {
                serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string())
            } else if let Some(s) = args.get("tags").and_then(|v| v.as_str()) {
                if s.is_empty() || s == "[]" {
                    "[]".to_string()
                } else {
                    s.to_string()
                }
            } else {
                "[]".to_string()
            };
            // P1-5: 可选时序真值区间
            let valid_from = args.get("valid_from").and_then(|v| v.as_str());
            let valid_to = args.get("valid_to").and_then(|v| v.as_str());
            // O2/O3：event_time 参数 deprecate → 映射为 occurred:YYYY-MM-DD tag，不写列
            if let Some(et) = args.get("event_time").and_then(|v| v.as_str()) {
                if !et.is_empty() {
                    eprintln!(
                        "[Memoria] WARN: event_time param deprecated (O2); mapping to occurred: tag"
                    );
                    if let Some(otag) = tools::ledger::occurred_tag_from_iso(et) {
                        tags = tools::ledger::merge_occurred_tag(&tags, &otag);
                    }
                }
            }
            // P0-4: 显式取代目标（取代指定旧记忆，带 404/403/409 失败模式）
            let supersedes_id = args.get("supersedes_id").and_then(|v| v.as_str());
            let relation = args.get("relation").and_then(|v| v.as_str());
            // PR1（Phase B 前置）：提取压缩元数据（由 agent-core 写入前门填充；Memoria 哑存储，不调 LLM）
            let actor = args.get("actor").and_then(|v| v.as_str());
            let memory_type = args.get("memory_type").and_then(|v| v.as_str());
            let parent_id = args.get("parent_id").and_then(|v| v.as_str());
            let raw_ref = args.get("raw_ref").and_then(|v| v.as_str());
            // P0: 带近义重复检测的 remember
            let body = match tools::remember::remember_with_dedup(
                &state.pool,
                content,
                cat,
                imp,
                src,
                ns,
                &tags,
                Some(&state.hnsw),
                Some(&state.query_cache),
                valid_from,
                valid_to,
                supersedes_id,
                relation,
                actor,
                memory_type,
                parent_id,
                raw_ref,
            ) {
                Ok(result) => {
                    if result.action == "superseded_near_dup" && !result.superseded_ids.is_empty() {
                        let pairs: Vec<String> = result
                            .superseded_ids
                            .iter()
                            .zip(result.similarities.iter())
                            .map(|(id, sim)| {
                                format!(
                                    "{{\"id\":\"{}\",\"similarity\":{}}}",
                                    id,
                                    (sim * 100.0).round() / 100.0
                                )
                            })
                            .collect();
                        format!(
                            r#"{{"status":"remembered","id":"{}","action":"{}","superseded":[{}]}}"#,
                            result.id,
                            result.action,
                            pairs.join(",")
                        )
                    } else {
                        format!(
                            r#"{{"status":"remembered","id":"{}","action":"{}"}}"#,
                            result.id, result.action
                        )
                    }
                }
                Err(e) => {
                    // P0-4：把 remember_with_dedup 返回的 "404:/403:/409:" 前缀映射为结构化错误码；
                    // 其余（db 写入失败等内部错误）归为 400。
                    let (code, msg) = match e.split_once(':') {
                        Some((prefix, rest)) => match prefix.trim().parse::<u16>() {
                            Ok(c @ (404 | 403 | 409)) => (c, rest.trim().to_string()),
                            _ => (400u16, e.clone()),
                        },
                        None => (400u16, e.clone()),
                    };
                    format!(r#"{{"status":"error","code":{},"message":"{}"}}"#, code, msg)
                }
            };
            body
        }
        "ingest_document" => {
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_WRITE, &_auth.role) {
                return err;
            }
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let filename = args
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("document.txt");
            if text.trim().is_empty() {
                return r#"{"status":"error","code":400,"message":"text required"}"#.to_string();
            }
            match memoria_core::document::ingest_plain_text(
                &state.pool,
                ns,
                filename,
                text,
                &_auth.agent_id,
            ) {
                Ok(out) => serde_json::to_string(&serde_json::json!({
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
                .unwrap_or_else(|_| {
                    r#"{"status":"error","message":"serialize"}"#.to_string()
                }),
                Err(e) => format!(
                    r#"{{"status":"error","code":422,"message":"{}"}}"#,
                    e.replace('"', "'")
                ),
            }
        }
        "memory_profile" => {
            // P0-3：profile_bucket 配额（每 ns ≤10/分钟，admin 豁免，见 §14.1 Q3）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_PROFILE, &_auth.role)
            {
                return err;
            }
            let static_limit = args
                .get("static_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(12) as usize;
            let dynamic_limit = args
                .get("dynamic_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(15) as usize;
            let as_of = args.get("as_of").and_then(|v| v.as_str());
            match tools::profile::memory_profile(
                &state.pool,
                ns,
                static_limit,
                dynamic_limit,
                as_of,
            ) {
                Ok(v) => serde_json::to_string(&v)
                    .unwrap_or_else(|_| "{\"status\":\"error\",\"message\":\"serialize\"}".to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_context" => {
            // P0-3：profile_bucket 配额（同上）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_PROFILE, &_auth.role)
            {
                return err;
            }
            let query = args.get("query").and_then(|v| v.as_str());
            let recall_k = args.get("recall_k").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
            let include_profile = args
                .get("include_profile")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let static_limit = args
                .get("static_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(12) as usize;
            let dynamic_limit = args
                .get("dynamic_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(15) as usize;
            let as_of = args.get("as_of").and_then(|v| v.as_str());
            match tools::profile::memory_context(
                &state.pool,
                Some(&state.hnsw),
                Some(&state.query_cache),
                ns,
                query,
                recall_k,
                include_profile,
                static_limit,
                dynamic_limit,
                as_of,
            ) {
                Ok(v) => serde_json::to_string(&v)
                    .unwrap_or_else(|_| "{\"status\":\"error\",\"message\":\"serialize\"}".to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_observe" => {
            // P2-2 配额：写入限流（admin 豁免）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_WRITE, &_auth.role) {
                return err;
            }
            let dialog = args.get("dialog").and_then(|v| v.as_str()).unwrap_or("");
            let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let src = args.get("source").and_then(|v| v.as_str()).unwrap_or("mcp");
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match tools::observe::observe(&state.pool, dialog, role, src, sid, ns) {
                Ok(id) => format!(r#"{{"status":"observed","id":"{}"}}"#, id),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_quota_status" => {
            // P2-2：返回本 ns 当前配额用量与上限（写入=日、搜索/profile=分钟、备份=小时）
            use memoria_core::quota::{KIND_BACKUP, KIND_PROFILE, KIND_SEARCH, KIND_WRITE};
            let kinds: [(&str, &str, &str); 4] = [
                ("write", KIND_WRITE, "MEMORIA_QUOTA_WRITES_PER_DAY"),
                ("search", KIND_SEARCH, "MEMORIA_QUOTA_SEARCHES_PER_MIN"),
                ("profile", KIND_PROFILE, "MEMORIA_QUOTA_PROFILES_PER_MIN"),
                ("backup", KIND_BACKUP, "MEMORIA_QUOTA_BACKUPS_PER_HOUR"),
            ];
            let mut quotas = serde_json::Map::new();
            for (label, k, env_key) in &kinds {
                let window = memoria_core::quota::quota_window(k);
                let limit: serde_json::Value = std::env::var(env_key)
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|n| serde_json::json!(n))
                    .unwrap_or_else(|| serde_json::json!(memoria_core::quota::quota_limit(k)));
                let used = memoria_core::quota::current_usage(&state.pool, ns, k);
                quotas.insert(
                    label.to_string(),
                    serde_json::json!({
                        "window": window,
                        "limit": limit,
                        "used": used,
                    }),
                );
            }
            serde_json::to_string(&serde_json::json!({
                "status": "ok",
                "namespace": ns,
                "quotas": quotas,
            }))
            .unwrap_or_default()
        }
        "memory_export" => {
            // P2-4：导出本 ns 记忆/实体为流式 JSONL（权限矩阵已按 NamespaceArg 校验 ns 归属）
            let include_vectors = args
                .get("include_vectors")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match memoria_core::tools::imp_exp::export_ns(&state.pool, ns, include_vectors) {
                Ok(jsonl) => serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "namespace": ns,
                    "bytes": jsonl.len(),
                    "export": jsonl,
                }))
                .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_import" => {
            // P2-4：从 JSONL 导入到本 ns（INSERT OR IGNORE 幂等；内部校验每行 ns 一致）
            let jsonl = args.get("jsonl").and_then(|v| v.as_str()).unwrap_or("");
            let on_conflict = match args
                .get("on_conflict")
                .and_then(|v| v.as_str())
                .unwrap_or("ignore")
            {
                "replace" => memoria_core::tools::imp_exp::OnConflict::Replace,
                _ => memoria_core::tools::imp_exp::OnConflict::Ignore,
            };
            if jsonl.is_empty() {
                return r#"{"status":"error","message":"jsonl 必填"}"#.to_string();
            }
            match memoria_core::tools::imp_exp::import_ns(&state.pool, ns, jsonl, on_conflict) {
                Ok(report) => serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "namespace": ns,
                    "inserted": report.inserted,
                    "ignored": report.ignored,
                    "errors": report.errors,
                }))
                .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_migration_manifest" => {
            // P2-4：迁移包清单（admin 专属，暴露全库行数 + DB/HNSW 校验和）
            if _auth.role != "admin" {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            match memoria_core::tools::imp_exp::build_migration_manifest(
                &state.pool,
                &state.db_path,
                &state.vec_index_path,
            ) {
                Ok(manifest) => {
                    serde_json::to_string(&serde_json::json!({"status":"ok","manifest":manifest}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "register_agent" => {
            let new_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            let display_name = args
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(new_id);
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            let default_ns = format!("agent/{}", new_id);
            let ns = args
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(&default_ns);
            match auth::register_agent(&state.auth_pool, new_id, display_name, &[ns], "user") {
                Ok(badge) => {
                    serde_json::to_string(&serde_json::json!({"status":"registered","badge":badge}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "register_user" => {
            let user_id = args.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let display_name = args
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(user_id);
            let password = args.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if user_id.is_empty() || password.is_empty() {
                return r#"{"status":"error","message":"user_id 与 password 必填"}"#.to_string();
            }
            // 口令强度最低要求：长度 >= 6（内部可信 LAN，仅作基本防线）
            if password.len() < 6 {
                return r#"{"status":"error","message":"口令至少 6 位"}"#.to_string();
            }
            let ns_override = args.get("namespace").and_then(|v| v.as_str());
            match auth::register_user(
                &state.auth_pool,
                user_id,
                display_name,
                password,
                ns_override,
            ) {
                Ok(badge) => serde_json::to_string(&serde_json::json!({
                    "status": "registered",
                    "agent_id": badge.agent_id,
                    "namespace": badge.namespace
                }))
                .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
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
                }))
                .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "import_install_memories" => {
            let from_ns = args.get("from_ns").and_then(|v| v.as_str()).unwrap_or("");
            let to_ns = args.get("to_ns").and_then(|v| v.as_str()).unwrap_or("");
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            if from_ns.is_empty() || to_ns.is_empty() || from_ns == to_ns {
                return r#"{"status":"error","message":"from_ns / to_ns 必填且不能相同"}"#
                    .to_string();
            }
            match state.pool.get() {
                Ok(conn) => {
                    // 迁移仅改 namespace 列：memory id = SHA256(content) 与 ns 无关，
                    // 全局唯一 PK；HNSW 按 id 索引、FTS 索引 content，均不含 ns，
                    // 故移动 ns 后向量/全文索引仍指向同一条记忆，无需重建。
                    let n1 = conn
                        .execute(
                            "UPDATE memories SET namespace=?1 WHERE namespace=?2",
                            rusqlite::params![to_ns, from_ns],
                        )
                        .unwrap_or(0);
                    let n2 = conn
                        .execute(
                            "UPDATE memory_relations SET namespace=?1 WHERE namespace=?2",
                            rusqlite::params![to_ns, from_ns],
                        )
                        .unwrap_or(0);
                    // user_prefs（B3 已加 namespace 列）一并迁移；若无该列则忽略。
                    let n3 = conn
                        .execute(
                            "UPDATE user_prefs SET namespace=?1 WHERE namespace=?2",
                            rusqlite::params![to_ns, from_ns],
                        )
                        .unwrap_or(0);
                    format!(
                        r#"{{"status":"ok","memories_moved":{},"relations_moved":{},"prefs_moved":{}}}"#,
                        n1, n2, n3
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"pool: {}"}}"#, e),
            }
        }
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
                        return serde_json::to_string(
                            &serde_json::json!({"status":"ok","logs":[]}),
                        )
                        .unwrap_or_default();
                    }
                    let union_sql: String = tables
                        .iter()
                        .map(|t| {
                            format!(
                                "SELECT agent_id, tool, params, allowed, timestamp FROM {}",
                                t
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" UNION ALL ");
                    let full_sql = format!(
                        "SELECT * FROM ({}) AS all_logs ORDER BY timestamp DESC LIMIT ?",
                        union_sql
                    );
                    let mut stmt = match conn.prepare(&full_sql) {
                        Ok(s) => s,
                        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
                    };
                    let rows = stmt.query_map(rusqlite::params![limit], |row| {
                        Ok(serde_json::json!({"agent_id":row.get::<_,String>(0)?,"tool":row.get::<_,String>(1)?,"params":row.get::<_,String>(2)?,"allowed":row.get::<_,i32>(3)?,"timestamp":row.get::<_,String>(4)?}))
                    });
                    let items: Vec<serde_json::Value> = match rows {
                        Ok(r) => r.flatten().collect(),
                        Err(_) => vec![],
                    };
                    serde_json::to_string(&serde_json::json!({"status":"ok","logs":items}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
        }
        "db_stats" => {
            // P0-1 修复：全库统计含 agent_registry / 审计总数 / HNSW 向量数，需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permissions::require_admin(&_auth, ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            let auth_conn = match state.auth_pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            let tables = [
                "memories",
                "messages",
                "sessions",
                "decisions",
                "user_prefs",
                "memory_relations",
                "decay_log",
                "dream_state",
            ];
            let auth_tables = ["agent_registry"];
            let mut m = serde_json::Map::new();
            for t in &tables {
                let c: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                    .unwrap_or(0);
                m.insert(t.to_string(), serde_json::Value::Number(c.into()));
            }
            for t in &auth_tables {
                let c: i64 = auth_conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                    .unwrap_or(0);
                m.insert(t.to_string(), serde_json::Value::Number(c.into()));
            }
            // 审计总行数（跨分区）
            let audit_count: i64 = auth_conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%'",
                )
                .and_then(|mut stmt| {
                    let tables: Vec<String> = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default();
                    let mut total: i64 = 0;
                    for t in tables {
                        if let Ok(c) =
                            auth_conn.query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| {
                                r.get::<_, i64>(0)
                            })
                        {
                            total += c;
                        }
                    }
                    Ok(total)
                })
                .unwrap_or(0);
            m.insert(
                "audit_log_total".to_string(),
                serde_json::Value::Number(audit_count.into()),
            );
            m.insert(
                "hnsw_vectors".to_string(),
                serde_json::Value::Number((state.hnsw.len() as i64).into()),
            );
            serde_json::to_string(&serde_json::json!({"status":"ok","stats":m})).unwrap_or_default()
        }
        "a2a_send" => {
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
            let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            // PFAiX 协作收件箱（A2A）：结构化信封支持。
            // 优先使用调用方直接传入的 content（JSON 字符串，审批流即如此）或 envelope（JSON 对象），
            // 兼容旧版 subject/body 文本格式。修复 latent bug：旧实现忽略 content 参数，
            // 导致审批流发送的 JSON 信封被丢弃为 "[ ] "。
            let raw_content = match args.get("content") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other)
                    .unwrap_or_else(|_| format!("[{}] {}", subject, body)),
                None => {
                    if let Some(env) = args.get("envelope") {
                        serde_json::to_string(env)
                            .unwrap_or_else(|_| format!("[{}] {}", subject, body))
                    } else {
                        format!("[{}] {}", subject, body)
                    }
                }
            };
            // 审计摘要：优先取信封里的 subject，否则用旧 subject
            let audit_subject = if let Some(serde_json::Value::String(s)) = args.get("content") {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| {
                        v.get("subject")
                            .and_then(|x| x.as_str())
                            .map(|x| x.to_string())
                    })
                    .unwrap_or_else(|| subject.to_string())
            } else {
                subject.to_string()
            };
            if to.is_empty() {
                return format!(r#"{{"status":"error","message":"missing 'to'"}}"#);
            }
            // P1-6：to 格式校验 — 仅允许字母数字连字符（agent-id 格式），
            // 拒绝含 ".." ".." "/" 等路径遍历/注入字符。
            if !to
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || to.len() > 64
            {
                return r#"{"status":"error","message":"invalid 'to' format: agent-id only, max 64 chars"}"#.to_string();
            }
            // P0-M1 修复：校验目标 Agent 的命名空间属于调用者授权范围，
            // 防止任意已认证 agent 向任意 namespace 注入消息（跨租户/跨项目消息投毒）。
            // 调用者自身（to == 自己）或 admin(*) 始终允许；其余必须落在 allowed_ns 内。
            let target_ns = format!("agent/{}", to);
            let self_msg = to == _auth.agent_id;
            if !self_msg && !auth::check_ns_access(_auth, &target_ns) {
                return format!(
                    r#"{{"status":"error","message":"无权向该 Agent 发送消息（超出命名空间授权范围）"}}"#
                );
            }
            match state.pool.get() {
                Ok(conn) => {
                    let _ = conn.execute(
                        "INSERT INTO memories (id, namespace, source, content, category, confidence, created_at, tier, importance)
                         VALUES (?, ?, ?, ?, 'a2a_message', 1.0, datetime('now'), 'hot', 2)",
                        rusqlite::params![format!("a2a_{}", uuid::Uuid::new_v4()), format!("agent/{}", to),
                                          format!("agent:{}", _auth.agent_id),
                                          raw_content.clone()],
                    );
                    // P1-6：a2a_send 审计（记录目标 agent + subject 摘要）
                    auth::audit_log(
                        &state.auth_pool,
                        &_auth.agent_id,
                        "a2a_send",
                        &format!("to={},subject={:.40}", to, audit_subject),
                        true,
                    );
                    format!(r#"{{"status":"sent","to":"{}"}}"#, to)
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "a2a_recv" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            match state.pool.get() {
                Ok(conn) => {
                    let result = (|| -> Result<String, String> {
                        let mut stmt = conn
                            .prepare(
                                "SELECT id, source, content, created_at FROM memories
                             WHERE namespace = ? AND category = 'a2a_message'
                             ORDER BY created_at DESC LIMIT ?",
                            )
                            .map_err(|e| format!("prepare: {}", e))?;
                        let rows: Vec<serde_json::Value> = stmt.query_map(
                            rusqlite::params![format!("agent/{}", _auth.agent_id), limit],
                            |row| Ok(serde_json::json!({"id":row.get::<_,String>(0)?,"from":row.get::<_,String>(1)?,"content":row.get::<_,String>(2)?,"time":row.get::<_,String>(3)?}))
                        ).map_err(|e| format!("query: {}", e))?.flatten().collect();
                        Ok(serde_json::to_string(
                            &serde_json::json!({"status":"ok","messages":rows}),
                        )
                        .unwrap_or_default())
                    })();
                    match result {
                        Ok(s) => s,
                        Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
                    }
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
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
                        Ok(
                            serde_json::to_string(
                                &serde_json::json!({"status":"ok","agents":rows}),
                            )
                            .unwrap_or_default(),
                        )
                    })();
                    match result {
                        Ok(s) => s,
                        Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
                    }
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "agent_revoke" => {
            let target = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() {
                return format!(r#"{{"status":"error","message":"missing agent_id"}}"#);
            }
            // 只有 admin 可以吊销 agent
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            // P0-1：统一 admin 门禁（角色优先 + body key 兜底）
            if !crate::permissions::require_admin(&_auth, admin_key, &state.admin_key) {
                return format!(r#"{{"status":"error","message":"admin required"}}"#);
            }
            match state.auth_pool.get() {
                Ok(conn) => {
                    let n = conn
                        .execute(
                            "DELETE FROM agent_registry WHERE agent_id = ?",
                            rusqlite::params![target],
                        )
                        .unwrap_or(0);
                    format!(
                        r#"{{"status":"revoked","agent_id":"{}","deleted":{}}}"#,
                        target, n
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "skill_market_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let category = args.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as u32;
            let caller_ns = _auth
                .allowed_ns
                .first()
                .map(|s| s.as_str())
                .unwrap_or("default");
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
                    serde_json::to_string(&serde_json::json!({"status":"ok","results":rows}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "skill_market_info" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                return r#"{"status":"error","message":"missing name"}"#.to_string();
            }
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
                        Ok(skill) => {
                            serde_json::to_string(&serde_json::json!({"status":"ok","skill":skill}))
                                .unwrap_or_default()
                        }
                        Err(_) => format!(
                            r#"{{"status":"error","message":"skill '{}' not found"}}"#,
                            name
                        ),
                    }
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "skill_market_publish" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                return r#"{"status":"error","message":"missing name"}"#.to_string();
            }
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            let visibility = args
                .get("visibility")
                .and_then(|v| v.as_str())
                .unwrap_or("public");
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let version = args
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("1.0.0");
            let author = args.get("author").and_then(|v| v.as_str()).unwrap_or("");
            let category = args
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("general");
            let steps = args
                .get("steps")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".to_string());
            let dependencies = args
                .get("dependencies")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".to_string());
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("manual");

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
                    format!(
                        r#"{{"status":"published","name":"{}","visibility":"{}"}}"#,
                        name, visibility
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            };
            // P1-6：publish 审计（记录 name + visibility）
            if publish_result.contains("\"status\":\"published\"") {
                auth::audit_log(
                    &state.auth_pool,
                    &_auth.agent_id,
                    "skill_market_publish",
                    &format!("name={},visibility={}", name, visibility),
                    true,
                );
            }
            return publish_result;
        }
        "skill_market_install" => {
            let skill_name = args
                .get("skill_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let target_agent = args
                .get("target_agent")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let admin_key = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if skill_name.is_empty() || target_agent.is_empty() {
                return r#"{"status":"error","message":"missing skill_name or target_agent"}"#
                    .to_string();
            }

            // 权限检查：admin_key 或 同 namespace
            if !auth::ct_eq(admin_key, &state.admin_key) {
                // 非 admin：只能给同 namespace 的 Agent 安装（使用精确角色判定，非子串）
                if _auth.role != "admin"
                    && !auth::check_ns_access(_auth, &format!("agent/{}", target_agent))
                {
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
                    conn.execute(
                        "UPDATE skill_catalog SET install_count = install_count + 1 WHERE name = ?",
                        rusqlite::params![skill_name],
                    )
                    .ok();
                    format!(
                        r#"{{"status":"installed","skill":"{}","target_agent":"{}"}}"#,
                        skill_name, target_agent
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            };
            // P1-6：install 审计（记录 skill_name + target_agent）
            if install_result.contains("\"status\":\"installed\"") {
                auth::audit_log(
                    &state.auth_pool,
                    &_auth.agent_id,
                    "skill_market_install",
                    &format!("skill={},target={}", skill_name, target_agent),
                    true,
                );
            }
            return install_result;
        }
        "skill_market_list_installed" => {
            let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            if agent_id.is_empty() {
                return r#"{"status":"error","message":"missing agent_id"}"#.to_string();
            }
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
                         WHERE agent_id = ? AND is_active = 1 ORDER BY installed_at DESC",
                    ) {
                        if let Ok(iter) = stmt.query_map(rusqlite::params![agent_id], |row| {
                            Ok(serde_json::json!({"skill_name": row.get::<_,String>(0)?,
                                "installed_at": row.get::<_,String>(1)?,
                                "installed_by": row.get::<_,String>(2)?}))
                        }) {
                            for r in iter.flatten() {
                                rows.push(r);
                            }
                        }
                    }
                    serde_json::to_string(&serde_json::json!({"status":"ok","skills":rows}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        // ── Phase 3 工具 ──
        "memory_decay" => match tools::decay::run_decay(&state.pool, ns) {
            Ok((processed, cold)) => format!(
                r#"{{"status":"ok","processed":{},"cold":{}}}"#,
                processed, cold
            ),
            Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
        },
        "memory_graph" => match tools::graph::build_graph(&state.pool, ns, 50) {
            Ok((nodes, edges)) => {
                format!(r#"{{"status":"ok","nodes":{},"edges":{}}}"#, nodes, edges)
            }
            Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
        },
        "memory_user_prefs" => {
            // 可选 tag 过滤（hard_rule|pref|style），默认返回全部偏好
            let tag_filter = args
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            match tools::prefs::user_prefs(&state.pool, &ns) {
                Ok(prefs) => {
                    let items: Vec<serde_json::Value> = prefs
                        .into_iter()
                        .filter(|p| tag_filter.as_ref().map_or(true, |t| &p.tag == t))
                        .map(|p| {
                            serde_json::json!({
                                "key": p.key,
                                "value": p.value,
                                "importance": p.importance,
                                "tag": p.tag,
                                "confidence": p.confidence,
                                "created_at": p.created_at,
                            })
                        })
                        .collect();
                    serde_json::to_string(&serde_json::json!({"status":"ok","prefs":items}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_recent_decisions" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            match tools::prefs::recent_decisions(&state.pool, ns, limit) {
                Ok(decisions) => {
                    let items: Vec<serde_json::Value> = decisions.into_iter().map(|(id, content, ts)| {
                        serde_json::json!({"id": id, "content": content, "time": ts})
                    }).collect();
                    serde_json::to_string(&serde_json::json!({"status":"ok","decisions":items}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        // ── P0 工具：备份 / 健康 / 去重 ──
        "memory_backup" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            // P2-2 备份配额（对所有人生效，含 admin；限制备份频率、防备份风暴）
            if let Some(err) = quota_gate(state, ns, memoria_core::quota::KIND_BACKUP, &_auth.role)
            {
                return err;
            }
            // 手动触发备份
            match memoria_core::backup::perform_backup(
                &state.pool,
                &state.db_path,
                &state.backup_dir,
                Some(&state.vec_index_path),
            ) {
                Ok(r) => format!(
                    r#"{{"status":"ok","backup_path":"{}","size_mb":{},"integrity_ok":{},"rotation_deleted":{},"tier":"{}"}}"#,
                    r.backup_path,
                    r.db_size_bytes / 1048576,
                    r.integrity_ok,
                    r.rotation_deleted,
                    r.tier
                ),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_backup_list" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            match memoria_core::backup::list_backups(&state.backup_dir) {
                Ok(v) => serde_json::to_string(&serde_json::json!({"status":"ok","backups":v}))
                    .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_health" => {
            // P2-5 修复：备份类操作需 admin 门禁
            let ak = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if _auth.role != "admin" && !auth::ct_eq(ak, &state.admin_key) {
                return r#"{"status":"error","message":"admin key required"}"#.to_string();
            }
            let report = memoria_core::health::run_health_check(
                &state.pool,
                &state.auth_pool,
                &state.hnsw,
                &state.db_path,
                &state.hnsw_status,
                &state.embedding_url,
            );
            serde_json::to_string(&serde_json::json!({
                "status":"ok",
                "embed": report.soft_checks.iter().find(|c| c.name == "embedding"),
                "report": report
            }))
                .unwrap_or_default()
        }
        "memory_dedup_chain" => {
            let memory_id = args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
            if memory_id.is_empty() {
                return r#"{"status":"error","message":"missing memory_id"}"#.to_string();
            }
            // P1-2 修复：校验该记忆所属 NS 对调用者可见（防跨 NS 读取 superseded 链 / IDOR）
            let mem_ns: String = match state.pool.get() {
                Ok(conn) => conn
                    .query_row(
                        "SELECT namespace FROM memories WHERE id = ?1",
                        rusqlite::params![memory_id],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap_or_default(),
                Err(_) => return r#"{"status":"error","message":"db error"}"#.to_string(),
            };
            if !auth::check_ns_access(_auth, &mem_ns) {
                return r#"{"status":"error","message":"namespace not authorized"}"#.to_string();
            }
            match tools::remember::get_supersession_chain(&state.pool, memory_id) {
                Ok(chain) => {
                    serde_json::to_string(&serde_json::json!({"status":"ok","superseded":chain}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
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
                Ok(()) => format!(
                    r#"{{"status":"merged","keep":"{}","merged":"{}"}}"#,
                    keep_id, merge_id
                ),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_evolve" => {
            // 演化写回：ns 门控（与写入同权），不强制 admin（agent-core consolidate 用 dashboard badge 调）
            let target_id = args.get("target_id").and_then(|v| v.as_str()).unwrap_or("");
            let ev_ns = args
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(ns);
            let evolved_context = args
                .get("evolved_context")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if target_id.is_empty() || evolved_context.is_empty() {
                return r#"{"status":"error","message":"missing target_id or evolved_context"}"#.to_string();
            }
            let link_count = args.get("link_count").and_then(|v| v.as_i64());
            let model = args.get("model").and_then(|v| v.as_str()).unwrap_or("");
            let change_type = args
                .get("change_type")
                .and_then(|v| v.as_str())
                .unwrap_or("context_update");
            if !auth::check_ns_access(&_auth, ev_ns) {
                return r#"{"status":"error","message":"ns access denied"}"#.to_string();
            }
            match tools::evolve::evolve_memory(
                &state.pool,
                target_id,
                ev_ns,
                evolved_context,
                link_count,
                model,
                change_type,
            ) {
                Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| r#"{"status":"evolved"}"#.to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "evolution_rollback" => {
            let admin_key_val = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permissions::require_admin(&_auth, admin_key_val, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            let log_id = args.get("log_id").and_then(|v| v.as_str()).unwrap_or("");
            if log_id.is_empty() {
                return r#"{"status":"error","message":"missing log_id"}"#.to_string();
            }
            match tools::evolve::evolution_rollback(&state.pool, log_id) {
                Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| r#"{"status":"rolled_back"}"#.to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "evolution_log_query" => {
            // PR5（P-A 元进化）：只读采样 evolution_log 负样本，供 agent-core 元进化闭环使用。
            let change_types: Vec<String> = args
                .get("change_types")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            let since = args
                .get("since")
                .and_then(|v| v.as_str())
                .unwrap_or("1970-01-01T00:00:00");
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(500);
            match tools::evolve::evolution_log_query(&state.pool, &change_types, since, limit, ns) {
                Ok(v) => serde_json::to_string(&v)
                    .unwrap_or_else(|_| r#"{"status":"ok","count":0,"items":[]}"#.to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_evolve_auto" => {
            // G2（HY3 硬门）：自包含自动演化触发。ns 门控（与写入同权）。
            let auto_ns = args.get("namespace").and_then(|v| v.as_str()).unwrap_or(ns);
            let auto_limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            if !auth::check_ns_access(&_auth, auto_ns) {
                return r#"{"status":"error","message":"ns access denied"}"#.to_string();
            }
            match tools::evolve::evolve_memory_auto(&state.pool, auto_ns, auto_limit) {
                Ok(v) => serde_json::to_string(&v)
                    .unwrap_or_else(|_| r#"{"status":"auto_evolved"}"#.to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "agent_registry_cleanup" => {
            // G4（HY3 硬门）：保守幂等清理 agent_registry 死行。需 Admin key。
            let admin_key_val = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permissions::require_admin(&_auth, admin_key_val, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            match auth::cleanup_agent_registry(&state.auth_pool) {
                Ok((removed, ids)) => serde_json::to_string(&serde_json::json!({
                    "status": "cleaned", "removed": removed, "removed_ids": ids
                }))
                .unwrap_or_else(|_| r#"{"status":"cleaned"}"#.to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "memory_maintenance_normalize" => {
            // Q1（§14.1 Q1）：admin 专属，破坏性操作，调用方须先 memory_backup。
            let admin_key_val = args.get("admin_key").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permissions::require_admin(&_auth, admin_key_val, &state.admin_key) {
                return r#"{"status":"error","message":"admin required"}"#.to_string();
            }
            match tools::imp_exp::normalize_valid_to(&state.pool) {
                Ok(rep) => serde_json::to_string(&serde_json::json!({
                    "status": "normalized",
                    "updated": rep.updated,
                    "samples": rep.samples,
                }))
                .unwrap_or_else(|_| "{\"status\":\"error\",\"message\":\"serialize\"}".to_string()),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        // ── 暗知识层 A1：夜间巩固哑工具（ns 门控已在 handle_tool_call 完成）──
        "memory_fetch_unconsolidated" => {
            let since = args
                .get("since")
                .and_then(|v| v.as_str())
                .unwrap_or("1970-01-01");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as i64;
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            let mut stmt = match conn.prepare(
                "SELECT id, content, category, created_at FROM memories
                 WHERE namespace = ?1 AND created_at > ?2
                   AND (category = 'observation' OR category IS NULL)
                 ORDER BY created_at ASC LIMIT ?3",
            ) {
                Ok(s) => s,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
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
            serde_json::to_string(
                &serde_json::json!({"status":"ok","count":items.len(),"items":items}),
            )
            .unwrap_or_default()
        }
        "dream_state_get" => {
            let phase = args
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("consolidate");
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            let row = conn.query_row(
                "SELECT last_run, cursor_ts, runs, items_out FROM dream_state
                 WHERE phase = ?1 AND namespace = ?2",
                rusqlite::params![phase, ns],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
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
        }
        "dream_state_update" => {
            let phase = args
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("consolidate");
            let cursor_ts = args.get("cursor_ts").and_then(|v| v.as_str()).unwrap_or("");
            let items_out = args.get("items_out").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
            let sessions = args
                .get("sessions_processed")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as i64;

            // P1-4 一：cursor_ts 非空校验（防止游标回退到 epoch）
            if cursor_ts.is_empty() {
                return r#"{"status":"error","message":"cursor_ts must be non-empty ISO-8601"}"#
                    .to_string();
            }

            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
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
                    return format!(
                        r#"{{"status":"error","message":"cursor_ts must advance: new '{}' not newer than previous '{}'"}}"#,
                        cursor_ts, prev
                    );
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
                return format!(
                    r#"{{"status":"error","message":"rate limited: phase '{}' for ns '{}' requires {}s cooldown"}}"#,
                    phase, ns, cooldown_secs
                );
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
        }
        // ── 知识图谱 B：实体工具 ──
        "entity_upsert" => {
            let default_id = uuid::Uuid::new_v4().to_string();
            let entity_id = args
                .get("entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&default_id)
                .to_string();
            let entity_type = args
                .get("entity_type")
                .and_then(|v| v.as_str())
                .unwrap_or("other");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let aliases = args.get("aliases").and_then(|v| v.as_str()).unwrap_or("[]");
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            match conn.execute(
                "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   name=excluded.name, aliases=excluded.aliases, summary=excluded.summary",
                rusqlite::params![entity_id, ns, entity_type, name, aliases, summary],
            ) {
                Ok(_) => {
                    serde_json::to_string(&serde_json::json!({"status":"ok","entity_id":entity_id}))
                        .unwrap_or_default()
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "entity_add_mention" => {
            let entity_id = args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
            let memory_id = args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
            let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
            };
            match conn.execute(
                "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![entity_id, memory_id, context, ns],
            ) {
                Ok(_) => serde_json::to_string(&serde_json::json!({"status":"ok"})).unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "entity_add_edge" => {
            let source = args
                .get("source_entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let target = args
                .get("target_entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let rtype = args
                .get("relation_type")
                .and_then(|v| v.as_str())
                .unwrap_or("related_to");
            // P2-3：关系类型受控枚举，拒绝未知类型（防止关系爆炸/垃圾关系污染图谱）
            if !memoria_core::tools::graph::is_valid_relation_type(rtype) {
                return format!(
                    r#"{{"status":"error","message":"invalid relation_type '{}'. allowed: {}"}}"#,
                    rtype,
                    memoria_core::tools::graph::relation_type_list()
                );
            }
            let weight = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let evidence = args.get("evidence").and_then(|v| v.as_str()).unwrap_or("");
            // P1-5: 可选时序真值区间
            let valid_from = args.get("valid_from").and_then(|v| v.as_str());
            let valid_to = args.get("valid_to").and_then(|v| v.as_str());
            let conn = match state.pool.get() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"pool: {}"}}"#, e),
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
        }
        "entity_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let ent_type = args.get("entity_type").and_then(|v| v.as_str());
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as i64;
            // P2-3：核心搜索逻辑下放到 tools::graph::search_entities（可测；含 mention context 搜索 + mentions_count）
            match memoria_core::tools::graph::search_entities(
                &state.pool,
                ns,
                query,
                ent_type,
                max_results,
            ) {
                Ok(rows) => serde_json::to_string(
                    &serde_json::json!({"status":"ok","count":rows.len(),"entities":rows}),
                )
                .unwrap_or_default(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        _ => format!(r#"{{"error":"Unknown tool: {}"}}"#, tool),
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
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

    match state
        .http_client
        .post(&state.bridge_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(val) => val
                .get("result")
                .and_then(|r| serde_json::to_string(r).ok())
                .unwrap_or_else(|| r#"{"error":"empty bridge response"}"#.to_string()),
            Err(e) => format!(r#"{{"error":"bridge parse: {}"}}"#, e),
        },
        Err(e) => format!(
            r#"{{"error":"bridge unreachable ({}): {}"}}"#,
            state.bridge_url, e
        ),
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
        Err(_) => return false, // 无标签记录→不匹配（P2-4 安全加固）
    };
    // tags 存为 JSON 数组 ["a","b"]，检查每个请求标签是否在其中
    tags.iter()
        .all(|tag| tags_str.contains(&format!("\"{}\"", tag)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_state() -> Arc<AppState> {
        // 每个测试用独立的共享内存库名，避免 :memory: 进程级共享导致的配额/数据串扰
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let db_path = format!(
            "file:memoria_mcp_{}_{}?mode=memory&cache=shared",
            std::process::id(),
            seq
        );
        let pool = memoria_core::storage::create_pool(&db_path, 4).expect("pool");
        memoria_core::storage::init_schema(&pool).expect("schema");
        memoria_core::storage::init_core_tables(&pool).expect("core");
        memoria_core::storage::migrate_superseded_by(&pool).expect("migrate superseded_by");
        memoria_core::storage::migrate_event_time(&pool).expect("migrate event_time");
        memoria_core::storage::migrate_temporal(&pool).expect("migrate temporal");
        memoria_core::storage::migrate_extract_fields(&pool).expect("migrate extract fields");
        memoria_core::storage::migrate_evolution(&pool).expect("migrate evolution");
        memoria_core::storage::migrate_memory_relation_types(&pool).expect("migrate relation types");
        memoria_core::quota::init_quota_table(&pool).expect("quota table");
        let auth_pool = memoria_core::storage::create_pool(":memory:", 4).expect("auth pool");
        memoria_core::storage::init_schema(&auth_pool).expect("auth schema");
        memoria_core::auth::init_auth_tables(&auth_pool).expect("auth tables");

        // P2-12：测试夹具也需要审计通道；丢弃 receiver（测试不校验审计落库）
        let (audit_tx, _audit_rx) = tokio::sync::mpsc::channel(1024);

        Arc::new(AppState {
            pool,
            auth_pool,
            hnsw: Arc::new(memoria_core::vector::HnswIndex::new()),
            hnsw_status: "uninitialized".to_string(),
            query_cache: Arc::new(memoria_core::vector::QueryCache::new()),
            admin_key: "test-admin-key".to_string(),
            bridge_url: "http://127.0.0.1:9000/mcp".to_string(),
            embedding_url: String::new(),
            http_client: reqwest::Client::new(),
            db_path: ":memory:".to_string(),
            backup_dir: ".".to_string(),
            vec_index_path: ":memory:".to_string(),
            audit_tx,
        })
    }

    // ── P3-0: embed_query 解析 + 优雅降级验证 ──
    #[tokio::test]
    async fn test_embed_query_parses_mock_response() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);
        let h = std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).unwrap(); // 读掉请求
                let body = r#"{"embeddings":[[0.1,0.2,0.3]],"dim":3,"model":"x"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let client = reqwest::Client::new();
        let v = super::embed_query(&client, &url, "hello").await;
        h.join().unwrap();
        assert_eq!(v, Some(vec![0.1f32, 0.2, 0.3]), "应解析出 embeddings[0]");
    }

    #[tokio::test]
    async fn test_embed_query_handles_500_gracefully() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);
        let h = std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut s = stream.unwrap();
                let _ = s.read(&mut [0u8; 4096]).unwrap();
                let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let client = reqwest::Client::new();
        let v = super::embed_query(&client, &url, "hello").await;
        h.join().unwrap();
        assert_eq!(v, None, "500 应优雅降级为 None，而非报错");
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
            headers.insert(
                "x-admin-key",
                axum::http::HeaderValue::from_static("test-admin-key"),
            );
            let res = health_check_full(axum::extract::State(state), headers).await;
            assert!(res.is_ok(), "valid admin key must allow /health/full");
        });
    }

    // ── P0-3：profile_bucket 配额（每 ns ≤10/分钟，admin 豁免，见 §14.1 Q3）──
    #[test]
    fn test_profile_bucket_limits_agent_then_exempts_admin() {
        let state = build_test_state();
        memoria_core::quota::init_quota_table(&state.pool).expect("quota table");
        let agent_auth = memoria_core::auth::AuthResult {
            agent_id: "agent/x".to_string(),
            allowed_ns: vec!["agent/x".to_string()],
            role: "read_write".to_string(),
        };
        let ns = serde_json::json!({ "namespace": "agent/x" });

        // 前 10 次 agent 调用放行，第 11 次被 profile_bucket 限流
        let mut allowed = 0;
        let mut denied = 0;
        for _ in 0..11 {
            let args = ns.as_object().unwrap().clone();
            let text = dispatch(&state, "memory_profile", &args, &agent_auth);
            if text.contains(r#""status":"ok""#) {
                allowed += 1;
            } else if text.contains("quota_exceeded") {
                denied += 1;
            } else {
                panic!("unexpected profile response: {}", text);
            }
        }
        assert_eq!(allowed, 10, "agent 前 10 次 profile 应放行");
        assert_eq!(denied, 1, "第 11 次 profile 应被 profile_bucket 限流");

        // admin 角色豁免：再调 5 次均放行
        let admin_auth = memoria_core::auth::AuthResult {
            agent_id: "admin".to_string(),
            allowed_ns: vec!["*".to_string()],
            role: "admin".to_string(),
        };
        for _ in 0..5 {
            let args = ns.as_object().unwrap().clone();
            let text = dispatch(&state, "memory_profile", &args, &admin_auth);
            assert!(
                text.contains(r#""status":"ok""#),
                "admin 应豁免 profile_bucket，但得到: {}",
                text
            );
        }
    }

    #[test]
    fn test_memory_context_returns_prompt_block() {
        let state = build_test_state();
        let auth = memoria_core::auth::AuthResult {
            agent_id: "agent/x".to_string(),
            allowed_ns: vec!["agent/x".to_string()],
            role: "read_write".to_string(),
        };
        let args = serde_json::json!({
            "namespace": "agent/x",
            "query": "测试回忆",
            "include_profile": true
        })
        .as_object()
        .unwrap()
        .clone();
        let text = dispatch(&state, "memory_context", &args, &auth);
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(v["status"], "ok", "memory_context response: {}", text);
        assert!(v["prompt_block"].is_string(), "memory_context 应产出 prompt_block");
        assert!(v["profile"].is_object(), "memory_context 应含 profile 块");
    }
}
