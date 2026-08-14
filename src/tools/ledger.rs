//! Ledger enrichment — HMS 类型化证据账本（Phase A/B / O1–O6）。
//!
//! **仅**由 `memory_context` 调用（O6）。每行：
//! `type` / `occurred`（优先 tags `occurred:YYYY-MM-DD`）/ `mentioned`（valid_from）/
//! `source_ref` / `entities`（Phase B / O1-P1：JOIN `entity_mentions`；可用
//! `MEMORIA_LEDGER_JOIN_ENTITIES=0` 回滚为空数组）/ score。
//! `event_time` 列仅作只读兼容兜底，不以之为写入主路径（O2）。

use crate::search::rrf::FusedResult;
use crate::storage::SqlitePool;
use serde_json::json;
use std::collections::{HashMap, HashSet};

/// 单条记忆的轻量元数据（批量回查用）。
struct MemMeta {
    category: String,
    valid_from: String,
    tags_json: String,
    /// 只读兼容：旧列，非写入主路径
    event_time_legacy: String,
}

/// 从 tags JSON 数组解析 `occurred:YYYY-MM-DD`（O3）。
pub fn parse_occurred_tag(tags_json: &str) -> Option<String> {
    let tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
    for t in tags {
        let s = t.trim();
        if let Some(rest) = s.strip_prefix("occurred:") {
            let date = rest.trim();
            // 宽松：YYYY-MM-DD 或带时间的前 10 字符
            // #R58 bug/low：`date.len()` 是**字节数**——用户标签含多字节字符
            // （如 occurred:💥💥💥，12 字节）时 `&date[..10]` 会在 UTF-8 字符中间
            // 切片 panic（remember 路径传调用方标签，可达）。`get(..10)` 对越界与
            // 非字符边界都返回 None，天然安全。
            // #R66 maintainability/low：**与 legacy_occurred 的数字校验对齐**——
            // 同一 occurred 字段此前两种规则（tags 路只查分隔符、legacy 路全数字），
            // "abcd-ef-gh" 会从 tags 路漏进 extract_text_signals。
            if date.len() >= 10 {
                let Some(d) = date.get(..10) else {
                    continue;
                };
                let b = d.as_bytes();
                if b[0..4].iter().all(|c| c.is_ascii_digit())
                    && b[4] == b'-'
                    && b[5].is_ascii_digit()
                    && b[6].is_ascii_digit()
                    && b[7] == b'-'
                    && b[8].is_ascii_digit()
                    && b[9].is_ascii_digit()
                {
                    return Some(d.to_string());
                }
            }
        }
    }
    None
}

/// 若 `event_time` 参数（ISO）可抽出日期，生成 `occurred:YYYY-MM-DD` tag（写入过渡，不写列）。
pub fn occurred_tag_from_iso(iso: &str) -> Option<String> {
    let s = iso.trim();
    // #R58 bug/low：同 parse_occurred_tag——`&s[..10]` 在多字节 ISO 输入上可 panic，
    // get(..10) 安全（越界/非边界返回 None）。
    // #R68 maintainability/medium：**数字校验与读路径对称**——写路径（mcp_server
    // remember 传用户 event_time）只查分隔符会让 "abcd-ef-gh" 以 occurred 标签
    // 持久化、读回时被新的严格 parse_occurred_tag 拒绝（writer/reader 规则分歧
    // 丢可读值）。
    if s.len() >= 10 {
        let Some(d) = s.get(..10) else {
            return None;
        };
        let b = d.as_bytes();
        if b[0..4].iter().all(|c| c.is_ascii_digit())
            && b[4] == b'-'
            && b[5].is_ascii_digit()
            && b[6].is_ascii_digit()
            && b[7] == b'-'
            && b[8].is_ascii_digit()
            && b[9].is_ascii_digit()
        {
            return Some(format!("occurred:{}", d));
        }
    }
    None
}

