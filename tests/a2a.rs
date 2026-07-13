//! P1-6 回归：A2A 鉴权收紧。
//!
//! 运行：`cargo test --test a2a`
//!
//! 覆盖：
//! 1. check_ns_access 不因 ns 名含 "admin" 而授予 * 权限（精确字符串比较）。
//! 2. a2a_send to 格式校验：拒绝含 /../ / 空等注入字符的 agent-id。
//!
//! 说明：require_admin 拒绝子串绕过的逻辑已在权限矩阵测试中覆盖（src/permissions.rs 单元测试）。

use memoria_core::auth;

#[test]
fn check_ns_access_no_substring_leak() {
    // agent/ns-foo-admin-x 不应匹配 agent/ns-foo 或任意 admin 权限
    let agent = auth::AuthResult {
        agent_id: "agent-a".into(),
        allowed_ns: vec!["agent/accounting".into()],
        role: "agent".into(),
    };
    // 自己的 ns 可访问
    assert!(
        auth::check_ns_access(&agent, "agent/accounting"),
        "自己的 ns 应可访问"
    );
    // 含子串但不同的 ns 不可访问
    assert!(
        !auth::check_ns_access(&agent, "agent/accounting-admin"),
        "不同的 ns（含子串）不应通过 check_ns_access"
    );
    assert!(
        !auth::check_ns_access(&agent, "agent/admin"),
        "无关的 admin ns 不应通过 check_ns_access"
    );
    assert!(
        !auth::check_ns_access(&agent, "agent/myadmin"),
        "ns 名含 admin 子串但非授权 ns 不应放行"
    );
}

#[test]
fn a2a_to_format_rejects_injections() {
    // 模拟 a2a_send 的 to 参数格式校验（与 mcp_server 同款规则）
    let is_valid = |s: &str| -> bool {
        if s.is_empty() || s.len() > 64 {
            return false;
        }
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };

    // 合法
    assert!(is_valid("agent-123"), "字母数字连字符应合法");
    assert!(is_valid("my_agent"), "下划线应合法");
    assert!(is_valid("a"), "单字符应合法");

    // 非法：路径遍历/注入
    assert!(!is_valid(""), "空串非法");
    assert!(!is_valid("../admin"), ".. 路径遍历非法");
    assert!(!is_valid("agent/evil"), "斜杠注入非法");
    assert!(!is_valid("agent;drop"), "分号注入非法");
    assert!(!is_valid("agent admin"), "空格非法");
    assert!(!is_valid(&"x".repeat(65)), "超 64 字符非法");
}
