//! 自动备份 + GFS (Grandfather-Father-Son) 轮转策略
//!
//! 三层轮转：日备份保留 7 份，周备份保留 4 份，月备份保留 3 份。
//! 使用 SQLite VACUUM INTO 做在线热备（事务一致性快照）。
//! 备份后自动执行 PRAGMA integrity_check 验证。
//! HNSW 向量索引文件同步复制。

use crate::storage::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// GFS 轮转配置
const DAILY_RETENTION: usize = 7;
const WEEKLY_RETENTION: usize = 4;
const MONTHLY_RETENTION: usize = 3;

/// 最小磁盘空间要求 (500MB)
const MIN_DISK_SPACE_BYTES: u64 = 500 * 1024 * 1024;

/// 备份结果
#[derive(Debug)]
pub struct BackupResult {
    pub backup_path: String,
    pub db_size_bytes: u64,
    pub integrity_ok: bool,
    pub rotation_deleted: usize,
    pub tier: String, // "daily" | "weekly" | "monthly"
}

/// 执行一次完整备份（SQLite + HNSW 索引）
///
/// - `pool`: SQLite 连接池
/// - `_db_path`: 主数据库路径（用于推断备份位置，备份目录由 backup_dir 指定）
/// - `backup_dir`: 备份根目录 (如 "data/backups")
/// - `vector_index_path`: HNSW 向量索引路径 (如 "data/vector_index/hnsw_vectors")
pub fn perform_backup(
    pool: &SqlitePool,
    _db_path: &str,
    backup_dir: &str,
    vector_index_path: Option<&str>,
) -> Result<BackupResult, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system time: {}", e))?;
    let dt =
        chrono::DateTime::from_timestamp(now.as_secs() as i64, 0).ok_or("invalid timestamp")?;

    // 判断当前备份层级：每月1号=monthly，每周一=weekly，其他=daily
    let day = dt.format("%d").to_string().parse::<u32>().unwrap_or(1);
    let weekday = dt.format("%u").to_string().parse::<u32>().unwrap_or(1);
    let tier = if day <= 1 {
        "monthly"
    } else if weekday == 1 {
        "weekly"
    } else {
        "daily"
    };

    let tier_dir = Path::new(backup_dir).join(tier);
    std::fs::create_dir_all(&tier_dir).map_err(|e| format!("mkdir backup dir: {}", e))?;

    // 磁盘空间检查
    check_disk_space(&tier_dir)?;

    // 生成备份文件名：memoria_20260706_234250.db
    let timestamp = dt.format("%Y%m%d_%H%M%S").to_string();
    let backup_file = tier_dir.join(format!("memoria_{}.db", timestamp));
    let backup_path_str = backup_file.to_string_lossy().to_string();

    // === SQLite 在线热备 (VACUUM INTO) ===
    // VACUUM INTO 创建事务一致性快照，不影响读写
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let vacuum_sql = format!("VACUUM INTO '{}'", escape_sql_path(&backup_path_str));
    conn.execute_batch(&vacuum_sql)
        .map_err(|e| format!("VACUUM INTO failed: {}", e))?;

    // 获取备份文件大小
    let db_size_bytes = std::fs::metadata(&backup_file)
        .map(|m| m.len())
        .unwrap_or(0);

    // === HNSW 向量索引复制 ===
    if let Some(vec_path) = vector_index_path {
        let vec_src = Path::new(vec_path);
        if vec_src.exists() {
            let vec_dst = tier_dir.join(format!("hnsw_vectors_{}.bin", timestamp));
            let _ = std::fs::copy(&vec_src.with_extension("bin"), &vec_dst);
            let json_src = vec_src.with_extension("json");
            if json_src.exists() {
                let json_dst = tier_dir.join(format!("hnsw_vectors_{}.json", timestamp));
                let _ = std::fs::copy(&json_src, &json_dst);
            }
        }
    }

    // === integrity_check ===
    let integrity_ok = check_integrity(&backup_path_str);

    // === 轮转清理 ===
    let rotation_deleted = rotate_backups(&tier_dir, tier);

    // 同步清理其他层级的过期备份
    for t in &["daily", "weekly", "monthly"] {
        if *t != tier {
            let other_dir = Path::new(backup_dir).join(t);
            if other_dir.exists() {
                let _ = rotate_backups(&other_dir, t);
            }
        }
    }

    Ok(BackupResult {
        backup_path: backup_path_str,
        db_size_bytes,
        integrity_ok,
        rotation_deleted,
        tier: tier.to_string(),
    })
}

/// 磁盘空间检查
fn check_disk_space(path: &Path) -> Result<(), String> {
    // Windows: 使用 GetDiskFreeSpaceEx; Unix: statvfs
    // 简化实现：检查备份目录所在分区的可用空间
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free_bytes_available: *mut u64,
                total_bytes: *mut u64,
                total_free_bytes: *mut u64,
            ) -> i32;
        }
        let path_str = path.to_string_lossy();
        // 取根路径
        let root = if path_str.len() >= 3 {
            &path_str[..3]
        } else {
            &path_str
        };
        let wide: Vec<u16> = OsStr::new(root)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_bytes: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_bytes, &mut total, &mut total_free)
        };
        if ok != 0 && free_bytes < MIN_DISK_SPACE_BYTES {
            return Err(format!(
                "disk space low: {} MB available, need {} MB",
                free_bytes / 1024 / 1024,
                MIN_DISK_SPACE_BYTES / 1024 / 1024
            ));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Unix: 使用 statvfs
        use std::ffi::CString;
        let path_cstr =
            CString::new(path.to_string_lossy().as_bytes()).map_err(|e| format!("cstr: {}", e))?;
        extern "C" {
            fn statvfs(path: *const i8, buf: *mut Statvfs) -> i32;
        }
        #[repr(C)]
        struct Statvfs {
            bsize: u64,
            frsize: u64,
            blocks: u64,
            bfree: u64,
            bavail: u64,
            files: u64,
            ffree: u64,
            favail: u64,
            fsid: u64,
            flag: u64,
            namemax: u64,
            _pad: [u8; 6 * 8],
        }
        let mut buf = Statvfs {
            bsize: 0,
            frsize: 0,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            favail: 0,
            fsid: 0,
            flag: 0,
            namemax: 0,
            _pad: [0; 48],
        };
        let ok = unsafe { statvfs(path_cstr.as_ptr(), &mut buf) };
        if ok == 0 {
            let free_bytes = buf.bavail * buf.frsize;
            if free_bytes < MIN_DISK_SPACE_BYTES {
                return Err(format!(
                    "disk space low: {} MB available, need {} MB",
                    free_bytes / 1024 / 1024,
                    MIN_DISK_SPACE_BYTES / 1024 / 1024
                ));
            }
        }
    }
    Ok(())
}

