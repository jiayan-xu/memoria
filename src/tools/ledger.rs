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
      if date.len() >= 10 {
        let d = &date[..10];
        if d.as_bytes().get(4) == Some(&b'-') && d.as_bytes().get(7) == Some(&b'-') {
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
  if s.len() >= 10 {
    let d = &s[..10];
    if d.as_bytes().get(4) == Some(&b'-') && d.as_bytes().get(7) == Some(&b'-') {
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
          r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "[]".to_string()),
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
        r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "[]".to_string()),
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
      r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "other".to_string()),
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
      let occurred = parse_occurred_tag(tags_json)
        .or_else(|| {
          if legacy_et.is_empty() {
            None
          } else if legacy_et.len() >= 10 {
            Some(legacy_et[..10].to_string())
          } else {
            Some(legacy_et)
          }
        })
        .unwrap_or_else(|| valid_from.clone());

      let entities = entities_map
        .get(&f.memory_id)
        .cloned()
        .unwrap_or_default();

      let text_signals = crate::search::text_signals::extract_text_signals(
        &f.content,
        tags_json,
        Some(occurred.as_str()),
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
}
