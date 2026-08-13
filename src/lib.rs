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
        let (hype_hnsw, hype_count) =
            vector::persist::build_hype_hnsw_or_default(&pool, ef_search);
        // 库代码（Python bindings 等宿主）不污染 stdout——用 eprintln；且无条件打印
        // （0 也打），与 main.rs 一致：空表（未启用）与 rebuild 静默降级可区分。
        eprintln!("[Memoria] HYPE HNSW vectors: {}", hype_count);

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
    /// HnswIndex::add 按 id 去重，in-place rebuild 只追加新 id（#R44），已存在 id 的
    /// 向量更新必须全新索引拾取。返回新索引加载的向量数。
    /// #R52 maintainability/medium：用 **build_hype_hnsw（Result 版）**而非
    /// `build_hype_hnsw_or_default`——or_default 把所有失败吸收成空索引 + WARN，
    /// 本方法**永不返回 Err**（签名误导）：调用方（Python bindings/运维工具）无法
    /// 程序化区分"刷新成功"与"降级空重建"，count==0 同时可能是"未启用"或"失败"。
    /// Err 路径真实可达：失败保留旧快照（检索不中断）并向调用方显式报错。
    /// #R53 maintainability/low 已知取舍：要求 `&mut self`——`build_hype_hnsw` 全量
    /// 重读+重索引表（大表秒级），Arc<Mutex> 宿主在刷新期间持锁会阻塞并发检索；
    /// 理想形态是构建锁外进行、仅最终 swap O(1) 同步（hype_hnsw 换内部可变槽），
    /// 留待后续。当前正确性优先，宿主错峰刷新即可。
    pub fn refresh_hype_index(&mut self) -> Result<usize, String> {
        let ef_search = vector::persist::resolve_ef_search();
        let (fresh, count) = match vector::persist::build_hype_hnsw(&self.pool, ef_search) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[Memoria] HYPE HNSW refresh failed, keeping existing index: {e}");
                return Err(e);
            }
        };
        // 仅构建成功才替换；失败保留旧快照，检索不中断。
        self.hype_hnsw = fresh;
        eprintln!("[Memoria] HYPE HNSW refreshed: {} vectors", count);
        Ok(count)
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
        // #R50 maintainability/medium：报告**权威表行数**而非内存快照——hype_hnsw 是
        // 构造时快照，运行时表更新（离线脚本重跑）后内存 len 静默偏离现实（如
        // 索引 0 会隐藏脚本已写入的向量，误导运维）；字段名也暗示反映存储。
        // #R51 maintainability/medium：查询失败**显式标记为 -1** 并 eprintln——此前
        // unwrap_or(内存 len) 把任何查询失败（缺表/锁）伪装成合理数字，运维无法区分
        // "HyPE 未启用（空表 0）"与"统计查询失败（-1）"，正是要 surface 的故障。
        let hype_store: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM memory_hype_vectors",
            [],
            |r| r.get(0),
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[Memoria] WARN: hype_vector_index_size query failed: {e}");
                -1
            }
        };
        m.insert(
            "hype_vector_index_size".to_string(),
            serde_json::Value::Number(hype_store.into()),
        );
        m.insert(
            "query_cache_size".to_string(),
            serde_json::Value::Number((self.query_cache.len() as i64).into()),
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
    }

    #[pymethods]
    impl PyEngine {
        #[new]
        #[pyo3(signature = (db_path, _embedding = "shibing624/text2vec-base-chinese"))]
        fn new(db_path: &str, _embedding: &str) -> PyResult<Self> {
            MemoriaEngine::new(db_path)
                .map(|e| PyEngine { inner: e })
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
                .hybrid_search(query, max_results, intent, namespace, tier, include_superseded)
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
        fn refresh_hype_index(&mut self) -> PyResult<usize> {
            self.inner
                .refresh_hype_index()
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
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
