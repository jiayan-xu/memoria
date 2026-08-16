//! Memoria Core — Rust memory engine
#![allow(dead_code)]

use std::sync::Arc;

pub mod auth;
pub mod backup;
pub mod document;
pub mod health;
pub mod ontology;
pub mod quota;
pub mod search;
pub mod session_watcher;
pub mod storage;
pub mod tools;
pub mod vector;
pub mod web_api;

use storage::SqlitePool;
use vector::{HnswIndex, QueryCache, VectorEntry};

/// MemoriaEngine — cross-platform memory engine.
/// Methods return Result<String, String> for both Python and standalone use.
pub struct MemoriaEngine {
    pub db_path: String,
    pub pool: Arc<SqlitePool>,
    pub hnsw: HnswIndex,
    /// V1（2026-08-12）：HyPE 假设问句向量索引（可选）。由 memory_hype_vectors 表重建，
    /// semantic_search 双路合并（取 max）。空索引=功能未启用，检索退化为单路内容向量。
    /// **限制（#R37 maintainability/low）**：仅在构造时从表一次性构建；引擎存活期间
    /// memory_hype_vectors 被外部写入（如离线脚本重跑、未来工具运行时写 hype 向量）时，
    /// 本索引不会自动刷新——需重建引擎/重启才能加载新向量。
    /// **#R49 documentation/low 运行时刷新**：在**已填充**索引上调用
    /// `persist::rebuild_hype_hnsw_from_store` 只**追加新 id**（HnswIndex::add 按 id 去重，
    /// 已存在 id 的向量更新被静默忽略——见 persist.rs #R44）。要拾取已存在 id 的更新
    /// 向量，必须全新索引替换：`let fresh = HnswIndex::new();
    /// fresh.set_ef_search(resolve_ef_search()); rebuild_hype_hnsw_from_store(&pool, &fresh)?;
    /// engine.hype_hnsw = fresh;`（两索引交替重建避免查询间隙）。
    pub hype_hnsw: HnswIndex,
    pub query_cache: QueryCache,
}

