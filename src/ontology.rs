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
/// 超时硬上界（秒）。#R17（第17轮 bug/medium）：`join_bounded` 里 `Instant::now() + bound` 走
/// std `Add<Duration>`，内部对溢出会 `expect` panic（Windows 100ns tick 表示在 ~2.9e11s 即溢出，
/// Unix i64 秒约 9.2e18s）。一个语法合法但过大的 env 值（如 u64::MAX）会让 materialize/status
/// 直接 panic 而非走"超时降级"路径，违背"必须超时"纪律。from_env 收敛到 1 天（86400s）上界，
/// 超过则告警拉回，杜绝溢出 panic。
const MAX_TIMEOUT_SECS: u64 = 86400;

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
    // #R15（第15轮 security/medium）：source/schema 的 O_EXCL 副本也由 guard 清理，
    // 与 batch/out 同生命周期，避免泄漏临时文件。
    extras: Vec<std::path::PathBuf>,
}
impl TempFileGuard {
    fn new(batch: std::path::PathBuf, out: std::path::PathBuf) -> Self {
        TempFileGuard { batch, out, extras: Vec::new() }
    }
    fn batch_path(&self) -> &std::path::Path {
        &self.batch
    }
    fn add(&mut self, p: std::path::PathBuf) {
        self.extras.push(p);
    }
}
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.batch);
        let _ = std::fs::remove_file(&self.out);
        for p in &self.extras {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 有界 join reader 线程（#4 第7轮 bug/medium）。
///
/// 若子进程（或其继承 stdout/stderr 写端的孙进程）在 kill 后仍持有管道写端，`read_to_string`
/// 不会 EOF，无界 `join()` 会永久阻塞，使"必须超时"的保证失效。有界 join：在 `bound` 内
/// 等到线程结束则返回其值；超时则 detach（线程必然在管道关闭后自然结束），本函数照常返回默认值，
/// 不阻塞调用方。
fn join_bounded<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    bound: Duration,
    default: T,
) -> T {
    let deadline = Instant::now() + bound;
    loop {
        if handle.is_finished() {
            return handle.join().unwrap_or(default);
        }
        if Instant::now() >= deadline {
            // 超时：detach，让线程在管道关闭后自行结束；不阻塞调用方。
            return default;
        }
        std::thread::sleep(Duration::from_millis(10));
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
                        Ok(n) if n > 0 && n <= MAX_TIMEOUT_SECS => Some(n),
                        Ok(n) if n > MAX_TIMEOUT_SECS => {
                            // #R17：超过硬上界拉回，避免 join_bounded 的 Instant 加法溢出 panic。
                            eprintln!(
                                "WARN: OPEN_ONTOLOGIES_TIMEOUT_SECS={:?} exceeds hard max {}s, clamped to {}s",
                                v, MAX_TIMEOUT_SECS, MAX_TIMEOUT_SECS
                            );
                            Some(MAX_TIMEOUT_SECS)
                        }
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
    // #7（第7轮 security/low）：data_dir 校验必须在 create_dir_all 之前——否则恶意
    // OPEN_ONTOLOGIES_DATA 会先创建目录（副作用）再被拒绝。把校验提到最前，任何注入
    // /危险输入在产生任何文件系统足迹前就被拒绝。
    // #5（第5轮 security/medium）：data_dir 来自 OPEN_ONTOLOGIES_DATA env（用户可控），
    // 且被拼进 out_ttl 后经 win_path_quoted 插进 batch 脚本的 `save` 行。与 schema_path/
    // source_ttl 相同，data_dir 若含换行/引号/分号会逃逸双引号注入额外 batch 指令——
    // 必须用同一套规则校验，否则 #115/#117 的注入防线被 data_dir 这道口子绕过。
    // 此前注释"data_dir 由唯一名自生成不含危险字符"是错的：唯一名只是后缀，前缀是用户可控。
    // #R4-4 前缀：**所有**危险输入校验（data_dir/profile/source_ttl）都必须在
    // create_dir_all 之前完成——否则恶意输入会先创建目录（副作用）再被拒绝，
    // 违背"任何注入/危险输入在产生任何文件系统足迹前就被拒绝"的不变式（#5 第8轮 security/low）。
    let data_dir_str = cfg.data_dir.to_string_lossy();
    // #R15（第15轮 security/low）：data_dir 拒绝字符集须与 source_ttl/schema_path 用"同一套规则"
    // ——非 Windows 上额外拒绝 `\`（尾随 `\` 逃逸 batch 双引号注入）。此前 out_ttl/batch 文件名的
    // `.ttl`/`.batch` 后缀恰好掩盖了该逃逸，属脆弱纵深防御；补上 `\` 避免未来改动引入注入面。
    let mut data_dir_forbid: Vec<char> = vec!['\n', '\r', '"', ';'];
    if !cfg!(windows) {
        data_dir_forbid.push('\\');
    }
    if data_dir_str.contains(data_dir_forbid.as_slice()) {
        return Err("invalid data_dir: control chars / quote / semicolon (and backslash on Unix) not allowed".to_string());
    }
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
    // #6（第11轮 security/medium）：source_ttl 拒绝字符集**在 Unix 上**必须含 `\\`。Unix 上
    // `win_path` 保留反斜杠（不替换），若 source_ttl 以 `\` 结尾，`win_path_quoted` 产出
    // `"path\"`——尾随反斜杠会转义 batch 脚本里闭合的双引号（`load "...\"`），让后续字符被
    // 解析为额外 batch 指令或破坏引号闭合，与已防的 `\n`/`"`/`;` 是同一注入面。source_ttl
    // 租户可控，必须拒绝。**Windows 上不能拒绝 `\`**：`win_path` 会把盘符分隔符 `\` 统一转成
    // `/`，正常 `C:\...` 路径必含反斜杠，拒绝会误拦合法路径（#11 实测）。
    // 因此在非 Windows 才把 `\\` 加入拒绝集。
    let mut forbidden: Vec<char> = vec!['\n', '\r', '"', ';'];
    if !cfg!(windows) {
        forbidden.push('\\');
    }
    if source_ttl.contains(forbidden.as_slice()) {
        return Err("invalid source_ttl: control chars / quote / semicolon (and backslash on Unix) not allowed".to_string());
    }
    // #6（第10轮 security/medium）：schema_path 的危险字符校验必须**前移到 create_dir_all
    // 之前**（与 data_dir/source_ttl 同处）。此前它在下方 batch 脚本构建处才做（create_dir_all
    // 之后），违背 #R4-4 不变式——crafted OPEN_ONTOLOGIES_SCHEMA 会先触发 data-dir 创建 + schema
    // 文件读取才被拒绝；且若 schema_path 指向 FIFO/命名管道，上方的 read_to_string 会无限阻塞
    // 在才有机会校验（可用性漏洞）。这里统一前置，任何危险输入在产生文件系统足迹前被拒绝。
    // #R15（第15轮 security/medium）：校验必须**无条件**执行，不能挂在 `if schema_path.exists()`
    // 上——否则攻击者在校验时文件尚不存在、随后（batch 脚本构建前）创建为名称含 `"`/`;`/换行的
    // 文件，win_path_quoted 只做双引号包裹不做自转义，路径被原样拼进 load 行，绕过注入防线。
    // 与 source_ttl/data_dir 的无条件校验一致。
    let sp = cfg.schema_path.to_string_lossy();
    // #6（第11轮 security/medium）：与 source_ttl 同，schema_path **在 Unix 上**拒绝 `\`
    // （可能尾随 `\` 逃逸 batch 引号注入）。Windows 上 `win_path` 会把 `\` 转成 `/`，正常
    // `C:\...` 必含反斜杠，不能拒绝（#11 实测）。schema_path 租户可控（OPEN_ONTOLOGIES_SCHEMA env）。
    let mut forbidden: Vec<char> = vec!['\n', '\r', '"', ';'];
    if !cfg!(windows) {
        forbidden.push('\\');
    }
    if sp.contains(forbidden.as_slice()) {
        return Err("invalid schema_path: control chars / quote / semicolon (and backslash on Unix) not allowed".to_string());
    }
    // #R4-4：create_dir_all 失败必须传播（此前 `let _ =` 吞错，data_dir 不可写时
    // 后续 write batch/out 会以更令人困惑的路径错误暴露）。未显式创建会造成写临时
    // 文件失败，且那发生在校验之后、spawn 之前，正是最该早期暴露的位置。
    // #5（第12轮 security/medium）：source_ttl/schema_path 的读取必须**在 create_dir_all 之前**
    // 完成——否则 crafted FIFO 路径会先创建 data_dir（文件系统足迹）才被拒绝，违背 #R4-4
    // "任何危险输入在产生文件系统足迹前就被拒绝"的不变式。故把 source_edges 解析整体前移。
    // #5（第12轮 security/medium 续）：source_ttl/schema 读取复用 `read_ttl_no_follow`
    // （O_NOFOLLOW 打开 + fstat 校验常规文件 + 持句柄读），关闭 metadata 检查与 read_to_string
    // 按名重开之间的 TOCTOU——与 out_ttl 的防御一致（此前用 `metadata().is_file()` + 按名重开，
    // 检查与读之间路径可被换成 FIFO 永久阻塞，可用性 DoS）。
    // 读取 source/schema 内容为字符串（既用于解析 source_edges，也用于后续写 O_EXCL 副本，
    // 关闭 #261 的 TOCTOU）。
    let source_content = read_ttl_no_follow(std::path::Path::new(source_ttl))
        .map_err(|e| format!("read source ttl: {}", e))?;
    let mut source_edges: std::collections::HashSet<(String, String, String)> =
        parse_ttl_edges(&source_content).into_iter().collect();
    let schema_content: Option<String> = if cfg.schema_path.exists() {
        // #6：schema 的受控边并入 source_edges 一起减。schema_path 的危险字符校验
        // 在下方的 batch 脚本构建处统一做（此处只读不写，早于任何注入面）。
        // #3（第9轮 bug/medium）：schema 读取失败不能再被 `if let Ok` 静默吞掉——若 schema_path
        // 存在但不可读（权限/瞬时 I/O），其显式受控边缺失，物化后集合差会把 schema 边误标为
        // 推断边写回（evidence=ontology:materialized），正是 #6 要防的误分类。错误必须传播，
        // 与模块"错误传播而非静默吞掉"纪律一致。
        // #1（第11轮 bug/high + #5 第12轮）：schema_path 用 read_ttl_no_follow（O_NOFOLLOW+fstat+
        // 持句柄读），拒绝 FIFO/管道且无 TOCTOU。
        let schema_src = read_ttl_no_follow(&cfg.schema_path)
            .map_err(|e| format!("read schema ttl {}: {}", cfg.schema_path.display(), e))?;
        source_edges.extend(parse_ttl_edges(&schema_src));
        Some(schema_src)
    } else {
        None
    };
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("create data dir {}: {}", cfg.data_dir.display(), e))?;
    // data_dir 可能是攻击者预置的符号链接：create_dir_all 会沿链接创建/写入，把本模块的临时
    // 文件（batch 脚本、源/schema 副本、预占 out_ttl）重定向进攻击者选定的目录。O_EXCL 预占 +
    // 随机名只防"覆盖已知文件"，挡不住"写进攻击者目录"。创建后校验最终 data_dir 非链接。
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink_metadata;
        if symlink_metadata(&cfg.data_dir)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "data dir {} is a symlink (possible attack); refusing to write temp artifacts",
                cfg.data_dir.display()
            ));
        }
    }
    // 每次调用用唯一文件名（pid + 单调序列 + 一次性随机数），避免并发调用（CLI + 定时任务 /
    // 未来 MCP/web 触发）互相覆盖 script/output，导致读到他方半写文件（#7/#R3-8）。
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // #R17（第17轮 security/medium）：仅 `pid + 单调序列` 可预测（CWE-377）。O_EXCL 预占只防
    // spawn 时刻路径已被符号链接占据；load+reason 窗口可能很长，能写 data_dir 的本地攻击者可在
    // 预占后 unlink 常规文件并预置符号链接，子进程 save 按路径覆盖写会沿链接写到任意可写文件。
    // 追加 8 字节不可预测随机数（getrandom），使攻击者无法预知路径，配合 O_NOFOLLOW 读兜底。
    // #R18（第18轮 security/high）：getrandom 失败（EINTR/seccomp/旧内核）必须**传播错误**而非
    // `let _ =` 静默丢弃——否则 rand_buf 保持全零，临时名退化回可预测的 `pid_seq_000...`，#R17
    // 的 CWE-377 防御被静默架空（无日志无告警）。fail closed：熵不可得就不继续物化，与模块
    // "错误必须传播"纪律一致。
    let mut rand_buf = [0u8; 8];
    getrandom::getrandom(&mut rand_buf)
        .map_err(|e| format!("getrandom failed (temp name entropy): {}", e))?;
    let uniq = format!(
        "{}_{}_{:016x}",
        std::process::id(),
        seq,
        u64::from_le_bytes(rand_buf)
    );
    let batch_file = cfg.data_dir.join(format!("materialize_{}.batch", uniq));
    let out_ttl = cfg.data_dir.join(format!("materialized_{}.ttl", uniq));
    // RAII：batch 脚本 + 物化 TTL 在任意退出路径（spawn 失败/超时/解析失败…）都被删除。#R3-6/#R4-2
    let mut temp_guard = TempFileGuard::new(batch_file.clone(), out_ttl.clone());

    // #R15（第15轮 security/medium）：batch 脚本若按原始路径 load source/schema，本模块读出后、
    // 子进程 load 前攻击者可把文件换成不同内容/FIFO——推理数据与 source_edges 基线不一致，
    // 集合差把显式边误标为推断写回（静默污染）；换成 FIFO 则子进程阻塞至超时（反复 DoS）。
    // 与 out_ttl 的 O_EXCL/O_NOFOLLOW 防御不对称。把已读入内存的内容写进唯一名 + O_EXCL 的
    // 副本，batch 引用副本，彻底关闭该 TOCTOU。
    let source_copy = cfg.data_dir.join(format!("source_{}.ttl", uniq));
    // #R17（第17轮 other/low）：先注册进 guard 再写入——若 create_new 成功但 write_all 中途失败
    // （磁盘满等）返回 Err，半成品临时文件已在 guard 内、随 drop 清理，不违背"所有退出路径都清理"
    // 不变式（此前 write 成功后才 add，失败路径会泄漏半成品临时文件，反复失败累积）。
    temp_guard.add(source_copy.clone());
    write_ttl_copy(&source_copy, &source_content)?;
    let schema_copy: Option<std::path::PathBuf> = if let Some(sc) = &schema_content {
        let p = cfg.data_dir.join(format!("schema_{}.ttl", uniq));
        temp_guard.add(p.clone());
        write_ttl_copy(&p, sc)?;
        Some(p)
    } else {
        None
    };

    // 剧本：load schema（含 OWL 传递/对称属性声明）→ load 数据 → reason → save。
    // OWL 推理需要本体声明（TransitiveProperty 等）在场，否则 supersedes 等只是普通属性，
    // 不会产生传递闭包推断（P0 验证实锤：schema 未 load 时 inferred=0）。
    let mut script = String::new();
    if let Some(sc) = &schema_copy {
        // #R15：batch 引用 O_EXCL 副本，不再按原始路径重开（关闭 TOCTOU）。
        script.push_str(&format!("load {}\n", win_path_quoted(sc)));
    } else {
        // 低危：#8 schema 缺失静默——显式告警，避免误以为推理已正确运行。
        eprintln!(
            "WARN: ontology schema not found at {} — OWL inference will produce 0 inferred edges",
            cfg.schema_path.display()
        );
    }
    script.push_str(&format!(
        "load {}\nreason --profile {}\nsave {}\n",
        win_path_quoted(&source_copy),
        profile,
        win_path_quoted(&out_ttl)
    ));
    // #5（第7轮 security/medium）：临时文件名 `materialize_{pid}_{seq}.batch` 可预测 + 用普通
    // std::fs::write（跟随符号链接、无 O_EXCL）。能写进 data_dir 的本地攻击者可预置符号链接，
    // 把 batch 脚本写入重定向到任意路径。改用 `create_new`（O_EXCL，独家创建）——若路径已存在
    // （含符号链接）则失败，杜绝"预置路径劫持写入"。out_ttl 由子进程以 save 创建，本模块只读，
    // 目标攻击面已由 batch 写的 O_EXCL 收窄。
    use std::io::Write;
    let mut batch_f = open_exclusive_0600(temp_guard.batch_path())
        .map_err(|e| format!("create batch script (exclusive): {}", e))?;
    batch_f
        .write_all(script.as_bytes())
        .map_err(|e| format!("write batch script: {}", e))?;

    // #2（第9轮 security/high）：**预防**而非**事后检测**符号链接攻击。out_ttl 路径
    // `materialized_{pid}_{seq}.ttl` 可预测，且由子进程的 save 命令创建——若攻击者在 spawn
    // 前预置同路径符号链接，子进程 save 会沿链接把 TTL 写到任意文件（CWE-377）。symlink_metadata
    // 事后检查只在写入后检测，挡不住写入本身。正确做法：spawn 前用 `create_new`（O_EXCL）把
    // out_ttl 预占成一个**常规文件**——若路径已被（符号链接）占据则 create_new 失败，杜绝
    // 预置链接；子进程 save 会以写模式覆盖这个常规文件（open-ontologies save 是覆盖写）。
    // 预占文件随后被 TempFileGuard 在任意退出路径清理。
    {
        let mut f = open_exclusive_0600(&out_ttl)
            .map_err(|e| format!("pre-create out_ttl (exclusive, anti-symlink): {}", e))?;
        // 清空到 0 字节（新建文件本就为空），确保子进程 save 从干净状态开始（open-ontologies 覆盖写）。
        let _ = f.write_all(b"");
    }

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
    // #R18（第18轮 bug/medium）：timeout_secs 是 pub 字段，调用方可直接构造 OntologyConfig 绕过
    // from_env 的 clamp。此处 use 点再 clamp 一次，保证 `Instant::now() + bound` 不因溢出 panic
    // （std::Add<Duration> 溢出会 expect panic），无论配置如何构造 no-panic 不变式都成立。
    let timeout = Duration::from_secs(cfg.timeout_secs.min(MAX_TIMEOUT_SECS));
    let wait_start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                // try_wait 出错（EINTR / Windows handle）：必须 kill + wait + join reader，
                // 否则子进程继续跑、reader 线程阻塞在 read_to_string，泄漏失控进程（#R3-3）。
                // join 用有界版本（#4），防止孙进程持管道导致永久阻塞。
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
                let _ = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
                return Err(format!("wait: {}", e));
            }
        }
        if wait_start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait(); // 收割子进程，避免僵尸（Unix）
            let _ = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
            let _ = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
            return Err(format!(
                "ontology materialize timed out after {}s (killed)",
                cfg.timeout_secs
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // 子进程已退出：join reader 线程，拿到完整输出。
    // #5（第9轮 bug/low + 第10轮 bug/medium）：成功路径既不能无界 join，也不能用太小的有界值。
    // 第9轮曾改无界 join（担心慢管道 drain 时静默丢 JSON 行），但第10轮指出：孙进程如果继承了
    // stdout/stderr 写端，即使直接子进程正常退出，read_to_string 仍不会撞 EOF，无界 join 会永久
    // 阻塞（未来 MCP/web 触发时永久占用 worker 线程）。
    // #R18（第18轮 bug/medium）：此前用 `cfg.timeout_secs.max(1)`（可达 86400s）作 bound——孙进程
    // 持管道时该 join 会阻塞近一天才超时，实际近于无界，违背"子进程必须超时"。直接子进程已退出，
    // 输出只需 drain，用固定小上界（2s，与其它错误路径一致）即可：既覆盖正常输出 flush，又在孙
    // 进程持管道时快速返回（不阻塞调用方、不丢已读部分）。
    let stdout = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
    let stderr = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
    // 若 2s 内未 join 完（孙进程持管道写端），join_bounded 返回空默认，已读缓冲被丢弃，
    // load/reason 的 JSON 行不会被解析、triples_before/after 静默 collapse 成 0。这里告警，
    // 避免"物化成功但计数全 0"的误导性报告（计数仅供监控，边解析直接读 TTL 文件不受影响）。
    if stdout.is_empty() {
        eprintln!("WARN: stdout reader not drained within 2s; triples counts may be 0");
    }
    if !status.success() {
        return Err(format!(
            "open-ontologies batch failed: {} (stderr: {})",
            status, stderr
        ));
    }

    // 解析 batch JSON 输出，提取 load/reason 的 triple 计数
    // #4（第11轮 maintainability/low）：若 open-ontologies 改了输出格式/混入非 JSON 日志行/
    // 省略字段，计数会静默落 0，报告 "triples: 0 -> 0 (inferred 0)" 误导运维与下游自动化。
    // 显式跟踪是否看到 load/reason 结果，未看到则告警，避免"物化成功但计数全 0"的假象。
    let mut triples_before = 0u64;
    let mut triples_after = 0u64;
    let mut used_profile = profile.to_string();
    let mut saw_load_result = false;
    let mut saw_reason_result = false;
    let mut unparsed_lines = 0usize;
    // #7（第12轮 bug/low）：load/reason 结果行存在但**预期数字字段**缺失/改名时，计数会静默
    // 落 0，而 WARN 只在整行缺失时才触发。需在字段缺失时也告警，避免 "triples: N -> 0" 误导。
    let mut load_missing_field = false;
    let mut reason_missing_field = false;
    for line in stdout.lines() {
        match parse_json_line(line) {
            Ok(v) => {
                let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                let Some(res) = v.get("result") else {
                    continue;
                };
                if cmd == "load" {
                    saw_load_result = true;
                    match res.get("triples_loaded").and_then(|n| n.as_u64()) {
                        Some(n) => triples_before += n,
                        None => load_missing_field = true,
                    }
                } else if cmd == "reason" {
                    saw_reason_result = true;
                    match res.get("final_triples").and_then(|n| n.as_u64()) {
                        Some(n) => triples_after = n,
                        None => reason_missing_field = true,
                    }
                    if let Some(p) = res.get("profile_used").and_then(|p| p.as_str()) {
                        used_profile = p.to_string();
                    }
                }
            }
            Err(_) => unparsed_lines += 1,
        }
    }
    if !saw_load_result {
        eprintln!(
            "WARN: no 'load' result in open-ontologies batch output ({} unparsed line(s)); \
             triples count may be inaccurate",
            unparsed_lines
        );
    } else if load_missing_field {
        eprintln!(
            "WARN: 'load' result present but 'triples_loaded' field missing/renamed; \
             triples_before set to 0"
        );
    }
    if !saw_reason_result {
        eprintln!(
            "WARN: no 'reason' result in open-ontologies batch output ({} unparsed line(s)); \
             triples count may be inaccurate",
            unparsed_lines
        );
    } else if reason_missing_field {
        eprintln!(
            "WARN: 'reason' result present but 'final_triples' field missing/renamed; \
             triples_after set to 0"
        );
    }

    // 解析导出 TTL 提取全部目标关系边（显式 + 推断，顺序无关）。
    // #2（第8/9轮 security/high）：out_ttl 路径 `materialized_{pid}_{seq}.ttl` 可预测，攻击者可
    // 预置符号链接让子进程 save 沿链接写任意文件（CWE-377）。第9轮已改为**预防**：spawn 前用
    // create_new(O_EXCL) 把 out_ttl 预占成常规文件（见上方），路径已被非符号链接占据，攻击者
    // 无法再预置链接。
    // #3（第11轮 security/low）：此前 `symlink_metadata` 常规文件检查与 `read_to_string(&out_ttl)`
    // 之间存在 TOCTOU——攻击者可在两者间把 out_ttl 换成符号链接/FIFO，其内容被当 TTL 解析并
    // diff。O_EXCL 预占收窄了竞态但没关死。正确做法：用 `O_NOFOLLOW`（Unix）打开 out_ttl、
    // 在**已打开的句柄**上 fstat 校验常规文件，并通过该句柄读取——符号链接在 open 时即被拒绝，
    // 后续路径替换不再影响（读的是已打开的真实文件），TOCTOU 关闭。Windows 无 O_NOFOLLOW，
    // 用 create_new 预占 + 此处常规检查兜底（Windows 上符号链接创建需管理员权限，风险已收窄）。
    let ttl = read_ttl_no_follow(&out_ttl)?;
    let all_edges = parse_ttl_edges(&ttl);
    // 子进程退出 0 不代表 save 真的产出了图：open-ontologies 输出格式改名/版本漂移/配置变化时
    // save 可能静默 no-op，导致 all_edges 为空而 CLI 仍报 "materialize OK ... inferred_edges: 0"，
    // 运维误读为"无新推断"而非"管线坏了"。load/reason 的 JSON 行告警覆盖不到 save 路径，这里补。
    if all_edges.is_empty() && !source_edges.is_empty() {
        eprintln!(
            "WARN: materialized TTL produced 0 relation edges while source had {} — \
             save may have no-opped; treat inferred_edges=0 as suspect",
            source_edges.len()
        );
    }
    // 推断边 = 物化后边集 ∖ 物化前显式边集（集合差，顺序无关）。
    // 不用 reason 报告的 inferred_count，也不假设"推断边排最前"——那些跨版本都不可靠（#113）。
    // #R17（第17轮 bug/medium）：比较前对集合两边做**轻量 IRI 归一**（尾部 #/ 归一），对齐
    // `http://x/onto/docA#` vs `http://x/onto/docA` 这类派生不一致，避免显式边被误判为推断。
    // 相对 vs 绝对（无 @base）的差异归下方 inferred_ratio 高占比告警兜底。
    let materialized_set: std::collections::HashSet<(String, String, String)> =
        all_edges.iter().map(normalize_edge).collect();
    let source_norm: std::collections::HashSet<(String, String, String)> =
        source_edges.iter().map(normalize_edge).collect();
    let inferred_edges: Vec<(String, String, String)> = materialized_set
        .difference(&source_norm)
        .cloned()
        .collect();
    // #R17：推断占比异常高（≥80% 物化边被判为推断）时告警。通常正常物化只新增少量推断边；
    // 若几乎全部边都是"推断"，强烈提示源/物化 IRI 形式不一致（如相对 vs 绝对）导致集合差误判，
    // 而非真实推理。运维据此排查，避免静默污染语义记账。
    if !materialized_set.is_empty() {
        let inferred_ratio = inferred_edges.len() as f64 / materialized_set.len() as f64;
        if inferred_ratio >= 0.8 {
            eprintln!(
                "WARN: inferred ratio {:.0}% ({}/{}) is abnormally high — possible IRI form \
                 mismatch between source and materialized TTL (e.g. relative vs absolute); \
                 explicit edges may be misclassified as inferred",
                inferred_ratio * 100.0,
                inferred_edges.len(),
                materialized_set.len()
            );
        }
    }

    // batch 脚本与 out_ttl 均由 TempFileGuard 在函数返回时自动删除（含成功路径，#R3-6/#R4-2）。
    // 边已全部解析进内存（all_edges/inferred_edges），out_ttl 无需保留，防磁盘耗尽（#5-high）。
    // all_edges 与 inferred_edges 同出一源（parse_ttl_edges / 集合差），返回前对 all_edges 也归一，
    // 保证两者 IRI 形式一致（尾部 #/ 归一）——否则未来调用方按 struct 文档写回 raw all_edges，
    // 会用 `http://x/docA#` 建实体行，与归一后的 `http://x/docA` 重复，破坏集合差 provenance。
    let all_edges: Vec<(String, String, String)> =
        all_edges.iter().map(normalize_edge).collect();

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

/// 以 `O_NOFOLLOW` 打开并读取一个 TTL 文件，关闭 symlink/类型检查与读取之间的 TOCTOU
/// （#3 第11轮引入，第12轮扩展复用于 source_ttl/schema_path）。
///
/// 用于所有"读取前需确认是常规文件"的 TTL 输入（out_ttl / source_ttl / schema_path）：
/// Unix：open 时 `O_NOFOLLOW` 直接拒绝符号链接 + `O_NONBLOCK` 拒绝 FIFO 阻塞，随后在已打开的
/// 句柄上 fstat 校验常规文件（拒绝 FIFO/设备，防阻塞读），再通过句柄读取——路径替换不影响
/// 已打开的真实文件。
/// #R17（第17轮 security/high）：Unix 分支必须同时加 `O_NONBLOCK`。仅 `O_NOFOLLOW` 时，open
/// 一个无 writer 的 FIFO 会无限阻塞到有 writer 出现（fstat 常规文件校验在 open 返回后才执行，
/// 永远轮不到）；若 out_ttl 在子进程运行期间被换成 FIFO，父进程 open 还会与子进程 save 的写端
/// 互相等待形成死锁。`O_NONBLOCK` 使 open FIFO 立即返回，fstat 确认常规文件后再读取（常规文件
/// 读写不受 O_NONBLOCK 影响），真正关闭 FIFO DoS 面。
/// Windows 无 `O_NOFOLLOW`：符号链接创建需管理员权限（风险已收窄），此处用 `symlink_metadata`
/// 校验兜底。
#[cfg(unix)]
fn read_ttl_no_follow(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("open ttl (O_NOFOLLOW|O_NONBLOCK): {}", e))?;
    let md = f
        .metadata()
        .map_err(|e| format!("fstat ttl: {}", e))?;
    if !md.is_file() {
        return Err(format!(
            "{} is not a regular file (possible symlink/fifo attack)",
            path.display()
        ));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("read ttl {}: {}", path.display(), e))?;
    Ok(s)
}

/// Windows 版本：无 `O_NOFOLLOW`，用 `symlink_metadata`（不跟随链接）确认非符号链接，
/// 再确认常规文件，最后**通过已打开的句柄**读取。create_new 预占已收窄竞态。
/// #R18（第18轮 security/medium）：此前 `symlink_metadata` 检查后 `std::fs::read_to_string(path)`
/// 按名重开——check-read 之间路径可被换成 symlink/junction（目录 junction/reparse point 创建不
/// 需管理员；dev/CI 常提权运行），使 FIFO 阻塞 DoS 与内容替换在 Windows 上仍部分敞开。改为
/// `File::open` 打开一次句柄，在句柄上 fstat 校验 + 通过句柄读：路径在 open 后被替换不影响
/// 已打开的真实文件，与 Unix 的持句柄读一致。
#[cfg(windows)]
fn read_ttl_no_follow(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let sm = std::fs::symlink_metadata(path)
        .map_err(|e| format!("lstat ttl {}: {}", path.display(), e))?;
    if sm.file_type().is_symlink() {
        return Err(format!("{} is a symlink (possible attack)", path.display()));
    }
    if !sm.is_file() {
        return Err(format!(
            "{} is not a regular file (possible fifo/device)",
            path.display()
        ));
    }
    // 打开一次句柄，之后全部经句柄操作（fstat + 读），不按名重开——关闭 check-read TOCTOU。
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("open ttl {}: {}", path.display(), e))?;
    let md = f
        .metadata()
        .map_err(|e| format!("fstat ttl {}: {}", path.display(), e))?;
    if !md.is_file() {
        return Err(format!(
            "{} is not a regular file (possible fifo/device)",
            path.display()
        ));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("read ttl {}: {}", path.display(), e))?;
    Ok(s)
}

