//! P1-2 验收：写入 3 条偏好（hard_rule / pref / style）后 memory_user_prefs 非空，
//! 且按约定聚合：共 3 条、hard_rule 排首、标签合法、tag 过滤生效。
//!
//! 运行：`cargo test --test user_prefs`

use memoria_core::storage::{create_pool, init_core_tables, init_schema, SqlitePool};
use memoria_core::tools::prefs::user_prefs;
use memoria_core::tools::remember::remember;

const PREF_TAGS: &[&str] = &["hard_rule", "pref", "style"];

#[test]
fn user_prefs_nonempty_after_writes() {
    let db = std::env::temp_dir().join(format!("memoria_prefs_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");
    let ns = "agent/p1_2_test";

    // 写 3 条偏好，分布三种标签；hard_rule 的 importance 最低也应排最前
    remember(&pool, "绝不使用 rm -rf 删除用户个人文件", "preference", 5, "test", ns, "[\"hard_rule\"]").unwrap();
    remember(&pool, "中文回复优先于英文", "preference", 4, "test", ns, "[\"pref\"]").unwrap();
    remember(&pool, "代码注释使用英文", "preference", 3, "test", ns, "[\"style\"]").unwrap();

    let prefs = user_prefs(&pool, ns).expect("user_prefs");
    assert!(!prefs.is_empty(), "写入 3 条偏好后工具必须非空");
    assert_eq!(prefs.len(), 3, "应聚合 3 条偏好");

    // hard_rule 必须排在最前（优先级最高）
    assert_eq!(prefs[0].tag, "hard_rule", "hard_rule 必须排在最前");

    // 全部命中偏好标签约定
    for p in &prefs {
        assert!(PREF_TAGS.contains(&p.tag.as_str()), "tag 必须是偏好约定之一: {}", p.tag);
    }

    // tag 过滤：仅 hard_rule
    let only_hard: Vec<_> = prefs.iter().filter(|p| p.tag == "hard_rule").collect();
    assert_eq!(only_hard.len(), 1, "hard_rule 过滤应剩 1 条");

    // tag 过滤：仅 style
    let only_style: Vec<_> = prefs.iter().filter(|p| p.tag == "style").collect();
    assert_eq!(only_style.len(), 1, "style 过滤应剩 1 条");
}

#[test]
fn user_prefs_empty_for_unknown_ns() {
    let db = std::env::temp_dir().join(format!("memoria_prefs_empty_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");

    // 未写入任何偏好的 ns → 必须返回空（不再空壳报错）
    let prefs = user_prefs(&pool, "agent/never_written").expect("user_prefs");
    assert!(prefs.is_empty(), "未写入偏好的 ns 应返回空");
}
