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

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// 物化子进程默认超时（秒）。OWL tableaux 推理最坏指数复杂度，必须设上限。
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// 允许的 OWL 推理 profile 白名单（#117 命令注入防护）。
/// 未来经 MCP/web 触发时 profile 是租户可控输入，必须严格白名单，
/// 拒绝含空白/;/" 的任意串（会向 batch 注入额外参数/指令）。
const VALID_PROFILES: &[&str] = &["rdfs", "owl-rl", "owl-dl", "owl-full"];

/// 单调递增序号，用于临时文件名的唯一性（#R3-8）。
/// pid + 单调序列（而非依赖 SystemTime 纳秒的粗粒度时钟），并发调用绝不碰撞。
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 临时文件 RAII 守卫（#R3-6/#R4-2）：持有 batch 脚本 + 物化 TTL 路径，drop 时删除，
/// 确保 spawn 失败 / 超时 / 解析失败等**所有**退出路径都清理临时文件。
/// 边已全部解析进内存（all_edges/inferred_edges），out_ttl 无需保留，防磁盘耗尽。
struct TempFileGuard {
    batch: std::path::PathBuf,
    out: std::path::PathBuf,
}
impl TempFileGuard {
    fn new(batch: std::path::PathBuf, out: std::path::PathBuf) -> Self {
        TempFileGuard { batch, out }
    }
    fn batch_path(&self) -> &std::path::Path {
        &self.batch
    }
}
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.batch);
        let _ = std::fs::remove_file(&self.out);
    }
}

