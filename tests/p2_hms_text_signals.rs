//! Phase P2 / M2.1 验收（HMS text_signals 最小切片）
//!   P2.1a: ledger 行含 text_signals（numbers/dates/update_markers）
//!   P2.1b: occurred tag 并入 dates 信号（O3，不写 event_time 列）
//!   P2.1c: hybrid 检索数字/日期 query 与正文重叠加成（O5，无 cross-encoder）
//!   P2.2c 已做: tags 持久化 signal:*（remember 写入 + ledger 读时合并）
//!   P2.2d 已做: agent-core consolidate LLM 抽取 signal tags
//!
//! 运行：`cargo test --test p2_hms_text_signals`

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::tools::profile::memory_context;
use memoria_core::tools::remember::remember_with_dedup;
use serde_json::Value;

fn fresh_engine(tag: &str) -> (MemoriaEngine, String) {
    let dir = std::env::temp_dir().join(format!("p2_hms_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = MemoriaEngine::new(dir.join("mem.db").to_str().unwrap()).expect("engine");
    (engine, "agent/hms_p2".to_string())
}

#[test]
fn ledger_includes_text_signals() {
    // #R63 test/medium：持 ENV_LOCK——本测试经 memory_context→hybrid_search→
    // text_signals_rerank_enabled() 读取 MEMORIA_TEXT_SIGNALS_RERANK；并行下与
    // env 变异测试的 set_var/remove_var 窗口重叠 = 数据竞争（set_var 在 edition
    // 2024 为 unsafe 正是为此）。
    let _env_guard = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
    let (engine, ns) = fresh_engine("ledger");
    let _ = remember_with_dedup(
        &engine.pool,
        "2026-07-10 进厂登记 120 吨，改为应急模式",
        "fact",
        3,
        "test",
        &ns,
        r#"["occurred:2026-07-10"]"#,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("120 吨 应急"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let recall = ctx["recall"].as_array().expect("recall");
    assert!(!recall.is_empty(), "应有 recall");
    let row = &recall[0];
    let ts = row
        .get("text_signals")
        .expect("P2.1a: ledger 应含 text_signals");
    let nums = ts["numbers"].as_array().expect("numbers");
    let dates = ts["dates"].as_array().expect("dates");
    let markers = ts["update_markers"].as_array().expect("update_markers");
    assert!(
        nums.iter().any(|n| n.as_str() == Some("120")),
        "numbers={:?}",
        nums
    );
    assert!(
        dates.iter().any(|d| d.as_str() == Some("2026-07-10")),
        "dates={:?}",
        dates
    );
    assert!(
        markers.iter().any(|m| m.as_str() == Some("改为")),
        "markers={:?}",
        markers
    );
}

// #R60：变异 MEMORIA_TEXT_SIGNALS_RERANK 的测试与读取它的测试共享串行锁
// （R33 曾移除 3 个"不依赖"测试的锁，#R63 加回——它们经 memory_context→
// hybrid_search 每次搜索都读该 env，并行下与 set_var 窗口重叠是数据竞争 UB）。
// #R64 performance/medium：**RwLock**——只读测试拿 read 锁可并发，仅变异测试
// （search_boosts / env_off）拿 write 锁串行（全 Mutex 让 5 个测试完全序列化）。
static ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// 共享 env 恢复 guard——Option<OsString> 无损快照 + Drop 恢复原值（含 unwind）；
/// search_boosts（pin enabled）与 env_off 共用。
/// **锁身份不变量**：`'a` 绑定 EnvWriteGuard（ENV_LOCK 专属 newtype，见下）——
/// 构造必须在锁作用域内（类型层面杜绝 drop 时无锁的 UB）。
/// #R69 maintainability/low：**newtype 强制锁身份**——此前 `&'a RwLockWriteGuard<
/// 'a, ()>` 对任何 `RwLock<()>` 完全透明：本二进制未来若新增第二把 `RwLock<()>`
/// static，其 guard 类型与此相同，pin_rerank 会无差别接受、误传后静默复现
/// 数据竞争（"依赖文件约定"与 soundness 注自相矛盾）。EnvWriteGuard 只能由
/// `ENV_LOCK.write()` 产生（构造点唯一），其他锁的 guard 无法构造本类型。
#[must_use = "EnvRestore::drop 负责恢复 env：必须具名绑定（let _pin = ...），禁止 let _ = 丢弃（语句末尾立即 drop，pin 变静默空操作）"]
struct EnvRestore<'a>(
    Option<std::ffi::OsString>,
    &'a EnvWriteGuard<'a>,
);

/// #R69 maintainability/low：ENV_LOCK 专属 write-guard newtype——类型层面区分
/// ENV_LOCK 与其他任意 `RwLock<()>`；仅由 `ENV_LOCK.write()` 包装构造。
struct EnvWriteGuard<'a>(std::sync::RwLockWriteGuard<'a, ()>);

impl EnvRestore<'_> {
    /// #R68 maintainability/medium：**唯一正确构造入口**——绑定 ENV_LOCK 的 write
    /// guard + 固定变量名（快照/置位/恢复三合一）；类型层面杜绝"借了别的锁的
    /// guard / 快照了别的变量"的误用（此前不变量只在 doc 注释里）。
    fn pin_rerank<'a>(guard: &'a EnvWriteGuard<'a>, value: &str) -> EnvRestore<'a> {
        let prev = std::env::var_os("MEMORIA_TEXT_SIGNALS_RERANK");
        // SAFETY: guard 是 ENV_LOCK.write() 的 newtype 包装（唯一构造点，类型
        // 强制）——本二进制内所有协作 env 变异经 ENV_LOCK 串行。
        // #R69 bug/medium（残余风险如实标注）：进程环境是单一全局资源（glibc
        // setenv 可能 realloc environ），Rust 2024 的安全条件要求 set_var/remove_var
        // 期间**无任何线程以任何方式访问进程环境**（包括读取**其他**变量）——
        // ENV_LOCK 只能串行化本二进制内相互协作的 env 访问，无法排除 libtest/
        // panic handler/第三方 C 代码对任意 env 的惰性并发读（如 RUST_BACKTRACE）。
        // 彻底消除 UB 需配合 RUST_TEST_THREADS=1（单线程测试）；默认并行 libtest
        // 下本操作在名义上不满足 edition 2024 契约，属测试专用、风险可控的
        // 已知妥协——维护者不得把"经 ENV_LOCK 串行"误读为完全安全。
        unsafe {
            std::env::set_var("MEMORIA_TEXT_SIGNALS_RERANK", value);
        }
        EnvRestore(prev, guard)
    }
}
impl Drop for EnvRestore<'_> {
    fn drop(&mut self) {
        // SAFETY: 锁（write）在 drop 时仍持有——guard 生命周期保证（self.1 存活）。
        unsafe {
            match &self.0 {
                Some(v) => std::env::set_var("MEMORIA_TEXT_SIGNALS_RERANK", v),
                None => std::env::remove_var("MEMORIA_TEXT_SIGNALS_RERANK"),
            }
        }
    }
}

