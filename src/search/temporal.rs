//! Temporal recency signal (S3).
//! Scores memories by recency: recent items get higher scores.

use crate::storage::SqlitePool;

use super::keyword::SignalResult;

/// Temporal signal: rank by recency + importance × decay.
pub fn temporal_search(
    pool: &SqlitePool,
    namespace: &str,
    limit: u32,
) -> Result<Vec<SignalResult>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    let sql = "
        SELECT rowid, id, content, created_at, last_recalled, importance,
               decay_factor, recall_count, tier, confidence
        FROM memories
        WHERE namespace = ?
        ORDER BY created_at DESC
        LIMIT ?";

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
    let rows = stmt.query_map(rusqlite::params![namespace, limit], |row| {
        let created: Option<String> = row.get(3)?;
        let recalled: Option<String> = row.get(4)?;
        let importance: f64 = row.get::<_, f64>(5).unwrap_or(3.0);
        let decay: f64 = row.get::<_, f64>(6).unwrap_or(1.0);
        let recall_count: f64 = row.get::<_, f64>(7).unwrap_or(0.0);

        // Calculate temporal score: recency + recall recency + recall frequency
        let days_since = days_ago(&created).unwrap_or(30.0);
        let days_recalled = days_ago(&recalled).unwrap_or(days_since);

        let imp_norm = importance / 3.0;
        let ts = ((1.0 / (1.0 + days_since * 0.5)) + (1.0 / (1.0 + days_recalled * 0.3))) * imp_norm * decay;
        let ts = ts * (1.0 + recall_count * 0.1); // recall frequency bonus

        Ok(SignalResult {
            memory_id: row.get::<_, String>(1)?,
            content: row.get::<_, String>(2)?,
            score: ts,
            source: "temporal_recency".to_string(),
        })
    }).map_err(|e| format!("query: {}", e))?;

    let mut results: Vec<SignalResult> = rows.flatten().collect();
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(results)
}

fn days_ago(date_str: &Option<String>) -> Option<f64> {
    let s = date_str.as_ref()?;
    let days = chrono::Utc::now().date_naive()
        .signed_duration_since(chrono::NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d").ok()?)
        .num_days();
    Some(days.max(0) as f64)
}
