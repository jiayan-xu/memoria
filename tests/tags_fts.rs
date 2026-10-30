//! memories_fts 索引 tags 的回归测试（2026-10-30）。
//!
//! 背景：老库 FTS 只有 content/namespace，tags 查询在关键词通道结构上不可达，
//! 守护把「标签检索能力缺失」误报成召回回归。本测试钉死三件事：
//!   1. 新库：仅出现在 tags 中的词能被 keyword_search 命中；
//!   2. 同步：tags UPDATE 后旧标签失效、新标签命中；
//!   3. 迁移：老库（content/namespace 两列 FTS）经 init_core_tables 后自动重建并生效。

use memoria_core::search::keyword::keyword_search;
use memoria_core::storage::{SqlitePool, create_pool, init_core_tables, init_schema};
use memoria_core::tools::remember::remember;

const NS: &str = "test/tags-fts";

/// 临时库 bootstrap：与 eval.rs 同款（进程唯一文件名，避免并发冲突）。
fn bootstrap(tag: &str) -> SqlitePool {
    let db = std::env::temp_dir().join(format!("memoria_{}_{}.db", tag, std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");
    pool
}

fn new_memory(pool: &SqlitePool, content: &str, tags: &str) -> String {
    remember(pool, content, "fact", 3, "test", NS, tags, None, None).expect("remember")
}

#[test]
fn tags_are_searchable_in_new_schema() {
    let pool = bootstrap("tags_fts_new");
    let id = new_memory(&pool, "正文完全不包含该标签词", r#"["silverneedle"]"#);

    let ids: Vec<String> = keyword_search(&pool, "silverneedle", NS, 10)
        .expect("keyword_search")
        .into_iter()
        .map(|r| r.memory_id)
        .collect();
    assert!(
        ids.contains(&id),
        "tags 中的词应能被关键词通道命中，实际结果: {:?}",
        ids
    );
}

#[test]
fn tag_update_resyncs_fts_index() {
    let pool = bootstrap("tags_fts_update");
    let id = new_memory(&pool, "正文完全不包含任何标签词", r#"["oldneedle"]"#);

    pool.get()
        .unwrap()
        .execute(
            "UPDATE memories SET tags = ? WHERE id = ?",
            rusqlite::params![r#"["newneedle"]"#, &id],
        )
        .expect("update tags");

    let old_hit = keyword_search(&pool, "oldneedle", NS, 10)
        .expect("keyword_search")
        .into_iter()
        .any(|r| r.memory_id == id);
    let new_hit = keyword_search(&pool, "newneedle", NS, 10)
        .expect("keyword_search")
        .into_iter()
        .any(|r| r.memory_id == id);
    assert!(!old_hit, "UPDATE 后旧 tags 不应仍可命中");
    assert!(new_hit, "UPDATE 后新 tags 应可命中");
}

#[test]
fn legacy_fts_is_migrated_to_include_tags() {
    let pool = bootstrap("tags_fts_legacy");
    let id = new_memory(&pool, "正文完全不包含该标签词", r#"["legacyneedle"]"#);

    // 降级成老库结构：两列 FTS + 旧触发器（模拟 2026-10-30 之前的线上库）
    pool.get()
        .unwrap()
        .execute_batch(
            "DROP TRIGGER IF EXISTS mem_ai;
             DROP TRIGGER IF EXISTS mem_ad;
             DROP TRIGGER IF EXISTS mem_au;
             DROP TABLE IF EXISTS memories_fts;
             CREATE VIRTUAL TABLE memories_fts USING fts5(
                 content, namespace,
                 content='memories',
                 content_rowid='rowid'
             );
             CREATE TRIGGER mem_ai AFTER INSERT ON memories BEGIN
                 INSERT INTO memories_fts(rowid, content, namespace)
                 VALUES (new.rowid, new.content, new.namespace);
             END;",
        )
        .expect("downgrade to legacy fts");
    let before = keyword_search(&pool, "legacyneedle", NS, 10)
        .expect("keyword_search")
        .into_iter()
        .any(|r| r.memory_id == id);
    assert!(!before, "老结构上 tags 本就不可达（回归前提）");

    init_core_tables(&pool).expect("migration via init_core_tables");

    let has_tags: i64 = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories_fts') WHERE name = 'tags'",
            [],
            |r| r.get(0),
        )
        .expect("pragma_table_info");
    assert_eq!(has_tags, 1, "迁移后 memories_fts 应含 tags 列");

    let after = keyword_search(&pool, "legacyneedle", NS, 10)
        .expect("keyword_search")
        .into_iter()
        .any(|r| r.memory_id == id);
    assert!(after, "迁移后 tags 查询应能命中");
}
