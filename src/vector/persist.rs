//! P1-3 向量持久化层。
//!
//! embedding 模型运行在 Python / 调用方，Rust 只接收并存储向量。
//! `memory_vectors` 表是 embedding 的**权威持久存储**：
//! - `remember` 拿到向量（query_cache 优先、其次本表）跑近义去重，并把新向量落表 + 增量加入 HNSW；
//! - 启动时从本表重建 HNSW，使近义去重在重启后依然可靠（不再依赖进程内 QueryCache 与 .bin 快取）。

use crate::storage::SqlitePool;
use crate::vector::{HnswIndex, VectorEntry, DIM};

/// 将 `Vec<f32>` 编码为 little-endian BLOB。
pub fn encode_vector(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for &x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

/// 从 little-endian BLOB 解码为 `Vec<f32>`。
pub fn decode_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 读取某记忆的持久向量（按记忆 id）。无则返回 None。
pub fn get_stored_vector(pool: &SqlitePool, id: &str) -> Option<Vec<f32>> {
    let conn = pool.get().ok()?;
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT vector FROM memory_vectors WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .ok()?;
    let v = decode_vector(&blob);
    if v.len() == DIM {
        Some(v)
    } else {
        None
    }
}

/// 轻量存在性探测（#R69 performance/low）：只查一行、不解码 BLOB——edge_refresh
/// 每 remember_with_dedup 调用一次（近义去重开启且有候选向量时），此前
/// get_stored_vector 为判存在而拉全向量 + 解码成 DIM 长 Vec<f32>，多一次 DB
/// 往返 + 一次完整解码；SELECT 1 + length(vector) 只取标量（SQLite 对 BLOB 的
/// length() 读头部元数据，O(1)，无内容传输）。
/// #R69 bug/medium（R41 谓词统一回归修复）：**DIM 感知**——`AND length(vector)
/// = DIM*4` 使 corrupt/partial 行（blob 长度≠DIM）返回 Ok(false) 而非 true：
/// 此前 R41 把守卫从 get_stored_vector 切到本函数时丢失了"损坏行被写路径
/// 治愈"的行为（旧谓词解码后 None → 触发 re-persist）；损坏行对 exists 为
/// true → persist 跳过、HNSW rebuild 的 skipped_dim 防御永久跳过它、edge_
/// refresh 却仍建边——图与向量索引分歧。恢复：损坏行视为"不存在"，守卫
/// 重新 persist 治愈（upsert 覆盖），edge_refresh 不建边，语义与旧
/// get_stored_vector 完全一致且保持轻量。
/// #R69 bug/medium（R41 三态诉求）：**Result 返回**——pool/DB 故障返回 Err
/// （含内部节流 WARN，3600s 窗口），调用方区分"无行/损坏"（Ok(false)）与
/// "无法检查"（Err），不再把瞬态 DB 中断误诊为"写入失败"。
/// #R69 bug/medium（R45 退化感知）：**degenerate 行也判不存在**——长度正确但
/// 全零的 legacy 行（put_vector_into 的"历史嵌入失败写入全 0 向量"注释承认的
/// 真实态）经 rebuild 的 skipped_degenerate 永久排除出 HNSW，但本探测此前
/// 只看长度返回 Ok(true)：守卫跳过 re-persist、edge_refresh 仍建边——复现
/// 本函数 doc 声称已消除的图/索引分歧，且 put_vector_into 拒绝退化向量使
/// 自愈循环（Ok(false)→re-persist）永不触发。`vector != zeroblob(DIM*4)` 把
/// 全零行归 Ok(false)（re-persist 尝试覆盖；NaN 行因非零仍 Ok(true)——写侧
/// 拒绝 NaN、历史 NaN 罕见，长度+全零守卫覆盖主要真实态）。
pub fn stored_vector_exists(pool: &SqlitePool, id: &str) -> Result<bool, String> {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            warn_throttled(
                "stored_vector_exists",
                "pool",
                &format!("pool get failed for {id}: {e}"),
            );
            return Err(format!("pool get failed for {id}: {e}"));
        }
    };
    match conn.query_row(
        "SELECT 1 FROM memory_vectors WHERE id = ? AND length(vector) = ? AND vector != zeroblob(?)",
        rusqlite::params![id, DIM as i64 * 4, DIM as i64 * 4],
        |_| Ok(()),
    ) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e, rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => {
            warn_throttled(
                "stored_vector_exists",
                "query",
                &format!("row probe failed for {id}: {e}"),
            );
            Err(format!("row probe failed for {id}: {e}"))
        }
    }
}

/// 写入/覆盖某记忆的持久向量（#R38 documentation/low：实现为 `ON CONFLICT(id) DO UPDATE`
/// upsert，而非 INSERT OR REPLACE——REPLACE 会整行删除重建，把
/// `memory_hype_vectors.question` 等非本函数列静默抹成 NULL）。
pub fn put_stored_vector(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
) -> Result<(), String> {
    put_vector_into(pool, id, namespace, vector, "memory_vectors")
}

