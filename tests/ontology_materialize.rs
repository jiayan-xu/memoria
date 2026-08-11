//! Open Ontologies 形态 B 骨架 — 端到端集成测试。
//!
//! 验证：物化（load schema+数据 → reason → save）→ 解析推断边 → 写回 entity_edges 完整闭环。
//!
//! 运行：`cargo test --test ontology_materialize`
//! 前置：OPEN_ONTOLOGIES_BIN 指向 open-ontologies 可执行文件；缺失时测试自动跳过。
//!
//! 防假绿门禁：**缺失二进制静默 SKIP 的默认只用于本地开发**。CI 的 `ci.yml` 测试步骤固定
//! `export REQUIRE_ONTOLOGIES_BIN=1`——在该标志下找不到二进制会 panic 而非 skip，故 CI 的
//! materialize→写回闭环**必然真实执行**（不会"tests passed 但没跑物化"）。本地如需同样硬门禁，
//! 手动设 `REQUIRE_ONTOLOGIES_BIN=1` 即可。
//!
//! 硬约束（报告 §8）：离线物化，写回 entity_edges（受控枚举门禁），不动热路径。

use memoria_core::ontology::{materialize, write_back_edges, OntologyConfig};
use rusqlite::params;

/// 测试用最小 schema（含 supersedes 传递属性声明）。
const SCHEMA_TTL: &str = r#"@prefix : <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
:supersedes a owl:ObjectProperty, owl:TransitiveProperty .
:createdBy a owl:ObjectProperty .
"#;

/// 测试用数据：docB supersedes docA, docC supersedes docB → 应推断 docC supersedes docA。
/// 用完整 `<...>` IRI（非前缀名），使 `parse_ttl_edges` 能解析出 source_edges，
/// 从而 `inferred_edges = materialized_set - source_edges` 真正只含推断边（#123）。
const DATA_TTL: &str = r#"@prefix : <http://memoria.ai/onto/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
<http://memoria.ai/onto/docA> <http://memoria.ai/onto/createdBy> <http://memoria.ai/onto/alice> .
<http://memoria.ai/onto/docB> <http://memoria.ai/onto/createdBy> <http://memoria.ai/onto/alice> ; <http://memoria.ai/onto/supersedes> <http://memoria.ai/onto/docA> .
<http://memoria.ai/onto/docC> <http://memoria.ai/onto/createdBy> <http://memoria.ai/onto/alice> ; <http://memoria.ai/onto/supersedes> <http://memoria.ai/onto/docB> .
"#;

