//! P1-4 验收：Dream 巩固流水线可靠化。
//!
//! 运行：`cargo test --test dream`
//!
//! 覆盖：
//! 1. 全量巩固周期：写 observation → fetch_unconsolidated → 写总结记忆 → 推进 cursor →
//!    再 fetch 为空 → cursor 已前进、runs 计数递增、items_out 累加。
//! 2. 跨 ns 隔离：不同 ns 的 cursor 互不影响。
//!
//! 说明：cursor 非空校验与 ns 限流（MCP handler 层）需通过 HTTP 级测试验证，
//! 不在本集成测试范围；`dream_cooldown` 函数已提取为纯函数可单测。

use memoria_core::storage::{SqlitePool, create_pool, init_core_tables, init_schema};
use serde_json::Value;
use std::collections::HashMap;

/// 查找 observation 类别的记忆（模拟 memory_fetch_unconsolidated）
fn fetch_unconsolidated(pool: &SqlitePool, ns: &str, since: &str, limit: i64) -> Vec<Value> {
    let conn = pool.get().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, content, category, created_at FROM memories
             WHERE namespace=?1 AND created_at>?2
               AND (category='observation' OR category IS NULL)
             ORDER BY created_at ASC LIMIT ?3",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![ns, since, limit], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?,
            "content": r.get::<_, Option<String>>(1)?,
            "category": r.get::<_, Option<String>>(2)?,
            "created_at": r.get::<_, Option<String>>(3)?,
        }))
    })
    .unwrap()
    .flatten()
    .collect()
}

/// dream_state_update（直接 SQL，与 MCP handler 同一语义）
fn dream_update(pool: &SqlitePool, phase: &str, ns: &str, cursor_ts: &str, items_out: i64) {
    let conn = pool.get().unwrap();
    conn.execute(
        "INSERT INTO dream_state(phase, namespace, last_run, cursor_ts, runs, items_out, sessions_processed)
         VALUES(?1, ?2, datetime('now'), ?3, 1, ?4, 0)
         ON CONFLICT(phase, namespace) DO UPDATE SET
           last_run=datetime('now'),
           cursor_ts=excluded.cursor_ts,
           runs=dream_state.runs+1,
           items_out=dream_state.items_out+excluded.items_out",
        rusqlite::params![phase, ns, cursor_ts, items_out],
    )
    .unwrap();
}

/// dream_state_get（直接 SQL）
fn dream_get(pool: &SqlitePool, phase: &str, ns: &str) -> HashMap<String, Value> {
    let conn = pool.get().unwrap();
    let row = conn.query_row(
        "SELECT last_run, cursor_ts, runs, items_out, sessions_processed
             FROM dream_state WHERE phase=?1 AND namespace=?2",
        rusqlite::params![phase, ns],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    );
    match row {
        Ok((lr, ct, runs, io, sp)) => {
            let mut m = HashMap::new();
            m.insert(
                "last_run".to_string(),
                Value::String(lr.unwrap_or_default()),
            );
            m.insert(
                "cursor_ts".to_string(),
                Value::String(ct.unwrap_or_else(|| "1970-01-01".into())),
            );
            m.insert("runs".to_string(), Value::Number((runs as i64).into()));
            m.insert("items_out".to_string(), Value::Number((io as i64).into()));
            m.insert(
                "sessions_processed".to_string(),
                Value::Number((sp as i64).into()),
            );
            m
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            let mut m = HashMap::new();
            m.insert("last_run".to_string(), Value::Null);
            m.insert("cursor_ts".to_string(), Value::String("1970-01-01".into()));
            m.insert("runs".to_string(), Value::Number(0.into()));
            m.insert("items_out".to_string(), Value::Number(0.into()));
            m.insert("sessions_processed".to_string(), Value::Number(0.into()));
            m
        }
        Err(e) => panic!("dream_get: {}", e),
    }
}

