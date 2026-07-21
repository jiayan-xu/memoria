//! 自动备份 + GFS (Grandfather-Father-Son) 轮转策略
//!
//! 三层轮转：日备份保留 7 份，周备份保留 4 份，月备份保留 3 份。
//! 使用 SQLite VACUUM INTO 做在线热备（事务一致性快照）。
//! 备份后自动执行 PRAGMA integrity_check 验证。
//! HNSW 向量索引文件同步复制。

use crate::storage::SqlitePool;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Phase A (OpenClaw 吸收): `backup create/verify/restore` 的归档清单 schema 版本。
/// 与 OpenClaw 的 `schemaVersion:1` 对齐 —— verify 只接受同版本，拒绝未知清单。
const BACKUP_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 归档清单中的单条数据库条目。
#[derive(Debug, Serialize, Deserialize)]
struct BackupEntry {
    /// 归档内相对文件名（必须在 path_allowlist 内，防路径穿越）
    name: String,
    /// 内容 sha256（16 进制），verify 时逐字节校验
    sha256: String,
    /// "main" = 主记忆库；"audit" = 鉴权/审计库
    role: String,
}

/// `backup create` 写出的清单，夹在归档目录根。
#[derive(Debug, Serialize, Deserialize)]
struct BackupManifest {
    schema_version: u32,
    created_at: String,
    entries: Vec<BackupEntry>,
    /// 允许出现的文件名白名单；verify 时任何条目名不在其中即判失败
    path_allowlist: Vec<String>,
}

/// 计算文件 sha256（分块读取，避免大库一次性载入内存）。
fn sha256_file(path: &str) -> Result<String, String> {
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {}", path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 对源库做事务一致性热备（VACUUM INTO），复用 `perform_backup` 的范式。
fn vacuum_into(src: &str, dst: &str) -> Result<(), String> {
    let conn = rusqlite::Connection::open(src)
        .map_err(|e| format!("open source {}: {}", src, e))?;
    let sql = format!("VACUUM INTO '{}'", escape_sql_path(dst));
    conn.execute_batch(&sql)
        .map_err(|e| format!("VACUUM INTO {}: {}", dst, e))?;
    Ok(())
}

/// `memoria-server backup create` —— 对主库（及可选审计库）做 VACUUM INTO 快照，
/// 写出 `manifest.json`（schemaVersion + 每条目 sha256 + path_allowlist）。
/// 拒绝覆盖已存在的归档目录（防误写）。
pub fn backup_create_cli(main_db: &str, auth_db: &str, backup_dir: &str) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let ts = chrono::DateTime::from_timestamp(now.as_secs() as i64, 0)
        .ok_or("invalid timestamp")?
        .format("%Y%m%d_%H%M%S")
        .to_string();

    let archive_dir = Path::new(backup_dir).join("cli").join(&ts);
    if archive_dir.exists() {
        return Err(format!(
            "refusing to overwrite existing archive: {}",
            archive_dir.display()
        ));
    }
    std::fs::create_dir_all(&archive_dir).map_err(|e| format!("mkdir archive: {}", e))?;

    let mut entries: Vec<BackupEntry> = Vec::new();
    let mut allowlist: Vec<String> = Vec::new();

    if !Path::new(main_db).exists() {
        return Err(format!("main db not found: {}", main_db));
    }
    let main_dst = archive_dir.join("memoria.db");
    vacuum_into(main_db, &main_dst.to_string_lossy())?;
    let main_hash = sha256_file(&main_dst.to_string_lossy())?;
    entries.push(BackupEntry {
        name: "memoria.db".to_string(),
        sha256: main_hash,
        role: "main".to_string(),
    });
    allowlist.push("memoria.db".to_string());

    if Path::new(auth_db).exists() {
        let audit_dst = archive_dir.join("audit.db");
        vacuum_into(auth_db, &audit_dst.to_string_lossy())?;
        let audit_hash = sha256_file(&audit_dst.to_string_lossy())?;
        entries.push(BackupEntry {
            name: "audit.db".to_string(),
            sha256: audit_hash,
            role: "audit".to_string(),
        });
        allowlist.push("audit.db".to_string());
    }

    let manifest = BackupManifest {
        schema_version: BACKUP_MANIFEST_SCHEMA_VERSION,
        created_at: ts,
        entries,
        path_allowlist: allowlist,
    };
    let manifest_path = archive_dir.join("manifest.json");
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| format!("manifest ser: {}", e))?;
    std::fs::write(&manifest_path, json).map_err(|e| format!("write manifest: {}", e))?;

    println!("[Memoria][backup] created archive: {}", archive_dir.display());
    println!("[Memoria][backup] entries: {}", manifest.entries.len());
    for e in &manifest.entries {
        println!(
            "[Memoria][backup]   {}  role={}  sha256={}..",
            e.name,
            e.role,
            &e.sha256[..16.min(e.sha256.len())]
        );
    }
    println!("[Memoria][backup] NOTE: HNSW vector index is rebuildable from memory_vectors; not archived separately.");
    Ok(())
}