/// V1（2026-08-12）：写入/覆盖某记忆的 HyPE 问句向量（与 `put_stored_vector` 对称，
/// 落 `memory_hype_vectors` 表）。同款退化/维度防御；共享 `put_vector_into` 实现。
/// #R67 other/low 已知限制：本 API 不写 `question` 列（新建行 question=NULL）——
/// 与离线流水线（build_hype_vectors.py 恒写 question）分歧；问题向量检索只依赖
/// vector 列，NULL question 不影响召回，但会与脚本产物形态不一。未来生产调用方
/// 若需要完整行语义，应扩展可选 question 参数（镜像脚本写契约）。
/// #R57 maintainability/low：`src/` 内无调用者（生产 HyPE 写入走离线脚本
/// build_hype_vectors.py 的原始 INSERT ... ON CONFLICT DO UPDATE，维护 question 列），
/// 但 tests/eval_semantic.rs 的 HyPE upsert 路径实测调用——测试覆盖使其非死代码，
/// 保留（生产侧脚本与库 API 双写路径的潜在分歧由 eval 测试的覆盖率断言兜底）。
pub fn put_hype_stored_vector(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
) -> Result<(), String> {
    put_vector_into(pool, id, namespace, vector, "memory_hype_vectors")
}

/// #R49/#R56 maintainability/medium：**SQL 由宏单源构建**——两张表的 select/insert
/// SQL 只差表名，此前两个手写 concat! 块近乎逐字重复（#R49 注释声称"单源构建
/// 分歧不可能发生"但实现是手写双份——改一张表的 upsert 列集而忘改另一张会静默
/// 分裂两条写入路径，正是 descriptor 要消除的漂移源）。宏以表名作 `concat!` 参数
/// 生成全部 SQL 字段，**表名只出现一次**（每表一行宏调用），模板固定、无注入面
/// （表名是字面量）、无运行时分配。
macro_rules! vector_table {
    ($table:literal, $fn_name:literal, $label:literal, $error_on_all_skipped:expr, $require_fresh:expr) => {
        VectorTable {
            table: $table,
            insert_sql: concat!(
                "INSERT INTO ",
                $table,
                " (id, namespace, vector, updated_at) ",
                "VALUES (?, ?, ?, datetime('now')) ",
                "ON CONFLICT(id) DO UPDATE SET vector=excluded.vector, ",
                "namespace=excluded.namespace, updated_at=excluded.updated_at"
            ),
            select_sql: concat!("SELECT id, vector FROM ", $table, " ORDER BY id"),
            fn_name: $fn_name,
            label: $label,
            error_on_all_skipped: $error_on_all_skipped,
            require_fresh: $require_fresh,
        }
    };
}

struct VectorTable {
    table: &'static str,
    /// #R53 performance/low：**预构建 SQL**（concat! 编译期拼接）——字段字面量
    /// 消除未来 typo/注入面。 #R60 documentation/low 纠正：旧实现是**字符串字面量**
    /// （无每调用分配，R53 的"每次 format! 分配"表述失实）；本字段的真实动机是
    /// 宏单源构建 SQL（#R49/#R56），分配/注入论述是附带收益。
    insert_sql: &'static str,
    select_sql: &'static str,
    fn_name: &'static str,
    label: &'static str,
    /// #R44 maintainability/medium：全 skip 时的错误语义（Err vs Ok(0)+WARN）作为**显式
    /// 字段**而非 `label == "hype"` 字符串比较——label 只服务于日志前缀，用它门控
    /// 行为会让"改 label 名"或"新增第三张表"静默改变错误契约（见 rebuild_from_table）。
    /// content 表保持 Ok(0)（历史兼容：遗留全坏行不该让启动 Err）；hype 表 Err
    /// （区分"功能未启用"与"表数据全损坏"）。
    error_on_all_skipped: bool,
    /// #R62 bug/high：**fresh-index 契约按表**——content 表公开 API 承诺
    /// ".bin 已加载也能安全增量补齐"；hype 表必须全新索引（add 按 id 去重）。
    /// 共享 guard 曾一视同仁拒绝 content 增量（正常重启每次 Err）。
    require_fresh: bool,
}

fn vector_tables() -> &'static [VectorTable] {
    static TABLES: [VectorTable; 2] = [
        vector_table!(
            "memory_vectors",
            "put_stored_vector",
            "content",
            false,
            false
        ),
        vector_table!(
            "memory_hype_vectors",
            "put_hype_stored_vector",
            "hype",
            true,
            true
        ),
    ];
    &TABLES
}

fn lookup_table(table: &str) -> Option<&'static VectorTable> {
    vector_tables().iter().find(|t| t.table == table)
}

