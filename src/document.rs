//! 部门共享文档入库：PDF / DOCX 抽文本 → 分块 → memory_remember。
//!
//! 约定（2026-07-23）：
//! - 二进制旁路存 `data/documents/...`，记忆行 `raw_ref` 指向相对路径
//! - `memory_type=document`；清单行 `parent_id=NULL`，分块挂 `parent_id=清单id`
//! - 目标 ns 由调用方指定（部门共享典型：`org/cs-pufa-2nd-thermal/dept/gufei`）

use crate::storage::SqlitePool;
use crate::tools::remember::{self, RememberResult};
use crate::vector::HnswIndex;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fs;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

pub const MAX_DOC_BYTES: u64 = 20 * 1024 * 1024; // 20 MiB
pub const CHUNK_CHARS: usize = 3500;
pub const DEFAULT_DEPT_NS: &str = "org/cs-pufa-2nd-thermal/dept/gufei";

/// 嵌入回调：文本 → 向量（None = 降级不嵌）。
///
/// P0-1（2026-10-13）：文档 ingestion 的向量注入口。此前 ingest 路径调
/// `remember_with_dedup` 时 hnsw/query_cache 恒传 None → candidate_vector
/// 结构上恒空 → 文档记忆（清单 + 全部块）永不落 memory_vectors、不进 HNSW，
/// 语义召回完全不可见（仅 FTS 可捞）。生产库实测 995 条无向量记忆中 602 条
/// 即此路径产物（2026-09/10 的 [文档]/[文档块]）。
///
/// 调用方在**同步阻塞上下文**内提供（MCP dispatch 的 spawn_blocking /
/// web_api 上传的 spawn_blocking），典型实现用临时 current-thread 运行时
/// block_on 异步嵌入请求（见 `mcp_server::block_on_embed`，与 block_on_rerank
/// 同款范式）。回调返回 None（未配置 embed 服务 / 请求失败）= fail-open 降级：
/// 记忆照常写入可用 FTS 召回，向量留待补嵌脚本（rebuild_vectors.py 扩展版）。
pub type EmbedFn<'a> = &'a dyn Fn(&str) -> Option<Vec<f32>>;

#[derive(Debug, Clone)]
pub struct IngestOutcome {
    pub doc_id: String,
    pub namespace: String,
    pub filename: String,
    pub kind: String,
    pub raw_ref: String,
    pub chars: usize,
    pub chunk_count: usize,
    pub manifest_id: String,
    pub chunk_ids: Vec<String>,
}

pub fn detect_kind(filename: &str, content_type: Option<&str>) -> Option<&'static str> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") || content_type == Some("application/pdf") {
        return Some("pdf");
    }
    if lower.ends_with(".docx")
        || content_type
            == Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    {
        return Some("docx");
    }
    if lower.ends_with(".xlsx")
        || content_type == Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
    {
        return Some("xlsx");
    }
    if lower.ends_with(".xls") || content_type == Some("application/vnd.ms-excel") {
        return Some("xls");
    }
    None
}

pub fn extract_text(kind: &str, bytes: &[u8]) -> Result<String, String> {
    match kind {
        "pdf" => extract_pdf(bytes),
        "docx" => extract_docx(bytes),
        "xlsx" | "xls" => extract_spreadsheet(bytes),
        _ => Err(format!("unsupported kind: {kind}")),
    }
}

fn extract_pdf(bytes: &[u8]) -> Result<String, String> {
    let text = match catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    })) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(format!("PDF parse error: {e}")),
        Err(_) => return Err("PDF parsing failed unexpectedly".into()),
    };
    let meaningful = text.chars().filter(|c| !c.is_whitespace()).count();
    if meaningful < 50 {
        return Err("PDF 似乎是扫描件/纯图片，当前不支持 OCR。请上传可选中文字的 PDF。".into());
    }
    Ok(text)
}

