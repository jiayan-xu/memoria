//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::QueryCache;
use crate::search::keyword::SignalResult;
use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use std::collections::HashMap;

/// #R56 maintainability/medium：语义检索的**结构化错误**——hybrid.rs 的冷却 key 此前
/// 靠子串匹配 String 错误文本派生（"query vector dim" / "all HNSW roads failed" /
/// fetch 类），文案改动会静默把故障重新归类进 hybrid_drop_other 桶（跨抑制回归，
/// #R54 声称修复的问题在消息层重现）；且 hybrid_drop_db 把 pool get / prepare /
/// query / 硬失败批等**不同** DB 故障合并一 key（并发异因互相压制）。枚举使 key
/// 派生结构化为 match，错误消息可任意改写而不影响分类。
#[derive(Debug)]
#[non_exhaustive]
pub enum SemanticError {
    /// 查询向量维度 ≠ HNSW DIM（嵌入模型/配置漂移）
    QueryDim(usize),
    /// 全部存在的路都失败（RwLock poisoned / 索引损坏）
    /// #R57 maintainability/low：**无 payload**——此前 String 字段恒为
    /// String::new()（死字段）；各路失败详情已由 search_and_merge 的 throttled
    /// eprintln 逐次记录，传播的错误只需分类标识。unit variant 同时消除 Display
    /// 拼接歧义（`{m}` 无分隔符粘贴）。
    RoadsFailed,
    /// memories 回查硬失败（pool/prepare/query/整批硬失败）
    Fetch(String),
}

impl std::fmt::Display for SemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SemanticError::QueryDim(d) => write!(
                f,
                "semantic_search: query vector dim {d} != HNSW DIM {}",
                crate::vector::DIM
            ),
            SemanticError::RoadsFailed => {
                write!(
                    f,
                    "semantic_search: all HNSW roads failed (poisoned/corrupted index)"
                )
            }
            SemanticError::Fetch(m) => write!(f, "semantic_search: memories fetch failed: {m}"),
        }
    }
}

// #R57 maintainability/low：标准 Error trait——pub 错误类型经 crate 公开模块可达，
// 空 impl（无 source 链）使其可用于 Box<dyn Error>/anyhow/thiserror 等泛型错误
// 处理代码。
impl std::error::Error for SemanticError {}