impl MemoriaEngine {
    pub fn new(db_path: &str) -> Result<Self, String> {
        // P2-11：SQLite 连接池大小可经 MEMORIA_DB_POOL_SIZE 覆盖（默认 4，范围 1..=64）。
        let pool_size: u32 = std::env::var("MEMORIA_DB_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&n| (1..=64).contains(&n))
            .unwrap_or(4);
        let pool = storage::create_pool(db_path, pool_size)?;
        storage::init_schema(&pool)?;
        storage::init_core_tables(&pool)?;
        // 迁移必须随引擎自洽（之前仅在 main.rs 调用，导致 lib/MemoriaEngine 路径下
        // superseded_by 等列缺失，近义去重静默失效）。统一在此收口，避免入口分叉。
        storage::migrate_superseded_by(&pool)?;
        storage::migrate_event_time(&pool)?;
        storage::migrate_user_prefs_namespace(&pool)?;
        storage::migrate_dream_state_ns(&pool)?;
        storage::migrate_temporal(&pool)?;
        storage::migrate_extract_fields(&pool)?;
        storage::migrate_evolution(&pool)?;
        storage::migrate_memory_relation_types(&pool)?;
        // P2-2：配额计数表随引擎自洽（与 main.rs 一致，避免 lib/MemoriaEngine 路径缺表）
        quota::init_quota_table(&pool)?;

        let vec_path = std::path::Path::new(db_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("vector_index")
            .join("hnsw_vectors");
        let hnsw = if HnswIndex::exists(&vec_path) {
            HnswIndex::load(&vec_path).unwrap_or_else(|e| {
                eprintln!("HNSW load: {}", e);
                HnswIndex::new()
            })
        } else {
            HnswIndex::new()
        };

        // P1-3：以 memory_vectors 持久表为权威源重建 HNSW（.bin 仅作可选快取）。
        // 即使 .bin 缺失/损坏或 QueryCache 进程内丢失，近义去重仍可跨重启可靠工作。
        if let Err(e) = vector::persist::rebuild_hnsw_from_store(&pool, &hnsw) {
            eprintln!("HNSW rebuild from memory_vectors: {}", e);
        }

        // V1（2026-08-12）：HyPE 问句向量索引（独立实例，memory_hype_vectors 为权威源）。
        // 表为空 → 索引空 → semantic_search 双路退化单路，行为与未启用一致（向后兼容）。
        // ef_search 解析与 hype 构造均走 persist 收口（与 main.rs 共用），
        // 避免两入口 ef 行为分裂（迁移收口的同一理由）。
        let ef_search = vector::persist::resolve_ef_search();
        hnsw.set_ef_search(ef_search);
        // 与 main.rs 共用 build_hype_hnsw_or_default（build + 软降级 + WARN 单一实现）：
        // 库路径不因 HyPE 故障 panic（Python bindings 宿主健壮性）。
        let (hype_hnsw, hype_count) = vector::persist::build_hype_hnsw_or_default(&pool, ef_search);
        // #R58 style/low：**成功路径静默**——MemoriaEngine::new 是库入口（Python
        // bindings / per-test 引擎创建），无条件打印让每个实例构造都刷一行 stderr
        // （含 n=0）；build_hype_hnsw_or_default 内部已有降级 WARN，空表（未启用）
        // 由 db_stats 的 store/live 对照呈现，无需构造期噪音。仅在 n==0 且表非空
        // （构建降级）时由 helper 的 WARN 覆盖——此处不重复。
        let _ = hype_count;

        Ok(Self {
            db_path: db_path.to_string(),
            pool: Arc::new(pool),
            hnsw,
            hype_hnsw,
            query_cache: QueryCache::new(),
        })
    }

    /// #R50 maintainability/medium：**运行时刷新 HyPE 索引**——hype_hnsw 是构造时
    /// 快照，引擎存活期间 memory_hype_vectors 被外部写入（离线脚本重跑、未来工具
    /// 运行时写）不会自动反映；此前只能重启，文档里的手动方案（新建索引+重建+swap）
    /// 不是引擎方法、Python bindings 调不到。本方法执行"全新索引重建 → 整体替换"：
    /// #R69 documentation/low（与 persist.rs #R44 doc 修订同步）：hype 表经
    /// require_fresh guard 对已填充索引**返回 Err**（见 rebuild_from_table #R62），
    /// 不存在"in-place 只追加新 id"路径——已存在 id 的向量更新必须全新索引拾取。
    /// 返回新索引加载的向量数。
    /// #R52 maintainability/medium：用 **build_hype_hnsw（Result 版）**而非
    /// `build_hype_hnsw_or_default`——or_default 把所有失败吸收成空索引 + WARN，
    /// 本方法**永不返回 Err**（签名误导）：调用方（Python bindings/运维工具）无法
    /// 程序化区分"刷新成功"与"降级空重建"，count==0 同时可能是"未启用"或"失败"。
    /// Err 路径真实可达：失败保留旧快照（检索不中断）并向调用方显式报错。
    /// #R53 maintainability/low 已知取舍：要求 `&mut self`——`build_hype_hnsw` 全量
    /// 重读+重索引表（大表秒级），Arc<Mutex> 宿主在刷新期间持锁会阻塞并发检索。
    /// **推荐模式（#R54 performance/medium）**：构建在锁外进行——`persist::
    /// build_hype_hnsw(&pool, ef)` 是自由函数只取 &pool，宿主可在无锁下构建新索引，
    /// 然后仅持锁执行 O(1) swap（`engine.hype_hnsw = fresh`）——锁内窗口从"秒级
    /// 构建"缩到"一次赋值"。本方法（&mut self 形态）面向简单宿主，构建在锁内。
    /// #R54 maintainability/low：刷新时复用 **content 索引当前的 ef**（self.hnsw
    /// 构造时解析并应用）——重新解析 env 会在长驻进程里 env 变更后让双路 ef 分裂
    /// （content 保持构造时值、hype 用新值），违背 ef 对齐目标。
    /// #R60 maintainability/medium：**两阶段 API**——`build_hype_index_fresh`（&self，
    /// 锁外/无独占借用下构建新索引）与 `swap_hype_index`（&mut self，O(1) 赋值）分离：
    /// Arc<Mutex> 宿主与多线程 Python 宿主可在**构建期间保持并发检索**，锁内窗口从
    /// "秒级构建"缩到"一次赋值"。refresh_hype_index 保留为简单宿主的一步封装。
    /// #R69 bug/medium：**两阶段路径同严**——R48 指出此前本函数走 build_hype_hnsw
    /// （丢弃 read_errors/skipped）：单行读取失败/跳过被吸收为 Ok(partial)，swap
    /// 无 mismatch（count==fresh.len()）静默安装缺失行索引。改用 detailed 构建器，
    /// 任一降级信号 >0 即 Err——与 refresh_hype_index 的门控一致。
    pub fn build_hype_index_fresh(&self) -> Result<(HnswIndex, usize), String> {
        // 复用 content 索引当前的 ef（#R54：避免 env 重解析导致双路 ef 分裂）。
        let ef_search = self.hnsw.ef_search();
        let (fresh, count, read_errors, skipped) =
            vector::persist::build_hype_hnsw_detailed(&self.pool, ef_search)?;
        if read_errors > 0 || skipped > 0 {
            return Err(format!(
                "HYPE HNSW build: partial rebuild ({read_errors} read error(s), {skipped} skipped row(s)); refusing to publish degraded index"
            ));
        }
        Ok((fresh, count))
    }
    pub fn swap_hype_index(&mut self, fresh: HnswIndex, count: usize) {
        // #R63 maintainability/low：**API 边界校验**——count 与 fresh.len() 不匹配的
        // 发布会让 db_stats 的降级检测启发（store>0 && live==0）被歪曲；调用方
        // 传错时如实报告（Python 两阶段流程经 token 保护不受影响）。
        if count != fresh.len() {
            eprintln!(
                "[Memoria] WARN: swap_hype_index count mismatch (count={count}, index={}); reporting actual index len",
                fresh.len()
            );
        }
        let count = fresh.len();
        self.hype_hnsw = fresh;
        // #R65 maintainability/low：count==0 不打印（构造路径同款纪律 #R58——
        // 空表周期刷新刷屏）。
        if count > 0 {
            eprintln!("[Memoria] HYPE HNSW refreshed: {} vectors", count);
        }
    }

    pub fn refresh_hype_index(&mut self) -> Result<usize, String> {
        // #R69 bug/medium：**部分重建不替换健康快照**——build_hype_hnsw 的 Err
        // 只覆盖全损；单行读取错误（NULL/类型漂移 blob）被计数 + WARN 吸收为
        // Ok(partial)。refresh 若用降级索引 swap 掉健康快照，双路语义检索静默
        // 退化为单路且无错误上报。detailed 版暴露 read_errors/skipped：任一
        // >0 时保留旧快照并 Err（显式告知部分重建，调用方可决定重试/人工处理；
        // skipped 含 dim 漂移/degenerate/corrupt 跳过行——比 read_errors 更
        // 常见的降级信号，R48 指出此前只查 read_errors 会漏掉维度漂移场景）。
        let (fresh, count, read_errors, skipped) = match vector::persist::build_hype_hnsw_detailed(
            &self.pool,
            self.hnsw.ef_search(),
        ) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[Memoria] HYPE HNSW refresh failed, keeping existing index: {e}");
                return Err(e);
            }
        };
        if read_errors > 0 || skipped > 0 {
            let msg = format!(
                "HYPE HNSW refresh: partial rebuild ({read_errors} read error(s), {skipped} skipped row(s)); keeping existing index"
            );
            eprintln!("[Memoria] WARN: {msg}");
            return Err(msg);
        }
        // 仅构建成功（且无部分重建）才替换；失败保留旧快照，检索不中断。
        // #R65 bug/medium：pending 槽的失效由 PyEngine::refresh_hype_index 处理
        // （字段属 PyEngine——两阶段 build/swap 与一步 refresh 不得混用，见其 doc）。
        // #R66 bug/low：返回**实际应用长度**（swap 内部对 count 不匹配做了修正——
        // 返回原始 count 会与 hype_vector_index_live 不一致）。
        let applied = fresh.len();
        self.swap_hype_index(fresh, count);
        Ok(applied)
    }

