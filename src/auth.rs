//! 名牌认证 + NS 隔离 + 审计日志
//!
//! agent_registry 表:
//!   agent_id TEXT UNIQUE NOT NULL
//!   display_name TEXT
//!   namespace TEXT NOT NULL
//!   badge_token TEXT UNIQUE NOT NULL  -- SHA-256 hash of agent_key
//!   permission TEXT DEFAULT 'read_write'
//!   allowed_skills TEXT DEFAULT '[]'
//!   created_at TEXT
//!   expires_at TEXT
//!   last_heartbeat TEXT

use crate::storage::SqlitePool;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 审计日志脱敏：敏感字段名片段（不区分大小写匹配）
const SENSITIVE_KEYS: &[&str] = &[
    "api_key", "api-key", "apikey", "token", "secret",
    "password", "passwd", "credential", "auth",
    "authorization", "bearer", "access_key", "accesskey",
    "private_key", "privatekey", "ssl_key", "ssh_key",
    "admin_key", "adminkey",
];

/// Agent 名牌（注册后返回给 Agent）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentBadge {
    pub agent_id: String,
    pub display_name: String,
    pub namespace: String,
    pub badge_token: String,      // 原始 token（仅注册时返回一次）
    pub permission: String,
    pub allowed_skills: Vec<String>,
    pub expires_at: String,
}

/// 认证结果（内部使用）
#[derive(Clone)]
pub struct AuthResult {
    pub agent_id: String,
    pub allowed_ns: Vec<String>,
    pub role: String,
}

/// 注册新 Agent（返回名牌，含原始 token）
pub fn register_agent(
    pool: &SqlitePool,
    agent_id: &str,
    display_name: &str,
    namespaces: &[&str],
    permission: &str,
) -> Result<AgentBadge, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // 生成唯一 namespace
    let namespace = if namespaces.is_empty() {
        format!("agent/{}", agent_id)
    } else {
        namespaces.join(",")
    };

    // 生成随机 token（使用时间戳 + agent_id + 随机数）
    let raw_token = format!("mem_{}_{}_{}", agent_id, chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0), {
        let mut buf = [0u8; 8];
        getrandom::getrandom(&mut buf).unwrap_or_default();
        u64::from_le_bytes(buf)
    });
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    let badge_token = format!("{:x}", hasher.finalize());

    let expires = (chrono::Utc::now() + chrono::Duration::days(365))
        .format("%Y-%m-%dT%H:%M:%S").to_string();

    conn.execute(
        "INSERT OR IGNORE INTO agent_registry
         (agent_id, display_name, namespace, badge_token, permission, allowed_skills, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, '[]', datetime('now'), ?)",
        rusqlite::params![agent_id, display_name, namespace, badge_token, permission, expires],
    ).ok(); // Ignore if already exists — caller should check

    // If already exists, return existing badge info
    let existing = conn.query_row(
        "SELECT agent_id, display_name, namespace, badge_token, permission, allowed_skills, expires_at
         FROM agent_registry WHERE agent_id = ?",
        rusqlite::params![agent_id],
        |row| {
            Ok(AgentBadge {
                agent_id: row.get(0)?,
                display_name: row.get(1)?,
                namespace: row.get(2)?,
                badge_token: row.get(3)?,
                permission: row.get(4)?,
                allowed_skills: serde_json::from_str(&row.get::<_, String>(5).unwrap_or_default()).unwrap_or_default(),
                expires_at: row.get(6)?,
            })
        }
    );

    if let Ok(badge) = existing {
        return Ok(badge);
    }

    Ok(AgentBadge {
        agent_id: agent_id.to_string(),
        display_name: display_name.to_string(),
        namespace: namespace.clone(),
        badge_token,  // SHA-256 hash
        permission: permission.to_string(),
        allowed_skills: vec![],
        expires_at: expires,
    })
}

