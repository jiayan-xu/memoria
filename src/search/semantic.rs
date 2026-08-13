//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::QueryCache;
use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use std::collections::HashMap;

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
    // #R37 bug/low：维度校验——query 向量长度 ≠ DIM 时 HNSW 距离计算跑在错配维度上，
    // 静默产生垃圾分数（最坏 panic 污染索引锁）。离线脚本已防服务器模型漂移（768d），
    // 运行时路径同样显式拒绝，使配置漂移快速可见而非悄悄劣化。
    if vector.len() != crate::vector::DIM {
        return Err(format!(
            "semantic_search: query vector dim {} != HNSW DIM {}",
            vector.len(),
            crate::vector::DIM
        ));
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
    let mut roads_failed = 0usize;
    if let Some(h) = hnsw {
        if search_and_merge(h, "content", &vector, limit, &mut best) {
            roads_ok += 1;
        } else {
            roads_failed += 1;
        }
    }
    if let Some(h) = hype_hnsw {
        if search_and_merge(h, "hype", &vector, limit, &mut best) {
            roads_ok += 1;
        } else {
            roads_failed += 1;
        }
    }
    if roads_ok == 0 && roads_failed > 0 {
        return Err("semantic_search: all HNSW roads failed (poisoned/corrupted index)".into());
    }
    // #R44 bug/medium：升级缺口——一路失败但幸存路**合法返回 0 结果**时（如 hype 空
    // 索引 + content 索引损坏，或反之），`roads_ok==1` 且 best 空会走 Ok(vec![])，
    // 掩盖坏路、语义通道静默降级（#R34 "区分索引空与索引坏"的目标在此组合下未达成）。
    // #R45 bug/low：但**不升级为 Err**——search_with_ef 只在 RwLock poisoning（持久
    // 条件）时失败，幸存路合法返回 0 匹配是大多数查询的常态（多数 query 无近邻），
    // 升级会把每个此类查询误报为通道损坏、刷错误日志直到重启；且 hybrid.rs 对 Err
    // 与空结果同等处理（仅记录后丢弃），升级无新增诊断价值。search_and_merge 已逐次
    // eprintln 失败路——降级以日志呈现，返回值仍 Ok(vec![])。
    // #R47 performance/low：该降级行**每查询都会命中**（失败路持续 + 多数查询无近邻），
    // 无条件 eprintln 会刷爆 stderr。
    // #R48 other/low：`once-only` 标志永不重武装——早期瞬时降级后，后续（可能持久的）
    // 降级期保持静默。用**时间戳冷却**（60s）重武装：持续故障每 ~60s 出一条信号，
    // 瞬时抖动只出一条。
    if roads_failed > 0 && best.is_empty() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        const COOLDOWN_SECS: u64 = 60;
        static LAST_DEGRADED_LOG: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = LAST_DEGRADED_LOG.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= COOLDOWN_SECS
            && LAST_DEGRADED_LOG
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            eprintln!(
                "semantic_search: {roads_failed} road(s) failed and survivors returned no matches (degraded; logged at most once per {COOLDOWN_SECS}s)"
            );
        }
    }
    if best.is_empty() {
        return Ok(vec![]);
    }

    // 合并上限说明（#R33 maintainability/low）：`search_with_ef(overfetch, overfetch)` 每路
    // 至多返回 overfetch 条，best 按 memory_id 去重后 len ≤ ovf_content + ovf_hype ≤ 2×ovf——
    // 因此无需（也无法）在此截断；曾有的 sort/truncate 分支是 dead code，已移除。

    // HNSW 是全局索引，无 namespace 维度。按调用者 ns 单趟回查 memories 表取
    // (namespace, content)，Rust 侧过滤 ns（杜绝跨租户泄露）。无 pool 时无法过滤，
    // 保守返回空。
    // #R44 performance/low：ids 排序后再分批——HashMap 键遍历随机序下，部分批失败时
    // 被剔除的 id 子集跨运行不确定（召回损失不可复现、日志难关联）；排序与最终输出
    // 的 memory_id 决胜一致，使失败行为确定。
    // #R45 performance/low：ns 回查与 content 回填**合并为一趟**查询——原两趟各 ~9 次
    // prepare+query（默认 recall_depth=50 时 best ~4096 id、BATCH=500），热路径最多
    // ~18 次串行 SQLite 往返；合并减半且关闭两趟之间的 TOCTOU 窗口（id 在 ns 回查后、
    // content 回填前被并发删除时，原实现静默丢正文）。
    let mut ids: Vec<&str> = best.keys().map(|s| s.as_str()).collect();
    ids.sort_unstable();
    // #R48 performance/medium：ns 过滤推入 SQL（见 fetch_memories_batch doc）——
    // 返回行已属当前 ns，输出循环无需再判 ns。
    let mut rows: HashMap<String, (String, Option<String>)> = match pool {
        Some(p) => fetch_memories_batch(p, &ids, namespace)?,
        None => return Ok(vec![]),
    };

    let mut out: Vec<SignalResult> = Vec::with_capacity(ids.len());
    for memory_id in &ids {
        // 不变量（#R45 maintainability/low）：fetch_memories_batch 未返回的 id（stale/
        // 删除/失败行）在此直接跳过——正文缺失的命中若进入融合会被 rrf_merge 以空
        // 正文锁定（P3-0），不返回候选只损失召回、不污染正文。此前 `unwrap_or_default`
        // 兜底在此**不可达**（死默认掩盖了 P3-0 不变量），已删除。
        let Some((_ns, content)) = rows.remove(*memory_id) else {
            continue;
        };
        // NULL content = 合法数据态，召回损失（剔 id）——与映射失败区分（#R42）。
        let Some(content) = content else {
            continue;
        };
        // #R49 performance/low：`best.get` guard 是死逻辑——ids 直接来自 best.keys()，
        // 每个迭代 id 必然在 best 中（原 guard 掩盖了循环不变量）。rows.remove 移出
        // content（每 id 恰访问一次，省 clone 分配；~8k/查询）。
        let (score, road) = best[*memory_id];
        out.push(SignalResult {
            memory_id: (*memory_id).to_string(),
            content,
            score,
            // 归因：winning road 标记进 source（#R35 maintainability/low）——
            // rrf.rs 的 channel_of 按子串匹配通道，";hype" 后缀不影响现有融合，
            // 但诊断"命中来自内容路还是问句路"成为可能。
            source: if road == "hype" {
                "hnsw_semantic;hype".to_string()
            } else {
                "hnsw_semantic".to_string()
            },
        });
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
            // #R49 performance/medium：search_with_ef 只在 RwLock poisoning（持久条件）
            // 时失败——每查询 eprintln 会刷爆 stderr（与 degraded 日志同款问题）。
            // 60s 冷却：持续故障 ≤1 行/分钟，瞬时抖动只记一次。
            use std::sync::atomic::{AtomicU64, Ordering};
            use std::time::{SystemTime, UNIX_EPOCH};
            const COOLDOWN_SECS: u64 = 60;
            static LAST_ROAD_FAIL_LOG: AtomicU64 = AtomicU64::new(0);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let last = LAST_ROAD_FAIL_LOG.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= COOLDOWN_SECS
                && LAST_ROAD_FAIL_LOG
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                eprintln!(
                    "[semantic] {label} HNSW search failed: {e} (at most once per {COOLDOWN_SECS}s)"
                );
            }
            false
        }
    }
}

