//! P2-4 导入 / 导出 / 跨机迁移
//!
//! - `export_ns`：把某命名空间的全部记忆与实体导出为 **流式 JSONL**（头部一行元数据 +
//!   每行一条记录），写入按 500 行分块拉取，避免一次性把全表 `Vec` 载入内存导致 OOM。
//!   `memory_vectors` 的 BLOB 以 base64 内嵌，默认不导出（embedding 体量大）。
//! - `import_ns`：解析 JSONL 逐行 `INSERT`，冲突策略 `Ignore`（默认，幂等，重复导入不翻倍）
//!   或 `Replace`（覆盖，用于迁移）。导入前校验每行 namespace 必须等于目标 ns，防止跨 ns 污染。
//! - `build_migration_manifest`：生成跨机迁移包清单 —— 对在线 DB 文件与 HNSW 索引文件做
//!   sha256 校验和 + 全表行数。**与 GFS 备份格式统一**：备份产出 `memoria_<ts>.db` +
//!   `hnsw_vectors_<ts>.bin`；迁移包 = 二者 + 本 manifest（可选再附一份 `memory_export` JSONL）。

use crate::storage::SqlitePool;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use rusqlite::params;
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use sha2::{Digest, Sha256};

/// 导出格式版本（写入头部；导入时拒绝更高版本）
pub const EXPORT_VERSION: u32 = 1;
/// 单次拉取分块大小（防止大表 OOM）
const CHUNK: usize = 500;

/// 导入冲突处理策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// INSERT OR IGNORE：已存在（PK 冲突）则跳过（默认，幂等安全）
    Ignore,
    /// INSERT OR REPLACE：已存在则覆盖（迁移用）
    Replace,
}

/// 导入结果报告
#[derive(Debug, Default)]
pub struct ImportReport {
    pub inserted: u64,
    pub ignored: u64,
    pub errors: Vec<String>,
    /// 每张表的 (inserted, ignored)
    pub per_table: std::collections::HashMap<String, (u64, u64)>,
}

/// 表导出规格（列顺序必须稳定，导入按同序反解）
struct TableSpec {
    name: &'static str,
    columns: &'static [&'static str],
    ns_col: &'static str,
    blob_col: Option<&'static str>,
}