/// 注册个人登录账号（本地账密）：以 `user_id` 为身份，命名空间 `agent/{user_id}`，
/// `badge_token` 存 `SHA256(password || user_id)`（口令哈希即令牌，客户端登录时自行算）。
/// 已存在同名账号则拒绝（避免无提示覆盖口令）。返回名牌但 `badge_token` 留空（不回显哈希）。
pub fn register_user(
    pool: &SqlitePool,
    user_id: &str,
    display_name: &str,
    password: &str,
    namespace_override: Option<&str>,
) -> Result<AgentBadge, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // 已存在同名账号 → 拒绝（防止无提示覆盖口令）
    let exists = conn
        .query_row(
            "SELECT 1 FROM agent_registry WHERE agent_id = ?1",
            rusqlite::params![user_id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if exists {
        return Err("用户已存在".to_string());
    }

    // 默认命名空间 agent/{user_id}；调用方（agent-core 代理层）可传入部署相关的
    // 组织命名空间（如追加 org/cs-pufa-2nd-thermal 以获得 dashboard 等共享工具可见性）。
    let namespace = namespace_override
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("agent/{}", user_id));
    // badge_token = SHA256(password || user_id)
    let mut hasher = Sha256::new();
    hasher.update(format!("{}{}", password, user_id).as_bytes());
    let badge_token = format!("{:x}", hasher.finalize());

    let expires = (chrono::Utc::now() + chrono::Duration::days(365))
        .format("%Y-%m-%dT%H:%M:%S").to_string();

    conn.execute(
        "INSERT INTO agent_registry
         (agent_id, display_name, namespace, badge_token, permission, allowed_skills, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, 'user', '[]', datetime('now'), ?5)",
        rusqlite::params![user_id, display_name, namespace, badge_token, expires],
    ).map_err(|e| format!("insert: {}", e))?;

    Ok(AgentBadge {
        agent_id: user_id.to_string(),
        display_name: display_name.to_string(),
        namespace,
        badge_token: String::new(), // 不回显哈希
        permission: "user".to_string(),
        allowed_skills: vec![],
        expires_at: expires,
    })
}

/// 个人账号登录：校验 `user_id` + `password`，成功返回该账号的 `badge_token`
/// （= `SHA256(password || user_id)`）与命名空间，供客户端后续作为 `x-user-key` 使用。
/// 口令错误或账号不存在均返回统一错误（不区分，防账号枚举）。
pub fn login_user(
    pool: &SqlitePool,
    user_id: &str,
    password: &str,
) -> Result<AgentBadge, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    let row = conn.query_row(
        "SELECT badge_token, display_name, namespace, permission FROM agent_registry
         WHERE agent_id = ?1 AND expires_at > datetime('now')",
        rusqlite::params![user_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    );

    let (stored_token, display_name, namespace, permission) = match row {
        Ok(v) => v,
        Err(_) => return Err("用户名或口令错误".to_string()),
    };

    // 重算 SHA256(password || user_id) 与库内比对（恒定时间）
    let mut hasher = Sha256::new();
    hasher.update(format!("{}{}", password, user_id).as_bytes());
    let computed = format!("{:x}", hasher.finalize());
    if !ct_eq(&computed, &stored_token) {
        return Err("用户名或口令错误".to_string());
    }

    Ok(AgentBadge {
        agent_id: user_id.to_string(),
        display_name,
        namespace,
        badge_token: computed, // 回传令牌供客户端存储
        permission,
        allowed_skills: vec![],
        expires_at: String::new(),
    })
}

/// 校验名牌 → 返回允许的 namespace 列表
pub fn authenticate(
    pool: &SqlitePool,
    agent_id: &str,
    badge_token: &str,
) -> Result<AuthResult, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    let row = conn.query_row(
        "SELECT badge_token, namespace, permission FROM agent_registry
         WHERE agent_id = ? AND expires_at > datetime('now')",
        rusqlite::params![agent_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }
    );

    match row {
        Ok((stored_token, ns_list, permission)) => {
            if !ct_eq(&stored_token, badge_token) {
                return Err("invalid badge token".to_string());
            }
            let allowed_ns: Vec<String> = ns_list.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Ok(AuthResult {
                agent_id: agent_id.to_string(),
                allowed_ns: if permission == "admin" { vec!["*".to_string()] } else { allowed_ns },
                role: permission,
            })
        }
        Err(_) => Err(format!("unknown or expired agent: {}", agent_id)),
    }
}

