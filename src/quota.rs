//! P2-2 配额与滥用防护（quota & abuse protection）
//!
//! 按命名空间 (ns) 限额，默认策略：
//!   - 写入 (remember / observe，覆盖 Capture 写入路径)：每自然日条数
//!       `MEMORIA_QUOTA_WRITES_PER_DAY`  (默认 10000)
//!   - 搜索 (search)：每分钟次数
//!       `MEMORIA_QUOTA_SEARCHES_PER_MIN` (默认 600 = 10 QPS/ns)
//!   - 备份 (backup)：每小时次数
//!       `MEMORIA_QUOTA_BACKUPS_PER_HOUR` (默认 12)
//!
//! 设计要点：
//!   - 计数表 `quota_counters(ns, kind, window, count)` 按时间桶聚合，桶滚动即自然归零，
//!     无需后台清理任务。
//!   - 写递增用 `BEGIN IMMEDIATE` 串行化，避免并发竞态导致超额放行。
//!   - 超限硬拒绝（返回 `QuotaError::Exceeded`）+ 调用方负责审计（denied 落 audit_log）。
//!   - admin 角色豁免（避免运维自锁），配额主要保护 agent / Capture 命名空间免受风暴。

use crate::storage::SqlitePool;
use chrono::Timelike;
use rusqlite::params;

/// 配额类型常量
pub const KIND_WRITE: &str = "write";
pub const KIND_SEARCH: &str = "search";
pub const KIND_BACKUP: &str = "backup";

/// 配额超限错误（含提示客户端退避的秒数）
#[derive(Debug, PartialEq, Eq)]
pub struct QuotaError {
    pub kind: String,
    pub limit: u64,
    pub retry_after_sec: u64,
}

/// 建表（幂等）。在 `init_core_tables` 之后调用一次。
pub fn init_quota_table(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS quota_counters (
            ns TEXT NOT NULL,
            kind TEXT NOT NULL,
            window TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (ns, kind, window)
        );",
    )
    .map_err(|e| format!("create quota_counters: {}", e))
}

/// 返回当前时间桶字符串（决定配额滚动周期）。
pub fn quota_window(kind: &str) -> String {
    let now = chrono::Local::now();
    match kind {
        KIND_WRITE => now.format("%Y-%m-%d").to_string(),
        KIND_SEARCH => now.format("%Y-%m-%dT%H:%M").to_string(),
        KIND_BACKUP => now.format("%Y-%m-%dT%H").to_string(),
        _ => now.format("%Y-%m-%d").to_string(),
    }
}

/// 限额（env 可配，带默认值）。
pub fn quota_limit(kind: &str) -> u64 {
    let (env_key, default) = match kind {
        KIND_WRITE => ("MEMORIA_QUOTA_WRITES_PER_DAY", 10_000u64),
        KIND_SEARCH => ("MEMORIA_QUOTA_SEARCHES_PER_MIN", 600u64),
        KIND_BACKUP => ("MEMORIA_QUOTA_BACKUPS_PER_HOUR", 12u64),
        _ => return u64::MAX,
    };
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// 距离下一时间桶开始的秒数（供 `retry-after` 提示客户端退避）。
pub fn retry_after_sec(kind: &str) -> u64 {
    let now = chrono::Local::now();
    match kind {
        KIND_WRITE => {
            // 距次日 00:00
            let sec_of_day = now.hour() as u64 * 3600 + now.minute() as u64 * 60 + now.second() as u64;
            (24 * 3600 - sec_of_day) % (24 * 3600)
        }
        KIND_SEARCH => {
            // 距下一分钟边界
            (60 - now.second() as u64) % 60
        }
        KIND_BACKUP => {
            // 距下一小时边界
            let sec_of_hour = now.minute() as u64 * 60 + now.second() as u64;
            (3600 - sec_of_hour) % 3600
        }
        _ => 60,
    }
}

/// 检查并递增计数；超限返回 `QuotaError::Exceeded`。
///
/// 使用 `BEGIN IMMEDIATE` 在事务开始时即获取写锁，使并发请求串行化，
/// 避免「读-改-写」竞态导致超额放行。
pub fn check_quota_with(
    pool: &SqlitePool,
    ns: &str,
    kind: &str,
    limit: u64,
) -> Result<(), QuotaError> {
    let window = quota_window(kind);
    let exceeded = || QuotaError {
        kind: kind.to_string(),
        limit,
        retry_after_sec: retry_after_sec(kind),
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return Err(exceeded()),
    };
    if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
        return Err(exceeded());
    }
    let current: i64 = conn
        .query_row(
            "SELECT count FROM quota_counters WHERE ns=? AND kind=? AND window=?",
            params![ns, kind, window],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let result = if limit == 0 {
        // 限额为 0 = 拒绝一切（禁写开关）
        Err(exceeded())
    } else if current >= limit as i64 {
        Err(exceeded())
    } else if current == 0 {
        conn.execute(
            "INSERT INTO quota_counters(ns, kind, window, count) VALUES (?, ?, ?, 1)",
            params![ns, kind, window],
        )
        .map(|_| ())
        .map_err(|_| exceeded())
    } else {
        conn.execute(
            "UPDATE quota_counters SET count = count + 1 WHERE ns=? AND kind=? AND window=?",
            params![ns, kind, window],
        )
        .map(|_| ())
        .map_err(|_| exceeded())
    };

    if result.is_ok() {
        let _ = conn.execute_batch("COMMIT");
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

/// 生产入口：限额取自 env（默认值见 `quota_limit`）。
pub fn check_quota(pool: &SqlitePool, ns: &str, kind: &str) -> Result<(), QuotaError> {
    check_quota_with(pool, ns, kind, quota_limit(kind))
}

/// 当前命名空间某类型的已用计数（供状态查询工具使用）。
pub fn current_usage(pool: &SqlitePool, ns: &str, kind: &str) -> i64 {
    let window = quota_window(kind);
    pool.get()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT count FROM quota_counters WHERE ns=? AND kind=? AND window=?",
                params![ns, kind, window],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .unwrap_or(0)
}
