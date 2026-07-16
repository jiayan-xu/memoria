//! 权限矩阵 + 统一门禁（P0-1）
//!
//! 设计目标（见 OPTIMIZATION_PLAN_2026-07-12.md §P0-1）：
//! 1. 单一真源记录「每个 MCP 入口 × 最低身份 × NS 策略 × 备注」，
//!    新增 handler 时必须在 `PERMISSION_MATRIX` 登记，否则 `coverage` 测试失败。
//! 2. 统一 `require_admin`：优先 `auth.role == "admin"`（admin 智能体的 X-Agent-Key），
//!    兼容请求体明文 `admin_key` 作为**弃用兜底**（打 WARN，过渡期不破坏旧客户端）。
//! 3. 越权缺口（`db_stats` 全库统计、`skill_market_list_installed` 跨 agent 查询）
//!    在 dispatch 侧加门禁/NS 校验，放行路径不变。

use crate::auth::{self, AuthResult};

/// 最低身份要求
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinRole {
    /// 任意已认证智能体
    Agent,
    /// 仅 `role == "admin"`（或弃用 body key 兜底）
    Admin,
}

/// NS 隔离策略（顶层 `handle_tool_call` 已对所有带 `namespace` 参数的入口做统一门控，
/// 此处仅描述「派生 NS 检查」的语义，便于审计与回归断言）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsPolicy {
    /// 顶层已按调用方 `namespace` 参数门控
    NamespaceArg,
    /// 目标命名空间（如 a2a_send 的 `to`）
    TargetNs,
    /// 记忆所属命名空间（如 memory_dedup_chain 按 memory_id 反查）
    MemoryIdNs,
    /// 目标 agent 命名空间（`agent/{id}`）
    AgentIdNs,
    /// 仅调用者自身收件箱/身份
    SelfOnly,
    /// 无命名空间概念
    None,
}

/// 权限矩阵条目
#[allow(dead_code)] // min_role/ns_policy/note 为矩阵文档与未来运行时校验用，当前由测试守护覆盖
pub struct Entry {
    pub tool: &'static str,
    pub min_role: MinRole,
    pub ns_policy: NsPolicy,
    pub note: &'static str,
}