/// 把 `occurred:...` 合并进 tags JSON 字符串（若已有同前缀则替换）。
pub fn merge_occurred_tag(tags_json: &str, occurred_tag: &str) -> String {
    let mut tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
    tags.retain(|t| !t.trim().starts_with("occurred:"));
    tags.push(occurred_tag.to_string());
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

/// #R60 test/low：O2 旧列 `event_time_legacy` → occurred 的**纯函数**（enrich_ledger
/// 的 fallback 抽出，可单测）。语义：空串 / 不足 10 字节 / 前 10 字节落在多字节字符
/// 内（get(..10) None）均返回 None——短串或畸形值不进入 ledger 的 occurred 字段
/// （下游 extract_text_signals 日期解析与 YYYY-MM-DD 约定不一致时会产生脏数据），
/// 由调用方 valid_from 兜底。此路径曾是 `&s[..10]` 切片 panic 点（#R58）。
/// 校验规则：**数字 + 分隔符格式**（字节 0..4/5-6/8-9 全 digit + 字节 4/7 == '-'，
/// 与 text_signals::is_iso_date_at 一致）。注意这是**格式校验**而非真实日期
/// 校验——"2024-13-99"（月份 13）仍会通过；下游日期处理是字符串比较而非解析，
/// 影响有限。
pub(crate) fn legacy_occurred(legacy_et: &str) -> Option<String> {
    if legacy_et.is_empty() {
        return None;
    }
    if legacy_et.len() < 10 {
        // #R60 other/low：短串（非 ISO 日期形态）返回 None——此前直接返回原始串
        // 会把非日期值写进 occurred（目标"不喂非日期值给下游"只达成一半）。
        return None;
    }
    // #R61 maintainability/low：**'-' 分隔符校验**（与 parse_occurred_tag 一致）——
    // ≥10 字节的非日期 ASCII（如 "hello-world"）此前会作为 occurred 流入
    // extract_text_signals 日期解析（doc 声称"畸形值不进 ledger"但未实现）。
    // #R64 maintainability/low：**完整数字校验**（对齐 text_signals::is_iso_date_at）——
    // 分隔符正确的非日期串（"abcd-ef-gh"/"2024-13-99"）此前仍会通过。
    let d = legacy_et.get(..10)?;
    let b = d.as_bytes();
    if b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
    {
        Some(d.to_string())
    } else {
        None
    }
}

/// Phase B：是否 JOIN entities 填 ledger（默认开；`MEMORIA_LEDGER_JOIN_ENTITIES=0/false/off` 关）。
pub fn ledger_join_entities_enabled() -> bool {
    match std::env::var("MEMORIA_LEDGER_JOIN_ENTITIES") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !(t == "0" || t == "false" || t == "off" || t == "no")
        }
        Err(_) => true,
    }
}

fn fetch_memory_meta(pool: &SqlitePool, ids: &[String]) -> HashMap<String, MemMeta> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let ph = vec!["?"; ids.len()].join(",");
    // event_time 只读兼容；列不存在时整句失败 → 降级不带该列
    let sql_with_et = format!(
        "SELECT id, category, valid_from, tags, event_time FROM memories WHERE id IN ({})",
        ph
    );
    let sql_no_et = format!(
        "SELECT id, category, valid_from, tags FROM memories WHERE id IN ({})",
        ph
    );
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

    let try_with_et = || -> Result<HashMap<String, MemMeta>, ()> {
        let mut map = HashMap::new();
        let mut stmt = conn.prepare(&sql_with_et).map_err(|_| ())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter().copied()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(3)?
                        .unwrap_or_else(|| "[]".to_string()),
                    r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            })
            .map_err(|_| ())?;
        for row in rows.flatten() {
            map.insert(
                row.0.clone(),
                MemMeta {
                    category: row.1,
                    valid_from: row.2,
                    tags_json: row.3,
                    event_time_legacy: row.4,
                },
            );
        }
        Ok(map)
    };

    if let Ok(map) = try_with_et() {
        return map;
    }

    if let Ok(mut stmt) = conn.prepare(&sql_no_et) {
        if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?
                    .unwrap_or_else(|| "[]".to_string()),
            ))
        }) {
            for row in rows.flatten() {
                out.insert(
                    row.0.clone(),
                    MemMeta {
                        category: row.1,
                        valid_from: row.2,
                        tags_json: row.3,
                        event_time_legacy: String::new(),
                    },
                );
            }
        }
    }
    out
}