fn extract_docx(bytes: &[u8]) -> Result<String, String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|e| format!("DOCX zip: {e}"))?;
    let mut doc_xml = zip
        .by_name("word/document.xml")
        .map_err(|_| "DOCX 缺少 word/document.xml".to_string())?;
    let mut xml_content = String::new();
    doc_xml
        .read_to_string(&mut xml_content)
        .map_err(|e| format!("read document.xml: {e}"))?;

    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(&xml_content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut result = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name: String = reader
                    .decoder()
                    .decode(e.name().as_ref())
                    .unwrap_or(Cow::Borrowed(""))
                    .into_owned();
                if name.ends_with(":t") || name == "w:t" || name == "t" {
                    in_text = true;
                }
            }
            Ok(Event::End(e)) => {
                let name: String = reader
                    .decoder()
                    .decode(e.name().as_ref())
                    .unwrap_or(Cow::Borrowed(""))
                    .into_owned();
                if name.ends_with(":t") || name == "w:t" || name == "t" {
                    in_text = false;
                    result.push(' ');
                }
                if name.ends_with(":p") || name == "w:p" || name == "p" {
                    result.push_str("\n\n");
                }
            }
            Ok(Event::Text(t)) if in_text => {
                let decoded = t.unescape().unwrap_or_default();
                result.push_str(&decoded);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("DOCX XML: {e}")),
            _ => {}
        }
        buf.clear();
    }

    let meaningful = result.chars().filter(|c| !c.is_whitespace()).count();
    if meaningful < 10 {
        return Err("DOCX 未提取到有效文本".into());
    }
    Ok(result)
}

/// Excel 序列号 → 可读日期时间（避免 DateTime 单元格输出 `45123.0`）。
/// 纪元 1899-12-30（含 1900 伪闰日偏移，对 1900-03 后日期正确）。
fn format_excel_serial(serial: f64) -> String {
    if !serial.is_finite() {
        return String::new();
    }
    let days = serial.floor() as i64;
    let frac = serial - serial.floor();
    let total_secs = (frac * 86_400.0).round() as i64;

    let date_str = if days == 0 {
        String::new()
    } else {
        chrono::NaiveDate::from_ymd_opt(1899, 12, 30)
            .and_then(|epoch| epoch.checked_add_signed(chrono::Duration::days(days)))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| format!("serial:{serial}"))
    };

    let time_str = {
        let s = total_secs.rem_euclid(86_400) as u32;
        format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    };

    if date_str.is_empty() {
        time_str
    } else if frac.abs() < 1e-9 || total_secs == 0 {
        date_str
    } else {
        format!("{date_str} {time_str}")
    }
}

fn extract_spreadsheet(bytes: &[u8]) -> Result<String, String> {
    use calamine::{DataType, Reader, open_workbook_auto_from_rs};
    let cursor = std::io::Cursor::new(bytes);
    let mut workbook =
        open_workbook_auto_from_rs(cursor).map_err(|e| format!("Excel 打开失败: {e}"))?;
    let mut out = String::new();
    for sheet_name in workbook.sheet_names().to_owned() {
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            out.push_str(&format!("# Sheet: {}\n", sheet_name));
            for row in range.rows() {
                let cells = row
                    .iter()
                    .map(|c| match c {
                        DataType::Empty => "".to_string(),
                        DataType::String(s) => s.to_string(),
                        DataType::Float(f) => format!("{}", f),
                        DataType::Int(i) => i.to_string(),
                        DataType::Bool(b) => b.to_string(),
                        // calamine 0.23: DateTime 是 Excel 序列号 f64，直接 format 会输出 45123.0
                        DataType::DateTime(serial) => format_excel_serial(*serial),
                        DataType::DateTimeIso(s) => s.clone(),
                        DataType::DurationIso(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\t");
                out.push_str(&cells);
                out.push('\n');
            }
            out.push('\n');
        }
    }
    let meaningful = out.chars().filter(|c| !c.is_whitespace()).count();
    if meaningful < 10 {
        return Err("Excel 未提取到有效文本".into());
    }
    Ok(out)
}

pub fn chunk_text(text: &str, chunk_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + chunk_chars).min(chars.len());
        out.push(chars[i..end].iter().collect());
        if end >= chars.len() {
            break;
        }
        // 轻微重叠，避免句中切断丢上下文
        i = end.saturating_sub(200).max(i + 1);
    }
    out
}