#[test]
fn search_boosts_on_numeric_query_overlap() {
    // #R62 test/medium：**显式 pin enabled**——强化断言依赖 rerank 默认开启；环境
    // 预设 MEMORIA_TEXT_SIGNALS_RERANK=0（dev shell/CI）会让正确代码假红。
    // ENV_LOCK 只串行化本二进制内的变异，管不到环境预设。
    let _env_guard = EnvWriteGuard(ENV_LOCK.write().unwrap_or_else(|p| p.into_inner()));
    // #R63 maintainability/low：共享 guard（EnvRestore 同款，见下——防两份恢复
    // 语义静默分歧）。
    let _pin = EnvRestore::pin_rerank(&_env_guard, "1");
    let (engine, ns) = fresh_engine("rerank");
    let a = remember_with_dedup(
        &engine.pool,
        "仓库库存 120 吨，盘点正常",
        "fact",
        2,
        "test",
        &ns,
        "[]",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("a");
    let _b = remember_with_dedup(
        &engine.pool,
        "今日天气晴朗适合出行",
        "fact",
        2,
        "test",
        &ns,
        "[]",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("b");
    let mid_a = a.id.clone();

    let fused = hybrid_search(
        &engine.pool,
        "120 吨",
        &ns,
        10,
        None,
        None,
        None,
        None,
        false,
    )
    .expect("search");
    assert!(!fused.is_empty());
    let top = &fused[0];
    assert_eq!(top.memory_id, mid_a, "P2.1c: 数字重叠应抬升含 120 的记忆");
    // #R60 test/low：**只断言通道标记**——`rrf_score > 0.0` 是平凡断言（任何成功
    // 融合结果都有正 RRF，text_signals 零贡献也通过）；source 含 text_signals 仅
    // 在数字/日期 boost >0 实际应用时追加，P2.1c 回归必红。
    assert!(
        top.source.contains(memoria_core::search::text_signals::SOURCE_MARKER),
        "text-signal boost must be applied: source={}",
        top.source
    );
}

#[test]
fn relative_date_in_ledger_signals() {
    let _env_guard = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
    let (engine, ns) = fresh_engine("reldate");
    let _ = remember_with_dedup(
        &engine.pool,
        "上周三巡检发现异常，昨天已修复",
        "fact",
        3,
        "test",
        &ns,
        "[]",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("上周三 异常"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let recall = ctx["recall"].as_array().expect("recall");
    // #R67 test/low：显式非空守卫（裸 [0] 在空 recall 时 index-out-of-bounds）。
    assert!(!recall.is_empty(), "recall empty for relative-date memory");
    let row = &recall[0];
    let dates = row["text_signals"]["dates"].as_array().expect("dates");
    assert!(
        dates.len() >= 2,
        "P2.2a: 相对日期应解析为绝对日 dates={:?}",
        dates
    );
    assert!(
        dates
            .iter()
            .any(|d| d.as_str().map(|s| s.len() == 10).unwrap_or(false)),
        "dates={:?}",
        dates
    );
}

#[test]
fn text_signals_rerank_env_off() {
    // #R65 bug/medium：**锁在 fresh_engine 之前**——fresh_engine/remember_with_dedup
    // 读 env（MEMORIA_NEAR_DUP_ENABLED/PERSIST/POOL_SIZE），与变异测试的 set_var
    // 窗口重叠即 UB（edition 2024 set_var 的并发访问契约）。
    let _guard = EnvWriteGuard(ENV_LOCK.write().unwrap_or_else(|p| p.into_inner()));
    let (engine, ns) = fresh_engine("envoff");
    let _ = remember_with_dedup(
        &engine.pool,
        "唯一记忆 999 件",
        "fact",
        2,
        "test",
        &ns,
        "[]",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("mem");

    // #R60 test/medium：env 变异加 **Drop guard 恢复**——断言 panic 时 remove_var
    // 不执行则变量泄漏污染后续测试（guard 的 Drop 在 unwind 路径恢复）。
    // #R61 bug/medium：**恢复原值而非无条件 remove**——进程启动时若 env 已带预设
    // 值（CI 配置/harness），无条件 remove 会永久抹除、改变后续依赖预设值的行为。

    // #R62 bug/low：var_os 无损快照——var().ok() 把非 Unicode 预设值归 None，
    // Drop 时误执行 remove_var（违反"恢复原值"契约）。
    let _restore = EnvRestore::pin_rerank(&_guard, "0");
    let fused =
        hybrid_search(&engine.pool, "999", &ns, 5, None, None, None, None, false).expect("s");
    assert!(!fused.is_empty(), "rerank-off search for '999' should return the memory");
    assert!(
        fused.iter().all(|r| !r.source.contains(memoria_core::search::text_signals::SOURCE_MARKER)),
        "关闭 rerank 时不应出现 text_signals 通道标记"
    );
}

#[test]
fn signal_tags_persisted_on_remember() {
    let _env_guard = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
    let (engine, ns) = fresh_engine("sigtags");
    let result = remember_with_dedup(
        &engine.pool,
        "2026-07-12 登记 120 吨，改为应急模式",
        "fact",
        3,
        "test",
        &ns,
        r#"["occurred:2026-07-12"]"#,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("remember");

    let conn = engine.pool.get().expect("conn");
    let tags: String = conn
        .query_row(
            "SELECT tags FROM memories WHERE id = ?",
            rusqlite::params![result.id],
            |r| r.get(0),
        )
        .expect("tags row");
    assert!(
        tags.contains("signal:num:120"),
        "P2.2c: tags 应含 signal:num:120, got {tags}"
    );
    assert!(tags.contains("signal:date:2026-07-12"));
    assert!(tags.contains("signal:update:改为"));
    assert!(tags.contains("occurred:2026-07-12"));

    let ctx: Value = memory_context(
        &engine.pool,
        None,
        None,
        None,
        &ns,
        Some("120 吨"),
        5,
        true,
        8,
        8,
        None,
    )
    .expect("context");
    let recall_arr = ctx["recall"].as_array().expect("recall");
    assert!(!recall_arr.is_empty(), "recall empty for signal-tags memory");
    let row = &recall_arr[0];
    let nums = row["text_signals"]["numbers"].as_array().expect("numbers");
    assert!(
        nums.iter().any(|n| n.as_str() == Some("120")),
        "ledger 应从 tags 读回 numbers={:?}",
        nums
    );
}
