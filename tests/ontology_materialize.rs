//! Open Ontologies 形态 B 骨架 — 端到端集成测试。
//!
//! 验证：物化（load schema+数据 → reason → save）→ 解析推断边 → 写回 entity_edges 完整闭环。
//!
//! 运行：`cargo test --test ontology_materialize`
//! 前置：OPEN_ONTOLOGIES_BIN 指向 open-ontologies 可执行文件；缺失时测试自动跳过。
//!
//! 硬约束（报告 §8）：离线物化，写回 entity_edges（受控枚举门禁），不动热路径。

use memoria_core::ontology::{materialize, write_back_edges, OntologyConfig};
use rusqlite::params;

/// 测试用最小 schema（含 supersedes 传递属性声明）。
const SCHEMA_TTL: &str = r#"@prefix : <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
:supersedes a owl:ObjectProperty, owl:TransitiveProperty .
:createdBy a owl:ObjectProperty .
"#;

/// 测试用数据：docB supersedes docA, docC supersedes docB → 应推断 docC supersedes docA。
const DATA_TTL: &str = r#"@prefix : <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
:docA :createdBy :alice .
:docB :createdBy :alice ; :supersedes :docA .
:docC :createdBy :alice ; :supersedes :docB .
"#;

/// 定位 open-ontologies 二进制；缺失则 None（测试跳过）。
/// 仅当显式设置了 OPEN_ONTOLOGIES_BIN 或二进制确定在 PATH 上时才返回 Some，
/// 避免在无二进制的环境（如 CI runner）误判后 spawn 失败。
fn find_bin() -> Option<String> {
    if let Ok(b) = std::env::var("OPEN_ONTOLOGIES_BIN") {
        if !b.is_empty() {
            return Some(b);
        }
    }
    // 用 `which` 探测 PATH（Windows 上 where）；找不到则视为无二进制 → 跳过
    let locator = if cfg!(windows) { "where" } else { "which" };
    let hit = std::process::Command::new(locator)
        .arg("open-ontologies")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    hit
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("memoria_onto_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn materialize_infers_transitive_supersedes() {
    let bin = match find_bin() {
        Some(b) => b,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let dir = temp_dir("materialize");
    let schema = dir.join("schema.ttl");
    let data = dir.join("data.ttl");
    std::fs::write(&schema, SCHEMA_TTL).unwrap();
    std::fs::write(&data, DATA_TTL).unwrap();

    let cfg = OntologyConfig {
        bin: bin.into(),
        data_dir: dir.join("out"),
        schema_path: schema,
        timeout_secs: 60,
    };

    let res = materialize(&cfg, data.to_str().unwrap(), "owl-rl").expect("materialize");
    // OWL TransitiveProperty：docC supersedes docA 应被推断
    assert!(res.triples_after > res.triples_before, "expected inference");
    assert!(
        res.inferred_edges
            .iter()
            .any(|(s, _p, o)| s.ends_with("docC") && o.ends_with("docA")),
        "expected docC supersedes docA in inferred_edges, got {:?}",
        res.inferred_edges
    );
    println!("inferred_edges: {:?}", res.inferred_edges);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_back_edges_upserts_into_entity_edges() {
    // 不依赖 open-ontologies 二进制，直接测写回逻辑。
    let dir = temp_dir("writeback");
    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";

    // 模拟物化推断出的边（docC supersedes docA, docA supersedes docB）
    let edges = vec![
        (
            "http://memoria.ai/onto/docC".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docA".to_string(),
        ),
        (
            "http://memoria.ai/onto/docA".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docB".to_string(),
        ),
        // 未知关系应被跳过
        (
            "http://memoria.ai/onto/docC".to_string(),
            "http://memoria.ai/onto/banana".to_string(),
            "http://memoria.ai/onto/docA".to_string(),
        ),
    ];

    let (written, skipped) = write_back_edges(&conn, ns, &edges).expect("write_back");
    assert_eq!(written, 2, "exactly 2 supersedes edges written");
    assert_eq!(skipped, 1, "1 unknown banana edge skipped");

    // 幂等：重复写回不重复
    let (w2, _) = write_back_edges(&conn, ns, &edges[..2]).expect("write_back again");
    assert_eq!(w2, 2);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges WHERE namespace=?1 AND relation_type='supersedes'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "idempotent upsert, no duplicates");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 完整闭环：物化 → 写回 entity_edges，验证推断边真正进库。
#[test]
fn end_to_end_materialize_and_writeback() {
    let bin = match find_bin() {
        Some(b) => b,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let dir = temp_dir("e2e");
    let schema = dir.join("schema.ttl");
    let data = dir.join("data.ttl");
    std::fs::write(&schema, SCHEMA_TTL).unwrap();
    std::fs::write(&data, DATA_TTL).unwrap();

    let cfg = OntologyConfig {
        bin: bin.into(),
        data_dir: dir.join("out"),
        schema_path: schema,
        timeout_secs: 60,
    };
    let res = materialize(&cfg, data.to_str().unwrap(), "owl-rl").expect("materialize");

    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";
    let (written, _) = write_back_edges(&conn, ns, &res.inferred_edges).expect("write_back");
    assert!(written >= 1, "at least 1 inferred edge written");

    // 验证推断边 docC supersedes docA 在库里
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges WHERE namespace=?1 AND relation_type='supersedes'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert!(found >= 1, "inferred supersedes edge persisted in entity_edges");
    println!("end-to-end: written={written}, persisted supersedes edges={found}");
    let _ = std::fs::remove_dir_all(&dir);
}