/// 恒定时间字符串比较（防 timing side-channel）
pub fn ct_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff = (ab.len() ^ bb.len()) as u8;
    let max = ab.len().max(bb.len());
    for i in 0..max {
        let x = *ab.get(i).unwrap_or(&0);
        let y = *bb.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

/// 检查 namespace 是否有权限（层级 / 包含匹配）
///
/// 授权 ns 与目标 ns 满足以下任一即放行：
/// - 完全一致（`ns == namespace`）
/// - 目标是授权 ns 的后代（`namespace` 以 `ns/` 开头）
/// - 授权 ns 是目标的后代（两者共享同一子树，`ns` 以 `namespace/` 开头）
///
/// 例：授权 `org/公司/div/工程线` 可覆盖其下 `dept/工程部/proj/P1` 等全部后代；
///     部门级工具 `dept/工程部` 又对其下属 `proj/P1` 可见（共享子树）。
pub fn check_ns_access(auth: &AuthResult, namespace: &str) -> bool {
    if auth.role == "admin" { return true; }
    if auth.allowed_ns.contains(&"*".to_string()) { return true; }
    auth.allowed_ns.iter().any(|ns| {
        *ns == namespace
            || namespace.starts_with(&format!("{}/", ns))
            || ns.starts_with(&format!("{}/", namespace))
    })
}

/// 审计日志参数脱敏（移除敏感字段值，保留结构）
///
/// 敏感字段：api_key, token, secret, password, credential, bearer 等
/// 脱敏方式：保留前4字符 + `****`
pub fn sanitize_params(params: &str) -> String {
    // 尝试解析为 JSON，对 value 字段脱敏
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(params) {
        sanitize_json_value(&mut v);
        return serde_json::to_string(&v).unwrap_or_else(|_| params.to_string());
    }

    // 非 JSON 格式 → 通用脱敏：替换敏感 key=value 模式
    let mut result = params.to_string();
    for key in SENSITIVE_KEYS {
        let candidates = [
            format!("{}=", key),
            format!("{}'", key),
            format!("\"{}\":", key),
            format!("{}:", key),
        ];
        for cand in &candidates {
            let lower_cand = cand.to_lowercase();
            // 关键修复：维护搜索起点 start，每轮打完码后推进到本次打码区域之后。
            // 原实现每轮都从 result 开头 find 同一 key 模式，而 replace_range 只替换了
            // 「值」、key 本身仍在，导致下一轮 find 在同一位置重复命中 → 死循环，
            // 卡死 spawn_blocking 线程 → Memoria 单核 100% 常驻（Memoria CPU 84% 根因）。
            let mut start = 0usize;
            while start < result.len() {
                let rel = result[start..].to_lowercase().find(&lower_cand);
                let pos = match rel {
                    Some(r) => start + r,
                    None => break,
                };
                let end_key = pos + cand.len();
                if end_key >= result.len() {
                    break;
                }
                // 定位到值结束（逗号/引号/空格/}）
                let end_val = result[end_key..]
                    .find(|c: char| c == ',' || c == '}' || c == ')' || c == ' ' || c == '\n' || c == '\r')
                    .map(|e| end_key + e)
                    .unwrap_or(result.len());
                let val = result[end_key..end_val].to_string();
                let masked = if val.len() > 4 {
                    format!("{}****", &val[..4])
                } else {
                    "****".to_string()
                };
                let masked_len = masked.len();
                result.replace_range(end_key..end_val, &masked);
                start = end_key + masked_len;
            }
        }
    }
    result
}

/// 递归脱敏 JSON 值中的敏感字段
fn sanitize_json_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let key_lower = key.to_lowercase();
                if SENSITIVE_KEYS.iter().any(|k| key_lower.contains(k)) {
                    if val.is_string() {
                        let s = val.as_str().unwrap();
                        let masked = if s.len() > 4 {
                            format!("{}****", &s[..4])
                        } else {
                            "****".to_string()
                        };
                        *val = serde_json::Value::String(masked);
                    }
                } else {
                    sanitize_json_value(val);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                sanitize_json_value(item);
            }
        }
        _ => {}
    }
}

