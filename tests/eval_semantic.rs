//! 含语义通道的真实召回率评测（Semantic Eval）— 手动评测范畴
//!
//! 与 tests/eval.rs 同一语料/用例，但走完整生产路径：
//! 本地 embedding 服务(8777) → QueryCache → HNSW → hybrid_search（含 S2 语义信号）。
//! 额外追加「同义改写」用例验证语义召回能力。
//!
//! 运行：`cargo test --test eval_semantic -- --nocapture`
//! 前置：embed_server.py 运行于 127.0.0.1:8777（MEMORIA_EMBEDDING_URL）。

use memoria_core::MemoriaEngine;
use memoria_core::search::hybrid::hybrid_search;
use memoria_core::storage::{SqlitePool, create_pool, init_core_tables, init_schema};
use memoria_core::tools::remember::remember_with_dedup;
use memoria_core::vector::{VectorEntry, persist};
use serde_json::Value;
use std::path::Path;
use std::time::Instant;

/// 召回下限（与 tests/eval.rs 对齐）
const RECALL_FLOOR: f64 = 0.85;
const EMBED_URL: &str = "http://127.0.0.1:8777/embed";

/// #R51/#R52 maintainability/low：结构化错误——此前 String 前缀匹配分类脆弱（改错误
/// 文案会静默改变重试行为；组合消息 `{e}; retry also failed: {e2}` 不可分类；且
/// 所有 5xx 一律瞬时导致 501/505 之类确定性错误白付重试——现瞬时 5xx 收窄为
/// 500/502/503/504，见 is_transient_embed_err）。错误类型在源头区分，重试决策 robust。
#[derive(Debug)]
enum EmbedError {
    /// send 失败（网络抖动/连接 reset）——瞬时。
    Transport(String),
    /// 非 2xx 状态码（含原因短语）。
    Status(u16, String),
    /// 2xx 但畸形 payload（缺字段/非向量/非数值）——确定性。
    Malformed(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Transport(e) => write!(f, "embed http: {e}"),
            // #R53 style/low：r 已含数字 code 与原因短语（"503 Service Unavailable"），
            // 再拼 {c} 会渲染成 "503 503 Service Unavailable"（重试路径里进一步
            // 复合）——只保留 r。
            EmbedError::Status(_c, r) => write!(f, "embed http status {r}"),
            EmbedError::Malformed(m) => write!(f, "embed: {m}"),
        }
    }
}

fn is_transient_embed_err(e: &EmbedError) -> bool {
    match e {
        EmbedError::Transport(_) => true,
        // #R52 maintainability/low：瞬时 5xx **收窄为 500/502/503/504**——501/505
        // 之类"未实现/版本不支持"是确定性（配置错误，重试同样失败）；此前所有 5xx
        // 归瞬时让确定性 501 白付 500ms + 重复请求（doc 注释与实现对齐）。
        EmbedError::Status(c, _) => matches!(c, 429 | 408 | 500 | 502 | 503 | 504),
        EmbedError::Malformed(_) => false,
    }
}

async fn embed(
    client: &reqwest::Client,
    text: &str,
    timeout: std::time::Duration,
) -> Result<Vec<f32>, EmbedError> {
    let body = serde_json::json!({"texts": [text], "normalize": false});
    let resp = client
        .post(EMBED_URL)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|e| EmbedError::Transport(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(EmbedError::Status(
            resp.status().as_u16(),
            resp.status().to_string(),
        ));
    }
    // #R56 bug/medium：**解码与 body 读取错误分开归类**——resp.json() 的失败既可能
    // 是确定性畸形 payload（JSON 解析失败，is_decode）也可能是瞬时网络故障（body
    // 流中途 reset/读超时，is_body/is_timeout）——此前全部归 Malformed（确定性、
    // 不重试）：corpus 路径 expect panic 整个测试（单次抖动全红）、HyPE 路径该 rid
    // 永久 skip（覆盖率假红），恰与有界重试要吸收的瞬时抖动目标相悖。
    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) if e.is_decode() => return Err(EmbedError::Malformed(format!("parse: {e}"))),
        Err(e) => return Err(EmbedError::Transport(format!("body read: {e}"))),
    };
    let arr = data["embeddings"]
        .as_array()
        .ok_or_else(|| EmbedError::Malformed("missing embeddings".to_string()))?;
    // #R52 bug/medium：空数组用 first() 守卫——`arr[0]` 在 `{"embeddings": []}` 时
    // 越界 panic 整个测试进程，直接违背结构化错误"畸形 payload 确定性分类不 panic"
    // 的设计目的。filter_map 静默丢非数值会产出截断向量——显式转换失败走
    // Malformed（同形状响应每次必现，确定性）。
    let first = arr
        .first()
        .ok_or_else(|| EmbedError::Malformed("empty embeddings".to_string()))?;
    first
        .as_array()
        .ok_or_else(|| EmbedError::Malformed("bad vector".to_string()))?
        .iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| EmbedError::Malformed("non-numeric vector element".to_string()))
        })
        .collect()
}