/// 把路径规范为 open-ontologies 可识别的形式。
///
/// 仅 Windows（或严格盘符前缀的 Windows 风格路径）时把 `\` 替换为 `/`；
/// Unix 上合法文件名自带的反斜杠不改写（#122，避免跨平台路径损坏）。
/// #9（第7轮 bug/low）：盘符启发式 `X:` 必须后跟 `/` 或 `\` 才判定为盘符——仅 `X:` 两字符
/// 会误匹配 Unix 相对路径 `a:b.ttl`（冒号与反斜杠在 Unix 都是合法文件名字符），导致其反斜杠被改坏。
fn win_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let is_drive = {
        let b = s.as_bytes();
        b.len() >= 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && b.get(2).map(|c| *c == b'/' || *c == b'\\').unwrap_or(false)
    };
    if cfg!(windows) || is_drive {
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

/// 以 `create_new`（O_EXCL）写一份 TTL 副本，供 batch 脚本按副本路径 load（#R15/#261）。
/// 副本用唯一名（pid+seq），攻击者无法预置符号链接劫持；batch 引用副本而非原始路径，
/// 关闭"模块读出后、子进程 load 前文件被换"的 TOCTOU。
fn write_ttl_copy(path: &std::path::Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = open_exclusive_0600(path)
        .map_err(|e| format!("create ttl copy (exclusive): {}", e))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("write ttl copy {}: {}", path.display(), e))
}

