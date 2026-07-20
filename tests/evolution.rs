//! PR4（Phase A 演化）验收：记忆演化写回 + 回滚 + 检索脏标记。
//!
//! 运行：`cargo test --test evolution`
//!
//! 覆盖（对应执行单 §6）：
//! 1. evolve_memory 写入 `evolved_context`/`evolved_at` 并记 `evolution_log`（old_value 可回滚）。
//! 2. evolution_rollback 按 `evolution_log.old_value` 恢复（H5：不 DROP 列，可逆）。
//! 3. link_count=None 时自动统计 `memory_relations` 关联边数。
//! 4. 未演化记忆（evolved_at IS NULL）在 hybrid_search 标 `pending_evolution=true`；演化后置 false。

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::evolve::{evolve_memory, evolution_rollback};
use memoria_core::tools::remember::remember_with_dedup;
use rusqlite::params;

/// 用与 p0_supersede 一致的 17 参签名插入一条记忆，返回其 id。
fn remember(pool: &memoria_core::storage::SqlitePool, content: &str, ns: &str) -> String {
    remember_with_dedup(
        pool, content, "fact", 3, "test", ns, "[]", None, None, None, None, None, None, None,
        None, None, None,
    )
    .expect("remember")
    .id
}

fn memory_row(
    pool: &memoria_core::storage::SqlitePool,
    id: &str,
) -> (Option<String>, Option<String>, Option<i64>) {
    let conn = pool.get().unwrap();
    conn.query_row(
        "SELECT evolved_context, evolved_at, link_count FROM memories WHERE id = ?",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

#[test]
fn evolve_writes_evolved_at_and_evolution_log() {
    let dir = std::env::temp_dir().join(format!("pr4_evolve_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/pr4_evolve";
    let id = remember(&engine.pool, "用户每周三上午开会", ns);

    let r = evolve_memory(
        &engine.pool,
        &id,
        ns,
        "用户固定周三上午有例会，需提前准备议程",
        None,
        "test-model",
        "context_update",
    )
    .expect("evolve");
    assert_eq!(r["status"].as_str(), Some("evolved"));
    let log_id = r["log_id"].as_str().expect("log_id").to_string();

    // memories 列已写
    let (ctx, at, links) = memory_row(&engine.pool, &id);
    assert_eq!(ctx.as_deref(), Some("用户固定周三上午有例会，需提前准备议程"));
    assert!(at.is_some(), "evolved_at 必须非空");
    assert_eq!(links, Some(0), "无关联边时 link_count=0");

    // evolution_log 记录正确
    let conn = engine.pool.get().unwrap();
    let (ct, old_v, new_v, model): (String, String, String, String) = conn
        .query_row(
            "SELECT change_type, old_value, new_value, model FROM evolution_log WHERE id = ?",
            params![log_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(ct, "context_update");
    assert_eq!(model, "test-model");
    // 首演化的 old_value：evolved_context=null, link_count=null
    assert!(
        old_v.contains("\"evolved_context\":null") && old_v.contains("\"link_count\":null"),
        "old_value 应记录演化前状态: {}",
        old_v
    );
    assert!(
        new_v.contains("用户固定周三上午有例会"),
        "new_value 应记录演化后状态: {}",
        new_v
    );
}

#[test]
fn rollback_restores_old_value() {
    let dir = std::env::temp_dir().join(format!("pr4_rollback_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/pr4_rollback";
    let id = remember(&engine.pool, "服务器在 A 机房", ns);

    // 首次演化：evolved_context = ctx1（old 为 None）
    let r1 = evolve_memory(
        &engine.pool,
        &id,
        ns,
        "ctx1: 服务器位于 A 机房（北京）",
        None,
        "m",
        "context_update",
    )
    .expect("evolve1");
    let log_id = r1["log_id"].as_str().expect("log_id").to_string();
    assert_eq!(memory_row(&engine.pool, &id).0.as_deref(), Some("ctx1: 服务器位于 A 机房（北京）"));

    // 回滚：恢复 old_value（首演化前 evolved_context=None）
    let rb = evolution_rollback(&engine.pool, &log_id).expect("rollback");
    assert_eq!(rb["status"].as_str(), Some("rolled_back"));
    let (ctx_after, at_after, _): (Option<String>, Option<String>, Option<i64>) =
        memory_row(&engine.pool, &id);
    assert!(ctx_after.is_none(), "回滚后应恢复为演化前状态（None）");
    // 设计：保留 evolved_at（仍视为「已处理」，非待演化）
    assert!(at_after.is_some(), "evolved_at 应保留（仍标记已处理）");

    // 回滚不存在的 log_id → noop（不 panic）
    let rb2 = evolution_rollback(&engine.pool, "ev-nonexistent").expect("rollback noop");
    assert_eq!(rb2["status"].as_str(), Some("noop"));
}

#[test]
fn evolve_autocounts_link_count_from_relations() {
    let dir = std::env::temp_dir().join(format!("pr4_links_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/pr4_links";
    let id = remember(&engine.pool, "项目 X 依赖模块 Y", ns);

    // 手动加 2 条关联边
    let conn = engine.pool.get().unwrap();
    for i in 0..2 {
        conn.execute(
            "INSERT INTO memory_relations(source_id, target_id, relation_type, weight, namespace) \
             VALUES(?1, ?2, 'updates', 0.9, ?3)",
            params![id, format!("dep_{}", i), ns],
        )
        .unwrap();
    }

    // link_count=None → 应自动统计为 2
    let r = evolve_memory(
        &engine.pool,
        &id,
        ns,
        "项目 X 依赖模块 Y，关联 2 个子模块",
        None,
        "m",
        "links_update",
    )
    .expect("evolve");
    assert_eq!(r["link_count"].as_i64(), Some(2));
    assert_eq!(memory_row(&engine.pool, &id).2, Some(2));

    // 显式给 link_count=5 应覆盖自动统计
    let r2 = evolve_memory(
        &engine.pool,
        &id,
        ns,
        "项目 X 依赖模块 Y（含外部依赖）",
        Some(5),
        "m",
        "links_update",
    )
    .expect("evolve2");
    assert_eq!(r2["link_count"].as_i64(), Some(5));
    assert_eq!(memory_row(&engine.pool, &id).2, Some(5));
}

#[test]
fn pending_evolution_flag_annotated_in_search() {
    let dir = std::env::temp_dir().join(format!("pr4_pending_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    let ns = "agent/pr4_pending";
    let id = remember(&engine.pool, "可演化的关键事实：公司主营固废处理", ns);

    // 未演化：检索应标 pending_evolution=true
    let before = hybrid_search(&engine.pool, "固废处理", ns, 10, None, None, None, false).unwrap();
    let hit = before.iter().find(|r| r.memory_id == id).expect("should find memory");
    assert!(hit.pending_evolution, "未演化记忆应标 pending_evolution=true");
    assert!(hit.evolved_at.is_none(), "未演化记忆 evolved_at 应为 NULL");

    // 演化后：检索应标 pending_evolution=false
    evolve_memory(
        &engine.pool,
        &id,
        ns,
        "公司已从单一固废处理扩展至热电联产与移动供热",
        None,
        "m",
        "context_update",
    )
    .expect("evolve");
    let after = hybrid_search(&engine.pool, "固废处理", ns, 10, None, None, None, false).unwrap();
    let hit2 = after.iter().find(|r| r.memory_id == id).expect("should find memory after evolve");
    assert!(!hit2.pending_evolution, "演化后记忆应标 pending_evolution=false");
    assert!(hit2.evolved_at.is_some(), "演化后 evolved_at 应非空");
}