/// embed 带**一次有界重试**（#R45 test/low）：embed() 是单发 POST 无重试——瞬时服务
/// 抖动（连接 reset/限流）若落在某 rid 首次遭遇，fail-once 策略会把它永久排除在
/// HyPE 覆盖外，小语料上覆盖率断言（≥80%）可能假红。一次 500ms 退避重试吞掉
/// 绝大多数瞬时抖动。
/// 瞬时错误（见 is_transient_embed_err）：send 失败 / 429 / 408 / 5xx；确定性错误
/// （其余 4xx、2xx 畸形 payload）不重试——系统性配置错误重试只多付 500ms + 必然
/// 失败的重复请求。
/// #R51 performance/low：**重试 attempt 用短超时（10s）**——挂死服务的最坏单 rid
/// 耗时 = 60s（首次）+ 0.5s（退避）+ 10s（重试）≈ **70.5s**（#R52 documentation/
/// low：此前注释误写 ~20.5s）；× 21 唯一 rid ≈ **24.7 分钟**——10s 上限已把 42
/// 分钟（复用 60s）压到 ~25 分钟，评估/调参时按此数。
async fn embed_with_retry(client: &reqwest::Client, text: &str) -> Result<Vec<f32>, EmbedError> {
    match embed(client, text, std::time::Duration::from_secs(60)).await {
        Ok(v) => Ok(v),
        Err(e) if !is_transient_embed_err(&e) => Err(e),
        Err(e) => {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match embed(client, text, std::time::Duration::from_secs(10)).await {
                Ok(v) => Ok(v),
                // #R52 maintainability/low：保留重试的**自身变体**（e2 为准）——
                // 此前无条件包 Transport 会把"重试命中确定性错误"（Malformed/4xx/
                // 501）展平成瞬时类，未来任何消费该错误的 is_transient 判定会错误
                // 重试；首次错误信息并入 e2 文本保留诊断。
                Err(e2) => match e2 {
                    EmbedError::Status(c, r) => {
                        Err(EmbedError::Status(c, format!("{r} (first attempt: {e})")))
                    }
                    EmbedError::Malformed(m) => {
                        Err(EmbedError::Malformed(format!("{m} (first attempt: {e})")))
                    }
                    EmbedError::Transport(t) => {
                        // #R56 maintainability/low：用**内部消息** t 而非首次错误的
                        // Display `{e}`——Transport 的 Display 已渲染 `embed http: <err>`
                        // 前缀，直接拼 `{e}` 会产出 `embed http: embed http: ...` 双重
                        // 前缀（与 #R53 给 Status 修的同类缺陷）。首次错误并入文本
                        // 保留诊断（t 通常即同一传输错误，信息不丢）。
                        Err(EmbedError::Transport(format!("{t} (first attempt: {e})")))
                    }
                },
            }
        }
    }
}

/// 单条记忆的 HyPE 补嵌结果（#R44 maintainability/low：四个失败分支的簿记收口）。
enum HypeOutcome {
    /// 问句化 embed → put → add 全链路成功且索引真正加入（n>0）。
    Covered,
    /// 任一环节失败/被拒，附原因（已去重计数由调用方处理）。
    Skipped(String),
}

