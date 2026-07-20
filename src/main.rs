//! Memoria 独立二进制入口
//!
//! 环境变量:
//!   MEMORIA_DB_PATH  (default: data/memoria.db)
//!   MEMORIA_PORT     (default: 9003)
//!   MEMORIA_HOST     (default: 127.0.0.1)
//!   MEMORIA_ADMIN_KEY (required — refuse to start if unset/empty)
//!   MEMORIA_BACKUP_DIR (default: data/backups)
//!   MEMORIA_BACKUP_INTERVAL_HOURS (default: 24)
//!   MEMORIA_EMBEDDING_URL (default: http://127.0.0.1:8777/embed — local embed_server.py;
//!                         set to empty ("") to disable HNSW semantic search)

mod mcp_server;
mod permissions;

use chrono::Datelike;
use mcp_server::AppState;
use memoria_core::{auth, backup, health, storage, vector::HnswIndex, web_api};
use std::sync::Arc;
use tower_http::services::ServeDir;

/// 联调：tracing 底板（与 agent-core 对齐）。
/// 日志级别：AGENT_CORE_LOG > RUST_LOG > 默认 info。
/// 格式：带 target 的 fmt subscriber。
fn init_tracing() {
    use tracing_subscriber::filter::EnvFilter;
    let filter = EnvFilter::try_from_env("AGENT_CORE_LOG")
        .or_else(|_| EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

/// 限制 tokio worker 线程数和 blocking 线程池上限
/// 防止 spawn_blocking 在锁争用时无限创建线程导致 CPU 膨胀
fn build_runtime() -> tokio::runtime::Runtime {
    // 运行时线程数可经 env 覆盖，默认沿用 2026-07-11 现场验证值（4 worker / 8 blocking）
    let worker_threads = std::env::var("MEMORIA_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| (1..=32).contains(&n))
        .unwrap_or(4);
    let max_blocking_threads = std::env::var("MEMORIA_MAX_BLOCKING_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| (1..=256).contains(&n))
        .unwrap_or(8);
    if worker_threads != 4 || max_blocking_threads != 8 {
        eprintln!(
            "[Memoria] Runtime threads: worker={}, max_blocking={} (env override)",
            worker_threads, max_blocking_threads
        );
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(max_blocking_threads)
        .thread_name("memoria")
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

fn main() {
    // 联调：tracing 底板（与 agent-core 对齐：AGENT_CORE_LOG > RUST_LOG > 默认 info）
    init_tracing();

    // Phase A (OpenClaw 吸收): `memoria-server backup <create|verify|restore>` 子命令。
    // 必须在获取单写者锁之前分发，避免与运行实例双写同一数据库。
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.len() >= 2 && cli_args[1] == "backup" {
        match backup::run_backup_cli(&cli_args[2..]) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("[Memoria][backup] ERROR: {}", e);
                std::process::exit(1);
            }
        }
    }

    let runtime = build_runtime();
    // P0-4 单写者守卫：启动早期加锁，防止两个 memoria-server 实例并发写同一数据库（双写损坏）。
    let db_path_for_lock =
        std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| "data/memoria.db".to_string());
    let lock_path = single_writer_lock_path(&db_path_for_lock);
    let _single_writer_lock = match acquire_single_writer_lock(&db_path_for_lock) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[Memoria][ERROR] {}", e);
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
    // ── 诊断入口（默认关闭）──
    // 仅当以 `--features diag` 且 `RUSTFLAGS="--cfg tokio_unstable"` 编译时启用，
    // 通过 tokio-console 客户端（默认连 :6669）暴露任务级栈，定位 busy-loop。
    #[cfg(feature = "diag")]
    let _console_guard = console_subscriber::init();

    let db_path = db_path_for_lock;
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
    // P1（审查 2026-07-13）：禁止可预测的 timestamp 自动 key；未设置则拒绝启动。
    let admin_key = match std::env::var("MEMORIA_ADMIN_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!(
                "[Memoria] FATAL: MEMORIA_ADMIN_KEY is required and must be non-empty.\n\
                 [Memoria] Set it in the environment or .env (see .env.example). Refusing to start."
            );
            std::process::exit(1);
        }
    };

    println!("[Memoria] v0.2.0 — standalone MCP server");
    println!("[Memoria] DB: {}", db_path);
    println!("[Memoria] Backup dir: {}", backup_dir);
    println!("[Memoria] Listen: {}:{}", host, port);
    // P0-2 修复：显式暴露到 0.0.0.0 / :: 时告警（远程须走反代 + TLS + auth）
    if is_exposed_bind(&host) {
        eprintln!("[Memoria][WARN] Binding to {} — service is exposed to the network. Use a reverse proxy + TLS + auth for remote access.", host);
    }

    let pool = storage::create_pool(&db_path, 16).expect("pool");
    storage::init_schema(&pool).expect("schema");
    storage::init_core_tables(&pool).expect("core tables");

    // P0: 迁移 — 添加 superseded_by 列
    storage::migrate_superseded_by(&pool).expect("migration: superseded_by");
    // P0+ 吸收 HMS: 添加 event_time 列（事件发生时点，与 valid_from 区分）
    storage::migrate_event_time(&pool).expect("migration: event_time");
    // P0: 迁移 — user_prefs 增加 namespace 列（跨租户隔离，B3 修复）
    storage::migrate_user_prefs_namespace(&pool).expect("migration: user_prefs namespace");
    // 暗知识层 A1: dream_state 升级为 (phase, namespace) 复合 PK + cursor_ts 幂等游标
    storage::migrate_dream_state_ns(&pool).expect("migration: dream_state namespace");
    // P1-5: 三表补充 valid_from/valid_to 列（轻量时序真值 / as_of 查询）
    storage::migrate_temporal(&pool).expect("migration: temporal columns");
    // PR1（Phase B 前置）：memories 增加 actor/memory_type/parent_id/raw_ref 提取元数据列
    storage::migrate_extract_fields(&pool).expect("migration: extract fields");
    storage::migrate_evolution(&pool).expect("migration: evolution");
    // P0/P1: memory_relations CHECK 扩展 updates|extends|derives
    storage::migrate_memory_relation_types(&pool).expect("migration: relation types");
    // P2-2: 配额计数表（滥用防护，按 ns 限额）
    memoria_core::quota::init_quota_table(&pool).expect("init quota table");

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
    // 加载 HNSW 索引；损坏/无法加载 → 软失败回退空索引（语义检索降级），不 panic
    let (hnsw, hnsw_status) = if HnswIndex::exists(&vec_path) {
        match HnswIndex::load(&vec_path) {
            Ok(h) => (h, "ok"),
            Err(e) => {
                eprintln!(
                    "[Memoria] WARN: HNSW 索引损坏/无法加载 ({}); 回退空索引, 语义检索降级",
                    e
                );
                (HnswIndex::new(), "corrupted")
            }
        }
    } else {
        (HnswIndex::new(), "uninitialized")
    };
    // P1-3：以 memory_vectors 持久表为权威源重建 HNSW（.bin 仅作可选快取）
    if let Err(e) = memoria_core::vector::persist::rebuild_hnsw_from_store(&pool, &hnsw) {
        eprintln!("[Memoria] WARN: HNSW rebuild from memory_vectors: {}", e);
    }
    println!("[Memoria] HNSW vectors: {}", hnsw.len());

    // ── P0: 启动健康检查 ──
    println!("[Memoria] Running startup health check...");
    let emb_url = std::env::var("MEMORIA_EMBEDDING_URL").unwrap_or_default();
    let report =
        health::run_health_check(&pool, &auth_pool, &hnsw, &db_path, hnsw_status, &emb_url);
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
        hnsw_status: hnsw_status.to_string(),
        query_cache: Arc::new(memoria_core::vector::QueryCache::new()),
        admin_key,
        bridge_url: std::env::var("MEMORIA_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000/mcp".to_string()),
        // P3-0: 查询时嵌入服务地址。空 = 禁用语义搜索（仅 FTS5+temporal+importance+category）。
        // 默认启用本地嵌入服务（embed_server.py @ 127.0.0.1:8777），让 HNSW 语义检索开箱即用；
        // 显式设为空字符串可关闭（语义信号优雅降级为跳过）。
        embedding_url: std::env::var("MEMORIA_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8777/embed".to_string()),
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
    }); // block_on
    // P0-4：优雅退出时移除单写者锁文件；进程崩溃残留的锁由下次启动的存活检测接管。
    let _ = std::fs::remove_file(&lock_path);
}

