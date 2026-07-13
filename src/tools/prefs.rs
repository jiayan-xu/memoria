//! Rust memory_user_prefs and memory_recent_decisions.
//!
//! P1-2 用户偏好块产品化：
//! - 偏好以「标准记忆」形式写入（`memory_remember`，`category = "preference"`，
//!   tags ∈ {pref, hard_rule, style}），不再写入独立的 user_prefs 表。
//! - `user_prefs` 从 `memories` 表按 ns 聚合近期高 importance 偏好，
//!   `hard_rule` 优先，其次 importance 降序、created_at 降序。
//! - 兼容遗留 `user_prefs` 表（Python 时代），作为低优先级兜底，避免历史数据不可见。

use crate::storage::SqlitePool;

/// 偏好类型标签约定（P1-2）。顺序即排序优先级（hard_rule 最高）。
pub const PREF_TAGS: &[&str] = &["hard_rule", "pref", "style"];

/// 单条偏好投影。
#[derive(Debug, Clone)]
pub struct PreferenceEntry {
    /// 主标签（hard_rule / pref / style / legacy）
    pub key: String,
    /// 偏好内容（记忆正文）
    pub value: String,
    /// 重要性（来自 memories.importance；legacy 为 0）
    pub importance: i64,
    /// 类型标签
    pub tag: String,
    /// 置信度（来自 memories.confidence）
    pub confidence: f64,
    /// 创建时间（ISO，legacy 为空）
    pub created_at: String,
}

/// 从 tags JSON 串中提取首个偏好类型标签（hard_rule > pref > style）。
fn pref_tag_of(tags: &str) -> Option<String> {
    for t in PREF_TAGS {
        if tags.contains(&format!("\"{}\"", t)) {
            return Some(t.to_string());
        }
    }
    None
}

/// 按 ns 聚合用户偏好（标准 memories 路径为主，legacy user_prefs 表兜底）。
///
/// 排序：hard_rule 优先 → importance 降序 → created_at 降序。
pub fn user_prefs(pool: &SqlitePool, namespace: &str) -> Result<Vec<PreferenceEntry>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // ── 主路径：标准 memories 表 ──
    let mut stmt = conn
        .prepare(
            "SELECT content, importance, tags, confidence, created_at FROM memories \
         WHERE namespace = ? AND category = 'preference' \
         AND (tags LIKE '%\"hard_rule\"%' OR tags LIKE '%\"pref\"%' OR tags LIKE '%\"style\"%')",
        )
        .map_err(|e| format!("prepare: {}", e))?;

    let rows = stmt
        .query_map(rusqlite::params![namespace], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2).unwrap_or_default(),
                row.get::<_, f64>(3)?,
                row.get::<_, String>(4).unwrap_or_default(),
            ))
        })
        .map_err(|e| format!("query: {}", e))?;

    let mut entries: Vec<PreferenceEntry> = Vec::new();
    for row in rows.flatten() {
        let (content, importance, tags, confidence, created_at) = row;
        if let Some(tag) = pref_tag_of(&tags) {
            entries.push(PreferenceEntry {
                key: tag.clone(),
                value: content,
                importance,
                tag,
                confidence,
                created_at,
            });
        }
    }

    // 排序：hard_rule 优先 → importance 降序 → created_at 降序
    entries.sort_by(|a, b| {
        let a_hard = (a.tag == "hard_rule") as u8;
        let b_hard = (b.tag == "hard_rule") as u8;
        b_hard
            .cmp(&a_hard)
            .then(b.importance.cmp(&a.importance))
            .then(b.created_at.cmp(&a.created_at))
    });

    // ── 兜底：遗留 user_prefs 表（Python 时代，key/value/confidence）─
    if let Ok(mut stmt) = conn.prepare(
        "SELECT key, value, confidence FROM user_prefs WHERE (namespace = ? OR namespace = 'default')"
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![namespace], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
        }) {
            for row in rows.flatten() {
                let (k, v, c) = row;
                entries.push(PreferenceEntry {
                    key: k.clone(),
                    value: v,
                    importance: 0,
                    tag: "legacy".to_string(),
                    confidence: c,
                    created_at: String::new(),
                });
            }
        }
    }

    Ok(entries)
}

/// Get recent decisions from `decisions` table (matching Python).
pub fn recent_decisions(
    pool: &SqlitePool,
    namespace: &str,
    limit: u32,
) -> Result<Vec<(String, String, String)>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut stmt = conn.prepare(
        "SELECT id, decision, created_at FROM decisions WHERE namespace = ? ORDER BY created_at DESC LIMIT ?"
    ).map_err(|e| format!("prepare: {}", e))?;

    let rows = stmt
        .query_map(rusqlite::params![namespace, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2).unwrap_or_default(),
            ))
        })
        .map_err(|e| format!("query: {}", e))?;

    let mut results = Vec::new();
    for row in rows.flatten() {
        results.push(row);
    }
    Ok(results)
}
