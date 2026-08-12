//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::QueryCache;
use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use std::collections::{HashMap, HashSet};

/// Semantic search via HNSW vector similarity.
/// Python must call cache_query_vector() first to provide the query embedding.
///
/// `pool` 用于按调用者 namespace 回查 `memories` 表，过滤 HNSW 全局索引返回的跨租户记忆
/// （B2 修复：HNSW 无 namespace 维度，原实现完全忽略 ns 导致跨租户记忆泄露）。
///
/// `hype_hnsw`（V1 2026-08-12）：HyPE 假设问句索引（可选）。传入时双路搜索——同一 query
/// 向量分别匹配「内容向量」（hnsw）与「问句向量」（hype_hnsw），按 memory_id 取两路
/// cosine 最大值合并。问句向量与 query 同为「问句式」表达，缩小措辞 gap（IEEE Access
/// 2025 HyPE）。两路结果分别过 ns 过滤后合并；任一索引缺失则退化为单路。
pub fn semantic_search(
    query: &str,
    namespace: &str,
    limit: u32,
    hnsw: Option<&HnswIndex>,
    hype_hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    pool: Option<&SqlitePool>,
) -> Result<Vec<SignalResult>, String> {
    let cache = match query_cache {
        Some(c) => c,
        None => return Ok(vec![]),
    };

    // Get cached embedding from Python (must have been cached via cache_query_vector)
    let vector = match cache.get(query) {
        Some(v) => v,
        None => return Ok(vec![]), // No cached embedding — skip semantic signal
    };

    // P0 防御：查询向量退化（NaN / 全零）则无法产生有效语义信号，提前返回空集。
    // 全零向量在 DistCosine 下与任意记忆 distance≈0 → score≈1.0，会灌入 limit 条垃圾；
    // NaN 则 score 非有限。add() 已拦退化向量入索引，此处对「查询向量」做双保险。
    let norm_sq: f32 = vector.iter().map(|&x| x * x).sum();
    if !norm_sq.is_finite() || norm_sq <= 0.0 {
        return Ok(vec![]);
    }

    // 收集两路候选：content 路（hnsw）+ hype 路（hype_hnsw），按 memory_id 合并取 max。
    // HNSW 是全局索引、无 namespace 维度（语义检索 B2 修复说明）：
    // 先按远大于 limit 的窗口过取全局候选，再按 ns 过滤，保证目标 ns 拿到足够语义候选。
    // 已知限制（#R32 other/low）：两路 cosine 分数分布不保证同尺度，per-id 取 max 会偏向
    // 系统性高分的一路——当前内容/问句向量同出自 Qwen3-VL-8B（同模型同空间），A/B 实测
    // （+9.4pp recall@10）未现偏差；若未来换模型/分路，须先校准两路分数尺度再依赖 raw max。
    // `best` 值 = (max cosine, winning road label)——记录哪一路胜出，供下游归因
    // （#R35 maintainability/low：此前只存分数，source 恒为 hnsw_semantic，无法区分
    // 命中来自内容路还是 HyPE 路）。
    let mut best: HashMap<String, (f64, &'static str)> = HashMap::new();
    // 两路分别搜索；`roads_ok` 统计成功路数——若**所有存在的路**都失败（如 RwLock
    // poisoned），返回 Err 而非空集，使调用方能区分"索引空"与"索引坏了"（#R34 other/low：
    // 此前全失败静默映射为空结果，健康检查无法发现语义通道悄悄降级）。
    // 单路失败（如 hype 坏而 content 正常）仍返回单路结果：search_and_merge 已 eprintln
    // 记录，且 hybrid.rs 的 Err 分支日志覆盖了"全部路失败"场景——单路降级以日志呈现
    // （#R35 other/medium：为单路失败单开 escalate 会误伤"hype 空索引 + content 正常"
    // 的合法态；完整方案是 per-road 失败注入结果元数据，留待后续）。
    let mut roads_ok = 0usize;
    if let Some(h) = hnsw {
        if search_and_merge(h, "content", &vector, limit, &mut best) {
            roads_ok += 1;
        }
    }
    if let Some(h) = hype_hnsw {
        if search_and_merge(h, "hype", &vector, limit, &mut best) {
            roads_ok += 1;
        }
    }
    if roads_ok == 0 && (hnsw.is_some() || hype_hnsw.is_some()) {
        return Err("semantic_search: all HNSW roads failed (poisoned/corrupted index)".into());
    }
    if best.is_empty() {
        return Ok(vec![]);
    }

    // 合并上限说明（#R33 maintainability/low）：`search_with_ef(overfetch, overfetch)` 每路
    // 至多返回 overfetch 条，best 按 memory_id 去重后 len ≤ ovf_content + ovf_hype ≤ 2×ovf——
    // 因此无需（也无法）在此截断；曾有的 sort/truncate 分支是 dead code，已移除。
    // ns 回查的 IN(...) 规模即 ≤ 2×ovf（默认 ~6000，远低于 SQLite 32766 变量上限；
    // 若未来索引规模使 2×ovf 逼近上限，需在 lookup_namespaces 内分批，见该函数注释）。

    // HNSW 是全局索引，无 namespace 维度。按调用者 ns 回查 memories 表，
    // 仅保留归属当前 ns 的记忆，杜绝跨租户泄露。无 pool 时无法过滤，保守返回空。
    let ids: Vec<&str> = best.keys().map(|s| s.as_str()).collect();
    let allowed: HashSet<String> = match pool {
        Some(p) => match lookup_namespaces(p, &ids) {
            Ok(map) => map
                .into_iter()
                .filter(|(_, ns)| ns == namespace)
                .map(|(id, _)| id)
                .collect(),
            Err(_) => return Ok(vec![]),
        },
        None => return Ok(vec![]),
    };
    if allowed.is_empty() {
        return Ok(vec![]);
    }

    // P3-0 修复：语义结果此前 content 恒为空（只带 memory_id），
    // 经 rrf_merge 首次插入即锁定空正文，导致「仅被语义命中」的记忆在 fusion 后丢失正文，
    // benchmark 拼上下文时得不到内容、答案必错。此处按 allowed id 批量回取 content 补齐。
    // #R34 performance/medium：与 lookup_namespaces 同款分批（BATCH=500）——allowed 规模
    // 与 best 同量级（可达 ~12000），单条 IN(...) 在旧 SQLite（999 变量上限）会 prepare
    // 失败且被 if let Ok 静默吞掉，导致全部语义结果正文为空却无任何告警。
    let mut contents: HashMap<String, String> = HashMap::new();
    if let Some(p) = pool {
        const BATCH: usize = 500;
        let ids: Vec<&String> = allowed.iter().collect();
        for chunk in ids.chunks(BATCH) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, content FROM memories WHERE id IN ({})",
                placeholders
            );
            // #R35 bug/medium：分批消除了变量上限触发，但**每个失败点都不能静默吞**——
            // 若某批 get/prepare/query_map 失败，该批 id 经 unwrap_or_default 变空正文，
            // 部分成功部分丢失，正是 P3-0 内容丢失 bug 的局部复发且难以察觉。至少记录
            // 每批失败（含批大小），与 lookup_namespaces 的传播式错误处理对齐。
            match p.get() {
                Ok(conn) => match conn.prepare(&sql) {
                    Ok(mut stmt) => match stmt.query_map(
                        rusqlite::params_from_iter(chunk.iter().map(|s| *s)),
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    ) {
                        Ok(rows) => {
                            for r in rows.flatten() {
                                contents.insert(r.0, r.1);
                            }
                        }
                        Err(e) => eprintln!(
                            "[semantic] content backfill query failed (batch {} ids): {}",
                            chunk.len(),
                            e
                        ),
                    },
                    Err(e) => eprintln!(
                        "[semantic] content backfill prepare failed (batch {} ids): {}",
                        chunk.len(),
                        e
                    ),
                },
                Err(e) => eprintln!(
                    "[semantic] content backfill pool.get failed (batch {} ids): {}",
                    chunk.len(),
                    e
                ),
            }
        }
    }

    let mut out: Vec<SignalResult> = Vec::with_capacity(allowed.len());
    for memory_id in &allowed {
        if let Some((score, road)) = best.get(memory_id) {
            let content = contents.get(memory_id).cloned().unwrap_or_default();
            out.push(SignalResult {
                memory_id: memory_id.clone(),
                content,
                score: *score,
                // 归因：winning road 标记进 source（#R35 maintainability/low）——
                // rrf.rs 的 channel_of 按子串匹配通道，";hype" 后缀不影响现有融合，
                // 但诊断"命中来自内容路还是问句路"成为可能。
                source: if *road == "hype" {
                    "hnsw_semantic;hype".to_string()
                } else {
                    "hnsw_semantic".to_string()
                },
            });
        }
    }
    // 按合并后分数降序（保持原语义：语义通道按相似度排序进入融合）。
    // 二级排序 key = memory_id：allowed 是 HashSet，迭代顺序每次运行随机——
    // 平分（score 相同）时稳定排序会保留该随机序，使 tie 的相对序与
    // `out.truncate(limit)` 边界取舍跨运行不确定。加 id 字典序打破平局，保证确定性。
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
    // 恢复「每通道贡献 limit 条」设计：过取后按 ns 过滤，再截断到本 ns 内 top-limit，
    // 避免把上千条跨 ns 候选灌进融合（既保平衡，又确保 gold 在正确的本 ns top 内）。
    out.truncate(limit as usize);
    Ok(out)
}

