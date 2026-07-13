//! 会话文件监听 — 替换 Python Capture Proxy
//!
//! 轮询 Reasonix 会话目录，自动推入记忆。

use crate::storage::SqlitePool;
use crate::tools;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// 默认监听的会话目录（可通过 WATCH_DIRS 环境变量设置，逗号分隔）
/// 示例: WATCH_DIRS=C:\sessions\reasonix,D:\data\chats
fn watch_dirs() -> Vec<String> {
    let env = std::env::var("WATCH_DIRS").unwrap_or_default();
    if env.is_empty() {
        Vec::new()
    } else {
        env.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// 每轮最多处理的消息数
const MAX_PER_POLL: usize = 20;

/// 启动持久监听循环（在 tokio::spawn 中运行）
/// 所有文件系统操作使用 spawn_blocking 隔离，避免阻塞 async worker 线程。
pub async fn watch_sessions_loop(pool: SqlitePool) {
    // 文件偏移跟踪: canonical_path -> bytes_read
    let mut offsets: HashMap<String, u64> = HashMap::new();

    println!("[SessionWatcher] Started (poll every 5s)");

    // 初始化 watch_dirs（一次性读取，不再每次 poll 都重新读环境变量）
    let dirs = watch_dirs();
    if dirs.is_empty() {
        println!("[SessionWatcher] WATCH_DIRS 未设置，不监听");
        return;
    }
    for d in &dirs {
        println!("[SessionWatcher] Watching: {}", d);
        // 初始化文件偏移 — 用 spawn_blocking 隔离文件系统操作
        let dir_clone = d.clone();
        let init_offsets: HashMap<String, u64> = tokio::task::spawn_blocking(move || {
            let mut off = HashMap::new();
            if let Ok(entries) = std::fs::read_dir(&dir_clone) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        if let Ok(meta) = std::fs::metadata(&p) {
                            off.insert(canonical(&p), meta.len());
                        }
                    }
                }
            }
            off
        })
        .await
        .unwrap_or_default();
        offsets.extend(init_offsets);
    }

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        // poll_once 内含同步文件系统操作，用 spawn_blocking 隔离
        let pool_clone = pool.clone();
        let dirs_clone = dirs.clone();
        let offsets_clone = offsets.clone();
        match tokio::task::spawn_blocking(move || {
            poll_once_blocking(&pool_clone, &dirs_clone, &mut offsets_clone.clone())
        })
        .await
        {
            Ok(new_offsets) => {
                offsets = new_offsets;
            }
            Err(e) => eprintln!("[SessionWatcher] poll task panicked: {}", e),
        }
    }
}

/// 同步版本的 poll_once（在 spawn_blocking 中调用）
/// 所有文件系统操作在此函数内同步执行，不会阻塞 async worker 线程。
fn poll_once_blocking(
    pool: &SqlitePool,
    dirs: &[String],
    offsets: &mut HashMap<String, u64>,
) -> HashMap<String, u64> {
    let mut total = 0usize;

    for dir in dirs {
        let dir_path = Path::new(dir);
        if !dir_path.exists() {
            continue;
        }

        let entries = match std::fs::read_dir(dir_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let key = canonical(&p);
            let offset = offsets.get(&key).copied().unwrap_or(0);

            let meta = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // 微信网关等频繁创建新文件：创建不足 30 秒的不处理
            let elapsed = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .unwrap_or_default();
            if elapsed < std::time::Duration::from_secs(30) {
                continue;
            }
            let new_len = meta.len();
            if new_len == offset {
                continue; // 未增长
            }
            // 文件变短（被覆盖重写）：从头开始读
            let read_offset = if new_len < offset { 0 } else { offset };

            // 只读新增部分
            let lines = match read_new_lines(&p, read_offset) {
                Ok(l) => l,
                Err(_) => continue,
            };

            for line in &lines {
                if let Some(text) = extract_dialog_text(line) {
                    if total >= MAX_PER_POLL {
                        break;
                    }
                    let _ = tools::observe::observe(
                        pool,
                        &text,
                        "user",
                        "session_watcher",
                        &p.to_string_lossy(),
                        "default",
                    );
                    total += 1;
                }
            }

            offsets.insert(key, new_len);
        }
    }

    // 返回更新后的 offsets
    offsets.clone()
}

/// 读文件从 offset 开始的所有行（只读模式，不触发文件系统变更通知）
fn read_new_lines(path: &Path, offset: u64) -> Result<Vec<String>, String> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(path)
        .map_err(|e| format!("open: {}", e))?;
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek: {}", e))?;

    let mut lines = Vec::new();
    let mut buf = String::new();
    while reader
        .read_line(&mut buf)
        .map_err(|e| format!("read: {}", e))?
        > 0
    {
        let trimmed = buf.trim().to_string();
        if !trimmed.is_empty() {
            lines.push(trimmed);
        }
        buf.clear();
    }
    Ok(lines)
}

/// 从 JSONL 行中提取用户对话文本
fn extract_dialog_text(line: &str) -> Option<String> {
    // JSONL 格式大致为: {"role":"user","content":"...","ts":"..."}
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
        // 只提取 user 角色的内容
        let role = val.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "user" || role == "human" {
            return val
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

fn canonical(p: &Path) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string_lossy().to_string())
}