/// Phase B / O1-P1：批量 JOIN `entity_mentions` × `entities`。
/// 返回 memory_id → `[{entity_id, name, entity_type}, ...]`（同实体去重，最多 16）。
pub fn fetch_entities_for_memories(
    pool: &SqlitePool,
    namespace: &str,
    memory_ids: &[String],
) -> HashMap<String, Vec<serde_json::Value>> {
    let mut out: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    if memory_ids.is_empty() || !ledger_join_entities_enabled() {
        return out;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let ph = vec!["?"; memory_ids.len()].join(",");
    let sql = format!(
        "SELECT m.memory_id, e.id, e.name, e.entity_type \
     FROM entity_mentions m \
     JOIN entities e ON e.id = m.entity_id \
     WHERE m.namespace = ?1 AND e.namespace = ?1 AND m.memory_id IN ({}) \
     ORDER BY m.memory_id, e.name",
        ph
    );
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + memory_ids.len());
    params.push(&namespace);
    for id in memory_ids {
        params.push(id);
    }
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?
                .unwrap_or_else(|| "other".to_string()),
        ))
    }) else {
        return out;
    };
    let mut seen: HashMap<String, HashSet<String>> = HashMap::new();
    for row in rows.flatten() {
        let (mid, eid, name, etype) = row;
        let set = seen.entry(mid.clone()).or_default();
        if set.contains(&eid) {
            continue;
        }
        let list = out.entry(mid.clone()).or_default();
        if list.len() >= 16 {
            continue;
        }
        set.insert(eid.clone());
        list.push(json!({
          "entity_id": eid,
          "name": name,
          "entity_type": etype,
        }));
    }
    out
}

