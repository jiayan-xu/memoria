//! Data models matching server.py SQLite schema.
//! All fields map 1:1 to database columns.
#![allow(dead_code)] // Phase 1.2: consumed by Phase 1.3/1.4

use serde::{Deserialize, Serialize};

/// A memory entry (matches `memories` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub namespace: String,
    pub source: Option<String>,
    pub content: String,
    pub category: Option<String>,
    pub confidence: f64,
    pub recall_count: i64,
    pub last_recalled: Option<String>,
    pub created_at: Option<String>,
    pub promoted_at: Option<String>,
    pub tier: String,
    pub evidence: Option<String>,
    pub importance: i64,
    pub decay_factor: f64,
}

/// A conversation message (matches `messages` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tokens: i64,
    pub seq: Option<i64>,
    pub timestamp: Option<String>,
}

/// A conversation session (matches `sessions` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub file_path: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<String>,
    pub message_count: i64,
    pub indexed_at: Option<String>,
}

/// A user preference (matches `user_prefs` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPref {
    pub key: String,
    pub value: String,
    pub evidence: Option<String>,
    pub confidence: f64,
    pub updated_at: Option<String>,
}

/// A decision record (matches `decisions` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub namespace: String,
    pub topic: Option<String>,
    pub decision: String,
    pub rationale: Option<String>,
    pub context: Option<String>,
    pub session_id: Option<String>,
    pub created_at: Option<String>,
}

/// A relation edge between two memories (matches `memory_relations` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub id: i64,
    pub namespace: String,
    pub source_id: String,
    pub target_id: String,
    pub relation_type: String,
    pub weight: f64,
    pub evidence: Option<String>,
    pub created_at: Option<String>,
}

/// Dream state (matches `dream_state` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamState {
    pub phase: String,
    pub last_run: Option<String>,
    pub sessions_processed: i64,
}

/// Decay log entry (matches `decay_log` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayLog {
    pub id: i64,
    pub memory_id: Option<String>,
    pub old_tier: Option<String>,
    pub new_tier: Option<String>,
    pub old_decay: Option<f64>,
    pub new_decay: Option<f64>,
    pub reason: Option<String>,
    pub logged_at: Option<String>,
}