const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "memories",
        columns: &[
            "id",
            "namespace",
            "source",
            "content",
            "category",
            "confidence",
            "recall_count",
            "last_recalled",
            "created_at",
            "promoted_at",
            "tier",
            "evidence",
            "importance",
            "decay_factor",
            "tags",
            "valid_from",
            "valid_to",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "user_prefs",
        columns: &[
            "key",
            "value",
            "evidence",
            "confidence",
            "updated_at",
            "namespace",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "decisions",
        columns: &[
            "id",
            "namespace",
            "topic",
            "decision",
            "rationale",
            "context",
            "session_id",
            "created_at",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "memory_relations",
        columns: &[
            "id",
            "namespace",
            "source_id",
            "target_id",
            "relation_type",
            "weight",
            "evidence",
            "created_at",
            "valid_from",
            "valid_to",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "memory_vectors",
        columns: &["id", "namespace", "vector", "updated_at"],
        ns_col: "namespace",
        blob_col: Some("vector"),
    },
    TableSpec {
        name: "entities",
        columns: &[
            "id",
            "namespace",
            "entity_type",
            "name",
            "aliases",
            "summary",
            "created_at",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "entity_mentions",
        columns: &[
            "id",
            "entity_id",
            "memory_id",
            "context",
            "namespace",
            "created_at",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
    TableSpec {
        name: "entity_edges",
        columns: &[
            "id",
            "namespace",
            "source_entity_id",
            "target_entity_id",
            "relation_type",
            "weight",
            "evidence",
            "created_at",
            "valid_from",
            "valid_to",
        ],
        ns_col: "namespace",
        blob_col: None,
    },
];

/// 导出某命名空间为 JSONL 字符串（流式分块，防 OOM）。
///
/// 格式：
/// ```text
/// {"memoria_export":1,"namespace":"acme","exported_at":"...","tables":[...],"counts":{...}}   <- 头部
/// {"table":"memories","row":{...}}                                                          <- 每条记录一行
/// ...
/// ```
pub fn export_ns(pool: &SqlitePool, ns: &str, include_vectors: bool) -> Result<String, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let mut out: Vec<u8> = Vec::new();
    let exported_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // 先统计每张表行数（供头部 + 调用方校验）
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for spec in TABLES {
        if spec.name == "memory_vectors" && !include_vectors {
            continue;
        }
        let c: u64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE {} = ?1",
                    spec.name, spec.ns_col
                ),
                params![ns],
                |r| r.get(0),
            )
            .map_err(|e| format!("count {}: {}", spec.name, e))?;
        counts.insert(spec.name.to_string(), c);
    }

    let tables_vec: Vec<&str> = TABLES
        .iter()
        .filter(|s| s.name != "memory_vectors" || include_vectors)
        .map(|s| s.name)
        .collect();
    let header = serde_json::json!({
        "memoria_export": EXPORT_VERSION,
        "namespace": ns,
        "exported_at": exported_at,
        "tables": tables_vec,
        "counts": counts,
    });
    serde_json::to_writer(&mut out, &header).map_err(|e| format!("write header: {}", e))?;
    out.push(b'\n');

    // 逐表分块流式写出
    for spec in TABLES {
        if spec.name == "memory_vectors" && !include_vectors {
            continue;
        }
        let cols_csv = spec.columns.join(",");
        let sql = format!(
            "SELECT {} FROM {} WHERE {} = ?1 ORDER BY rowid LIMIT ?2 OFFSET ?3",
            cols_csv, spec.name, spec.ns_col
        );
        let mut offset: u64 = 0;
        loop {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| format!("prep {}: {}", spec.name, e))?;
            let rows = stmt
                .query_map(params![ns, CHUNK as i64, offset as i64], |r| {
                    row_to_json(r, spec)
                })
                .map_err(|e| format!("query {}: {}", spec.name, e))?;
            let mut n = 0u64;
            for row in rows {
                let val = row.map_err(|e| format!("row {}: {}", spec.name, e))?;
                serde_json::to_writer(&mut out, &val).map_err(|e| format!("write row: {}", e))?;
                out.push(b'\n');
                n += 1;
            }
            if n == 0 || n < CHUNK as u64 {
                break;
            }
            offset += n;
        }
    }

    String::from_utf8(out).map_err(|e| format!("utf8: {}", e))
}

/// 从 JSONL 导入到目标命名空间。
///
/// - 头行解析版本号（拒绝更高版本）。
/// - 逐行 `INSERT`：冲突策略 `Ignore`（默认，幂等）/ `Replace`（覆盖）。
/// - 每张行的 namespace 必须等于 `ns`，否则记录错误并跳过（防止跨 ns 污染）。
/// - 外层事务包裹，全部成功才提交。
pub fn import_ns(
    pool: &SqlitePool,
    ns: &str,
    jsonl: &str,
    on_conflict: OnConflict,
) -> Result<ImportReport, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let verb = match on_conflict {
        OnConflict::Ignore => "INSERT OR IGNORE",
        OnConflict::Replace => "INSERT OR REPLACE",
    };
    let mut report = ImportReport::default();

    let mut lines = jsonl.lines();
    let header_line = lines.next().ok_or_else(|| "empty export".to_string())?;
    let header: serde_json::Value =
        serde_json::from_str(header_line).map_err(|e| format!("header parse: {}", e))?;
    let version = header
        .get("memoria_export")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "missing version".to_string())?;
    if version > EXPORT_VERSION as u64 {
        return Err(format!("unsupported export version {}", version));
    }

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("tx: {}", e))?;

    for (i, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                report
                    .errors
                    .push(format!("line {} parse error: {}", i + 2, e));
                continue;
            }
        };
        let table = match val.get("table").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                report.errors.push(format!("line {} missing table", i + 2));
                continue;
            }
        };
        let row = match val.get("row").and_then(|v| v.as_object()) {
            Some(r) => r,
            None => {
                report.errors.push(format!("line {} missing row", i + 2));
                continue;
            }
        };
        let spec = match TABLES.iter().find(|s| s.name == table) {
            Some(s) => s,
            None => {
                report
                    .errors
                    .push(format!("line {} unknown table {}", i + 2, table));
                continue;
            }
        };
        // namespace 一致性：防御跨 ns 污染
        let row_ns = row.get(spec.ns_col).and_then(|v| v.as_str()).unwrap_or("");
        if row_ns != ns {
            report.errors.push(format!(
                "line {} ns mismatch (row ns '{}' != target '{}')",
                i + 2,
                row_ns,
                ns
            ));
            continue;
        }

        let values = json_row_to_values(spec, row);
        let cols_csv = spec.columns.join(",");
        let ph = vec!["?"; spec.columns.len()].join(",");
        let sql = format!("{} INTO {} ({}) VALUES ({})", verb, spec.name, cols_csv, ph);
        match conn.execute(&sql, params_from_iter(values.into_iter())) {
            Ok(n) => {
                // n=0 表示 INSERT OR IGNORE 命中 PK 冲突（已存在）→ ignored
                if n == 0 {
                    report.ignored += 1;
                    report
                        .per_table
                        .entry(table.to_string())
                        .or_insert((0, 0))
                        .1 += 1;
                } else {
                    report.inserted += 1;
                    report
                        .per_table
                        .entry(table.to_string())
                        .or_insert((0, 0))
                        .0 += 1;
                }
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("line {} insert {}: {}", i + 2, table, e));
            }
        }
    }

    tx.commit().map_err(|e| format!("commit: {}", e))?;
    Ok(report)
}