/// 把召回结果富化为类型化证据账本（O1-P1 / O2 / O3 / O6）。
pub fn enrich_ledger(
    pool: &SqlitePool,
    namespace: &str,
    fused: &[FusedResult],
) -> Vec<serde_json::Value> {
    if fused.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = fused.iter().map(|f| f.memory_id.clone()).collect();
    let meta = fetch_memory_meta(pool, &ids);
    let entities_map = fetch_entities_for_memories(pool, namespace, &ids);

    fused
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let m = meta.get(&f.memory_id);
            let category = m.map(|x| x.category.clone()).unwrap_or_default();
            let valid_from = m.map(|x| x.valid_from.clone()).unwrap_or_default();
            let tags_json = m.map(|x| x.tags_json.as_str()).unwrap_or("[]");
            let legacy_et = m.map(|x| x.event_time_legacy.clone()).unwrap_or_default();

            // O3 优先 tags；O2 旧列只读兜底；再退到 valid_from
            // #R61 other/low 行为变化记录：legacy_occurred 对短串/畸形值返回 None 后
            // 回退 valid_from（可能为空）——O2 旧行的 occurred 从"原始串透传"变为
            // "None/valid_from"；下游（mcp_server ledger/profile、extract_text_signals
            // 日期解析）对空 occurred 容忍（None 分支已存在），符合"不喂非日期值"
            // 的既定目标。
            let occurred = parse_occurred_tag(tags_json)
                .or_else(|| legacy_occurred(&legacy_et))
                .unwrap_or_else(|| valid_from.clone());

            let entities = entities_map.get(&f.memory_id).cloned().unwrap_or_default();

            let text_signals = crate::search::text_signals::extract_text_signals(
                &f.content,
                tags_json,
                // #R64 maintainability/low：空 occurred 传 **None**（显式契约——
                // extract_text_signals 的 !tag_date.is_empty() 守卫只是隐性容忍；
                // 守卫被移除时保持健壮）。
                (!occurred.is_empty()).then_some(occurred.as_str()),
            );

            json!({
              "index": i + 1,
              "memory_id": f.memory_id,
              "content": f.content,
              "rrf_score": f.rrf_score,
              "source": f.source,
              "type": category,
              "occurred": occurred,
              "mentioned": valid_from,
              "source_ref": format!("{}:{}", namespace, f.memory_id),
              "entities": entities,
              "text_signals": text_signals,
              "is_latest": true,
              "evolved_at": f.evolved_at,
              "pending_evolution": f.pending_evolution,
            })
        })
        .collect()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn parse_occurred_tag_ok() {
        assert_eq!(
            parse_occurred_tag(r#"["fact","occurred:2024-03-01"]"#).as_deref(),
            Some("2024-03-01")
        );
        assert_eq!(parse_occurred_tag(r#"["nope"]"#), None);
    }

    #[test]
    fn merge_occurred_replaces() {
        let out = merge_occurred_tag(r#"["a","occurred:2020-01-01"]"#, "occurred:2024-03-01");
        let tags: Vec<String> = serde_json::from_str(&out).unwrap();
        assert!(tags.contains(&"a".to_string()));
        assert!(tags.contains(&"occurred:2024-03-01".to_string()));
        assert!(!tags.iter().any(|t| t == "occurred:2020-01-01"));
    }

    // #R59 test/low：**多字节回归测试**——#R58 修的 &s[..10] 切片 panic 由调用方
    // 标签可达（occurred:💥💥💥 12 字节）；无回归用例则未来重构可静默重引入。
    #[test]
    fn parse_occurred_tag_multibyte_safe() {
        // 字节 10 落在多字节字符内：get(..10) None → 整体 None（不 panic、不误判）
        assert_eq!(parse_occurred_tag(r#"["occurred:💥💥💥"]"#), None);
        // 前 10 字节恰好是完整日期（2024-03-01 是 10 ASCII 字节）→ 正常解析，
        // 后续多字节尾巴不影响
        assert_eq!(
            parse_occurred_tag(r#"["occurred:2024-03-01💥"]"#).as_deref(),
            Some("2024-03-01")
        );
        // occurred_tag_from_iso 同样多字节安全（返回带 occurred: 前缀）
        assert_eq!(
            occurred_tag_from_iso("2024-03-01T12:00:00").as_deref(),
            Some("occurred:2024-03-01")
        );
        assert_eq!(
            occurred_tag_from_iso("2024-03-01💥extra").as_deref(),
            Some("occurred:2024-03-01")
        );
        assert_eq!(occurred_tag_from_iso("💥💥💥💥💥"), None);
    }

    // #R60 test/low：legacy_occurred 单测——enrich_ledger 的 O2 fallback（#R59 唯一
    // 实际改动的路径）此前无任何覆盖：多字节截断 / 短串 / 空串 / 正常日期。
    #[test]
    fn legacy_occurred_cases() {
        assert_eq!(legacy_occurred(""), None);
        assert_eq!(legacy_occurred("2024"), None); // 短串（<10 字节）→ None
        assert_eq!(legacy_occurred("2024-03-01T12:00:00").as_deref(), Some("2024-03-01"));
        // 前 10 字节落在多字节字符内 → None（#R58 panic 点的安全退化）
        assert_eq!(legacy_occurred("💥💥💥💥💥"), None);
        assert_eq!(legacy_occurred("2024-03-01💥extra").as_deref(), Some("2024-03-01"));
        // #R63 test/low：≥10 字节但分隔符非 '-' 的 ASCII 串——#R61 修复的核心
        // 路径（此前透传前 10 字节 "hello-worl" 进 extract_text_signals），必须
        // 有直接用例锁住。
        assert_eq!(legacy_occurred("hello-world"), None);
    }
}