/// 定位 open-ontologies 二进制；缺失则 None（测试跳过）。
/// 仅当显式设置了 OPEN_ONTOLOGIES_BIN 且路径存在、或二进制确定在 PATH 上时才返回 Some，
/// 避免在无二进制的环境（如 CI runner）误判后 spawn 失败。
///
/// 设计不变式（why，非过程记录）：
/// - 所有探测（env 路径 --help、PATH 遍历）都带超时，避免挂死的二进制 / 无响应的 PATH
///   探测卡住测试进程（与主模块"子进程必须超时"纪律一致）。
/// - 权威配置 > 隐式回退：显式设置 OPEN_ONTOLOGIES_BIN 即表达"必须用它"，校验失败（非文件/
///   --help 探测失败/超时）绝不回退 PATH 扫描，由 find_bin 硬失败——防止 stale/误配的 pinned
///   路径被 PATH 上另一个二进制掩盖，让套件假绿通过而不跑真实物化。
/// - FAIL OPEN 关闭：`REQUIRE_ONTOLOGIES_BIN=1` 时找不到二进制直接 panic（CI 硬要求），而非
///   静默 skip——防止 CI 上"测试绿了但实际没跑真实物化"的假绿通过门禁。
fn find_bin() -> Option<String> {
    // REQUIRE_ONTOLOGIES_BIN 接受常见真值（1/true/yes/on）；空/"0"/"false"/"no"/"off" 视为未要求；
    // **任何已设置的其它值都视为要求**（fail-closed）——CI typo（"ture"）或尾随空格若静默当 unset
    // 会让"找不到二进制就 panic"的门禁悄悄失效、套件静默 skip（防假绿工具自身成了假绿源）。
    // 用 var_os + lossy 区分"未设置"(None) 与"设为非 UTF-8 值"(Some)——后者按 fail-closed 处理，
    // 与下方 OPEN_ONTOLOGIES_BIN 的 var_os 检测语义一致。
    let require = match std::env::var_os("REQUIRE_ONTOLOGIES_BIN") {
        None => false,
        Some(v) => {
            let t = v.to_string_lossy().trim().to_ascii_lowercase();
            match t.as_str() {
                "" | "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                other => {
                    eprintln!(
                        "WARN: REQUIRE_ONTOLOGIES_BIN={:?} unrecognized, treating as REQUIRED \
                         (fail-closed: unrecognized value must not silently disable the gate)",
                        other
                    );
                    true
                }
            }
        }
    };
    // #R19（第19轮 maintainability/low）：OPEN_ONTOLOGIES_BIN **只读一次**（此处），沿调用链传给
    // locate_bin，不再在 find_bin/locate_bin 各读一次——否则 `let found = locate_bin()` 先执行、
    // explicit_bin 后读，两次观测可发散（其它线程改 env），check-then-act 窗口复燃。
    let env_bin = std::env::var_os("OPEN_ONTOLOGIES_BIN");
    let found = locate_bin(&env_bin);
    // 显式设置 OPEN_ONTOLOGIES_BIN 的 CI，若路径无效（非文件/--help 探测失败/超时），套件必须
    // 在 REQUIRE=1 之外也硬失败——否则一个 stale/误配的 pinned 路径会让套件静默绿过而不跑真实
    // 物化。显式设置本身即表达"必须用这个二进制"，校验失败即 panic（权威配置>隐式回退）。
    // "是否设置"用 var_os 判定（`.is_some()` 而非 `!is_empty()`）：空串（CI 模板未填、export 后
    // 未赋值）也表达"必须用它"，校验必失败（is_file false）走下方硬失败，避免静默 skip 复燃假绿。
    let explicit_bin = env_bin.is_some();
    if found.is_none() && explicit_bin {
        panic!(
            "OPEN_ONTOLOGIES_BIN is set but invalid (not a file or --help probe failed); \
             refusing to silently skip materialization tests"
        );
    }
    if found.is_none() && require {
        panic!(
            "REQUIRE_ONTOLOGIES_BIN=1 but open-ontologies binary not found \
             (set OPEN_ONTOLOGIES_BIN or ensure it's on PATH)"
        );
    }
    found
}