/// 物化结果：从 TTL 解析出的推断边（全部显式边 + 推断边统计）。
#[derive(Debug, Default)]
pub struct MaterializeResult {
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
                .and_then(|v| {
                    // #7（第6轮 bug/low）：解析失败不能静默回退 60s 默认——操作者以为配了短超时、
                    // 实际却让挂死的物化跑满一分钟。打印告警。0 会被接受但无文档且语义怪（首轮
                    // ~50ms 即杀），统一拒绝为 1s 下限。
                    match v.parse::<u64>() {
                        Ok(n) if n > 0 => Some(n),
                        Ok(_) => {
                            eprintln!("WARN: OPEN_ONTOLOGIES_TIMEOUT_SECS={:?} is 0/invalid, clamped to 1s", v);
                            Some(1)
                        }
                        Err(_) => {
                            eprintln!(
                                "WARN: OPEN_ONTOLOGIES_TIMEOUT_SECS={:?} not parseable, falling back to default {}s",
                                v, DEFAULT_TIMEOUT_SECS
                            );
                            None
                        }
                    }
                })
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
    // #R4-4：create_dir_all 失败必须传播（此前 `let _ =` 吞错，data_dir 不可写时
    // 后续 write batch/out 会以更令人困惑的路径错误暴露）。未显式创建会造成写临时
    // 文件失败，且那发生在校验之后、spawn 之前，正是最该早期暴露的位置。
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("create data dir {}: {}", cfg.data_dir.display(), e))?;
    // #5（第5轮 security/medium）：data_dir 来自 OPEN_ONTOLOGIES_DATA env（用户可控），
    // 且被拼进 out_ttl 后经 win_path_quoted 插进 batch 脚本的 `save` 行。与 schema_path/
    // source_ttl 相同，data_dir 若含换行/引号/分号会逃逸双引号注入额外 batch 指令——
    // 必须用同一套规则校验，否则 #115/#117 的注入防线被 data_dir 这道口子绕过。
    // 此前注释"data_dir 由唯一名自生成不含危险字符"是错的：唯一名只是后缀，前缀是用户可控。
    let data_dir_str = cfg.data_dir.to_string_lossy();
    if data_dir_str.contains(['\n', '\r', '"', ';']) {
        return Err("invalid data_dir: control chars / quote / semicolon not allowed".to_string());
    }
    // 每次调用用唯一文件名（pid + 单调序列），避免并发调用（CLI + 定时任务 / 未来
    // MCP/web 触发）互相覆盖 script/output，导致读到他方半写文件（#7/#R3-8）。
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let uniq = format!("{}_{}", std::process::id(), seq);
    let batch_file = cfg.data_dir.join(format!("materialize_{}.batch", uniq));
    let out_ttl = cfg.data_dir.join(format!("materialized_{}.ttl", uniq));
    // RAII：batch 脚本 + 物化 TTL 在任意退出路径（spawn 失败/超时/解析失败…）都被删除。#R3-6/#R4-2
    let temp_guard = TempFileGuard::new(batch_file.clone(), out_ttl.clone());

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
        // #R3-7：schema_path 也做与 source_ttl 相同的危险字符校验（换行/引号/分号），
        // 否则 crafted env 可注入额外 batch 指令。data_dir 的同类校验见上方（#5），
        // 它同样被拼进 batch 脚本的 save 行；bin 仅用于 spawn 不走 batch 脚本注入面。
        if sp.contains(['\n', '\r', '"', ';']) {
            return Err("invalid schema_path: control chars / quote / semicolon not allowed".to_string());
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
    std::fs::write(temp_guard.batch_path(), script)
        .map_err(|e| format!("write batch script: {}", e))?;

    let mut child = Command::new(&cfg.bin)
        .arg("batch")
        .arg(win_path(temp_guard.batch_path()))
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
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                // try_wait 出错（EINTR / Windows handle）：必须 kill + wait + join reader，
                // 否则子进程继续跑、reader 线程阻塞在 read_to_string，泄漏失控进程（#R3-3）。
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("wait: {}", e));
            }
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

    // batch 脚本与 out_ttl 均由 TempFileGuard 在函数返回时自动删除（含成功路径，#R3-6/#R4-2）。
    // 边已全部解析进内存（all_edges/inferred_edges），out_ttl 无需保留，防磁盘耗尽（#5-high）。

    Ok(MaterializeResult {
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
/// - **前缀展开**（#R4-3）：收集 `@prefix p: <base>` 声明，把 `p:local` 展开为完整 IRI；
///   默认前缀 `@prefix : <base>` 用 `:local` 展开。这样 source_ttl 若用前缀名（而非完整
///   `<...>` IRI），`parse_ttl_edges` 仍能解析出 source_edges，`inferred_edges` 的
///   diff 才不会被"前缀名 vs 完整 IRI"的不一致误判（#R4-3/#123）。
/// - 主语块行：`<s> <p> <o> , <o2> ; <p2> <o3> .`（对象用 `,` 分隔，谓词用 `;` 分隔）
/// - **跨行续行**：前一行以 `;`/`,` 结尾时，当前行以 `<` 开头是谓词/对象续行而非新主语，
///   subj/pred 状态跨行保留（用 `prev_sep` 跟踪）。
/// - 类型续行 `a <Type> .`（无主语，跳过）
/// - 忽略 `#` 注释 / 空行
///
/// 返回全部目标关系边（显式 + 推断）。推断边由调用方对源 TTL 与物化 TTL 的
/// 边集做集合差得出（见 `materialize`），避免依赖导出器"推断边排最前"这一不稳稿假设。
fn parse_ttl_edges(ttl: &str) -> Vec<(String, String, String)> {
    let mut all: Vec<(String, String, String)> = Vec::new();
    // #10（第6轮 bug/low）：剥离 UTF-8 BOM（\u{FEFF}）。许多 TTL 导出器会在首行前加 BOM，
    // 使首行 `\u{FEFF}@prefix ...` 既匹配不上 parse_prefix_decl 的 `@prefix`、也不以 `@` 开头
    // 被跳过，导致默认/声明前缀永不注册、后续 `:local` 无法展开、source_edges 少算。
    let ttl = ttl.strip_prefix('\u{FEFF}').unwrap_or(ttl);
    // 前缀表：`p:local` → 完整 IRI。#R4-3 前缀展开。默认前缀 `:` 也在此表。
    let mut prefixes: HashMap<String, String> = HashMap::new();
    let mut subj: Option<String> = None;
    let mut pred: Option<String> = None;
    // 上一行结尾的分隔符：Some(';') = 下行首个 IRI 是新谓词；Some(',') = 下行首个 IRI 是新对象。
    // None = 上一行是完整语句（以 `.` 结尾），下行若以 `<` 开头是新主语块。
    let mut prev_sep: Option<char> = None;

    for raw in ttl.lines() {
        // #7（第5轮 bug/medium）：先剥离行内注释（`#` 到行尾）。Turtle 允许行内注释，
        // 若不剥离，`... <docA> . # see <http://.../supersedes>` 里的 `<http://.../supersedes>` 会被
        // tokenize_ttl 当作真实对象，pred 仍为 supersedes 时产生伪边 (docC, supersedes, supersedes)，
        // 且能逃过"物化−源"集合差成为推断边并写库。
        // 剥离须避开 `<...>` IRI 内与引号字面量内的 `#`（#7 明示）。
        let stripped = strip_inline_comment(raw);
        let line_trimmed = stripped.trim();
        if line_trimmed.is_empty() || line_trimmed.starts_with('#') {
            continue;
        }
        // #R4-3：`@prefix p: <base>` 声明（含 `@prefix : <base>` 默认前缀）。
        // 在跳过所有 `@` 行之前先收集，供后续 `p:local` 展开。
        if let Some(p) = parse_prefix_decl(line_trimmed) {
            prefixes.insert(p.0, p.1);
            // 前缀声明行不参与三元组状态机，且不改变 prev_sep（前缀行必以 `.` 结尾）。
            prev_sep = None;
            continue;
        }
        if line_trimmed.starts_with('@') {
            // 其它 @ 指令（@base 等）本骨架不处理，跳过。
            continue;
        }
        // 剥掉行尾续行标记。
        let line = line_trimmed.trim_end_matches([';', ',', '.']).trim();
        // 提取本行所有 <iri> token 及分隔符（; , 区分谓词/对象续行）
        let tokens = tokenize_ttl(line);

        // #6（第6轮 bug/medium）：`a <Type>` 是 rdf:type 简写，不产出目标关系边。
        // 但只有**纯类型声明行**（`a <Type>` 是整行唯一内容，无后续谓词/对象）才能整行跳过；
        // `a <Type> ; <pred> <obj>`（同一行以 `;` 分隔出关系边）不是纯类型，整行跳过会
        // 静默丢失 `<pred> <obj>` 边——该边从 source_edges 与 all_edges 双双消失，物化 diff
        // 会把既有链接误报为推断或漏写回。故：非纯类型行只跳过前导 `a <Type>` 两个 token。
        // 谓词 `a` 在 tokenize 中作为 token 捕获（`a ` 分支）。
        let type_pred_at_0 = tokens.first().map(|t| t.as_str()) == Some("a");
        let has_type_target = tokens.get(1).is_some();
        let is_pure_type = type_pred_at_0 && has_type_target && tokens.len() == 2;
        if is_pure_type {
            // 纯类型声明行（`a <Type>`）：无目标关系边，跳过内容。
            // 但必须保留行尾续行标记：块中间的 `a <Type> ;` 后可能还有 `<pred> <obj>` 续行，
            // 直接置 None 会把后续续行误判为新主语块，导致真实语义边被静默丢弃（#118）。
            // 本行以 `.` 结尾则重置为 None（下行为新主语块）。
            prev_sep = if line_trimmed.ends_with(';') {
                Some(';')
            } else if line_trimmed.ends_with(',') {
                Some(',')
            } else {
                None
            };
            continue;
        }
        // 非纯类型：跳过前导 `a <Type>` 两个 token（若有），继续处理其余关系边（#6）。
        let mut i = if type_pred_at_0 && has_type_target { 2 } else { 0 };

        // 行首首个 token 若能展开为 IRI，且上一行不是续行 → 新主语块。
        // 同时兼容 `<iri>` 与 `p:local`/`:local`（#R4-3 前缀展开）。
        let first_term = tokens.get(i).and_then(|t| expand_term(t, &prefixes));
        // 仅当上一行不是续行（`;`/`,`）且本行首 token 是 IRI 时，才视为新主语块。
        let new_block = prev_sep.is_none() && first_term.is_some();
        if new_block {
            // 主语（i 可能已跳过前导 `a <Type>`，故用 i+1 而非硬编码 1）
            if let Some(s) = first_term {
                subj = Some(s);
                pred = None;
                i += 1;
            }
        } else if prev_sep == Some(';') && first_term.is_some() {
            // 续行且上一行以 `;` 结尾：行首 IRI 是新谓词（沿用 subj）。
            if let Some(p) = first_term {
                pred = Some(p);
                i += 1;
            }
        } else if first_term.is_none() && prev_sep.is_none() {
            // 行首不是可展开 IRI 且非续行。可能是 blank node 主语（`_:b0 <p> <o>` /
            // `[ ... ] <p> <o>`）或其它非 IRI 主语。
            // #5（第6轮 bug/medium）：blank node 主语不能直接把表示边丢弃——否则
            // `source_edges` 少算这些边，物化后若推理器对 blank node skolemize 成 IRIs，
            // "物化−源"集合差会把显式边误标为推断边并写库（evidence=ontology:materialized），
            // 污染推断记账。故对 `_:label` / `[...]` 主语：保留 subj 为 blank node 占位，
            // 让后续 `<pred> <obj>` 正常产出边（blank node 主语本身不展开，仅作占位）。
            // 仅对确实无法识别的主语（既非 IRI 也非 blank node）才重置，避免 stale 伪边（#R3-2）。
            let first_tok = tokens.first().map(|t| t.as_str()).unwrap_or("");
            let is_bnode = first_tok.starts_with("_:") || first_tok.starts_with('[');
            if is_bnode {
                subj = Some(first_tok.to_string());
                pred = None;
                i = 1;
            } else {
                subj = None;
                pred = None;
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
                _ => {
                    // 尝试把 token 展开为完整 IRI：#R4-3 前缀展开。
                    // 支持 `<iri>` 与 `p:local`（含默认 `:local`）两种形式。
                    if let Some(iri) = expand_term(tok, &prefixes) {
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
                    }
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

/// 剥离 Turtle 行内注释：从行内第一个"裸 `#`"到行尾。
/// 必须避开 `<...>` IRI 内与 `"..."`/`'...'` 引号字面量内的 `#`（#7）——
/// 例如 `http://foo#bar` 的 `#` 是 IRI 的一部分，不是注释起点。
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // 跳过 `<...>` IRI 跨度
            if let Some(end) = line[i + 1..].find('>') {
                i += end + 2;
                continue;
            } else {
                return &line[..i]; // 未闭合 `<`，其后到行尾视为注释
            }
        } else if b == b'"' || b == b'\'' {
            // 跳过引号字面量（简单配对，不处理转义——字面量内 `#` 不是注释）
            let quote = b;
            if let Some(end) = line[i + 1..].find(quote as char) {
                i += end + 2;
                continue;
            } else {
                return &line[..i];
            }
        } else if b == b'#' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// 解析 `@prefix p: <base>` 声明，返回 `(前缀名, 基IRI)`。
/// 前缀名可为空（`@prefix : <base>` 表示默认前缀）。
/// 只接受这种定型形式；`p:` 与 `<base>` 之间可有空白。返回 None 表示不是前缀声明。
fn parse_prefix_decl(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("@prefix")?.trim_start();
    // 前缀名：`p:` 或 `:`（默认前缀），`:` 前可有字母/数字/下划线/连字符/点。
    let colon = rest.find(':')?;
    let name = rest[..colon].trim().to_string();
    let after = rest[colon + 1..].trim_start();
    // `@prefix p: <base> .` —— 末尾通常有 `.`，先剥掉再取 `<>` 内 IRI（#R4-3）。
    let after = after.strip_suffix('.').unwrap_or(after).trim_end();
    let base = after.strip_prefix('<')?.strip_suffix('>')?;
    Some((name, base.to_string()))
}

/// 把 TTL 术语 token 展开为完整 IRI。
/// - `<iri>` → 直接返回内层 IRI。
/// - `p:local`（含默认 `:local`）→ 用前缀表展开；前缀未声明则返回 None（跳过）。
/// - 其它（如 `a`）→ None。
fn expand_term(tok: &str, prefixes: &HashMap<String, String>) -> Option<String> {
    if tok.starts_with('<') && tok.ends_with('>') && tok.len() >= 2 {
        return Some(tok[1..tok.len() - 1].to_string());
    }
    // 前缀名形式：`p:local` 或 `:local`。`a` 谓词（无冒号）不展开。
    if let Some(colon) = tok.find(':') {
        let name = &tok[..colon];
        let local = &tok[colon + 1..];
        if let Some(base) = prefixes.get(name) {
            return Some(format!("{}{}", base, local));
        }
    }
    None
}

/// 把一行 TTL 切成 token：`<iri>` 整体 + `p:local`/`:local` 前缀名 + `;``,` 分隔符。
///
/// 跳过引号字面量跨度（`"foo,bar"` / `"x;y"`）：字面量内的 `,`/`;` 不是 Turtle
/// 分隔符，不得触发状态机（否则会误清 pred / 错连 stale subj，#R3-5）。
/// #R4-3：前缀名 `p:local`（含默认 `:local`）与 `a` 谓词是合法术语，需捕获为 token，
/// 由调用方用 `expand_term` 展开为完整 IRI。
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
        } else if b == b'"' || b == b'\'' {
            // 引号字面量：跳到配对的闭合引号（简单处理，不处理转义 —— 字面量内分隔符直接略过）
            let quote = b;
            if let Some(end) = rest[1..].find(quote as char) {
                rest = &rest[end + 2..];
            } else {
                break;
            }
        } else if b == b';' || b == b',' {
            out.push(rest[..1].to_string());
            rest = &rest[1..];
        } else if b == b'[' {
            // #8（第5轮 bug/medium）：blank node 对象 `[ a <Document> ]`。必须把整个 `[...]`
            // 跨度当作不透明 token 跳过（不产出任何 out token），否则内层 `<Document>` 会被
            // 当作外层谓词的对象，产生伪边 (docA, partOf, Document)。OWL 推理 TTL 常见这种结构。
            // #5（第6轮 bug/medium）：把整个 `[...]` 跨度作为一个**不透明 token** 保留（而非
            // 丢弃），使 `[ ... ] <p> <o>` 这类 blank node 属性列表主语能在解析层被识别为
            // blank node 主语（见 #5 分支），避免 `<p>` 被误读为新主语、真实边丢失。
            if let Some(end) = rest.find(']') {
                out.push(rest[..=end].to_string());
                rest = &rest[end + 1..];
            } else {
                // 未闭合 `[`：丢弃整行剩余部分（视为不完整 blank node）
                break;
            }
        } else if rest.starts_with("_:")
            || is_qname_start(b)
            || b == b':'                    // 默认前缀 `:local`
            || rest.starts_with("a ")        // `a` 谓词（rdf:type 简写）
            || rest.starts_with("a\t")
            || rest.starts_with("a;")
            || rest.starts_with("a,")
            || (b == b'a' && rest.len() == 1)
        {
            // 前缀名 / blank node 标签 / `a` 谓词：捕获到下一个空白或分隔符。
            let next = rest
                .find([' ', '\t', ';', ',', '<', '"', '\'', '>'])
                .unwrap_or(rest.len());
            out.push(rest[..next].to_string());
            rest = &rest[next..];
        } else {
            // 其它（空白、`.` 等）跳过
            let next = rest
                .find(['<', ';', ',', '"', '\'', ' ', '\t'])
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

/// 判断字节是否可作为 QName 前缀名 / 局部名的起始（字母、数字、`_`、`-`）。
fn is_qname_start(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
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
            // #9（第6轮 bug/low）跨租户一致性：entities.id 是全局主键（无命名空间维度），
            // 仅 `DO NOTHING` 会让第一个命名空间"拥有"该 IRI 的实体行；后续其它命名空间引用
            // 同一 IRI 时，entity_edges.namespace 与 entities.namespace 会分叉。改为冲突时
            // 刷新 namespace，使实体行归属最近一次写它的命名空间，抑制跨租户分歧。
            for (eid, ename) in [(s.as_str(), sname.as_str()), (o.as_str(), oname.as_str())] {
                tx.execute(
                    "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)
                     VALUES(?1, ?2, 'concept', ?3, '[]', NULL)
                     ON CONFLICT(id) DO UPDATE SET namespace=excluded.namespace",
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
/// #R4-5：末段为空（如 `http://example.org/` 以 `/` 结尾）时回退完整 IRI，
/// 避免把空字符串当实体名写入（`iri.rsplit().next()` 对尾斜杠返回 `""`）。
fn iri_local_name(iri: &str) -> String {
    let last = iri.rsplit(['/', '#']).next().unwrap_or(iri);
    if last.is_empty() {
        iri.to_string()
    } else {
        last.to_string()
    }
}

/// 受控本体命名空间前缀（#R4-1 命名空间感知）。
/// 只有来自这些命名空间的谓词 IRI 才被识别为受控语义边；其它命名空间的
/// 局部名即使撞上白名单（如 `http://foreign-vendor/onto#references`）也拒绝，
/// 防止无关词汇注入 entity_edges。
/// - `http://memoria.ai/onto/`：本模块 schema_core 的默认本体命名空间
/// - `http://www.w3.org/2002/07/owl#`：OWL 内置（不作为语义边，仅站位）
/// - `http://example.org/`：测试 / 示例命名空间
const CONTROLLED_NS: &[&str] = &[
    "http://memoria.ai/onto/",
    "http://example.org/",
];

/// 把 TTL 关系的完整 IRI 映射到 RELATION_TYPES 短名。
/// 返回 None = 该校验规则不属于受控枚举（跳过写回）。
///
/// 命名空间感知（#R4-1）：谓词 IRI 必须位于 `CONTROLLED_NS` 命名空间内，
/// 且局部名命中显式白名单，才映射成语义边。其它命名空间的局部名撞名亦拒绝，
/// 严格符合"防垃圾边"意图（与文档一致）。
fn map_relation_iri(pred: &str) -> Option<String> {
    // #4（第6轮 security/high）：精确前缀门禁——谓词必须是 `<受控前缀> + 单个局部名`。
    // 此前 `starts_with` + `rsplit` 取末段，`http://example.org/vendor/private/supersedes` 之类
    // 深层 IRI 会被误认为受控关系（只要末段撞白名单），使攻击者/租户可在受控前缀下注入任意
    // 深度的白名单局部名进 entity_edges，违背 #R4-1 防垃圾边意图。
    // 修正：匹配前缀后，剩余部分必须是单个局部名（不含 '/' 或 '#'），否则拒绝。
    let ns = CONTROLLED_NS.iter().find(|ns| pred.starts_with(**ns));
    let short = match ns {
        Some(ns) => {
            let rest = &pred[ns.len()..];
            if rest.is_empty() || rest.contains(['/', '#']) {
                return None;
            }
            rest
        }
        None => return None,
    };
    match short {
        "references" => Some("references".to_string()),
        "supersedes" => Some("supersedes".to_string()),
        "createdBy" | "created_by" => Some("created_by".to_string()),
        "conflictsWith" | "conflicts_with" => Some("conflicts_with".to_string()),
        "dependsOn" | "depends_on" => Some("depends_on".to_string()),
        "partOf" | "part_of" => Some("part_of".to_string()),
        "belongsTo" | "belongs_to" => Some("belongs_to".to_string()),
        _ => None,
    }
}

/// 探测 serve-http 在线通道（占位，本期不接 MCP 客户端）。
/// 返回 (进程是否可 spawn, 健康描述)。
pub fn status(cfg: &OntologyConfig) -> Result<String, String> {
    let start = Instant::now();
    // #R3-9：--help 探测也带超时（沿本模块"子进程必须超时"规矩），
    // 避免挂死的二进制无限阻塞探活。
    let mut child = Command::new(&cfg.bin)
        .arg("--help")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn open-ontologies: {} (未安装？)", e))?;
    use std::io::Read;
    let so = child.stdout.take();
    let se = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut b = String::new();
        if let Some(mut o) = so { let _ = o.read_to_string(&mut b); }
        b
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut b = String::new();
        if let Some(mut e) = se { let _ = e.read_to_string(&mut b); }
        b
    });
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let wait_start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => {
                // kill + wait 让子进程退出；reader 线程因 EOF 自然结束，下方统一 join。
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
        if wait_start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout_txt = stdout_reader.join().unwrap_or_default();
    let stderr_txt = stderr_reader.join().unwrap_or_default();
    let detected = if stdout_txt.contains("serve-http") || stderr_txt.contains("serve-http") {
        "serve-http 可用"
    } else {
        "serve-http 未检测到"
    };
    let ok = matches!(status, Some(st) if st.success());
    let msg = format!(
        "open-ontologies 可执行: {} ({})\nbin: {}\ndata: {}\nschema: {}\ndetected: {}",
        cfg.bin.display(),
        if ok { "OK" } else { "FAIL/超时" },
        cfg.bin.display(),
        cfg.data_dir.display(),
        cfg.schema_path.display(),
        detected,
    );
    Ok(format!("{}\ncheck_ms: {}", msg, start.elapsed().as_millis()))
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

    #[test]
    fn ttl_parse_expands_prefixes() {
        // #R4-3：source_ttl 用前缀名（非完整 IRI）时，parse_ttl_edges 必须展开为完整 IRI，
        // 否则 source_edges 为空、物化后 diff 会误把所有显式边都当推断边。
        let ttl = r#"@prefix : <http://example.org/> .
@prefix ex: <http://memoria.ai/onto/> .
:docC ex:supersedes :docA , :docB ;
    a ex:Document .
:docB ex:supersedes :docA ;
    a ex:Document .
"#;
        let all = parse_ttl_edges(ttl);
        // 3 条显式边（docC supersedes docA, docC supersedes docB, docB supersedes docA）
        assert_eq!(all.len(), 3, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
        assert!(all.contains(&(
            "http://example.org/docB".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
    }

    #[test]
    fn iri_local_name_falls_back_on_empty() {
        // #R4-5：末段为空（尾斜杠 IRI）时回退完整 IRI，不写入空名称。
        assert_eq!(iri_local_name("http://example.org/docA"), "docA");
        assert_eq!(iri_local_name("http://example.org/"), "http://example.org/");
        assert_eq!(iri_local_name("http://example.org#"), "http://example.org#");
    }

    #[test]
    fn ttl_parse_strips_inline_comments() {
        // #7（第5轮）：行内注释 `# ...` 到行尾会被剥离，注释里的 `<IRI>` 不产出伪边；
        // 但 `<...>#...`（IRI 内 #）与引号字面量内的 # 不被当作注释起点。
        // 依赖：谓词是受控命名空间内、对象是 `<...>`，注释在行尾。
        let ttl = r#"@prefix : <http://example.org/> .
:docC <http://example.org/supersedes> :docA . # see <http://example.org/supersedes>
"#;
        let all = parse_ttl_edges(ttl);
        // 只应有 1 条真实边（docC supersedes docA），注释里的 <http://example.org/supersedes>
        // 不得被当作对象产生伪边 (docC, supersedes, supersedes)。
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
    }

    #[test]
    fn strip_inline_comment_ignores_iri_and_quotes() {
        // IRI 内 `#` 是 IRI 一部分，不是注释起点
        assert_eq!(strip_inline_comment("<http://foo#bar> <p> <o> ."), "<http://foo#bar> <p> <o> .");
        // 引号字面量内 `#` 不是注释起点
        assert_eq!(strip_inline_comment("\"a#b\" <p> <o> ."), "\"a#b\" <p> <o> .");
        // 行内裸 `#` 起注释
        assert_eq!(strip_inline_comment("<a> <b> . # comment"), "<a> <b> . ");
        // 行首 `#` 整行注释
        assert_eq!(strip_inline_comment("# full comment"), "");
    }

    #[test]
    fn ttl_parse_skips_blank_node_objects() {
        // #8（第5轮）：blank node 对象 `[ a <Document> ]` 整体跳过，内层 <Document> 不得
        // 成为外层谓词的对象，否则产生伪边 (docA, partOf, Document)。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docA> <http://example.org/partOf> [ a <http://example.org/Document> ] .
"#;
        let all = parse_ttl_edges(ttl);
        // partOf 是受控关系，但对象是 blank node（无实 IRI），不应产出任何边。
        assert_eq!(all.len(), 0, "got {:?}", all);
    }

    #[test]
    fn relation_iri_mapping_exact_prefix() {
        // #4（第6轮 security/high）：前缀匹配必须是"受控前缀+单个局部名"，
        // 深层 IRI（前缀后仍含 / 或 #）即使末段撞白名单也必须拒绝。
        assert_eq!(map_relation_iri("http://example.org/supersedes").as_deref(), Some("supersedes"));
        // 深层路径：前缀后含 '/'，必须拒绝（防注入白名单局部名）
        assert_eq!(map_relation_iri("http://example.org/vendor/private/supersedes").as_deref(), None);
        assert_eq!(map_relation_iri("http://memoria.ai/onto/foo/createdBy").as_deref(), None);
        // 前缀后含 '#' 也拒绝
        assert_eq!(map_relation_iri("http://example.org/sub#supersedes").as_deref(), None);
        // 空局部名拒绝
        assert_eq!(map_relation_iri("http://example.org/").as_deref(), None);
    }

    #[test]
    fn ttl_parse_keeps_blank_node_subject_edges() {
        // #5（第6轮 bug/medium）：blank node 主语 `_:b0 <p> <o>` 的边不能被丢弃，
        // 否则 source_edges 少算，物化 diff 会把显式边误标为推断边。
        let ttl = r#"@prefix : <http://example.org/> .
_:b0 <http://example.org/supersedes> :docA .
"#;
        let all = parse_ttl_edges(ttl);
        // blank node 主语不展开，但边应保留（subj=_:b0 占位）
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "_:b0".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
    }

    #[test]
    fn ttl_parse_mixed_type_and_relation_line() {
        // #6（第6轮 bug/medium）：`a <Type> ; <pred> <obj>` 不是纯类型行，整行跳过会丢边。
        // 应只跳过前导 `a <Type>`，仍产出后续 `<pred> <obj>` 边。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docC> a <http://example.org/Document> ; <http://example.org/supersedes> <http://example.org/docA> .
"#;
        let all = parse_ttl_edges(ttl);
        // 应产出 1 条边（docC supersedes docA）；纯类型 `a <Type>` 不产出边
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
    }

    #[test]
    fn ttl_parse_strips_bom() {
        // #10（第6轮 bug/low）：首行带 UTF-8 BOM 时前缀仍能注册、默认前缀展开仍工作。
        let ttl = "\u{FEFF}@prefix : <http://example.org/> .\n:docC <http://example.org/supersedes> :docA .\n";
        let all = parse_ttl_edges(ttl);
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string()
        )));
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
                "materialize OK\nduration_ms: {}\nprofile: {}\ntriples: {} -> {} (inferred {})\ninferred_edges: {}",
                res.duration_ms,
                res.profile,
                res.triples_before,
                res.triples_after,
                res.triples_after.saturating_sub(res.triples_before),
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
    // 短暂等待，确认进程未立即崩溃（端口已绑定即成功）。
    // #9（第5轮 bug/low）：stdout/stderr 若在等待期间持续被写且不 drain，超过 OS 管道缓冲
    // （~64KB）后子进程会阻塞在 write 上、永远无法退出，使"存活>800ms"成为不可靠的信号。
    // 故用 reader 线程并发 drain，与 materialize 的既有模式一致。
    use std::io::Read;
    let so = child.stdout.take();
    let se = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut b = String::new();
        if let Some(mut o) = so { let _ = o.read_to_string(&mut b); }
        b
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut b = String::new();
        if let Some(mut e) = se { let _ = e.read_to_string(&mut b); }
        b
    });
    std::thread::sleep(Duration::from_millis(800));
    // #9：try_wait 出错时必须 kill + wait + join reader，避免遗留失控进程与阻塞线程
    // （与 materialize/status 的 kill+wait 模式一致）。
    let status = match child.try_wait() {
        Ok(st) => st,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!("wait: {}", e));
        }
    };
    if let Some(st) = status {
        // #8（第6轮 maintainability/low）：退出路径也要 join 两个 reader（此前只 join
        // stderr_reader，stdout_reader 的 JoinHandle 被 drop 不 join——若子进程延迟关 stdout，
        // 该 reader 线程会长期阻塞在 read_to_string）。与成功路径 / materialize/status 的
        // kill+wait+join 纪律一致。
        let _ = stdout_reader.join();
        let err = stderr_reader.join().unwrap_or_default();
        return Err(format!(
            "serve-http exited immediately (code {}): {}",
            st, err
        ));
    }
    // 进程在 800ms 内未退出 → 视为能正常启动（占位验证）。随后 kill 并 wait 收割，
    // 避免 Unix 下遗留僵尸进程（#121，与 materialize 超时路径的 kill+wait 一致）。
    let _ = child.kill();
    let _ = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    Ok(format!(
        "serve-http 在线通道占位 OK\nport: {} (进程存活>800ms, 未实测端口监听)\nstorage: persistent\nverification_ms: {}\n(注: serve-http 为 MCP Streamable HTTP, 健康探测需 MCP 握手, 本期未接客户端; '端口已绑定'未实测, 仅验证进程可启动)",
        port,
        start.elapsed().as_millis()
    ))
}