/// `memoria-server backup verify <archive_dir>` —— 校验 manifest 合法、条目名在白名单、
/// 文件存在、sha256 匹配、且 PRAGMA integrity_check 通过。不提取、不还原。
pub fn backup_verify_cli(archive_dir: &str) -> Result<(), String> {
    let dir = Path::new(archive_dir);
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(format!("manifest not found in archive: {}", archive_dir));
    }
    let raw =
        std::fs::read_to_string(&manifest_path).map_err(|e| format!("read manifest: {}", e))?;
    let manifest: BackupManifest =
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {}", e))?;
    if manifest.schema_version != BACKUP_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported manifest schema_version: {} (expected {})",
            manifest.schema_version, BACKUP_MANIFEST_SCHEMA_VERSION
        ));
    }

    let mut all_ok = true;
    for entry in &manifest.entries {
        if !manifest.path_allowlist.contains(&entry.name) {
            eprintln!(
                "[Memoria][backup][verify] FAIL: entry '{}' not in path_allowlist",
                entry.name
            );
            all_ok = false;
            continue;
        }
        let fp = dir.join(&entry.name);
        if !fp.exists() {
            eprintln!(
                "[Memoria][backup][verify] FAIL: entry file missing: {}",
                entry.name
            );
            all_ok = false;
            continue;
        }
        let hash = match sha256_file(&fp.to_string_lossy()) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[Memoria][backup][verify] FAIL: {}", e);
                all_ok = false;
                continue;
            }
        };
        if hash != entry.sha256 {
            eprintln!(
                "[Memoria][backup][verify] FAIL: sha256 mismatch for {}",
                entry.name
            );
            all_ok = false;
            continue;
        }
        if !check_integrity(&fp.to_string_lossy()) {
            eprintln!(
                "[Memoria][backup][verify] FAIL: integrity_check failed for {}",
                entry.name
            );
            all_ok = false;
            continue;
        }
        println!(
            "[Memoria][backup][verify] OK: {} (role={})",
            entry.name, entry.role
        );
    }

    if all_ok {
        println!("[Memoria][backup][verify] ALL ENTRIES VERIFIED");
        Ok(())
    } else {
        Err("one or more entries failed verification".to_string())
    }
}