    pub fn hybrid_search(
        &self,
        query: &str,
        max_results: u32,
        _intent: &str,
        namespace: &str,
        _tier: &str,
        include_superseded: bool,
    ) -> Result<String, String> {
        let results = search::hybrid::hybrid_search(
            &self.pool,
            query,
            namespace,
            max_results,
            Some(&self.hnsw),
            Some(&self.hype_hnsw),
            Some(&self.query_cache),
            None,
            include_superseded,
        )?;
        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "memory_id": r.memory_id,
                    "content": truncate(&r.content, 200),
                    "rrf_score": r.rrf_score,
                    "source": r.source,
                    "evolved_at": r.evolved_at,
                    "pending_evolution": r.pending_evolution,
                })
            })
            .collect();
        serde_json::to_string(&serde_json::json!({
            "status": "completed",
            "total_results": results.len(),
            "results": items,
        }))
        .map_err(|e| e.to_string())
    }

    pub fn db_stats(&self) -> Result<String, String> {
        let conn = self.pool.get().map_err(|e| format!("pool: {}", e))?;
        let tables = [
            "memories",
            "messages",
            "sessions",
            "decisions",
            "user_prefs",
            "memory_relations",
            "decay_log",
            "dream_state",
        ];
        let mut m = serde_json::Map::new();
        for t in &tables {
            let c: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                .unwrap_or(0);
            m.insert(t.to_string(), serde_json::Value::Number(c.into()));
        }
        m.insert(
            "vector_index_size".to_string(),
            serde_json::Value::Number((self.hnsw.len() as i64).into()),
        );
        // V1（#R37 maintainability/low）：HyPE 索引规模也纳入公开统计——
        // 否则 Python/standalone 宿主只能从启动 eprintln 行观察，API 无感知。
        // #R50/#R54 maintainability/low：**同时暴露 store 行数与 live 索引 len**——
        // 只报 store 会在 HyPE 构建降级（or_default 失败 → 内存索引空而表有行）时
        // 返回正值、看起来健康（API-only 宿主不解析启动 WARN 就无从发现单路退化）。
        // 双字段对照：store > 0 且 live == 0 = 构建降级，一眼可见。
        // 查询失败显式标记为 -1（#R51：缺表/锁不被伪装成合理数字）。
        // #R55 performance/low：**COUNT 结果 30s 缓存**——db_stats 每调用一次
        // O(n) 全表扫描（在 8 个既有 COUNT 之上），监控循环高频轮询时逐次放大；
        // 缓存由内存 HNSW 滞后语义兜底（live len 仍实时）。失败 WARN 走 60s 冷却
        // ——持续故障下每调用刷一行会把 stderr 淹没在同一行里。
        let hype_store = query_hype_count_cached(&conn, &self.db_path);
        m.insert(
            "hype_vector_index_size".to_string(),
            serde_json::Value::Number(hype_store.into()),
        );
        m.insert(
            "hype_vector_index_live".to_string(),
            serde_json::Value::Number((self.hype_hnsw.len() as i64).into()),
        );
        m.insert(
            "query_cache_size".to_string(),
            serde_json::Value::Number((self.query_cache.len() as i64).into()),
        );
        // #R62 maintainability/low：**语义信号丢弃累计计数**——hybrid drop 语义
        // 通道时递增（semantic::bump_semantic_drops）；持续降级（索引损坏/DB
        // 故障）不再只对 tail stderr 的人可见，健康检查可据此告警。
        // #R64 other/low：字段名披露**进程级全局**作用域——多引擎进程（多 PyEngine）
        // 中此计数合并所有引擎，per-engine 健康监控按名可辨。
        m.insert(
            "semantic_signal_drops_global".to_string(),
            serde_json::Value::Number(
                (search::semantic::semantic_drop_count() as i64).into(),
            ),
        );
        serde_json::to_string(&serde_json::Value::Object(m)).map_err(|e| e.to_string())
    }

    pub fn add_vectors(&self, ids: Vec<String>, vectors: Vec<Vec<f32>>) -> Result<usize, String> {
        if ids.len() != vectors.len() {
            return Err("ids/vectors length mismatch".to_string());
        }
        let entries: Vec<VectorEntry> = ids
            .iter()
            .cloned()
            .zip(vectors.iter().cloned())
            .map(|(id, v)| VectorEntry { id, vector: v })
            .collect();
        let added = self.hnsw.add(&entries)?;
        // P1-3：批量向量也落 memory_vectors 表，统一为权威持久源（namespace 取自 memories）。
        for (id, v) in ids.iter().zip(vectors.iter()) {
            let ns = vector::persist::lookup_namespace(&self.pool, id)
                .unwrap_or_else(|| "default".to_string());
            let _ = vector::persist::put_stored_vector(&self.pool, id, &ns, v);
        }
        Ok(added)
    }

    pub fn vector_search(&self, qv: Vec<f32>, k: u32) -> Result<String, String> {
        serde_json::to_string(&self.hnsw.search(&qv, k as usize)?).map_err(|e| e.to_string())
    }

    pub fn vector_count(&self) -> usize {
        self.hnsw.len()
    }
    pub fn cache_query_vector(&self, text: &str, v: Vec<f32>) {
        self.query_cache.put(text, v);
    }
    pub fn get_cached_query_vector(&self, text: &str) -> Option<Vec<f32>> {
        self.query_cache.get(text)
    }

    pub fn save_index(&self) -> Result<(), String> {
        if self.db_path == ":memory:" {
            return Ok(());
        }
        let p = std::path::Path::new(&self.db_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("vector_index")
            .join("hnsw_vectors");
        self.hnsw.save(&p)
    }
}