/// 计算文件 sha256（流式分块读取，支持大文件）
pub fn compute_sha256(path: &str) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {}", path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 生成跨机迁移包清单：在线 DB 文件 + HNSW 索引文件的 sha256 校验和 + 全表行数。
///
/// 与 GFS 备份格式统一说明见返回 JSON 的 `format_note`。
pub fn build_migration_manifest(
    pool: &SqlitePool,
    db_path: &str,
    hnsw_path: &str,
) -> Result<serde_json::Value, String> {
    let conn = pool.get().map_err(|e| format!("pool: {}", e))?;
    let exported_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut counts = serde_json::Map::new();
    for spec in TABLES {
        let c: u64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", spec.name), [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        counts.insert(spec.name.to_string(), serde_json::json!(c));
    }

    let db_sha = if std::path::Path::new(db_path).exists() {
        compute_sha256(db_path)?
    } else {
        String::new()
    };
    let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let hnsw_bin = format!("{}.bin", hnsw_path);
    let hnsw_sha = if std::path::Path::new(&hnsw_bin).exists() {
        compute_sha256(&hnsw_bin)?
    } else {
        String::new()
    };
    let hnsw_size = std::fs::metadata(&hnsw_bin).map(|m| m.len()).unwrap_or(0);

    Ok(serde_json::json!({
        "memoria_migration_bundle": 1,
        "exported_at": exported_at,
        "format_note": "与 GFS 备份格式统一：备份产出 memoria_<ts>.db + hnsw_vectors_<ts>.bin；迁移包 = 二者 + 本 manifest（可选再附一份 memory_export JSONL）。恢复时先 PRAGMA integrity_check 校验 DB，再复制回主库路径，MemoriaEngine::new 启动时自动重建 HNSW。",
        "db": { "path": db_path, "size_bytes": db_size, "sha256": db_sha },
        "hnsw": { "path": hnsw_bin, "size_bytes": hnsw_size, "sha256": hnsw_sha },
        "row_counts": counts,
    }))
}

// ── 内部辅助 ──

/// 把一行读出为 JSON（`Value::Integer/Real/Text/Null`，BLOB 列 base64）
fn row_to_json(r: &rusqlite::Row, spec: &TableSpec) -> Result<serde_json::Value, rusqlite::Error> {
    let mut obj = serde_json::Map::new();
    for (i, col) in spec.columns.iter().enumerate() {
        let v: SqlValue = r.get(i)?;
        let json_v = match v {
            SqlValue::Null => serde_json::Value::Null,
            SqlValue::Integer(n) => serde_json::json!(n),
            SqlValue::Real(f) => serde_json::json!(f),
            SqlValue::Text(t) => serde_json::Value::String(t),
            SqlValue::Blob(b) => serde_json::Value::String(B64.encode(&b)),
        };
        obj.insert((*col).to_string(), json_v);
    }
    Ok(serde_json::json!({ "table": spec.name, "row": obj }))
}

/// 把 JSON 行还原为 SQLite 参数（保留数字/字符串类型，BLOB 列 base64 解码）
fn json_row_to_values(
    spec: &TableSpec,
    row: &serde_json::Map<String, serde_json::Value>,
) -> Vec<SqlValue> {
    spec.columns
        .iter()
        .map(|col| {
            let v = row.get(*col);
            if Some(*col) == spec.blob_col {
                match v.and_then(|x| x.as_str()) {
                    Some(b64) => match B64.decode(b64) {
                        Ok(b) => SqlValue::Blob(b),
                        Err(_) => SqlValue::Null,
                    },
                    None => SqlValue::Null,
                }
            } else {
                match v {
                    Some(serde_json::Value::Null) | None => SqlValue::Null,
                    Some(serde_json::Value::String(s)) => SqlValue::Text(s.clone()),
                    Some(serde_json::Value::Number(n)) => {
                        if let Some(i) = n.as_i64() {
                            SqlValue::Integer(i)
                        } else if let Some(f) = n.as_f64() {
                            SqlValue::Real(f)
                        } else {
                            SqlValue::Text(n.to_string())
                        }
                    }
                    Some(serde_json::Value::Bool(b)) => SqlValue::Integer(if *b { 1 } else { 0 }),
                    Some(other) => SqlValue::Text(serde_json::to_string(other).unwrap_or_default()),
                }
            }
        })
        .collect()
}
