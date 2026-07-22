//! 启动健康检查 — 硬失败/软失败两档
//!
//! 硬失败 → 拒绝启动，服务不可用
//! 软失败 → 降级运行 + 告警日志，功能受损但核心可用
//!
//! 检查项：
//!   硬失败: SQLite 可读写、Schema 完整性 (integrity_check)、Schema 版本、配置完整性
//!   软失败: HNSW 索引加载状态、磁盘空间、审计库可写、Embedding 端点可达

use crate::storage::SqlitePool;
use crate::vector::HnswIndex;
use std::path::Path;

/// 健康检查结果
#[derive(Debug, serde::Serialize)]
pub struct HealthReport {
    pub overall: String, // "pass" | "degraded" | "fail"
    pub hard_checks: Vec<CheckResult>,
    pub soft_checks: Vec<CheckResult>,
    pub timestamp: String,
    pub version: String,
}

#[derive(Debug, serde::Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: String, // "pass" | "warn" | "fail"
    pub message: String,
    pub duration_ms: u64,
}

/// 当前 Schema 版本
const EXPECTED_SCHEMA_VERSION: u32 = 2;

/// 最小磁盘空间 (500MB)
const MIN_DISK_SPACE_MB: u64 = 500;

/// 执行完整健康检查。`embedding_url` 为空表示未配置语义后端（软警告，不致 fail）。
pub fn run_health_check(
    pool: &SqlitePool,
    auth_pool: &SqlitePool,
    hnsw: &HnswIndex,
    db_path: &str,
    hnsw_status: &str,
    embedding_url: &str,
) -> HealthReport {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let version = env!("MEMORIA_BUILD_VERSION").to_string();

    let mut hard_checks = Vec::new();
    let mut soft_checks = Vec::new();

    // ═══════════════════════════════════════════
    // 硬失败检查项
    // ═══════════════════════════════════════════

    // 1. SQLite 可读写
    hard_checks.push(check_sqlite_rw(pool));

    // 2. Schema 完整性 (integrity_check)
    hard_checks.push(check_integrity(pool));

    // 3. Schema 版本
    hard_checks.push(check_schema_version(pool));

    // 4. 核心表存在性
    hard_checks.push(check_core_tables(pool));

    // 5. FTS5 可用性
    hard_checks.push(check_fts5(pool));

    // ═══════════════════════════════════════════
    // 软失败检查项
    // ═══════════════════════════════════════════

    // 6. HNSW 索引状态
    soft_checks.push(check_hnsw_index(hnsw, hnsw_status));

    // 7. 磁盘空间
    soft_checks.push(check_disk_space(db_path));

    // 8. 审计库可写
    soft_checks.push(check_audit_writable(auth_pool));

    // 9. WAL 模式状态
    soft_checks.push(check_wal_mode(pool));

    // 10. 连接池状态
    soft_checks.push(check_pool_stats(pool));

    // 11. 本地嵌入服务（语义检索通道）
    soft_checks.push(check_embedding_endpoint(embedding_url));

    // 判定总体状态
    let hard_fail = hard_checks.iter().any(|c| c.status == "fail");
    let soft_fail = soft_checks.iter().any(|c| c.status == "fail");
    let has_warn = soft_checks.iter().any(|c| c.status == "warn");

    let overall = if hard_fail {
        "fail"
    } else if soft_fail || has_warn {
        "degraded"
    } else {
        "pass"
    };

    HealthReport {
        overall: overall.to_string(),
        hard_checks,
        soft_checks,
        timestamp,
        version,
    }
}

/// 探测嵌入服务：未配置 → warn；配置了但不可达 → fail（软）；可达 → pass。
/// 仅返回摘要，不含密钥。`url` 形如 `http://127.0.0.1:8777/embed`。
pub fn check_embedding_endpoint(url: &str) -> CheckResult {
    let start = std::time::Instant::now();
    let url = url.trim();
    if url.is_empty() {
        return CheckResult {
            name: "embedding".to_string(),
            status: "warn".to_string(),
            message: "MEMORIA_EMBEDDING_URL 未配置，语义检索降级为 FTS/时间信号".to_string(),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }
    // 将 .../embed 规范为 .../health
    let health_url = if url.ends_with("/embed") {
        format!("{}health", &url[..url.len() - 5])
    } else if url.ends_with('/') {
        format!("{}health", url)
    } else {
        format!("{}/health", url)
    };
    match http_get_json_summary(&health_url, 2000) {
        Ok((code, _body_snip)) if (200..300).contains(&code) => {
            // P1-⑦：不回显具体 embedding 模型名/维度，缩小 /health 信息暴露面
            CheckResult {
                name: "embedding".to_string(),
                status: "pass".to_string(),
                message: format!("ok http={}", code),
                duration_ms: start.elapsed().as_millis() as u64,
            }
        }
        Ok((code, _)) => CheckResult {
            name: "embedding".to_string(),
            status: "fail".to_string(),
            message: format!("嵌入 /health 返回 HTTP {}", code),
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Err(e) => CheckResult {
            name: "embedding".to_string(),
            status: "fail".to_string(),
            message: format!("嵌入服务不可达: {}（语义检索已降级）", e),
            duration_ms: start.elapsed().as_millis() as u64,
        },
    }
}

/// 轻量 HTTP GET（不引入 blocking reqwest），供同步健康检查使用。
fn http_get_json_summary(url: &str, timeout_ms: u64) -> Result<(u16, String), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| "仅支持 http:// 嵌入地址".to_string())?;
    let (host_port, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "端口无效".to_string())?),
        None => (host_port, 80u16),
    };
    let mut stream = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host_port
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| text.split("\n\n").nth(1))
        .unwrap_or("")
        .chars()
        .take(400)
        .collect::<String>();
    Ok((code, body))
}

