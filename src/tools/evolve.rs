//! PR4（Phase A 演化）：记忆演化写回 + 回滚（哑存储，认知在 agent-core 的 Dream/consolidate）。
//!
//! 遵守 H1/H2：本模块**不调 LLM**，只做纯 SQL 写 + 记 `evolution_log`（old_value 可回滚）。
//! 演化决策（update_context / add_tags / add_edge / supersede）由 agent-core 的 consolidate
//! 批处理 LLM 产出，再通过 MCP `memory_evolve` / `evolution_rollback` 调到这里落库。

use rusqlite::params;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::storage::SqlitePool;

/// 演化降级开关读取：默认演化写回开启；`MEMORIA_EVOLVE_WRITE=0/false/off/no` 关闭（仅记日志不写）。
pub fn evolution_write_enabled() -> bool {
    match std::env::var("MEMORIA_EVOLVE_WRITE") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !(t == "0" || t == "false" || t == "off" || t == "no")
        }
        Err(_) => true,
    }
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn gen_id(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", prefix, ts)
}

/// 对一条记忆施加演化：写入 `evolved_context` / `evolved_at` / `link_count`，
/// 并记 `evolution_log`（old_value 可回滚）。
///
/// - `link_count`：显式给定则用之；为 `None` 时默认统计该记忆当前 `memory_relations` 关联边数。
/// - `change_type`：变更类型（context_update / links_update / ...），记入日志。
pub fn evolve_memory(
    pool: &SqlitePool,
    target_id: &str,
    namespace: &str,
    evolved_context: &str,
    link_count: Option<i64>,
    model: &str,
    change_type: &str,
) -> Result<Value, String> {
    if !evolution_write_enabled() {
        return Ok(json!({
            "status": "skipped",
            "reason": "MEMORIA_EVOLVE_WRITE disabled",
            "target_id": target_id,
        }));
    }
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // 读旧值（用于回滚）
    let old: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT evolved_context, link_count FROM memories WHERE id = ? AND namespace = ?",
            params![target_id, namespace],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let (old_ctx, old_links) = old.unwrap_or((None, None));

    let links = match link_count {
        Some(l) => l,
        None => conn
            .query_row(
                "SELECT COUNT(*) FROM memory_relations WHERE source_id = ? OR target_id = ?",
                params![target_id, target_id],
                |row| row.get(0),
            )
            .unwrap_or(0),
    };

    let now = now_iso();
    conn.execute(
        "UPDATE memories SET evolved_context = ?, evolved_at = ?, link_count = ? \
         WHERE id = ? AND namespace = ?",
        params![evolved_context, now, links, target_id, namespace],
    )
    .map_err(|e| format!("evolve update: {}", e))?;

    let old_value = json!({ "evolved_context": old_ctx, "link_count": old_links }).to_string();
    let new_value = json!({ "evolved_context": evolved_context, "link_count": links }).to_string();
    let log_id = gen_id("ev");
    conn.execute(
        "INSERT INTO evolution_log (id, new_id, target_id, change_type, old_value, new_value, model, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![log_id, target_id, target_id, change_type, old_value, new_value, model, now],
    )
    .map_err(|e| format!("evolution_log insert: {}", e))?;

    Ok(json!({
        "status": "evolved",
        "target_id": target_id,
        "evolved_at": now,
        "link_count": links,
        "log_id": log_id,
    }))
}

/// 按 `evolution_log.id` 回滚某次演化：恢复 `old_value`（evolved_context + link_count）。
/// 仅恢复内容字段，保留 `evolved_at`（仍视为此记忆已被演化处理过，非「待演化」）。
pub fn evolution_rollback(pool: &SqlitePool, log_id: &str) -> Result<Value, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT target_id, old_value FROM evolution_log WHERE id = ?",
            params![log_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let (target_id, old_value) = match row {
        Some(r) => r,
        None => {
            return Ok(json!({ "status": "noop", "message": "evolution_log not found" }));
        }
    };
    let (old_ctx, old_links) = match old_value {
        Some(s) => {
            let v: Value = serde_json::from_str(&s).unwrap_or(json!({}));
            (
                v["evolved_context"].as_str().map(|x| x.to_string()),
                v["link_count"].as_i64(),
            )
        }
        None => (None, None),
    };
    conn.execute(
        "UPDATE memories SET evolved_context = ?, link_count = ? WHERE id = ?",
        params![old_ctx, old_links, target_id],
    )
    .map_err(|e| format!("rollback update: {}", e))?;
    Ok(json!({
        "status": "rolled_back",
        "target_id": target_id,
        "restored_evolved_context": old_ctx,
        "restored_link_count": old_links,
    }))
}