/// #R51 maintainability/low：写路径 WARN **限流**——系统性故障（模型 dim 漂移/
/// 磁盘满/BUSY/schema 漂移）时调用方（add_vectors 等）循环 `let _ =` 丢弃 Result，
/// 每写一条刷一行相同 WARN。前 3 条打印（含具体错误文本），其后静默计数，每满
/// 1000 次打一条聚合行——读侧 READ_ERR_LOG_CAP 同款纪律，镜像对称。
/// #R57 other/medium：**按 (fn_name, reason) 分槽**——put_vector_into 的三类失败
/// （degenerate / dim mismatch / execute error）此前共享 fn_name 槽：系统性事故
/// （如嵌入模型产出零向量）会耗尽 3 条详情预算，压制更可操作的错误（BUSY/磁盘满/
/// 约束冲突）；聚合行只含 fn_name + 计数、无错误文本，多因并发事故被掩盖。reason
/// 入槽后每类独立预算，聚合行含 reason 使计数可归因。
/// #R59 refute（性能评论）：**消息构造不惰性化**——调用方传 `&msg` 前已 `format!`
/// 一次，该 String **同时是 `Err(msg)` 的返回体**（put_vector_into 的 Result 契约）：
/// 每次失败必有 1 次分配（构造错误消息），与被抑制无关。若把 warn 改闭包（打印才
/// 求值），打印路径反而多 1 次分配（闭包求值 + Err 构造），净变差；唯一能省的是
/// 让 Err 也惰性——不可能（错误消息立即需要）。当前"共享同一 String"已是该约束下
/// 最优。调用方 `let _ =` 丢弃是它们的显式选择，不改变 API 契约。
fn warn_throttled(fn_name: &str, reason: &str, msg: &str) {
    // #R52 bug/medium：**按槽分**（32 槽 fnv，key=fn_name+reason）——此前两个全局
    // static 被所有写路径共享：先失败的路径消耗前 3 条详情预算（另一路径早期失败
    // 永不单独记录），且聚合行把全局计数归因到碰巧落在 1000 倍数的 fn_name（恰在
    // `let _` 丢弃 Result 的事故中，WARN 是唯一信号）。AtomicU64 fetch_add 原子递增
    // ——check-then-act 竞态（并发全过 `logged < 3` 门槛超发详情行）不存在。槽冲突
    // 只造成两 key 共享预算，可接受（与 throttled_eprintln 同款权衡）。
    use std::sync::atomic::{AtomicU64, Ordering};
    static SLOTS: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
    static SLOT_EPOCHS: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
    let mut h: u64 = 0xcbf29ce484222325;
    // #R58 performance/low：**免中间 String**——`format!("{fn_name}:{reason}")` 在
    // 每次失败调用都堆分配（含被抑制的大多数：n>3 且非 1000 倍数）；此函数专为
    // 写失败风暴设计（生产调用方 `let _ =` 丢弃 Result 的紧循环），与文件自身的
    // no-runtime-allocation 纪律（#R53）对齐。直接 chain 字节序列哈希。
    for b in fn_name.bytes().chain([b':']).chain(reason.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let slot = &SLOTS[(h as usize) % SLOTS.len()];
    // #R61 maintainability/low：**60s 时间窗重置**——此前 process-lifetime 计数永不
    // 归零：execute 槽（合并 BUSY/磁盘满/约束/schema 漂移）的 3 条详情预算被早期
    // 瞬时失败永久耗尽，后续更严重故障只在下一个 1000 倍数出现一次简短聚合
    // （生产调用方 let _ 丢弃 Result，长时间运行中可观测性持续退化）。
    // 窗口翻转：赢者（compare_exchange 成功）把计数清零，随后自己的 fetch_add
    // 成为新窗口第 1 条；输者计数可能被清零吞掉 1 次（诊断计数近似，可接受）。
    // #R65 bug/medium：**单调时钟基准（OnceLock<Instant>）**——SystemTime 墙钟
    // 前后跳会误翻转窗口（WARN 风暴）或让预算耗尽（聚合行失效）；与
    // query_hype_count_cached（#R63）/semantic #R54 的 Instant 纪律一致。
    // #R67 other/medium：**窗口 3600s**——60s 窗口让 ≤3/min 的持续故障每窗口重开
    // 3 条详情预算（~1440 行/天，恰是 #R51 要抑制的刷屏）；小时级窗口下慢速
    // 故障只有每小时前 3 条详情 + 聚合行。
    use std::sync::OnceLock;
    static CLOCK_BASE: OnceLock<std::time::Instant> = OnceLock::new();
    let base = *CLOCK_BASE.get_or_init(std::time::Instant::now);
    let epoch = base.elapsed().as_secs() / 3600;
    let se = &SLOT_EPOCHS[(h as usize) % SLOT_EPOCHS.len()];
    let old_epoch = se.load(Ordering::Relaxed);
    if old_epoch != epoch
        && se
            .compare_exchange(old_epoch, epoch, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        slot.store(0, Ordering::Relaxed);
    }
    let n = slot.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 3 {
        eprintln!("[persist] WARN: {msg}");
    } else if n % 1000 == 0 {
        eprintln!(
            "[persist] WARN: {fn_name}:{reason}: {n} failures so far (sample above; suppressing repeats)"
        );
    }
}

/// 共享实现：校验后写入 `table`（须含 id/namespace/vector/updated_at 列——
/// 迁移已幂等补齐，见 insert_sql #R52）。
///
/// `table` 仅接受 descriptor 白名单（杜绝字符串插值注入面）；
/// 用 `ON CONFLICT(id) DO UPDATE` 而非 INSERT OR REPLACE——REPLACE 会整行删除重建，
/// 把 `memory_hype_vectors.question`（离线脚本写入的假设问句）静默抹成 NULL。
fn put_vector_into(
    pool: &SqlitePool,
    id: &str,
    namespace: &str,
    vector: &[f32],
    table: &str,
) -> Result<(), String> {
    // #R65 other/low：pool.get 失败也走 warn_throttled——生产 add_vectors 路径
    // `let _ =` 丢弃 Result，池耗尽/连接获取失败的反复出现会完全静默（正是限流
    // 要 surface 的系统性故障类）。
    let td =
        lookup_table(table).ok_or_else(|| format!("put_vector_into: unknown table {table}"))?;
    let fn_name = td.fn_name;
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{fn_name}: pool: {e}");
            warn_throttled(fn_name, "pool", &msg);
            return Err(msg);
        }
    };
    // P0 防御：拒绝退化（零 / 非有限）向量落库。历史嵌入失败的写入会在 memory_vectors
    // 留下全 0 向量，污染 HNSW 语义召回（零向量被 DistCosine 误判为完美匹配）。
    // 调用方均 `let _ =` 忽略返回值，故记忆仍照常写入、仅缺语义向量（退化为 keyword-only）。
    let norm_sq: f64 = vector.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    if !norm_sq.is_finite() || norm_sq == 0.0 {
        // #R41 maintainability/low：与维度分支一致地记录——调用方均 `let _ =` 忽略
        // Result，零/NaN 写入若不 eprintln 与错长写入的可见性不对称。
        let msg = format!("{fn_name}: degenerate (zero/NaN) vector rejected");
        warn_throttled(fn_name, "degenerate", &msg);
        return Err(msg);
    }
    // 写入时校验维度：错误长度的向量落库后会被 rebuild 静默跳过（仅 stderr 告警），
    // 造成"API 接受但索引永远不含"的死行——fail fast 使写入/读取路径一致。
    // 注意：退化检查在前、维度检查在后——零值且错长度的向量报"degenerate"而非
    // "dimension mismatch"，是刻意为之（退化更根本，先拒绝）。
    if vector.len() != DIM {
        // #R40 other/low：调用方均 `let _ =` 忽略返回值——维度拒绝若静默，嵌入模型
        // 维度漂移（1024→768）时记忆照常写入但语义覆盖悄然下降、无任何日志。记录
        // 使失败可观测（与 rebuild 侧 skipped 行的 eprintln 诊断对齐）。
        let msg = format!(
            "{fn_name}: dimension mismatch: expected {}, got {}",
            DIM,
            vector.len()
        );
        warn_throttled(fn_name, "dim_mismatch", &msg);
        return Err(msg);
    }
    conn.execute(
        td.insert_sql,
        rusqlite::params![id, namespace, encode_vector(vector)],
    )
    .map(|_| ())
    .map_err(|e| {
        // #R44 bug/medium：execute 失败必须 eprintln——所有生产调用方 `let _ =`
        // 丢弃 Result（lib.rs:197、remember.rs 多处），schema 漂移/磁盘满/BUSY/
        // 约束冲突完全不可见；与上方 degenerate/dim 两个 fail-fast 分支的可观测性
        // 对齐（此前仅该分支静默，索引缺口只能从 HNSW 长度差异推断）。
        let msg = format!("{fn_name}: {}", e);
        warn_throttled(fn_name, "execute", &msg);
        msg
    })?;
    Ok(())
}

