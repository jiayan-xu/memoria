//! Open Ontologies 语义层 — 形态 B 微服务骨架（离线物化 + 写回）
//!
//! 定位：memoria 的通用语义增强层（增强而非替换）。复用 `Open Ontologies`
//! 单二进制 MCP server（`open-ontologies serve` 的 `batch` 模式）做离线物化：
//!   load schema+data → reason(OWL) → save 导出含推断边的 TTL → 解析推断边 → 写回 entity_edges。
//!
//! 设计约束（报告 §8，P0 规矩）：
//! - 离线批处理物化，写回 memoria，**绝不动热路径（:9003 在线查询）**。
//! - 子进程必须超时 + 降级（沿用"定时脚本必须带超时"规矩）。
//! - 只定义少量核心通用类型，业务域类型留给租户扩展，不进系统层。
//!
//! 配置（env，复用 main.rs 模式）：
//!   OPEN_ONTOLOGIES_BIN   open-ontologies 可执行文件路径（默认 `open-ontologies`）
//!   OPEN_ONTOLOGIES_DATA  数据目录（默认 `data/ontology`）
//!   OPEN_ONTOLOGIES_SCHEMA 通用本体 schema 文件（默认 `data/ontology/schema.ttl`）
//!   OPEN_ONTOLOGIES_TIMEOUT_SECS 物化子进程超时（默认 60）
//!
//! 2026-08-11 P0 骨架。关系类型映射与 RELATION_TYPES（tools/graph.rs）保持一致。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// 物化子进程默认超时（秒）。OWL tableaux 推理最坏指数复杂度，必须设上限。
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// 允许的 OWL 推理 profile 白名单（#117 命令注入防护）。
/// 未来经 MCP/web 触发时 profile 是租户可控输入，必须严格白名单，
/// 拒绝含空白/;/" 的任意串（会向 batch 注入额外参数/指令）。
const VALID_PROFILES: &[&str] = &["rdfs", "owl-rl", "owl-dl", "owl-full"];

/// 物化结果：导出文件路径 + 从 TTL 解析出的推断边。
#[derive(Debug, Default)]
pub struct MaterializeResult {
    pub output_path: String,
    /// (source, relation, target) 三元组（仅推断边）
    pub inferred_edges: Vec<(String, String, String)>,
    /// (source, relation, target) 三元组（全部显式边，含原数据）
    pub all_edges: Vec<(String, String, String)>,
    pub triples_before: u64,
    pub triples_after: u64,
    pub profile: String,
    pub duration_ms: u64,
}

/// 配置读取（env 驱动，全部有默认值）。
#[derive(Debug, Clone)]
pub struct OntologyConfig {
    pub bin: PathBuf,
    pub data_dir: PathBuf,
    pub schema_path: PathBuf,
    pub timeout_secs: u64,
}

