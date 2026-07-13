//! P2-3 验收 + 回归：实体图谱增强（受控关系枚举 + mention 上下文搜索 + 证据导出）。
//!
//! 运行：`cargo test --test entity_graph`
//!
//! 覆盖：
//! 1. 关系类型受控枚举：合法通过 / 非法拒绝 / 列表可见。
//! 2. entity_search 命中 mention context（实体名不含关键词，但提及上下文含）。
//! 3. entity_search 返回每实体 mentions_count。
//! 4. export_graph 节点附带 mentions 证据（memory_id + context），支持 UI 下钻。

use memoria_core::MemoriaEngine;
use memoria_core::tools::graph::{
    export_graph, is_valid_relation_type, relation_type_list, search_entities,
};
use rusqlite::params;

fn temp_engine(tag: &str) -> (MemoriaEngine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("memoria_entity_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    (engine, db)
}

/// entity_mentions.memory_id 引用 memories(id)，测试需先 seed 对应记忆以满足 FK。
fn seed_memory(conn: &rusqlite::Connection, id: &str, ns: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO memories(id, namespace) VALUES(?1, ?2)",
        params![id, ns],
    )
    .unwrap();
}

#[test]
fn relation_type_enum_enforced() {
    // 合法类型通过
    assert!(is_valid_relation_type("uses"));
    assert!(is_valid_relation_type("related_to"));
    assert!(is_valid_relation_type("triggers"));
    assert!(is_valid_relation_type("collaborates_with"));
    // 非法类型拒绝（防止关系爆炸 / 垃圾关系）
    assert!(!is_valid_relation_type("banana"));
    assert!(!is_valid_relation_type(""));
    assert!(!is_valid_relation_type("owns_everything_illegal"));
    // 允许列表对所有合法类型可见（用于错误提示与 schema）
    let list = relation_type_list();
    assert!(list.contains("uses"));
    assert!(list.contains("mentions"));
    assert!(list.contains("triggers"));
    assert!(!list.contains("banana"));
}

#[test]
fn entity_search_finds_by_mention_context() {
    let (engine, db) = temp_engine("mention_ctx");
    let ns = "agent/eg";
    let conn = engine.pool.get().unwrap();
    // 实体名不含 "区块链"，但某条记忆的提及上下文含
    conn.execute(
        "INSERT INTO entities(id,namespace,entity_type,name,aliases,summary) VALUES(?1,?2,'concept',?3,'[]',NULL)",
        params!["e1", ns, "量子计算实验室"],
    ).unwrap();
    seed_memory(&conn, "m1", ns);
    conn.execute(
        "INSERT INTO entity_mentions(entity_id,memory_id,context,namespace) VALUES(?1,?2,?3,?4)",
        params!["e1", "m1", "该研究基于区块链共识机制", ns],
    )
    .unwrap();
    // 关键词命中 mention context → 仍应召回该实体
    let rows = search_entities(&engine.pool, ns, "区块链", None, 20).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"].as_str(), Some("量子计算实验室"));
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn entity_search_returns_mentions_count() {
    let (engine, db) = temp_engine("mention_cnt");
    let ns = "agent/eg";
    let conn = engine.pool.get().unwrap();
    conn.execute(
        "INSERT INTO entities(id,namespace,entity_type,name,aliases,summary) VALUES(?1,?2,'person',?3,'[]',NULL)",
        params!["e2", ns, "张三"],
    ).unwrap();
    for i in 1..=3 {
        let mid = format!("m{}", i);
        seed_memory(&conn, &mid, ns);
        conn.execute(
            "INSERT INTO entity_mentions(entity_id,memory_id,context,namespace) VALUES(?1,?2,?3,?4)",
            params!["e2", mid, format!("ctx{}", i), ns],
        ).unwrap();
    }
    let rows = search_entities(&engine.pool, ns, "张三", None, 20).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["mentions_count"].as_i64(), Some(3));
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn export_graph_includes_mentions() {
    let (engine, db) = temp_engine("export_mentions");
    let ns = "agent/eg";
    let conn = engine.pool.get().unwrap();
    conn.execute(
        "INSERT INTO entities(id,namespace,entity_type,name,aliases,summary) VALUES(?1,?2,'org',?3,'[]',NULL)",
        params!["e3", ns, "OpenAI"],
    ).unwrap();
    seed_memory(&conn, "memA", ns);
    conn.execute(
        "INSERT INTO entity_mentions(entity_id,memory_id,context,namespace) VALUES(?1,?2,?3,?4)",
        params!["e3", "memA", "发布了 GPT 系列模型", ns],
    )
    .unwrap();
    let g = export_graph(&engine.pool, ns).unwrap();
    let nodes = g["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1);
    // 节点携带 mention 证据，UI 可下钻到记忆
    let mentions = nodes[0]["mentions"].as_array().expect("mentions array");
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0]["memory_id"].as_str(), Some("memA"));
    assert_eq!(mentions[0]["context"].as_str(), Some("发布了 GPT 系列模型"));
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}