/// ATX 标题：`#`–`####`（含可选闭合 #）。返回 (level, title)。
fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_end();
    if !t.starts_with('#') {
        return None;
    }
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 4 {
        return None;
    }
    let rest = t[level..].trim();
    if rest.is_empty() {
        return None;
    }
    // 避免把代码块里的 # 当标题：行内若以 ``` 开头则跳过由调用方处理
    let title = rest.trim_end_matches('#').trim().to_string();
    if title.is_empty() {
        return None;
    }
    Some((level, title))
}

/// 按 Markdown 标题切段，并维护标题路径面包屑。
/// 每段含自身标题行 + 正文；无标题前言 path 为空。
fn split_markdown_sections(text: &str) -> Vec<(Vec<String>, String)> {
    let mut sections: Vec<(Vec<String>, String)> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;

    let path_of = |stack: &[(usize, String)]| -> Vec<String> {
        stack.iter().map(|(_, t)| t.clone()).collect()
    };

    let flush = |path: &Vec<String>, buf: &mut String, sections: &mut Vec<(Vec<String>, String)>| {
        if !buf.trim().is_empty() {
            sections.push((path.clone(), std::mem::take(buf)));
        } else {
            buf.clear();
        }
    };

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            current.push_str(line);
            current.push('\n');
            continue;
        }
        if !in_fence {
            if let Some((level, title)) = parse_atx_heading(line) {
                let path = path_of(&stack);
                flush(&path, &mut current, &mut sections);
                while let Some(&(l, _)) = stack.last() {
                    if l >= level {
                        stack.pop();
                    } else {
                        break;
                    }
                }
                stack.push((level, title));
                current.push_str(line);
                current.push('\n');
                continue;
            }
        }
        current.push_str(line);
        current.push('\n');
    }
    let path = path_of(&stack);
    flush(&path, &mut current, &mut sections);
    sections
}

fn is_table_row(line: &str) -> bool {
    line.contains('\t') || (line.contains('|') && line.matches('|').count() >= 2)
}

/// 超长表格段：按行边界切，每块重复表头，避免字符切撕行。
fn chunk_preserving_rows(body: &str, chunk_chars: usize) -> Vec<String> {
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let table_rows = lines.iter().filter(|l| is_table_row(l)).count();
    // 表格占比不足 → 退回字符切
    if table_rows * 2 < lines.len() {
        return chunk_text(body, chunk_chars);
    }
    // 表头 = 首个表格行（或首行）
    let header = lines
        .iter()
        .find(|l| is_table_row(l))
        .copied()
        .unwrap_or(lines[0]);
    let header_line = format!("{header}\n");
    let header_len = header_line.chars().count();

    let mut out = Vec::new();
    let mut current = String::new();
    for line in lines {
        if std::ptr::eq(line, header) {
            // 表头行只在每块开头写一次
            continue;
        }
        let line_len = line.chars().count() + 1;
        let base = if current.is_empty() {
            header_len
        } else {
            current.chars().count()
        };
        if base + line_len > chunk_chars && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current.push_str(&header_line);
        }
        if current.is_empty() {
            current.push_str(&header_line);
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(body.to_string());
    }
    out
}

