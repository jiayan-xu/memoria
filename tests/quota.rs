//! P2-2 验收 + 回归：配额与滥用防护（quota & abuse protection）。
//!
//! 运行：`cargo test --test quota`
//!
//! 覆盖：
//! 1. 配额表幂等初始化（MemoriaEngine::new 自带）。
//! 2. 写入配额（日桶）限额生效：超限硬拒绝。
//! 3. 搜索配额（分钟桶）同桶累加，超限拒绝。
//! 4. 备份配额（小时桶）+ current_usage 计数可见。
//! 5. quota_limit 默认值与 quota_window 格式正确（运维可预期）。

use memoria_core::MemoriaEngine;
use memoria_core::quota::{
    KIND_BACKUP, KIND_SEARCH, KIND_WRITE, QuotaError, check_quota_with, current_usage, quota_limit,
    quota_window,
};

fn temp_engine(tag: &str) -> (MemoriaEngine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("memoria_quota_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    (engine, db)
}

#[test]
fn init_quota_table_idempotent() {
    let (engine, db) = temp_engine("init");
    // MemoriaEngine::new 已建表；再建一次必须幂等
    assert!(memoria_core::quota::init_quota_table(&engine.pool).is_ok());
    // 表可查询
    assert_eq!(current_usage(&engine.pool, "any", KIND_WRITE), 0);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn write_quota_enforces_limit() {
    let (engine, db) = temp_engine("write");
    let ns = "agent/quota_write";
    // 显式 limit=3：前 3 次放行，第 4 次拒绝（日桶不会在测试期间滚动）
    assert!(check_quota_with(&engine.pool, ns, KIND_WRITE, 3).is_ok());
    assert!(check_quota_with(&engine.pool, ns, KIND_WRITE, 3).is_ok());
    assert!(check_quota_with(&engine.pool, ns, KIND_WRITE, 3).is_ok());
    let err = check_quota_with(&engine.pool, ns, KIND_WRITE, 3).unwrap_err();
    assert_eq!(err.kind, KIND_WRITE);
    // current_usage 反映已用 3
    assert_eq!(current_usage(&engine.pool, ns, KIND_WRITE), 3);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn search_quota_same_minute_bucket() {
    let (engine, db) = temp_engine("search");
    let ns = "agent/quota_search";
    // limit=2：同分钟内第 3 次拒绝（测试通常在 1 分钟内完成）
    assert!(check_quota_with(&engine.pool, ns, KIND_SEARCH, 2).is_ok());
    assert!(check_quota_with(&engine.pool, ns, KIND_SEARCH, 2).is_ok());
    let err = check_quota_with(&engine.pool, ns, KIND_SEARCH, 2).unwrap_err();
    assert_eq!(err.kind, KIND_SEARCH);
    assert_eq!(current_usage(&engine.pool, ns, KIND_SEARCH), 2);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn backup_quota_hourly_enforces() {
    let (engine, db) = temp_engine("backup");
    let ns = "agent/quota_backup";
    // limit=1：第 2 次拒绝（小时桶）
    assert!(check_quota_with(&engine.pool, ns, KIND_BACKUP, 1).is_ok());
    let err = check_quota_with(&engine.pool, ns, KIND_BACKUP, 1).unwrap_err();
    assert_eq!(err.kind, KIND_BACKUP);
    assert_eq!(current_usage(&engine.pool, ns, KIND_BACKUP), 1);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn zero_limit_denies_everything() {
    let (engine, db) = temp_engine("zero");
    let ns = "agent/quota_zero";
    let err = check_quota_with(&engine.pool, ns, KIND_WRITE, 0).unwrap_err();
    assert_eq!(err.limit, 0);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn defaults_and_window_format() {
    // 不设 env 时返回既定默认值
    assert_eq!(quota_limit(KIND_WRITE), 10_000);
    assert_eq!(quota_limit(KIND_SEARCH), 600);
    assert_eq!(quota_limit(KIND_BACKUP), 12);
    // 时间桶格式形态
    assert_eq!(quota_window(KIND_WRITE).len(), 10); // YYYY-MM-DD
    assert_eq!(quota_window(KIND_SEARCH).len(), 16); // YYYY-MM-DDTHH:MM
    assert_eq!(quota_window(KIND_BACKUP).len(), 13); // YYYY-MM-DDTHH
}