// ── P0-2 辅助函数（纯逻辑，便于单测）──

/// P0-2：判断监听地址是否暴露到所有网络接口。
pub fn is_exposed_bind(host: &str) -> bool {
    host == "0.0.0.0" || host == "::" || host == "[::]"
}

/// P0-2：自动生成的 admin key 写入本地受控文件（仅本次启动可查，不回显任何片段）。
/// 返回写入路径。Unix 下设为 0600；Windows 下仅写入数据目录（ACL 模型不同）。
pub fn write_auto_admin_key(db_path: &str, key: &str) -> std::io::Result<std::path::PathBuf> {
    let key_path = std::path::Path::new(db_path)
        .parent()
        .map(|p| p.join("admin_key.secret"))
        .unwrap_or_else(|| std::path::PathBuf::from("admin_key.secret"));
    std::fs::write(&key_path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key_path)
}

// ── P0-4 单写者守卫（纯 std，零新依赖）──

/// P0-4：返回单写者锁文件路径（与数据库同目录，名为 memoria.pid）。
pub fn single_writer_lock_path(db_path: &str) -> std::path::PathBuf {
    std::path::Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("memoria.pid")
}

/// P0-4：获取单写者锁。成功返回持有锁文件的 File（main 退出时删文件）；
/// 若已有存活实例持有锁，则返回 Err 拒绝启动，避免双写同一数据库。
pub fn acquire_single_writer_lock(db_path: &str) -> Result<std::fs::File, String> {
    let lock_path = single_writer_lock_path(db_path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            write_pid(&mut f)?;
            Ok(f)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(old_pid) = read_pid(&lock_path) {
                if is_process_alive(old_pid) {
                    return Err(format!(
                        "Memoria 已在运行（pid {}）。拒绝启动第二个写者以避免双写同一数据库。\
                        如需强制，请先停止旧进程或删除锁文件：{}",
                        old_pid,
                        lock_path.display()
                    ));
                }
                // 残留锁（旧进程已退出）：接管
                let _ = std::fs::remove_file(&lock_path);
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
                {
                    Ok(mut f) => {
                        write_pid(&mut f)?;
                        eprintln!(
                            "[Memoria][WARN] 检测到残留锁文件（pid {} 已退出），已接管。",
                            old_pid
                        );
                        Ok(f)
                    }
                    Err(e2) => Err(format!("无法接管锁文件 {}: {}", lock_path.display(), e2)),
                }
            } else {
                // 无法解析旧 PID：强制重建
                let _ = std::fs::remove_file(&lock_path);
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
                    .map_err(|e| format!("无法创建锁文件 {}: {}", lock_path.display(), e))?;
                write_pid(&mut f)?;
                Ok(f)
            }
        }
        Err(e) => Err(format!("无法创建锁文件 {}: {}", lock_path.display(), e)),
    }
}