/// #R55 performance/low：hype 表行数的**缓存查询**（30s TTL）——db_stats 每调用
/// 一次 O(n) COUNT 全表扫描，监控轮询高频调用时成本逐次放大；表由离线脚本在
/// 服务外写入，30s 陈旧可接受（live 索引 len 在 stats 中仍实时）。查询失败
/// WARN 60s 冷却——持续故障（缺表/锁）下每调用刷一行会把 stderr 淹没在同一行。
/// #R56 bug/medium：**缓存按 db 身份键控**——进程级 static 若不区分 db，同进程
/// 内多引擎实例（多 PyEngine / 测试 recreate 模式）会让先填缓存的那个实例把
/// 自己的行数供给所有其他实例最多 30s；`hype_vector_index_size` 恰是检测 HyPE
/// 构建降级（store>0 && live==0）的关键指标，错值会让除第一个外的所有库误判。
/// 键 = db_path（进程内实例的辨识身份）。`Instant::now()` 在**锁内**取并用
/// saturating_duration_since——锁外捕获存在竞态：对端线程可先写入更新的时间戳，
/// 本线程 `now.duration_since(*at)` 在 now < at 时 panic（与 semantic.rs #R55
/// 同款竞态，db_stats 是库路径，panic 不可接受）。
/// #R57 细节：
/// - **失败也缓存（-1，同 30s TTL）**——此前 Err 分支不写缓存，`*v != -1` 守卫是
///   死代码：持续失败（软降级 DDL 后缺表/锁）时每次 db_stats 都重跑失败的 COUNT，
///   监控轮询对已降级库叠加负载。缓存 -1 后恢复可见性延迟 ≤30s（TTL 语义一致）。
/// - **失败冷却按 db_path 键控**——进程级单冷却会让一库失败压制所有库的 WARN
///   60s；WARN 消息含 db_path，故障定位直接。
/// - **缓存条目随读写淘汰**——>30s TTL 的条目在下次访问时清除，多 db 路径
///   进程（per-test 临时库）不累积。
fn query_hype_count_cached(conn: &rusqlite::Connection, db_path: &str) -> i64 {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;
    // #R64 bug/medium：**:memory: 跳过缓存**——db_path 对内存引擎不是唯一身份
    // （进程内多个 :memory: 引擎各自独立库却共享键 "…memory…"）：A 填充的 count
    // 会被 B 读到（降级检测启发 store>0 && live==0 被污染）。文件路径删除重建
    // 同路径 30s 内的陈旧也一并规避（直接查成本可接受）。
    // #R65 style/low：显式括号（A || (B && C) 依赖 Rust 优先级易误读）；注释不夸大
    // （文件路径删除重建的 30s 陈旧仍由缓存路径承担——此分支只覆盖内存标识）。
    // #R66 bug/low：**FAIL_EPOCHS 冷却以 ":memory:" 为共享键**——多内存引擎一库
    // 失败会静默其他引擎 WARN 60s（缓存已按引擎跳过、冷却无 per-engine 身份可
    // 用）；接受为已知限制（内存引擎通常单例/低频），注释留痕。
    if db_path.starts_with(":memory:")
        || (db_path.starts_with("file:") && db_path.contains("mode=memory"))
    {
        // #R65 maintainability/low：:memory: 分支 WARN 也走 60s 冷却（轮询刷屏
        // 与缓存路径同款纪律）。
        // #R67 performance/low：**同样修剪过期条目**（此前只插不删——多内存 URI
        // 各失败一次长期累积）。
        {
            let now = Instant::now();
            if let Ok(mut fe) = FAIL_EPOCHS.lock() {
                if let Some(fmap) = fe.as_mut() {
                    fmap.retain(|_, at| now.saturating_duration_since(*at).as_secs() < 60);
                }
            }
        }
        return match conn.query_row(
            &format!("SELECT COUNT(*) FROM {}", crate::storage::MEMORY_HYPE_VECTORS_TABLE),
            [],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(c) => c,
            Err(e) => {
                // #R69 bug/low：`Instant::now()` 在**锁内**取（#R56/#R66 纪律）——
                // 锁外采样时另一线程可在本线程取 now 后、本线程拿到锁前为同一
                // db_path 插入更新的时间戳：更晚的时间戳使 saturating_duration_
                // since 得 ~0（<60s），冷却被静默延长；更早的使条目被修剪、WARN
                // 提前多发。锁窗口内采样保证判定与插入同一时钟视图。
                let mut fe = FAIL_EPOCHS.lock().unwrap_or_else(|p| p.into_inner());
                let now = Instant::now();
                let fmap = fe.get_or_insert_with(HashMap::new);
                let last = fmap.get(db_path).copied();
                let due = match last {
                    Some(at) => now.saturating_duration_since(at).as_secs() >= 60,
                    None => true,
                };
                if due {
                    fmap.insert(db_path.to_string(), now);
                    drop(fe);
                    eprintln!(
                        "[Memoria] WARN: hype_vector_index_size query failed (db {db_path}): {e}"
                    );
                }
                -1
            }
        };
    }
    static CACHE: Mutex<Option<HashMap<String, (Instant, i64)>>> = Mutex::new(None);
    static FAIL_EPOCHS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    // #R65 performance/medium：**FAIL_EPOCHS 每次调用修剪**（此前只错误路径——大量
    // db_path 各失败一次的长期进程条目永积，违背 #R57 不累积意图）。
    {
        if let Ok(mut fe) = FAIL_EPOCHS.lock() {
            // #R66 bug/low：now 在**锁内**取（#R56 纪律——锁外采样在交错窗口
            // 给出偏移 TTL 判定；未来换 duration_since 会重演 panic 风险）。
            let now = Instant::now();
            if let Some(fmap) = fe.as_mut() {
                fmap.retain(|_, at| now.saturating_duration_since(*at).as_secs() < 60);
            }
        }
    }
    {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        let m = cache.get_or_insert_with(HashMap::new);
        // 淘汰过期条目（写入路径顺带维护，避免无界累积）。
        let now = Instant::now();
        m.retain(|_, (at, _)| now.saturating_duration_since(*at).as_secs() < 30);
        if let Some((at, v)) = m.get(db_path) {
            let now = Instant::now();
            if now.saturating_duration_since(*at).as_secs() < 30 {
                return *v;
            }
        }
    }
    // #R69 maintainability/low：表名走共享常量（同 :memory: 分支——rename/迁移
    // 只改 storage::MEMORY_HYPE_VECTORS_TABLE 一处）。
    let r = conn.query_row(
        &format!("SELECT COUNT(*) FROM {}", crate::storage::MEMORY_HYPE_VECTORS_TABLE),
        [],
        |r| r.get::<_, i64>(0),
    );
    match r {
        Ok(c) => {
            let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
            let m = cache.get_or_insert_with(HashMap::new);
            m.insert(db_path.to_string(), (Instant::now(), c));
            c
        }
        Err(e) => {
            // 失败也缓存（-1，同 TTL）——见函数 doc #R57。
            // #R58 bug/medium：**-1 不覆盖新鲜成功值**——check-then-act 窗口内对端
            // 线程可能刚写入成功（本线程 miss 缓存后才跑 COUNT）；瞬时失败（BUSY）
            // 用 -1 覆盖成功会污染 `hype_vector_index_size`（store>0 && live==0 的
            // 降级检测指标）最多 30s。仅当无非 stale 成功值时才写 -1。
            // #R59 performance/low：CACHE guard **尽早释放**——此前 guard 存活到
            // FAIL_EPOCHS.lock() 与 eprintln 之后：stderr 重定向到慢管道时写阻塞
            // 会持 CACHE 锁卡住所有 db_stats 调用者；双锁嵌套还引入锁序约束。
            // 作用域只覆盖 preserve_success 检查 + 插入。
            {
                let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
                let m = cache.get_or_insert_with(HashMap::new);
                let preserve_success = match m.get(db_path) {
                    Some((at, v)) if *v != -1 && {
                        let now = Instant::now();
                        now.saturating_duration_since(*at).as_secs() < 30
                    } => true,
                    _ => false,
                };
                if !preserve_success {
                    m.insert(db_path.to_string(), (Instant::now(), -1));
                }
            }
            // 失败冷却按 db_path 键控（#R57）；#R58 bug/low：**条目淘汰**——FAIL_EPOCHS
            // 与 CACHE 同目标（多 db 路径进程不累积），>60s 冷却窗口的旧条目无用。
            // #R63 maintainability/low：**Instant 单调时钟**——SystemTime 墙钟回拨时
            // saturating_sub 恒 <60s 静默压制所有 WARN（恰在故障场景失效），前跳则
            // 提前清冷却引发 WARN 风暴；与 CACHE 的 Instant 纪律一致（semantic #R54
            // 同款理由）。
            // #R69 bug/low：**now 在锁内取**（#R56/#R66 纪律）——锁外采样时另一线程
            // 可在本线程取 now 后、本线程拿到锁前插入更新的时间戳：更晚的时间戳
            // 使 saturating_duration_since 得 ~0（<60s），条目被保留、WARN 被静默
            // 抑制（恰在故障期最需要可见性时）；更早的时间戳使条目被修剪、WARN
            // 提前多发。锁内采样保证判定与插入同一时钟视图。
            let mut fe = FAIL_EPOCHS.lock().unwrap_or_else(|p| p.into_inner());
            let now_instant = Instant::now();
            let fmap = fe.get_or_insert_with(HashMap::new);
            fmap.retain(|_, at| now_instant.saturating_duration_since(*at).as_secs() < 60);
            let last = fmap.get(db_path).copied();
            let due = match last {
                Some(at) => now_instant.saturating_duration_since(at).as_secs() >= 60,
                None => true,
            };
            if due {
                fmap.insert(db_path.to_string(), now_instant);
                drop(fe);
                eprintln!(
                    "[Memoria] WARN: hype_vector_index_size query failed (db {db_path}): {e}"
                );
            }
            -1
        }
    }
}