/// 问句化改写嵌入 → 落库 → 入 hype 索引，一次调用完成（#R44 maintainability/low：
/// 此前整个 embed/put/add 金字塔嵌套在语料循环体内，四个失败分支重复
/// `hype_skipped += 1 / hype_processed.remove` 簿记，深嵌套让策略矛盾难以发现。
/// 抽出后成败簿记集中在调用方一处）。
///
/// #R44/#R45 bug/medium 策略：**失败即跳过，不重试整个链路**——近义重复条目对同一
/// rid 重试 = 重复付费 embed；系统性故障（服务拒绝前缀 prompt）重试同样失败。
/// 瞬时抖动由 embed_with_retry 的一次有界重试覆盖，不再需要跨重复条目的重试机制。
async fn hype_upsert_one(
    client: &reqwest::Client,
    engine: &MemoriaEngine,
    pool: &SqlitePool,
    rid: &str,
    ns: &str,
    content: &str,
    content_vec: &[f32],
) -> HypeOutcome {
    // 问句化改写：与内容向量不同（否则双路合并无意义）。嵌入失败则跳过（不 put/add）
    // ——fallback 到内容向量会让两路恒等、退化为内容通道，且把内容向量写进 question
    // 列与离线脚本的问句向量不一致，rebuild 后会混入两种形态。
    match embed_with_retry(client, &format!("用户提问：{content}")).await {
        Err(e) => HypeOutcome::Skipped(format!("question embed failed: {e}")),
        Ok(hv) => {
            // #R46 test/medium：核心不变量「hv ≠ v」必须**校验**而非仅注释——嵌入服务
            // 若忽略「用户提问：」前缀、或本地开发用恒等 stub 替代真模型，hv≈v，
            // 双路 max 合并退化为内容通道（测不到 HyPE 的 read/merge 路径），而
            // hype_hnsw.len()>0 与 80% 覆盖断言全部照过（恰好是本块要防的 no-op 假
            // 覆盖）。cosine ≥ 0.99 视为恒等 → Skipped：恒等 stub 会触发大量 skip →
            // 覆盖率断言失败 → 测试红，把假覆盖变成显式失败。
            // #R51 bug/low：guard 前置**长度与有限性检查**——zip 静默截断到较短切片
            // （嵌入模型中途换维度时 cosine 算在错配上）；NaN 分量使 cos=NaN，
            // `NaN >= 0.99` 恒 false → 恒等检查被静默绕过。下游 put/add 会拒
            // NaN/错维（无损坏），但本 guard 驱动的 skip/covered 判定必须可预测。
            if hv.len() != content_vec.len() {
                return HypeOutcome::Skipped(format!(
                    "question vector dim {} != content vector dim {} (embed model drifted mid-run?)",
                    hv.len(),
                    content_vec.len()
                ));
            }
            let dot: f64 = hv
                .iter()
                .zip(content_vec.iter())
                .map(|(a, b)| (*a as f64) * (*b as f64))
                .sum();
            let n1: f64 = hv.iter().map(|x| (*x as f64) * (*x as f64)).sum();
            let n2: f64 = content_vec.iter().map(|x| (*x as f64) * (*x as f64)).sum();
            let cos = dot / (n1.sqrt() * n2.sqrt()).max(1e-12);
            if !cos.is_finite() {
                return HypeOutcome::Skipped(format!(
                    "cosine not finite (n1={n1:.4}, n2={n2:.4}); degenerate vector?"
                ));
            }
            if cos >= 0.99 {
                return HypeOutcome::Skipped(format!(
                    "question vector identical to content vector (cos={cos:.4}); embed prefix ignored?"
                ));
            }
            // put 可能因退化向量（零/NaN）/维度/DB 失败返回 Err——内容路容忍静默跳过，
            // 此处按 skip 计数（局部 embed 服务异常不应打挂整个评测；覆盖率断言兜底）。
            match persist::put_hype_stored_vector(pool, rid, ns, &hv) {
                Err(e) => HypeOutcome::Skipped(format!("put rejected (degenerate/dim/db): {e}")),
                Ok(()) => {
                    // add 失败/0 条按 skip 而非 panic：put 用 f64 校验、add 用 f32 复检
                    // （重复 id 返回 0 属正常），put 通过仍可能 n==0。add 的 Err（维度
                    // 不符/锁污染）与 Ok(0)（dup id / f32 复检拒绝）是不同失败，日志区分。
                    match engine.hype_hnsw.add(&[VectorEntry {
                        id: rid.to_string(),
                        vector: hv,
                    }]) {
                        Ok(n) if n > 0 => HypeOutcome::Covered,
                        Ok(_) => HypeOutcome::Skipped(
                            "add returned 0 entries (dup id or f32 re-validation)".into(),
                        ),
                        Err(e) => HypeOutcome::Skipped(format!("add failed: {e}")),
                    }
                }
            }
        }
    }
}

