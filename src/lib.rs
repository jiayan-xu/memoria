//! Memoria Core — Rust memory engine
#![allow(dead_code)]

use std::sync::Arc;

pub mod auth;
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
    pub query_cache: QueryCache,
}

impl MemoriaEngine {
    pub fn new(db_path: &str) -> Result<Self, String> {
        let pool = storage::create_pool(db_path, 4)?;
        storage::init_schema(&pool)?;
        storage::init_core_tables(&pool)?;

        let vec_path = std::path::Path::new(db_path)
            .parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("vector_index").join("hnsw_vectors");
        let hnsw = if HnswIndex::exists(&vec_path) {
            HnswIndex::load(&vec_path).unwrap_or_else(|e| {
                eprintln!("HNSW load: {}", e);
                HnswIndex::new()
            })
        } else {
            HnswIndex::new()
        };

        Ok(Self { db_path: db_path.to_string(), pool: Arc::new(pool), hnsw, query_cache: QueryCache::new() })
    }

    pub fn hybrid_search(&self, query: &str, max_results: u32, _intent: &str, namespace: &str, _tier: &str) -> Result<String, String> {
        let results = search::hybrid::hybrid_search(
            &self.pool, query, namespace, max_results,
            Some(&self.hnsw), Some(&self.query_cache),
        )?;
        let items: Vec<serde_json::Value> = results.iter().map(|r| {
            serde_json::json!({
                "memory_id": r.memory_id,
                "content": truncate(&r.content, 200),
                "rrf_score": r.rrf_score,
                "source": r.source,
            })
        }).collect();
        serde_json::to_string(&serde_json::json!({
            "status": "completed",
            "total_results": results.len(),
            "results": items,
        })).map_err(|e| e.to_string())
    }

    pub fn db_stats(&self) -> Result<String, String> {
        let conn = self.pool.get().map_err(|e| format!("pool: {}", e))?;
        let tables = ["memories","messages","sessions","decisions","user_prefs","memory_relations","decay_log","dream_state"];
        let mut m = serde_json::Map::new();
        for t in &tables {
            let c: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", t),[],|r|r.get(0)).unwrap_or(0);
            m.insert(t.to_string(), serde_json::Value::Number(c.into()));
        }
        m.insert("vector_index_size".to_string(), serde_json::Value::Number((self.hnsw.len() as i64).into()));
        m.insert("query_cache_size".to_string(), serde_json::Value::Number((self.query_cache.len() as i64).into()));
        serde_json::to_string(&serde_json::Value::Object(m)).map_err(|e| e.to_string())
    }

    pub fn add_vectors(&self, ids: Vec<String>, vectors: Vec<Vec<f32>>) -> Result<usize, String> {
        if ids.len() != vectors.len() { return Err("ids/vectors length mismatch".to_string()); }
        let entries: Vec<VectorEntry> = ids.into_iter().zip(vectors).map(|(id, v)| VectorEntry { id, vector: v }).collect();
        self.hnsw.add(&entries)
    }

    pub fn vector_search(&self, qv: Vec<f32>, k: u32) -> Result<String, String> {
        serde_json::to_string(&self.hnsw.search(&qv, k as usize)?).map_err(|e| e.to_string())
    }

    pub fn vector_count(&self) -> usize { self.hnsw.len() }
    pub fn cache_query_vector(&self, text: &str, v: Vec<f32>) { self.query_cache.put(text, v); }
    pub fn get_cached_query_vector(&self, text: &str) -> Option<Vec<f32>> { self.query_cache.get(text) }

    pub fn save_index(&self) -> Result<(), String> {
        if self.db_path == ":memory:" { return Ok(()); }
        let p = std::path::Path::new(&self.db_path).parent().unwrap_or_else(|| std::path::Path::new(".")).join("vector_index").join("hnsw_vectors");
        self.hnsw.save(&p)
    }
}

// Utility
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len { return s.to_string(); }
    let mut end = max_len;
    while !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

// ── Python bindings (optional) ──
#[cfg(feature = "python")]
mod python {
    use pyo3::prelude::*;
    use super::*;

    #[pyclass(name = "MemoriaEngine")]
    pub struct PyEngine { inner: MemoriaEngine }

    #[pymethods]
    impl PyEngine {
        #[new]
        #[pyo3(signature = (db_path, _embedding = "shibing624/text2vec-base-chinese"))]
        fn new(db_path: &str, _embedding: &str) -> PyResult<Self> {
            MemoriaEngine::new(db_path).map(|e| PyEngine { inner: e })
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        fn hybrid_search(&self, query: &str, max_results: u32, intent: &str, namespace: &str, tier: &str) -> PyResult<String> {
            self.inner.hybrid_search(query, max_results, intent, namespace, tier).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
        }
        fn db_stats(&self) -> PyResult<String> { self.inner.db_stats().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e)) }
        fn add_vectors(&self, ids: Vec<String>, vectors: Vec<Vec<f32>>) -> PyResult<usize> { self.inner.add_vectors(ids, vectors).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e)) }
        fn vector_search(&self, qv: Vec<f32>, k: u32) -> PyResult<String> { self.inner.vector_search(qv, k).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e)) }
        fn vector_count(&self) -> usize { self.inner.vector_count() }
        fn cache_query_vector(&self, text: &str, v: Vec<f32>) { self.inner.cache_query_vector(text, v); }
        fn get_cached_query_vector(&self, text: &str) -> Option<Vec<f32>> { self.inner.get_cached_query_vector(text) }
        fn save_index(&self) -> PyResult<()> { self.inner.save_index().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e)) }
    }

    #[pymodule]
    fn memoria_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PyEngine>()?;
        m.add("__version__", env!("CARGO_PKG_VERSION"))?;
        Ok(())
    }
}
