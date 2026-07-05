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
            if stored_token != badge_token {
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

/// 检查 namespace 是否有权限（精确匹配）
pub fn check_ns_access(auth: &AuthResult, namespace: &str) -> bool {
    if auth.role == "admin" { return true; }
    if auth.allowed_ns.contains(&"*".to_string()) { return true; }
    auth.allowed_ns.iter().any(|ns| ns == namespace)
}

/// 写入审计日志（自动按周分表）
pub fn audit_log(pool: &SqlitePool, agent_id: &str, tool: &str, params: &str, allowed: bool) {
    if let Ok(conn) = pool.get() {
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
            rusqlite::params![agent_id, tool, params, if allowed { 1 } else { 0 }],
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
