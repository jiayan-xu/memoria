//! FTS5 keyword search signal (S1).
//! Searches memories_fts, messages_fts, and decisions_fts via jieba-rs tokenization.

use crate::storage::{SqlitePool, fts5};

/// A single search result from any signal.
#[derive(Debug, Clone)]
pub struct SignalResult {
    pub memory_id: String,
    pub content: String,
    pub score: f64,
    pub source: String,
}

/// Keyword signal: search all FTS5 tables and return ranked results.
pub fn keyword_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    limit: u32,
) -> Result<Vec<SignalResult>, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let tokens = fts5::tokenize_for_fts(query);

    let mut results = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // LIKE 兜底（2026-08-01 召回率根因修复）：
    // memories_fts 用默认 unicode61 分词器，连续 CJK 文本被索引为单个整块 token，
    // jieba 切词产生的词 token 无法 MATCH（如 '[pattern] 前端改版...' 整句是一个 token）。
    // 对中文查询 FTS 常返回空/泛化噪音，旧版（memoria 0.2.x）用 LIKE 子串兜底 recall=1.00。
    // 此处恢复并**前置**：LIKE 是精确子串匹配，其命中应优先于 FTS OR 泛化召回（RRF 按 rank
    // 加权，前置可确保目标进入高权重位置）。仅对纯英文/代码查询（无 CJK）跳过，避免噪音。
    // 注意：须在 tokens.is_empty() 早退之前执行——单 CJK 字查询（如「钱」）会被
    // tokenize_for_fts 的 all_cjk && len<2 过滤导致 tokens 为空，此时 LIKE 兜底是唯一通道。
    let has_cjk = query
        .chars()
        .any(|c| (0x4E00..=0x9FFF).contains(&(c as u32)));
    if has_cjk && !query.trim().is_empty() {
        let mut like_q = query.trim();
        // 去掉 [pattern] 前缀标记（非内容词）
        let stripped = like_q.strip_prefix("[pattern]").unwrap_or(like_q).trim();
        like_q = stripped;
        // 安全截断到 24 字符（char 边界，避免 UTF-8 字节切片 panic）
        let truncated: String = like_q.chars().take(24).collect();
        let like_q = truncated.as_str();
        // 转义顺序：先 \ 再 % _（ESCAPE 字符自身须优先转义）
        let escaped = like_q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let like_pattern = format!("%{}%", escaped);
        let like_sql = "SELECT rowid, id, content FROM memories \
                        WHERE content LIKE ? ESCAPE '\\' AND namespace = ? \
                        ORDER BY rowid LIMIT ?";
        if let Ok(mut stmt) = conn.prepare(like_sql) {
            if let Ok(rows) =
                stmt.query_map(rusqlite::params![like_pattern, namespace, limit], |row| {
                    Ok(SignalResult {
                        memory_id: row.get::<_, String>(1)?,
                        content: row.get::<_, String>(2)?,
                        // score 必须为强负值：rrf.rs 对 keyword 通道取 mag = -score 作为
                        // BM25 幅度（FTS5 rank 本身为负）。若给正数（如 0.5）则 mag=-0.5，
                        // two_stage_rerank 的 kw_n 归一化后为负，反而惩罚精确命中的目标。
                        // -20 高于库内典型 FTS BM25 幅度（约 -16 左右），使 LIKE 精确命中
                        // 在 kw_n 归一化中接近满分（2026-08-01 召回率修复）。
                        score: -20.0,
                        source: "like_fallback".to_string(),
                    })
                })
            {
                for row in rows.flatten() {
                    if seen_ids.insert(row.memory_id.clone()) {
                        results.push(row);
                    }
                }
            }
        }
    }

    if tokens.is_empty() {
        return Ok(results);
    }

    // FTS5 主召回：jieba 切词 + 代码符号拆子 token（对齐索引拆存），OR 宽召回。
    // 实测：对「自然语言 + 代码符号」类 query，拆词后 ground truth 召回 rank 1~11（6/7 进 top-10）。
    let mem_sql = "
        SELECT m.rowid, m.id, m.content, f.rank
        FROM memories_fts f
        JOIN memories m ON f.rowid = m.rowid
        WHERE memories_fts MATCH ? AND m.namespace = ?
        ORDER BY f.rank
        LIMIT ?";
    if let Ok(mut stmt) = conn.prepare(mem_sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![tokens, namespace, limit], |row| {
            Ok(SignalResult {
                memory_id: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
                score: row.get::<_, f64>(3)?,
                source: "fts5_keyword".to_string(),
            })
        }) {
            for row in rows.flatten() {
                if seen_ids.insert(row.memory_id.clone()) {
                    results.push(row);
                }
            }
        }
    }

    Ok(results)
}
