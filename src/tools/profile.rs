//! P0-3：会话开场注入用的 Memory Profile / Context 合成视图。
//!
//! 设计见 `docs/DESIGN_MEMORY_PROFILE_AND_GRAPH.md`：
//! - `memory_profile(ns)`：只读合成视图，返回 `static`（稳定偏好/硬规则）+ `dynamic`
//!   （近期 decision/fact/pattern 且当前 tip），不新建「第二套偏好存储」。
//! - `memory_context(ns)`：`memory_profile` + 可选 `query` 时追加 top-k recall，产出可直接拼 prompt 的 `prompt_block`。
//! - 两者一律计入 `profile_bucket` 配额（每 ns ≤10 次/分钟，admin 豁免，见 §14.1 Q3）。

use crate::search;
use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache};
use serde_json::{json, Value};

/// 由 `category` + `tags` 派生 `kind`（响应投影字段，无库列，禁止 ALTER 加 kind）。
fn derive_kind(category: &str, tags: &str) -> String {
    if tags.contains("\"hard_rule\"") {
        "hard_rule".to_string()
    } else if tags.contains("\"pref\"") {
        "pref".to_string()
    } else if tags.contains("\"style\"") {
        "style".to_string()
    } else if tags.contains("\"insight\"") || tags.contains("\"auto_insight\"") {
        "insight".to_string()
    } else {
        category.to_string()
    }
}

/// 把 memories 表的 tags JSON 串解析为 JSON Value（失败则空数组）。
fn tags_json(tags: &str) -> Value {
    serde_json::from_str::<Value>(tags).unwrap_or(Value::Array(vec![]))
}

/// 当前时刻（ISO-8601 含字面量 T，与 remember / valid_at 字典序比较一致）。
fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// 合成 `memory_profile` 只读视图。
///
/// `as_of`：若提供则按该时刻 `valid_*` 过滤，且不强制 tip（visible_as_of）；
/// 默认 now + `superseded_by IS NULL`（is_latest_now）。
pub fn memory_profile(
    pool: &SqlitePool,
    namespace: &str,
    static_limit: usize,
    dynamic_limit: usize,
    as_of: Option<&str>,
) -> Result<Value, String> {
    let now = now_iso();
    let t = as_of.unwrap_or(now.as_str());
    let tip_only = as_of.is_none();
    let tip_clause = if tip_only {
        "AND superseded_by IS NULL"
    } else {
        ""
    };

    // ── static：稳定偏好/身份 ──
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let static_sql = format!(
        "SELECT id, content, category, importance, tags, created_at FROM memories \
         WHERE namespace = ? \
           AND ( (category = 'preference' \
                  AND (tags LIKE '%\"hard_rule\"%' OR tags LIKE '%\"pref\"%' OR tags LIKE '%\"style\"%')) \
                 OR tags LIKE '%\"profile_static\"%' ) \
           {tip} \
           AND (valid_from IS NULL OR valid_from <= ?) \
           AND (valid_to IS NULL OR valid_to >= ?) \
         ORDER BY (tags LIKE '%\"hard_rule\"%') DESC, importance DESC, created_at DESC \
         LIMIT ?",
        tip = tip_clause
    );
    let mut stmt = conn
        .prepare(&static_sql)
        .map_err(|e| format!("prepare static: {}", e))?;
    let static_rows = stmt
        .query_map(
            rusqlite::params![namespace, t, t, static_limit as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4).unwrap_or_default(),
                    row.get::<_, String>(5).unwrap_or_default(),
                ))
            },
        )
        .map_err(|e| format!("query static: {}", e))?;

    let mut static_items: Vec<Value> = Vec::new();
    for row in static_rows.flatten() {
        let (id, content, category, importance, tags, created_at) = row;
        static_items.push(json!({
            "id": id,
            "kind": derive_kind(&category, &tags),
            "content": content,
            "importance": importance,
            "created_at": created_at,
            "category": category,
            "tags": tags_json(&tags),
        }));
    }

    // ── dynamic：近期仍有效的决策/事实/模式（排除 insight）──
    let dyn_sql = format!(
        "SELECT id, content, category, importance, tags, created_at FROM memories \
         WHERE namespace = ? \
           {tip} \
           AND (valid_from IS NULL OR valid_from <= ?) \
           AND (valid_to IS NULL OR valid_to >= ?) \
           AND (category IN ('decision','fact','pattern') \
                OR tags LIKE '%\"decision\"%' OR tags LIKE '%\"fact\"%' OR tags LIKE '%\"pattern\"%') \
           AND NOT (tags LIKE '%\"insight\"%' OR tags LIKE '%\"auto_insight\"%') \
         ORDER BY importance DESC, created_at DESC \
         LIMIT ?",
        tip = tip_clause
    );
    let mut dyn_stmt = conn
        .prepare(&dyn_sql)
        .map_err(|e| format!("prepare dynamic: {}", e))?;
    let dyn_rows = dyn_stmt
        .query_map(
            rusqlite::params![namespace, t, t, dynamic_limit as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4).unwrap_or_default(),
                    row.get::<_, String>(5).unwrap_or_default(),
                ))
            },
        )
        .map_err(|e| format!("query dynamic: {}", e))?;

    let mut dyn_items: Vec<Value> = Vec::new();
    for row in dyn_rows.flatten() {
        let (id, content, category, importance, tags, created_at) = row;
        dyn_items.push(json!({
            "id": id,
            "kind": derive_kind(&category, &tags),
            "content": content,
            "importance": importance,
            "created_at": created_at,
            "category": category,
            "tags": tags_json(&tags),
        }));
    }

    let static_text = render_static_text(&static_items);
    let dynamic_text = render_dynamic_text(&dyn_items);

    Ok(json!({
        "status": "ok",
        "namespace": namespace,
        "generated_at": now,
        "as_of": t,
        "is_latest_applied": tip_only,
        "static": static_items,
        "dynamic": dyn_items,
        "static_text": static_text,
        "dynamic_text": dynamic_text,
    }))
}

