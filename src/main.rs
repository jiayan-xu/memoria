//! Memoria 独立二进制入口
//!
//! 环境变量:
//!   MEMORIA_DB_PATH  (default: data/memoria.db)
//!   MEMORIA_PORT     (default: 9003)
//!   MEMORIA_HOST     (default: 127.0.0.1)
//!   MEMORIA_ADMIN_KEY (default: auto-generated)
//!   MEMORIA_BACKUP_DIR (default: data/backups)
//!   MEMORIA_BACKUP_INTERVAL_HOURS (default: 24)

mod mcp_server;

use memoria_core::{auth, backup, health, storage, vector::HnswIndex, web_api};
use mcp_server::AppState;
use std::sync::Arc;
use tower_http::services::ServeDir;
use chrono::Datelike;

#[tokio::main]
async fn main() {
    // ── 诊断入口（默认关闭）──
    // 仅当以 `--features diag` 且 `RUSTFLAGS="--cfg tokio_unstable"` 编译时启用，
    // 通过 tokio-console 客户端（默认连 :6669）暴露任务级栈，定位 busy-loop。
    #[cfg(feature = "diag")]
    let _console_guard = console_subscriber::init();

    let db_path = std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| {
        "data/memoria.db".to_string()
    });
    let auth_db_path = std::env::var("MEMORIA_AUTH_DB_PATH").unwrap_or_else(|_| {
        let p = std::path::Path::new(&db_path);
        p.parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("audit.db")
            .to_string_lossy().to_string()
    });
    let backup_dir = std::env::var("MEMORIA_BACKUP_DIR").unwrap_or_else(|_| {
        let p = std::path::Path::new(&db_path);
        p.parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("backups")
            .to_string_lossy().to_string()
    });
    let backup_interval_hours: u64 = std::env::var("MEMORIA_BACKUP_INTERVAL_HOURS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(24);
    let port: u16 = std::env::var("MEMORIA_PORT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(9003);
    let host = std::env::var("MEMORIA_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let (admin_key, admin_key_auto) = match std::env::var("MEMORIA_ADMIN_KEY") {
        Ok(k) => (k, false),
        Err(_) => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            (format!("mem-admin-{:x}", ts.as_nanos()), true)
        }
    };

    println!("[Memoria] v0.2.0 — standalone MCP server");
    println!("[Memoria] DB: {}", db_path);
    println!("[Memoria] Backup dir: {}", backup_dir);
    println!("[Memoria] Listen: {}:{}", host, port);
    // P2-3 修复：密钥前缀仅当自动生成时打印，且改 stderr（避免被 stdout 日志捕获泄露）
    if admin_key_auto {
        eprintln!("[Memoria] Admin key (auto-generated): {}...", &admin_key[..16.min(admin_key.len())]);
    }

    let pool = storage::create_pool(&db_path, 16).expect("pool");
    storage::init_schema(&pool).expect("schema");
    storage::init_core_tables(&pool).expect("core tables");

    // P0: 迁移 — 添加 superseded_by 列
    storage::migrate_superseded_by(&pool).expect("migration: superseded_by");

    println!("[Memoria] Auth DB: {}", auth_db_path);
    let auth_pool = storage::create_pool(&auth_db_path, 16).expect("auth pool");
    // P0 修复：auth_pool 此前仅调 init_auth_tables，漏掉 init_schema 的 WAL/busy_timeout PRAGMA，
    // 导致 rollback-journal 模式下写操作（register_agent / audit_log）与并发读争用写锁，
    // 且 connection_timeout 等待连接形成 ~20s 卡顿。补 WAL + busy_timeout=5000。
    storage::init_schema(&auth_pool).expect("auth schema (WAL/busy_timeout)");
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
                // P2-3 修复：默认 agent token 前缀改 stderr（避免被 stdout 日志捕获泄露）
                let _ = writeln!(std::io::stderr(), "[Memoria] Default agent token: {}...", &token[..end]);
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

    // ── P0: 启动健康检查 ──
    println!("[Memoria] Running startup health check...");
    let report = health::run_health_check(&pool, &auth_pool, &hnsw, &db_path);
    match report.overall.as_str() {
        "pass" => println!("[Memoria] Health check: PASS (all checks OK)"),
        "degraded" => {
            println!("[Memoria] Health check: DEGRADED — some soft checks failed:");
            for c in report.soft_checks.iter().filter(|c| c.status != "pass") {
                println!("  ⚠ {} — {} ({})", c.name, c.message, c.status);
            }
        }
        "fail" => {
            eprintln!("[Memoria] Health check: HARD FAIL — refusing to start:");
            for c in report.hard_checks.iter().filter(|c| c.status == "fail") {
                eprintln!("  ✗ {} — {} ({})", c.name, c.message, c.status);
            }
            std::process::exit(1);
        }
        _ => {}
    }

    let state = Arc::new(AppState {
        pool,
        auth_pool,
        hnsw: Arc::new(hnsw),
        query_cache: Arc::new(memoria_core::vector::QueryCache::new()),
        admin_key,
        bridge_url: std::env::var("MEMORIA_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000/mcp".to_string()),
        http_client: reqwest::Client::new(),
        db_path: db_path.clone(),
        backup_dir: backup_dir.clone(),
        vec_index_path: vec_path.to_string_lossy().to_string(),
    });
    let mut app = mcp_server::build_app(state.clone());

    // ── Web API 路由（替换 Python /stats /graph /decay_timeline）──
    {
        let ws = Arc::new(web_api::WebApiState {
            pool: state.pool.clone(),
            auth_pool: state.auth_pool.clone(),
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

    // ── P0: 自动备份定时任务 ──
    let backup_pool = state.pool.clone();
    let backup_db_path = db_path.clone();
    let backup_dir_clone = backup_dir.clone();
    let backup_vec_path = vec_path.to_string_lossy().to_string();
    tokio::spawn(async move {
        // 启动后先等 60 秒，让服务稳定
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        // 首次备份
        match backup::perform_backup(
            &backup_pool,
            &backup_db_path,
            &backup_dir_clone,
            Some(&backup_vec_path),
        ) {
            Ok(r) => println!(
                "[Memoria] Auto-backup: {} ({} MB, integrity={}, deleted={}, tier={})",
                r.backup_path,
                r.db_size_bytes / 1048576,
                r.integrity_ok,
                r.rotation_deleted,
                r.tier
            ),
            Err(e) => eprintln!("[Memoria] Auto-backup FAILED: {}", e),
        }
        // 定时循环
        let interval = std::time::Duration::from_secs(backup_interval_hours * 3600);
        loop {
            tokio::time::sleep(interval).await;
            match backup::perform_backup(
                &backup_pool,
                &backup_db_path,
                &backup_dir_clone,
                Some(&backup_vec_path),
            ) {
                Ok(r) => println!(
                    "[Memoria] Auto-backup: {} ({} MB, integrity={}, deleted={}, tier={})",
                    r.backup_path,
                    r.db_size_bytes / 1048576,
                    r.integrity_ok,
                    r.rotation_deleted,
                    r.tier
                ),
                Err(e) => eprintln!("[Memoria] Auto-backup FAILED: {}", e),
            }
        }
    });

    // ── P1-1: 审计日志定时清理（每6小时）──
    let cleanup_auth_pool = state.auth_pool.clone();
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(6 * 3600);
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(conn) = cleanup_auth_pool.get() {
                // 调用 auth 模块的清理函数
                // cleanup_stale_tables 是私有函数，通过 audit_log 间接触发
                // 这里直接执行 SQL 清理
                let now = chrono::Local::now();
                let cutoff = now - chrono::Duration::days(90);
                let cutoff_week = format!("{}W{:02}", cutoff.format("%G"), cutoff.iso_week().week());
                let tables: Vec<String> = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%'")
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| row.get::<_, String>(0))
                            .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    })
                    .unwrap_or_default();
                let mut dropped = 0;
                for table in &tables {
                    let week_str = table.trim_start_matches("audit_log_");
                    if week_str <= cutoff_week.as_str() {
                        let _ = conn.execute(&format!("DROP TABLE IF EXISTS {}", table), []);
                        dropped += 1;
                    }
                }
                if dropped > 0 {
                    println!("[Memoria] Audit cleanup: dropped {} partitions (>90 days)", dropped);
                }
            }
        }
    });

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("[Memoria] Ready on {}", addr);
    axum::serve(listener, app).await.unwrap();
}
