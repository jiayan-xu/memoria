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

use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::MemoriaEngine;

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
        &engine.pool, "公司总部在北京", "fact", 5, "test", ns, "[]",
        None, None,
        Some("2025-01-01T00:00:00"), Some("2025-12-31T23:59:59"),
    ).expect("m1 北京");
    let _v2 = remember_with_dedup(
        &engine.pool, "公司总部迁至上海", "fact", 5, "test", ns, "[]",
        None, None,
        Some("2026-01-01T00:00:00"), None,
    ).expect("m2 上海");

    // 2025 年中：仅北京有效
    let r_2025 = hybrid_search(&engine.pool, "总部", ns, 10, None, None, Some("2025-06-01T00:00:00")).unwrap();
    let c_2025: Vec<String> = r_2025.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_2025, "北京"), "2025 应命中北京版");
    assert!(!has(&c_2025, "上海"), "2025 不应命中上海版（尚未生效）");

    // 2026 年中：仅上海有效（北京已于 2025 年底失效）
    let r_2026 = hybrid_search(&engine.pool, "总部", ns, 10, None, None, Some("2026-06-01T00:00:00")).unwrap();
    let c_2026: Vec<String> = r_2026.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_2026, "上海"), "2026 应命中上海版");
    assert!(!has(&c_2026, "北京"), "2026 北京版已失效，不应命中");

    // as_of=now（2026-07-12）：北京版 valid_to 已过 → 仅上海
    let r_now = hybrid_search(&engine.pool, "总部", ns, 10, None, None, Some("2026-07-12T00:00:00")).unwrap();
    let c_now: Vec<String> = r_now.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_now, "上海"), "now 应命中上海版");
    assert!(!has(&c_now, "北京"), "now 北京版已失效，不应命中");

    // 早于任何版本（2024）：两者均未生效
    let r_before = hybrid_search(&engine.pool, "总部", ns, 10, None, None, Some("2024-01-01T00:00:00")).unwrap();
    assert!(r_before.is_empty(), "2024 早于任何版本的 valid_from，应无结果");

    // None = 不过滤（库函数纯向后兼容）：两版均返回
    let r_all = hybrid_search(&engine.pool, "总部", ns, 10, None, None, None).unwrap();
    let c_all: Vec<String> = r_all.iter().map(|r| r.content.clone()).collect();
    assert!(has(&c_all, "北京"), "None 不应过滤，应看到北京版");
    assert!(has(&c_all, "上海"), "None 不应过滤，应看到上海版");
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
        &engine.pool, "长期有效的偏好 X", "preference", 3, "test", ns, "[]",
        None, None,
        Some("2020-01-01T00:00:00"), None,  // valid_from 远早于测试，valid_to 开放
    ).expect("m");

    // as_of=now 命中；as_of 远未来也仍命中（valid_to 开放）
    let now = hybrid_search(&engine.pool, "长期有效", ns, 10, None, None, Some("2026-07-12T00:00:00")).unwrap();
    assert!(has(&now.iter().map(|r| r.content.clone()).collect::<Vec<_>>(), "长期有效"), "valid_from 早于 as_of 应在 now 命中");

    let future = hybrid_search(&engine.pool, "长期有效", ns, 10, None, None, Some("2076-01-01T00:00:00")).unwrap();
    assert!(has(&future.iter().map(|r| r.content.clone()).collect::<Vec<_>>(), "长期有效"), "valid_to 开放版应在未来命中");
}