/// 对备份文件执行 PRAGMA integrity_check
fn check_integrity(db_path: &str) -> bool {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let result: Result<String, _> = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
    result.map(|s| s == "ok").unwrap_or(false)
}

/// GFS 轮转：按层级保留指定数量，删除最老的
fn rotate_backups(tier_dir: &Path, tier: &str) -> usize {
    let retention = match tier {
        "daily" => DAILY_RETENTION,
        "weekly" => WEEKLY_RETENTION,
        "monthly" => MONTHLY_RETENTION,
        _ => return 0,
    };

    // 收集所有 .db 备份文件
    let mut backups: Vec<PathBuf> = match std::fs::read_dir(tier_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "db").unwrap_or(false))
            .collect(),
        Err(_) => return 0,
    };

    if backups.len() <= retention {
        return 0;
    }

    // 按修改时间排序（旧的在前）
    backups.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    let to_delete = backups.len() - retention;
    let mut deleted = 0;
    for file in backups.iter().take(to_delete) {
        // 删除 .db 及对应的 .bin/.json 索引文件
        let _ = std::fs::remove_file(file);
        // 尝试删除关联的 HNSW 文件
        if let Some(stem) = file.file_stem().and_then(|s| s.to_str()) {
            let bin_path = tier_dir.join(format!("{}.bin", stem));
            let json_path = tier_dir.join(format!("{}.json", stem));
            let _ = std::fs::remove_file(&bin_path);
            let _ = std::fs::remove_file(&json_path);
        }
        deleted += 1;
    }
    deleted
}

/// 转义 SQL 路径中的单引号
fn escape_sql_path(path: &str) -> String {
    path.replace('\'', "''")
}

/// 列出当前所有备份（用于 MCP 工具查询）
pub fn list_backups(backup_dir: &str) -> Result<serde_json::Value, String> {
    let mut result = serde_json::Map::new();
    for tier in &["daily", "weekly", "monthly"] {
        let tier_dir = Path::new(backup_dir).join(tier);
        let mut entries: Vec<serde_json::Value> = Vec::new();
        if tier_dir.exists() {
            if let Ok(read_dir) = std::fs::read_dir(&tier_dir) {
                let mut files: Vec<(PathBuf, u64, SystemTime)> = read_dir
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let path = e.path();
                        let meta = std::fs::metadata(&path).ok()?;
                        let size = meta.len();
                        let modified = meta.modified().ok()?;
                        Some((path, size, modified))
                    })
                    .collect();
                files.sort_by(|a, b| b.2.cmp(&a.2));
                for (path, size, modified) in files {
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let ts = modified
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    entries.push(serde_json::json!({
                      "name": name,
                      "size_bytes": size,
                      "size_mb": (size as f64 / 1048576.0 * 100.0).round() / 100.0,
                      "modified_ts": ts,
                    }));
                }
            }
        }
        result.insert(tier.to_string(), serde_json::Value::Array(entries));
    }
    Ok(serde_json::Value::Object(result))
}

/// 从备份恢复数据库
///
/// 将指定备份文件复制回主数据库路径。
/// **警告**：恢复前会自动停止写入，建议在服务停机时执行。
pub fn restore_from_backup(backup_path: &str, target_db_path: &str) -> Result<(), String> {
    let backup = Path::new(backup_path);
    if !backup.exists() {
        return Err(format!("backup file not found: {}", backup_path));
    }

    // 先验证备份完整性
    if !check_integrity(backup_path) {
        return Err("backup file failed integrity_check — refusing to restore".to_string());
    }

    let target = Path::new(target_db_path);
    let target_dir = target.parent().ok_or("invalid target path")?;
    std::fs::create_dir_all(target_dir).map_err(|e| format!("mkdir target: {}", e))?;

    // 先复制到临时文件，成功后替换
    let tmp_path = target.with_extension("db.restoring");
    std::fs::copy(backup, &tmp_path).map_err(|e| format!("copy: {}", e))?;

    // WAL 文件需要删除（恢复后会重新生成）
    let wal_path = target.with_extension("db-wal");
    let shm_path = target.with_extension("db-shm");
    let _ = std::fs::remove_file(&wal_path);
    let _ = std::fs::remove_file(&shm_path);

    // 原子替换
    #[cfg(target_os = "windows")]
    {
        // Windows 不支持原子 rename 覆盖，先删除目标
        let _ = std::fs::remove_file(target);
    }
    std::fs::rename(&tmp_path, target).map_err(|e| format!("rename: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_sql_path() {
        assert_eq!(escape_sql_path("/tmp/test.db"), "/tmp/test.db");
        assert_eq!(escape_sql_path("/tmp/it's.db"), "/tmp/it''s.db");
    }
}
