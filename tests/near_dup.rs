//! P1-3 验收 + 回归：近义去重可靠性（embedding 持久化 + 重启重建 HNSW）。
//!
//! 运行：`cargo test --test near_dup`
//!
//! 覆盖：
//! 1. 重启后连续写入近义句仍进入同一 supersede 链（验收）。
//! 2. 低于阈值不合并（阈值可配生效）。
//! 3. 跨 namespace 不合并（NS 隔离回归）。

use memoria_core::MemoriaEngine;
use memoria_core::storage::SqlitePool;
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::vector::DIM;

/// 构造单位向量：v = a·e0 + b·e1（前 2 维），其余为 0 → |v| = sqrt(a²+b²)。
/// 两向量余弦 = a1·a2 + b1·b2（当均单位长时）。
fn unit_vec(a: f32, b: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = a;
    v[1] = b;
    v
}

fn superseded_by(pool: &SqlitePool, id: &str) -> Option<String> {
    let conn = pool.get().ok()?;
    conn.query_row(
        "SELECT superseded_by FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

#[test]
fn near_dup_survives_restart() {
    let dir = std::env::temp_dir().join(format!("memoria_nd_restart_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let ns = "agent/nd1";

    // ── Session 1：写入「句子A」并持久化其向量 ──
    let engine1 = MemoriaEngine::new(db.to_str().unwrap()).expect("engine1");
    let v_a = unit_vec(1.0, 0.0); // |v|=1
    engine1.cache_query_vector("句子A", v_a.clone());
    let r1 = remember_with_dedup(
        &engine1.pool,
        "句子A",
        "fact",
        5,
        "test",
        ns,
        "[]",
        Some(&engine1.hnsw),
        Some(&engine1.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember A");
    let id_a = r1.id.clone();
    // 确认向量已落持久表（重启可用）
    assert!(
        memoria_core::vector::persist::get_stored_vector(&engine1.pool, &id_a).is_some(),
        "向量必须持久化到 memory_vectors"
    );

    // ── 模拟重启：丢弃进程内 QueryCache/HNSW，从持久表重建 ──
    // 共享父目录下本测试独占子目录，天然隔离 .bin，避免陈旧向量索引掩盖 rebuild 逻辑。
    drop(engine1);
    let engine2 = MemoriaEngine::new(db.to_str().unwrap()).expect("engine2 (restart)");
    assert!(
        engine2.hnsw.len() >= 1,
        "重启后 HNSW 应已从 memory_vectors 重建出向量"
    );

    // ── Session 2：写入近义句 B（与 A 余弦 0.99 > 0.92）──
    let v_b = unit_vec(0.99, 0.141); // 0.99²+0.141²≈1 → 单位；cos(A,B)=0.99
    engine2.cache_query_vector("句子A的近义表述", v_b.clone());
    let r2 = remember_with_dedup(
        &engine2.pool,
        "句子A的近义表述",
        "fact",
        5,
        "test",
        ns,
        "[]",
        Some(&engine2.hnsw),
        Some(&engine2.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember B");

    // 验收：B 应被标记为 A 的近义 supersede（superseded_by 记在旧记忆 A 上：A.superseded_by = B）
    assert_eq!(
        r2.action, "superseded_near_dup",
        "重启后近义句必须进入同一 supersede 链"
    );
    assert_eq!(
        r2.superseded_ids,
        vec![id_a.clone()],
        "B 的 superseded 目标必须是 A"
    );
    assert_eq!(
        superseded_by(&engine2.pool, &id_a),
        Some(r2.id.clone()),
        "memories.superseded_by 必须等于 B（记在旧记忆 A 上）"
    );
}

#[test]
fn near_dup_below_threshold_not_merged() {
    let dir = std::env::temp_dir().join(format!("memoria_nd_thresh_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let ns = "agent/nd2";
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");

    let v_a = unit_vec(1.0, 0.0);
    engine.cache_query_vector("概念甲", v_a.clone());
    let r1 = remember_with_dedup(
        &engine.pool,
        "概念甲",
        "fact",
        5,
        "test",
        ns,
        "[]",
        Some(&engine.hnsw),
        Some(&engine.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember A");

    // B 与 A 余弦 0.5（远低于 0.92 默认阈值）→ 不应合并
    let v_b = unit_vec(0.5, 0.866); // 0.5²+0.866²=1 → 单位；cos=0.5
    engine.cache_query_vector("概念乙", v_b.clone());
    let r2 = remember_with_dedup(
        &engine.pool,
        "概念乙",
        "fact",
        5,
        "test",
        ns,
        "[]",
        Some(&engine.hnsw),
        Some(&engine.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember B");

    assert_eq!(r2.action, "created", "低于阈值的近义不应合并");
    assert!(r2.superseded_ids.is_empty(), "superseded_ids 必须为空");
    assert_eq!(
        superseded_by(&engine.pool, &r2.id),
        None,
        "B.superseded_by 必须为 NULL"
    );
    assert_eq!(superseded_by(&engine.pool, &r1.id), None, "A 不受影响");
}

#[test]
fn near_dup_respects_namespace() {
    let dir = std::env::temp_dir().join(format!("memoria_nd_ns_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");

    let v = unit_vec(1.0, 0.0);
    // ns1 写入 A（向量 v）
    engine.cache_query_vector("跨ns-A", v.clone());
    let r1 = remember_with_dedup(
        &engine.pool,
        "跨ns-A",
        "fact",
        5,
        "test",
        "agent/ns1",
        "[]",
        Some(&engine.hnsw),
        Some(&engine.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember A");

    // ns2 写入 B，向量与 A 完全相同（余弦 1.0），但不同 ns → 不应被 A supersede
    engine.cache_query_vector("跨ns-B", v.clone());
    let r2 = remember_with_dedup(
        &engine.pool,
        "跨ns-B",
        "fact",
        5,
        "test",
        "agent/ns2",
        "[]",
        Some(&engine.hnsw),
        Some(&engine.query_cache),
        None,
        None,
        None,
        None,
    )
    .expect("remember B");

    assert_eq!(
        r2.action, "created",
        "跨 ns 的近义句不应被其它 ns 的记忆 supersede"
    );
    assert!(r2.superseded_ids.is_empty(), "跨 ns 不得合并");
    assert_eq!(
        superseded_by(&engine.pool, &r2.id),
        None,
        "B.superseded_by 必须为 NULL（NS 隔离）"
    );
    assert_eq!(superseded_by(&engine.pool, &r1.id), None, "A 不受影响");
}
