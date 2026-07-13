//! P2-4 导入 / 导出 / 迁移 验收测试
use memoria_core::tools::imp_exp::{build_migration_manifest, export_ns, import_ns, OnConflict};
use memoria_core::MemoriaEngine;
use rusqlite::params;
use std::path::PathBuf;

fn temp_engine(tag: &str) -> (MemoriaEngine, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "memoria_ie_{}_{}_{}",
        std::process::id(),
        tag,
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    (engine, db)
}

fn seed_acme(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO memories(id,namespace,content,category,importance,confidence) VALUES(?1,?2,?3,?4,?5,?6)",
        params!["m1", "acme", "Memoria 是独立记忆中心", "fact", 5, 0.9],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories(id,namespace,content) VALUES(?1,?2,?3)",
        params!["m2", "acme", "第二记忆"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entities(id,namespace,entity_type,name,aliases,summary) VALUES(?1,?2,?3,?4,?5,?6)",
        params!["e1", "acme", "system", "Memoria", "[\"记忆中心\"]", "独立记忆引擎"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entity_mentions(entity_id,memory_id,context,namespace) VALUES(?1,?2,?3,?4)",
        params!["e1", "m1", "该研究基于 Memoria 架构", "acme"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entity_edges(namespace,source_entity_id,target_entity_id,relation_type,weight) VALUES(?1,?2,?3,?4,?5)",
        params!["acme", "e1", "e1", "related_to", 1.0],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_relations(namespace,source_id,target_id,relation_type,weight) VALUES(?1,?2,?3,?4,?5)",
        params!["acme", "m1", "m2", "semantic_related", 0.7],
    )
    .unwrap();
}

fn count_all(conn: &rusqlite::Connection, ns: &str) -> (u64, u64, u64, u64, u64) {
    let m: u64 = conn.query_row("SELECT COUNT(*) FROM memories WHERE namespace=?1", [ns], |r| r.get(0)).unwrap();
    let e: u64 = conn.query_row("SELECT COUNT(*) FROM entities WHERE namespace=?1", [ns], |r| r.get(0)).unwrap();
    let em: u64 = conn.query_row("SELECT COUNT(*) FROM entity_mentions WHERE namespace=?1", [ns], |r| r.get(0)).unwrap();
    let ee: u64 = conn.query_row("SELECT COUNT(*) FROM entity_edges WHERE namespace=?1", [ns], |r| r.get(0)).unwrap();
    let mr: u64 = conn.query_row("SELECT COUNT(*) FROM memory_relations WHERE namespace=?1", [ns], |r| r.get(0)).unwrap();
    (m, e, em, ee, mr)
}

#[test]
fn export_import_round_trip() {
    let (e1, _db1) = temp_engine("rt1");
    seed_acme(&e1.pool.get().unwrap());

    let jsonl = export_ns(&e1.pool, "acme", false).unwrap();
    // 头部版本正确
    let header: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(header["memoria_export"], 1);

    let (e2, _db2) = temp_engine("rt2");
    let r = import_ns(&e2.pool, "acme", &jsonl, OnConflict::Ignore).unwrap();
    if !r.errors.is_empty() || r.inserted != 6 {
        panic!("ROUND-TRIP report: inserted={} ignored={} errors={:?} per_table={:?}", r.inserted, r.ignored, r.errors, r.per_table);
    }
    assert_eq!(r.ignored, 0);

    let (m, e, em, ee, mr) = count_all(&e2.pool.get().unwrap(), "acme");
    assert_eq!((m, e, em, ee, mr), (2, 1, 1, 1, 1));
}

#[test]
fn streaming_chunked_no_oom_and_restores() {
    let (e1, _db1) = temp_engine("stream1");
    let conn = e1.pool.get().unwrap();
    let n: usize = 1200; // > CHUNK(500)，必须分块
    for i in 0..n {
        conn.execute(
            "INSERT INTO memories(id,namespace,content) VALUES(?1,?2,?3)",
            params![format!("m{}", i), "acme", format!("content {}", i)],
        )
        .unwrap();
    }
    let jsonl = export_ns(&e1.pool, "acme", false).unwrap();
    // 行数 = 头部 1 + 1200 数据
    let line_count = jsonl.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(line_count, n + 1);

    let (e2, _db2) = temp_engine("stream2");
    let r = import_ns(&e2.pool, "acme", &jsonl, OnConflict::Ignore).unwrap();
    assert_eq!(r.inserted, n as u64);
    let cnt: u64 = e2.pool.get().unwrap().query_row("SELECT COUNT(*) FROM memories WHERE namespace='acme'", [], |r| r.get(0)).unwrap();
    assert_eq!(cnt, n as u64);
}

#[test]
fn import_conflict_ignore_is_idempotent() {
    let (e1, _db1) = temp_engine("conf1");
    seed_acme(&e1.pool.get().unwrap());
    let jsonl = export_ns(&e1.pool, "acme", false).unwrap();

    let (e2, _db2) = temp_engine("conf2");
    let r1 = import_ns(&e2.pool, "acme", &jsonl, OnConflict::Ignore).unwrap();
    assert_eq!(r1.inserted, 6);
    let r2 = import_ns(&e2.pool, "acme", &jsonl, OnConflict::Ignore).unwrap();
    assert_eq!(r2.inserted, 0);
    assert_eq!(r2.ignored, 6, "重复导入应全部 ignored，不翻倍");
    let (m, e, em, ee, mr) = count_all(&e2.pool.get().unwrap(), "acme");
    assert_eq!((m, e, em, ee, mr), (2, 1, 1, 1, 1));
}

#[test]
fn migration_manifest_has_checksums() {
    let (e1, db1) = temp_engine("manifest1");
    seed_acme(&e1.pool.get().unwrap());
    // 确保数据落盘
    e1.pool.get().unwrap().execute_batch("PRAGMA wal_checkpoint(FULL)").unwrap();

    let manifest = build_migration_manifest(&e1.pool, db1.to_str().unwrap(), "nonexistent_hnsw_path")
        .unwrap();
    assert_eq!(manifest["memoria_migration_bundle"], 1);
    assert!(!manifest["db"]["sha256"].as_str().unwrap().is_empty(), "DB 校验和应非空");
    assert!(manifest["db"]["size_bytes"].as_u64().unwrap() > 0);
    assert!(manifest["hnsw"]["sha256"].as_str().unwrap().is_empty(), "不存在的 HNSW 校验和为空");
    // 全表行数存在
    assert_eq!(manifest["row_counts"]["memories"], 2);
    assert_eq!(manifest["row_counts"]["entities"], 1);
}
