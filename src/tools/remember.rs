//! Rust memory_remember implementation.
//! Phase 2.5: SQLite INSERT with SHA-256 dedup (compatible with Python side).
//! Returns the memory ID (existing or new).

use crate::storage::SqlitePool;
use sha2::{Digest, Sha256};

/// Remember a durable memory with SHA-256 dedup (compatible with Python).
pub fn remember(
    pool: &SqlitePool,
    content: &str,
    category: &str,
    importance: i64,
    source: &str,
    namespace: &str,
    tags: &str,  // JSON array: '["tag1","tag2"]'
) -> Result<String, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;

    // SHA-256 hash matching Python's hashlib.sha256(content.encode()).hexdigest()[:16]
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize())[..16].to_string();

    // Use content_hash as part of memory ID for dedup
    let mem_id = content_hash.clone();  // No prefix — matches Python _hash_content()

    // Check if already exists (INSERT OR REPLACE uses the same ID)
    let existing: Result<String, _> = conn.query_row(
        "SELECT id FROM memories WHERE id = ?",
        rusqlite::params![mem_id],
        |row| row.get(0),
    );

    if let Ok(_existing_id) = existing {
        // Memory exists — boost importance and merge tags
        let tags_safe = if tags.is_empty() || tags == "[]" { String::new() } else { tags.to_string() };
        conn.execute(
            "UPDATE memories SET importance = MAX(importance, ?), confidence = MAX(confidence, 0.8),
             recall_count = recall_count + 1, last_recalled = ? WHERE id = ?",
            rusqlite::params![importance, chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(), mem_id],
        ).map_err(|e| format!("update: {}", e))?;
        // Merge tags if provided
        if !tags_safe.is_empty() {
            let _ = conn.execute(
                "UPDATE memories SET tags = ? WHERE id = ? AND tags = '[]'",
                rusqlite::params![tags_safe, mem_id],
            );
        }
        return Ok(mem_id);
    }

    // Insert new memory
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let tags_safe = if tags.is_empty() || tags == "[]" { "[]".to_string() } else { tags.to_string() };
    conn.execute(
        "INSERT INTO memories (id, namespace, source, content, category, confidence,
         recall_count, created_at, tier, importance, decay_factor, tags)
         VALUES (?, ?, ?, ?, ?, 0.8, 0, ?, 'hot', ?, 1.0, ?)",
        rusqlite::params![mem_id, namespace, source, content, category, now, importance, tags_safe],
    ).map_err(|e| format!("insert: {}", e))?;

    Ok(mem_id)
}