// Utility
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ── Python bindings (optional) ──
#[cfg(feature = "python")]
mod python {
    use super::*;
    use pyo3::prelude::*;

    #[pyclass(name = "MemoriaEngine")]
    pub struct PyEngine {
        inner: MemoriaEngine,
        /// #R60：两阶段 refresh 的**暂存槽**——HnswIndex 不能过 PyO3 边界，构建
        /// 结果（&self 方法，detach 期间其他线程可并发检索）先存此处，随后
        /// swap_hype_index（&mut self，O(1) 窗口）落地到引擎。
        pending_hype: std::sync::Mutex<Option<(HnswIndex, usize, u64)>>,
        /// #R61：build/swap 配对 token 序列（单调递增，见 build_hype_index doc）。
        build_seq: std::sync::atomic::AtomicU64,
        /// #R69 bug/high：**build in-flight 守卫**——`build_hype_index` 是 &self
        /// （PyO3 只挡并发 mix build/swap，挡不住并发 build/build）；token 在
        /// build **开始**时分配（start order，#R61 的完成时分配让"先开始后完成"
        /// 的 build 覆盖新快照并拿最高 token——陈旧索引静默安装）。同一实例
        /// 并发 build 直接 Err（fail-fast），杜绝单槽竞态。
        hype_build_inflight: std::sync::Mutex<Option<u64>>,
    }

