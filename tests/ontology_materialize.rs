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
/// #R4-6：所有探测（env 路径 --help、`which`/`where`）都带超时，避免挂死的二进制
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
    // env 值先做 is_file() 校验 + 可执行探测（--help 能 spawn），缺失/不可执行则跳过，
    // 避免后续 materialize(...).expect(...) 因 spawn 失败而 panic（#R3-1）。
    if let Ok(b) = std::env::var("OPEN_ONTOLOGIES_BIN") {
        if !b.is_empty()
            && std::path::Path::new(&b).is_file()
            && probe_help(&b)
        {
            return Some(b);
        }
    }
    // 用 `which`/`where` 探测 PATH。输出可能多行（同名二进制在多个 PATH 条目），
    // 只取第一条匹配路径；找不到则视为无二进制 → 跳过。
    // #1（第5轮 test/medium）：PATH 分支此前只查 is_file() 不跑 probe_help，且 which/where
    // 本身无超时——stale/不可执行的 open-ontologies 会通过 find_bin，后期 materialize() 才 panic，
    // 违背 #R3-1"应跳过而非崩溃"；which/where 挂死也会卡死测试（违背 #R4-6"所有探测带超时"）。
    // 故：对 which/where 用带超时的 probe_locate 封装，并对定位到的路径再跑一次 probe_help。
    let locator = if cfg!(windows) { "where" } else { "which" };
    let out = probe_locate(locator)?;
    let stdout = String::from_utf8_lossy(&out);
    stdout
        .lines()
        .map(str::trim)
        .find(|s| {
            !s.is_empty() && std::path::Path::new(s).is_file() && probe_help(s)
        })
        .map(str::to_string)
}

/// 带超时的 `which`/`where` 探测（#1）。spawn 失败 / 非零退出 / 超时都返回 None。
fn probe_locate(locator: &str) -> Option<Vec<u8>> {
    let mut child = std::process::Command::new(locator)
        .arg("open-ontologies")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                if !st.success() {
                    let _ = child.wait();
                    return None;
                }
                use std::io::Read;
                let mut buf = Vec::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_end(&mut buf);
                }
                return Some(buf);
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// 带超时的 `--help` 可执行探测（#R4-6）。spawn 失败、超时、非零退出都视为不可用。
/// 用 `std::process::Child::wait_timeout` 无法直接获得（std 无此 API），故用
/// `spawn` + 轮询 `try_wait` + 超时 kill 的既有模式。
fn probe_help(bin: &str) -> bool {
    let mut child = match std::process::Command::new(bin)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
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

/// RAII 临时目录守卫（#123）：drop 时自动清理，panic 也不泄漏临时文件。
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("memoria_onto_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn materialize_infers_transitive_supersedes() {
    let bin = match find_bin() {
        Some(b) => b,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let _guard = TempDir::new("materialize");
    let dir = _guard.path();
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

    let res = materialize(&cfg, &data.to_string_lossy(), "owl-rl").expect("materialize");
    // OWL TransitiveProperty：docC supersedes docA 应被推断
    assert!(res.triples_after > res.triples_before, "expected inference");
    // #2（第5轮 test/low）：与 e2e 测试一致，用**精确 IRI 相等**而非 suffix 匹配断言推断边，
    // 避免 `s.ends_with("docC")` 误接受 `.../notdocC` 等错误 IRI，让 URI/存储格式回归在此暴露。
    assert!(
        res.inferred_edges
            .iter()
            .any(|(s, p, o)| {
                s == "http://memoria.ai/onto/docC"
                    && p == "http://memoria.ai/onto/supersedes"
                    && o == "http://memoria.ai/onto/docA"
            }),
        "expected docC supersedes docA in inferred_edges, got {:?}",
        res.inferred_edges
    );
    println!("inferred_edges: {:?}", res.inferred_edges);
}

#[test]
fn write_back_edges_upserts_into_entity_edges() {
    // 不依赖 open-ontologies 二进制，直接测写回逻辑。
    let _guard = TempDir::new("writeback");
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
    let bin = match find_bin() {
        Some(b) => b,
        None => {
            eprintln!("SKIP: OPEN_ONTOLOGIES_BIN not found");
            return;
        }
    };
    let _guard = TempDir::new("e2e");
    let dir = _guard.path();
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
    let res = materialize(&cfg, &data.to_string_lossy(), "owl-rl").expect("materialize");

    let db = dir.join("mem.db");
    let engine = memoria_core::MemoriaEngine::new(&db.to_string_lossy()).expect("engine");
    let conn = engine.pool.get().unwrap();
    let ns = "agent/onto";
    let (written, _) = write_back_edges(&conn, ns, &res.inferred_edges).expect("write_back");
    assert!(written >= 1, "at least 1 inferred edge written");

    // 验证**具体推断边** docC supersedes docA 在库里（#123/#R3-10）：
    // 因 DATA_TTL 用完整 IRI，inferred_edges 只含真正新增的推断边；
    // 用精确 IRI 相等（而非 LIKE）钉死实体 id 存完整 IRI 的契约，若传递推理或
    // 存储格式回归则失败。
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
    println!("end-to-end: written={written}, persisted docC-supersedes-docA edges={found}");
}