impl OntologyConfig {
    pub fn from_env() -> Self {
        let data_dir = std::env::var("OPEN_ONTOLOGIES_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data/ontology"));
        Self {
            bin: std::env::var("OPEN_ONTOLOGIES_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("open-ontologies")),
            data_dir: data_dir.clone(),
            schema_path: std::env::var("OPEN_ONTOLOGIES_SCHEMA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| data_dir.join("schema.ttl")),
            timeout_secs: std::env::var("OPEN_ONTOLOGIES_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
        }
    }
}

/// 运行一次离线物化：spawn `open-ontologies batch`，执行 load→reason→save。
///
/// 关键点（P0 验证实锤）：
/// - 必须在**同一进程**内 load→reason→save（单命令 CLI 每次新进程，内存图即丢）。
/// - 路径必须用 Windows 正斜杠（`C:/...`），Git Bash `/c/...` 与反斜杠都不被识别。
/// - `--data-dir` 指向物化工作目录。
///
/// 返回 None 表示降级（子进程不可用/超时），调用方决定是否写回。
pub fn materialize(
    cfg: &OntologyConfig,
    source_ttl: &str,
    profile: &str,
) -> Result<MaterializeResult, String> {
    let start = Instant::now();
    let _ = std::fs::create_dir_all(&cfg.data_dir);
    // 每次调用用唯一文件名（pid + 纳秒时间戳），避免并发调用（CLI + 定时任务 / 未来
    // MCP/web 触发）互相覆盖 script/output，导致读到他方半写文件（#7）。
    let uniq = format!("{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let batch_file = cfg.data_dir.join(format!("materialize_{}.batch", uniq));
    let out_ttl = cfg.data_dir.join(format!("materialized_{}.ttl", uniq));

    // 安全校验（#115/#117 命令注入）：profile 白名单 + source 拒绝危险字符。
    // 换行会注入额外的 load/reason/save 指令；profile 含空白/;/" 会向 batch 注入额外参数。
    // 未来经 MCP/web 触发时 profile/source 是租户可控输入，必须严格白名单。
    if !VALID_PROFILES.contains(&profile) {
        return Err(format!(
            "invalid profile: {:?} (allowed: {})",
            profile,
            VALID_PROFILES.join(", ")
        ));
    }
    if source_ttl.contains(['\n', '\r', '"', ';']) {
    return Err("invalid source_ttl: control chars / quote / semicolon not allowed".to_string());
}
    // 物化前解析源 TTL 的显式目标关系边集，供物化后 diff 出真正的新增推断边（#113）。
    let source_edges: std::collections::HashSet<(String, String, String)> = {
        let src = std::fs::read_to_string(source_ttl)
            .map_err(|e| format!("read source ttl: {}", e))?;
        parse_ttl_edges(&src).into_iter().collect()
    };

    // 剧本：load schema（含 OWL 传递/对称属性声明）→ load 数据 → reason → save。
    // OWL 推理需要本体声明（TransitiveProperty 等）在场，否则 supersedes 等只是普通属性，
    // 不会产生传递闭包推断（P0 验证实锤：schema 未 load 时 inferred=0）。
    let mut script = String::new();
    if cfg.schema_path.exists() {
        let sp = cfg.schema_path.to_string_lossy();
        if sp.contains('"') {
            return Err("invalid schema_path: double quote not allowed".to_string());
        }
        script.push_str(&format!("load {}\n", win_path_quoted(&cfg.schema_path)));
    } else {
        // 低危：#8 schema 缺失静默——显式告警，避免误以为推理已正确运行。
        eprintln!(
            "WARN: ontology schema not found at {} — OWL inference will produce 0 inferred edges",
            cfg.schema_path.display()
        );
    }
    script.push_str(&format!(
        "load {}\nreason --profile {}\nsave {}\n",
        win_path_quoted(std::path::Path::new(source_ttl)),
        profile,
        win_path_quoted(&out_ttl)
    ));
    std::fs::write(&batch_file, script)
        .map_err(|e| format!("write batch script: {}", e))?;

    let mut child = Command::new(&cfg.bin)
        .arg("batch")
        .arg(win_path(&batch_file))
        .arg("--data-dir")
        .arg(win_path(&cfg.data_dir))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn open-ontologies: {}", e))?;

    // 并发 drain stdout/stderr：子进程在等待期间持续写管道，若不在运行中读取，
    // 超过 OS 管道缓冲（~64KB）后子进程会阻塞在 write 上、永远无法退出。
    // 用 reader 线程立即读取，避免"健康运行被超时误杀"。
    use std::io::Read;
    let so = child.stdout.take();
    let se = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut o) = so {
            let _ = o.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut e) = se {
            let _ = e.read_to_string(&mut buf);
        }
        buf
    });