/// 写入审计日志（自动按周分表，参数自动脱敏）
pub fn audit_log(pool: &SqlitePool, agent_id: &str, tool: &str, params: &str, allowed: bool) {
    if let Ok(conn) = pool.get() {
        let sanitized = sanitize_params(params);
        let week_table = format!("audit_log_{}", iso_week());
        // 自动创建当周表（幂等）
        let _ = conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent_id TEXT,
                    tool TEXT,
                    params TEXT,
                    allowed INTEGER DEFAULT 1,
                    timestamp TEXT
                )", week_table
            ),
            [],
        );
        let _ = conn.execute(
            &format!(
                "INSERT INTO {} (agent_id, tool, params, allowed, timestamp)
                 VALUES (?, ?, ?, ?, datetime('now'))", week_table
            ),
            rusqlite::params![agent_id, tool, sanitized, if allowed { 1 } else { 0 }],
        );
    }
}

/// 返回 ISO 周标识（如 2026W27）
fn iso_week() -> String {
    let now = chrono::Local::now();
    format!("{}W{:02}", now.format("%G"), now.iso_week().week())
}

/// 初始化认证相关表 + 按周分表 + 清理超期分区
pub fn init_auth_tables(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_registry (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT UNIQUE NOT NULL,
            display_name TEXT NOT NULL,
            namespace TEXT NOT NULL,
            badge_token TEXT UNIQUE NOT NULL,
            permission TEXT DEFAULT 'read_write',
            allowed_skills TEXT DEFAULT '[]',
            created_at TEXT,
            expires_at TEXT,
            last_heartbeat TEXT
        );
        CREATE TABLE IF NOT EXISTS skill_catalog (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE NOT NULL,
            description TEXT DEFAULT '',
            version TEXT DEFAULT '1.0.0',
            author TEXT DEFAULT '',
            category TEXT DEFAULT 'general',
            visibility TEXT DEFAULT 'public',
            steps TEXT DEFAULT '[]',
            dependencies TEXT DEFAULT '[]',
            confidence REAL DEFAULT 0.5,
            install_count INTEGER DEFAULT 0,
            checksum TEXT DEFAULT '',
            source TEXT DEFAULT 'manual',
            is_active INTEGER DEFAULT 1,
            published_at TEXT DEFAULT '',
            updated_at TEXT DEFAULT '',
            published_by TEXT DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS agent_skill_whitelist (
            agent_id TEXT NOT NULL,
            skill_name TEXT NOT NULL,
            installed_at TEXT DEFAULT '',
            installed_by TEXT DEFAULT '',
            is_active INTEGER DEFAULT 1,
            PRIMARY KEY (agent_id, skill_name)
        );"
    ).map_err(|e| format!("create tables: {}", e))?;

    // 创建当周审计表
    let week_table = format!("audit_log_{}", iso_week());
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT,
                tool TEXT,
                params TEXT,
                allowed INTEGER DEFAULT 1,
                timestamp TEXT
            )", week_table
        ),
        [],
    ).map_err(|e| format!("create week table: {}", e))?;

    // 创建索引（存在则跳过）
    let _ = conn.execute(
        &format!("CREATE INDEX IF NOT EXISTS idx_{}_time ON {}(timestamp DESC)", week_table, week_table),
        [],
    );

    // 清理超期分区（>90 天）
    cleanup_stale_tables(&conn);

    Ok(())
}

/// 删除 90 天前的审计分区表
fn cleanup_stale_tables(conn: &rusqlite::Connection) {
    let now = chrono::Local::now();
    let cutoff = now - chrono::Duration::days(90);
    let cutoff_week = format!("{}W{:02}", cutoff.format("%G"), cutoff.iso_week().week());

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%'")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    for table in &tables {
        let week_str = table.trim_start_matches("audit_log_");
        if week_str <= cutoff_week.as_str() {
            let _ = conn.execute(&format!("DROP TABLE IF EXISTS {}", table), []);
            tracing::info!("删除超期审计分区: {}", table);
        }
    }
}
