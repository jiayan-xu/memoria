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
    pub superseded_by: Option<String>,
    /// PR1（Phase B 前置）：写入前门提取压缩元数据（agent-core 主，Memoria 薄存储）。
    /// actor=事实作者/来源主体；memory_type=declarative/procedural/...；
    /// parent_id=原子事实挂回的原始记忆；raw_ref=原文旁路存储引用。
    /// 旧行 NULL 视为 agent_inferred / declarative（检索/画像读取时兜底）。
    pub actor: Option<String>,
    pub memory_type: Option<String>,
    pub parent_id: Option<String>,
    pub raw_ref: Option<String>,
    /// PR4（Phase A 演化）：演化写回元数据（agent-core 的 Dream/consolidate 批处理填充；Memoria 哑存储）。
    /// evolved_context=演化合成的上下文/摘要；evolved_at=最近演化时间戳（NULL=待演化/脏标记）；
    /// link_count=演化后关联边数。旧行 NULL 视为「待演化」，recall 可降权/标注。
    pub evolved_context: Option<String>,
    pub evolved_at: Option<String>,
    pub link_count: Option<i64>,
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
    pub namespace: String,
    pub last_run: Option<String>,
    pub cursor_ts: Option<String>,
    pub runs: i64,
    pub items_out: i64,
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

/// A knowledge graph entity (matches `entities` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub namespace: String,
    pub entity_type: String,
    pub name: String,
    pub aliases: Option<String>,
    pub summary: Option<String>,
    pub created_at: Option<String>,
}

/// An entity mention in a memory (matches `entity_mentions` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityMention {
    pub id: i64,
    pub entity_id: String,
    pub memory_id: String,
    pub context: Option<String>,
    pub namespace: String,
    pub created_at: Option<String>,
}

/// A relation edge between two entities (matches `entity_edges` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEdge {
    pub id: i64,
    pub namespace: String,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation_type: String,
    pub weight: f64,
    pub evidence: Option<String>,
    pub created_at: Option<String>,
}