/// 查询某记忆所属 namespace（用于批量落表时补全维度）。
pub fn lookup_namespace(pool: &SqlitePool, id: &str) -> Option<String> {
    let conn = pool.get().ok()?;
    conn.query_row(
        "SELECT namespace FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .ok()
}

/// 从 `memory_vectors` 表重建 HNSW 索引（启动权威路径）。
///
/// `HnswIndex::add` 内部按 id 去重，因此即使 .bin 已加载也能安全增量补齐；
/// 返回实际加入的向量条数。
pub fn rebuild_hnsw_from_store(pool: &SqlitePool, hnsw: &HnswIndex) -> Result<usize, String> {
    rebuild_from_table(pool, hnsw, "memory_vectors").map(|(count, _, _)| count)
}

/// V1（2026-08-12）：从 `memory_hype_vectors` 表重建 HyPE 问句向量 HNSW 索引。
///
/// 与 `rebuild_hnsw_from_store` 平行（共享 `rebuild_from_table` 实现），喂给**独立的 HyPE
/// HNSW 实例**（内容索引与问句索引分离，因 HnswIndex 按 id 去重、同一 memory_id 只能有
/// 一条向量）。调用方（main.rs/lib.rs）在启动时对两个索引分别 rebuild；`semantic_search`
/// 双路搜索后按 memory_id 取 max 合并。
///
/// #R44 bug/medium 已知限制（#R69 documentation/low 修订：**与 require_fresh 行为
/// 对齐**——此前 doc 说"已填充索引只追加新 id、已存在 id 更新被静默忽略"，但
/// require_fresh guard（见 rebuild_from_table #R62）使 **hype 表在 hnsw.len() > 0
/// 时直接返回 Err**，并非静默追加：调用方读到旧 doc 会期待 append 语义而收到
/// 错误。当前准确契约：content 表（rebuild_hnsw_from_store）可增量补齐（add 按
/// id 去重）；hype 表要求**全新索引**，已填充索引调用返回 Err——需要拾取更新
/// 向量时须 `let fresh = HnswIndex::new(); fresh.set_ef_search(..);
/// rebuild_hype_hnsw_from_store(pool, &fresh)?;` 后整体替换。
pub fn rebuild_hype_hnsw_from_store(pool: &SqlitePool, hnsw: &HnswIndex) -> Result<usize, String> {
    rebuild_from_table(pool, hnsw, "memory_hype_vectors").map(|(count, _, _)| count)
}

/// 带 read_errors/skipped 的 HyPE rebuild（#R69 bug/medium）：build_hype_hnsw 的
/// 底层——部分重建（行级读取错误、或 dim 漂移/degenerate/corrupt 跳过行被吸收
/// 为 WARN）对 refresh 路径不可接受：refresh 会用降级索引静默替换健康快照。
/// 返回 (count, read_errors, skipped) 供调用方在 swap 前决策（任一 > 0 → 拒绝
/// 替换，保留旧快照）。
pub fn rebuild_hype_hnsw_from_store_detailed(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
) -> Result<(usize, usize, usize), String> {
    rebuild_from_table(pool, hnsw, "memory_hype_vectors")
}

/// V1（2026-08-12）：解析 `MEMORIA_EF_SEARCH` 的**唯一入口**（main.rs 与 lib.rs 共用）。
/// 此前两入口各自复制「env 读取 + clamp + 默认 128」——若一处改 clamp/默认而另一处漏改，
/// 内容/HyPE 索引 ef 行为静默分裂。收口后两入口天然同步。
/// #R40 maintainability/low：**clamp** 而非过滤——`.filter(ef>=16)` 会把运维刻意配置的
/// 低值（如 8，trade recall for latency）静默替换成默认 128 且无提示；`.map(ef.max(16))`
/// 保留低值意图并夹到文档下限。
/// #R41 other/low：补**上界** clamp（typo 如 100000000 会让 HNSW 每查询延迟/内存爆炸）
/// 并在原始值非法/被 clamp 时告警——此前静默映射默认值无任何诊断。
pub fn resolve_ef_search() -> usize {
    const EF_MAX: usize = 4096;
    // #R45 maintainability/low：默认值单一常量——此前不可解析分支与缺省分支各写
    // 一个 128，未来改默认只改一处时另一处静默保留旧值（入口收口的初衷就是防分裂）。
    const DEFAULT_EF: usize = 128;
    match std::env::var("MEMORIA_EF_SEARCH") {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(raw) => {
                let ef = raw.clamp(16, EF_MAX);
                if ef != raw {
                    eprintln!(
                        "[persist] WARN: MEMORIA_EF_SEARCH={raw} clamped to [16, {EF_MAX}] -> {ef}"
                    );
                }
                ef
            }
            Err(_) => {
                eprintln!(
                    "[persist] WARN: MEMORIA_EF_SEARCH={v:?} not parseable, using default {DEFAULT_EF}"
                );
                DEFAULT_EF
            }
        },
        Err(_) => DEFAULT_EF,
    }
}