/// #R62 maintainability/low：**语义信号丢弃累计计数**——hybrid 每次 drop 语义通道
/// 时递增；db_stats 暴露该计数，使持续降级（索引损坏/DB 故障）可被监控/健康检查
/// 感知（stderr 冷却行对 MCP/Python 调用方不可见）。
/// #R63 bug/high：**单一模块级 static**——两个函数各自的 function-local static 是
/// 两个独立实例（bump 递增的与 read 读取的不是同一个），计数恒 0、可观测性
/// 死路。
static SEMANTIC_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn bump_semantic_drops() {
    use std::sync::atomic::Ordering;
    SEMANTIC_DROPS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn semantic_drop_count() -> u64 {
    use std::sync::atomic::Ordering;
    SEMANTIC_DROPS.load(Ordering::Relaxed)
}

/// 60s 冷却的 eprintln（#R50/#R51 maintainability/low）：语义检索路径多处诊断日志
/// （road fail / degraded / fetch 批级 / stale 汇总 / hybrid drop）此前各自实现
/// static 冷却——复制到第 4 处时收敛为共享 helper，冷却语义一致；按 **key** 分开
/// 计数（不同故障原因/路/调用点不互相抑制——单个全局 static 会让先失败的路径
/// 永久压住后失败的路径）。
/// #R52/#R53 maintainability/low：**Mutex<HashMap> 无碰撞存储**——固定槽数组
/// （16/64 槽）总有确定性碰撞（DefaultHasher 固定 key，11 个键 64 槽碰撞概率
/// ~58%；碰撞是永久性的，正是 16 槽 FNV 的跨抑制问题）。60s 冷却下锁竞争无关
/// 紧要，per-key 精确隔离。
/// #R54 other/low：**单调时钟（Instant）**——SystemTime 是墙钟，回拨时
/// now < last 使 saturating_sub 恒 < 60s，所有冷却日志被静默压制到时钟追平；
/// 恰在需要可观测性的故障场景失效。Instant 对墙钟调整免疫。
/// #R57 performance/low：**消息闭包化（FnOnce -> String）**——此前调用方总是先
/// format! 分配再传入；hot-path 诊断（roads_degraded 每查询、fetch_stale 多租户
/// 每查询）在冷却期间白付每次查询的堆分配，60s 抑制只限输出不限开销。冷却
/// 通过后才求值闭包，被抑制的调用零分配。
pub(crate) fn throttled_eprintln(key: &'static str, msg: impl FnOnce() -> String) {
    const COOLDOWN_SECS: u64 = 60;
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST_LOGS: Mutex<Option<HashMap<&'static str, Instant>>> = Mutex::new(None);
    let mut guard = LAST_LOGS.lock().unwrap_or_else(|p| p.into_inner());
    let m = guard.get_or_insert_with(HashMap::new);
    // #R55 bug/medium：`Instant::now()` 在**锁内**取——锁外捕获存在竞态：另一线程
    // 可在本线程取 now 后为同一 key 插入更新的时间戳并释放锁，本线程随后
    // `now.duration_since(t)` 在 now < t 时 **panic**（Instant 要求 self >= earlier），
    // panic 沿降级检索路径（并发下恰常触发）传播到请求处理。saturating_
    // duration_since 兜底同型边界。判定后先解锁再打印，锁窗口只覆盖 HashMap 操作。
    let now = Instant::now();
    let due = match m.get(key).copied() {
        Some(t) => now.saturating_duration_since(t).as_secs() >= COOLDOWN_SECS,
        None => true,
    };
    if due {
        m.insert(key, now);
        drop(guard);
        eprintln!("{} (at most once per {COOLDOWN_SECS}s)", msg());
    }
}

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
) -> Result<Vec<SignalResult>, SemanticError> {
    let cache = match query_cache {
        Some(c) => c,
        None => return Ok(vec![]),
    };

    // Get cached embedding from Python (must have been cached via cache_query_vector)
    let vector = match cache.get(query) {
        Some(v) => v,
        None => return Ok(vec![]), // No cached embedding — skip semantic signal
    };

    // #R37 bug/low：维度校验**先于**退化检查（#R51 bug/low）——dim 检查是纯长度比较
    // （廉价、不依赖向量值）；此前退化分支（NaN/全零 → Ok(vec![])）先返回，错维度的
    // 全零/NaN 缓存向量静默产出空结果，配置漂移不发声（dim 检查的初衷就是大声暴露）。
    // query 向量长度 ≠ DIM 时 HNSW 距离计算跑在错配维度上，静默产生垃圾分数（最坏
    // panic 污染索引锁）。离线脚本已防服务器模型漂移（768d），运行时路径同样显式拒绝。
    if vector.len() != crate::vector::DIM {
        // #R56 maintainability/medium：结构化错误（hybrid.rs 按变体派生冷却 key）。
        return Err(SemanticError::QueryDim(vector.len()));
    }

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
    // #R53 performance/low：容量提示——best 上界 ~2×overfetch（默认 recall_depth=50
    // 时 ~4096 条/查询），HashMap::new() 每查询多次 rehash/growth；rows 上界
    // ids.len()（排序后已知）。热路径一致优化的自然补全。
    // #R54 bug/high：**容量 clamp 到实际索引规模**——limit 来自 hybrid.rs
    // primary_limit = recall_depth.max(max_results*3)，max_results 是 MCP
    // memory_search 用户可控参数（无上界钳制）；40×limit 在
    // max_results=1e9 时 ≈120e9：with_capacity 要么 32 位容量溢出 panic、
    // 要么 64 位多 TB 分配 OOM 中止进程——热请求路径。overfetch 本身恒被
    // h.len() 钳制，真实合并集 ≤ 双索引容量；容量提示随索引规模走即可。
    // #R55 performance/low：**4096 floor 曾删除、#R59 恢复**——R55 的理由是默认
    // primary_limit=50 时 40*50=2000 < 4096 地板总胜出、小索引白分配；但
    // search_and_merge 每路 overfetch 有 `.max(2048)` 地板，合并集可达
    // 2×2048=4096，`limit*40` 只覆盖 ~一半，>2000 合并 id 仍触发 rehash。
    // #R55 的担忧已由 `.min(cap_total+2)` 钳制小索引消除，恢复 floor 匹配真实界。
    let cap_total = hnsw.map_or(0, |h| h.len()) + hype_hnsw.map_or(0, |h| h.len());
    let best_cap = (limit as usize)
        .saturating_mul(40)
        .max(4096)
        .min(cap_total.saturating_add(2));
    let mut best: HashMap<String, (f64, &'static str)> = HashMap::with_capacity(best_cap);
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
        // #R56 maintainability/medium：结构化错误（RoadsFailed 变体）。
        return Err(SemanticError::RoadsFailed);
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
    // 瞬时抖动只出一条。冷却经共享 helper（#R51，key 隔离）。
    if roads_failed > 0 && best.is_empty() {
        throttled_eprintln("roads_degraded", || {
            format!(
                "semantic_search: {roads_failed} road(s) failed and survivors returned no matches (degraded)"
            )
        });
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
    // #R53 performance/low：rows 容量由 fetch_memories_batch 内部按 ids.len() 预分配。
    let mut rows: HashMap<String, Option<String>> = match pool {
        Some(p) => fetch_memories_batch(p, &ids, namespace).map_err(SemanticError::Fetch)?,
        None => return Ok(vec![]),
    };

    let mut out: Vec<SignalResult> = Vec::with_capacity(ids.len());
    // #R61 performance/low：**drain best 移动 String**——`(*memory_id).to_string()`
    // 每候选 clone 一次（热路径 ~8k/查询）；best 在此循环后即弃，且最终
    // out.sort_by 使迭代顺序无关结果。先 drop(ids) 释放借用，再 for 移动
    // best 的 owned String 进 SignalResult（每行省一次堆分配）。
    drop(ids);
    for (memory_id, (score, road)) in best {
        // 不变量（#R45 maintainability/low）：fetch_memories_batch 未返回的 id（stale/
        // 删除/失败行）在此直接跳过——正文缺失的命中若进入融合会被 rrf_merge 以空
        // 正文锁定（P3-0），不返回候选只损失召回、不污染正文。此前 `unwrap_or_default`
        // 兜底在此**不可达**（死默认掩盖了 P3-0 不变量），已删除。
        // #R58 performance/low：行元组已去 namespace（SQL 过滤保证），直接解构。
        let Some(content) = rows.remove(&memory_id) else {
            continue;
        };
        // NULL content = 合法数据态，召回损失（剔 id）——与映射失败区分（#R42）。
        let Some(content) = content else {
            continue;
        };
        out.push(SignalResult {
            memory_id,
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
            // #R50/#R51 maintainability/low：冷却按**路**分离（key=label）——单个
            // 全局 static 在 content/hype 双路 60s 内相继失败时只记首路；共享 helper
            // 的 key 隔离天然满足，无需手写双 static。
            throttled_eprintln(label, || {
                format!("[semantic] {label} HNSW search failed: {e}")
            });
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
) -> Result<HashMap<String, Option<String>>, String> {
    // #R58 performance/low：SQL 已按 namespace 过滤（AND namespace = ?），返回行的
    // namespace 恒等于请求值——SELECT 与返回元组中的 namespace 是**死数据**（调用方
    // 立即丢弃 `let Some((_ns, content))`）。热路径每行少读一列、少包一层元组。
    const BATCH: usize = 500;
    // #R53 performance/low：容量 = ids.len()（上界已知，避免 growth/rehash）。
    let mut out = HashMap::with_capacity(ids.len());
    let mut batches_total = 0usize;
    let mut hard_failed_batches = 0usize;
    // #R48 performance/medium：stale id 日志**聚合**——内存 HNSW 不随删除修剪
    // （预期滞后），批量删除后每次查询大量 stale id，逐批 eprintln 每查询刷 ~9 行
    // stderr 淹没真实错误。批内只累计计数，函数末尾汇总一行。
    // #R50/#R51 performance/medium：批级诊断日志（prepare/query 失败、混合批、stale
    // 汇总、行映射样本）统一走共享 60s 冷却 helper——持久故障时每查询 ~9 批 ×
    // 每批 1 行刷爆 stderr；硬失败场景由 Err 传播 + hybrid 的 60s 冷却兜底可观测。
    // 行映射样本（具体错误文本）经同一冷却，60s 窗口内保留最近一条（#R51
    // performance/medium：无冷却时 cap-3 样本每查询刷 3 行）。
    let mut stale_ids_total = 0usize;
    // #R61：首个硬失败（prepare/query）的底层错误文本（见函数尾 Err 组装）。
    let mut first_hard_err: Option<String> = None;
    for chunk in ids.chunks(BATCH) {
        batches_total += 1;
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id, content FROM memories WHERE id IN ({}) AND namespace = ?",
            placeholders
        );
        let mut hard_failed = false;
        let mut row_errors = 0usize;
        let mut got = 0usize;
        // #R48 performance/low：连接**每批重新获取**——跨整循环持有会在并发搜索时
        // 把连接钉住 ~9 次往返时长，小池场景加剧耗尽（耗尽即整通道 Err）。
        // #R62 maintainability/low：pool 耗尽也走**统一聚合**（此前 `?` 直返绕过
        // hard_failed_batches 汇总与 stale 汇总——与 #R47/#R61 的"所有基础设施
        // 失败都进汇总 Err"契约不一致）。
        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => {
                if first_hard_err.is_none() {
                    first_hard_err = Some(format!("pool: {e}"));
                }
                throttled_eprintln("fetch_pool", || {
                    format!("pool get failed (batch {} ids): {}", chunk.len(), e)
                });
                hard_failed = true;
                // #R63 bug/medium：break 前**计入硬失败批**——函数尾按
                // hard_failed_batches 汇总（此前漏计 → 返回 Ok(partial) 而非 Err，
                // #R47"任一硬失败即 Err"契约被绕过，hybrid 视作成功低召回查询）。
                hard_failed_batches += 1;
                break;
            }
        };
        match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map(
                rusqlite::params_from_iter(
                    chunk.iter().map(|s| *s).chain(std::iter::once(namespace)),
                ),
                // content 可空（schema `content TEXT` 无 NOT NULL）：Option 读取，
                // NULL 是合法数据态（单独计数），不当作行映射错误（#R42 bug/high：
                // 否则全 NULL 批被误判为列漂移硬失败，触发全批升级）。
                |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                },
            ) {
                Ok(rows) => {
                    for r in rows {
                        match r {
                            Ok((id, content)) => {
                                out.insert(id, content);
                                got += 1;
                            }
                            Err(e) => {
                                row_errors += 1;
                                // #R51 performance/medium：样本行也走共享冷却——
                                // 无冷却时 cap-3 样本在每查询刷 3 行（mostly stale +
                                // 单行不可映射的常见混合态）。
                                throttled_eprintln("fetch_row_mapping", || {
                                    format!(
                                        "[semantic] fetch row mapping failed (batch {} ids): {}",
                                        chunk.len(),
                                        e
                                    )
                                });
                            }
                        }
                    }
                    // #R46 bug/medium：行映射失败默认**按行丢弃**。
                    // #R49 bug/high：系统性判定比较 **chunk.len()（请求数）**——
                    // `row_errors == returned`（returned = got + row_errors）是重言式
                    // （本分支已要求 got == 0），任何"0 成功 + ≥1 映射错误"的批
                    // （含 mostly stale + 单行异常）都会被误判为系统性列漂移。
                    // 要求"无 stale 掺水"（所有请求 id 都返回了行且全部失败）必须
                    // 比较请求数；含 stale 的混合批不升级——漏判但安全（#R48 承认）。
                    if got == 0 && row_errors > 0 {
                        if row_errors == chunk.len() {
                            if first_hard_err.is_none() {
                                // 行映射错误的文本在 Err(e) 分支可见——用行级 e 的
                                // 内容不直接可得（循环外），记录类别级说明。
                                first_hard_err = Some(format!(
                                    "systematic column drift (all {} returned rows failed mapping)",
                                    chunk.len()
                                ));
                            }
                            throttled_eprintln("fetch_systemic", || {
                                format!(
                                    "all {} requested ids returned rows and ALL failed mapping (systematic column drift)",
                                    chunk.len()
                                )
                            });
                            hard_failed = true;
                        } else {
                            throttled_eprintln("fetch_mixed", || {
                                format!(
                                    "batch of {} ids: {row_errors} row(s) failed mapping, 0 ok (row drops only; rest stale)",
                                    chunk.len()
                                )
                            });
                        }
                    } else if row_errors > 0 {
                        throttled_eprintln("fetch_partial", || {
                            format!(
                                "{row_errors} of {} rows failed mapping (dropping those ids)",
                                chunk.len()
                            )
                        });
                    }
                }
                Err(e) => {
                    if first_hard_err.is_none() {
                        first_hard_err = Some(format!("query: {e}"));
                    }
                    throttled_eprintln("fetch_query", || {
                        format!("query failed (batch {} ids): {}", chunk.len(), e)
                    });
                    hard_failed = true;
                }
            },
            Err(e) => {
                if first_hard_err.is_none() {
                    first_hard_err = Some(format!("prepare: {e}"));
                }
                throttled_eprintln("fetch_prepare", || {
                    format!("prepare failed (batch {} ids): {}", chunk.len(), e)
                });
                hard_failed = true;
            }
        }
        // #R50 maintainability/low：stale 计数**统一**——每 chunk 末尾计算未返回
        // 行数（saturating 防下溢），覆盖全部混合形态：全 stale（got==0,
        // row_errors==0 → 全计）、部分匹配（got>0 → 计缺口，此前漏计）、失败批
        // 混合（row_errors>0 → 计缺口）。聚合诊断与真实内存 HNSW 滞后一致。
        // #R51 bug/low：**硬失败批不计入 stale**——prepare/query 失败时 got==0 &&
        // row_errors==0，全计会把这批缺失归因于"预期内存 HNSW 滞后"，而真实原因是
        // 基础设施故障（且函数随即 Err）——良性态与硬失败混在一个诊断里误导运维。
        if !hard_failed {
            stale_ids_total += chunk.len().saturating_sub(got + row_errors);
        }
        if hard_failed {
            hard_failed_batches += 1;
        }
    }
    if stale_ids_total > 0 {
        throttled_eprintln("fetch_stale", || {
            // #R54 other/low：0 行有两类原因——跨 ns 候选（查询 SQL 已按 ns 过滤，
            // 全局 HNSW 过取窗口在多租户下大部分是外 ns 向量，属**设计的过滤行为**）
            // 与删除滞后（悬空 id）。文案并列两因，避免把正常跨租户过取误归因为
            // 删除滞后、驱动错误的召回排查。
            format!(
                "{stale_ids_total} candidate id(s) matched 0 rows in this ns (cross-tenant overfetch or deletion lag)"
            )
        });
    }
    // #R47 bug/medium：**任一**批硬失败即 Err——部分失败返回 Ok(partial) 会让调用方
    // 把召回损失当完整结果（静默丢候选、监控不可见）；Err 使 hybrid.rs 记录
    // `semantic signal dropped`，降级可观测。瞬时 BUSY 重试成本低（下次查询恢复），
    // 选择可观测性优先。
    if hard_failed_batches > 0 {
        // #R61 maintainability/medium：**首个底层错误嵌入 Err**——prepare/query 的
        // 具体 rusqlite 错误此前只出现在 60s 冷却的 stderr 行；hybrid.rs 每查询
        // 打 `semantic signal dropped: {e}` 却无根因。first_hard_err 在失败分支
        // 记录首个错误文本，跨过错误边界（operator 读传播日志即可定位）。
        let detail = first_hard_err
            .as_deref()
            .map(|d| format!(" (first error: {d})"))
            .unwrap_or_default();
        return Err(format!(
            "memories fetch hard-failed for {hard_failed_batches} of {batches_total} batches{detail}"
        ));
    }
    Ok(out)
}