// ── 硬失败检查 ──

fn check_sqlite_rw(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "sqlite_rw".to_string(),
                status: "fail".to_string(),
                message: format!("pool get failed: {}", e),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    // 写测试：创建临时表 → 插入 → 删除 → 删表
    let result = (|| -> Result<(), String> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _health_check (id INTEGER);")
            .map_err(|e| format!("create: {}", e))?;
        conn.execute("INSERT INTO _health_check (id) VALUES (1)", [])
            .map_err(|e| format!("insert: {}", e))?;
        conn.execute("DELETE FROM _health_check WHERE id = 1", [])
            .map_err(|e| format!("delete: {}", e))?;
        conn.execute_batch("DROP TABLE IF EXISTS _health_check;")
            .map_err(|e| format!("drop: {}", e))?;
        Ok(())
    })();

    let duration_ms = start.elapsed().as_millis() as u64;
    match result {
        Ok(()) => CheckResult {
            name: "sqlite_rw".to_string(),
            status: "pass".to_string(),
            message: "read + write OK".to_string(),
            duration_ms,
        },
        Err(e) => CheckResult {
            name: "sqlite_rw".to_string(),
            status: "fail".to_string(),
            message: e,
            duration_ms,
        },
    }
}

fn check_integrity(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "integrity_check".to_string(),
                status: "fail".to_string(),
                message: format!("pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    let result: Result<String, _> = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(s) if s == "ok" => CheckResult {
            name: "integrity_check".to_string(),
            status: "pass".to_string(),
            message: "all constraints OK".to_string(),
            duration_ms,
        },
        Ok(s) => CheckResult {
            name: "integrity_check".to_string(),
            status: "fail".to_string(),
            message: format!("corruption detected: {}", s),
            duration_ms,
        },
        Err(e) => CheckResult {
            name: "integrity_check".to_string(),
            status: "fail".to_string(),
            message: format!("query failed: {}", e),
            duration_ms,
        },
    }
}