/// `create_new`（O_EXCL）打开并独占创建；Unix 上以 0600 权限（避免临时本体数据被同机他用户
/// 读取）。`#[cfg]` 不能夹在链式调用中间，故封成辅助函数，两平台共用同一调用点。
fn open_exclusive_0600(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
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
    // #R15（第15轮 bug/high）：`@base <iri>` 声明。Turtle 导出常见 `@base <...>` + 相对 `<rel>`
    // IRI 形式；若不支持，主语/谓词/对象都不会解析为绝对 IRI，受控关系谓词无法命中命名空间，
    // 显式边从 source_edges 整体丢失，集合差把全部显式边误判为推断边写回（静默污染）。
    let mut base: Option<String> = None;
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
        if let Some(b) = parse_base_decl(line_trimmed) {
            // #R15：`@base <iri>` 声明，供相对 `<rel>` IRI 展开。不参与三元组状态机。
            base = Some(b);
            prev_sep = None;
            continue;
        }
        if line_trimmed.starts_with('@') {
            // 其它 @ 指令本骨架不处理，跳过。
            continue;
        }
        // 剥掉行尾续行标记。
        let line = line_trimmed.trim_end_matches([';', ',', '.']).trim();
        // #11（第12轮 bug/high）：Turtle 允许续行分隔符出现在**行首**：
        //     :docC :supersedes :docA
        //       , :docB ;
        //       :createdBy :alice .
        // 此时行首 token 是 `,`/`;`。若直接按"行首首个 IRI 决定主语/续行"处理，
        // `expand_term` 对 `,`/`;` 返回 None，且 prev_sep（仅由上一行结尾推导）可能为 None，
        // 会落入 `first_term.is_none() && prev_sep.is_none()` 分支重置 subj/pred，
        // 静默丢弃续行的三元组（docB/createdBy 边）→ source_edges 漏算 → 物化 diff 误标为推断
        // 并写回（evidence=ontology:materialized），污染推断记账。行首分隔符 = 显式续行标记，
        // 必须把它的语义并入 prev_sep 并跳过该 token，让后续 IRI 按续行处理。
        let leading_sep = if line_trimmed.starts_with(';') {
            Some(';')
        } else if line_trimmed.starts_with(',') {
            Some(',')
        } else {
            None
        };
        // 提取本行所有 <iri> token 及分隔符（; , 区分谓词/对象续行）
        let tokens = tokenize_ttl(line);
        // 若行首有分隔符，它是续行标记，不参与 token 索引（跳过它）；把 prev_sep 设为该分隔符
        // 的语义。行首 `;` → 下行首个 IRI 是新谓词；行首 `,` → 下行首个 IRI 是新对象。
        if leading_sep.is_some() {
            prev_sep = leading_sep;
        }

        // #6（第6轮 bug/medium）：`a <Type>` 是 rdf:type 简写，不产出目标关系边。
        // 但只有**纯类型声明行**（`a <Type>` 是整行唯一内容，无后续谓词/对象）才能整行跳过；
        // `a <Type> ; <pred> <obj>`（同一行以 `;` 分隔出关系边）不是纯类型，整行跳过会
        // 静默丢失 `<pred> <obj>` 边——该边从 source_edges 与 all_edges 双双消失，物化 diff
        // 会把既有链接误报为推断或漏写回。故：非纯类型行只跳过前导 `a <Type>` 两个 token。
        // 谓词 `a` 在 tokenize 中作为 token 捕获（`a ` 分支）。
        let type_pred_at_0 = tokens.first().map(|t| t.as_str()) == Some("a");
        // #R15（第15轮 bug/low）：`a <T>` 的类型对象必须被显式跳过（含 `,`/`;` 续行），
        // 不能依赖 `tokens.len()==2` 判定纯类型——`a <T> ; <pred> <obj>` 或 `a <T> , <U> ;`
        // 时 token 数 >2，旧逻辑把 `<T>` 当谓词、`,` 后的 `<U>` 当对象，若 `<T>` 恰为受控
        // 命名空间内白名单局部名（如 `.../partOf` 用作类）会产出伪边 (s, T, U)。显式跳过
        // `a` 及其全部类型对象与分隔符，只保留真正的目标关系边。#6 原先的"纯类型跳过"由
        // 这个更通用的类型对象跳过取代。
        // 计算本行起始 token 索引 `i`（类型行跳过 `a <T>` 及对象列表；普通行做主语/谓词判定）。
        // 之后统一用 while 处理关系边（类型行 `;` 续行与普通行共用同一状态机）。
        let mut i;
        if type_pred_at_0 {
            // tokens: [a, <T0>, {, <T1>...} | {; pred obj...}]
            // 跳过 `a`（index 0）。
            i = 1;
            // 跳过第一个类型对象 `<T0>`。
            if i < tokens.len() {
                i += 1;
            }
            // 跳过逗号分隔的其它类型对象 `, <Tn>`。
            while i < tokens.len() && tokens[i] == "," {
                i += 2; // 跳过 `,` 和其后的类型对象
            }
            // 若后面紧跟 `;`，这是续行（后续可能有关系边），置 i 于 `;` 处交给状态机；
            // 否则本行无目标关系边，按纯类型行处理。
            if i < tokens.len() && tokens[i] == ";" {
                // 不能跳过 `;`：跳过会绕过状态机 `;` 分支（清 pred），遗留上一行 stale pred，
                // 续行首个 `<pred>` 会被误当上一 stale 谓词的对象产出伪边。显式清空再前进。
                pred = None;
                i += 1;
            } else {
                // 纯类型行（或仅对象列表）：无目标关系边。
                prev_sep = if line_trimmed.ends_with(';') {
                    Some(';')
                } else if line_trimmed.ends_with(',') {
                    Some(',')
                } else {
                    None
                };
                continue;
            }
        } else {
            i = 0;
            // 行首首个 token 若能展开为 IRI，且上一行不是续行 → 新主语块。
            // 同时兼容 `<iri>` 与 `p:local`/`:local`（#R4-3 前缀展开）。
            let first_term = tokens.get(i).and_then(|t| expand_term(t, &prefixes, &base));
            // 仅当上一行不是续行（`;`/`,`）且本行首 token 是 IRI 时，才视为新主语块。
            let new_block = prev_sep.is_none() && first_term.is_some();
            if new_block {
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
                // 让后续 `<pred> <obj>` 正常产出边。仅对确实无法识别的主语才重置，避免 stale 伪边。
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
        }

        // 统一处理关系边（类型行 `;` 续行与普通行共用状态机，#R15）。
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
                    // #R15（第15轮 bug/low）：`a` 是 rdf:type 简写谓词，可能出现于**主语之后**：
                    // `<subj> a <T0> , <T1> ; <pred> <obj>`。当 `a` 落在谓词位时，其后到 `;`（或行尾）
                    // 的对象都是类型对象，不能按"谓词+对象"产边——否则 `<T0>` 被当谓词、`,` 后的
                    // `<T1>` 被当对象，若 `<T0>` 恰为受控命名空间内白名单局部名（如 `.../partOf`
                    // 用作类）会产出伪边 (s, T0, T1)。这里显式跳过 `a` 及其全部类型对象列表。
                    if pred.is_none() && tok == "a" {
                        // 跳过 `a` 本身。
                        i += 1;
                        // 跳过首个类型对象 `<T0>`（若存在）。
                        if i < tokens.len() {
                            i += 1;
                        }
                        // 跳过逗号分隔的其它类型对象 `, <Tn>`。
                        while i < tokens.len() && tokens[i] == "," {
                            i += 2;
                        }
                        // 若 `<T0>` 后紧跟 `;`，由主循环 `;` 分支清 pred 继续关系边；
                        // 否则本行无目标关系边，循环条件自然结束。
                        continue;
                    }
                    // 尝试把 token 展开为完整 IRI：#R4-3 前缀展开。
                    // 支持 `<iri>` 与 `p:local`（含默认 `:local`）两种形式。
                    if let Some(iri) = expand_term(tok, &prefixes, &base) {
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
                    } else if pred.is_some()
                        && (tok.starts_with("_:") || tok.starts_with('['))
                    {
                        // #7（第10轮 bug/medium）：blank node **对象** 不能静默丢弃，须与
                        // 主语处理对称。若 `_:b0`/`[...]` 在对象位被丢，source_edges 漏算这些边；
                        // 物化后若 OWL 推理器把 blank node skolemize 成新 IRI，集合差会误标为
                        // 推断边写回（evidence=ontology:materialized）。故对象位保留 blank node
                        // 占位进 source_edges。写回时 write_back_edges 已过滤 `_:`/`[` 端点。
                        if let (Some(s), Some(p)) = (&subj, &pred) {
                            if map_relation_iri(p).is_some() {
                                all.push((s.clone(), p.clone(), (*tok).to_string()));
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
            // #10（第7轮 bug/low）：跳过引号字面量时处理转义——`\"` 是字面量内的转义引号，
            // 不是闭合引号。此前 `find(quote)` 会把 `"...\"..."` 里的 `\"` 误当闭合，之后的
            // `#`/`;`/`,` 被误判，可能截断行或伪造边。逐字节扫描，遇 `\` 跳过下一位。
            let quote = b;
            let mut j = i + 1;
            let mut found = false;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2; // 跳过转义序列（含转义引号）
                    continue;
                }
                if bytes[j] == quote {
                    i = j + 1;
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                return &line[..i]; // 未闭合引号，其后到行尾视为注释
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

/// 解析 `@base <iri> .` 声明，返回基 IRI（#R15/#631 bug/high）。
/// Turtle 导出常见形式：`@base <http://.../onto/>` + 相对 `<rel>` IRI。
/// 返回 None 表示不是 `@base` 声明。
fn parse_base_decl(line: &str) -> Option<String> {
    let rest = line.strip_prefix("@base")?.trim_start();
    // `@base <iri> .` —— 末尾通常有 `.`，先剥掉再取 `<>` 内 IRI。
    let rest = rest.strip_suffix('.').unwrap_or(rest).trim_end();
    rest.strip_prefix('<')?.strip_suffix('>').map(|s| s.to_string())
}

/// 把 TTL 术语 token 展开为完整 IRI。
/// - `<iri>` → `<rel>` 是相对 IRI 且有 `@base` 时按 RFC3986 相对解析；否则直接返回内层 IRI。
/// - `p:local`（含默认 `:local`）→ 用前缀表展开；前缀未声明则返回 None（跳过）。
/// - 其它（如 `a`）→ None。
/// #R15（第15轮 bug/high）：`base` 参数来自 `@base` 声明，供相对 `<rel>` 展开。
fn expand_term(tok: &str, prefixes: &HashMap<String, String>, base: &Option<String>) -> Option<String> {
    if tok.starts_with('<') && tok.ends_with('>') && tok.len() >= 2 {
        let inner = &tok[1..tok.len() - 1];
        // 相对 IRI（无 scheme 且非 `//`、非绝对路径空根）：需 base 解析，否则受控谓词无法命中。
        // 绝对判定用 RFC3986 scheme `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` 加尾随 `:`。
        // 仅 `contains("://")` 会漏掉 `urn:`/`mailto:`/`file:`/`tel:` 等无 `//` 的合法 scheme，
        // 被误并入 @base 产生 `http://base/mailto:foo@bar` 类垃圾实体 id。
        let has_valid_scheme = inner.split_once(':').map_or(false, |(scheme, rest)| {
            !rest.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        });
        let is_absolute = has_valid_scheme || inner.starts_with("//");
        if !is_absolute {
            if let Some(b) = base {
                return Some(resolve_iri(b, inner));
            }
        }
        return Some(inner.to_string());
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

/// 简化的 RFC3986 相对 IRI 解析：把 `rel` 按 `base` 解析为绝对 IRI。
/// 覆盖本骨架需要的 Turtle 相对形式：`<rel>`、`<dir/rel>`、`<rel.ttl>` 等。
/// 不做完整的 RFC3986 段合并（非 goal），但正确处理常见导出形式。
fn resolve_iri(base: &str, rel: &str) -> String {
    // 分割 base 的 scheme + authority + path：`scheme://authority/path...`
    let (scheme, scheme_rest) = match base.find("://") {
        Some(i) => (&base[..i], &base[i + 3..]),
        None => ("", base),
    };
    // scheme_rest = `authority/path`
    let (authority, base_path) = match scheme_rest.find('/') {
        Some(i) => (&scheme_rest[..i], &scheme_rest[i..]),
        None => (scheme_rest, ""),
    };
    // 相对引用以 `/` 开头 → 替换整个路径
    if rel.starts_with('/') {
        return format!("{}://{}{}", scheme, authority, rel);
    }
    // 相对引用是纯路径 → 基于 base 的目录部分拼接
    let base_dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    format!("{}://{}{}{}", scheme, authority, base_dir, rel)
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
            // #10（第7轮 bug/low）：跳过引号字面量时处理转义——`\"` 是字面量内转义引号，
            // 不是闭合引号。此前 `find(quote)` 会把 `"...\"..."` 里的 `\"` 误当闭合，之后的
            // `;`/`,` 被误判，可能伪造边。逐字节扫描，遇 `\` 跳过下一位。
            let quote = b;
            let bytes = rest.as_bytes();
            let mut j = 1;
            let mut found = false;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == quote {
                    rest = &rest[j + 1..];
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                break; // 未闭合引号，丢弃行剩余部分
            }
            // #3（第8轮 bug/medium）：类型化字面量 `"..."^^<datatype>` 与语言标签
            // `"..."@en`。字面量闭合后紧跟的 `^^<datatype>` 是一个整体术语——datatype
            // 是字面量的类型标注，不是三元组的对象。若 `^^` 与 `<datatype>` 各自被当
            // 普通 token，`<datatype>` 会被 parse_ttl_edges 展开为 IRI 并当作当前谓词的
            // 对象，产出伪边 (s, p, <datatype-IRI>)，且因 datatype 不在 source_edges 而
            // 误入 inferred_edges 写回为垃圾实体。故闭合后把 `^^<datatype>` 或 `@lang`
            // 一并吞掉（不产出 token）。
            if rest.starts_with("^^") {
                // `^^<datatype>`：吞到 `>`。漏 `>` 视作行尾废弃。
                match rest.find('>') {
                    Some(end) => rest = &rest[end + 1..],
                    None => break,
                }
            } else if rest.starts_with('@') {
                // `@lang`：吞到下一个空白/分隔符。
                let next = rest
                    .find([' ', '\t', ';', ',', '.'])
                    .unwrap_or(rest.len());
                rest = &rest[next..];
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
            // 已知局限（#10 第7轮 bug/low）：跨行 blank node（`[ a <Doc> ; <pred> <obj> ]`
            // 的 `]` 在下一行）本骨架不处理——无 `]` 时丢弃本行剩余、后续行重新附着到外层主语。
            // 这是轻量 TTL 扫描器的取舍；生产级多行 blank node 需跨行状态机（超出本期范围）。
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
            // #6（第8轮 bug/low）：blank node 端点（`_:b0`，或 `[ ... ]` 属性列表主语被
            // parse_ttl_edges 保留为 `[...]` 字符串）是 **document-scoped**、非全局唯一——
            // 不同命名空间/多次运行的同名 blank node 会坍缩成一个实体（ON CONFLICT DO NOTHING
            // 静默合并），在语义图里产生垃圾实体/边。物化后的 TTL 里 blank node 是推理器
            // skolemize 的临时节点，对下游无语义价值。写回前过滤掉 `_:` 与 `[` 端点。
            if s.starts_with("_:") || o.starts_with("_:") || s.starts_with('[') || o.starts_with('[') {
                skipped += 1;
                continue;
            }
            // 实体 id = 完整 IRI（区分命名空间）；name = 局部名（可读）。
            let sname = iri_local_name(s);
            let oname = iri_local_name(o);
            // 幂等 upsert 两端实体（满足外键约束）——错误必须传播，不静默吞掉（#9），
            // 否则 INSERT 失败会被掩盖，直到边插入时才暴露为令人困惑的 FK 冲突。
            // #3（第7轮 bug/medium）：entities.id 是全局主键（无命名空间维度），entity_edges
            // 却按行带 namespace。若 `DO UPDATE SET namespace=excluded.namespace`，两个命名空间
            // 引用同一 IRI 时实体行会"翻动"到最后写它的命名空间，而既有边仍带各自 namespace，
            // 分歧只是从"首个拥有者"变成"最后写入者"，查询 join 仍不一致。故保持 DO NOTHING
            // （首个拥有者语义），不静默改租户归属；实体 id 用完整 IRI 已尽量降低撞名概率（#6）。
            // #R18（第18轮 security/medium）：es.entities.id 是全局 PK，entity_edges 按行带
            // namespace——多租户引用同一 IRI 时，第二个租户的边 FK 指向第一个租户的 entity 行，
            // 该行的 name/aliases/summary 经共享行对第二个租户可见，且第二个租户按 namespace 统计
            // 实体时该行不计入（graph build_graph 分歧）。完整修复需把 namespace 并入 entities
            // 复合主键（跨 schema 迁移 + entity_mentions/entity_edges 复合 FK），属独立数据模型
            // 任务，不在本骨架热循环内强改（避免大范围迁移引入回归）。此处做**防御性检测**：
            // 当实体 id 已存在但 namespace 不同（跨租户撞名）时显式告警，暴露此前被 ON CONFLICT
            // DO NOTHING 静默掩盖的串扰，供运维/后续迁移决策；实体行保持首个 owner 语义。
            for (eid, ename) in [(s.as_str(), sname.as_str()), (o.as_str(), oname.as_str())] {
                let existing_ns: Option<String> = tx
                    .query_row(
                        "SELECT namespace FROM entities WHERE id = ?1",
                        rusqlite::params![eid],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(existing) = existing_ns {
                    if existing != namespace {
                        eprintln!(
                            "WARN: entity {} already owned by namespace {:?}, cannot re-owner as {:?}; \
                             share row but per-namespace graph isolation may be lossy (see R18)",
                            eid, existing, namespace
                        );
                        continue; // 跳过 upsert，保持首个 owner；不静默覆盖
                    }
                }
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
            // #4（第9轮 bug/low）：`tx.execute` 返回受影响行数。conflict 且 WHERE 不满足
            // （既有 evidence 既非 NULL 也非 ontology:materialized，即用户手工/其它管线 curated）
            // 时是 no-op（0 行），不应计入 written——否则返回的 (written, skipped) 虚高，误导
            // CLI 输出与调用方。仅当确实插入/更新了行才 written += 1。
            let affected = tx
                .execute(
                    "INSERT INTO entity_edges(namespace, source_entity_id, target_entity_id, relation_type, weight, evidence)
                     VALUES(?1, ?2, ?3, ?4, 1.0, 'ontology:materialized')
                     ON CONFLICT(namespace, source_entity_id, target_entity_id, relation_type)
                     DO UPDATE SET evidence=excluded.evidence
                     WHERE entity_edges.evidence IS NULL OR entity_edges.evidence='ontology:materialized'",
                    rusqlite::params![namespace, s, o, rtype],
                )
                .map_err(|e| format!("insert edge {} {} {}: {}", s, rtype, o, e))?;
            if affected > 0 {
                written += 1;
            }
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

/// 规范化 IRI 用于**集合差比较**（#R17 第17轮 bug/medium）。
///
/// 推断边 = `materialized_set.difference(&source_edges)` 依赖源 TTL 与推理器输出的 IRI **逐字符串
/// 相等**。`expand_term` 对无 `@base` 的相对 `<rel>` IRI 会保留相对形式，而 open-ontologies 按自己
/// 的 base 解析成绝对 IRI——此时全部显式边落进差集、被误标为推断边写回（静默污染）。`@base` 支持
/// 只覆盖"源 TTL 声明了 @base"这一类场景。
///
/// 这里做**轻量归一**，对齐最常见的推导不一致来源：
/// - 去除多余尾部 `#`/`/`（同一 IRI 的 `http://x/onto/docA#` 与 `http://x/onto/docA` 视为相同）
/// - 但对"相对 vs 绝对"（无 @base 时）无力对齐——那需要子进程统一的 base 语义，归到下方
///   `inferred_ratio` 告警兜底捕捉。
fn normalize_iri(iri: &str) -> String {
    let mut s = iri.to_string();
    while s.len() > 1 && (s.ends_with('#') || s.ends_with('/')) {
        s.pop();
    }
    s
}

/// 对三元组做轻量 IRI 归一（用于集合差比较，#R17）。顺序不变。
fn normalize_edge(e: &(String, String, String)) -> (String, String, String) {
    (normalize_iri(&e.0), normalize_iri(&e.1), normalize_iri(&e.2))
}

/// 受控本体命名空间前缀（#R4-1 命名空间感知）。
/// 只有来自这些命名空间的谓词 IRI 才被识别为受控语义边；其它命名空间的
/// 局部名即使撞上白名单（如 `http://foreign-vendor/onto#references`）也拒绝，
/// 防止无关词汇注入 entity_edges。
/// - `http://memoria.ai/onto/`：本模块 schema_core 的默认本体命名空间（生产受控）
/// - `http://www.w3.org/2002/07/owl#`：OWL 内置（不作为语义边，仅站位）
/// - `http://example.org/`：**仅测试 / 示例**——见 `controlled_ns()` 的 cfg 门禁
///
/// #8（第10轮 security/low）：`http://example.org/` 是测试专用命名空间，但此前在**生产匹配
/// 路径**里活跃。一旦模块被租户可控 TTL 触发（未来 MCP/web），租户可用 `example.org` 下的
/// 白名单局部名（supersedes/createdBy…）绕过受控词汇门禁。故生产构建的受控命名空间只含
/// memoria 本体；example.org 通过 `#[cfg(test)]` 仅在测试构建加入（单测/集测 fixture 依赖它）。
const PROD_CONTROLLED_NS: &[&str] = &["http://memoria.ai/onto/"];

/// 返回当前编译目标的受控命名空间集合（生产 vs 测试）。
fn controlled_ns() -> &'static [&'static str] {
    #[cfg(test)]
    {
        // 测试构建：生产命名空间 + 测试专用 example.org（静态数组，避免临时值引用）
        &["http://memoria.ai/onto/", "http://example.org/"]
    }
    #[cfg(not(test))]
    {
        PROD_CONTROLLED_NS
    }
}

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
    let ns = controlled_ns().iter().find(|ns| pred.starts_with(**ns));
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
    // `--help` 探测用固定短超时，与物化超时解耦：若二进制在 --help 上挂死，
    // status 是健康检查命令，必须快速失败而非按物化超时（可达 86400s）阻塞近一天。
    const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
    let timeout = PROBE_TIMEOUT;
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
    let stdout_txt = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
    let stderr_txt = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
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
    fn ttl_parse_handles_leading_separator_continuation() {
        // #4（第12轮 bug/high）：Turtle 允许续行分隔符 `,`/`;` 出现在**行首**。此前状态机只认
        // 行尾分隔符，行首分隔符会落入 `first_term.is_none() && prev_sep.is_none()` 分支重置
        // subj/pred，静默丢弃续行三元组 → source_edges 漏算 → 物化 diff 误标为推断并写回。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docC> <http://example.org/supersedes> <http://example.org/docA>
    , <http://example.org/docB> ;
    <http://example.org/createdBy> <http://example.org/alice> .
"#;
        let all = parse_ttl_edges(ttl);
        // 3 条显式边：docC supersedes docA, docC supersedes docB, docC createdBy alice
        assert_eq!(all.len(), 3, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docC".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docB".to_string()
        )));
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
    fn ttl_parse_expands_base_relative_iris() {
        // #R15（第15轮 bug/high）：`@base <...>` + 相对 `<rel>` IRI（Turtle 导出常见形式）。
        // 若不展开，主语/谓词/对象都不会解析为绝对 IRI，受控谓词无法命中命名空间，显式边
        // 从 source_edges 整体丢失，集合差把全部显式边误判为推断边写回（静默污染）。
        let ttl = r#"@base <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
<supersedes> a owl:ObjectProperty, owl:TransitiveProperty .
<docB> <supersedes> <docA> ;
    a owl:ObjectProperty .
"#;
        let all = parse_ttl_edges(ttl);
        // `a` 类型行不产边；只应产出 (docB, supersedes, docA)，且经 @base 展开为绝对 IRI。
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://memoria.ai/onto/docB".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docA".to_string()
        )), "got {:?}", all);
    }

    #[test]
    fn ttl_parse_skips_type_objects_with_continuation() {
        // #R15（第15轮 bug/low）：`a <T> ; <pred> <obj>` 或 `a <T> , <U> ;` 带续行分隔符时，
        // `<T>` 绝不能被当谓词、`,` 后的 `<U>` 当对象——若 `<T>`/`<U>` 恰为受控命名空间内
        // 白名单局部名会产出伪边。类型对象须显式跳过，只保留真正的目标关系边。
        let ttl = r#"@base <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
<docC> a <http://memoria.ai/onto/partOf>, <http://memoria.ai/onto/Agent> ;
    <supersedes> <docA> .
"#;
        let all = parse_ttl_edges(ttl);
        // 只产 1 条 (docC, supersedes, docA)；不得因类型对象 partOf/Agent 产伪边。
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://memoria.ai/onto/docC".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docA".to_string()
        )), "got {:?}", all);
    }

    #[test]
    fn ttl_parse_type_continuation_clears_stale_pred() {
        // #R20（第20轮 bug/high）：类型声明行 `a <T> ;` 的 `;` **不能**被跳过——若前一续行在
        // `pred` 里留有 stale 值（如 partOf），跳过 `;` 会让状态机 `;` 分支（清 pred）永不执行，
        // 续行 `<supersedes> <docC>` 被误当 stale 谓词的对象，产出伪边 (docA, partOf, supersedes)
        // 与 (docA, partOf, docC)，且真实边 (docA, supersedes, docC) 丢失。物化 diff 会把丢失的
        // 显式边误标为推断边写库（evidence=ontology:materialized），污染 provenance。
        let ttl = r#"@prefix : <http://memoria.ai/onto/> .
:docA :partOf :docB ;
    a :Document ; :supersedes :docC .
"#;
        let all = parse_ttl_edges(ttl);
        // 期望 2 条真实边：(docA, partOf, docB)（前一续行）与 (docA, supersedes, docC)（类型行后）。
        // 若 `;` 被跳过导致 stale pred 残留，会多出 (docA, partOf, supersedes)/(docA, partOf, docC)
        // 伪边（共 4 条）且 (docA, supersedes, docC) 丢失。断言精确计数 + 无 partOf 伪边。
        assert_eq!(all.len(), 2, "got {:?}", all);
        assert!(
            all.contains(&(
                "http://memoria.ai/onto/docA".to_string(),
                "http://memoria.ai/onto/supersedes".to_string(),
                "http://memoria.ai/onto/docC".to_string()
            )),
            "got {:?}",
            all
        );
        // partOf 只应有合法对象 docB；若 stale pred 残留会把 supersedes/docC 也塞成 partOf 对象。
        assert!(
            !all
                .iter()
                .any(|(_, p, o)| p == "http://memoria.ai/onto/partOf"
                    && o != "http://memoria.ai/onto/docB"),
            "stale pred partOf must not capture supersedes/docC as objects: got {:?}",
            all
        );
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
    fn ttl_parse_skips_inner_type_of_blank_node_object() {
        // #8（第5轮）+ #7（第10轮 bug/medium）：blank node 对象 `[ a <Document> ]`——内层
        // `<Document>` 不得成为外层谓词的对象（否则产生伪边 (docA, partOf, Document)），
        // 且整个 `[ ... ]` 跨度须作为**对象占位**保留进 source_edges（#7，与主语对称，防
        // skolemize 后误标推断）。此前断言 `all.len()==0`（静默丢对象边）已被 #7 修正。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docA> <http://example.org/partOf> [ a <http://example.org/Document> ] .
"#;
        let all = parse_ttl_edges(ttl);
        // 对象是本 bracket 跨度占位（非内层 <Document>），partOf 受控关系应保留这条边
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docA".to_string(),
            "http://example.org/partOf".to_string(),
            "[ a <http://example.org/Document> ]".to_string()
        )), "got {:?}", all);
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
    fn ttl_parse_keeps_blank_node_object_edges() {
        // #7（第10轮 bug/medium）：blank node **对象**（`_:b0` / `[ ... ]`）须与主语对称保留
        // 进 source_edges，否则 source_edges 漏算，物化后推理器 skolemize 成新 IRI 时这些边
        // 被集合差误标为推断边写回（evidence=ontology:materialized）。写回时 write_back_edges
        // 已过滤 `_:`/`[` 端点，不会污染全局实体。
        let ttl = r#"@prefix : <http://example.org/> .
:docA <http://example.org/supersedes> _:b0 .
:docB <http://example.org/partOf> [ a <http://example.org/Document> ] .
"#;
        let all = parse_ttl_edges(ttl);
        // 两条边都应保留（对象为 blank node 占位）
        assert!(
            all.contains(&(
                "http://example.org/docA".to_string(),
                "http://example.org/supersedes".to_string(),
                "_:b0".to_string()
            )),
            "blank-node object edge should be retained, got {:?}",
            all
        );
        assert!(
            all.contains(&(
                "http://example.org/docB".to_string(),
                "http://example.org/partOf".to_string(),
                "[ a <http://example.org/Document> ]".to_string()
            )),
            "bracket blank-node object edge should be retained, got {:?}",
            all
        );
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

    #[test]
    fn win_path_drive_heuristic_strict() {
        // #9（第7轮 bug/low）：盘符启发式必须严格（X: 后跟 / 或 \）。
        // 在非 Windows 上，Unix 相对路径 `a:b.ttl`（含反斜杠）不得被改写。
        if !cfg!(windows) {
            // 仅冒号、后随非 / \ 的路径，不是盘符 → 保留反斜杠
            assert_eq!(
                win_path(std::path::Path::new("a:b.ttl")),
                "a:b.ttl"
            );
            // 严格盘符（X:/ 或 X:\）→ 反斜杠转正斜杠
            assert_eq!(
                win_path(std::path::Path::new("D:\\data\\a.ttl")),
                "D:/data/a.ttl"
            );
        }
    }

    #[test]
    fn strip_inline_comment_handles_escaped_quotes() {
        // #10（第7轮 bug/low）：`\"` 是字面量内转义引号，不是闭合引号，其后的 `#` 是注释起点。
        // 若误把 `\"` 当闭合，`#` 会被吞进字面量、注释不剥离。
        assert_eq!(
            strip_inline_comment("\"a\\\"b\" <p> <o> . # see"),
            "\"a\\\"b\" <p> <o> . "
        );
        // 引号字面量内 `#` 不是注释起点
        assert_eq!(strip_inline_comment("\"a#b\" <p> <o> ."), "\"a#b\" <p> <o> .");
    }

    #[test]
    fn ttl_parse_typed_literal_datatype_not_an_object() {
        // #3（第8轮 bug/medium）：类型化字面量 `"..."^^<datatype>` 中 datatype 是字面量的
        // 类型标注，不是三元组对象。若 `<datatype>` 被当对象，会产出伪边 (s, p, <datatype>)
        // 且因 datatype 不在 source_edges 而误入 inferred_edges 写回为垃圾实体。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docA> <http://example.org/createdBy> "2024-01-01"^^<http://www.w3.org/2001/XMLSchema#date> .
<http://example.org/docB> <http://example.org/createdBy> "alice"@en .
"#;
        let all = parse_ttl_edges(ttl);
        // 两个字面量对象（类型化 + 语言标签）都不应产出边，因为对象不是 IRI
        assert_eq!(all.len(), 0, "typed literal datatype emitted a spurious edge: got {:?}", all);
    }

    #[test]
    fn ttl_parse_typed_literal_keeps_other_relations() {
        // 类型化字面量不该吞掉同一行后续的 IRI 关系边。
        let ttl = r#"@prefix : <http://example.org/> .
<http://example.org/docA> <http://example.org/createdBy> "2024-01-01"^^<http://www.w3.org/2001/XMLSchema#date> ; <http://example.org/supersedes> <http://example.org/docB> .
"#;
        let all = parse_ttl_edges(ttl);
        assert_eq!(all.len(), 1, "got {:?}", all);
        assert!(all.contains(&(
            "http://example.org/docA".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docB".to_string()
        )));
    }

    #[test]
    fn write_back_filters_blank_node_endpoints() {
        // #6（第8轮 bug/low）：blank node 端点（`_:b0` / `[ ... ]`）是 document-scoped、
        // 非全局唯一，写回会坍缩成同一实体污染全局 entities 表。write_back_edges 应过滤。
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory conn");
        // 建 entity_edges 所需的最小表结构（与 storage/sqlite.rs 对齐）
        conn.execute_batch(
            "CREATE TABLE entities(
                id TEXT PRIMARY KEY, namespace TEXT, entity_type TEXT,
                name TEXT, aliases TEXT, summary TEXT
            );
            CREATE TABLE entity_edges(
                namespace TEXT, source_entity_id TEXT, target_entity_id TEXT,
                relation_type TEXT, weight REAL, evidence TEXT,
                PRIMARY KEY(namespace, source_entity_id, target_entity_id, relation_type)
            );",
        )
        .expect("create tables");
        // blank node 端点的边应被过滤（skipped=1, written=0）
        let edges = vec![(
            "_:b0".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string(),
        )];
        let (written, skipped) = write_back_edges(&conn, "test", &edges).expect("write");
        assert_eq!(written, 0, "blank node source should be filtered");
        assert_eq!(skipped, 1, "blank node source should count as skipped");
        // 正常 IRI 端点仍应写入
        let edges2 = vec![(
            "http://example.org/docB".to_string(),
            "http://example.org/supersedes".to_string(),
            "http://example.org/docA".to_string(),
        )];
        let (w2, _) = write_back_edges(&conn, "test", &edges2).expect("write");
        assert_eq!(w2, 1, "normal IRI edge should be written");
    }

    #[test]
    fn map_relation_iri_outputs_in_rel_types() {
        // #6（第12轮 maintainability/low）：map_relation_iri 硬编码的短名白名单必须与
        // RELATION_TYPES（tools/graph.rs）保持同步。若未来 RELATION_TYPES 改名/删除某短名，
        // map_relation_iri 仍产出旧短名 → is_valid_relation_type 拒绝 → 写回被静默丢弃，
        // 且无编译/测试信号。此测试断言 map_relation_iri 的每个输出都落在 RELATION_TYPES 内，
        // 把耦合变成显式约束。
        let ns = "http://memoria.ai/onto/";
        let locals = [
            "references",
            "supersedes",
            "createdBy",
            "created_by",
            "conflictsWith",
            "conflicts_with",
            "dependsOn",
            "depends_on",
            "partOf",
            "part_of",
            "belongsTo",
            "belongs_to",
        ];
        for local in locals {
            let iri = format!("{ns}{local}");
            let short = map_relation_iri(&iri)
                .unwrap_or_else(|| panic!("map_relation_iri({iri}) should map"));
            assert!(
                RELATION_TYPES.contains(&short.as_str()),
                "map_relation_iri short name '{short}' (from '{local}') not in RELATION_TYPES"
            );
        }
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
            // #R17（第17轮 other/low）：`(inferred {})` 原用 `triples_after - triples_before`——那是三元组
        // 计数差（含 schema 本体声明、rdf:type 等非语义边），与下一行 `inferred_edges: {}`（语义
        // 推断边数）不一致，可能误导运维（"inferred 500" 而实际仅 3 条语义边）。改标签为
        // `triples_delta` 明确其语义，避免与 inferred_edges 混淆。
        Ok(format!(
            "materialize OK\nduration_ms: {}\nprofile: {}\ntriples: {} -> {} (triples_delta {})\ninferred_edges: {}",
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
    // 解析 --port N（#8 第7轮 maintainability/low）：非法值不再静默回退 18080——
    // 操作者以为配了自定义端口实际跑了默认端口，会引发困惑。`--port abc`、`--port 0` 显式报错；
    // 未提供 --port 才用默认 18080。
    let mut port: u16 = 18080;
    if let Some(idx) = args.iter().position(|a| a == "--port") {
        let raw = args.get(idx + 1).ok_or("--port requires a value")?;
        let parsed: u16 = raw
            .parse()
            .map_err(|_| format!("invalid --port value {:?} (expected 1..=65535)", raw))?;
        if parsed == 0 {
            return Err("invalid --port value 0 (0 means ephemeral; specify 1..=65535)".to_string());
        }
        port = parsed;
    }
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
            let _ = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
            let _ = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
            return Err(format!("wait: {}", e));
        }
    };
    if let Some(st) = status {
        // #8（第6轮 maintainability/low）：退出路径也要 join 两个 reader（此前只 join
        // stderr_reader，stdout_reader 的 JoinHandle 被 drop 不 join——若子进程延迟关 stdout，
        // 该 reader 线程会长期阻塞在 read_to_string）。与成功路径 / materialize/status 的
        // kill+wait+join 纪律一致。用有界 join（#4）防孙进程持管道永久阻塞。
        let _ = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
        let err = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
        return Err(format!(
            "serve-http exited immediately (code {}): {}",
            st, err
        ));
    }
    // 进程在 800ms 内未退出 → 视为能正常启动（占位验证）。随后 kill 并 wait 收割，
    // 避免 Unix 下遗留僵尸进程（#121，与 materialize 超时路径的 kill+wait 一致）。
    let _ = child.kill();
    let _ = child.wait();
    let _ = join_bounded(stdout_reader, Duration::from_secs(2), String::new());
    let _ = join_bounded(stderr_reader, Duration::from_secs(2), String::new());
    Ok(format!(
        "serve-http 在线通道占位 OK\nport: {} (进程存活>800ms, 未实测端口监听)\nstorage: persistent\nverification_ms: {}\n(注: serve-http 为 MCP Streamable HTTP, 健康探测需 MCP 握手, 本期未接客户端; '端口已绑定'未实测, 仅验证进程可启动)",
        port,
        start.elapsed().as_millis()
    ))
}