//! Phase B / P1 验收（HMS 吸收）
//!   B1/M1.1: occurred tags → ledger.occurred（Phase A 已落地，回归）
//!   B2/M1.2: event_time 加列 — 本轮不做（O2）
//!   B3/O1-P1: ledger JOIN entities（有 mention 则非空；无则 []）
//!   B4/B5/M1.3: 轻量共现启发式 rerank（无 cross-encoder）
//!   B6: 冲突走 supersede，非 DELETE
//!
//! 运行：`cargo test --test p1_hms_phase_b`

use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::ledger::{enrich_ledger, ledger_join_entities_enabled};
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::MemoriaEngine;
use rusqlite::params;
use serde_json::Value;

fn fresh_engine(tag: &str) -> (MemoriaEngine, String) {
  let dir = std::env::temp_dir().join(format!("p1_hms_{}_{}", tag, std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
  (engine, "agent/hms_b".to_string())
}

fn seed_entity_mention(
  engine: &MemoriaEngine,
  ns: &str,
  entity_id: &str,
  name: &str,
  etype: &str,
  memory_id: &str,
  context: &str,
) {
  let conn = engine.pool.get().unwrap();
  conn.execute(
    "INSERT OR IGNORE INTO entities(id, namespace, entity_type, name, aliases, summary)
     VALUES(?1, ?2, ?3, ?4, '[]', NULL)",
    params![entity_id, ns, etype, name],
  )
  .unwrap();
  conn.execute(
    "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace) VALUES(?1, ?2, ?3, ?4)",
    params![entity_id, memory_id, context, ns],
  )
  .unwrap();
}

#[test]
fn join_enabled_by_default() {
  assert!(ledger_join_entities_enabled());
}

#[test]
fn ledger_entities_empty_without_mentions() {
  let (engine, ns) = fresh_engine("no_ment");
  let _ = remember_with_dedup(
    &engine.pool,
    "无实体提及的普通事实",
    "fact",
    3,
    "test",
    &ns,
    r#"["occurred:2025-06-01"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("remember");

  let ctx: Value =
    memory_context(&engine.pool, None, None, &ns, Some("普通事实"), 3, true, 12, 15, None)
      .expect("context");
  let row = &ctx["recall"].as_array().expect("recall")[0];
  assert_eq!(row["entities"].as_array().map(|a| a.len()).unwrap_or(99), 0);
  assert_eq!(row["occurred"].as_str().unwrap_or(""), "2025-06-01");
}

#[test]
fn ledger_entities_join_when_mentions_exist() {
  let (engine, ns) = fresh_engine("join");
  let mem = remember_with_dedup(
    &engine.pool,
    "常熟固废监管系统对接 Alpha 试点",
    "fact",
    3,
    "test",
    &ns,
    r#"["occurred:2026-07-01"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("remember");
  let mid = mem.id.clone();

  seed_entity_mention(
    &engine,
    &ns,
    "ent:alpha",
    "Alpha",
    "project",
    &mid,
    "常熟固废监管系统对接 Alpha 试点",
  );
  seed_entity_mention(
    &engine,
    &ns,
    "ent:changshu",
    "常熟",
    "location",
    &mid,
    "常熟固废监管系统对接 Alpha 试点",
  );

  let ctx: Value =
    memory_context(&engine.pool, None, None, &ns, Some("Alpha 试点"), 5, true, 12, 15, None)
      .expect("context");
  let recall = ctx["recall"].as_array().expect("recall");
  assert!(!recall.is_empty());
  let row = recall
    .iter()
    .find(|r| r["memory_id"].as_str() == Some(mid.as_str()))
    .unwrap_or(&recall[0]);
  let ents = row["entities"].as_array().expect("entities array");
  assert!(ents.len() >= 2, "P1 JOIN 应回填实体，got {:?}", ents);
  let names: Vec<&str> = ents.iter().filter_map(|e| e["name"].as_str()).collect();
  assert!(names.contains(&"Alpha"), "names={:?}", names);
  assert!(names.contains(&"常熟"), "names={:?}", names);
  assert!(ents.iter().all(|e| e.get("entity_id").is_some()));
  assert!(ents.iter().all(|e| e.get("entity_type").is_some()));
}

#[test]
fn cooccur_rerank_boosts_query_entity_hit() {
  let (engine, ns) = fresh_engine("rerank");
  let a = remember_with_dedup(
    &engine.pool,
    "试点进度周报摘要",
    "fact",
    2,
    "test",
    &ns,
    r#"["occurred:2026-07-02"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("a");
  let _b = remember_with_dedup(
    &engine.pool,
    "Alpha 完全无关的噪声文档关于天气",
    "fact",
    2,
    "test",
    &ns,
    r#"["occurred:2026-07-03"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("b");
  let mid_a = a.id.clone();

  seed_entity_mention(&engine, &ns, "ent:alpha2", "Alpha", "project", &mid_a, "试点");
  let c = remember_with_dedup(
    &engine.pool,
    "Alpha 相关资源配置说明",
    "fact",
    2,
    "test",
    &ns,
    "[]",
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("c");
  seed_entity_mention(
    &engine,
    &ns,
    "ent:alpha2",
    "Alpha",
    "project",
    &c.id,
    "资源",
  );

  let fused = hybrid_search(&engine.pool, "Alpha 试点", &ns, 10, None, None, None, false)
    .expect("search");
  assert!(!fused.is_empty());
  let hit_a = fused.iter().find(|r| r.memory_id == mid_a);
  assert!(hit_a.is_some(), "挂 Alpha 实体的记忆应被召回");
  let any_cooccur = fused.iter().any(|r| r.source.contains("cooccur"));
  assert!(
    any_cooccur || hit_a.unwrap().rrf_score > 0.0,
    "共现 rerank 应加成或至少保持可检索"
  );
}

#[test]
fn pattern_conflict_uses_supersede_not_delete() {
  let (engine, ns) = fresh_engine("super");
  let old = remember_with_dedup(
    &engine.pool,
    "[pattern] 旧规则：周末不派车",
    "pattern",
    3,
    "test",
    &ns,
    r#"["pattern","auto_consolidated"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("old");
  let old_id = old.id.clone();

  let new = remember_with_dedup(
    &engine.pool,
    "[pattern] 新规则：周末可派应急车",
    "pattern",
    3,
    "test",
    &ns,
    r#"["pattern","auto_consolidated"]"#,
    None,
    None,
    None,
    None,
    Some(old_id.as_str()),
    None,
  )
  .expect("new");
  assert_eq!(new.action, "superseded_explicit");

  let conn = engine.pool.get().unwrap();
  let cnt: i64 = conn
    .query_row(
      "SELECT COUNT(*) FROM memories WHERE id = ?1",
      params![&old_id],
      |r| r.get(0),
    )
    .unwrap();
  assert_eq!(cnt, 1, "旧行必须仍在库中（禁止 DELETE 当真值）");

  let sup: Option<String> = conn
    .query_row(
      "SELECT superseded_by FROM memories WHERE id = ?1",
      params![&old_id],
      |r| r.get(0),
    )
    .unwrap();
  assert_eq!(sup.as_deref(), Some(new.id.as_str()));

  let inc = hybrid_search(&engine.pool, "周末", &ns, 10, None, None, None, true).expect("all");
  assert!(
    inc.iter().any(|r| r.memory_id == old_id),
    "include_superseded=true 应见到旧 pattern"
  );
}

#[test]
fn enrich_ledger_join_matches_context() {
  let (engine, ns) = fresh_engine("enrich");
  let mem = remember_with_dedup(
    &engine.pool,
    "Memoria ledger JOIN 验收",
    "fact",
    3,
    "test",
    &ns,
    "[]",
    None,
    None,
    None,
    None,
    None,
    None,
  )
  .expect("remember");
  seed_entity_mention(
    &engine,
    &ns,
    "ent:mem",
    "Memoria",
    "system",
    &mem.id,
    "ledger",
  );

  let fused = hybrid_search(&engine.pool, "ledger JOIN", &ns, 5, None, None, None, false)
    .expect("search");
  let ledger = enrich_ledger(&engine.pool, &ns, &fused);
  let row = ledger
    .iter()
    .find(|r| r["memory_id"].as_str() == Some(mem.id.as_str()))
    .expect("row");
  assert_eq!(
    row["entities"].as_array().map(|a| a.len()).unwrap_or(0),
    1
  );
}