fn locate_bin(env_bin: &Option<std::ffi::OsString>) -> Option<String> {
    // 设计不变式：
    // 1. 权威 override：显式设置 OPEN_ONTOLOGIES_BIN 即表达"必须用它"，校验失败（非文件/
    //    --help 探测失败/超时）绝**不**回退 PATH 扫描，由 find_bin 硬失败（防 stale pinned
    //    路径被 PATH 上另一个二进制掩盖造成假绿）。
    // 2. 用 var_os 检测"是否设置"而非 var().ok()——后者对非 UTF-8 值与"未设置"无法区分，
    //    会静默回退重新打开假绿。非 UTF-8 值经 to_string_lossy 参与校验，仍权威失败。
    // 3. 环境只读一次：probe_timeout 与 OPEN_ONTOLOGIES_BIN 均在入口读一次，沿调用链传参，
    //    消除 find_bin/locate_bin 双读与循环内重复 var_os 造成的 check-then-act 窗口。
    let pt = probe_timeout();
    if let Some(b_os) = env_bin {
        let b = b_os.to_string_lossy();
        // 空串与其它无效值同语义——一旦 set 即"必须用它"，is_file 必 false，校验失败返回 None，
        // 由 find_bin 硬失败（拒绝静默跳过），绝不回退 PATH 掩盖假绿。
        if b.is_empty() {
            return None;
        }
                // env 值先做 is_file() 校验 + 可执行/身份探测（--help 能 spawn 且输出确认是 open-ontologies），
        // 失败即权威地返回 None，避免后续 materialize(...).expect(...) 因 spawn 失败而 panic 或跑错
        // 二进制（#R3-1）。explicit 路径本就 fail-closed，`probe_bin` 的身份校验保证它不是同名其它工具。
        if std::path::Path::new(&*b).is_file() && probe_bin(&b, pt) {
            return Some(b.into_owned());
        }
        return None; // 显式设置但校验失败 → 权威失败，不回退 PATH
    }
    // 未显式设置：才走 PATH 遍历。不在 PATH 上则 None（测试跳过）。
    // 不用 `where`/`which`：Windows 控制台代码页（GBK）会让非 ASCII 安装路径乱码误判"未找到"，
    // 且 `where` 会顺带搜 CWD 可能选中过时文件。直接遍历 split_paths(PATH) 并对候选探测。
    let path_var = std::env::var_os("PATH")?;
    // PATH 探测需**整体预算**：单个挂死候选阻塞 N×10s（N=PATH 条目数），与"所有探测必须带
    // 超时"只约束单次、不约束整体相矛盾。用共享 deadline 循环内检查，到点放弃返回 None。
    let scan_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for dir in std::env::split_paths(&path_var) {
        // #2（第10轮 bug/low）：split_paths 对空 PATH 条目（前导/尾随/连续冒号）产出空分量，
        // 在 POSIX 语义上表示 CWD。`dir.join("open-ontologies")` 对空 dir 得到相对路径
        // `open-ontologies`，`is_file()` 会按进程 CWD 解析——恰好选中 CWD 里过时/无关的同名
        // 二进制，违背"不搜 CWD"承诺。显式跳过空分量。
        if dir.as_os_str().is_empty() {
            continue;
        }
        if std::time::Instant::now() > scan_deadline {
            return None; // 整体探测超时，放弃扫描（REQUIRE=1 下 find_bin panic）
        }
        let exe = if cfg!(windows) {
            dir.join("open-ontologies.exe")
        } else {
            dir.join("open-ontologies")
        };
        // #1（第8轮 bug/medium）：`to_str()?` 在循环内对非 UTF-8 路径会**中止整个扫描**
        // 返回 None，而非跳过该候选继续后续 PATH 项——这与"重写避免 where 的编码坑"的
        // 意图直接矛盾（首个非 UTF-8 PATH 目录即触发假绿 skip 或 REQUIRE=1 假红）。
        // 改用 `if let` 只跳过该候选，继续扫描后续目录。
        if exe.is_file() {
            // #7（第11轮 other/low → 第12轮 bug/low）：deadline 只在 PATH 循环顶检查，但单个
            // 挂死候选在 deadline 前一刻被发现仍可耗尽整个 PROBE_TIMEOUT（默认 10s），使注释
            // 宣称的"整体探测超时，放弃扫描"硬预算形同虚设。第12轮把**剩余预算**传给 probe_help
            // （deadline=min(PROBE_TIMEOUT, 剩余)），探测不会超过剩余预算，30s 整体预算成为真实上界。
            // #2（第13轮 bug/low）：deadline 刚过时 remaining≈0，cap=min(PROBE_TIMEOUT, 0)≈0，
            // probe_help 仍会 spawn 子进程然后在首次 deadline 检查立即 kill——纯因边界竞态误拒
            // 健康二进制（spurious skip 或 REQUIRE=1 下假 panic）。剩余预算过小时跳过探测，不
            // spawn 一个必然被杀的进程。
            let remaining = scan_deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.as_millis() < 100 {
                return None; // 剩余预算不足，放弃扫描
            }
            if let Some(s) = exe.to_str() {
                // #R17：budget = min(单次探测超时, 剩余整体预算)，与 probe_bin 的 deadline 语义一致
                // PATH 候选仅凭"可执行"不足以确认它就是 open-ontologies——一个更早 PATH 上的
                // stale/同名二进制会静默被选中，违背本文件中心的防假绿意图。`probe_bin` 在 --help
                // 探测的同时校验输出含 open-ontologies 标识，身份不符即拒绝该候选。
                if probe_bin(s, pt.min(remaining)) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// 探活子进程超时（秒）。
/// #3（第6轮 test/low）：比实际 materialize 的 timeout_secs(60) 小的固定 3s，在慢机器/冷缓存/
/// Defender 扫描下会把"反应慢但正常"的二进制误判为缺失（假绿跳过 或 REQUIRE=1 假红）。放大到
/// 10s 并可用 PROBE_TIMEOUT_SECS env 覆盖，兼顾 CI 慢 runner 与本地快速失败。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 探测超时硬上界（秒）。镜像 PATH 扫描的 30s 整体预算（scan_deadline），防止误配的
/// `PROBE_TIMEOUT_SECS`（如 3600）配合挂死的子进程把测试套件拖住几个小时。
/// #R14（第14轮 test/low）：`probe_timeout()` 此前无上界，`OPEN_ONTOLOGIES_BIN` 分支把完整值当
/// `budget` 传入 `probe_help`，而 30s `scan_deadline` 只守卫扫描循环——clamp 只处理 0→1s 无最大。
const PROBE_TIMEOUT_MAX_SECS: u64 = 30;

fn probe_timeout() -> std::time::Duration {
    // #8（第11轮 test/low + 第12轮 other/low）：与生产 `OntologyConfig::from_env` 约定一致——
    // 显式 `PROBE_TIMEOUT_SECS=0` 视为误配，**clamp 到 1s**（拒绝 0 导致的"所有探测立即失败、
    // 测试静默跳/panic"），而非回退到 10s 默认。此前实现用 `.filter(|n| *n > 0)` 回退默认，
    // 与生产的 clamp（0→1s）语义不一致，注释宣称"统一"却未真正统一——改为真正的 clamp。
    // #R14（第14轮 test/low）：补上界 clamp（>30s 拉回 30s）并在不可解析/超范围时 `eprintln!`
    // 告警，而非静默回退——镜像生产 `from_env` 的 WARN 行为，避免"配了长超时实际没生效"的误导。
    // #R26（第25轮 bug/low）：`std::env::var` 的 `Err(_)` 无法区分"未设置"(NotPresent) 与
    // "设为非 UTF-8 值"(NotUnicode)——后者会静默落到最后分支回退默认、不带 WARN，与 doc 注释
    // 宣称的"不可解析值告警"相悖，也和文件其它 env（REQUIRE_ONTOLOGIES_BIN/OPEN_ONTOLOGIES_BIN）
    // 用 `var_os` 区分 set-非UTF-8 与 unset 的 fail-closed 语义不一致。改用 `var_os`：`None`(未设置)
    // → 默认；`Some(非UTF-8)` → WARN + 回退默认（"set 了但读不出"是异常，必须告警而非装没看见）。
    match std::env::var_os("PROBE_TIMEOUT_SECS") {
        None => PROBE_TIMEOUT,
        Some(os) => {
            let v = os.to_string_lossy();
            match v.parse::<u64>() {
                Ok(n) if n == 0 => {
                    eprintln!("WARN: PROBE_TIMEOUT_SECS={:?} is 0/invalid, clamped to 1s", v);
                    std::time::Duration::from_secs(1)
                }
                Ok(n) if n > PROBE_TIMEOUT_MAX_SECS => {
                    eprintln!(
                        "WARN: PROBE_TIMEOUT_SECS={:?} exceeds hard max {}s, clamped to {}s",
                        v, PROBE_TIMEOUT_MAX_SECS, PROBE_TIMEOUT_MAX_SECS
                    );
                    std::time::Duration::from_secs(PROBE_TIMEOUT_MAX_SECS)
                }
                Ok(n) => std::time::Duration::from_secs(n),
                Err(_) => {
                    eprintln!(
                        "WARN: PROBE_TIMEOUT_SECS={:?} (non-UTF-8 or unparseable), falling back to default {}s",
                        v, PROBE_TIMEOUT.as_secs()
                    );
                    PROBE_TIMEOUT
                }
            }
        }
    }
}

/// 带超时的可执行 + 身份探测（--help）。spawn 失败、超时、非零退出、stdout 不含 open-ontologies
/// 标识，都视为不可用。`budget` 为调用方传入的**剩余整体探测预算**，本次探测的 deadline = budget
/// （调用方已按 `probe_timeout().min(剩余预算)` 语义给出，见 find_bin/locate_bin）。
/// 设计要点（#R26 第25轮 bug/medium + other/low 合并修复）：
/// 1. stdout 重定向到**临时文件**而非管道——候选二进制若 spawn 了继承 stdout 写端的孙进程，
///    管道 EOF 永远不来、`read_to_end` 无界阻塞会挂死整个测试；文件读总是有界（读到子进程已写的
///    内容即可），孙进程持写端也不阻塞。
/// 2. 身份校验与可执行探测合并：`--help` 输出必含 clap 的 `name = "open-ontologies"`，子串匹配
///    `open-ontologies`（大小写不敏感）即确认"真的是 open-ontologies"，无需额外 `--version`
///    探测（v1.1.1 的 CLI 未注册 version 子命令，`--version` 会非零退出误拒真实二进制）。
/// 3. 先 wait 再读：轮询 try_wait 拿退出状态，退出后读文件；超时 kill。
fn probe_bin(bin: &str, budget: std::time::Duration) -> bool {
    // stdout 写临时文件：进程退出后读文件内容做身份校验。NamedTempFile 随机名 + 独占创建，
    // 与测试其它临时资源一致；drop 自动清理。
    let mut out_file = match tempfile::Builder::new()
        .prefix("memoria_onto_probe_")
        .tempfile()
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    // 子进程写 stdout 到 out_file 的文件句柄；父进程保留 out_file 待进程退出后读。
    let out_handle = match out_file.reopen() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let mut child = match std::process::Command::new(bin)
        .arg("--help")
        .stdout(std::process::Stdio::from(out_handle))
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + budget;
    loop {
        match child.try_wait() {
            // --help 非零退出也视为不可用（此前任何退出码都返回 true，与 doc 注释矛盾）。
            Ok(Some(st)) => {
                if !st.success() {
                    return false;
                }
                break;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // 进程已退出：读临时文件内容校验身份（文件读有界，不依赖管道 EOF）。
    use std::io::Read;
    let mut out = Vec::new();
    let _ = out_file.as_file().read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    let lower = text.to_ascii_lowercase();
    if !lower.contains("open-ontologies") {
        eprintln!(
            "WARN: candidate binary {bin:?} --help output lacks 'open-ontologies' identity: {text:?}"
        );
        return false;
    }
    true
}

/// RAII 临时目录守卫（#1 第12轮 security/low）：改用 `tempfile::TempDir`——随机名 + O_EXCL
/// 独占创建，消除此前 `memoria_onto_{pid}_{tag}` 可预测路径的符号链接 TOCTOU 与 PID 复用
/// 误删风险。drop 时自动清理，panic 也不泄漏临时文件。
/// `tag` 作目录名前缀，便于测试失败时定位。
fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("memoria_onto_{tag}_"))
        .tempdir()
        .expect("create unique temp dir")
}

/// #4（第10轮 maintainability/low）：materialize 相关集成测试共享的 fixture 搭建——
/// 定位二进制（缺失则跳过）+ 写 schema/data TTL + 构造 OntologyConfig。此前两个测试
/// 各自重复这段，fixture 或 skip 行为一变就得两处同步改，容易漂移。
/// 返回 `(guard, cfg, data_path)`；guard 保持 TempDir 存活到测试结束。
fn setup_bin_and_fixtures(
    tag: &str,
) -> Option<(tempfile::TempDir, OntologyConfig, std::path::PathBuf)> {
    let bin = find_bin()?; // 缺失则 None → 调用方 SKIP
    let guard = temp_dir(tag);
    let dir = guard.path().to_path_buf();
    let schema = dir.join("schema.ttl");
    let data_path = dir.join("data.ttl");
    std::fs::write(&schema, SCHEMA_TTL).unwrap();
    std::fs::write(&data_path, DATA_TTL).unwrap();
    let cfg = OntologyConfig {
        bin: bin.into(),
        data_dir: dir.join("out"),
        schema_path: schema,
        timeout_secs: 60,
    };
    Some((guard, cfg, data_path))
}

/// #4（第10轮 maintainability/low）：共享的"docC supersedes docA 被推断"精确 IRI 断言。
/// 用**精确 IRI 相等**而非 suffix 匹配，避免 `s.ends_with("docC")` 误接受 `.../notdocC`
/// 等错误 IRI，让 URI/存储格式回归在此暴露（#2 第5轮 test/low）。
fn assert_inferred_doc_c_supersedes_doc_a(inferred: &[(String, String, String)]) {
    assert!(
        inferred
            .iter()
            .any(|(s, p, o)| {
                s == "http://memoria.ai/onto/docC"
                    && p == "http://memoria.ai/onto/supersedes"
                    && o == "http://memoria.ai/onto/docA"
            }),
        "expected docC supersedes docA in inferred_edges, got {:?}",
        inferred
    );
}

/// #4（第10轮 maintainability/low）：共享的"在库中查到 docC supersedes docA"断言。
fn assert_persisted_doc_c_supersedes_doc_a(conn: &rusqlite::Connection, ns: &str) {
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges
             WHERE namespace=?1 AND relation_type='supersedes'
               AND source_entity_id='http://memoria.ai/onto/docC'
               AND target_entity_id='http://memoria.ai/onto/docA'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        found == 1,
        "inferred edge docC supersedes docA must persist exactly once in entity_edges (got {found})"
    );
}

#[test]
fn materialize_infers_transitive_supersedes() {
    let (_guard, cfg, data_path) = match setup_bin_and_fixtures("materialize") {
        Some(x) => x,
        None => {
            eprintln!(
                "SKIP: open-ontologies binary unavailable (set OPEN_ONTOLOGIES_BIN, \
                 or REQUIRE_ONTOLOGIES_BIN=1 to fail instead of skip)"
            );
            return;
        }
    };
    // #R19（第19轮 other/low）：fixture 路径本地生成，用 `to_str().expect` 显式失败而非
    // `to_string_lossy()` 静默替换非 UTF-8 字节（否则 materialize 会打开不同的、不存在的文件，
    // 报令人困惑的 "read source ttl" 错误）。变量名 data_path 明确其为路径而非 TTL 内容。
    let res = materialize(&cfg, data_path.to_str().expect("fixture path must be UTF-8"), "owl-rl")
        .expect("materialize");
    // OWL TransitiveProperty：docC supersedes docA 应被推断
    assert!(res.triples_after > res.triples_before, "expected inference");
    assert_inferred_doc_c_supersedes_doc_a(&res.inferred_edges);
    // #5（第11轮 test/medium）：必须**负断言**显式源边不在 inferred_edges。DATA_TTL 显式声明
    // 的显式边是 5 条：docB/docC supersedes、docA/docB/docC createdBy（map_relation_iri 会把
    // createdBy 也提取，且 docB/docC 的 createdBy 位于 `;` 前的多谓词行上）。若 parse_ttl_edges
    // 提取 source_edges 失败（前缀/完整 IRI 不匹配、`;` 续行回归等），集合差 `materialized -
    // source_edges` 会把**任一条**显式边误算进 inferred_edges——正是 #123 完整 IRI fixture 要防
    // 的场景，但此测试此前只断言"有 docC supersedes docA"仍会通过。负列表须穷尽全部 5 条显式边，
    // 才能拦住"`;` 续行回归把部分显式边当推断"的完整回归面。
    for (s, p, o) in &[
        ("http://memoria.ai/onto/docB", "http://memoria.ai/onto/supersedes", "http://memoria.ai/onto/docA"),
        ("http://memoria.ai/onto/docC", "http://memoria.ai/onto/supersedes", "http://memoria.ai/onto/docB"),
        ("http://memoria.ai/onto/docA", "http://memoria.ai/onto/createdBy", "http://memoria.ai/onto/alice"),
        ("http://memoria.ai/onto/docB", "http://memoria.ai/onto/createdBy", "http://memoria.ai/onto/alice"),
        ("http://memoria.ai/onto/docC", "http://memoria.ai/onto/createdBy", "http://memoria.ai/onto/alice"),
    ] {
        assert!(
            !res.inferred_edges.iter().any(|(s2, p2, o2)| s2 == s && p2 == p && o2 == o),
            "explicit source edge ({}, {}, {}) must NOT be in inferred_edges — parse_ttl_edges regression?",
            s, p, o
        );
    }
    println!("inferred_edges: {:?}", res.inferred_edges);
}

#[test]
fn write_back_edges_upserts_into_entity_edges() {
    // 不依赖 open-ontologies 二进制，直接测写回逻辑。
    let _guard = temp_dir("writeback");
    let dir = _guard.path();
    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(db.to_str().expect("temp path must be UTF-8")).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";

    // 模拟物化推断出的边（docC supersedes docA, docA supersedes docB）
    let edges = vec![
        (
            "http://memoria.ai/onto/docC".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docA".to_string(),
        ),
        (
            "http://memoria.ai/onto/docA".to_string(),
            "http://memoria.ai/onto/supersedes".to_string(),
            "http://memoria.ai/onto/docB".to_string(),
        ),
        // 未知关系应被跳过
        (
            "http://memoria.ai/onto/docC".to_string(),
            "http://memoria.ai/onto/banana".to_string(),
            "http://memoria.ai/onto/docA".to_string(),
        ),
    ];

    let (written, skipped) = write_back_edges(&conn, ns, &edges).expect("write_back");
    assert_eq!(written, 2, "exactly 2 supersedes edges written");
    assert_eq!(skipped, 1, "1 unknown banana edge skipped");

    // #6（第11轮 test/low）：`skipped==1` 不足以证明 allowlist 门禁生效——一个同时插入行又
    // 递增 skipped（或误报 affected=0）的门禁回归仍会通过。必须查库确认未知 banana 边**确实
    // 未持久化**，否则允许列表不变式被破坏却测试仍绿。
    let banana_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges WHERE namespace=?1 AND relation_type='banana'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        banana_count, 0,
        "unknown banana edge must NOT be persisted by write_back_edges"
    );

    // 幂等：重复写回不重复。#R27（第27轮 test/low）：不再断言 `w2 == 2`——那是 `write_back_edges`
    // **内部受影响行计数**的实现细节（如未来重构为 dedupe-before-write / INSERT OR IGNORE，不再
    // 计数 no-op 更新时该断言会假失败，尽管幂等语义仍成立）。幂等的真正契约是 DB 终态行数，
    // 只断言库状态（下方 count==2）即可。
    let (_w2, _) = write_back_edges(&conn, ns, &edges[..2]).expect("write_back again");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges WHERE namespace=?1 AND relation_type='supersedes'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "idempotent upsert, no duplicates");
}

/// 完整闭环：物化 → 写回 entity_edges，验证推断边真正进库。
#[test]
fn end_to_end_materialize_and_writeback() {
    let (guard, cfg, data_path) = match setup_bin_and_fixtures("e2e") {
        Some(x) => x,
        None => {
            eprintln!(
                "SKIP: open-ontologies binary unavailable (set OPEN_ONTOLOGIES_BIN, \
                 or REQUIRE_ONTOLOGIES_BIN=1 to fail instead of skip)"
            );
            return;
        }
    };
    let dir = guard.path().to_path_buf();
    // #R19：同 materialize_infers_transitive_supersedes——to_str().expect 显式失败，非静默替换。
    let res =
        materialize(&cfg, data_path.to_str().expect("fixture path must be UTF-8"), "owl-rl")
            .expect("materialize");

    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(db.to_str().expect("temp path must be UTF-8")).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";
    let (written, _) = write_back_edges(&conn, ns, &res.inferred_edges).expect("write_back");
    // #R15（第15轮 test/low）：必须断言**精确计数**而非 `>=1`。DATA_TTL 的传递闭包只应推断
    // 恰好 1 条新 supersedes 边（docC supersedes docA）；重复插入或部分写入回归（如同一推断边
    // 写两次）会通过 `>=1` 却违反精确语义。查库数 supersedes 边数须 ==1，与单测级严格度一致。
    assert_eq!(written, 1, "exactly 1 inferred supersedes edge written (DATA_TTL closure)");
    let supersedes_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_edges WHERE namespace=?1 AND relation_type='supersedes'",
            params![ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(supersedes_count, 1, "exactly 1 supersedes edge persisted in entity_edges");

    // 验证**具体推断边** docC supersedes docA 在库里（#123/#R3-10）：
    // 因 DATA_TTL 用完整 IRI，inferred_edges 只含真正新增的推断边。
    assert_persisted_doc_c_supersedes_doc_a(&conn, ns);
    println!("end-to-end: written={written}");
}
