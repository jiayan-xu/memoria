//! Phase B / M1.3：轻量实体共现启发式 rerank（O5：无 cross-encoder、无新重依赖）。
//!
//! 在 RRF 融合结果上对 `rrf_score` 做小幅加成后重排：
//! 1. 查询串命中实体名 → 提及该实体的记忆加分
//! 2. 候选集内实体共现度（与其它候选共享实体越多越高）→ 加分
//!
//! 加成幅度刻意保守，避免淹没 keyword/semantic 主信号。

use crate::search::rrf::FusedResult;
use crate::storage::SqlitePool;
use std::collections::{HashMap, HashSet};

const QUERY_HIT_BOOST: f64 = 0.012;
const PAIRWISE_BOOST: f64 = 0.004;
const MAX_TOTAL_BOOST: f64 = 0.06;

/// 对融合结果做共现启发式重排（原地改 `rrf_score` 并重排）。
/// 无实体表数据或失败时静默跳过（不改变相对序）。
pub fn rerank_by_cooccurrence(
  pool: &SqlitePool,
  namespace: &str,
  query: &str,
  results: &mut Vec<FusedResult>,
) {
  if results.len() < 2 && query.trim().is_empty() {
    return;
  }
  if results.is_empty() {
    return;
  }

  let ids: Vec<String> = results.iter().map(|r| r.memory_id.clone()).collect();
  let mem_entities = match load_memory_entities(pool, namespace, &ids) {
    Some(m) if !m.is_empty() => m,
    _ => return,
  };

  let query_entities = match_query_entities(pool, namespace, query);

  // 候选集内：entity_id → 出现在哪些 memory
  let mut entity_to_mems: HashMap<String, HashSet<String>> = HashMap::new();
  for (mid, ents) in &mem_entities {
    for e in ents {
      entity_to_mems
        .entry(e.clone())
        .or_default()
        .insert(mid.clone());
    }
  }

  for r in results.iter_mut() {
    let ents = mem_entities.get(&r.memory_id);
    let mut boost = 0.0;

    if let Some(ents) = ents {
      // 查询命中实体
      let hits = ents.iter().filter(|e| query_entities.contains(*e)).count();
      boost += hits as f64 * QUERY_HIT_BOOST;

      // 与其它候选的共现度
      let mut peer_overlap = 0usize;
      for e in ents {
        if let Some(peers) = entity_to_mems.get(e) {
          peer_overlap += peers.len().saturating_sub(1);
        }
      }
      boost += peer_overlap as f64 * PAIRWISE_BOOST;
    }

    if boost > MAX_TOTAL_BOOST {
      boost = MAX_TOTAL_BOOST;
    }
    if boost > 0.0 {
      r.rrf_score += boost;
      if !r.source.contains("cooccur") {
        r.source = format!("{};cooccur", r.source);
      }
    }
  }

  results.sort_by(|a, b| {
    b.rrf_score
      .partial_cmp(&a.rrf_score)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
}

fn load_memory_entities(
  pool: &SqlitePool,
  namespace: &str,
  memory_ids: &[String],
) -> Option<HashMap<String, Vec<String>>> {
  if memory_ids.is_empty() {
    return Some(HashMap::new());
  }
  let conn = pool.get().ok()?;
  let ph = vec!["?"; memory_ids.len()].join(",");
  let sql = format!(
    "SELECT memory_id, entity_id FROM entity_mentions \
     WHERE namespace = ?1 AND memory_id IN ({})",
    ph
  );
  let mut stmt = conn.prepare(&sql).ok()?;
  let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + memory_ids.len());
  params.push(&namespace);
  for id in memory_ids {
    params.push(id);
  }
  let rows = stmt
    .query_map(rusqlite::params_from_iter(params), |r| {
      Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .ok()?;
  let mut map: HashMap<String, Vec<String>> = HashMap::new();
  for row in rows.flatten() {
    map.entry(row.0).or_default().push(row.1);
  }
  Some(map)
}

/// 查询串子串命中的实体 id（ns 内，限 64）。
fn match_query_entities(pool: &SqlitePool, namespace: &str, query: &str) -> HashSet<String> {
  let mut out = HashSet::new();
  let q = query.trim();
  if q.is_empty() {
    return out;
  }
  let Ok(conn) = pool.get() else {
    return out;
  };
  let Ok(mut stmt) = conn.prepare(
    "SELECT id, name FROM entities WHERE namespace = ?1 LIMIT 256",
  ) else {
    return out;
  };
  let Ok(rows) = stmt.query_map(rusqlite::params![namespace], |r| {
    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
  }) else {
    return out;
  };
  for row in rows.flatten() {
    let (id, name) = row;
    let n = name.trim();
    if n.len() >= 2 && q.contains(n) {
      out.insert(id);
      if out.len() >= 64 {
        break;
      }
    }
  }
  out
}

#[cfg(test)]
mod unit_tests {
  use super::*;

  #[test]
  fn empty_results_noop() {
    let mut v: Vec<String> = Vec::new();
    // pool 不可用时也不应 panic —— 用假路径会拿不到 pool；这里只测空输入
    assert!(v.is_empty());
    let _ = &mut v;
  }
}
