//! P0 验收：显式 supersedes_id + 默认 isLatest 过滤 + 失败模式。
//!
//! 运行：`cargo test --test p0_supersede`
//!
//! 覆盖（对应 DESIGN §10.4 / §12.1）：
//! 1. 显式 supersede：旧行 superseded_by 已设 + valid_to 已 stamp（方案 A）。
//! 2. 默认检索（include_superseded=false）不返回被取代旧记忆；true 返回。
//! 3. 失败模式：404（目标不存在）/ 403（跨 ns）/ 409（非 tip、自指）。

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::remember::remember_with_dedup;
use sha2::{Digest, Sha256};

fn content_id(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

fn contents_of(results: &[memoria_core::search::FusedResult]) -> Vec<String> {
    results.iter().map(|r| r.content.clone()).collect()
}

#[test]
fn explicit_supersede_stamps_valid_to_and_hides_by_default() {
    let dir = std::env::temp_dir().join(format!("p0_sup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/sup";

    let a = remember_with_dedup(
        &engine.pool,
        "用户旧地址在北京",
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
    )
    .expect("remember A");
    let a_id = a.id.clone();

    // 显式取代 A（新内容，新 id）
    let b = remember_with_dedup(
        &engine.pool,
        "用户新地址搬到上海",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a_id),
        None,
    )
    .expect("remember B superseding A");
    assert_eq!(b.action, "superseded_explicit", "应标记 superseded_explicit");

    // 直接 DB 校验：A.superseded_by == B.id 且 valid_to 已 stamp（非 NULL）
    let conn = engine.pool.get().unwrap();
    let row: (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT superseded_by, valid_to FROM memories WHERE id = ?",
            rusqlite::params![a_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0.as_deref(), Some(b.id.as_str()), "A.superseded_by 应指向 B");
    assert!(row.1.is_some(), "方案 A：A.valid_to 必须已 stamp（非 NULL）");

    // 默认检索（include_superseded=false）：不返回旧记忆 A，返回新记忆 B
    let def = hybrid_search(&engine.pool, "地址", ns, 10, None, None, None, false).unwrap();
    let c_def = contents_of(&def);
    assert!(
        c_def.iter().any(|c| c.contains("上海")),
        "默认检索应返回新记忆 B（上海）"
    );
    assert!(
        !c_def.iter().any(|c| c.contains("北京")),
        "默认检索不应返回已被取代的旧记忆 A（北京）"
    );

    // include_superseded=true：旧记忆 A 也应出现（调试/链展开）
    let inc = hybrid_search(&engine.pool, "地址", ns, 10, None, None, None, true).unwrap();
    let c_inc = contents_of(&inc);
    assert!(
        c_inc.iter().any(|c| c.contains("北京")),
        "include_superseded=true 应返回旧记忆 A（北京）"
    );
}

#[test]
fn supersedes_id_404_when_target_missing() {
    let dir = std::env::temp_dir().join(format!("p0_sup_404_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/sup404";
    let r = remember_with_dedup(
        &engine.pool,
        "某记忆",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some("deadbeefdeadbeef"),
        None,
    );
    assert!(r.is_err(), "目标不存在应返回 Err");
    assert!(
        r.unwrap_err().contains("404"),
        "应返回 404 失败码（目标不存在）"
    );
}

#[test]
fn supersedes_id_403_cross_namespace() {
    let dir = std::env::temp_dir().join(format!("p0_sup_403_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns_other = "agent/other";
    let a = remember_with_dedup(
        &engine.pool,
        "其他命名空间的记忆",
        "fact",
        3,
        "test",
        ns_other,
        "[]",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember A in other ns");

    let ns_self = "agent/self";
    let r = remember_with_dedup(
        &engine.pool,
        "跨 ns 取代尝试",
        "fact",
        3,
        "test",
        ns_self,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a.id),
        None,
    );
    assert!(r.is_err(), "跨 ns 取代应返回 Err");
    assert!(
        r.unwrap_err().contains("403"),
        "应返回 403 失败码（跨 namespace）"
    );
}

#[test]
fn supersedes_id_409_when_target_not_tip() {
    let dir = std::env::temp_dir().join(format!("p0_sup_409_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/sup409";

    let a = remember_with_dedup(
        &engine.pool,
        "旧记忆",
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
    )
    .expect("remember A");
    // B 取代 A → A 不再是 tip
    let _b = remember_with_dedup(
        &engine.pool,
        "取代 A 的新记忆",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a.id),
        None,
    )
    .expect("remember B superseding A");

    // 再尝试用 C 取代已非 tip 的 A → 409
    let r = remember_with_dedup(
        &engine.pool,
        "再次取代 A",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a.id),
        None,
    );
    assert!(r.is_err(), "取代非 tip 应返回 Err");
    assert!(
        r.unwrap_err().contains("409"),
        "应返回 409 失败码（目标非 tip）"
    );
}

#[test]
fn supersedes_id_409_self_reference() {
    let dir = std::env::temp_dir().join(format!("p0_sup_self_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/supself";
    let content = "自指取代测试内容";
    let self_id = content_id(content); // 新记忆的 id 即此 content 的哈希
    let r = remember_with_dedup(
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
        Some(&self_id),
        None,
    );
    assert!(r.is_err(), "自指取代应返回 Err");
    assert!(
        r.unwrap_err().contains("409"),
        "应返回 409 失败码（自指）"
    );
}

#[test]
fn stamp_preserves_expired_valid_to() {
    let dir = std::env::temp_dir().join(format!("p0_sup_stamp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/stamp";

    let a = remember_with_dedup(
        &engine.pool,
        "已过期的临时事实",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        Some("2020-01-01T00:00:00"),
        Some("2020-06-01T00:00:00"), // 已过期
        None,
        None,
    )
    .expect("A");

    let b = remember_with_dedup(
        &engine.pool,
        "取代已过期事实的新 tip",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a.id),
        None,
    )
    .expect("B");
    assert_eq!(b.action, "superseded_explicit");

    let conn = engine.pool.get().unwrap();
    let vt: Option<String> = conn
        .query_row(
            "SELECT valid_to FROM memories WHERE id = ?",
            rusqlite::params![a.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        vt.as_deref(),
        Some("2020-06-01T00:00:00"),
        "§9.1: 已过期 valid_to 不得被 now 回拨"
    );
}

#[test]
fn supersede_404_does_not_leave_dirty_tip() {
    let dir = std::env::temp_dir().join(format!("p0_sup_atom_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/atom";
    let content = "原子性失败不应留下的记忆内容 XYZ";
    let r = remember_with_dedup(
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
        Some("deadbeefdeadbeef"),
        None,
    );
    assert!(r.is_err());
    assert!(r.unwrap_err().contains("404"));
    let conn = engine.pool.get().unwrap();
    let cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE content = ?",
            rusqlite::params![content],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 0, "404 失败后不得留下脏 tip");
}

#[test]
fn explicit_supersede_writes_updates_edge() {
    let dir = std::env::temp_dir().join(format!("p0_sup_edge_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/edge";
    let a = remember_with_dedup(
        &engine.pool,
        "旧事实边",
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
    )
    .expect("A");
    let b = remember_with_dedup(
        &engine.pool,
        "新事实边",
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&a.id),
        Some("updates"),
    )
    .expect("B");
    let conn = engine.pool.get().unwrap();
    let rt: String = conn
        .query_row(
            "SELECT relation_type FROM memory_relations WHERE source_id = ? AND target_id = ?",
            rusqlite::params![a.id, b.id],
            |r| r.get(0),
        )
        .expect("updates edge must exist");
    assert_eq!(rt, "updates");
}

#[test]
fn exact_duplicate_still_applies_supersedes_id() {
    let dir = std::env::temp_dir().join(format!("p0_sup_exact_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/exact";
    let content = "同一内容精确重复并取代";
    let first = remember_with_dedup(
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
    )
    .expect("first");
    let old = remember_with_dedup(
        &engine.pool,
        "将被精确重复内容取代的旧 tip",
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
    )
    .expect("old");
    let r = remember_with_dedup(
        &engine.pool,
        content, // exact match → first.id
        "fact",
        3,
        "test",
        ns,
        "[]",
        None,
        None,
        None,
        None,
        Some(&old.id),
        None,
    )
    .expect("exact+supersede");
    assert_eq!(r.action, "superseded_explicit");
    assert_eq!(r.id, first.id);
    let conn = engine.pool.get().unwrap();
    let sup: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = ?",
            rusqlite::params![old.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sup.as_deref(), Some(first.id.as_str()));
}
