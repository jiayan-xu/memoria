//! Embedding bridge — placeholder for Phase 1.5.
//!
//! Phase 1.3: Python passes pre-computed vectors directly to Rust via add_vectors().
//! The embedding model runs in Python; Rust only stores and searches vectors.
//! Phase 1.5 will add direct PyO3 bridge if performance requires it.

#![allow(dead_code)]

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

const CACHE_SIZE: usize = 10_000;

/// Query vector cache (Python → Rust).
/// Python computes embedding, passes to Rust, Rust caches for search.
pub struct QueryCache {
    cache: Mutex<LruCache<String, Vec<f32>>>,
}

unsafe impl Send for QueryCache {}
unsafe impl Sync for QueryCache {}

impl QueryCache {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
        }
    }

    /// Store a query vector (pre-computed by Python).
    pub fn put(&self, text: &str, vector: Vec<f32>) {
        if let Ok(mut c) = self.cache.lock() {
            c.put(text.to_string(), vector);
        }
    }

    /// Retrieve cached query vector.
    pub fn get(&self, text: &str) -> Option<Vec<f32>> {
        self.cache.lock().ok().and_then(|mut c| c.get(text).cloned())
    }

    pub fn len(&self) -> usize {
        self.cache.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn capacity(&self) -> usize {
        CACHE_SIZE
    }
}
