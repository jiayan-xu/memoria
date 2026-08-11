//! Open Ontologies 形态 B 骨架 — 端到端集成测试。
//!
//! 验证：物化（load schema+数据 → reason → save）→ 解析推断边 → 写回 entity_edges 完整闭环。
//!
//! 运行：`cargo test --test ontology_materialize`
//! 前置：OPEN_ONTOLOGIES_BIN 指向 open-ontologies 可执行文件；缺失时测试自动跳过。
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
/// #R4-6：所有探测（env 路径 --help、PATH 遍历）都带超时，避免挂死的二进制
/// / 无响应的 PATH 探测卡住测试进程（与主模块"子进程必须超时"规矩一致）。
/// #R4-7：设置 `REQUIRE_ONTOLOGIES_BIN=1` 时，找不到二进制直接 panic（CI 硬要求），
/// 而不是静默 skip——防止 CI 上"测试绿了但实际没跑真实物化"的假绿通过门禁。
fn find_bin() -> Option<String> {
    let require = std::env::var("REQUIRE_ONTOLOGIES_BIN").map(|v| v == "1").unwrap_or(false);
    let found = locate_bin();
    if found.is_none() && require {
        panic!(
            "REQUIRE_ONTOLOGIES_BIN=1 but open-ontologies binary not found \
             (set OPEN_ONTOLOGIES_BIN or ensure it's on PATH)"
        );
    }
    found
}