    // 超时护栏：推理最坏指数复杂度，防止卡死。
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let wait_start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| format!("wait: {}", e))? {
            break st;
        }
        if wait_start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait(); // 收割子进程，避免僵尸（Unix）
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "ontology materialize timed out after {}s (killed)",
                cfg.timeout_secs
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // 子进程已退出：join reader 线程，拿到完整输出。
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "open-ontologies batch failed: {} (stderr: {})",
            status, stderr
        ));
    }

    // 解析 batch JSON 输出，提取 load/reason 的 triple 计数
    let mut triples_before = 0u64;
    let mut triples_after = 0u64;
    let mut used_profile = profile.to_string();
    for line in stdout.lines() {
        if let Ok(v) = parse_json_line(line) {
            if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                if let Some(res) = v.get("result") {
                    if cmd == "load" {
                        triples_before += res
                            .get("triples_loaded")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                    } else if cmd == "reason" {
                        triples_after = res.get("final_triples").and_then(|n| n.as_u64()).unwrap_or(0);
                        if let Some(p) = res.get("profile_used").and_then(|p| p.as_str()) {
                            used_profile = p.to_string();
                        }
                    }
                }
            }
        }
    }

    // 解析导出 TTL 提取全部目标关系边（显式 + 推断，顺序无关）。
    let ttl = std::fs::read_to_string(&out_ttl)
        .map_err(|e| format!("read materialized ttl: {}", e))?;
    let all_edges = parse_ttl_edges(&ttl);
    // 推断边 = 物化后边集 ∖ 物化前显式边集（集合差，顺序无关）。
    // 不用 reason 报告的 inferred_count，也不假设"推断边排最前"——那些跨版本都不可靠（#113）。
    let materialized_set: std::collections::HashSet<(String, String, String)> =
        all_edges.iter().cloned().collect();
    let inferred_edges: Vec<(String, String, String)> = materialized_set
        .difference(&source_edges)
        .cloned()
        .collect();

    // 清理临时 batch 脚本（#119）：避免反复物化累积临时文件致磁盘耗尽。
    // out_ttl 视为产物保留，供调用方消费后自行清理。
    let _ = std::fs::remove_file(&batch_file);

    Ok(MaterializeResult {
        output_path: out_ttl.to_string_lossy().to_string(),
        inferred_edges,
        all_edges,
        triples_before,
        triples_after,
        profile: used_profile,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

/// 解析 batch 输出的单行 JSON（容错：跳过错行）。
fn parse_json_line(line: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(line)
}

/// 把路径规范为 open-ontologies 可识别的形式。
///
/// 仅 Windows（或含盘符前缀的 Windows 风格路径）时把 `\` 替换为 `/`；
/// Unix 上合法文件名自带的反斜杠不改写（#122，避免跨平台路径损坏）。
fn win_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) || s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.as_bytes().get(1) == Some(&b':') {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// 同 `win_path`，但始终用双引号包裹（batch 脚本解析需要，#117）。
/// 路径含 `"` 时无法安全转义，直接拒绝（由调用方拦截）。
fn win_path_quoted(p: &std::path::Path) -> String {
    let s = win_path(p);
    format!("\"{}\"", s)
}

/// 轻量 TTL 三元组解析器（目标谓词扫描版）。
///
/// 只解析本骨架需要的子集：提取「主语 → 目标关系谓词 → 对象」三元组。
/// 目标谓词 = 能映射到受控枚举（RELATION_TYPES）的关系 IRI；`rdf:type`/`owl:*`
/// 等声明谓词不产出语义边（由 map_relation_iri 返回 None 自然剔除）。
///
/// 处理 open-ontologies 导出的 Turtle 子集：
/// - 主语块行：`<s> <p> <o> , <o2> ; <p2> <o3> .`（对象用 `,` 分隔，谓词用 `;` 分隔）
/// - **跨行续行**：前一行以 `;`/`,` 结尾时，当前行以 `<` 开头是谓词/对象续行而非新主语，
///   subj/pred 状态跨行保留（用 `prev_cont` 跟踪）。
/// - 类型续行 `a <Type> .`（无主语，跳过）
/// - 忽略 `@prefix` / `#` 注释 / 空行
///
/// 返回全部目标关系边（显式 + 推断）。推断边由调用方对源 TTL 与物化 TTL 的
/// 边集做集合差得出（见 `materialize`），避免依赖导出器"推断边排最前"这一不稳稿假设。
fn parse_ttl_edges(ttl: &str) -> Vec<(String, String, String)> {
    let mut all: Vec<(String, String, String)> = Vec::new();
    let mut subj: Option<String> = None;
    let mut pred: Option<String> = None;
    // 上一行结尾的分隔符：Some(';') = 下行首个 IRI 是新谓词；Some(',') = 下行首个 IRI 是新对象。
    // None = 上一行是完整语句（以 `.` 结尾），下行若以 `<` 开头是新主语块。
    let mut prev_sep: Option<char> = None;

    for raw in ttl.lines() {
        let line_trimmed = raw.trim();
        if line_trimmed.is_empty()
            || line_trimmed.starts_with('#')
            || line_trimmed.starts_with('@')
        {
            continue;
        }
        // 类型续行 `<subject> a <Type>` 或 `a <Type>`：无目标关系边，跳过类型声明。
        // 需 trim 后判断（真实 TTL 用 tab 缩进，`a <Type>` 前有空白）。
        let type_only = line_trimmed
            .trim_start_matches([',', ';'])
            .trim_start()
            .starts_with("a ")
            || line_trimmed.trim_start().starts_with("a <");
        if type_only {
            // 类型声明行（`a <Type>`）无目标关系边，跳过内容。
            // 但必须保留行尾续行标记：块中间的 `a <Type> ;` 后可能还有 `<pred> <obj>` 续行，
            // 直接置 None 会把后续续行误判为新主语块，导致真实语义边被静默丢弃（#118）。
            // 若本行以 `.` 结尾则重置为 None（下行为新主语块）。
            prev_sep = if line_trimmed.ends_with(';') {
                Some(';')
            } else if line_trimmed.ends_with(',') {
                Some(',')
            } else {
                None
            };
            continue;
        }

        // 剥掉行尾续行标记；记录本行结尾分隔符供下轮使用。
        let line = line_trimmed.trim_end_matches([';', ',', '.']);
        let line = line.trim();

        // 提取本行所有 <iri> token 及分隔符（; , 区分谓词/对象续行）
        let tokens = tokenize_ttl(line);

        let mut i = 0;
        // 仅当行首是 `<` 且上一行不是续行 → 新主语块。
        let new_block = line.starts_with('<') && prev_sep.is_none();
        if new_block {
            // 主语
            if let Some(t) = tokens.get(0) {
                if t.starts_with('<') {
                    subj = Some(t[1..t.len() - 1].to_string());
                    pred = None;
                    i = 1;
                }
            }
        } else if line.starts_with('<') && prev_sep == Some(';') {
            // 续行且上一行以 `;` 结尾：行首 IRI 是新谓词（沿用 subj）。
            if let Some(t) = tokens.get(0) {
                if t.starts_with('<') {
                    pred = Some(t[1..t.len() - 1].to_string());
                    i = 1;
                }
            }
        }
        while i < tokens.len() {
            let tok = &tokens[i];
            match tok.as_str() {
                ";" => {
                    // 下一个 IRI 是新谓词
                    pred = None;
                    i += 1;
                }
                "," => {
                    i += 1;
                }
                _ if tok.starts_with('<') => {
                    let iri = tok[1..tok.len() - 1].to_string();
                    if pred.is_none() {
                        // 谓词
                        pred = Some(iri);
                    } else {
                        // 对象
                        if let (Some(s), Some(p)) = (&subj, &pred) {
                            if map_relation_iri(&p).is_some() {
                                all.push((s.clone(), p.clone(), iri));
                            }
                        }
                    }
                    i += 1;
                }
                _ => {
                    // 非 IRI（如 `a` 谓词）跳过
                    i += 1;
                }
            }
        }
        // 记录本行结尾分隔符：`;` 谓词续行 / `,` 对象续行 / `.` 或其它 → None
        prev_sep = if line_trimmed.ends_with(';') {
            Some(';')
        } else if line_trimmed.ends_with(',') {
            Some(',')
        } else {
            None
        };
    }

    all
}

/// 把一行 TTL 切成 token：`<iri>` 整体 + `;``,` 分隔符。
fn tokenize_ttl(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let b = rest.as_bytes()[0];
        if b == b'<' {
            if let Some(end) = rest.find('>') {
                out.push(rest[..=end].to_string());
                rest = &rest[end + 1..];
            } else {
                break;
            }
        } else if b == b';' || b == b',' {
            out.push(rest[..1].to_string());
            rest = &rest[1..];
        } else {
            // 跳过空白及 `a` 等裸 token（逐字符推进到下一个 < 或分隔符）
            let next = rest
                .find(['<', ';', ','])
                .unwrap_or(rest.len());
            if next == 0 {
                rest = &rest[1..];
            } else {
                rest = &rest[next..];
            }
        }
    }
    out
}

/// 物化完成后，把推断边 / 全部边写回 entity_edges（幂等 upsert）。
///
/// 关系类型映射：TTL 的完整 IRI → RELATION_TYPES 短名。未知关系跳过（防垃圾边）。
/// 复用 `tools::graph::is_valid_relation_type` 门禁。
///
/// 外键约束（storage/sqlite.rs:213）：`entity_edges.source/target_entity_id`
/// REFERENCES entities(id)。故写边前须先确保两端实体已存在（幂等 upsert）。
/// 实体 id 取**完整 IRI**（如 `http://memoria.ai/onto/docA`），避免两个不同命名空间
/// 共享局部名（`http://ns1/docA` vs `http://ns2/docA`）时坍缩成同一实体（#6）；
/// name 字段取局部名供展示，entity_type 用合法 CHECK 值 `concept`。
pub fn write_back_edges(
    pool: &rusqlite::Connection,
    namespace: &str,
    edges: &[(String, String, String)],
) -> Result<(usize, usize), String> {
    let mut written = 0usize;
    let mut skipped = 0usize;
    // #120：整个写回用事务包裹——循环中途任一条出错时整体 ROLLBACK，
    // 避免部分写回造成数据不一致；上千条边也只开一次事务，性能更好。
    // `unchecked_transaction` 允许在 &Connection 上使用（调用方保证池连接可用）。
    let tx = pool
        .unchecked_transaction()
        .map_err(|e| format!("begin transaction: {}", e))?;
    let r = (|| -> Result<(), String> {
        for (s, p, o) in edges {
            let rtype = match map_relation_iri(p) {
                Some(r) => r,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            if !crate::tools::graph::is_valid_relation_type(&rtype) {
                skipped += 1;
                continue;
            }
            if s.is_empty() || o.is_empty() {
                skipped += 1;
                continue;
            }
            // 实体 id = 完整 IRI（区分命名空间）；name = 局部名（可读）。
            let sname = iri_local_name(s);
            let oname = iri_local_name(o);
            // 幂等 upsert 两端实体（满足外键约束）——错误必须传播，不静默吞掉（#9），
            // 否则 INSERT 失败会被掩盖，直到边插入时才暴露为令人困惑的 FK 冲突。
            for (eid, ename) in [(s.as_str(), sname.as_str()), (o.as_str(), oname.as_str())] {
                tx.execute(
                    "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)
                     VALUES(?1, ?2, 'concept', ?3, '[]', NULL)
                     ON CONFLICT(id) DO NOTHING",
                    rusqlite::params![eid, namespace, ename],
                )
                .map_err(|e| format!(
                    "upsert entity {}: {}",
                    eid, e
                ))?;
            }
            tx.execute(
                "INSERT INTO entity_edges(namespace, source_entity_id, target_entity_id, relation_type, weight, evidence)
                 VALUES(?1, ?2, ?3, ?4, 1.0, 'ontology:materialized')
                 ON CONFLICT(namespace, source_entity_id, target_entity_id, relation_type)
                 DO UPDATE SET evidence=excluded.evidence",
                rusqlite::params![namespace, s, o, rtype],
            )
            .map_err(|e| format!("insert edge {} {} {}: {}", s, rtype, o, e))?;
            written += 1;
        }
        Ok(())
    })();
    // 提交或回滚；即使业务循环出错也保证事务关闭。
    match r {
        Ok(()) => tx.commit().map_err(|e| format!("commit transaction: {}", e))?,
        Err(e) => {
            let _ = tx.rollback();
            return Err(e);
        }
    }
    Ok((written, skipped))
}

/// 从完整 IRI 提取末段局部名（`http://x/y/docA` → `docA`）。
fn iri_local_name(iri: &str) -> String {
    iri.rsplit(['/', '#'])
        .next()
        .unwrap_or(iri)
        .to_string()
}

/// 把 TTL 关系的完整 IRI/短名映射到 RELATION_TYPES 短名。
/// 返回 None = 该校验规则不属于受控枚举（跳过写回）。
fn map_relation_iri(pred: &str) -> Option<String> {
    let short = pred
        .rsplit(|c| c == '/' || c == '#')
        .next()
        .unwrap_or(pred);
    match short {
        "references" => Some("references".to_string()),
        "supersedes" => Some("supersedes".to_string()),
        "createdBy" | "created_by" => Some("created_by".to_string()),
        "conflictsWith" | "conflicts_with" => Some("conflicts_with".to_string()),
        "dependsOn" | "depends_on" => Some("depends_on".to_string()),
        "partOf" | "part_of" => Some("part_of".to_string()),
        "belongsTo" | "belongs_to" => Some("belongs_to".to_string()),
        _ if crate::tools::graph::is_valid_relation_type(short) => Some(short.to_string()),
        _ => None,
    }
}

/// 探测 serve-http 在线通道（占位，本期不接 MCP 客户端）。
/// 返回 (进程是否可 spawn, 健康描述)。
pub fn status(cfg: &OntologyConfig) -> Result<String, String> {
    let start = Instant::now();
    let out = Command::new(&cfg.bin)
        .arg("--help")
        .output()
        .map_err(|e| format!("spawn open-ontologies: {} (未安装？)", e))?;
    let stderr_txt = String::from_utf8_lossy(&out.stderr);
    let stdout_txt = String::from_utf8_lossy(&out.stdout);
    let detected = if stdout_txt.contains("serve-http") || stderr_txt.contains("serve-http") {
        "serve-http 可用"
    } else {
        "serve-http 未检测到"
    };
    Ok(format!(
        "open-ontologies 可执行: {} ({})\nbin: {}\ndata: {}\nschema: {}\ndetected: {}",
        cfg.bin.display(),
        if out.status.success() { "OK" } else { "FAIL" },
        cfg.bin.display(),
        cfg.data_dir.display(),
        cfg.schema_path.display(),
        detected,
    ))
    .map(|s| {
        // 保留耗时信息
        format!("{}\ncheck_ms: {}", s, start.elapsed().as_millis())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_parse_extracts_edges() {
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docC> <http://example.org/supersedes> <http://example.org/docA> , <http://example.org/docB> ;
    a <http://example.org/Document> .
<http://example.org/docB> <http://example.org/supersedes> <http://example.org/docA> ;
    a <http://example.org/Document> .
<http://example.org/docA> <http://example.org/createdBy> <http://example.org/alice> ;
    a <http://example.org/Document> .
"#;
        let all = parse_ttl_edges(ttl);
        // 4 条显式边（docC supersedes docA, docC supersedes docB, docB supersedes docA, docA createdBy alice）
        assert_eq!(all.len(), 4);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
    }

    #[test]
    fn ttl_parse_handles_predicate_continuation() {
        // #116：一拍子一行（谓词续行）应沿用上一行 subj，不得误判为新主语。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docC> <http://example.org/supersedes> <http://example.org/docB> ;
    <http://example.org/createdBy> <http://example.org/alice> .
"#;
        let all = parse_ttl_edges(ttl);
        assert_eq!(all.len(), 2, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/createdBy".to_string(),
            "http://example.org/alice".to_string()
        )));
    }

    #[test]
    fn relation_iri_mapping() {
        assert_eq!(map_relation_iri("http://example.org/supersedes").as_deref(), Some("supersedes"));
        assert_eq!(map_relation_iri("http://example.org/createdBy").as_deref(), Some("created_by"));
        assert_eq!(map_relation_iri("http://example.org/conflicts_with").as_deref(), Some("conflicts_with"));
        assert_eq!(map_relation_iri("http://example.org/banana").as_deref(), None);
    }

    #[test]
    fn win_path_normalizes() {
        // Windows 风格路径：`\` → `/`
        assert_eq!(win_path(std::path::Path::new("D:\\data\\a.ttl")), "D:/data/a.ttl");
        // 盘符前缀路径也被识别为 Windows 风格
        assert_eq!(win_path(std::path::Path::new("C:/data/a.ttl")), "C:/data/a.ttl");
        // #117：路径始终双引号包裹（供 batch 脚本解析）
        assert_eq!(win_path_quoted(std::path::Path::new("D:\\my data\\a.ttl")), "\"D:/my data/a.ttl\"");
        assert_eq!(win_path_quoted(std::path::Path::new("D:\\data\\a.ttl")), "\"D:/data/a.ttl\"");
    }

    #[test]
    fn profile_whitelist_rejects_unknown() {
        // #117：profile 白名单
        assert!(VALID_PROFILES.contains(&"owl-rl"));
        assert!(!VALID_PROFILES.contains(&"owl-rl --some-flag"));
    }
}

// 供外部（web_api / mcp_server）复用的通用入口
pub use crate::tools::graph::RELATION_TYPES;

/// `memoria-server ontology <materialize|status|serve>` CLI 入口（仿 backup::run_backup_cli）。
///
/// - `materialize <source_ttl> [profile]`：跑一次离线物化（load→reason→save），
///   打印推断边统计。**不写回** memoria 库（避免与运行实例竞态）。
/// - `status`：探活 open-ontologies 二进制 + 报告配置。
/// - `serve [--port N]`：启动 serve-http 在线通道占位（MCP Streamable HTTP）。
///   本期不接 MCP 客户端，仅验证进程可启动 + 端口可绑定。
pub fn run_ontology_cli(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err(
            "usage: memoria-server ontology <materialize|status|serve>\n\
              materialize <source_ttl> [profile=rdfs]\n\
              status\n\
              serve [--port N]"
                .to_string(),
        );
    }
    let cfg = OntologyConfig::from_env();
    match args[0].as_str() {
        "materialize" => {
            let src = args.get(1).ok_or("materialize requires <source_ttl>")?;
            let profile = args.get(2).map(|s| s.as_str()).unwrap_or("rdfs");
            let res = match materialize(&cfg, src, profile) {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            Ok(format!(
                "materialize OK\nduration_ms: {}\nprofile: {}\ntriples: {} -> {} (inferred {})\noutput: {}\ninferred_edges: {}",
                res.duration_ms,
                res.profile,
                res.triples_before,
                res.triples_after,
                res.triples_after.saturating_sub(res.triples_before),
                res.output_path,
                res.inferred_edges.len(),
            ))
        }
        "status" => status(&cfg),
        "serve" => serve_http_placeholder(&cfg, args),
        other => Err(format!(
            "unknown ontology subcommand: {} (expected materialize|status|serve)",
            other
        )),
    }
}

/// serve-http 在线通道占位：spawn open-ontologies serve-http 并验证端口可绑定。
///
/// 局限（报告中如实标注）：serve-http 是 MCP Streamable HTTP 协议，非 REST /health，
/// 健康探测需 MCP 握手（本期未实现）。本命令仅验证进程能起、端口能绑，
/// 打印启动信息后退出（不维持长驻进程，避免与 memoria 主服务端口冲突）。
fn serve_http_placeholder(cfg: &OntologyConfig, args: &[String]) -> Result<String, String> {
    // 解析 --port N
    let port = args
        .windows(2)
        .find(|w| w[0] == "--port")
        .and_then(|w| w[1].parse::<u16>().ok())
        .unwrap_or(18080);
    let start = Instant::now();
    let mut child = Command::new(&cfg.bin)
        .arg("serve-http")
        .arg("--port")
        .arg(port.to_string())
        .arg("--storage-mode")
        .arg("persistent")
        .arg("--pretty")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn open-ontologies serve-http: {}", e))?;
    // 短暂等待，确认进程未立即崩溃（端口已绑定即成功）
    std::thread::sleep(Duration::from_millis(800));
    let status = child.try_wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(st) = status {
        let mut err = String::new();
        use std::io::Read;
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        return Err(format!(
            "serve-http exited immediately (code {}): {}",
            st, err
        ));
    }
    // 进程在 800ms 内未退出 → 视为能正常启动（占位验证）。随后 kill 并 wait 收割，
    // 避免 Unix 下遗留僵尸进程（#121，与 materialize 超时路径的 kill+wait 一致）。
    let _ = child.kill();
    let _ = child.wait();
    Ok(format!(
        "serve-http 在线通道占位 OK\nport: {} (进程存活>800ms, 未实测端口监听)\nstorage: persistent\nverification_ms: {}\n(注: serve-http 为 MCP Streamable HTTP, 健康探测需 MCP 握手, 本期未接客户端; '端口已绑定'未实测, 仅验证进程可启动)",
        port,
        start.elapsed().as_millis()
    ))
}