//! Memoria 检索质量评测（Memory Eval）— P1-1
//!
//! 固定语料 + 固定用例，度量真实生产检索路径（hybrid_search，mcp_server 已对齐同一入口）。
//! 指标：召回@k、零结果率、P50/P95 延迟、通道贡献（来自 FusedResult.signal_scores）。
//! 运行：`cargo test --test eval`（CI 发布前必跑）。
//!
//! 说明：CI 无 embedding 后端，故 hnsw/query_cache 传 None，语义通道不参与；
//! 评测覆盖 keyword/temporal/importance/category 四路确定性融合 + 精确去重。
//! 语义通道（近义 supersede）需运行时 embedding 后端，属手动评测范畴。

use memoria_core::search::hybrid::hybrid_search;
use memoria_core::storage::{create_pool, init_core_tables, init_schema, SqlitePool};
use memoria_core::tools::remember::remember;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// 召回下限：任一回归导致召回跌破此值，CI 变红。
const RECALL_FLOOR: f64 = 0.85;

#[test]
fn memory_eval() {
    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/cases");
    let corpus: Vec<Value> = read_json_array(&cases_dir.join("corpus.json"));
    let cases: Vec<Value> = read_json_array(&cases_dir.join("cases.json"));
    assert!(!corpus.is_empty() && !cases.is_empty(), "eval cases 不能为空");

    // fixture DB（临时文件，不入库）
    let db = std::env::temp_dir().join(format!("memoria_eval_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool: SqlitePool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");

    // 插入语料，记录每条返回的 memory id
    let mut ids: Vec<String> = Vec::with_capacity(corpus.len());
    for item in &corpus {
        let content = item["content"].as_str().expect("corpus[].content");
        let category = item["category"].as_str().unwrap_or("fact");
        let importance = item["importance"].as_i64().unwrap_or(3);
        let ns = item["ns"].as_str().unwrap_or("agent/default");
        let tags = item["tags"].as_str().unwrap_or("[]");
        let id = remember(&pool, content, category, importance, "eval", ns, tags, None, None)
            .unwrap_or_else(|e| panic!("remember failed for '{}': {}", content, e));
        // 应用时序偏移（控制 temporal 信号）
        if let Some(off) = item["created_offset_days"].as_i64() {
            if off > 0 {
                let ts = chrono::Utc::now() - chrono::Duration::days(off);
                let _ = pool.get().unwrap().execute(
                    "UPDATE memories SET created_at = ? WHERE id = ?",
                    rusqlite::params![ts.format("%Y-%m-%dT%H:%M:%S").to_string(), &id],
                );
            }
        }
        ids.push(id);
    }

    // 跑用例
    let mut recall_hits = 0u32;
    let mut recall_total = 0u32;
    let mut zero_results = 0u32;
    let mut latencies: Vec<f64> = Vec::new();
    let mut channel_counts: HashMap<String, u32> = HashMap::new();
    let mut failures: Vec<String> = Vec::new();

    for c in &cases {
        let q = c["query"].as_str().expect("cases[].query");
        let ns = c["ns"].as_str().unwrap_or("agent/default");
        let k = c["k"].as_u64().unwrap_or(5) as u32;
        let ctype = c["type"].as_str().unwrap_or("");
        let expect: Vec<usize> = c["expect_indices"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|x| x as usize)).collect())
            .unwrap_or_default();
        let must_not: Vec<usize> = c["must_not_indices"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|x| x as usize)).collect())
            .unwrap_or_default();
        let dedup_pair: Option<(usize, usize)> = c["dedup_pair"].as_array().and_then(|a| {
            if a.len() == 2 {
                Some((a[0].as_u64().unwrap_or(0) as usize, a[1].as_u64().unwrap_or(0) as usize))
            } else {
                None
            }
        });

        let start = Instant::now();
        // 与生产同一路径；CI 无 embedding 后端，hnsw/query_cache 传 None
        let results = hybrid_search(&pool, q, ns, k, None, None, None).unwrap_or_default();
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);

        let result_ids: Vec<&str> = results.iter().map(|r| r.memory_id.as_str()).collect();
        if results.is_empty() {
            zero_results += 1;
        }

        let mut case_ok = true;
        for &e in &expect {
            match ids.get(e) {
                None => {
                    case_ok = false;
                    failures.push(format!("[{}] expect_indices 越界: {}", ctype, e));
                }
                Some(expected_id) => {
                    if result_ids.contains(&expected_id.as_str()) {
                        if let Some(r) = results.iter().find(|r| r.memory_id == *expected_id) {
                            for (ch, _) in &r.signal_scores {
                                *channel_counts.entry(ch.clone()).or_insert(0) += 1;
                            }
                        }
                    } else {
                        case_ok = false;
                        failures.push(format!(
                            "[{}] 期望 idx {} (id={}) 未进入 top-{}，q='{}'",
                            ctype, e, expected_id, k, q
                        ));
                    }
                }
            }
        }
        for &m in &must_not {
            if let Some(forbidden_id) = ids.get(m) {
                if result_ids.contains(&forbidden_id.as_str()) {
                    case_ok = false;
                    failures.push(format!(
                        "[{}] must_not idx {} (id={}) 泄露，q='{}'",
                        ctype, m, forbidden_id, q
                    ));
                }
            }
        }
        if let Some((a, b)) = dedup_pair {
            if let (Some(ia), Some(ib)) = (ids.get(a), ids.get(b)) {
                if ia != ib {
                    case_ok = false;
                    failures.push(format!(
                        "[dedup] idx {} 与 {} 未合并（{} != {}）",
                        a, b, ia, ib
                    ));
                }
            }
        }

        if !expect.is_empty() || !must_not.is_empty() || dedup_pair.is_some() {
            recall_total += 1;
            if case_ok {
                recall_hits += 1;
            }
        }
    }

    // 指标汇总
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let recall = if recall_total > 0 {
        recall_hits as f64 / recall_total as f64
    } else {
        1.0
    };
    let zero_rate = if !cases.is_empty() {
        zero_results as f64 / cases.len() as f64
    } else {
        0.0
    };

    eprintln!("===== Memoria Memory Eval =====");
    eprintln!(
        "cases={} recall@k={:.2} zero_result_rate={:.2}",
        cases.len(),
        recall,
        zero_rate
    );
    eprintln!("latency p50={:.2}ms p95={:.2}ms", p50, p95);
    eprintln!("channel_contribution={:?}", channel_counts);
    if !failures.is_empty() {
        eprintln!("FAILURES:\n{}", failures.join("\n"));
    }
    eprintln!("===============================");

    // 断言（质量门）
    assert!(
        recall >= RECALL_FLOOR,
        "召回@k {:.2} 低于下限 {:.2}",
        recall,
        RECALL_FLOOR
    );
    assert_eq!(zero_rate, 0.0, "存在零结果用例（召回缺口）");
    assert!(failures.is_empty(), "共 {} 个评测失败", failures.len());

    drop(pool);
    let _ = std::fs::remove_file(&db);
}

fn read_json_array(p: &Path) -> Vec<Value> {
    let s = std::fs::read_to_string(p)
        .unwrap_or_else(|e| panic!("读取评测文件失败 {}: {}", p.display(), e));
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("解析评测文件失败 {}: {}", p.display(), e))
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx]
}