/// V1（2026-08-12）：构造并重建 HyPE 索引的**唯一入口**（main.rs 与 lib.rs 共用）。
///
/// lib 路径与 standalone 路径此前各自写了一遍「解析 MEMORIA_EF_SEARCH + 双索引 ef 对齐 +
/// HyPE rebuild」——正是代码库已收敛的入口分叉问题（见 migrate_superseded_by 注释
/// "统一在此收口，避免入口分叉"）。若两处后续调参（clamp/默认值）会静默分裂，故收口。
/// 返回 (hype_hnsw, count)：count 供调用方区分「功能未启用（空表，count=0）」与
/// 「rebuild 失败（表有数据但加载失败）」——后者以 `Result` 错误呈现（#R36
/// maintainability/low：此前静默丢弃 Result，失败只能从 WARN 行推断）。
pub fn build_hype_hnsw(pool: &SqlitePool, ef_search: usize) -> Result<(HnswIndex, usize), String> {
    let hype_hnsw = HnswIndex::new();
    hype_hnsw.set_ef_search(ef_search);
    let (count, _, _) = rebuild_hype_hnsw_from_store_detailed(pool, &hype_hnsw)?;
    Ok((hype_hnsw, count))
}

/// 带 read_errors/skipped 的 build（#R69 bug/medium）：refresh 路径专用——部分
/// 重建（read_errors > 0 或 skipped > 0——dim 漂移/degenerate/corrupt 跳过行
/// 是更常见的降级信号）必须由调用方拒绝替换健康快照；or_default 与启动路径
/// 保持原语义（容忍部分行，WARN 可见）。
pub fn build_hype_hnsw_detailed(
    pool: &SqlitePool,
    ef_search: usize,
) -> Result<(HnswIndex, usize, usize, usize), String> {
    let hype_hnsw = HnswIndex::new();
    hype_hnsw.set_ef_search(ef_search);
    let (count, read_errors, skipped) = rebuild_hype_hnsw_from_store_detailed(pool, &hype_hnsw)?;
    Ok((hype_hnsw, count, read_errors, skipped))
}

