//! Phase P2 / M2.1 验收（HMS text_signals 最小切片）
//!   P2.1a: ledger 行含 text_signals（numbers/dates/update_markers）
//!   P2.1b: occurred tag 并入 dates 信号（O3，不写 event_time 列）
//!   P2.1c: hybrid 检索数字/日期 query 与正文重叠加成（O5，无 cross-encoder）
//!   P2.2c 已做: tags 持久化 signal:*（remember 写入 + ledger 读时合并）
//!   P2.2d 已做: agent-core consolidate LLM 抽取 signal tags
//!
//! 运行：`cargo test --test p2_hms_text_signals`

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::remember_with_dedup;
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
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        None,
    )
    .expect("remember");

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("120 吨 应急"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let recall = ctx["recall"].as_array().expect("recall");
    assert!(!recall.is_empty(), "应有 recall");
    let row = &recall[0];
    let ts = row
        .get("text_signals")
        .expect("P2.1a: ledger 应含 text_signals");
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

// #R60：变异 MEMORIA_TEXT_SIGNALS_RERANK 的测试与依赖其默认值的测试共享串行锁
// （见 text_signals_rerank_env_off）。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn search_boosts_on_numeric_query_overlap() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        None,
    )
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
        None,
    )
    .expect("b");
    let mid_a = a.id.clone();

    let fused = hybrid_search(
        &engine.pool,
        "120 吨",
        &ns,
        10,
        None,
        None,
        None,
        None,
        false,
    )
    .expect("search");
    assert!(!fused.is_empty());
    let top = &fused[0];
    assert_eq!(top.memory_id, mid_a, "P2.1c: 数字重叠应抬升含 120 的记忆");
    // #R60 test/low：**只断言通道标记**——`rrf_score > 0.0` 是平凡断言（任何成功
    // 融合结果都有正 RRF，text_signals 零贡献也通过）；source 含 text_signals 仅
    // 在数字/日期 boost >0 实际应用时追加，P2.1c 回归必红。
    assert!(
        top.source.contains(memoria_core::search::text_signals::SOURCE_MARKER),
        "text-signal boost must be applied: source={}",
        top.source
    );
}

#[test]
fn relative_date_in_ledger_signals() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        None,
    )
    .expect("remember");

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("上周三 异常"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let row = &ctx["recall"].as_array().expect("recall")[0];
    let dates = row["text_signals"]["dates"].as_array().expect("dates");
    assert!(
        dates.len() >= 2,
        "P2.2a: 相对日期应解析为绝对日 dates={:?}",
        dates
    );
    assert!(
        dates
            .iter()
            .any(|d| d.as_str().map(|s| s.len() == 10).unwrap_or(false)),
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
        None,
    )
    .expect("mem");

    // #R60 test/medium：env 变异加 **Drop guard 恢复 + 共享串行锁**——`set_var`
    // 变异进程全局状态（edition 2024 中 unsafe 正是为此）：同二进制并行测试可
    // 观察到瞬时的 "0"（flaky，依赖默认值的 search_boosts 会静默跳过 rerank），
    // 断言 panic 时 remove_var 不执行则变量泄漏污染后续测试。guard 的 Drop 在
    // unwind 路径也恢复；ENV_LOCK 为文件级（与 search_boosts 共享，见上）。
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // #R61 bug/medium：**恢复原值而非无条件 remove**——进程启动时若 env 已带预设
    // 值（CI 配置/harness），无条件 remove 会永久抹除、改变后续依赖预设值的行为。
    struct EnvRestore(Option<String>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("MEMORIA_TEXT_SIGNALS_RERANK", v),
                    None => std::env::remove_var("MEMORIA_TEXT_SIGNALS_RERANK"),
                }
            }
        }
    }
    let prev = std::env::var("MEMORIA_TEXT_SIGNALS_RERANK").ok();
    unsafe {
        std::env::set_var("MEMORIA_TEXT_SIGNALS_RERANK", "0");
    }
    let _restore = EnvRestore(prev);
    let fused =
        hybrid_search(&engine.pool, "999", &ns, 5, None, None, None, None, false).expect("s");
    assert!(
        fused.iter().all(|r| !r.source.contains(memoria_core::search::text_signals::SOURCE_MARKER)),
        "关闭 rerank 时不应出现 text_signals 通道标记"
    );
}

#[test]
fn signal_tags_persisted_on_remember() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        None,
    )
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

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("120 吨"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let row = &ctx["recall"].as_array().expect("recall")[0];
    let nums = row["text_signals"]["numbers"].as_array().expect("numbers");
    assert!(
        nums.iter().any(|n| n.as_str() == Some("120")),
        "ledger 应从 tags 读回 numbers={:?}",
        nums
    );
}