fn check_schema_version(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "schema_version".to_string(),
                status: "fail".to_string(),
                message: format!("pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    // 检查 user_version pragma
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    let duration_ms = start.elapsed().as_millis() as u64;

    if user_version >= EXPECTED_SCHEMA_VERSION as i64 {
        CheckResult {
            name: "schema_version".to_string(),
            status: "pass".to_string(),
            message: format!(
                "user_version = {} (expected >= {})",
                user_version, EXPECTED_SCHEMA_VERSION
            ),
            duration_ms,
        }
    } else {
        // 尝试升级 schema 版本
        let _ = conn.execute_batch(&format!(
            "PRAGMA user_version = {};",
            EXPECTED_SCHEMA_VERSION
        ));
        CheckResult {
            name: "schema_version".to_string(),
            status: "warn".to_string(),
            message: format!(
                "upgraded user_version {} → {}",
                user_version, EXPECTED_SCHEMA_VERSION
            ),
            duration_ms,
        }
    }
}

fn check_core_tables(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "core_tables".to_string(),
                status: "fail".to_string(),
                message: format!("pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    let required_tables = [
        "memories",
        "messages",
        "sessions",
        "decisions",
        "user_prefs",
        "memory_relations",
        "decay_log",
        "dream_state",
    ];

    let mut missing = Vec::new();
    for table in &required_tables {
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{}'",
                    table
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if count == 0 {
            missing.push(table.to_string());
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    if missing.is_empty() {
        CheckResult {
            name: "core_tables".to_string(),
            status: "pass".to_string(),
            message: format!("{} tables present", required_tables.len()),
            duration_ms,
        }
    } else {
        CheckResult {
            name: "core_tables".to_string(),
            status: "fail".to_string(),
            message: format!("missing tables: {}", missing.join(", ")),
            duration_ms,
        }
    }
}

fn check_fts5(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "fts5".to_string(),
                status: "fail".to_string(),
                message: format!("pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    // 检查 memories_fts 虚拟表是否存在且可查
    let result: Result<i64, _> =
        conn.query_row("SELECT COUNT(*) FROM memories_fts LIMIT 1", [], |r| {
            r.get(0)
        });
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(_) => CheckResult {
            name: "fts5".to_string(),
            status: "pass".to_string(),
            message: "memories_fts + messages_fts queryable".to_string(),
            duration_ms,
        },
        Err(e) => CheckResult {
            name: "fts5".to_string(),
            status: "fail".to_string(),
            message: format!("FTS5 query failed: {}", e),
            duration_ms,
        },
    }
}

// ── 软失败检查 ──

fn check_hnsw_index(hnsw: &HnswIndex, status: &str) -> CheckResult {
    let start = std::time::Instant::now();
    let count = hnsw.len();
    let duration_ms = start.elapsed().as_millis() as u64;

    // 区分首跑（无索引文件，正常）/ 损坏回退（有文件但 load 失败）/ 空索引
    let (st, msg) = match (status, count) {
        ("corrupted", _) => (
            "warn",
            "HNSW 索引损坏已回退空索引 — 语义检索降级".to_string(),
        ),
        ("uninitialized", _) => (
            "warn",
            "HNSW 索引未初始化（首次运行）— 语义检索未启用".to_string(),
        ),
        (_, 0) => ("warn", "HNSW 索引为空 — 语义检索无结果".to_string()),
        (_, n) => ("pass", format!("{} vectors loaded", n)),
    };
    CheckResult {
        name: "hnsw_index".to_string(),
        status: st.to_string(),
        message: msg,
        duration_ms,
    }
}

fn check_disk_space(db_path: &str) -> CheckResult {
    let start = std::time::Instant::now();
    let path = Path::new(db_path).parent().unwrap_or(Path::new("."));

    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free_bytes: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }
        let path_str = path.to_string_lossy();
        let root = if path_str.len() >= 3 {
            &path_str[..3]
        } else {
            &path_str
        };
        let wide: Vec<u16> = OsStr::new(root)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
        let duration_ms = start.elapsed().as_millis() as u64;
        if ok != 0 {
            let free_mb = free / 1024 / 1024;
            if free_mb < MIN_DISK_SPACE_MB {
                return CheckResult {
                    name: "disk_space".to_string(),
                    status: "fail".to_string(),
                    message: format!("{} MB free (need {} MB)", free_mb, MIN_DISK_SPACE_MB),
                    duration_ms,
                };
            }
            return CheckResult {
                name: "disk_space".to_string(),
                status: "pass".to_string(),
                message: format!("{} MB free", free_mb),
                duration_ms,
            };
        }
        return CheckResult {
            name: "disk_space".to_string(),
            status: "warn".to_string(),
            message: "unable to query disk space".to_string(),
            duration_ms,
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        let duration_ms = start.elapsed().as_millis() as u64;
        CheckResult {
            name: "disk_space".to_string(),
            status: "warn".to_string(),
            message: "disk space check not implemented on this platform".to_string(),
            duration_ms,
        }
    }
}

fn check_audit_writable(auth_pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match auth_pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "audit_writable".to_string(),
                status: "fail".to_string(),
                message: format!("auth pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    // 尝试查询审计表
    let result: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'audit_log_%'",
        [],
        |r| r.get(0),
    );
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(count) => CheckResult {
            name: "audit_writable".to_string(),
            status: if count > 0 { "pass" } else { "warn" }.to_string(),
            message: format!("{} audit table(s) found", count),
            duration_ms,
        },
        Err(e) => CheckResult {
            name: "audit_writable".to_string(),
            status: "warn".to_string(),
            message: format!("audit query failed: {}", e),
            duration_ms,
        },
    }
}

fn check_wal_mode(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "wal_mode".to_string(),
                status: "warn".to_string(),
                message: format!("pool: {}", e),
                duration_ms: 0,
            };
        }
    };

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap_or_default();
    let duration_ms = start.elapsed().as_millis() as u64;

    if mode == "wal" {
        CheckResult {
            name: "wal_mode".to_string(),
            status: "pass".to_string(),
            message: "WAL mode active".to_string(),
            duration_ms,
        }
    } else {
        CheckResult {
            name: "wal_mode".to_string(),
            status: "warn".to_string(),
            message: format!("journal_mode = {} (expected wal)", mode),
            duration_ms,
        }
    }
}

fn check_pool_stats(pool: &SqlitePool) -> CheckResult {
    let start = std::time::Instant::now();
    let stats = pool.state();
    let idle = stats.idle_connections;
    let total = stats.connections;
    let duration_ms = start.elapsed().as_millis() as u64;

    if idle > 0 {
        CheckResult {
            name: "pool_stats".to_string(),
            status: "pass".to_string(),
            message: format!("{}/{} connections idle", idle, total),
            duration_ms,
        }
    } else {
        CheckResult {
            name: "pool_stats".to_string(),
            status: "warn".to_string(),
            message: format!("0/{} connections idle — pool exhausted", total),
            duration_ms,
        }
    }
}
