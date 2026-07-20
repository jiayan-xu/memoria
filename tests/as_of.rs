//! P1-5 验收：轻量时序真值（as_of）。
//!
//! 运行：`cargo test --test as_of`
//!
//! 覆盖：
//! 1. 同 subject 前后矛盾两条记忆，as_of 分别命中正确版本（北京版 / 上海版）。
//! 2. 默认 as_of=now：已失效（valid_to 过去）的版本不返回。
//! 3. as_of 早于任何 valid_from：无结果。
//!
//! 关键：写入路径 `remember_with_dedup(..., valid_from, valid_to)` 落 temporal 区间；
//! 查询路径 `hybrid_search(..., as_of)` 据此过滤「当时有效」的记忆。

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::remember::remember_with_dedup;

fn has(contents: &[String], needle: &str) -> bool {
    contents.iter().any(|c| c.contains(needle))
}

#[test]
fn as_of_resolves_contradictory_memories() {
    let dir = std::env::temp_dir().join(format!("memoria_asof_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/asof";

    // 矛盾两版：北京（2025 年内有效）→ 上海（2026 起长期有效）
    let _v1 = remember_with_dedup(
        &engine.pool,
        "公司总部在北京",
        "fact",
        5,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2025-01-01T00:00:00"),
        Some("2025-12-31T23:59:59"),
        None,
        None,
    
        None,
        None,
        None,
        None)
    .expect("m1 北京");
    let _v2 = remember_with_dedup(
        &engine.pool,
        "公司总部迁至上海",
        "fact",
        5,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2026-01-01T00:00:00"),
        None,
        None,
        None,
    
        None,
        None,
        None,
        None)
    .expect("m2 上海");

    // 2025 年中：仅北京有效
    let r_2025 = hybrid_search(
        &engine.pool,
        "总部",
        ns,
        10,
        None,
        None,
        Some("2025-06-01T00:00:00"),
        false,
    )
    .unwrap();
    let c_2025: Vec<String> = r_2025.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_2025, "北京"), "2025 应命中北京版");
    assert!(!has(&c_2025, "上海"), "2025 不应命中上海版（尚未生效）");

    // 2026 年中：仅上海有效（北京已于 2025 年底失效）
    let r_2026 = hybrid_search(
        &engine.pool,
        "总部",
        ns,
        10,
        None,
        None,
        Some("2026-06-01T00:00:00"),
        false,
    )
    .unwrap();
    let c_2026: Vec<String> = r_2026.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_2026, "上海"), "2026 应命中上海版");
    assert!(!has(&c_2026, "北京"), "2026 北京版已失效，不应命中");

    // as_of=now（2026-07-12）：北京版 valid_to 已过 → 仅上海
    let r_now = hybrid_search(
        &engine.pool,
        "总部",
        ns,
        10,
        None,
        None,
        Some("2026-07-12T00:00:00"),
        false,
    )
    .unwrap();
    let c_now: Vec<String> = r_now.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_now, "上海"), "now 应命中上海版");
    assert!(!has(&c_now, "北京"), "now 北京版已失效，不应命中");

    // 早于任何版本（2024）：两者均未生效
    let r_before = hybrid_search(
        &engine.pool,
        "总部",
        ns,
        10,
        None,
        None,
        Some("2024-01-01T00:00:00"),
        false,
    )
    .unwrap();
    assert!(
        r_before.is_empty(),
        "2024 早于任何版本的 valid_from，应无结果"
    );

    // None = is_latest_now（当前有效）：北京版 valid_to 已过 → 仅上海
    let r_now_default =
        hybrid_search(&engine.pool, "总部", ns, 10, None, None, None, false).unwrap();
    let c_now_default: Vec<String> = r_now_default.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_now_default, "上海"), "None=is_latest_now：当前有效应命中上海版");
    assert!(
        !has(&c_now_default, "北京"),
        "None=is_latest_now：北京版已过期，不应命中"
    );

    // include_superseded=true → 跳过整段过滤（含时序真值），两版均返回
    let r_all = hybrid_search(&engine.pool, "总部", ns, 10, None, None, None, true).unwrap();
    let c_all: Vec<String> = r_all.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_all, "北京"), "include_superseded=true 应看到北京版");
    assert!(has(&c_all, "上海"), "include_superseded=true 应看到上海版");
}