/// 结构感知切块：优先整段（标题边界）保留；表格段按行切；其余段内超长再按字符切。
/// 返回 (标题面包屑, 分块正文)。
pub fn chunk_text_structured(text: &str, chunk_chars: usize) -> Vec<(Vec<String>, String)> {
    let sections = split_markdown_sections(text);
    if sections.is_empty() {
        return Vec::new();
    }
    // 无标题长文 → 退回纯字符切块（path 空）
    if sections.len() == 1 && sections[0].0.is_empty() {
        return chunk_text(text, chunk_chars)
            .into_iter()
            .map(|c| (Vec::new(), c))
            .collect();
    }
    let mut out: Vec<(Vec<String>, String)> = Vec::new();
    for (path, body) in sections {
        let n = body.chars().count();
        if n <= chunk_chars {
            out.push((path, body));
        } else {
            for piece in chunk_preserving_rows(&body, chunk_chars) {
                out.push((path.clone(), piece));
            }
        }
    }
    out
}

fn breadcrumb_join(path: &[String]) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!(" · {}", path.join(" › "))
    }
}

fn safe_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload.bin");
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn ns_dir_component(ns: &str) -> String {
    ns.chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect()
}

fn content_hash16(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// 保存二进制 + 写入清单记忆与分块记忆。
///
/// P0-1（2026-10-13）：`embed`/`hnsw` 为 None 时退化为历史行为（不嵌向量，
/// 仅 FTS 可召回）；两者齐备时清单行与每个分块在 remember 返回 id 后
/// 事后嵌（见 `EmbedFn` 文档）。
pub fn ingest_bytes(
    pool: &SqlitePool,
    doc_root: &Path,
    namespace: &str,
    filename: &str,
    kind: &str,
    bytes: &[u8],
    actor: &str,
    embed: Option<EmbedFn<'_>>,
    hnsw: Option<&HnswIndex>,
) -> Result<IngestOutcome, String> {
    if bytes.len() as u64 > MAX_DOC_BYTES {
        return Err(format!(
            "文件过大（上限 {} MiB）",
            MAX_DOC_BYTES / 1024 / 1024
        ));
    }
    let text = extract_text(kind, bytes)?;
    let doc_id = content_hash16(bytes);
    let safe_name = safe_filename(filename);
    let rel_dir = format!("documents/{}/{}", ns_dir_component(namespace), doc_id);
    let abs_dir = doc_root.join(&rel_dir);
    fs::create_dir_all(&abs_dir).map_err(|e| format!("mkdir: {e}"))?;
    let abs_file = abs_dir.join(&safe_name);
    fs::write(&abs_file, bytes).map_err(|e| format!("write file: {e}"))?;
    let raw_ref = format!("{rel_dir}/{safe_name}");

    let chunks = chunk_text_structured(&text, CHUNK_CHARS);
    if chunks.is_empty() {
        return Err("抽取文本为空".into());
    }

    let preview: String = text.chars().take(400).collect();
    let manifest = format!(
        "[文档] {filename}\n类型: {kind}\n大小: {} bytes\n字符: {}\n分块: {}\n路径: {raw_ref}\n---\n{preview}",
        bytes.len(),
        text.chars().count(),
        chunks.len(),
    );
    let tags_manifest = format!(r#"["document","{kind}","dept-share","file:{safe_name}"]"#);
    let man: RememberResult = remember::remember_with_dedup(
        pool,
        &manifest,
        "document",
        7,
        actor,
        namespace,
        &tags_manifest,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(actor),
        Some("document"),
        None,
        Some(&raw_ref),
    )
    .map_err(|e| format!("manifest remember: {e}"))?;

    // P0-1（2026-10-13）：清单行事后嵌——embed/hnsw 缺失或失败 = fail-open 降级
    // （记忆仍写入、无向量），与 remember_with_dedup 创建路径同款失败语义。
    if let (Some(embed), Some(h)) = (embed, hnsw) {
        if let Some(v) = embed(&manifest) {
            remember::persist_and_index(pool, h, &man.id, namespace, &v);
        }
    }

    let chunk_ids = write_chunk_memories(
        pool,
        &man.id,
        &chunks,
        filename,
        kind,
        &safe_name,
        actor,
        namespace,
        &raw_ref,
        embed,
        hnsw,
    )?;

    Ok(IngestOutcome {
        doc_id,
        namespace: namespace.to_string(),
        filename: filename.to_string(),
        kind: kind.to_string(),
        raw_ref,
        chars: text.chars().count(),
        chunk_count: chunk_ids.len(),
        manifest_id: man.id,
        chunk_ids,
    })
}

/// 已抽好文本时入库（MCP / 外部解析器）。仍要求提供旁路文件名；无二进制则 raw_ref=text-only:…
///
/// P0-1（2026-10-13）：`embed`/`hnsw` 语义同 `ingest_bytes`。
pub fn ingest_plain_text(
    pool: &SqlitePool,
    namespace: &str,
    filename: &str,
    text: &str,
    actor: &str,
    embed: Option<EmbedFn<'_>>,
    hnsw: Option<&HnswIndex>,
) -> Result<IngestOutcome, String> {
    let meaningful = text.chars().filter(|c| !c.is_whitespace()).count();
    if meaningful < 10 {
        return Err("文本过短".into());
    }
    let kind = detect_kind(filename, None).unwrap_or("text");
    let doc_id = content_hash16(text.as_bytes());
    let safe_name = safe_filename(filename);
    let raw_ref = format!("text-only/{}/{safe_name}", ns_dir_component(namespace));
    let chunks = chunk_text_structured(text, CHUNK_CHARS);
    let preview: String = text.chars().take(400).collect();
    let manifest = format!(
        "[文档] {filename}\n类型: {kind}\n字符: {}\n分块: {}\n路径: {raw_ref}\n---\n{preview}",
        text.chars().count(),
        chunks.len(),
    );
    let tags_manifest = format!(r#"["document","{kind}","dept-share","file:{safe_name}"]"#);
    let man = remember::remember_with_dedup(
        pool,
        &manifest,
        "document",
        7,
        actor,
        namespace,
        &tags_manifest,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(actor),
        Some("document"),
        None,
        Some(&raw_ref),
    )
    .map_err(|e| format!("manifest remember: {e}"))?;

    // P0-1（2026-10-13）：清单行事后嵌（同 ingest_bytes，fail-open 降级）。
    if let (Some(embed), Some(h)) = (embed, hnsw) {
        if let Some(v) = embed(&manifest) {
            remember::persist_and_index(pool, h, &man.id, namespace, &v);
        }
    }

    let chunk_ids = write_chunk_memories(
        pool,
        &man.id,
        &chunks,
        filename,
        kind,
        &safe_name,
        actor,
        namespace,
        &raw_ref,
        embed,
        hnsw,
    )?;

    Ok(IngestOutcome {
        doc_id,
        namespace: namespace.to_string(),
        filename: filename.to_string(),
        kind: kind.to_string(),
        raw_ref,
        chars: text.chars().count(),
        chunk_count: chunk_ids.len(),
        manifest_id: man.id,
        chunk_ids,
    })
}

/// 分块写入：`[文档块 i/N] 文件名 · 标题路径` 面包屑 + 正文。
///
/// P0-1（2026-10-13）：`embed`/`hnsw` 齐备时每块 remember 返回 id 后事后嵌
/// （body 文本 → 向量 → put_stored_vector + hnsw.add）；缺失 = 历史行为。
fn write_chunk_memories(
    pool: &SqlitePool,
    manifest_id: &str,
    chunks: &[(Vec<String>, String)],
    filename: &str,
    kind: &str,
    safe_name: &str,
    actor: &str,
    namespace: &str,
    raw_ref: &str,
    embed: Option<EmbedFn<'_>>,
    hnsw: Option<&HnswIndex>,
) -> Result<Vec<String>, String> {
    let mut chunk_ids = Vec::new();
    let total = chunks.len();
    for (i, (path, chunk)) in chunks.iter().enumerate() {
        let crumb = breadcrumb_join(path);
        let body = format!("[文档块 {}/{}] {filename}{crumb}\n\n{chunk}", i + 1, total);
        let tags = format!(r#"["document","{kind}","dept-share","chunk","file:{safe_name}"]"#);
        let r = remember::remember_with_dedup(
            pool,
            &body,
            "document",
            6,
            actor,
            namespace,
            &tags,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(actor),
            Some("document"),
            Some(manifest_id),
            Some(raw_ref),
        )
        .map_err(|e| format!("chunk {} remember: {e}", i + 1))?;
        chunk_ids.push(r.id.clone());
        // P0-1（2026-10-13）：分块事后嵌——每块独立嵌（块才是语义召回的主体），
        // 单块失败不影响其它块与记忆写入（fail-open）。
        if let (Some(embed), Some(h)) = (embed, hnsw) {
            if let Some(v) = embed(&body) {
                remember::persist_and_index(pool, h, &r.id, namespace, &v);
            }
        }
    }
    Ok(chunk_ids)
}

pub fn resolve_doc_root(db_path: &str) -> PathBuf {
    if let Ok(p) = std::env::var("MEMORIA_DOC_DIR") {
        return PathBuf::from(p);
    }
    Path::new(db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_overlaps_and_covers() {
        let s: String = (0..8000).map(|_| '字').collect();
        let chunks = chunk_text(&s, 3500);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].chars().count() <= 3500);
    }

    #[test]
    fn detect_kinds() {
        assert_eq!(detect_kind("a.PDF", None), Some("pdf"));
        assert_eq!(detect_kind("b.docx", None), Some("docx"));
        assert_eq!(detect_kind("c.xlsx", None), Some("xlsx"));
        assert_eq!(detect_kind("d.xls", None), Some("xls"));
        assert_eq!(detect_kind("e.txt", None), None);
    }

    #[test]
    fn excel_serial_formats_date_not_float() {
        // 2023-07-01 12:00:00 ≈ serial 45108.5（1899-12-30 纪元）
        let s = format_excel_serial(45108.5);
        assert!(!s.contains('.'), "got {s}");
        assert!(s.starts_with("2023-07-01"), "got {s}");
        assert!(s.contains("12:00:00"), "got {s}");
        // 纯日期
        let d = format_excel_serial(45108.0);
        assert_eq!(d, "2023-07-01");
    }

    #[test]
    fn structured_chunks_respect_headings() {
        let md = "# A\nintro\n\n## B\nbody-b\n\n## C\nbody-c\n";
        let chunks = chunk_text_structured(md, 10_000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, vec!["A".to_string()]);
        assert_eq!(chunks[1].0, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(chunks[2].0, vec!["A".to_string(), "C".to_string()]);
        assert!(chunks[1].1.contains("body-b"));
        // 大节再切时保留同一面包屑
        let big = format!("# T\n{}", "x".repeat(5000));
        let parts = chunk_text_structured(&big, 2000);
        assert!(parts.len() >= 2);
        assert!(parts.iter().all(|(p, _)| p == &vec!["T".to_string()]));
    }

    #[test]
    fn structured_chunks_ignore_code_fence_headings() {
        let md = "# R\n```\n# not a heading\n```\nreal\n";
        let chunks = chunk_text_structured(md, 10_000);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].1.contains("# not a heading"));
    }

    #[test]
    fn table_chunks_keep_header_and_rows_intact() {
        // 模拟 Excel 抽出的 TSV：# Sheet 标题 + 表头 + 数据行
        let mut md = String::from("# Sheet: 产量\n列A\t列B\t列C\n");
        for i in 0..80 {
            md.push_str(&format!("行{i}\t值{i}\t2023\n"));
        }
        let chunks = chunk_text_structured(&md, 400);
        assert!(chunks.len() >= 2);
        for (path, body) in &chunks {
            assert_eq!(path, &vec!["Sheet: 产量".to_string()]);
            // 每块都带表头
            assert!(body.contains("列A\t列B\t列C"), "missing header in: {}", body);
            // 行不被撕开（无「行N」被截半后接下一逻辑行的混合体——每行完整以 \n 收）
            for line in body.lines() {
                if line.starts_with("行") {
                    let parts: Vec<&str> = line.split('\t').collect();
                    assert_eq!(parts.len(), 3, "torn row: {line}");
                }
            }
        }
    }

    // ── P0-1（2026-10-13）：ingestion 事后嵌回归 ──
    // 背景：ingest 路径曾恒传 hnsw/query_cache=None → 文档记忆永不进 HNSW，
    // 生产库 995 条无向量记忆中 602 条是此路径产物。以下两测锁死修复契约。

    fn test_engine(tag: &str) -> (crate::MemoriaEngine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("memoria_doc_emb_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mem.db");
        let engine = crate::MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
        (engine, dir)
    }

    /// 确定性假嵌入：非零、依文本长度微变（模拟"不同块不同向量"）。
    fn fake_embed(s: &str) -> Option<Vec<f32>> {
        let mut v = vec![0.0f32; crate::vector::DIM];
        v[0] = 1.0;
        v[1] = (s.len() % 11) as f32 * 0.01;
        Some(v)
    }

    #[test]
    fn ingest_persists_vectors_when_embed_provided() {
        let (engine, _dir) = test_engine("ok");
        let out = ingest_plain_text(
            &engine.pool,
            "agent/doctest",
            "t.md",
            "# 标题一\n\n正文内容足够长度通过最小校验。\n\n## 标题二\n\n更多正文内容。",
            "test",
            Some(&fake_embed),
            Some(&engine.hnsw),
        )
        .expect("ingest");

        let expected = 1 + out.chunk_count; // manifest + 每块
        let conn = engine.pool.get().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, expected as i64, "manifest + 每块都应落 memory_vectors");
        assert_eq!(engine.hnsw.len(), expected, "HNSW 应含全部向量");
        // 幂等：manifest id 有持久向量
        assert!(
            crate::vector::persist::get_stored_vector(&engine.pool, &out.manifest_id).is_some(),
            "manifest 向量必须可查"
        );
    }

    #[test]
    fn ingest_degrades_without_embed() {
        // fail-open：embed/hnsw 缺失 → 记忆照常写入、无向量（历史行为保持）
        let (engine, _dir) = test_engine("degrade");
        let out = ingest_plain_text(
            &engine.pool,
            "agent/doctest",
            "t.md",
            "# 标题一\n\n正文内容足够长度通过最小校验。\n\n## 标题二\n\n更多正文内容。",
            "test",
            None,
            None,
        )
        .expect("ingest 不应因缺 embed 而失败");

        let conn = engine.pool.get().unwrap();
        let nv: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nv, 0, "无 embed 时不应有向量");
        let nm: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nm, (1 + out.chunk_count) as i64, "记忆必须全部写入");
    }

    #[test]
    fn ingest_embed_fn_none_also_degrades() {
        // 回调返回 None（嵌入服务故障）= 单条降级，不阻断 ingestion
        let (engine, _dir) = test_engine("none");
        let broken = |_s: &str| -> Option<Vec<f32>> { None };
        let out = ingest_plain_text(
            &engine.pool,
            "agent/doctest",
            "t.md",
            "# 标题一\n\n正文内容足够长度通过最小校验。\n\n## 标题二\n\n更多正文内容。",
            "test",
            Some(&broken),
            Some(&engine.hnsw),
        )
        .expect("ingest 不应因 embed 失败而失败");
        let conn = engine.pool.get().unwrap();
        let nv: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nv, 0);
        assert_eq!(out.chunk_count, 2, "分块数不受 embed 影响");
    }
}
