//! Embedding bridge — placeholder for Phase 1.5.
//!
//! Phase 1.3: Python passes pre-computed vectors directly to Rust via add_vectors().
//! The embedding model runs in Python; Rust only stores and searches vectors.
//! Phase 1.5 will add direct PyO3 bridge if performance requires it.
//!
//! P0-1（2026-10-13）：`embed_query` / `block_on_embed` 从 mcp_server（bin 专享
//! 模块）下沉至此——lib 内的 web_api（文档上传事后嵌）与 bin 内的 mcp_server
//! （dispatch 写入侧/查询侧嵌）共用同一 HTTP 嵌入桥，杜绝两条路径复制粘贴漂移。

#![allow(dead_code)]

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

const CACHE_SIZE: usize = 10_000;

/// 查询时生成 query embedding —— 修复「HNSW 语义搜索从未生效」的根因。
/// 独立 MCP 服务（memoria-server）在查询时调用本地嵌入服务（默认 127.0.0.1:8777/embed）。
/// 3 次重试（200ms/400ms 退避）+ 60s 超时；全部失败返回 None 并打印（fail-open：
/// 调用方降级为无语义信号，不阻断主流程）。
pub async fn embed_query(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    hyde: bool,
) -> Option<Vec<f32>> {
    let body = if hyde {
        serde_json::json!({ "texts": [query], "hyde": true })
    } else {
        serde_json::json!({ "texts": [query] })
    };
    let mut last_err: Option<String> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200 * (1 << (attempt - 1)))).await;
        }
        let resp = match client
            .post(url)
            .timeout(std::time::Duration::from_secs(60))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(format!("send#{attempt}: {e}"));
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = Some(format!("http{}", resp.status().as_u16()));
            continue;
        }
        match resp.json::<serde_json::Value>().await {
            Ok(v) => {
                let arr = match v.get("embeddings").and_then(|a| a.as_array()) {
                    Some(a) => a,
                    None => {
                        last_err = Some("no embeddings field".into());
                        continue;
                    }
                };
                let first = match arr.first().and_then(|f| f.as_array()) {
                    Some(f) => f,
                    None => {
                        last_err = Some("embeddings[0] not array".into());
                        continue;
                    }
                };
                let vec: Vec<f32> = first
                    .iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect();
                if vec.is_empty() {
                    last_err = Some("empty vec".into());
                    continue;
                }
                return Some(vec);
            }
            Err(e) => {
                last_err = Some(format!("json#{attempt}: {e}"));
                continue;
            }
        }
    }
    eprintln!(
        "[embed_query] FAILED after retries for q={:?}: {:?}",
        query, last_err
    );
    None
}

/// 阻塞包装：在**同步阻塞上下文**（MCP dispatch 的 spawn_blocking / web_api
/// 上传的 spawn_blocking）里用临时 current-thread 运行时执行 embed_query。
/// 与 mcp_server 的 block_on_rerank 同款范式（dispatch 是同步函数，无法 .await）。
/// `url` 为空 = 语义通道未配置，直接 None（fail-open）——不去 POST 空 URL
/// 白耗 3 次重试。P0-1（2026-10-13）供文档 ingestion 事后嵌使用。
pub fn block_on_embed(client: &reqwest::Client, url: &str, text: &str) -> Option<Vec<f32>> {
    if url.trim().is_empty() {
        return None;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(embed_query(client, url, text, false))
}

/// Query vector cache (Python → Rust).
/// Python computes embedding, passes to Rust, Rust caches for search.
pub struct QueryCache {
    cache: Mutex<LruCache<String, Vec<f32>>>,
}

// SAFETY (P2-10): `QueryCache` 内部唯一字段 `cache: Mutex<LruCache<...>>`
// 经 `std::sync::Mutex` 提供互斥访问，跨线程读写均经锁保护，不存在数据竞争。
// 因此可为 `QueryCache` 安全实现 `Send` + `Sync`。
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
        self.cache
            .lock()
            .ok()
            .and_then(|mut c| c.get(text).cloned())
    }

    pub fn len(&self) -> usize {
        self.cache.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn capacity(&self) -> usize {
        CACHE_SIZE
    }
}
