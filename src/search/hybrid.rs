//! 统一搜索入口 — 整合 5 信号（keyword + semantic + temporal + importance + category）
//!
//! 替代 lib.rs hybrid_search 和 mcp_server.rs 中各自维护的搜索逻辑。

use crate::search::{self, FusedResult, SignalResult};
use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, QueryCache};

/// 执行 5 信号融合搜索，返回 RRF 排序结果
///
/// `as_of`: P1-5 轻量时序真值。
/// - `Some("2026-01-02T00:00:00")` → `visible_as_of`：仅返回该时刻「有效」的记忆
///   （valid_from <= as_of 且 (valid_to IS NULL 或 valid_to >= as_of)），不查 superseded_by（时序真值优先）。
/// - `None`（默认）→ `is_latest_now`：superseded_by IS NULL 且当前(now)有效（§14.1 Q2）。
/// `include_superseded=true`（F2）` 不跳过过滤，而是把「仍有效(valid_at now)但被取代」的历史真值降权补回（标 time_status="superseded"）。
pub fn hybrid_search(
    pool: &SqlitePool,
    query: &str,
    namespace: &str,
    max_results: u32,
    hnsw: Option<&HnswIndex>,
    hype_hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
    as_of: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<FusedResult>, String> {
    // A+C 配置（均可经 env 覆盖）
    let recall_depth = env_u32("MEMORIA_RECALL_DEPTH", 50).max(8);
    let w_keyword = env_f64("MEMORIA_WEIGHT_KEYWORD", 1.0);
    let w_semantic = env_f64("MEMORIA_WEIGHT_SEMANTIC", 1.0);
    // A/B 实测（2026-07-24，eval/nl_recall_bench.json + 权重扫描）：
    // 原 0.2/0.5/0.3（语义主导）→ keyword 通道召回仅 14%（结构性 0% 经 tokenize_for_fts 拆词修复后）。
    // 修复后扫描发现 kw 提到 0.5 全面最优：整体/semantic/keyword/hybrid recall@10 同步大涨
    // （28%→53~59% / 35%→55~65% / 14%→43% / 20%→60%），且 semantic 不降反升（语义噪声被压）。
    // 固化为 0.3/0.2/0.5（rrf/sem/kw）。
    let rrf_w = env_f64("MEMORIA_RERANK_W_RRF", 0.3);
    let sem_w = env_f64("MEMORIA_RERANK_W_SEM", 0.2);
    let kw_w = env_f64("MEMORIA_RERANK_W_KW", 0.5);
    // 软信号权重（env 可配，默认保持原始值 1.0/1.0/0.5）。
    // 2026-07-26 实测扫描（58 问句冷态）：0.2→67.2% / 0.5→69.0% / 1.0→69.0% @10。
    // 降权反而劣化——软信号帮助「非强语义/关键词命中」的边界 gold 进入 top-100 候选池；
    // 真正解决「软信号淹没纯语义」的是主通道保底(hybrid.rs:282)，而非降权。故默认维持原值。
    let w_temporal = env_f64("MEMORIA_RERANK_W_TEMPORAL", 1.0);
    let w_importance = env_f64("MEMORIA_RERANK_W_IMPORTANCE", 1.0);
    let w_category = env_f64("MEMORIA_RERANK_W_CATEGORY", 0.5);

    let fts_limit = max_results * 3; // 辅助信号（temporal/importance）检索深度
    let primary_limit = recall_depth.max(fts_limit); // 主信号（语义/关键词）宽召回深度
    let mut signals: Vec<Vec<SignalResult>> = Vec::new();
    let mut weights: Vec<f64> = Vec::new();
    // 主召回通道原始结果，供「主通道保底」使用（见 take 前）
    let mut sem_res: Option<Vec<SignalResult>> = None;
    let mut kw_res: Option<Vec<SignalResult>> = None;

    // S1: Keyword (FTS5 + LIKE) — 宽召回
    if let Ok(kw) = search::keyword::keyword_search(pool, query, namespace, primary_limit) {
        if !kw.is_empty() {
            kw_res = Some(kw.clone());
            signals.push(kw);
            weights.push(w_keyword);
        }
    }

    // S2: Semantic (HNSW vector) — 宽召回（V1：可选 HyPE 问句索引双路合并）。
    // 门控：任一索引存在即启用——semantic_search 内部按索引各自独立搜索再合并，
    // 单索引缺失时自然退化为单路（与 semantic_search 的契约一致）。若仅因 hnsw 为
    // None 而整体跳过，会连已有的 hype 通道也一起丢掉。
    if let Some(qc) = query_cache {
        if hnsw.is_some() || hype_hnsw.is_some() {
            match search::semantic::semantic_search(
                query,
                namespace,
                primary_limit,
                hnsw,
                hype_hnsw,
                Some(qc),
                Some(pool),
            ) {
                Ok(sem) => {
                    if !sem.is_empty() {
                        sem_res = Some(sem.clone());
                        signals.push(sem);
                        weights.push(w_semantic);
                    }
                }
                // #R35 maintainability/low：semantic_search 在"全部 HNSW 路失败"
                // （poisoned/corrupted）时显式返回 Err——此处不能静默丢弃整个语义信号
                // （退化为 keyword-only 无痕），至少记录，让降级可观测。
                // #R37 maintainability/low：Err 也可能来自 DB/pool 故障（content backfill
                // pool/get 失败等），日志前缀保持中立，不预设根因是索引损坏。
                Err(e) => {
                    eprintln!("[hybrid] semantic signal dropped: {}", e);
                }
            }
        }
    }

    // S3: Temporal (recency bias)
    if let Ok(temp) = search::temporal::temporal_search(pool, namespace, fts_limit) {
        if !temp.is_empty() {
            signals.push(temp);
            weights.push(w_temporal);
        }
    }

    // S4: Importance (recall count + decay)
    if let Ok(imp) = search::importance::importance_search(pool, namespace, fts_limit) {
        if !imp.is_empty() {
            signals.push(imp);
            weights.push(w_importance);
        }
    }

    // S5: Category (query intent match)
    if let Ok(cat) = search::importance::category_search(pool, query, namespace, max_results) {
        if !cat.is_empty() {
            signals.push(cat);
            weights.push(w_category);
        }
    }

    let mut fused = if signals.is_empty() {
        Vec::new()
    } else {
        search::rrf::rrf_merge(&signals, &weights, 60.0)
    };

    // 2-hop graph expansion —— 2026-07-26 图召回定论：默认关闭（max_hops=0）。
    // 实测（58 问句冷态 A/B，同二进制）：图开(=2) vs 图关(=0) @10 均为 69.0%（Δ=0）；
    // 16/18 缺失 gold 结构上 2 跳不可达，少数进池图项被主通道保底压在 33 名外、永不到 top-10。
    // 图扩展每查询额外 ~80 次 DB 查找，零召回收益，故召回路径默认禁用；
    // 仍可经 MEMORIA_GRAPH_HOPS>0 环境变量按需开启做实验。
    if let Ok(expanded) = search::rrf::graph_expand(pool, &fused, 0, namespace) {
        fused.extend(expanded);
    }

    // Dedup by memory_id
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<FusedResult> = fused
        .into_iter()
        .filter(|r| seen.insert(r.memory_id.clone()))
        .collect();

    // P0 + P1-5 / F2：isLatest / visible_as_of / 历史降权过滤（§14.1 Q2）。
    // 统一在 dedup 后、take 前执行；graph_expand 邻居因已并入 unique 一并覆盖。
    // 规则：
    //  - as_of=Some(T)          → 仅 visible_as_of（valid_* 有效），忽略 superseded_by（时序真值优先）。
    //  - as_of=None（默认 tip） → is_latest_now：superseded_by IS NULL 且当前(now)有效 = current。
    //  - include_superseded=true（F2）→ 在 current 之外，补回「被取代」的历史真值（含 valid_to
    //    已关闭的旧版本），标 time_status="superseded" 并乘 MEMORIA_HISTORY_DOWNWEIGHT 降权。
    //    （2026-08-06 修复：此前仅补回 valid_at now 仍有效的，与 F2「跳过时序过滤」语义不符，
    //    as_of 集成测试 include_superseded 断言失败。）
    if !unique.is_empty() {
        if let Ok(conn) = pool.get() {
            let ids: Vec<String> = unique.iter().map(|r| r.memory_id.clone()).collect();
            let ph = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, superseded_by, valid_from, valid_to FROM memories WHERE id IN ({})",
                ph
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let info: std::collections::HashMap<
                    String,
                    (Option<String>, Option<String>, Option<String>),
                > = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<String>>(3)?,
                            ),
                        ))
                    })
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default();
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let ref_time = as_of.unwrap_or_else(|| now.as_str());
                let downweight = env_f64("MEMORIA_HISTORY_DOWNWEIGHT", 0.5).clamp(0.0, 1.0);
                // 先算 keep 标记（与 unique 顺序一致）
                let mut keep: Vec<bool> = Vec::with_capacity(unique.len());
                for r in unique.iter() {
                    match info.get(&r.memory_id) {
                        Some((sup, vf, vt)) => {
                            let valid = valid_at(vf.as_deref(), vt.as_deref(), ref_time);
                            if include_superseded {
                                keep.push(true); // F2：跳过整段时序/superseded 过滤，全部补回
                            } else if as_of.is_some() {
                                keep.push(valid); // 时序真值：仅看 valid_*, 忽略 superseded_by
                            } else if sup.is_none() && valid {
                                keep.push(true); // is_latest_now
                            } else {
                                keep.push(false);
                            }
                        }
                        None => keep.push(false),
                    }
                }
                // 应用 time_status + 降权（仅对保留项）
                let mut idx = 0;
                for r in unique.iter_mut() {
                    let k = keep[idx];
                    idx += 1;
                    if !k {
                        continue;
                    }
                    match info.get(&r.memory_id) {
                        Some((sup, vf, vt)) => {
                            let valid = valid_at(vf.as_deref(), vt.as_deref(), ref_time);
                            if as_of.is_some() {
                                r.time_status = if valid {
                                    Some("current".to_string())
                                } else {
                                    None
                                };
                            } else if sup.is_none() && valid {
                                r.time_status = Some("current".to_string());
                            } else {
                                r.time_status = Some("superseded".to_string());
                                r.rrf_score *= downweight;
                            }
                        }
                        None => r.time_status = None,
                    }
                }
                // 剔除未保留项
                let mut i = 0;
                unique.retain(|_| {
                    let k = keep[i];
                    i += 1;
                    k
                });
            }
        }
    }

    // F1b：补全召回计数/时间元数据（access_count / last_recalled），供 two_stage_rerank 频率+新鲜度加权。
    if !unique.is_empty() {
        if let Ok(conn) = pool.get() {
            let ids: Vec<String> = unique.iter().map(|r| r.memory_id.clone()).collect();
            let ph = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, access_count, last_recalled FROM memories WHERE id IN ({})",
                ph
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let meta: std::collections::HashMap<String, (i64, Option<String>)> = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (row.get::<_, i64>(1).unwrap_or(0), row.get::<_, Option<String>>(2)?),
                        ))
                    })
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default();
                for r in unique.iter_mut() {
                    if let Some((c, t)) = meta.get(&r.memory_id) {
                        r.access_count = *c;
                        r.last_recalled = t.clone();
                    }
                }
            }
        }
    }

    // PR4（Phase A 演化）：演化脏标记（evolved_at IS NULL = 待演化）+ 可选降权。
    // 独立于 isLatest/as_of 过滤，对全部候选（含 include_superseded）标注，供 recall 降权/标注。
    if !unique.is_empty() {
        if let Ok(conn) = pool.get() {
            let ids: Vec<String> = unique.iter().map(|r| r.memory_id.clone()).collect();
            let ph = vec!["?"; ids.len()].join(",");
            let sql = format!("SELECT id, evolved_at FROM memories WHERE id IN ({})", ph);
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let evo: std::collections::HashMap<String, Option<String>> = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default();
                let downweight = pending_downweight();
                for r in unique.iter_mut() {
                    let ev = evo.get(&r.memory_id).cloned().flatten();
                    r.evolved_at = ev.clone();
                    r.pending_evolution = ev.is_none();
                    if r.pending_evolution && downweight < 1.0 {
                        r.rrf_score *= downweight;
                    }
                }
            }
        }
    }

    // Phase B / M1.3：轻量共现启发式 rerank（O5：无 cross-encoder）
    // 在 take 前对候选池加成重排，再截断到 max_results。
    search::cooccur::rerank_by_cooccurrence(pool, namespace, query, &mut unique);
    // P2 / M2.1：text_signals 数字/日期重叠加成（可 MEMORIA_TEXT_SIGNALS_RERANK=0 关闭）
    search::text_signals::rerank_by_text_signals(query, &mut unique);

    // A：两阶段重排 — 在宽候选池上用「幅度感知混合分」重排到 max_results。
    // 第一阶=RRF 位置融合（rrf_merge 完成）；第二阶=对候选池用
    // 归一化 RRF + 原始 cosine(sem_cos) + 原始 BM25(kw_bm25) 混合重排，显著降误召。
    // cooccur/text_signals 加成已折入 rrf_score，归一化后相对序保留，不丢前序成果。
    two_stage_rerank(&mut unique, rrf_w, sem_w, kw_w);

    // 主通道保底（P0 召回修复，2026-07-26）：semantic/keyword 是核心召回通道，
    // 但 rrf_merge 把 temporal/importance/category 也作平等召回通道累加，而这三个软信号
    // 条件宽松、覆盖几乎全库，其 rrf 普遍叠加在绝大多数记忆上，导致「仅强命中语义/关键词」
    // 的纯单通道记忆在 take 截断时被多通道项淹没（实测最终 100 池仅 1 条 semantic）。
    // 此处把两个主通道各自的 top-RESERVE 候选提到前部，确保它们优先入池，
    // 不被全库软信号挤出；temporal/importance/category 仍作为 rerank 增益作用于保底项内部。
    const RESERVE: usize = 50;
    let mut reserve: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(s) = &sem_res {
        for r in s.iter().take(RESERVE) {
            reserve.insert(r.memory_id.clone());
        }
    }
    if let Some(k) = &kw_res {
        for r in k.iter().take(RESERVE) {
            reserve.insert(r.memory_id.clone());
        }
    }
    if !reserve.is_empty() {
        let mut reordered: Vec<FusedResult> = Vec::new();
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in unique.iter() {
            if reserve.contains(&r.memory_id) && taken.insert(r.memory_id.clone()) {
                reordered.push(r.clone());
            }
        }
        for r in unique.iter() {
            if taken.insert(r.memory_id.clone()) {
                reordered.push(r.clone());
            }
        }
        unique = reordered;
    }

    let unique: Vec<FusedResult> = unique.into_iter().take(max_results as usize).collect();

    Ok(unique)
}