/// PR3 双时态补洞 — 统一更新路径（核心验收）。
///
/// 场景：用户 2026-01 在 A 公司（当前真值，valid_to 开放）；2026-07 跳槽到 B 公司，
/// 显式 `supersedes_id=A` 且声明自身 `valid_from=2026-07-01`。
///
/// 关键不变量：旧 tip A 的 `valid_to` 必须关闭在「新事实的生效起点」2026-07-01，
/// 而非墙钟 `now`——否则 `as_of=now` 仍会命中已取代的旧事实（端点闭合导致旧 tip 在
/// `now` 时刻仍“有效”）。这是 PR3 修复的本质 bug。
#[test]
fn supersede_closes_old_valid_to_at_new_valid_from() {
    let dir = std::env::temp_dir().join(format!("memoria_asof_sup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/asof_sup";

    // A：2026-01 起在 A 公司（valid_to 开放 = 当前真值）
    let a = remember_with_dedup(
        &engine.pool,
        "用户 2026 年起在 A 公司工作",
        "fact",
        5,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2026-01-01T00:00:00"),
        None, // valid_to 开放
        None,
        None,
    
        None,
        None,
        None,
        None)
    .expect("A");
    let a_id = a.id.clone();

    // B：2026-07 起跳槽到 B 公司，显式取代 A，声明自身 valid_from
    let b = remember_with_dedup(
        &engine.pool,
        "用户 2026 年 7 月起跳槽到 B 公司工作",
        "fact",
        5,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2026-07-01T00:00:00"),
        None,
        Some(&a_id),
        None,
    
        None,
        None,
        None,
        None)
    .expect("B superseding A");
    assert_eq!(b.action, "superseded_explicit");
    let b_id = b.id.clone();

    // 统一更新路径：旧 tip A 的 valid_to 关闭在 B 的 valid_from（2026-07-01），而非墙钟 now
    let conn = engine.pool.get().unwrap();
    let row: (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT superseded_by, valid_to FROM memories WHERE id = ?",
            rusqlite::params![a_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0.as_deref(), Some(b_id.as_str()), "A.superseded_by 应指向 B");
    assert_eq!(
        row.1.as_deref(),
        Some("2026-07-01T00:00:00"),
        "PR3：旧 tip valid_to 必须关闭在新事实 valid_from（非墙钟 now）"
    );

    // 旧事实仍在库（非 DELETE；时序失效 ≠ 物理删除）
    let cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE id = ?",
            rusqlite::params![a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 1, "旧事实必须保留，不得物理删除");

    // as_of=2026-03（跳槽前）：应命中 A，不命中 B
    let r_before = hybrid_search(
        &engine.pool,
        "工作",
        ns,
        10,
        None,
        None,
        Some("2026-03-01T00:00:00"),
        false,
    )
    .unwrap();
    let c_before: Vec<String> = r_before.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_before, "A 公司"), "as_of=2026-03 应命中 A 公司（当时真值）");
    assert!(
        !has(&c_before, "B 公司"),
        "as_of=2026-03 不应命中 B 公司（尚未生效）"
    );

    // as_of=now（2026-07-20，晚于跳槽）：应命中 B，不命中 A
    let r_now = hybrid_search(
        &engine.pool,
        "工作",
        ns,
        10,
        None,
        None,
        Some("2026-07-20T00:00:00"),
        false,
    )
    .unwrap();
    let c_now: Vec<String> = r_now.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_now, "B 公司"), "as_of=now 应命中 B 公司（当前真值）");
    assert!(
        !has(&c_now, "A 公司"),
        "as_of=now A 公司已失效，不应命中（PR3 边界修复的关键）"
    );

    // include_superseded=true → 两版均可见（历史不被丢弃）
    let r_all = hybrid_search(&engine.pool, "工作", ns, 10, None, None, None, true).unwrap();
    let c_all: Vec<String> = r_all.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_all, "A 公司"), "include_superseded=true 应看到 A 公司");
    assert!(has(&c_all, "B 公司"), "include_superseded=true 应看到 B 公司");
}

#[test]
fn valid_to_open_ended_default() {
    let dir = std::env::temp_dir().join(format!("memoria_asof_open_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let ns = "agent/asof2";

    // 不传 valid_from/valid_to → valid_from=now、valid_to=NULL（长期有效）
    // 但 exact now 可能晚于测试 as_of（2026-07-12T00:00:00），故明确设 valid_from 为过去时间
    let _ = remember_with_dedup(
        &engine.pool,
        "长期有效的偏好 X",
        "preference",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2020-01-01T00:00:00"),
        None, // valid_from 远早于测试，valid_to 开放
        None, // supersedes_id（本测试不涉及显式取代）
        None, // relation
    
        None,
        None,
        None,
        None)
    .expect("m");

    // as_of=now 命中；as_of 远未来也仍命中（valid_to 开放）
    let now = hybrid_search(
        &engine.pool,
        "长期有效",
        ns,
        10,
        None,
        None,
        Some("2026-07-12T00:00:00"),
        false,
    )
    .unwrap();
    assert!(
        has(
            &now.iter().map(|r| r.content.clone()).collect::<Vec<_>>(),
            "长期有效"
        ),
        "valid_from 早于 as_of 应在 now 命中"
    );

    let future = hybrid_search(
        &engine.pool,
        "长期有效",
        ns,
        10,
        None,
        None,
        Some("2076-01-01T00:00:00"),
        false,
    )
    .unwrap();
    assert!(
        has(
            &future.iter().map(|r| r.content.clone()).collect::<Vec<_>>(),
            "长期有效"
        ),
        "valid_to 开放版应在未来命中"
    );
}