/// 单路 HNSW 搜索并合并进 `best`（按 memory_id 取 max cosine，记录 winning road 归因）。
/// 内容路与 HyPE 路共用，保证双路契约一致（#R33 maintainability/low：两路若各写一份，
/// 未来改 overfetch/分数过滤/错误处理极易漏改一路，静默分裂）。`label` 仅用于错误日志
/// 前缀与归因标记。返回 true = 该路搜索成功（即使 0 结果）；false = 索引故障（RwLock
/// poisoned）。
fn search_and_merge(
    h: &HnswIndex,
    label: &'static str,
    vector: &[f32],
    limit: u32,
    best: &mut HashMap<String, (f64, &'static str)>,
) -> bool {
    let cap = h.len();
    let overfetch = (limit as usize)
        .saturating_mul(20)
        .max(2048)
        .min(cap.max(1));
    // search_with_ef 仅可能在索引被并发 panic 污染（RwLock poisoned）时返回 Err——
    // 不静默吞掉（会永久静默降级语义通道且无法诊断），记录并返回 false 供调用方
    // 聚合判定"全部路失败 → 显式 Err"（#R34 other/low）。
    match h.search_with_ef(vector, overfetch, overfetch) {
        Ok(results) => {
            for (memory_id, distance) in results {
                let score = 1.0 - distance as f64;
                if score.is_finite() && score > 0.0 {
                    let e = best.entry(memory_id).or_insert((0.0, label));
                    if score > e.0 {
                        *e = (score, label);
                    }
                }
            }
            true
        }
        Err(e) => {
            eprintln!("[semantic] {label} HNSW search failed: {e}");
            false
        }
    }
}

/// 批量回查 memory_id 的 namespace（分批 IN 查询，避免 N+1 且不超 SQLite 变量上限）。
///
/// 双路合并后 id 数可达 ~12000（limit=300 时 2×overfetch），单条 IN(...) 逼近/超过
/// SQLITE_MAX_VARIABLE_NUMBER（bundled 32766；旧库可能 999）会 prepare 失败并静默降级。
/// 每批最多 500 个占位符，分批查询后合并（#R33 performance/medium）。
fn lookup_namespaces(
    pool: &SqlitePool,
    results: &[&str],
) -> Result<HashMap<String, String>, String> {
    const BATCH: usize = 500;
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut map = HashMap::new();
    for chunk in results.chunks(BATCH) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id, namespace FROM memories WHERE id IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {}", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter().map(|s| *s)), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query: {}", e))?;
        for row in rows.flatten() {
            map.insert(row.0, row.1);
        }
    }
    Ok(map)
}