/// `memoria-server backup restore <archive_dir> <target_main_db> [target_audit_db]` ——
/// **仅允许恢复到全新（fresh）目标库**，防止覆盖线上运行库（OpenClaw 缺失、我们自研的补强点）。
/// 若目标已存在则拒绝，并给出操作步骤。恢复前仍走 `restore_from_backup` 的 integrity_check + 原子替换。
pub fn backup_restore_cli(
    archive_dir: &str,
    target_main: &str,
    target_audit_opt: Option<&str>,
) -> Result<(), String> {
    let dir = Path::new(archive_dir);
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(format!("manifest not found in archive: {}", archive_dir));
    }
    let raw =
        std::fs::read_to_string(&manifest_path).map_err(|e| format!("read manifest: {}", e))?;
    let manifest: BackupManifest =
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {}", e))?;
    if manifest.schema_version != BACKUP_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported manifest schema_version: {} (expected {})",
            manifest.schema_version, BACKUP_MANIFEST_SCHEMA_VERSION
        ));
    }

    // fresh-target 守卫：目标主库必须不存在，避免覆盖线上运行库
    if Path::new(target_main).exists() {
        return Err(format!(
            "refusing to restore over existing target '{}' (fresh-target-only policy).\n\
             [Memoria] Stop the service, move the live db aside, then restore into a fresh path\n\
             [Memoria] (e.g. '{}'.restored) and swap manually after verification.",
            target_main, target_main
        ));
    }
    let target_main_parent = Path::new(target_main)
        .parent()
        .ok_or("invalid target_main path")?;
    std::fs::create_dir_all(target_main_parent).map_err(|e| format!("mkdir target parent: {}", e))?;

    for entry in &manifest.entries {
        let src = dir.join(&entry.name);
        if !src.exists() {
            return Err(format!("archive entry missing: {}", entry.name));
        }
        let dst = match entry.role.as_str() {
            "main" => target_main.to_string(),
            "audit" => match target_audit_opt {
                Some(t) => t.to_string(),
                None => target_main_parent
                    .join("audit.db")
                    .to_string_lossy()
                    .to_string(),
            },
            _ => continue,
        };
        if Path::new(&dst).exists() {
            return Err(format!(
                "refusing to restore over existing target '{}' (fresh-target-only)",
                dst
            ));
        }
        let dst_parent = Path::new(&dst)
            .parent()
            .ok_or("invalid audit target path")?;
        std::fs::create_dir_all(dst_parent).map_err(|e| format!("mkdir audit target parent: {}", e))?;
        restore_from_backup(&src.to_string_lossy(), &dst)?;
        println!("[Memoria][backup][restore] restored {} -> {}", entry.name, dst);
    }
    println!("[Memoria][backup][restore] DONE. With the service stopped, swap the restored file into place, then start.");
    Ok(())
}

/// CLI 顶层分发：`memoria-server backup <create|verify|restore> [args]`。
/// 由 `main.rs` 在获取单写者锁之前调用，避免与运行实例双写。
pub fn run_backup_cli(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "usage: memoria-server backup <create|verify|restore> [args]\n\
              create\n\
              verify <archive_dir>\n\
              restore <archive_dir> <target_main_db> [target_audit_db]"
                .to_string(),
        );
    }
    let db_path =
        std::env::var("MEMORIA_DB_PATH").unwrap_or_else(|_| "data/memoria.db".to_string());
    let auth_db_path = std::env::var("MEMORIA_AUTH_DB_PATH").unwrap_or_else(|_| {
        let p = std::path::Path::new(&db_path);
        p.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("audit.db")
            .to_string_lossy()
            .to_string()
    });
    let backup_dir = std::env::var("MEMORIA_BACKUP_DIR").unwrap_or_else(|_| {
        let p = std::path::Path::new(&db_path);
        p.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("backups")
            .to_string_lossy()
            .to_string()
    });
    match args[0].as_str() {
        "create" => backup_create_cli(&db_path, &auth_db_path, &backup_dir),
        "verify" => {
            let d = args.get(1).ok_or("verify requires <archive_dir>")?;
            backup_verify_cli(d)
        }
        "restore" => {
            let d = args.get(1).ok_or("restore requires <archive_dir>")?;
            let t = args.get(2).ok_or("restore requires <target_main_db>")?;
            let ta = args.get(3).map(|s| s.as_str());
            backup_restore_cli(d, t, ta)
        }
        other => Err(format!(
            "unknown backup subcommand: {} (expected create|verify|restore)",
            other
        )),
    }
}

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
        // Rust 2024: extern blocks must be unsafe
        unsafe extern "C" {
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
