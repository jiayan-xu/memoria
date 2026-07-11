//! Rust memory_user_prefs and memory_recent_decisions.
//! Phase 3: Queries the correct tables matching Python side:
//! - user_prefs → user_prefs table
//! - recent_decisions → decisions table

use crate::storage::SqlitePool;

/// Get user preferences from `user_prefs` table, scoped to the caller's namespace.
/// 同时返回 `default` 命名空间下的全局偏好（兼容旧数据 + 全局共享偏好），
/// 但仅返回归属当前 ns 或全局 `default` 的行，杜绝跨租户泄露（B3 修复）。
pub fn user_prefs(pool: &SqlitePool, namespace: &str) -> Result<Vec<(String, String, f64)>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn.prepare(
        "SELECT key, value, confidence FROM user_prefs WHERE (namespace = ? OR namespace = 'default') ORDER BY updated_at DESC LIMIT 50"
    ).map_err(|e| format!("prepare: {}", e))?;

    let rows = stmt.query_map(rusqlite::params![namespace], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
        ))
    }).map_err(|e| format!("query: {}", e))?;

    let mut results = Vec::new();
    for row in rows.flatten() {
        results.push(row);
    }
    Ok(results)
}

/// Get recent decisions from `decisions` table (matching Python).
pub fn recent_decisions(pool: &SqlitePool, namespace: &str, limit: u32) -> Result<Vec<(String, String, String)>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn.prepare(
        "SELECT id, decision, created_at FROM decisions WHERE namespace = ? ORDER BY created_at DESC LIMIT ?"
    ).map_err(|e| format!("prepare: {}", e))?;

    let rows = stmt.query_map(rusqlite::params![namespace, limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2).unwrap_or_default(),
        ))
    }).map_err(|e| format!("query: {}", e))?;

    let mut results = Vec::new();
    for row in rows.flatten() {
        results.push(row);
    }
    Ok(results)
}
