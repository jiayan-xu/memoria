//! 含语义通道的真实召回率评测（Semantic Eval）— 手动评测范畴
//!
//! 与 tests/eval.rs 同一语料/用例，但走完整生产路径：
//! 本地 embedding 服务(8777) → QueryCache → HNSW → hybrid_search（含 S2 语义信号）。
//! 额外追加「同义改写」用例验证语义召回能力。
//!
//! 运行：`cargo test --test eval_semantic -- --nocapture`
//! 前置：embed_server.py 运行于 127.0.0.1:8777（MEMORIA_EMBEDDING_URL）。

use memoria_core::search::hybrid::hybrid_search;
use memoria_core::storage::{create_pool, init_core_tables, init_schema};
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::MemoriaEngine;
use serde_json::Value;
use std::path::Path;
use std::time::Instant;

/// 召回下限（与 tests/eval.rs 对齐）
const RECALL_FLOOR: f64 = 0.85;
const EMBED_URL: &str = "http://127.0.0.1:8777/embed";

async fn embed(client: &reqwest::Client, text: &str) -> Result<Vec<f32>, String> {
    let body = serde_json::json!({"texts": [text], "normalize": false});
    let resp = client
        .post(EMBED_URL)
        .timeout(std::time::Duration::from_secs(60))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("embed http: {}", e))?;
    let data: Value = resp.json().await.map_err(|e| format!("embed parse: {}", e))?;
    let arr = data["embeddings"]
        .as_array()
        .ok_or_else(|| "embed: missing embeddings".to_string())?;
    arr[0]
        .as_array()
        .ok_or_else(|| "embed: bad vector".to_string())?
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect::<Vec<f32>>()
        .pipe(Ok)
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