/// 单趟批量回查 memories：id → (namespace, content Option<String>)。
///
/// 合并原 lookup_namespaces（ns 回查）与 content backfill（正文回填）两趟 IN(...)
/// 查询为一趟（#R45 performance/low：热路径 SQLite 往返减半——默认 recall_depth=50
/// 时 best ~4096 id、BATCH=500，原两趟共 ~18 次 prepare+query，合并后 ~9 次；且关闭
/// 两趟之间并发删除的 TOCTOU 窗口）。分批 BATCH=500（旧 SQLite 999 变量上限）。
///
/// 错误语义（#R45 bug/medium）：**0 行匹配不算失败**——运行时删除不修剪内存 HNSW
/// （web_api/mcp_server 只删 memories；mem_ad_vec 触发器清向量表，但内存索引到重启/
/// 重建才更新），悬空 id 是预期数据滞后态；用户删光记忆或恢复不带向量的备份后，
/// 全批 stale 是正常结果，升级 Err 会把良性状态误报为系统性故障、刷
/// `[hybrid] semantic signal dropped` 日志。
/// 硬失败 = prepare/query 错误（基础设施），或**所有请求行都返回且全部映射失败**
/// （`row_errors == chunk.len()`：无 stale 掺水，可确认是系统性列漂移）（#R47 bug/low：
/// 仅 `got==0 && row_errors>0` 会把"mostly stale + 单行异常"误判为系统性——一批 500
/// 个请求只有 1 行存在且恰好不可映射时，整批升级会让 hybrid.rs 因单条异常行丢弃
/// 整个语义通道；判定用**实际返回行数**（returned = got + row_errors）而非请求数：
/// 无 stale 掺水（所有请求 id 都返回了行且全部失败）才是确凿的列漂移。**任一**批
/// 硬失败即 Err（#R47 bug/medium：部分失败返回 Ok(partial) 会让调用方把部分召回
/// 损失当完整结果，静默丢失最多 BATCH×N 个候选且不可观测；Err 使 hybrid.rs 记录
/// `semantic signal dropped`，降级可见可查）。
///
/// ns 过滤**推入 SQL**（#R48 performance/medium）：多租户部署下全局候选大部分属
/// 其他 ns，Rust 侧过滤意味着读取/分配大量立即丢弃的 content 全文（MB 级 I/O 与
/// 堆抖动），且跨租户正文被拉过 fetch 边界（虽不返回）。`AND namespace = ?` 保持
/// 单趟往返/TOCTOU 收益且从不读外 ns 行。
fn fetch_memories_batch(
    pool: &SqlitePool,
    ids: &[&str],
    namespace: &str,
) -> Result<HashMap<String, (String, Option<String>)>, String> {
    const BATCH: usize = 500;
    let mut out = HashMap::new();
    let mut batches_total = 0usize;
    let mut hard_failed_batches = 0usize;
    // #R48 performance/medium：stale id 日志**聚合**——内存 HNSW 不随删除修剪
    // （预期滞后），批量删除后每次查询大量 stale id，逐批 eprintln 每查询刷 ~9 行
    // stderr 淹没真实错误。批内只累计计数，函数末尾汇总一行。
    let mut stale_ids_total = 0usize;
    // 行级映射错误同样限流（前 3 条作原因样本，其余计数汇总）。
    let mut row_err_logged = 0usize;
    const ROW_ERR_LOG_CAP: usize = 3;
    for chunk in ids.chunks(BATCH) {
        batches_total += 1;
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id, namespace, content FROM memories WHERE id IN ({}) AND namespace = ?",
            placeholders
        );
        let mut hard_failed = false;
        // #R48 performance/low：连接**每批重新获取**——跨整循环持有会在并发搜索时
        // 把连接钉住 ~9 次往返时长，小池场景加剧耗尽（耗尽即整通道 Err）。
        let conn = pool.get().map_err(|e| format!("semantic fetch pool: {}", e))?;
        match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map(
                rusqlite::params_from_iter(
                    chunk.iter().map(|s| *s).chain(std::iter::once(namespace)),
                ),
                // content 可空（schema `content TEXT` 无 NOT NULL）：Option 读取，
                // NULL 是合法数据态（单独计数），不当作行映射错误（#R42 bug/high：
                // 否则全 NULL 批被误判为列漂移硬失败，触发全批升级）。
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            ) {
                Ok(rows) => {
                    let mut row_errors = 0usize;
                    let mut got = 0usize;
                    for r in rows {
                        match r {
                            Ok((id, ns, content)) => {
                                out.insert(id, (ns, content));
                                got += 1;
                            }
                            Err(e) => {
                                row_errors += 1;
                                if row_err_logged < ROW_ERR_LOG_CAP {
                                    row_err_logged += 1;
                                    eprintln!(
                                        "[semantic] fetch row mapping failed (batch {} ids): {}",
                                        chunk.len(),
                                        e
                                    );
                                }
                            }
                        }
                    }
                    // #R46 bug/medium：行映射失败默认**按行丢弃**。
                    // #R49 bug/high：系统性判定比较 **chunk.len()（请求数）**——
                    // 上一轮改用 `row_errors == returned`（returned = got + row_errors）
                    // 是**重言式**：本分支已要求 got == 0，returned == row_errors 恒真，
                    // 任何"0 成功 + ≥1 映射错误"的批（包括 mostly stale + 单行异常）
                    // 都被误判为系统性列漂移 → 整批 Err → hybrid 丢弃整个语义通道。
                    // 要求"无 stale 掺水"（所有请求 id 都返回了行且全部失败）必须
                    // 比较请求数：`row_errors == chunk.len()`。
                    // 已知权衡（#R48 评论 9 承认）：含 stale 的混合批（部分行全失败
                    // + 部分悬空 id）不会升级——漏判但安全（按行丢弃、日志可见）。
                    if got == 0 && row_errors > 0 {
                        if row_errors == chunk.len() {
                            eprintln!(
                                "[semantic] fetch: all {} requested ids returned rows and ALL failed mapping (systematic column drift)",
                                chunk.len()
                            );
                            hard_failed = true;
                        } else {
                            eprintln!(
                                "[semantic] fetch: batch of {} ids: {row_errors} row(s) failed mapping, 0 ok (row drops only; rest stale)",
                                chunk.len()
                            );
                        }
                    } else if got == 0 {
                        // 0 行无错误 = stale ids（并发删除/悬空 HNSW id）——预期数据
                        // 滞后态，仅计数不升级（#R45 bug/medium，见函数 doc）。
                        stale_ids_total += chunk.len();
                    } else if row_errors > 0 {
                        // #R49 other/low：**混合批**的 stale 也计入——成功行 + 悬空 id
                        // 混存时，未返回的行数（chunk.len() - got - row_errors）此前
                        // 被漏计，聚合诊断系统性低估内存 HNSW 滞后。
                        stale_ids_total += chunk.len() - got - row_errors;
                        eprintln!(
                            "[semantic] fetch: {row_errors} of {} rows failed mapping (dropping those ids)",
                            chunk.len()
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[semantic] fetch query failed (batch {} ids): {}",
                        chunk.len(),
                        e
                    );
                    hard_failed = true;
                }
            },
            Err(e) => {
                eprintln!(
                    "[semantic] fetch prepare failed (batch {} ids): {}",
                    chunk.len(),
                    e
                );
                hard_failed = true;
            }
        }
        if hard_failed {
            hard_failed_batches += 1;
        }
    }
    if stale_ids_total > 0 {
        eprintln!(
            "[semantic] fetch: {stale_ids_total} candidate id(s) matched 0 rows (expected in-memory HNSW lag after deletions)"
        );
    }
    // #R47 bug/medium：**任一**批硬失败即 Err——部分失败返回 Ok(partial) 会让调用方
    // 把召回损失当完整结果（静默丢候选、监控不可见）；Err 使 hybrid.rs 记录
    // `semantic signal dropped`，降级可观测。瞬时 BUSY 重试成本低（下次查询恢复），
    // 选择可观测性优先。
    if hard_failed_batches > 0 {
        return Err(format!(
            "semantic_search: memories fetch hard-failed for {hard_failed_batches} of {batches_total} batches"
        ));
    }
    Ok(out)
}