fn locate_bin() -> Option<String> {
    // #1（第9轮 maintainability/low）：`OPEN_ONTOLOGIES_BIN` 是显式 override，一旦设置就必须
    // 权威——若校验失败（非文件 / --help 探测失败或超时），直接返回 None（REQUIRE=1 下
    // find_bin 会 panic），**绝不静默回退到 PATH 扫描**。否则 CI 里一个 stale/损坏的 pinned
    // 路径会被 PATH 上恰好存在的另一个 open-ontologies 二进制掩盖，产出"跑错了可执行文件"
    // 的假绿（或对错误版本报错，令人困惑）。
    // #1（第10轮 bug/low）：必须用 `var_os`（OsString）检测"是否设置"，而非 `var().ok()`——
    // 后者对非 UTF-8 值返回 `Err(NotUnicode)` → `.ok()` 映射为 None，与"未设置"无法区分，
    // 会静默回退到 PATH 扫描，恰好重新打开上面要防的假绿场景。用 var_os 后，非 UTF-8 值用
    // to_string_lossy 也能参与校验，且依旧权威失败不回退。
    if let Some(b_os) = std::env::var_os("OPEN_ONTOLOGIES_BIN") {
        let b = b_os.to_string_lossy();
        if b.is_empty() {
            return None; // 显式空值 = 明确禁用，不回退
        }
        // env 值先做 is_file() 校验 + 可执行探测（--help 能 spawn），失败即权威地返回 None，
        // 避免后续 materialize(...).expect(...) 因 spawn 失败而 panic 或跑错二进制（#R3-1）。
        if std::path::Path::new(&*b).is_file() && probe_help(&b, probe_timeout()) {
            return Some(b.into_owned());
        }
        return None; // 显式设置但校验失败 → 权威失败，不回退 PATH
    }
    // 未显式设置：才走 PATH 遍历。不在 PATH 上则 None（测试跳过）。
    // #2（第7轮 bug/low）：不再 shell 到 `where`/`which`——(a) Windows 的 `where` 用控制台
    // 代码页（如 GBK）输出，非 ASCII 安装路径会被 from_utf8_lossy 乱码，误判"未找到"；
    // (b) `where` 会顺带搜 CWD，可能选中过时/无关的同名文件。改为直接遍历 `split_paths(PATH)`
    // 并对每个候选 dir 里的 `open-ontologies(.exe)` 用 probe_help 探测（无编码问题、不搜 CWD）。
    let path_var = std::env::var_os("PATH")?;
    // #3（第10轮 performance/low）：PATH 探测需要**整体预算**，否则单个挂死候选阻塞 N×10s
    // （N = PATH 条目数），与文件自身"#R4-6 所有探测必须带超时"的规则只约束了单次探测、
    // 未约束整体相矛盾。用共享 deadline 在循环内检查，到点即放弃扫描返回 None（REQUIRE=1 下
    // find_bin panic，CI 不会无限等）。
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
            let remaining = scan_deadline.saturating_duration_since(std::time::Instant::now());
            if let Some(s) = exe.to_str() {
                if probe_help(s, remaining) {
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

fn probe_timeout() -> std::time::Duration {
    // #8（第11轮 test/low + 第12轮 other/low）：与生产 `OntologyConfig::from_env` 约定一致——
    // 显式 `PROBE_TIMEOUT_SECS=0` 视为误配，**clamp 到 1s**（拒绝 0 导致的"所有探测立即失败、
    // 测试静默跳/panic"），而非回退到 10s 默认。此前实现用 `.filter(|n| *n > 0)` 回退默认，
    // 与生产的 clamp（0→1s）语义不一致，注释宣称"统一"却未真正统一——改为真正的 clamp。
    std::env::var("PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|n| std::time::Duration::from_secs(n.max(1)))
        .unwrap_or(PROBE_TIMEOUT)
}

/// 带超时的 `--help` 可执行探测（#R4-6）。spawn 失败、超时、非零退出都视为不可用。
/// 用 `std::process::Child::wait_timeout` 无法直接获得（std 无此 API），故用
/// `spawn` + 轮询 `try_wait` + 超时 kill 的既有模式。
/// #2（第12轮 bug/low）：`budget` 为调用方传入的**剩余整体探测预算**；本次探测的 deadline =
/// `min(probe_timeout, budget)`。这样 PATH 扫描中在 scan_deadline 前一刻发现的候选，其探测
/// 不会超过剩余预算，把 30s 整体预算做成真实硬上界（此前探测只要在 deadline 前启动就跑满
/// 整个 PROBE_TIMEOUT，最坏 ~40s，注释宣称的"硬预算"名不副实）。
fn probe_help(bin: &str, budget: std::time::Duration) -> bool {
    let mut child = match std::process::Command::new(bin)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let cap = probe_timeout().min(budget);
    let deadline = std::time::Instant::now() + cap;
    loop {
        match child.try_wait() {
            // #3（第5轮 test/low）：--help 非零退出也视为不可用（此前任何退出码都返回 true，
            // 与 doc 注释"非零退出视为不可用"矛盾）。
            Ok(Some(st)) => return st.success(),
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
    let data = dir.join("data.ttl");
    std::fs::write(&schema, SCHEMA_TTL).unwrap();
    std::fs::write(&data, DATA_TTL).unwrap();
    let cfg = OntologyConfig {
        bin: bin.into(),
        data_dir: dir.join("out"),
        schema_path: schema,
        timeout_secs: 60,
    };
    Some((guard, cfg, data))
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
        found >= 1,
        "inferred edge docC supersedes docA must persist in entity_edges"
    );
}

#[test]
fn materialize_infers_transitive_supersedes() {
    let (_guard, cfg, data) = match setup_bin_and_fixtures("materialize") {
        Some(x) => x,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let res = materialize(&cfg, &data.to_string_lossy(), "owl-rl").expect("materialize");
    // OWL TransitiveProperty：docC supersedes docA 应被推断
    assert!(res.triples_after > res.triples_before, "expected inference");
    assert_inferred_doc_c_supersedes_doc_a(&res.inferred_edges);
    // #5（第11轮 test/medium）：必须**负断言**显式源边不在 inferred_edges。DATA_TTL 显式声明
    // docB supersedes docA、docC supersedes docB、docA createdBy alice 等；若 parse_ttl_edges 提取
    // source_edges 失败（前缀/完整 IRI 不匹配回归），集合差 `materialized - source_edges` 会把
    // 这些显式边误算进 inferred_edges——正是 #123 完整 IRI fixture 要防的场景，但此测试此前只
    // 断言"有 docC supersedes docA"仍会通过。显式断言显式边不在推断集中，才能拦住该回归。
    for (s, p, o) in &[
        ("http://memoria.ai/onto/docB", "http://memoria.ai/onto/supersedes", "http://memoria.ai/onto/docA"),
        ("http://memoria.ai/onto/docC", "http://memoria.ai/onto/supersedes", "http://memoria.ai/onto/docB"),
        ("http://memoria.ai/onto/docA", "http://memoria.ai/onto/createdBy", "http://memoria.ai/onto/alice"),
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
    let engine = memoria_core::MemoriaEngine::new(&db.to_string_lossy()).expect("engine");
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

    // 幂等：重复写回不重复
    let (w2, _) = write_back_edges(&conn, ns, &edges[..2]).expect("write_back again");
    assert_eq!(w2, 2);
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
    let (guard, cfg, data) = match setup_bin_and_fixtures("e2e") {
        Some(x) => x,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let dir = guard.path().to_path_buf();
    let res = materialize(&cfg, &data.to_string_lossy(), "owl-rl").expect("materialize");

    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(&db.to_string_lossy()).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";
    let (written, _) = write_back_edges(&conn, ns, &res.inferred_edges).expect("write_back");
    assert!(written >= 1, "at least 1 inferred edge written");

    // 验证**具体推断边** docC supersedes docA 在库里（#123/#R3-10）：
    // 因 DATA_TTL 用完整 IRI，inferred_edges 只含真正新增的推断边。
    assert_persisted_doc_c_supersedes_doc_a(&conn, ns);
    println!("end-to-end: written={written}");
}
