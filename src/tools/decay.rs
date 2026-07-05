//! Rust memory_decay — 衰减循环。
//! Phase 3: 定期衰减 memories，记录衰减日志。

use crate::storage::SqlitePool;

/// Run a single decay cycle.
/// Returns (processed, cold_count).
pub fn run_decay(pool: &SqlitePool, namespace: &str) -> Result<(u32, u32), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // 1. Log old values BEFORE updating — match each row individually
    conn.execute(
        "INSERT INTO decay_log (memory_id, old_tier, new_tier, old_decay, new_decay, reason, logged_at)
         SELECT id, tier, 'decayed', decay_factor, ROUND(decay_factor * 0.95, 4), 'auto_decay', datetime('now')
         FROM memories WHERE tier IN ('hot','warm') AND namespace = ? AND decay_factor > 0.1",
        rusqlite::params![namespace],
    ).map_err(|e| format!("log: {}", e))?;

    // 2. Decay all warm/hot memories
    let affected = conn.execute(
        "UPDATE memories SET decay_factor = ROUND(decay_factor * 0.95, 4)
         WHERE tier IN ('hot','warm') AND namespace = ? AND decay_factor > 0.1",
        rusqlite::params![namespace],
    ).map_err(|e| format!("decay: {}", e))?;
    let processed = affected as u32;

    // 3. Move very cold memories to 'cold' tier
    let cold_affected = conn.execute(
        "UPDATE memories SET tier = 'cold' WHERE tier IN ('hot','warm')
         AND namespace = ? AND decay_factor <= 0.1 AND recall_count < 3",
        rusqlite::params![namespace],
    ).map_err(|e| format!("cold: {}", e))?;

    Ok((processed, cold_affected as u32))
}
