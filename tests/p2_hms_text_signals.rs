//! Phase P2 / M2.1 验收（HMS text_signals 最小切片）
//!   P2.1a: ledger 行含 text_signals（numbers/dates/update_markers）
//!   P2.1b: occurred tag 并入 dates 信号（O3，不写 event_time 列）
//!   P2.1c: hybrid 检索数字/日期 query 与正文重叠加成（O5，无 cross-encoder）
//!   P2.2c 已做: tags 持久化 signal:*（remember 写入 + ledger 读时合并）
//!   P2.2d 已做: agent-core consolidate LLM 抽取 signal tags
//!
//! 运行：`cargo test --test p2_hms_text_signals`

use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::MemoriaEngine;
use serde_json::Value;

fn fresh_engine(tag: &str) -> (MemoriaEngine, String) {
  let dir = std::env::temp_dir().join(format!("p2_hms_{}_{}", tag, std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
  (engine, "agent/hms_p2".to_string())
}

#[test]
fn ledger_includes_text_signals() {
  let (engine, ns) = fresh_engine("ledger");
  let _ = remember_with_dedup(
    &engine.pool,
    "2026-07-10 进厂登记 120 吨，改为应急模式",
    "fact",
    3,
    "test",
    &ns,
    r#"["occurred:2026-07-10"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  
        None,
        None,
        None,
        None)
  .expect("remember");

  let ctx: Value =
    memory_context(&engine.pool, None, None, &ns, Some("120 吨 应急"), 5, true, 8, 8, None)
      .expect("context");
  let recall = ctx["recall"].as_array().expect("recall");
  assert!(!recall.is_empty(), "应有 recall");
  let row = &recall[0];
  let ts = row.get("text_signals").expect("P2.1a: ledger 应含 text_signals");
  let nums = ts["numbers"].as_array().expect("numbers");
  let dates = ts["dates"].as_array().expect("dates");
  let markers = ts["update_markers"].as_array().expect("update_markers");
  assert!(
    nums.iter().any(|n| n.as_str() == Some("120")),
    "numbers={:?}",
    nums
  );
  assert!(
    dates.iter().any(|d| d.as_str() == Some("2026-07-10")),
    "dates={:?}",
    dates
  );
  assert!(
    markers.iter().any(|m| m.as_str() == Some("改为")),
    "markers={:?}",
    markers
  );
}

#[test]
fn search_boosts_on_numeric_query_overlap() {
  let (engine, ns) = fresh_engine("rerank");
  let a = remember_with_dedup(
    &engine.pool,
    "仓库库存 120 吨，盘点正常",
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
  
        None,
        None,
        None,
        None)
  .expect("a");
  let _b = remember_with_dedup(
    &engine.pool,
    "今日天气晴朗适合出行",
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
  
        None,
        None,
        None,
        None)
  .expect("b");
  let mid_a = a.id.clone();

  let fused = hybrid_search(&engine.pool, "120 吨", &ns, 10, None, None, None, false)
    .expect("search");
  assert!(!fused.is_empty());
  let top = &fused[0];
  assert_eq!(top.memory_id, mid_a, "P2.1c: 数字重叠应抬升含 120 的记忆");
  assert!(
    top.source.contains("text_signals") || top.rrf_score > 0.0,
    "source={}",
    top.source
  );
}

#[test]
fn relative_date_in_ledger_signals() {
  let (engine, ns) = fresh_engine("reldate");
  let _ = remember_with_dedup(
    &engine.pool,
    "上周三巡检发现异常，昨天已修复",
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
  
        None,
        None,
        None,
        None)
  .expect("remember");

  let ctx: Value =
    memory_context(&engine.pool, None, None, &ns, Some("上周三 异常"), 5, true, 8, 8, None)
      .expect("context");
  let row = &ctx["recall"].as_array().expect("recall")[0];
  let dates = row["text_signals"]["dates"].as_array().expect("dates");
  assert!(
    dates.len() >= 2,
    "P2.2a: 相对日期应解析为绝对日 dates={:?}",
    dates
  );
  assert!(
    dates.iter().any(|d| d.as_str().map(|s| s.len() == 10).unwrap_or(false)),
    "dates={:?}",
    dates
  );
}

#[test]
fn text_signals_rerank_env_off() {
  let (engine, ns) = fresh_engine("envoff");
  let _ = remember_with_dedup(
    &engine.pool,
    "唯一记忆 999 件",
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
  
        None,
        None,
        None,
        None)
  .expect("mem");

  unsafe {
    std::env::set_var("MEMORIA_TEXT_SIGNALS_RERANK", "0");
  }
  let fused = hybrid_search(&engine.pool, "999", &ns, 5, None, None, None, false).expect("s");
  unsafe {
    std::env::remove_var("MEMORIA_TEXT_SIGNALS_RERANK");
  }
  assert!(
    fused.iter().all(|r| !r.source.contains("text_signals")),
    "关闭 rerank 时不应出现 text_signals 通道标记"
  );
}

#[test]
fn signal_tags_persisted_on_remember() {
  let (engine, ns) = fresh_engine("sigtags");
  let result = remember_with_dedup(
    &engine.pool,
    "2026-07-12 登记 120 吨，改为应急模式",
    "fact",
    3,
    "test",
    &ns,
    r#"["occurred:2026-07-12"]"#,
    None,
    None,
    None,
    None,
    None,
    None,
  
        None,
        None,
        None,
        None)
  .expect("remember");

  let conn = engine.pool.get().expect("conn");
  let tags: String = conn
    .query_row(
      "SELECT tags FROM memories WHERE id = ?",
      rusqlite::params![result.id],
      |r| r.get(0),
    )
    .expect("tags row");
  assert!(
    tags.contains("signal:num:120"),
    "P2.2c: tags 应含 signal:num:120, got {tags}"
  );
  assert!(tags.contains("signal:date:2026-07-12"));
  assert!(tags.contains("signal:update:改为"));
  assert!(tags.contains("occurred:2026-07-12"));

  let ctx: Value =
    memory_context(&engine.pool, None, None, &ns, Some("120 吨"), 5, true, 8, 8, None)
      .expect("context");
  let row = &ctx["recall"].as_array().expect("recall")[0];
  let nums = row["text_signals"]["numbers"].as_array().expect("numbers");
  assert!(
    nums.iter().any(|n| n.as_str() == Some("120")),
    "ledger 应从 tags 读回 numbers={:?}",
    nums
  );
}
