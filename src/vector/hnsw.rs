//! HNSW vector index with save/load via flat vector store.
//!
//! Since hnsw_rs has lifetime constraints that complicate serde,
//! we save vectors as a flat binary file and rebuild the HNSW
//! graph on load. For ~10k 768-dim vectors this takes ~1-2s.

use hnsw_rs::hnsw::Hnsw;
use hnsw_rs::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

pub const DIM: usize = 768;
const DEFAULT_M: usize = 16;
const DEFAULT_EF_C: usize = 200;
const DEFAULT_EF_S: usize = 50;
const MAX_CAPACITY: usize = 1_000_000;

/// A vector entry: String ID + 768-dim embedding.
#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub id: String,
    pub vector: Vec<f32>,
}

/// Thread-safe HNSW index with ID mapping and disk persistence.
pub struct HnswIndex {
    inner: RwLock<Hnsw<'static, f32, DistCosine>>,
    id_map: RwLock<Vec<String>>,
    id_to_seq: RwLock<HashMap<String, usize>>,
    ef_search: RwLock<usize>,
    /// All vectors saved for rebuild on load
    vectors: RwLock<Vec<VectorEntry>>,
}

unsafe impl Send for HnswIndex {}
unsafe impl Sync for HnswIndex {}

impl HnswIndex {
    pub fn new() -> Self {
        let hnsw = Hnsw::<f32, DistCosine>::new(
            DEFAULT_M, MAX_CAPACITY, 16, DEFAULT_EF_C, DistCosine,
        );
        Self {
            inner: RwLock::new(hnsw),
            id_map: RwLock::new(Vec::new()),
            id_to_seq: RwLock::new(HashMap::new()),
            ef_search: RwLock::new(DEFAULT_EF_S),
            vectors: RwLock::new(Vec::new()),
        }
    }

    pub fn add(&self, entries: &[VectorEntry]) -> Result<usize, String> {
        let mut id_map = self.id_map.write().map_err(|e| format!("id_map: {}", e))?;
        let mut id_to_seq = self.id_to_seq.write().map_err(|e| format!("id_to_seq: {}", e))?;
        let inner = self.inner.write().map_err(|e| format!("inner: {}", e))?;
        let mut vectors = self.vectors.write().map_err(|e| format!("vectors: {}", e))?;

        let mut added = 0;
        for entry in entries {
            // Dimension check
            if entry.vector.len() != DIM {
                return Err(format!(
                    "vector dimension mismatch: expected {}, got {} (id: {})",
                    DIM, entry.vector.len(), entry.id
                ));
            }
            if id_to_seq.contains_key(&entry.id) {
                continue;
            }
            let seq = id_map.len();
            id_map.push(entry.id.clone());
            id_to_seq.insert(entry.id.clone(), seq);
            inner.insert_slice((&entry.vector, seq));
            vectors.push(entry.clone());
            added += 1;
        }
        Ok(added)
    }

    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>, String> {
        let ef = *self.ef_search.read().map_err(|e| format!("ef: {}", e))?;
        let inner = self.inner.read().map_err(|e| format!("inner: {}", e))?;
        let id_map = self.id_map.read().map_err(|e| format!("id_map: {}", e))?;

        let results = inner.search(query, k, ef);
        Ok(results.iter().map(|n| {
            let id = id_map.get(n.get_origin_id())
                .cloned()
                .unwrap_or_else(|| format!("<unknown:{}>", n.get_origin_id()));
            (id, n.get_distance())
        }).collect())
    }

    pub fn len(&self) -> usize {
        self.vectors.read().map(|v| v.len()).unwrap_or(0)
    }

    pub fn set_ef_search(&self, ef: usize) {
        if let Ok(mut e) = self.ef_search.write() {
            *e = ef;
        }
    }

    /// Save vectors + id_map to disk.
    /// Format: binary flat file — [n_vectors][for each: id_len(u32), id_bytes, 768×f32]
    /// Plus: JSON index file for fast loading.
    /// The HNSW graph is NOT saved — rebuilt on load.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let vectors = self.vectors.read().map_err(|e| format!("vectors: {}", e))?;
        let id_map = self.id_map.read().map_err(|e| format!("id_map: {}", e))?;