/// P1-5: 判断记忆在 `as_of` 时刻是否有效。
/// 有效区间：[valid_from, valid_to]，端点闭合。任一端点缺失按「无界」处理。
/// 注意：valid_from/valid_to 为固定格式 ISO-8601 字符串，字典序即时间序，可直接比较。
fn valid_at(valid_from: Option<&str>, valid_to: Option<&str>, as_of: &str) -> bool {
    let from_ok = match valid_from {
        None => true,
        Some(v) => v <= as_of,
    };
    let to_ok = match valid_to {
        None => true,
        Some(v) => v >= as_of,
    };
    from_ok && to_ok
}

/// PR4（Phase A 演化）：待演化 tip 的召回降权系数。
/// 默认 1.0 = 仅标注不降权（避免大量新鲜写入被无谓压低，保护 recall 质量）；
/// 可由 `MEMORIA_PENDING_DOWNWEIGHT` 配置（如 0.85）。脏标记 `pending_evolution` 始终标注。
fn pending_downweight() -> f64 {
    match std::env::var("MEMORIA_PENDING_DOWNWEIGHT") {
        Ok(v) => v.trim().parse::<f64>().unwrap_or(1.0).clamp(0.0, 1.0),
        Err(_) => 1.0,
    }
}

/// A：两阶段幅度感知重排。
/// 在 RRF 融合 + cooccur/text_signals 加成之后的宽候选池上，
/// 用「归一化 RRF + 原始 cosine(sem_cos) + 原始 BM25(kw_bm25)」的混合分重排，
/// 再交回 `take(max_results)` 截断。cooccur/text_signals 的加成已折入 rrf_score，
/// 归一化后相对序保留，故不丢失前序重排成果。
fn two_stage_rerank(results: &mut Vec<FusedResult>, w_rrf: f64, w_sem: f64, w_kw: f64) {
    if results.is_empty() {
        return;
    }
    let rrf_max = results.iter().map(|r| r.rrf_score).fold(0.0_f64, f64::max);
    let sem_max = results
        .iter()
        .map(|r| r.sem_cos.unwrap_or(0.0))
        .fold(0.0_f64, f64::max);
    let kw_max = results
        .iter()
        .map(|r| r.kw_bm25.unwrap_or(0.0))
        .fold(0.0_f64, f64::max);
    // 1b 修复：图传递相关性分量（graph_signal）。默认 0.15 给图扩展项保底分，
    // 抵消此前 sem_n/kw_n=0 导致的静默压低；由 MEMORIA_RERANK_W_GRAPH 配置。
    let graph_max = results
        .iter()
        .map(|r| r.graph_signal.unwrap_or(0.0))
        .fold(0.0_f64, f64::max);
    let w_graph = env_f64("MEMORIA_RERANK_W_GRAPH", 0.15);
    // F1b：频率 + 新鲜度分量（env 可调权重，默认 0.1；为 0 时退化为现状）
    let w_freq = env_f64("MEMORIA_RERANK_W_FREQ", 0.1);
    let w_rec = env_f64("MEMORIA_RERANK_W_REC", 0.1);
    let k_freq = env_f64("MEMORIA_FREQ_K", 10.0).max(1.0);
    let lambda = env_f64("MEMORIA_RECENCY_LAMBDA", 0.01).max(0.0);
    let now_secs = chrono::Utc::now().timestamp();
    for r in results.iter_mut() {
        let rrf_n = if rrf_max > 0.0 { r.rrf_score / rrf_max } else { 0.0 };
        let sem_n = if sem_max > 0.0 {
            r.sem_cos.unwrap_or(0.0) / sem_max
        } else {
            0.0
        };
        let kw_n = if kw_max > 0.0 {
            r.kw_bm25.unwrap_or(0.0) / kw_max
        } else {
            0.0
        };
        let graph_n = if graph_max > 0.0 {
            r.graph_signal.unwrap_or(0.0) / graph_max
        } else {
            0.0
        };
        // F1b：频率饱和（access_count/(access_count+K)）+ 新鲜度指数衰减（按 last_recalled 距今年化）
        let c = r.access_count as f64;
        let freq_n = c / (c + k_freq);
        let recency_n = match &r.last_recalled {
            Some(t) => {
                let secs = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S")
                    .map(|d| d.and_utc().timestamp())
                    .unwrap_or(now_secs);
                let age_h = ((now_secs - secs) as f64 / 3600.0).max(0.0);
                (-lambda * age_h).exp()
            }
            None => 0.5,
        };
        r.rrf_score = w_rrf * rrf_n + w_sem * sem_n + w_kw * kw_n
            + w_graph * graph_n
            + w_freq * freq_n + w_rec * recency_n;
        if !r.source.contains("rerank2") {
            r.source = format!("{};rerank2", r.source);
        }
    }
    results.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn env_f64(key: &str, default: f64) -> f64 {
    match std::env::var(key) {
        Ok(v) => v.trim().parse::<f64>().unwrap_or(default),
        Err(_) => default,
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    match std::env::var(key) {
        Ok(v) => v.trim().parse::<u32>().unwrap_or(default),
        Err(_) => default,
    }
}