fn write_pid(f: &mut std::fs::File) -> Result<(), String> {
    use std::io::Write;
    f.write_all(format!("{}", std::process::id()).as_bytes())
        .map_err(|e| format!("写入 PID 到锁文件失败: {}", e))?;
    f.flush().map_err(|e| format!("flush 锁文件失败: {}", e))
}

fn read_pid(lock_path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(lock_path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    use std::process::Command;
    if let Ok(out) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
        .output()
    {
        // 命中时 CSV 行含该 PID；未命中时输出 "INFO: No tasks..."
        return String::from_utf8_lossy(&out.stdout).contains(&pid.to_string());
    }
    // 无法判定时保守认为存活（拒绝接管，避免误杀健康进程）
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_exposed_bind() {
        assert!(is_exposed_bind("0.0.0.0"));
        assert!(is_exposed_bind("::"));
        assert!(is_exposed_bind("[::]"));
        assert!(!is_exposed_bind("127.0.0.1"));
        assert!(!is_exposed_bind("localhost"));
        assert!(!is_exposed_bind("192.168.1.10"));
    }

    #[test]
    fn test_write_auto_admin_key() {
        let dir = std::env::temp_dir().join(format!("memoria_p02_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("test.db");
        let key = "mem-admin-deadbeefcafe0000";
        let p = write_auto_admin_key(db.to_str().unwrap(), key).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), key);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_single_writer_lock_path() {
        let p = single_writer_lock_path("data/memoria.db");
        assert_eq!(p.file_name().unwrap(), "memoria.pid");
        let p2 = single_writer_lock_path("/abs/path/memoria.db");
        assert_eq!(p2.file_name().unwrap(), "memoria.pid");
    }

    #[test]
    fn test_second_writer_refused_when_alive() {
        let dir = std::env::temp_dir().join(format!("memoria_lock_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("memoria.db");
        let db_s = db.to_str().unwrap().to_string();
        let f1 = acquire_single_writer_lock(&db_s).expect("first writer should acquire lock");
        // 第二个写者持有相同（存活）PID，应被拒绝
        let res = acquire_single_writer_lock(&db_s);
        assert!(
            res.is_err(),
            "second writer must be refused while first is alive"
        );
        drop(f1);
        let _ = std::fs::remove_file(single_writer_lock_path(&db_s));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