        // Ensure directory exists
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }

        // Save vectors as binary
        let bin_path = path.as_ref().with_extension("bin");
        let mut buf: Vec<u8> = Vec::with_capacity(vectors.len() * (4 + 64 + 768 * 4));
        let n: u32 = vectors.len() as u32;
        buf.extend_from_slice(&n.to_le_bytes());
        for v in vectors.iter() {
            let id_bytes = v.id.as_bytes();
            let len: u32 = id_bytes.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(id_bytes);
            for &val in &v.vector {
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }
        std::fs::write(&bin_path, &buf).map_err(|e| format!("write: {}", e))?;

        // Save ID map as JSON for fast lookup
        let json_path = path.as_ref().with_extension("json");
        let json_data: Vec<&str> = id_map.iter().map(|s| s.as_str()).collect();
        let json_str = serde_json::to_string(&json_data).map_err(|e| format!("json: {}", e))?;
        std::fs::write(&json_path, &json_str).map_err(|e| format!("write json: {}", e))?;

        Ok(())
    }

    /// Load vectors from disk and rebuild HNSW graph.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let bin_path = path.as_ref().with_extension("bin");
        let data = std::fs::read(&bin_path).map_err(|e| format!("read: {}", e))?;

        let mut offset = 0;
        if data.len() < 4 {
            return Err("truncated file".to_string());
        }
        let n = u32::from_le_bytes(data[0..4].try_into().map_err(|_| "bad header slice".to_string())?) as usize;
        offset += 4;

        let mut id_map: Vec<String> = Vec::with_capacity(n);
        let mut id_to_seq: HashMap<String, usize> = HashMap::with_capacity(n);
        let mut vectors: Vec<VectorEntry> = Vec::with_capacity(n);

        for seq in 0..n {
            if offset + 4 > data.len() {
                return Err("truncated at ID length".to_string());
            }
            let id_len = u32::from_le_bytes(data[offset..offset+4].try_into().map_err(|_| "bad id-len slice".to_string())?) as usize;
            offset += 4;
            if offset + id_len + 768 * 4 > data.len() {
                return Err("truncated at vector data".to_string());
            }
            let id = String::from_utf8(data[offset..offset+id_len].to_vec())
                .map_err(|_| "invalid UTF-8 in ID")?;
            offset += id_len;
            let mut vector = Vec::with_capacity(768);
            for _ in 0..768 {
                let val = f32::from_le_bytes(data[offset..offset+4].try_into().map_err(|_| "bad vector slice".to_string())?);
                vector.push(val);
                offset += 4;
            }
            id_map.push(id.clone());
            id_to_seq.insert(id, seq);
            vectors.push(VectorEntry { id: id_map.last().unwrap().clone(), vector });
        }

        // Rebuild HNSW graph
        let hnsw = Hnsw::<f32, DistCosine>::new(DEFAULT_M, MAX_CAPACITY, 16, DEFAULT_EF_C, DistCosine);
        for entry in &vectors {
            hnsw.insert_slice((&entry.vector, id_to_seq[&entry.id]));
        }

        Ok(Self {
            inner: RwLock::new(hnsw),
            id_map: RwLock::new(id_map),
            id_to_seq: RwLock::new(id_to_seq),
            ef_search: RwLock::new(DEFAULT_EF_S),
            vectors: RwLock::new(vectors),
        })
    }

    /// Check if saved vector store exists on disk.
    pub fn exists(path: impl AsRef<Path>) -> bool {
        path.as_ref().with_extension("bin").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_corrupted_bin_returns_err_not_panic() {
        let dir = std::env::temp_dir().join("memoria_hnsw_test_corrupt");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("hnsw_vectors.bin");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[1u8, 2, 3]).unwrap(); // len < 4 → 截断错误，不应 panic
        drop(f);
        let res = HnswIndex::load(&p);
        assert!(res.is_err(), "corrupted bin must return Err, not panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_index_is_empty() {
        assert_eq!(HnswIndex::new().len(), 0);
    }

    #[test]
    fn concurrent_search_stable() {
        let h = std::sync::Arc::new(HnswIndex::new());
        let entries: Vec<VectorEntry> = (0..20)
            .map(|i| VectorEntry { id: format!("v{}", i), vector: vec![(i as f32) / 20.0; DIM] })
            .collect();
        assert_eq!(h.add(&entries).expect("add"), 20);

        // 20 路并发 search（读锁），全部完成不得 panic / 死锁
        let mut handles = Vec::new();
        for _ in 0..20 {
            let h2 = h.clone();
            handles.push(std::thread::spawn(move || {
                let _ = h2.search(&vec![0.5f32; DIM], 5);
            }));
        }
        for handle in handles {
            handle.join().expect("thread must not panic");
        }
    }
}