#[test]
fn full_consolidation_cycle() {
    let db = std::env::temp_dir().join(format!("memoria_dream_cycle_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");
    let ns = "agent/p1_4_cycle";

    // 1. 写入 3 条 observation 到 ns（使用间隔 1s 的显式时间戳保证游标可区隔）
    let conn = pool.get().unwrap();
    let base = chrono::Utc::now();
    let ts1 = base.format("%Y-%m-%dT%H:%M:%S").to_string();
    let ts2 = (base + chrono::Duration::seconds(1))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let ts3 = (base + chrono::Duration::seconds(2))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    for (i, ts) in [(&ts1, "obs_0"), (&ts2, "obs_1"), (&ts3, "obs_2")]
        .iter()
        .enumerate()
    {
        conn.execute(
            "INSERT INTO memories(id,namespace,content,category,created_at,importance)
             VALUES(?1,?2,?3,'observation',?4,1)",
            rusqlite::params![format!("obs_{}", i), ns, format!("观察内容 {}", i), ts.0],
        )
        .unwrap();
    }

    // 2. 首跑：已确认 dream_state 从 epoch 开始
    let ds_before = dream_get(&pool, "consolidate", ns);
    assert_eq!(
        ds_before["cursor_ts"].as_str().unwrap(),
        "1970-01-01",
        "首跑游标应为 epoch"
    );
    assert_eq!(ds_before["runs"].as_i64().unwrap(), 0, "首跑 runs 应为 0");

    // 3. 抓取未巩固 observation
    let batch = fetch_unconsolidated(&pool, ns, "1970-01-01", 200);
    assert!(!batch.is_empty(), "应抓到 observation");
    assert!(batch.len() >= 3, "至少 3 条");

    // 确定最大 created_at（游标）
    let max_ts: String = batch
        .iter()
        .filter_map(|v| v["created_at"].as_str().map(|s| s.to_string()))
        .max()
        .unwrap_or_else(|| "1970-01-01".into());
    assert_ne!(max_ts, "1970-01-01", "必须有时间戳");

    // 4. dream_state_update 推进游标（模拟巩固完成）
    dream_update(&pool, "consolidate", ns, &max_ts, 1);

    let ds_after1 = dream_get(&pool, "consolidate", ns);
    assert_eq!(ds_after1["runs"].as_i64().unwrap(), 1, "runs 应变为 1");
    assert_eq!(
        ds_after1["items_out"].as_i64().unwrap(),
        1,
        "items_out 应为 1"
    );
    assert_eq!(
        ds_after1["cursor_ts"].as_str().unwrap(),
        &max_ts,
        "cursor_ts 应推进到上次最大时间"
    );

    // 5. 再用同一游标抓取 → 应为空（游标已推进、没有新记录）
    let batch2 = fetch_unconsolidated(&pool, ns, &max_ts, 200);
    assert_eq!(batch2.len(), 0, "游标推进后不应再抓到已处理的记录");

    // 6. 二次巩固：模拟写入新 observation 后再抓取（晚于上一批最大 ts3）
    let ts_late = (base + chrono::Duration::seconds(10))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    conn.execute(
        "INSERT INTO memories(id,namespace,content,category,created_at,importance)
         VALUES('obs_late',?1,'晚到的观察','observation',?2,1)",
        rusqlite::params![ns, ts_late],
    )
    .unwrap();

    let batch3 = fetch_unconsolidated(&pool, ns, &max_ts, 200);
    assert!(!batch3.is_empty(), "游标之后的 observation 应被抓到");

    let max_ts3: String = batch3
        .iter()
        .filter_map(|v| v["created_at"].as_str().map(|s| s.to_string()))
        .max()
        .unwrap();
    dream_update(&pool, "consolidate", ns, &max_ts3, 2);

    let ds_after2 = dream_get(&pool, "consolidate", ns);
    assert_eq!(
        ds_after2["runs"].as_i64().unwrap(),
        2,
        "二次巩固后 runs 应为 2"
    );
    assert_eq!(
        ds_after2["items_out"].as_i64().unwrap(),
        3,
        "items_out 应累加为 1+2=3"
    );
    assert_eq!(
        ds_after2["cursor_ts"].as_str().unwrap(),
        &max_ts3,
        "cursor_ts 应推进到新批次最大"
    );

    // 7. 最终游标之后应无数据
    let final_batch = fetch_unconsolidated(&pool, ns, &max_ts3, 200);
    assert_eq!(final_batch.len(), 0, "最终游标之后应为空");
}

#[test]
fn dream_ns_isolation() {
    let db = std::env::temp_dir().join(format!("memoria_dream_ns_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");

    // ns-a：巩固到游标 "2026-01-01T00:00:00"
    dream_update(&pool, "consolidate", "agent/a", "2026-01-01T00:00:00", 5);
    let ds_a = dream_get(&pool, "consolidate", "agent/a");
    assert_eq!(ds_a["runs"].as_i64().unwrap(), 1);

    // ns-b：游标应为 epoch（未巩固过）
    let ds_b = dream_get(&pool, "consolidate", "agent/b");
    assert_eq!(
        ds_b["cursor_ts"].as_str().unwrap(),
        "1970-01-01",
        "未巩固过的 ns 游标应为 epoch"
    );
    assert_eq!(ds_b["runs"].as_i64().unwrap(), 0, "未巩固过的 ns runs 为 0");

    // ns-b 独立巩固
    dream_update(&pool, "consolidate", "agent/b", "2026-02-01T00:00:00", 3);

    // ns-a 不受影响
    let ds_a2 = dream_get(&pool, "consolidate", "agent/a");
    assert_eq!(
        ds_a2["cursor_ts"].as_str().unwrap(),
        "2026-01-01T00:00:00",
        "ns-a 游标不应被 ns-b 污染"
    );

    // ns-b 独立推进
    let ds_b2 = dream_get(&pool, "consolidate", "agent/b");
    assert_eq!(ds_b2["cursor_ts"].as_str().unwrap(), "2026-02-01T00:00:00");
    assert_eq!(ds_b2["runs"].as_i64().unwrap(), 1);
}
