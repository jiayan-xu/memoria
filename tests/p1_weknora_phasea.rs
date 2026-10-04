//! WeKnora 吸收 Phase A P1 验收：NormalizedKey 主题收敛 + 确认状态机。
//!
//! 运行：`cargo test --test p1_weknora_phasea`
//!
//! 覆盖：
//! 1. topic_key 主题收敛：「生产库用 MySQL」与「生产库已迁到 PostgreSQL」同
//!    normalized key（词序无关），第二条写入后第一条被自动 supersede（读路径
//!    零模型调用）；默认召回只见新 tip。
//! 2. 未传 topic_key → 行为不变（两条共存，不 supersede）。
//! 3. confirm=false 写入 pending：默认召回不可见；include_pending=true 可见。
//! 4. rejected：软删除，include_pending 也不可见。

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::search::hybrid::hybrid_search_reported;
use memoria_core::tools::remember::{remember_with_dedup_ex, WriteExtras};

fn contents(engine: &MemoriaEngine, ns: &str, query: &str, include_pending: bool) -> Vec<String> {
    let (fused, _report) = hybrid_search_reported(
        &engine.pool,
        query,
        ns,
        10,
        None,
        None,
        None,
        None,
        false,
        include_pending,
    )
    .expect("hybrid_search_reported");
    fused.into_iter().map(|r| r.content).collect()
}

#[test]
fn topic_key_supersedes_same_subject() {
    let dir = std::env::temp_dir().join(format!("memoria_p1wk_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/p1wk";

    let r1 = remember_with_dedup_ex(
        &engine.pool,
        "生产库用 MySQL",
        "fact",
        5,
        "test",
        ns,
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
        WriteExtras {
            topic_key: Some("生产数据库"),
            confirm: None,
        },
    )
    .expect("m1");
    assert_eq!(r1.action, "created");

    // 同主题不同值（「生产库 数据库」同 token 集合——词序无关）
    let r2 = remember_with_dedup_ex(
        &engine.pool,
        "生产库已迁到 PostgreSQL",
        "fact",
        5,
        "test",
        ns,
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
        WriteExtras {
            topic_key: Some("数据库 生产"),
            confirm: None,
        },
    )
    .expect("m2");
    assert_eq!(r2.action, "superseded_topic_key", "同 key 应自动取代");
    assert_eq!(r2.superseded_ids, vec![r1.id.clone()]);

    // 默认召回：仅见新 tip
    let got = contents(&engine, ns, "生产库", false);
    assert!(
        got.iter().any(|c| c.contains("PostgreSQL")),
        "新 tip 应可见: {got:?}"
    );
    assert!(
        !got.iter().any(|c| c.contains("MySQL") && !c.contains("PostgreSQL")),
        "旧值不应作为 tip 可见: {got:?}"
    );
}

#[test]
fn no_topic_key_behavior_unchanged() {
    let dir = std::env::temp_dir().join(format!("memoria_p1wk2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/p1wk2";

    // 未传 topic_key：同主题矛盾共存（默认路径不受 P1 影响）
    for content in ["固废清运用 A 车", "固废清运改用 B 车"] {
        remember_with_dedup_ex(
            &engine.pool,
            content,
            "fact",
            3,
            "test",
            ns,
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
            WriteExtras::default(),
        )
        .expect("write");
    }
    let conn = engine.pool.get().unwrap();
    let tips: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ? AND superseded_by IS NULL",
            rusqlite::params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tips, 2, "无 topic_key 不应触发 supersede");
}

#[test]
fn confirm_state_machine_filters_recall() {
    let dir = std::env::temp_dir().join(format!("memoria_p1wk3_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/p1wk3";

    // pending 写入 + 正常写入
    let rp = remember_with_dedup_ex(
        &engine.pool,
        "待确认观察：机房湿度疑似超标",
        "observation",
        3,
        "test",
        ns,
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
        WriteExtras {
            topic_key: None,
            confirm: Some(false),
        },
    )
    .expect("pending write");
    remember_with_dedup_ex(
        &engine.pool,
        "已确认事实：机房巡检按周执行",
        "fact",
        3,
        "test",
        ns,
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
        WriteExtras::default(),
    )
    .expect("active write");

    // 默认：pending 不可见
    let got = contents(&engine, ns, "机房", false);
    assert!(!got.iter().any(|c| c.contains("湿度")), "pending 默认不可见: {got:?}");
    assert!(got.iter().any(|c| c.contains("巡检")), "active 应可见: {got:?}");

    // include_pending=true：pending 可见
    let (fused_p, report_p) = hybrid_search_reported(
        &engine.pool,
        "湿度",
        ns,
        10,
        None,
        None,
        None,
        None,
        false,
        true,
    )
    .unwrap();
    assert!(
        fused_p.iter().any(|r| r.content.contains("湿度")),
        "include_pending=true 应可见 pending: {:?}",
        fused_p.iter().map(|r| r.content.clone()).collect::<Vec<_>>()
    );
    assert!(
        !report_p.dropped.iter().any(|d| d.starts_with("pending_filtered:")),
        "include_pending=true 时不应有 pending_filtered 计数: {:?}",
        report_p.dropped
    );

    // memory_reject 语义：置 rejected 后 include_pending 也不可见
    let conn = engine.pool.get().unwrap();
    conn.execute(
        "UPDATE memories SET confirm_status = 'rejected' WHERE id = ?",
        rusqlite::params![rp.id],
    )
    .unwrap();
    let got = contents(&engine, ns, "湿度", true);
    assert!(
        !got.iter().any(|c| c.contains("湿度")),
        "rejected 即使 include_pending 也不可见: {got:?}"
    );

    // 兼容包装（老签名）仍工作
    let _legacy = hybrid_search(&engine.pool, "机房", ns, 5, None, None, None, None, false);
}