/// V1（2026-08-12）：build + 软降级兜底的**唯一入口**（main.rs 与 lib.rs 共用）。
///
/// rebuild 失败不 panic（软降级空索引，语义检索退单路），失败以 eprintln 显式告警。
/// 调用方只负责各自的日志流（stdout vs stderr）——若 build/降级/WARN 行为在两入口
/// 各写一份，后续改一处另一处静默分裂（#R38 maintainability/low）。
/// #R42 other/low：注意 count=0 折叠了"空表（功能未启用）"与"rebuild 失败（数据损坏）"
/// 两种状态——区分只存在于本函数发出的 WARN 行；调用方若需要编程式区分，请改用
/// `build_hype_hnsw`（返回 Result）。
pub fn build_hype_hnsw_or_default(pool: &SqlitePool, ef_search: usize) -> (HnswIndex, usize) {
    match build_hype_hnsw(pool, ef_search) {
        Ok(x) => x,
        Err(e) => {
            // #R43 maintainability/low：统一 persist 层告警前缀（其余均为 [persist]）——
            // 运维 grep persist 告警不会漏掉此行。
            eprintln!(
                "[persist] WARN: HYPE HNSW rebuild failed (semantic degraded to single path): {}",
                e
            );
            // #R39 maintainability/low：fallback 索引也须对齐 ef_search——成功路径设了
            // 配置值，fallback 若用默认 128 会在配置 ≠128 时与内容索引分裂（索引当前
            // 为空，但 lib.rs 文档支持运行时手动 rebuild 填充，届时会用错误的 ef 检索）。
            let fallback = HnswIndex::new();
            fallback.set_ef_search(ef_search);
            (fallback, 0)
        }
    }
}