    #[pymethods]
    impl PyEngine {
        #[new]
        #[pyo3(signature = (db_path, _embedding = "shibing624/text2vec-base-chinese"))]
        fn new(db_path: &str, _embedding: &str) -> PyResult<Self> {
            MemoriaEngine::new(db_path)
                .map(|e| PyEngine {
                    inner: e,
                    pending_hype: std::sync::Mutex::new(None),
                    build_seq: std::sync::atomic::AtomicU64::new(0),
                    hype_build_inflight: std::sync::Mutex::new(None),
                })
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        #[pyo3(signature = (query, max_results=5, intent="", namespace="default", tier="", include_superseded=false))]
        fn hybrid_search(
            &self,
            query: &str,
            max_results: u32,
            intent: &str,
            namespace: &str,
            tier: &str,
            include_superseded: bool,
        ) -> PyResult<String> {
            self.inner
                .hybrid_search(
                    query,
                    max_results,
                    intent,
                    namespace,
                    tier,
                    include_superseded,
                )
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        fn db_stats(&self) -> PyResult<String> {
            self.inner
                .db_stats()
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        // #R53 maintainability/low：PyEngine 暴露 refresh_hype_index——方法本身要求
        // &mut self（构建期间独占引擎，见 MemoriaEngine::refresh_hype_index 的注释；
        // Arc<Mutex> 宿主在刷新期间持锁，秒级构建会阻塞并发检索——当前取舍：
        // 正确性优先，宿主可错峰刷新）。此前该方法无任何调用点（doc 声称给
        // Python bindings 用但未绑定），refresh 不可达。
        // #R55 performance/medium：**释放 GIL**——`py.detach`（pyo3 0.23+ 从 allow_threads 改名） 让构建期间
        // 其他 Python 线程继续运行（此前整段重建冻结整个解释器；PyEngine.inner 是
        // 实例独占非 Mutex 共享，allow_threads 无锁竞争语义）。
        // #R56 bug/medium 已知取舍：`&mut self` 的 PyRefMut 借用在 allow_threads 期间
        // 仍持有——其他线程若在刷新中调用**同一实例**的任何方法，PyO3 运行时借用
        // 检查会抛 PyBorrowError（"Already borrowed"）。benefit 只对"从不触碰该实例
        // 的线程"成立；跨实例（多 PyEngine）完全安全。
        // #R60 maintainability/medium：**两阶段 API 消除该取舍**——`build_hype_index`
        // 只取 &self（detach 期间其他线程可自由调用**任意 &self 方法**，
        // 包括 hybrid_search），构建完成后 `swap_hype_index` 的 &mut self 窗口只剩
        // O(1) 赋值（微秒级，PyBorrowError 窗口可忽略）。多线程宿主推荐此组合。
        fn refresh_hype_index(&mut self, py: Python<'_>) -> PyResult<usize> {
            // #R69 bug/medium：**pending 存在时 fail-fast**——PyO3 只挡并发混用
            // （borrow 错误），挡不住顺序序列 build→refresh→swap：refresh 无条件
            // 清槽会静默丢弃刚构建的 HnswIndex（秒级重建白费），后续 swap(token)
            // 报误导性 "no pending hype index; call build_hype_index first"——
            // 正是两阶段 API 要防的失败模式，但工作已丢失。先检查 pending：有
            // 则返回显式 Err 并保留槽位（调用方仍可 swap 落地上一个良好索引）。
            {
                let slot = self
                    .pending_hype
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if slot.is_some() {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "pending two-phase build exists; call swap_hype_index first (refresh would discard the pending index)",
                    ));
                }
            }
            let r: Result<usize, String> = py.detach(|| self.inner.refresh_hype_index());
            // #R65 bug/medium：**清 pending + 失效旧 token**——两阶段流程若先
            // build_hype_index()（pending 槽带 token N）后调 refresh，陈旧 pending
            // 索引仍持有效 token，后续 swap(N) 会激活它静默回退本次刷新。
            // #R67 bug/medium：**仅成功时清**——refresh 失败（瞬时 DB busy）时
            // 保留 pending 槽与已发 token：调用方仍可 swap(N) 落地上一个
            // 已知良好的索引（此前无条件清槽把有效快照连同逃生通道一起丢弃）。
            // （#R69：上方 pending 前置检查使此处清槽只命中"无 pending"路径——
            // 单槽语义下 refresh 与 build 不再能互踩。）
            if r.is_ok() {
                *self
                    .pending_hype
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = None;
                self.build_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            r.map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        /// #R60：两阶段 refresh 的构建阶段（&self——detach 下其他线程可并发
        /// 检索）；HnswIndex 不能过 PyO3 边界，结果暂存 pending_hype。
        /// #R65 documentation/low：**同一实例的 build 与 swap 必须严格串行**——
        /// build 期间 &self PyRef 借用存活（detach 闭包运行中），并发 &mut 调用
        /// （swap）会阻塞/PyBorrowError 整个构建窗口；构建完成后才可 swap。
        /// #R61 bug/medium：**返回单调 token**——单槽 last-writer-wins 下并发 build
        /// 会让先者被静默丢弃、swap 激活错误索引且 count 不匹配；调用方须把
        /// 返回值传给 swap_hype_index(token) 配对，陈旧 build 被拒绝（fail-fast
        /// 而非覆盖）。
        /// #R69 bug/high：**token 在 build 开始时分配（start order）+ in-flight
        /// 守卫**——此前 token 在构建完成后分配（completion order）：并发 build
        /// A（先开始、读旧快照）后完成时覆盖 B（后开始、读新快照）的槽并拿
        /// 最高 token，A 的 swap 成功安装**陈旧索引**（缺最近 HyPE 向量）而 B
        /// 的新构建反被拒为 stale——单槽 last-writer-wins + completion-order 无法
        /// 区分"最后完成者"与"最新快照"。token 前置 + `hype_build_inflight`
        /// 守卫使同一实例并发 build 直接 fail-fast（A 未完成时 B 立即 Err，
        /// 无竞态窗口）。
        /// #R69 bug/medium：**panic 安全**——detach 内构建 panic（未来 unwrap、
        /// hnsw_rs 内部 panic、锁中毒 unwind）时 PyO3 边界转 PanicException、
        /// 手动清理行被跳过 → 守卫永久 Some、后续 build 永久 Err。改用 RAII
        /// guard（Drop 无条件清守卫，含 panic 展开路径）。
        fn build_hype_index(&self, py: Python<'_>) -> PyResult<u64> {
            // token 在**构建开始前**分配：start order 决定槽所有权（#R69）。
            let token = self.build_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // in-flight 守卫：同一实例并发 build fail-fast（#R69 bug/high——
            // PyO3 的 &self 借用不阻止并发 build/build，之前两个 build 会竞态
            // 覆盖 pending 槽）。
            {
                let mut inflight = self
                    .hype_build_inflight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if inflight.is_some() {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "concurrent hype build in progress (started token {}); build/swap must be serialized on one engine",
                        inflight.unwrap()
                    )));
                }
                *inflight = Some(token);
            }
            // RAII 守卫：任何出口（Ok/Err/panic 展开）都清 in-flight（#R69
            // bug/medium——手动清理在 panic 路径被跳过、守卫永久 Some）。
            struct InflightGuard<'a>(&'a std::sync::Mutex<Option<u64>>);
            impl Drop for InflightGuard<'_> {
                fn drop(&mut self) {
                    *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
            }
            let _guard = InflightGuard(&self.hype_build_inflight);
            let r: Result<(HnswIndex, usize), String> =
                py.detach(|| self.inner.build_hype_index_fresh());
            let (fresh, count) =
                r.map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
            // 写入 pending 槽（token 已配对 start order）；_guard drop 清 in-flight。
            *self
                .pending_hype
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some((fresh, count, token));
            Ok(token)
        }
        fn swap_hype_index(&mut self, token: u64) -> PyResult<usize> {
            // #R62 bug/high：**先校验后 take**——`.take()` 在 token 校验前消费槽：
            // 陈旧 token 调用会把当前 pending（有效）索引移除并 drop，合法所有者的
            // 后续 swap 报 "no pending"（秒级重建白费 + "latest was" 误导）。
            let mut slot = self
                .pending_hype
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match &*slot {
                None => {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "no pending hype index; call build_hype_index first",
                    ))
                }
                Some((_, _, t)) if *t != token => {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "stale build token {token} (latest was {t}); rebuild and swap with its token"
                    )))
                }
                _ => {}
            }
            let (fresh, count, _) = slot.take().unwrap();
            // #R66 performance/low：**guard 提前释放**——token/pending 已消费，
            // swap 内 eprintln（count 不匹配 WARN）在慢 stderr 管道上阻塞时不再
            // 卡住其他线程的 build/swap（#R59 同款纪律）。
            drop(slot);
            self.inner.swap_hype_index(fresh, count);
            // #R64 bug/low：返回**实际应用长度**——引擎内部对 count 不匹配做了
            // 修正（取 fresh.len()）；返回 pending 槽的原始 count 会让调用方拿到
            // 与 hype_vector_index_live 不一致的统计。
            Ok(self.inner.hype_hnsw.len())
        }
        fn add_vectors(&self, ids: Vec<String>, vectors: Vec<Vec<f32>>) -> PyResult<usize> {
            self.inner
                .add_vectors(ids, vectors)
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        fn vector_search(&self, qv: Vec<f32>, k: u32) -> PyResult<String> {
            self.inner
                .vector_search(qv, k)
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        fn vector_count(&self) -> usize {
            self.inner.vector_count()
        }
        fn cache_query_vector(&self, text: &str, v: Vec<f32>) {
            self.inner.cache_query_vector(text, v);
        }
        fn get_cached_query_vector(&self, text: &str) -> Option<Vec<f32>> {
            self.inner.get_cached_query_vector(text)
        }
        fn save_index(&self) -> PyResult<()> {
            self.inner
                .save_index()
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
    }

    #[pymodule]
    fn memoria_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PyEngine>()?;
        m.add("__version__", env!("MEMORIA_BUILD_VERSION"))?;
        Ok(())
    }
}
