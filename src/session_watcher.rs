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
        env.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }
}

/// 每轮最多处理的消息数
const MAX_PER_POLL: usize = 20;

/// 启动持久监听循环（在 tokio::spawn 中运行）
pub async fn watch_sessions_loop(pool: SqlitePool) {
    // 文件偏移跟踪: canonical_path -> bytes_read
    let mut offsets: HashMap<String, u64> = HashMap::new();

    println!("[SessionWatcher] Started (poll every 5s)");
    let dirs = watch_dirs();
    if dirs.is_empty() {
        println!("[SessionWatcher] WATCH_DIRS 未设置，不监听");
        return;
    }
    for d in &dirs {
        println!("[SessionWatcher] Watching: {}", d);
        // 初始化文件偏移
        if let Ok(entries) = std::fs::read_dir(d) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Ok(meta) = std::fs::metadata(&p) {
                        offsets.insert(canonical(&p), meta.len());
                    }
                }
            }
        }
    }

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        if let Err(e) = poll_once(&pool, &mut offsets).await {
            eprintln!("[SessionWatcher] poll error: {}", e);
        }
    }
}

async fn poll_once(
    pool: &SqlitePool,
    offsets: &mut HashMap<String, u64>,
) -> Result<(), String> {
    let mut total = 0usize;

    for dir in &watch_dirs() {
        let dir_path = Path::new(dir);
        if !dir_path.exists() {
            continue;
        }

        let entries = std::fs::read_dir(dir_path).map_err(|e| format!("read_dir: {}", e))?;
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
            let new_len = meta.len();
            if new_len <= offset {
                continue; // 未增长
            }

            // 只读新增部分
            let lines = match read_new_lines(&p, offset) {
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

    Ok(())
}

/// 读文件从 offset 开始的所有行
fn read_new_lines(path: &Path, offset: u64) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {}", e))?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset)).map_err(|e| format!("seek: {}", e))?;

    let mut lines = Vec::new();
    let mut buf = String::new();
    while reader.read_line(&mut buf).map_err(|e| format!("read: {}", e))? > 0 {
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
            return val.get("content").and_then(|c| c.as_str()).map(|s| s.to_string());
        }
    }
    None
}

fn canonical(p: &Path) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string_lossy().to_string())
}
