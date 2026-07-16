//! P0+ 验收：吸收 HMS 的三项改造
//!   1. event_time 双轨时间（memories.event_time 写入 + 召回 occurred/mentioned 区分）
//!   2. memory_context / memory_search_v2 类型化证据账本（type/occurred/mentioned/source_ref/entities）
//!   3. Self-Evolution 护栏（关键词触发确定性控制注记）
//!
//! 运行：`cargo test --test p0_hms_absorption`

use memoria_core::MemoriaEngine;
use memoria_core::tools::ledger::enrich_ledger;
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::{remember_with_dedup, set_event_time};
use memoria_core::tools::self_evolution::guardrails;
use serde_json::Value;

fn fresh_engine(tag: &str) -> (MemoriaEngine, String) {
    let dir = std::env::temp_dir().join(format!("p0_hms_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    (engine, "agent/hms".to_string())
}

#[test]
fn event_time_written_and_distinct_from_valid_from() {
    let (engine, ns) = fresh_engine("event");
    let result = remember_with_dedup(
        &engine.pool,
        "项目 Alpha 于 2024-03-01 启动",
        "fact",
        3,
        "test",
        &ns,
        "[]",
        None,
        None,
        None, // valid_from → now（断言时刻）
        None,
        None,
        None,
    )
    .expect("remember");
    // P0+ 吸收 HMS: 事件发生时点（与 valid_from 断言时刻区分），独立 UPDATE
    set_event_time(&engine.pool, &result.id, "2024-03-01T00:00:00").expect("set event_time");

    let ctx: Value = memory_context(&engine.pool, None, None, &ns, Some("Alpha 启动"), 3, true, 12, 15, None)
        .expect("context");
    let recall = ctx["recall"].as_array().expect("recall array");
    assert!(!recall.is_empty(), "recall 不应为空");
    let row = &recall[0];
    // occurred = event_time（事件发生），mentioned = valid_from（断言时刻，≈ now）
    assert_eq!(row["occurred"].as_str().unwrap_or(""), "2024-03-01T00:00:00");
    assert_ne!(row["mentioned"].as_str().unwrap_or(""), "2024-03-01T00:00:00");
    // 类型化账本字段齐备
    assert_eq!(row["type"].as_str().unwrap_or(""), "fact");
    assert!(row["source_ref"].as_str().unwrap_or("").starts_with(&format!("{}:", ns)));
    assert_eq!(
        row["source_ref"].as_str().unwrap_or(""),
        format!("{}:{}", ns, row["memory_id"].as_str().unwrap_or(""))
    );
}

#[test]
fn ledger_falls_back_occurred_to_valid_from_when_event_time_absent() {
    let (engine, ns) = fresh_engine("fallback");
    let _ = remember_with_dedup(
        &engine.pool,
        "未带 event_time 的普通事实",
        "decision",
        3,
        "test",
        &ns,
        "[]",
        None,
        None,
        None, // 无 valid_from（→ now），无 event_time
        None,
        None,
        None,
    )
    .expect("remember");

    let ctx: Value = memory_context(&engine.pool, None, None, &ns, Some("普通事实"), 3, true, 12, 15, None)
        .expect("context");
    let recall = ctx["recall"].as_array().expect("recall array");
    let row = &recall[0];
    // 缺 event_time 时 occurred 以 valid_from 兜底（二者皆 = now）
    assert_eq!(
        row["occurred"].as_str().unwrap_or(""),
        row["mentioned"].as_str().unwrap_or("")
    );
    assert_eq!(row["type"].as_str().unwrap_or(""), "decision");
}

#[test]
fn enrich_ledger_emits_typed_fields() {
    let (engine, ns) = fresh_engine("enrich");
    let result = remember_with_dedup(
        &engine.pool,
        "北京总部成立于 2020 年",
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
    set_event_time(&engine.pool, &result.id, "2020-01-01T00:00:00").expect("set event_time");
    let fused = memoria_core::search::hybrid::hybrid_search(
        &engine.pool,
        "北京总部",
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
    let row = &ledger[0];
    assert_eq!(row["occurred"].as_str().unwrap_or(""), "2020-01-01T00:00:00");
    assert_eq!(row["type"].as_str().unwrap_or(""), "fact");
    assert!(row["source_ref"].as_str().unwrap_or("").starts_with(&format!("{}:", ns)));
}

#[test]
fn self_evolution_guardrails_trigger_on_keywords() {
    // 计数类
    let g1 = guardrails("how many 项目参与了试点？");
    assert!(g1.iter().any(|s| s.starts_with("COUNT_TOTAL_DEDUP")));
    // 相对日期
    let g2 = guardrails("上个月的总量是多少");
    assert!(g2.iter().any(|s| s.starts_with("RELATIVE_DATE_GROUNDING")));
    // 金额差值
    let g3 = guardrails("A 和 B 的差额是多少？");
    assert!(g3.iter().any(|s| s.starts_with("AMOUNT_DIFFERENCE_CALIBRATION")));
    // 当前/历史态
    let g4 = guardrails("当前的最新状态是什么？");
    assert!(g4.iter().any(|s| s.starts_with("CURRENT_PREVIOUS_ARBITRATION")));
    // 无触发
    let g0 = guardrails("你好");
    assert!(g0.is_empty());
}

#[test]
fn context_includes_guardrails_field() {
    let (engine, ns) = fresh_engine("gfield");
    let result = remember_with_dedup(
        &engine.pool,
        "上海分部上周新增 3 条产线",
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
    set_event_time(&engine.pool, &result.id, "2026-07-06T00:00:00").expect("set event_time");
    let ctx: Value = memory_context(&engine.pool, None, None, &ns, Some("上周新增了几条产线？"), 3, true, 12, 15, None)
        .expect("context");
    assert!(
        ctx["guardrails"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "含关键词的问题应触发 guardrails"
    );
    // prompt_block 仍向后兼容（含 content）
    assert!(ctx["prompt_block"].as_str().unwrap_or("").contains("上海分部"));
}
