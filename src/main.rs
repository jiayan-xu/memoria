//! Memoria 独立二进制入口
//!
//! 环境变量:
//!   MEMORIA_DB_PATH  (default: data/memoria.db)
//!   MEMORIA_PORT     (default: 9003)
//!   MEMORIA_HOST     (default: 127.0.0.1)
//!   MEMORIA_ADMIN_KEY (default: auto-generated)

mod mcp_server;

use memoria_core::{auth, storage, vector::HnswIndex, web_api};
use mcp_server::AppState;
use std::sync::Arc;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let db_path = std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| {
        "data/memoria.db".to_string()
    });
    let auth_db_path = std::env::var("MEMORIA_AUTH_DB_PATH").unwrap_or_else(|_| {
        let p = std::path::Path::new(&db_path);
        p.parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("audit.db")
            .to_string_lossy().to_string()
    });
    let port: u16 = std::env::var("MEMORIA_PORT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(9003);
    let host = std::env::var("MEMORIA_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let admin_key = std::env::var("MEMORIA_ADMIN_KEY")
        .unwrap_or_else(|_| {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            format!("mem-admin-{:x}", ts.as_nanos())
        });

    println!("[Memoria] v0.2.0 — standalone MCP server");
    println!("[Memoria] DB: {}", db_path);
    println!("[Memoria] Listen: {}:{}", host, port);
    println!("[Memoria] Admin key: {}...", &admin_key[..16.min(admin_key.len())]);

    let pool = storage::create_pool(&db_path, 4).expect("pool");
    storage::init_schema(&pool).expect("schema");
    storage::init_core_tables(&pool).expect("core tables");

    println!("[Memoria] Auth DB: {}", auth_db_path);
    let auth_pool = storage::create_pool(&auth_db_path, 2).expect("auth pool");
    auth::init_auth_tables(&auth_pool).expect("auth tables");

    // Bootstrap default admin with known key
    let _ = auth::register_agent(&auth_pool, "admin", "Administrator", &["*"], "admin");
    // Override admin's badge_token with the raw admin_key (authenticate compares directly)
    if let Ok(conn) = auth_pool.get() {
        let _ = conn.execute(
            "UPDATE agent_registry SET badge_token = ? WHERE agent_id = 'admin'",
            rusqlite::params![admin_key],
        );
    }

    // Register default agent and make its badge_token known
    match auth::register_agent(&auth_pool, "default", "Default Agent", &["default"], "read_write") {
        Ok(badge) => {
            let token = &badge.badge_token;
            if !token.is_empty() {
                use std::io::Write;
                let end = 16.min(token.len());
                let _ = writeln!(std::io::stdout(), "[Memoria] Default agent token: {}...", &token[..end]);
            }
        }
        Err(e) => {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "[Memoria] Default agent registration failed: {}", e);
        }
    }

    let vec_path = std::path::Path::new(&db_path)
        .parent().unwrap_or_else(|| std::path::Path::new("."))
        .join("vector_index").join("hnsw_vectors");
    let hnsw = if HnswIndex::exists(&vec_path) {
        HnswIndex::load(&vec_path).unwrap_or_else(|e| {
            eprintln!("[Memoria] HNSW load: {}", e);
            HnswIndex::new()
        })
    } else {
        HnswIndex::new()
    };
    println!("[Memoria] HNSW vectors: {}", hnsw.len());

    let state = Arc::new(AppState {
        pool,
        auth_pool,
        hnsw: Arc::new(hnsw),
        query_cache: Arc::new(memoria_core::vector::QueryCache::new()),
        admin_key,
        bridge_url: std::env::var("MEMORIA_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000/mcp".to_string()),
        http_client: reqwest::Client::new(),
    });
    let mut app = mcp_server::build_app(state.clone());

    // ── Web API 路由（替换 Python /stats /graph /decay_timeline）──
    {
        let ws = Arc::new(web_api::WebApiState {
            pool: state.pool.clone(),
        });
        app = app.merge(web_api::build_web_api_routes(ws));
    }

    // ── Web UI 静态文件 ──
    let web_dir = std::env::var("MEMORIA_WEB_DIR").unwrap_or_else(|_| {
        let base = std::path::Path::new(&db_path)
            .parent().and_then(|p| p.parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        base.join("web").to_string_lossy().to_string()
    });
    if std::path::Path::new(&web_dir).exists() {
        let serve_dir = ServeDir::new(&web_dir).append_index_html_on_directories(true);
        app = app.nest_service("/app", serve_dir);
        println!("[Memoria] Web UI: {} → /app", web_dir);
    } else {
        println!("[Memoria] Web UI not found at {}", web_dir);
    }

    // ── 会话文件监听（替换 Capture Proxy） ──
    let watch_pool = state.pool.clone();
    tokio::spawn(async move {
        memoria_core::session_watcher::watch_sessions_loop(watch_pool).await;
    });

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("[Memoria] Ready on {}", addr);
    axum::serve(listener, app).await.unwrap();
}
