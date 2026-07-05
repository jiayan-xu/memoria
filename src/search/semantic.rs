//! Semantic search signal (S2) using HNSW vector index.
//! Uses the query cache to retrieve pre-computed embeddings from Python.

use crate::search::keyword::SignalResult;
use crate::vector::HnswIndex;
use crate::QueryCache;

/// Semantic search via HNSW vector similarity.
/// Python must call cache_query_vector() first to provide the query embedding.
pub fn semantic_search(
    query: &str,
    _namespace: &str,
    limit: u32,
    hnsw: Option<&HnswIndex>,
    query_cache: Option<&QueryCache>,
) -> Result<Vec<SignalResult>, String> {
    let hnsw = match hnsw {
        Some(h) => h,
        None => return Ok(vec![]),
    };

    let cache = match query_cache {
        Some(c) => c,
        None => return Ok(vec![]),
    };

    // Get cached embedding from Python (must have been cached via cache_query_vector)
    let vector = match cache.get(query) {
        Some(v) => v,
        None => return Ok(vec![]),  // No cached embedding — skip semantic signal
    };

    // Search HNSW index
    let results = hnsw.search(&vector, limit as usize)?;
    let mut out = Vec::with_capacity(results.len());
    for (memory_id, distance) in results {
        let score = 1.0 - distance; // Convert cosine distance to similarity
        if score > 0.0 {
            out.push(SignalResult {
                memory_id,
                content: String::new(),
                score: score as f64,
                source: "hnsw_semantic".to_string(),
            });
        }
    }
    Ok(out)
}
