//! P0-3 验收：memory_profile 合成视图（static + dynamic）+ insight 排除。
//!
//! 运行：`cargo test --test p0_profile`
//!
//! 覆盖（对应 DESIGN §5 / §10.1 / §12.1）：
//! 1. static：来自 preference + hard_rule 标签。
//! 2. dynamic：来自 decision/fact/pattern 且当前 tip；排除 insight。
//! 3. is_latest_applied=true。

use memoria_core::MemoriaEngine;
use memoria_core::tools::profile::memory_profile;
use serde_json::Value;

#[test]
fn profile_returns_static_and_dynamic_excluding_insight() {
    let dir = std::env::temp_dir().join(format!("p0_profile_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/profile";

    // static 源：preference + hard_rule
    let _ = memoria_core::tools::remember::remember_with_dedup(
        &engine.pool,
        "称呼用户为老大，默认简体中文",
        "preference",
        5,
        "test",
        ns,
        "[\"hard_rule\"]",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("pref");

    // dynamic 源：decision
    let _ = memoria_core::tools::remember::remember_with_dedup(
        &engine.pool,
        "Memoria 定位为薄存储，脑子在 agent-core",
        "decision",
        4,
        "test",
        ns,
        "[\"decision\"]",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("decision");

    // insight 不应进入 dynamic
    let _ = memoria_core::tools::remember::remember_with_dedup(
        &engine.pool,
        "自动归纳：入厂量周末偏低",
        "pattern",
        3,
        "test",
        ns,
        "[\"insight\"]",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("insight");

    let v: Value = memory_profile(&engine.pool, ns, 12, 15, None).expect("profile");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["is_latest_applied"], true);

    let static_arr = v["static"].as_array().expect("static array");
    assert!(
        static_arr.iter().any(|e| e["content"].as_str().unwrap_or("").contains("老大")),
        "static 应包含 hard_rule 偏好"
    );

    let dynamic_arr = v["dynamic"].as_array().expect("dynamic array");
    assert!(
        dynamic_arr
            .iter()
            .any(|e| e["content"].as_str().unwrap_or("").contains("薄存储")),
        "dynamic 应包含 decision"
    );
    assert!(
        !dynamic_arr
            .iter()
            .any(|e| e["content"].as_str().unwrap_or("").contains("入厂量周末偏低")),
        "dynamic 应排除 insight 记忆"
    );

    // prompt 文本块非空
    assert!(
        v["static_text"].as_str().unwrap_or("").contains("稳定偏好"),
        "static_text 应含标题"
    );
    assert!(
        v["dynamic_text"].as_str().unwrap_or("").contains("近期动态"),
        "dynamic_text 应含标题"
    );
}