#[tokio::test]
async fn memory_eval_semantic() {
    // CI/无本地嵌入服务时跳过（本测试属手动语义评测范畴，依赖 127.0.0.1:8777；
    // GitHub Actions runner 无该服务 → 此前 CI 恒红、本地全绿）
    {
        let probe = reqwest::Client::new();
        let ok = probe
            .get("http://127.0.0.1:8777/health")
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !ok {
            eprintln!("[skip] 本地嵌入服务(127.0.0.1:8777)不可达，跳过语义评测（CI 环境）");
            return;
        }
    }
    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/cases");
    let corpus: Vec<Value> = read_json_array(&cases_dir.join("corpus.json"));
    let cases: Vec<Value> = read_json_array(&cases_dir.join("cases.json"));
    assert!(!corpus.is_empty() && !cases.is_empty(), "eval cases 不能为空");

    // fixture DB
    let db = std::env::temp_dir().join(format!("memoria_eval_sem_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");

    // 引擎（写入侧向量入 HNSW）
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let client = reqwest::Client::new();

    // 1) 语料嵌入 + 写入（向量入 QueryCache → HNSW）
    let mut ids: Vec<String> = Vec::with_capacity(corpus.len());
    let mut hype_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in &corpus {
        let content = item["content"].as_str().expect("corpus[].content");
        let category = item["category"].as_str().unwrap_or("fact");
        let importance = item["importance"].as_i64().unwrap_or(3);
        let ns = item["ns"].as_str().unwrap_or("agent/default");
        let tags = item["tags"].as_str().unwrap_or("[]");

        let v = embed(&client, content).await.expect("corpus embed");
        engine.cache_query_vector(content, v.clone());
        let r = remember_with_dedup(
            &pool,
            content,
            category,
            importance,
            "eval_sem",
            ns,
            tags,
            Some(&engine.hnsw),
            Some(&engine.query_cache),
            None, // valid_from
            None, // valid_to
            None, // supersedes_id
            None, // relation
            None, // actor
            None, // memory_type
            None, // parent_id
            None, // raw_ref
        )
        .expect("remember");
        let rid = r.id.clone();
        ids.push(r.id);

        // V1：HyPE 通道覆盖——写入**问句化改写**的向量并加入 hype 索引，使语义测试真实走
        // 双路合并（若用与内容路相同的向量，两路 cosine 恒等、合并退化为内容通道，
        // 测不到 HyPE 的 read/merge 路径——正是本块要防的 no-op 假覆盖）。真实生产里问句
        // 向量由 LLM 生成；测试里以「用户提问：+ 内容」嵌入近似（问句-内容措辞不同，
        // 双路分数各异，合并取 max 才有意义）。
        // 结果必须断言：put/add 静默失败会让 hype_hnsw 恒空、测试假绿。注意：
        // remember_with_dedup 近义去重命中时返回**已有记忆 id**，其 hype 向量可能已 add 过
        // （HnswIndex 按 id 去重，重复 add 返回 0 属正常），故仅首次遇到该 rid 才断言 n>0。
        {
            use memoria_core::vector::{VectorEntry, persist};
            // 首次遇到该 rid 才持久化 + 入索引：近义去重命中的旧 id 若只 put 不 add，
            // 权威表会被新向量覆盖而运行中索引仍持旧向量——表/索引分歧，后续 rebuild
            // 会得到与当前索引不同的向量。put 与 add 必须同受 hype_seen 守卫（#R33 bug/low）。
            if hype_seen.insert(rid.clone()) {
                // 问句化改写：与内容向量不同（否则双路合并无意义）。嵌入失败则**跳过**
                // 该条（不 put/add）——fallback 到内容向量会让两路恒等、退化为内容通道，
                // 正是本块要防的 no-op 假覆盖（且把内容向量写进 question 列与离线脚本
                // 的问句向量不一致，rebuild 后会混入两种形态）。
                if let Ok(hv) = embed(&client, &format!("用户提问：{content}")).await {
                    persist::put_hype_stored_vector(&pool, &rid, ns, &hv)
                        .expect("put_hype_stored_vector should succeed");
                    let n = engine
                        .hype_hnsw
                        .add(&[VectorEntry {
                            id: rid.clone(),
                            vector: hv,
                        }])
                        .expect("hype_hnsw.add should succeed");
                    assert!(n > 0, "hype_hnsw.add added 0 entries (degenerate vector or duplicate id)");
                }
            }
        }

        // 应用时序偏移
        if let Some(off) = item["created_offset_days"].as_i64() {
            if off > 0 {
                let ts = chrono::Utc::now() - chrono::Duration::days(off);
                let _ = pool.get().unwrap().execute(
                    "UPDATE memories SET created_at = ? WHERE id = ?",
                    rusqlite::params![ts.format("%Y-%m-%dT%H:%M:%S").to_string(), &rid],
                );
            }
        }
    }

    // 2) 官方 12 用例（query 嵌入 → 语义信号参与融合）
    let mut recall_hits = 0u32;
    let mut recall_total = 0u32;
    let mut zero_results = 0u32;
    let mut latencies: Vec<f64> = Vec::new();
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

        // query 嵌入
        if let Ok(v) = embed(&client, q).await {
            engine.cache_query_vector(q, v);
        }

        let start = Instant::now();
        let results = hybrid_search(
            &pool, q, ns, k,
            Some(&engine.hnsw), Some(&engine.hype_hnsw), Some(&engine.query_cache), None, false,
        )
        .unwrap_or_default();
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
                    if !result_ids.contains(&expected_id.as_str()) {
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

        if !expect.is_empty() || !must_not.is_empty() {
            recall_total += 1;
            if case_ok {
                recall_hits += 1;
            }
        }
    }

    // 3) 追加「同义改写」用例：验证语义召回（不共享关键词）
    let paraphrase_cases: Vec<(String, usize)> = vec![
        // 官方 corpus 中的记忆，用完全不同的说法查询
        ("代码托管平台开源仓库".to_string(), 5),      // corpus 中 GitHub 相关
        ("在线的开发项目托管服务".to_string(), 5),
        ("技术文档的版本管理".to_string(), 3),
    ];
    let mut sem_hits = 0u32;
    let mut sem_failures: Vec<String> = Vec::new();
    for (pq, idx) in &paraphrase_cases {
        let ns = "agent/default";
        let k = 5;
        if let Ok(v) = embed(&client, pq).await {
            engine.cache_query_vector(pq, v);
        }
        let results = hybrid_search(
            &pool, pq, ns, k,
            Some(&engine.hnsw), Some(&engine.hype_hnsw), Some(&engine.query_cache), None, false,
        )
        .unwrap_or_default();
        let result_ids: Vec<&str> = results.iter().map(|r| r.memory_id.as_str()).collect();
        let ok = ids.get(*idx).map(|id| result_ids.contains(&id.as_str())).unwrap_or(false);
        if ok {
            sem_hits += 1;
        } else {
            sem_failures.push(format!(
                "同义改写 '{}' 未召回 idx {}（top-{}: {:?}）",
                pq, idx, k, result_ids
            ));
        }
    }

    // 指标
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let recall = if recall_total > 0 { recall_hits as f64 / recall_total as f64 } else { 1.0 };
    let zero_rate = if !cases.is_empty() { zero_results as f64 / cases.len() as f64 } else { 0.0 };

    eprintln!("===== Memoria Semantic Eval（含 S2 语义通道）=====");
    eprintln!(
        "官方用例: cases={} recall@k={:.2} zero_result_rate={:.2}",
        recall_total, recall, zero_rate
    );
    eprintln!("同义改写: {}/{} 命中", sem_hits, paraphrase_cases.len());
    eprintln!("latency p50={:.2}ms p95={:.2}ms", p50, p95);
    if !failures.is_empty() {
        eprintln!("FAILURES:\n{}", failures.join("\n"));
    }
    if !sem_failures.is_empty() {
        eprintln!("SEM_FAILURES:\n{}", sem_failures.join("\n"));
    }
    eprintln!("=================================================");

    assert!(recall >= RECALL_FLOOR, "召回@k {:.2} 低于下限 {:.2}", recall, RECALL_FLOOR);
    assert_eq!(zero_rate, 0.0, "存在零结果用例");
    assert!(failures.is_empty(), "共 {} 个评测失败", failures.len());

    drop(engine);
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