/// 合成 `memory_context`：profile + 可选 recall。
pub fn memory_context(
    pool: &SqlitePool,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    namespace: &str,
    query: Option<&str>,
    recall_k: u32,
    include_profile: bool,
    static_limit: usize,
    dynamic_limit: usize,
    as_of: Option<&str>,
) -> Result<Value, String> {
    let profile: Value = if include_profile {
        memory_profile(pool, namespace, static_limit, dynamic_limit, as_of)?
    } else {
        json!({ "static": [], "dynamic": [], "static_text": "", "dynamic_text": "" })
    };

    let mut recall: Vec<Value> = Vec::new();
    if let Some(q) = query {
        if !q.is_empty() {
            if let Ok(fused) = search::hybrid::hybrid_search(
                pool,
                q,
                namespace,
                recall_k,
                hnsw,
                query_cache,
                as_of, // 透传 as_of；None → is_latest_now
                false, // include_superseded=false
            ) {
                for f in fused {
                    recall.push(json!({
                        "memory_id": f.memory_id,
                        "content": f.content,
                        "rrf_score": f.rrf_score,
                        "source": f.source,
                        "is_latest": as_of.is_none(),
                    }));
                }
            }
        }
    }

    let prompt_block = render_prompt_block(&profile, &recall);

    Ok(json!({
        "status": "ok",
        "namespace": namespace,
        "profile": profile,
        "recall": recall,
        "prompt_block": prompt_block,
    }))
}

/// 渲染 static 为 markdown。
fn render_static_text(items: &[Value]) -> String {
    if items.is_empty() {
        return "## 稳定偏好\n（无）".to_string();
    }
    let mut s = String::from("## 稳定偏好\n");
    for it in items {
        let kind = it["kind"].as_str().unwrap_or("");
        let content = it["content"].as_str().unwrap_or("");
        s.push_str(&format!("- [{}] {}\n", kind, content));
    }
    s
}

/// 渲染 dynamic 为 markdown。
fn render_dynamic_text(items: &[Value]) -> String {
    if items.is_empty() {
        return "## 近期动态\n（无）".to_string();
    }
    let mut s = String::from("## 近期动态\n");
    for it in items {
        let kind = it["kind"].as_str().unwrap_or("");
        let content = it["content"].as_str().unwrap_or("");
        s.push_str(&format!("- [{}] {}\n", kind, content));
    }
    s
}

/// 渲染整段 prompt_block（profile + recall 拼接）。
fn render_prompt_block(profile: &Value, recall: &[Value]) -> String {
    let mut block = String::new();
    block.push_str(profile["static_text"].as_str().unwrap_or(""));
    block.push('\n');
    block.push_str(profile["dynamic_text"].as_str().unwrap_or(""));

    if !recall.is_empty() {
        block.push_str("\n## 相关回忆\n");
        for r in recall {
            let content = r["content"].as_str().unwrap_or("");
            block.push_str(&format!("- {}\n", content));
        }
    }
    block
}