/// 全量权限矩阵（P0-1 单一真源）。与 `mcp_server::tools_list()` 双向覆盖有测试守护。
pub const PERMISSION_MATRIX: &[Entry] = &[
    // ── 普通记忆读写（顶层 namespace 门控）──
    Entry {
        tool: "memory_search",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "混合检索",
    },
    Entry {
        tool: "memory_search_v2",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "多信号融合检索",
    },
    Entry {
        tool: "memory_remember",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "写入记忆",
    },
    Entry {
        tool: "memory",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "memory_remember 薄别名",
    },
    Entry {
        tool: "memory_profile",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "会话开场静态/动态合成视图（P0-3）",
    },
    Entry {
        tool: "memory_context",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "profile + recall 合成 prompt_block（P0-3）",
    },
    Entry {
        tool: "memory_recall",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "回忆检索别名（走 search 配额）",
    },
    Entry {
        tool: "memory_observe",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "低优先级观察",
    },
    Entry {
        tool: "memory_decay",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "衰减循环",
    },
    Entry {
        tool: "memory_graph",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "构建关系图",
    },
    Entry {
        tool: "memory_user_prefs",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "用户偏好",
    },
    Entry {
        tool: "memory_quota_status",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "配额用量查询（P2-2）",
    },
    Entry {
        tool: "memory_recent_decisions",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "最近决策",
    },
    Entry {
        tool: "memory_fetch_unconsolidated",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "巩固原料读取",
    },
    Entry {
        tool: "dream_state_get",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "巩固进度读取",
    },
    Entry {
        tool: "dream_state_update",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "巩固进度推进",
    },
    Entry {
        tool: "entity_upsert",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "实体创建/更新",
    },
    Entry {
        tool: "entity_add_mention",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "实体提及",
    },
    Entry {
        tool: "entity_add_edge",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "实体关系边",
    },
    Entry {
        tool: "entity_search",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "实体搜索",
    },
    Entry {
        tool: "memory_export",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "导出本 ns 记忆/实体",
    },
    Entry {
        tool: "memory_import",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::NamespaceArg,
        note: "导入到本 ns（幂等）",
    },
    Entry {
        tool: "memory_migration_manifest",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "迁移包校验和（admin）",
    },
    // ── Admin 专属管理操作 ──
    Entry {
        tool: "register_agent",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::NamespaceArg,
        note: "创建 Agent（admin）",
    },
    Entry {
        tool: "import_install_memories",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "命名空间迁移（admin）",
    },
    Entry {
        tool: "db_stats",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "全库统计含 agent_registry/审计/HNSW（admin，新增门禁）",
    },
    Entry {
        tool: "agent_list",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "列出 Agent（admin）",
    },
    Entry {
        tool: "agent_revoke",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "吊销 Agent（admin）",
    },
    Entry {
        tool: "memory_backup",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "手动备份（admin）",
    },
    Entry {
        tool: "memory_backup_list",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "列出备份（admin）",
    },
    Entry {
        tool: "memory_health",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "健康报告（admin）",
    },
    Entry {
        tool: "memory_merge",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "合并记忆（admin）",
    },
    Entry {
        tool: "memory_maintenance_normalize",
        min_role: MinRole::Admin,
        ns_policy: NsPolicy::None,
        note: "Q1 归一时间格式/清洗哨兵（admin，须先备份）",
    },
    // ── 无命名空间的身份/自省工具 ──
    Entry {
        tool: "register_user",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "本地账密注册",
    },
    Entry {
        tool: "login_user",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "本地账密登录",
    },
    Entry {
        tool: "get_allowed_ns",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "返回自身授权 ns",
    },
    Entry {
        tool: "audit_query",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "审计日志查询（自身范围）",
    },
    // ── Bridge 转发工具（无本地 ns 概念）──
    Entry {
        tool: "cross_agent_query",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    Entry {
        tool: "system_status",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    Entry {
        tool: "panel_discuss",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    Entry {
        tool: "reasonix_dispatch",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    Entry {
        tool: "continue_task",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    Entry {
        tool: "auto_route",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "Bridge 转发",
    },
    // ── 技能市场 ──
    Entry {
        tool: "skill_market_search",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "公开/同 ns 技能搜索",
    },
    Entry {
        tool: "skill_market_info",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "技能详情（公开）",
    },
    Entry {
        tool: "skill_market_publish",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::None,
        note: "admin 或同 visibility ns 可发布",
    },
    Entry {
        tool: "skill_market_install",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::AgentIdNs,
        note: "admin 或同 ns 管理权可安装",
    },
    Entry {
        tool: "skill_market_list_installed",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::AgentIdNs,
        note: "仅自身/授权 ns（新增 NS 校验）",
    },
    // ── A2A 消息 ──
    Entry {
        tool: "a2a_send",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::TargetNs,
        note: "目标 agent ns 须授权",
    },
    Entry {
        tool: "a2a_recv",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::SelfOnly,
        note: "仅自身收件箱",
    },
    // ── 派生 NS 检查 ──
    Entry {
        tool: "memory_dedup_chain",
        min_role: MinRole::Agent,
        ns_policy: NsPolicy::MemoryIdNs,
        note: "记忆所属 ns 须授权",
    },
];

/// 在矩阵中查找某工具的权限条目
pub fn matrix_lookup(tool: &str) -> Option<&'static Entry> {
    PERMISSION_MATRIX.iter().find(|e| e.tool == tool)
}

/// 统一 admin 门禁（P0-1 收口）
///
/// 放行条件（满足任一）：
/// - 调用者 `role == "admin"`（推荐：admin 智能体的 X-Agent-Key）；
/// - 提供正确的弃用 `admin_key` 且与 `configured_key` 恒定时间相等。
///
/// 注意：`configured_key` 为空时（未设 MEMORIA_ADMIN_KEY）不会因 `ct_eq("", "")` 误放行，
/// 因为要求 `provided` 非空且相等；admin 角色仍可用。
pub fn require_admin(auth: &AuthResult, provided: &str, configured_key: &str) -> bool {
    if auth.role == "admin" {
        return true;
    }
    if configured_key.is_empty() {
        return false;
    }
    if !provided.is_empty() && auth::ct_eq(provided, configured_key) {
        tracing::warn!(
            "admin action authorized via DEPRECATED body admin_key (agent={}); migrate to admin agent X-Agent-Key",
            auth.agent_id
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 每个 `tools_list` 中的工具都必须在权限矩阵登记（新增 handler 的强制断言）
    #[test]
    fn matrix_covers_all_registered_tools() {
        for t in crate::mcp_server::tools_list() {
            let name = t["name"].as_str().expect("tool entry must have name");
            assert!(
                matrix_lookup(name).is_some(),
                "tool '{}' is missing from PERMISSION_MATRIX — register it in permissions.rs",
                name
            );
        }
    }

    /// 矩阵条目不能有孤儿（拼写错/已删除工具残留）
    #[test]
    fn no_orphan_matrix_entries() {
        let names: HashSet<String> = crate::mcp_server::tools_list()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for e in PERMISSION_MATRIX {
            assert!(
                names.contains(e.tool),
                "matrix entry '{}' is not in tools_list (orphan?)",
                e.tool
            );
        }
    }

    /// require_admin：角色优先，key 兜底，空配置不误放行
    #[test]
    fn require_admin_role_and_key() {
        let admin = AuthResult {
            agent_id: "admin".into(),
            allowed_ns: vec!["*".into()],
            role: "admin".into(),
        };
        let user = AuthResult {
            agent_id: "u".into(),
            allowed_ns: vec!["agent/u".into()],
            role: "user".into(),
        };

        assert!(
            require_admin(&admin, "", "cfgkey"),
            "admin role must pass without key"
        );
        assert!(
            !require_admin(&user, "", "cfgkey"),
            "plain user without key must be rejected"
        );
        assert!(
            require_admin(&user, "cfgkey", "cfgkey"),
            "correct body key must still work (deprecated path)"
        );
        assert!(
            !require_admin(&user, "wrong", "cfgkey"),
            "wrong key must be rejected"
        );
        // configured_key 为空：即使 provided 也为空，不能 ct_eq("", "") 误放行
        assert!(
            !require_admin(&user, "", ""),
            "empty configured key must not authorize anyone"
        );
        // configured_key 为空但 provided 非空：不匹配
        assert!(
            !require_admin(&user, "x", ""),
            "empty configured key + provided must not authorize"
        );
    }

    /// 附录 A 验收：持 default token 的 read_write agent 不能读 admin 命名空间
    #[test]
    fn default_agent_cannot_access_admin_ns() {
        let user = AuthResult {
            agent_id: "default".into(),
            allowed_ns: vec!["default".into()],
            role: "read_write".into(),
        };
        assert!(!auth::check_ns_access(&user, "admin"));
        assert!(auth::check_ns_access(&user, "default"));
    }

    /// admin 角色放行任意 ns（矩阵语义一致性）
    #[test]
    fn admin_role_bypasses_ns() {
        let admin = AuthResult {
            agent_id: "admin".into(),
            allowed_ns: vec!["*".into()],
            role: "admin".into(),
        };
        assert!(auth::check_ns_access(&admin, "anything/at/all"));
    }
}