// #R55 test/low：**测试级超时**——retry 路径最坏 ~70.5s/rid（~21 rid ≈ 25 分钟）
// 仅当 embed_server 挂死（accept 不响应）时出现；#[tokio::test] 默认无全局超时，
// CI 里挂死服务会拖垮整个 suite 数十分钟才暴露首个失败。600s 上限远高于正常
// 运行（~1-2 分钟）但能快速 fail 挂死场景，与逐 attempt 超时互补。
// 注：tokio-macros 2.x 的 #[tokio::test] 已移除 timeout 属性（Unknown attribute），
// 用 tokio::time::timeout 手动包装等价实现。
#[tokio::test]
async fn memory_eval_semantic() {
    let inner = memory_eval_semantic_inner();
    tokio::pin!(inner);
    if tokio::time::timeout(std::time::Duration::from_secs(600), inner)
        .await
        .is_err()
    {
        panic!("memory_eval_semantic timed out after 600s (embed server hung?)");
    }
}

async fn memory_eval_semantic_inner() {
    // CI/无本地嵌入服务时跳过（本测试属手动语义评测范畴，依赖 127.0.0.1:8777；
    // GitHub Actions runner 无该服务 → 此前 CI 恒红、本地全绿）
    {
        let probe = reqwest::Client::new();
        let ok = probe
            .get("http://127.0.0.1:8777/health")
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !ok {
            eprintln!("[skip] 本地嵌入服务(127.0.0.1:8777)不可达，跳过语义评测（CI 环境）");
            return;
        }
    }
    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/cases");
    let corpus: Vec<Value> = read_json_array(&cases_dir.join("corpus.json"));
    let cases: Vec<Value> = read_json_array(&cases_dir.join("cases.json"));
    assert!(
        !corpus.is_empty() && !cases.is_empty(),
        "eval cases 不能为空"
    );

    // fixture DB
    let db = std::env::temp_dir().join(format!("memoria_eval_sem_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let pool = create_pool(db.to_str().unwrap(), 4).expect("create_pool");
    init_schema(&pool).expect("init_schema");
    init_core_tables(&pool).expect("init_core_tables");

    // 引擎（写入侧向量入 HNSW）
    let engine = MemoriaEngine::new(db.to_str().unwrap()).expect("engine");
    let client = reqwest::Client::new();

    // 1) 语料嵌入 + 写入（向量入 QueryCache → HNSW）
    let mut ids: Vec<String> = Vec::with_capacity(corpus.len());
    let mut hype_processed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // #R43 bug/medium：total 独立计数（首次遇到即 +1，无论成败）——processed 失败时
    // 会 remove（允许重试），断言时只剩成功的，用它作分母是重言式。
    let mut hype_total = 0usize;
    let mut hype_covered = 0usize;
    let mut hype_skipped = 0usize;
    for item in &corpus {
        let content = item["content"].as_str().expect("corpus[].content");
        let category = item["category"].as_str().unwrap_or("fact");
        let importance = item["importance"].as_i64().unwrap_or(3);
        let ns = item["ns"].as_str().unwrap_or("agent/default");
        let tags = item["tags"].as_str().unwrap_or("[]");

        // #R52 test/low：content-embed 也走 embed_with_retry——瞬时 Transport/429/5xx
        // 在任一语料项上会 panic 整个测试（embed_with_retry 引入的初衷正是吸收
        // 本测试的瞬时抖动，HyPE 侧已用、内容侧此前漏用）。
        let v = embed_with_retry(&client, content)
            .await
            .expect("corpus embed");
        // v 用于内容路（cache_query_vector）；HyPE 块需内容向量副本校验「问句向量与内容
        // 向量确实不同」核心不变量（#R46 test/medium）——clone 一次（1024 维，廉价）。
        engine.cache_query_vector(content, v.clone());
        let r = remember_with_dedup(
            &pool,
            content,
            category,
            importance,
            "eval_sem",
            ns,
            tags,
            Some(&engine.hnsw),
            Some(&engine.query_cache),
            None, // valid_from
            None, // valid_to
            None, // supersedes_id
            None, // relation
            None, // actor
            None, // memory_type
            None, // parent_id
            None, // raw_ref
        )
        .expect("remember");
        let rid = r.id.clone();
        ids.push(r.id);

        // V1：HyPE 通道覆盖——写入**问句化改写**的向量并加入 hype 索引，使语义测试真实走
        // 双路合并（若用与内容路相同的向量，两路 cosine 恒等、合并退化为内容通道，
        // 测不到 HyPE 的 read/merge 路径——正是本块要防的 no-op 假覆盖）。真实生产里问句
        // 向量由 LLM 生成；测试里以「用户提问：+ 内容」嵌入近似（问句-内容措辞不同，
        // 双路分数各异，合并取 max 才有意义）。
        // 结果必须断言：put/add 静默失败会让 hype_hnsw 恒空、测试假绿。注意：
        // remember_with_dedup 近义去重命中时返回**已有记忆 id**，其 hype 向量可能已 add 过
        // （HnswIndex 按 id 去重，重复 add 返回 0 属正常），故仅首次遇到该 rid 才处理。
        {
            // #R39 bug/medium：唯一处理集——每个 rid 只计数一次（近义去重会让同一 rid
            // 出现多次，重复计数虚增分母拉低覆盖率造成假红）。
            // #R43 bug/medium：`hype_total` 独立计数（首次遇到即 +1，无论成败）——此前
            // 失败时把 rid 从 processed 移除以便重试，断言时 processed 只剩成功的，
            // `hype_covered >= processed.len()*0.8` 恒真（重言式），无法防 no-op 假绿。
            // 用 total 作分母：若问句化 embed 对多数记忆持续失败，覆盖断言真实失败。
            // #R44/#R45 maintainability/low：**processed 即唯一去重集**（成功+失败都在
            // 内，每个 rid 至多处理一次）——曾有的 hype_failed_once 恒为 processed 子集
            // （insert 先于一切簿记执行），guard 冗余、conditional 计数死代码，已删除；
            // 瞬时失败由 embed_with_retry 覆盖，无需跨重复条目的重试。
            // #R55 style/low：HashSet::insert 返回 bool（true=新插入）——contains+
            // insert 两次哈希换成单次，避免 check-then-act 形态。
            if hype_processed.insert(rid.clone()) {
                hype_total += 1;
                match hype_upsert_one(&client, &engine, &pool, &rid, ns, content, &v).await {
                    HypeOutcome::Covered => {
                        hype_covered += 1;
                    }
                    HypeOutcome::Skipped(reason) => {
                        eprintln!("[eval_semantic] hype skipped for {rid:?}: {reason}");
                        hype_skipped += 1;
                    }
                }
            }
        }

        // 应用时序偏移
        if let Some(off) = item["created_offset_days"].as_i64() {
            if off > 0 {
                let ts = chrono::Utc::now() - chrono::Duration::days(off);
                let _ = pool.get().unwrap().execute(
                    "UPDATE memories SET created_at = ? WHERE id = ?",
                    rusqlite::params![ts.format("%Y-%m-%dT%H:%M:%S").to_string(), &rid],
                );
            }
        }
    }

    // #R37/#R38/#R43 test/medium：防假绿收口——若问句化 embed 持续失败（如服务拒绝带
    // 前缀的 prompt），put/add 全部静默跳过、hype_hnsw 恒空或只覆盖 1 条，测试会走
    // content-only 路径仍通过——正是本块要防的 no-op 假覆盖。断言**高覆盖率**：
    // 分母用 hype_total（唯一记忆数，无论成败都计入）——processed 在失败时被 remove
    // 无法作分母（重言式）。
    assert!(
        engine.hype_hnsw.len() > 0,
        "hype index empty after corpus setup: question-embed likely failing"
    );
    // #R47 maintainability/low：hype_seen 已删除——它与 hype_covered 在同一 match 臂
    // 同时写入、processed guard 保证每 rid 至多一次，`assert_eq!(hype_seen.len(),
    // hype_covered)` 恒真（重言式，正是 #R43 批评的模式），保留只会误导。
    // #R46 test/low：hype_total 很小时（近义去重把唯一 rid 压到几条，如 2 条）单次瞬时
    // skip 会放大为假红（1/2=50% < 80%）——embed_with_retry 已吞掉绝大多数瞬时抖动，
    // 剩余 skip 是小概率事件，按比例断言与"系统性故障"难以区分。
    // #R47 bug/medium：floor 必须**门控于小 total**——无条件 `hype_covered >= 3` 会让
    // 22 条语料（~21 唯一 rid）只覆盖 3 条（~14%）就通过，重新打开 no-op 假绿。
    // 阈值 = max(min(3, total), 0.8×total)：total≤3 要求全覆盖；total≥4 时比例权威。
    // #R48 documentation/low：hype_covered 是整数、与 f64 阈值比较——实际需求是
    // ceil(0.8×total)（total=4 时 3.2 → 须 4，即 100%；total=6..9 时 83%~88%），
    // 仅 total 为 5 的倍数时恰好 80%。注释以 ceil 语义表述以免误导调参。
    assert!(
        hype_covered as f64 >= (hype_total.min(3) as f64).max(hype_total as f64 * 0.8),
        "hype coverage too low: {hype_covered}/{hype_total} unique memories have HyPE vectors \
         (question-embed likely failing); skipped={hype_skipped}",
    );

    // 2) 官方 12 用例（query 嵌入 → 语义信号参与融合）
    let mut recall_hits = 0u32;
    let mut recall_total = 0u32;
    let mut zero_results = 0u32;
    let mut latencies: Vec<f64> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for c in &cases {
        let q = c["query"].as_str().expect("cases[].query");
        let ns = c["ns"].as_str().unwrap_or("agent/default");
        let k = c["k"].as_u64().unwrap_or(5) as u32;
        let ctype = c["type"].as_str().unwrap_or("");
        let expect: Vec<usize> = c["expect_indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_u64().map(|x| x as usize))
                    .collect()
            })
            .unwrap_or_default();
        let must_not: Vec<usize> = c["must_not_indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_u64().map(|x| x as usize))
                    .collect()
            })
            .unwrap_or_default();

        // query 嵌入
        // #R54 test/medium：query 嵌入也走 embed_with_retry——单发 embed 的瞬时
        // Transport/429/5xx 会静默丢 query 向量（if let Ok 吞错），hybrid
        // 召回低于 RECALL_FLOOR(0.85) 假红；与 corpus/HyPE 路径统一重试策略。
        // #R56 bug/low：双 attempt 全败时**必须记录**——静默降级为 content-only
        // 检索会让召回低于 RECALL_FLOOR 与真实回归不可区分。
        match embed_with_retry(&client, q).await {
            Ok(v) => engine.cache_query_vector(q, v),
            Err(e) => {
                eprintln!("[eval_semantic] query embed failed (content-only for this case): {e}")
            }
        }

        let start = Instant::now();
        let results = hybrid_search(
            &pool,
            q,
            ns,
            k,
            Some(&engine.hnsw),
            Some(&engine.hype_hnsw),
            Some(&engine.query_cache),
            None,
            false,
        )
        .unwrap_or_default();
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);

        let result_ids: Vec<&str> = results.iter().map(|r| r.memory_id.as_str()).collect();
        if results.is_empty() {
            zero_results += 1;
        }

        let mut case_ok = true;
        for &e in &expect {
            match ids.get(e) {
                None => {
                    case_ok = false;
                    failures.push(format!("[{}] expect_indices 越界: {}", ctype, e));
                }
                Some(expected_id) => {
                    if !result_ids.contains(&expected_id.as_str()) {
                        case_ok = false;
                        failures.push(format!(
                            "[{}] 期望 idx {} (id={}) 未进入 top-{}，q='{}'",
                            ctype, e, expected_id, k, q
                        ));
                    }
                }
            }
        }
        for &m in &must_not {
            if let Some(forbidden_id) = ids.get(m) {
                if result_ids.contains(&forbidden_id.as_str()) {
                    case_ok = false;
                    failures.push(format!(
                        "[{}] must_not idx {} (id={}) 泄露，q='{}'",
                        ctype, m, forbidden_id, q
                    ));
                }
            }
        }

        if !expect.is_empty() || !must_not.is_empty() {
            recall_total += 1;
            if case_ok {
                recall_hits += 1;
            }
        }
    }

    // 3) 追加「同义改写」用例：验证语义召回（不共享关键词）
    let paraphrase_cases: Vec<(String, usize)> = vec![
        // 官方 corpus 中的记忆，用完全不同的说法查询
        ("代码托管平台开源仓库".to_string(), 5), // corpus 中 GitHub 相关
        ("在线的开发项目托管服务".to_string(), 5),
        ("技术文档的版本管理".to_string(), 3),
    ];
    let mut sem_hits = 0u32;
    let mut sem_failures: Vec<String> = Vec::new();
    for (pq, idx) in &paraphrase_cases {
        let ns = "agent/default";
        let k = 5;
        // #R56 bug/low：同 query 路径——双败记录，防 content-only 静默降级
        // 与真实回归不可区分。
        match embed_with_retry(&client, pq).await {
            Ok(v) => engine.cache_query_vector(pq, v),
            Err(e) => {
                eprintln!("[eval_semantic] paraphrase embed failed (content-only): {e}")
            }
        }
        let results = hybrid_search(
            &pool,
            pq,
            ns,
            k,
            Some(&engine.hnsw),
            Some(&engine.hype_hnsw),
            Some(&engine.query_cache),
            None,
            false,
        )
        .unwrap_or_default();
        let result_ids: Vec<&str> = results.iter().map(|r| r.memory_id.as_str()).collect();
        let ok = ids
            .get(*idx)
            .map(|id| result_ids.contains(&id.as_str()))
            .unwrap_or(false);
        if ok {
            sem_hits += 1;
        } else {
            sem_failures.push(format!(
                "同义改写 '{}' 未召回 idx {}（top-{}: {:?}）",
                pq, idx, k, result_ids
            ));
        }
    }

    // 指标
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let recall = if recall_total > 0 {
        recall_hits as f64 / recall_total as f64
    } else {
        1.0
    };
    let zero_rate = if !cases.is_empty() {
        zero_results as f64 / cases.len() as f64
    } else {
        0.0
    };

    eprintln!("===== Memoria Semantic Eval（含 S2 语义通道）=====");
    eprintln!(
        "官方用例: cases={} recall@k={:.2} zero_result_rate={:.2}",
        recall_total, recall, zero_rate
    );
    eprintln!("同义改写: {}/{} 命中", sem_hits, paraphrase_cases.len());
    eprintln!("latency p50={:.2}ms p95={:.2}ms", p50, p95);
    if !failures.is_empty() {
        eprintln!("FAILURES:\n{}", failures.join("\n"));
    }
    if !sem_failures.is_empty() {
        eprintln!("SEM_FAILURES:\n{}", sem_failures.join("\n"));
    }
    eprintln!("=================================================");

    assert!(
        recall >= RECALL_FLOOR,
        "召回@k {:.2} 低于下限 {:.2}",
        recall,
        RECALL_FLOOR
    );
    assert_eq!(zero_rate, 0.0, "存在零结果用例");
    assert!(failures.is_empty(), "共 {} 个评测失败", failures.len());

    drop(engine);
    drop(pool);
    let _ = std::fs::remove_file(&db);
}

fn read_json_array(p: &Path) -> Vec<Value> {
    let s = std::fs::read_to_string(p)
        .unwrap_or_else(|e| panic!("读取评测文件失败 {}: {}", p.display(), e));
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("解析评测文件失败 {}: {}", p.display(), e))
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx]
}
