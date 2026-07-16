//! Phase A / O1–O6 验收
//!   O1: P0 无 mention 时 entities=[]（P1 JOIN 见 p1_hms_phase_b）
//!   O2/O3: occurred 自 tags `occurred:YYYY-MM-DD`；不以 event_time 写入为主路径
//!   O6: ledger 仅 memory_context；search_v2 默认不 enrich
//!   Self-Evolution 纯函数可测；O4 完成依据在 agent-core
//!
//! 运行：`cargo test --test p0_hms_absorption`

use memoria_core::tools::ledger::{enrich_ledger, parse_occurred_tag};
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::tools::self_evolution::guardrails;
use memoria_core::MemoriaEngine;
use serde_json::Value;

fn fresh_engine(tag: &str) -> (MemoriaEngine, String) {
    let dir = std::env::temp_dir().join(format!("p0_hms_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    (engine, "agent/hms".to_string())
}

#[test]
fn occurred_from_tags_distinct_from_mentioned() {
    let (engine, ns) = fresh_engine("tag");
    let _ = remember_with_dedup(
        &engine.pool,
        "项目 Alpha 于 2024-03-01 启动",
        "fact",
        3,
        "test",
        &ns,
        r#"["pilot","occurred:2024-03-01"]"#,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");

    let ctx: Value =
        memory_context(&engine.pool, None, None, &ns, Some("Alpha 启动"), 3, true, 12, 15, None)
            .expect("context");
    let recall = ctx["recall"].as_array().expect("recall array");
    assert!(!recall.is_empty(), "recall 不应为空");
    let row = &recall[0];
    assert_eq!(row["occurred"].as_str().unwrap_or(""), "2024-03-01");
    assert_ne!(row["mentioned"].as_str().unwrap_or(""), "2024-03-01");
    assert_eq!(row["type"].as_str().unwrap_or(""), "fact");
    assert!(row["source_ref"]
        .as_str()
        .unwrap_or("")
        .starts_with(&format!("{}:", ns)));
    // O1
    assert_eq!(row["entities"].as_array().map(|a| a.len()).unwrap_or(1), 0);
    // O6: ledger 字段存在且与 recall 同构
    assert!(ctx["ledger"].is_array());
    // 不应依赖 guardrails 完成 O4
    assert!(ctx.get("guardrails").is_none() || ctx["guardrails"].as_array().map(|a| a.is_empty()).unwrap_or(true));
}

#[test]
fn entities_empty_without_mentions() {
    // Phase A 语义保留：无 entity_mentions 时 entities=[]；有 mention 的 JOIN 见 Phase B 测试
    let (engine, ns) = fresh_engine("ent");
    let _ = remember_with_dedup(
        &engine.pool,
        "实体硬空验收事实",
        "fact",
        3,
        "test",
        &ns,
        r#"["occurred:2025-01-15"]"#,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");
    let fused = memoria_core::search::hybrid::hybrid_search(
        &engine.pool,
        "实体硬空",
        &ns,
        5,
        None,
        None,
        None,
        false,
    )
    .expect("search");
    let ledger = enrich_ledger(&engine.pool, &ns, &fused);
    assert!(!ledger.is_empty());
    assert_eq!(
        ledger[0]["entities"].as_array().map(|a| a.len()).unwrap_or(99),
        0
    );
    assert_eq!(ledger[0]["occurred"].as_str().unwrap_or(""), "2025-01-15");
}

#[test]
fn occurred_falls_back_to_valid_from_without_tag() {
    let (engine, ns) = fresh_engine("fallback");
    let _ = remember_with_dedup(
        &engine.pool,
        "未带 occurred tag 的普通事实",
        "decision",
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

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        &ns,
        Some("普通事实"),
        3,
        true,
        12,
        15,
        None,
    )
    .expect("context");
    let row = &ctx["recall"].as_array().expect("recall")[0];
    assert_eq!(
        row["occurred"].as_str().unwrap_or(""),
        row["mentioned"].as_str().unwrap_or("")
    );
}

#[test]
fn parse_occurred_tag_unit() {
    assert_eq!(
        parse_occurred_tag(r#"["x","occurred:2026-07-16"]"#).as_deref(),
        Some("2026-07-16")
    );
}

#[test]
fn self_evolution_pure_fn_still_works() {
    let g1 = guardrails("how many 项目参与了试点？");
    assert!(g1.iter().any(|s| s.starts_with("COUNT_TOTAL_DEDUP")));
    let g2 = guardrails("上个月的总量是多少");
    assert!(g2.iter().any(|s| s.starts_with("RELATIVE_DATE_GROUNDING")));
    let g3 = guardrails("A 和 B 的差额是多少？");
    assert!(g3
        .iter()
        .any(|s| s.starts_with("AMOUNT_DIFFERENCE_CALIBRATION")));
    let g4 = guardrails("当前的最新状态是什么？");
    assert!(g4
        .iter()
        .any(|s| s.starts_with("CURRENT_PREVIOUS_ARBITRATION")));
    assert!(guardrails("你好").is_empty());
}

#[test]
fn prompt_block_compatible() {
    let (engine, ns) = fresh_engine("pb");
    let _ = remember_with_dedup(
        &engine.pool,
        "上海分部上周新增 3 条产线",
        "fact",
        3,
        "test",
        &ns,
        r#"["occurred:2026-07-06"]"#,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");
    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        &ns,
        Some("上周新增了几条产线？"),
        3,
        true,
        12,
        15,
        None,
    )
    .expect("context");
    assert!(ctx["prompt_block"]
        .as_str()
        .unwrap_or("")
        .contains("上海分部"));
}