/// 共享实现：从 `table`（须含 id/vector 列）读取全部向量并加入 HNSW。
///
/// 错误/告警前缀用 descriptor 的 `label`（区分 content/hype，便于日志定位）——不再由
/// 调用方传第二份字符串，避免与 descriptor 漂移（#R38 maintainability/low）。
/// 统计并告警被跳过的行（解码失败 / 维度 ≠ DIM），使索引健康度可观测——
/// 数据损坏不再被静默吞掉。表名**白名单 dispatch**（descriptor 查找，非字符串插值）。
fn rebuild_from_table(
    pool: &SqlitePool,
    hnsw: &HnswIndex,
    table: &str,
) -> Result<(usize, usize, usize), String> {
    // #R61 maintainability/medium：**fresh-index 契约强制**——HnswIndex::add 按 id
    // 去重：已填充索引上 rebuild 只追加新 id、已有 id 的向量更新被静默忽略，而
    // "rebuild" 名字与 Ok(count) 暗示全量刷新（调用方拿到陈旧结果不自知）。doc
    // 警告只是软防线；非空索引直接 Err，误用响亮失败（refresh 路径用全新索引
    // 构造，不受影响）。
    let td = lookup_table(table)
        .ok_or_else(|| format!("rebuild_from_table: unknown rebuild table {table}"))?;
    // #R61 maintainability/medium：**fresh-index 契约强制（按表）**——HnswIndex::add
    // 按 id 去重：已填充索引上 rebuild 只追加新 id、已有 id 的向量更新被静默忽略。
    // #R62 bug/high：仅 hype 表强制（require_fresh）——content 表公开契约支持
    // ".bin 已加载增量补齐"（启动路径 load 快照后 rebuild 对齐权威表），无条件
    // guard 会让正常重启每次 Err（快照缺行永不入索引）。
    if td.require_fresh && hnsw.len() > 0 {
        return Err(format!(
            "rebuild_from_table({table}): index already populated ({} ids) - rebuild requires a fresh index; in-place rebuild silently ignores existing-id updates",
            hnsw.len()
        ));
    }
    let label = td.label;
    // #R65 other/low：pool.get 失败也走 warn_throttled（rebuild 路径用 label 作
    // 槽前缀——与写路径 fn_name 语义一致）。
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{label}: pool: {e}");
            warn_throttled(label, "pool", &msg);
            return Err(msg);
        }
    };
    let mut stmt = conn
        .prepare(td.select_sql)
        .map_err(|e| format!("prepare {}: {}", label, e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| format!("query {}: {}", label, e))?;

    let mut entries: Vec<VectorEntry> = Vec::new();
    // #R47 maintainability/low：skip 分因计数——degenerate（零/NaN）、dim 不符、blob
    // 字节长度损坏是三种可区分原因（此前单计数在 all-skipped 时无法一眼判断是维度
    // 漂移还是整体损坏；read_errors 已单独）。
    let mut skipped_degenerate = 0usize;
    let mut skipped_dim = 0usize;
    let mut skipped_blob = 0usize;
    let mut read_errors = 0usize;
    let mut rows_seen = 0usize;
    // #R47 maintainability/low：行级读取失败只记前 3 条——系统性漂移时每行一条会
    // 刷爆 stderr（hype 表可达数千行），聚合 read_errors 计数已存在；保留前几条
    // 作原因样本即可。
    let mut read_err_logged = 0usize;
    const READ_ERR_LOG_CAP: usize = 3;
    for row in rows {
        match row {
            Ok((id, blob)) => {
                rows_seen += 1;
                // #R53 performance/low：**blob 字节长度在 decode 前判定**——损坏/超长
                // blob 先全量解码分配再被拒（恰是要防的损坏场景）；chunks_exact(4)
                // 语义下分类主要由 blob.len() 决定。
                // #R54 maintainability/low：**4 字节对齐先查**——非 4 的倍数 = 截断/
                // 对齐损坏（写入端 encode_vector 恒产出 4 倍数长度，非倍数必是损坏）：
                // 与"维度漂移"（短但 4 对齐，如 768d 模型）区分开，否则 DIM*4-2 这类
                // 截断 blob 被误读成 1024→768 模型漂移、DIM*4+2 被归"超长"（而
                // chunks_exact 恰好解出 DIM 个有效 f32 本可入索引）。all-skipped 的
                // Err 现在门控 hype 启动路径，归因错误会把运维引向错误根因。
                // #R58 other/low：**空/极短 blob 先归 corrupt（skipped_blob）**——
                // 0 字节或单个 float（4 字节）通过对齐检查后被归 skipped_dim（模型
                // 漂移）会误导运维往"换模型"方向查（而 0/4 字节更可能是截断/损坏）；
                // 这些计数器喂给 hype 启动路径的 all-skipped 归因（error_on_all_
                // skipped）。空 blob 在此单独短路。
                // #R60 bug/low：`<= 4` 而非仅 `is_empty()`——R58 的意图是"0 字节或
                // 单个 float 归 corrupt（skipped_blob）而非模型漂移（skipped_dim）"，
                // 但 4 字节 blob 通过 is_empty + 4 对齐后仍落 `len < DIM*4` → dim 桶。
                // encode_vector 恒产出 4 倍数，单 float blob 更可能是截断/损坏；
                // hype 启动门（error_on_all_skipped）的 all-skipped 归因会因此把
                // 损坏表误指为维度漂移。
                if blob.len() <= 4 {
                    skipped_blob += 1;
                    continue;
                }
                if blob.len() % 4 != 0 {
                    skipped_blob += 1;
                    continue;
                }
                if blob.len() < DIM * 4 {
                    skipped_dim += 1;
                    continue;
                }
                // #R68 maintainability/low：**4 对齐超长归 skipped_dim**（与短侧
                // 对称）——写入端 encode_vector 恒产出 v.len()*4 字节，超长 4 对齐
                // blob（如 (DIM+1)*4，大维度写时代遗留）与短侧同为维度漂移产物
                // （chunks_exact 可解出 DIM+1 个有效 float）；`%4!=0`/`<=4` 分支
                // 已捕获字节级损坏，此处归 corrupt 会把运维引向"存储损坏"而非
                // "模型维度变更"。
                // #R69 maintainability/low：**死代码清理**——`% 4 == 0` 谓词冗余：
                // 上面 `%4 != 0` guard 之后所有到达此处的 blob 必 4 对齐，原
                // `> DIM*4 && %4==0` 与随后的 `> DIM*4`（归 skipped_blob）两个分支
                // 中后者不可达（前者的条件已覆盖全部超长）。合并为单分支归
                // skipped_dim——维度漂移与字节损坏的分类由 %4!=0/<=4 guard 完成，
                // 无功能性变化，但消除"未来编辑误把超长改回 skipped_blob"的陷阱。
                if blob.len() > DIM * 4 {
                    skipped_dim += 1;
                    continue;
                }
                let v = decode_vector(&blob);
                // #R41 bug/medium：读侧镜像写侧的退化检查——历史失败嵌入留下的全零/NaN
                // 行（写侧 P0 防御的注释承认已存在）若只查维度会被**每次启动重新加载进
                // HNSW**，写侧防御形同虚设。有限且非零范数才入索引，退化行计入 skipped
                // 并出现在诊断里。
                let norm_sq: f64 = v.iter().map(|x| (*x as f64) * (*x as f64)).sum();
                if !norm_sq.is_finite() || norm_sq == 0.0 {
                    skipped_degenerate += 1;
                } else {
                    entries.push(VectorEntry { id, vector: v });
                }
            }
            Err(e) => {
                rows_seen += 1;
                read_errors += 1;
                // 注意：decode_vector 本身不可失败（长度不符走 Ok 分支的跳过路径），
                // 此 Err 分支捕获的是 query_map 的行读取/列转换/SQLite 迭代错误。
                if read_err_logged < READ_ERR_LOG_CAP {
                    read_err_logged += 1;
                    // #R66 other/low：统一 [persist] WARN 前缀（写路径 let _ 丢弃 Result 时这些
                    // 样本行是主要失败信号，ops grep [persist] WARN 会漏掉引用了错误文本的行）。
                    // #R69 style/low：缩进对齐 enclosing 块（此前 8 空格错位，ops grep
                    // [persist] WARN 时难以阅读）。
                    eprintln!("[persist] WARN: {label} row read/iteration failed: {e}");
                }
            }
        }
    }
    let skipped = skipped_degenerate + skipped_dim + skipped_blob;
    if skipped > 0 {
        // #R47 maintainability/low：摘要按原因分解——运维一眼判断维度漂移 vs 整体
        // blob 损坏 vs 退化行（read_errors 单独汇总，不混入）。
        eprintln!(
            "[persist] {label}: {skipped} row(s) skipped (degenerate {skipped_degenerate}, dim != {DIM} {skipped_dim}, corrupt blob length {skipped_blob})"
        );
    }
    if read_errors > 0 {
        eprintln!(
            "[persist] {label}: {read_errors} row(s) failed read/iteration{}",
            if read_err_logged < read_errors {
                format!(" (first {read_err_logged} logged above)")
            } else {
                String::new()
            }
        );
    }

    // #R37 bug/medium：区分"空表"（无行，count=0 = 功能未启用）与"表有数据但全部损坏"。
    // #R43 bug/medium：仅 **hype** 路径全 skip 时 Err（区分"功能未启用"与"数据损坏"）；
    // **content** 路径保持 Ok(0) + WARN——历史全退化行部署此前就是 Ok(0)，改 Err 是
    // 对现有公共函数 rebuild_hnsw_from_store 的契约变更（startup 路径，下游可能 `?`）。
    // #R44 maintainability/medium：Err/Ok 语义由 descriptor 显式字段
    // `error_on_all_skipped` 决定——不用 `label == "hype"` 字符串门控（改 label 名或
    // 新增第三张表会静默改变错误契约）。
    // #R46 bug/low：全损归因须区分 read_errors 与 skipped——全行读取失败（列类型漂移）
    // 时 skipped=0，原消息统一报"dim mismatch/corrupt"会误导运维（#R45 已把 read_errors
    // 从 skip 摘要分离，此处条件/消息一并纳入）。
    if entries.is_empty() && rows_seen > 0 {
        let cause = if skipped == 0 {
            format!("all {rows_seen} row(s) failed read/iteration")
        } else {
            format!("{rows_seen} row(s) unusable ({skipped} skipped, {read_errors} read errors)")
        };
        if td.error_on_all_skipped {
            // #R55 bug/low：skip-cause 括号**仅当确有 skip 行时追加**——skipped==0
            // （全行读取失败，如列漂移）时列出 dim/corrupt 归因是误导（#R46 声称已修
            // 的归因错误在启动门控路径重现）；cause 此时已含真实原因。
            let detail = if skipped > 0 {
                " (dim mismatch, corrupt/degenerate blob, or read error)"
            } else {
                ""
            };
            return Err(format!("{label}: {cause}{detail}"));
        }
        // #R68 other/low：统一 [persist] WARN 前缀（skip 健康信号是 let-_ 调用方
        // 的主要可观测性，ops grep [persist] WARN 必须能命中）。
        eprintln!("[persist] WARN: {label}: {cause}; returning 0 (historical behavior)");
    }
    if entries.is_empty() {
        return Ok((0, read_errors, skipped));
    }
    let added = hnsw.add(&entries)?;
    Ok((added, read_errors, skipped))
}
