//! Phase B / M1.3：轻量实体共现启发式 rerank（O5：无 cross-encoder、无新重依赖）。
//!
//! 在 RRF 融合结果上对 `rrf_score` 做小幅加成后重排：
//! 1. 查询串命中实体名 → 提及该实体的记忆加分
//! 2. 候选集内实体共现度（与其它候选共享实体越多越高）→ 加分
//!
//! 加成幅度刻意保守，避免淹没 keyword/semantic 主信号。

use crate::search::rrf::FusedResult;
use crate::storage::SqlitePool;
use std::collections::{HashMap, HashSet};

const QUERY_HIT_BOOST: f64 = 0.012;
const PAIRWISE_BOOST: f64 = 0.004;
const MAX_TOTAL_BOOST: f64 = 0.06;

/// 对融合结果做共现启发式重排（原地改 `rrf_score` 并重排）。
/// 无实体表数据或失败时静默跳过（不改变相对序）。
pub fn rerank_by_cooccurrence(
    pool: &SqlitePool,
    namespace: &str,
    query: &str,
    results: &mut Vec<FusedResult>,
) {
    if results.len() < 2 && query.trim().is_empty() {
        return;
    }
    if results.is_empty() {
        return;
    }

    let ids: Vec<String> = results.iter().map(|r| r.memory_id.clone()).collect();
    let mem_entities = match load_memory_entities(pool, namespace, &ids) {
        Some(m) if !m.is_empty() => m,
        _ => return,
    };

    let query_entities = match_query_entities(pool, namespace, query);

    // 候选集内：entity_id → 出现在哪些 memory
    let mut entity_to_mems: HashMap<String, HashSet<String>> = HashMap::new();
    for (mid, ents) in &mem_entities {
        for e in ents {
            entity_to_mems
                .entry(e.clone())
                .or_default()
                .insert(mid.clone());
        }
    }

    for r in results.iter_mut() {
        let ents = mem_entities.get(&r.memory_id);
        let mut boost = 0.0;

        if let Some(ents) = ents {
            // 查询命中实体
            let hits = ents.iter().filter(|e| query_entities.contains(*e)).count();
            boost += hits as f64 * QUERY_HIT_BOOST;

            // 与其它候选的共现度
            let mut peer_overlap = 0usize;
            for e in ents {
                if let Some(peers) = entity_to_mems.get(e) {
                    peer_overlap += peers.len().saturating_sub(1);
                }
            }
            boost += peer_overlap as f64 * PAIRWISE_BOOST;
        }

        if boost > MAX_TOTAL_BOOST {
            boost = MAX_TOTAL_BOOST;
        }
        if boost > 0.0 {
            r.rrf_score += boost;
            if !r.source.contains("cooccur") {
                r.source = format!("{};cooccur", r.source);
            }
        }
    }

    results.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn load_memory_entities(
    pool: &SqlitePool,
    namespace: &str,
    memory_ids: &[String],
) -> Option<HashMap<String, Vec<String>>> {
    if memory_ids.is_empty() {
        return Some(HashMap::new());
    }
    let conn = pool.get().ok()?;
    let ph = vec!["?"; memory_ids.len()].join(",");
    let sql = format!(
        "SELECT memory_id, entity_id FROM entity_mentions \
     WHERE namespace = ?1 AND memory_id IN ({}) \
     AND entity_id NOT IN (SELECT id FROM entities WHERE name = '')",
        ph
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + memory_ids.len());
    params.push(&namespace);
    for id in memory_ids {
        params.push(id);
    }
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows.flatten() {
        map.entry(row.0).or_default().push(row.1);
    }
    Some(map)
}

/// 查询串子串命中的实体 id（ns 内，限 64）。
fn match_query_entities(pool: &SqlitePool, namespace: &str, query: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let q = query.trim();
    if q.is_empty() {
        return out;
    }
    let Ok(conn) = pool.get() else {
        return out;
    };
    let Ok(mut stmt) = conn.prepare("SELECT id, name FROM entities WHERE namespace = ?1 LIMIT 256")
    else {
        return out;
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![namespace], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        let (id, name) = row;
        let n = name.trim();
        if n.len() >= 2 && q.contains(n) {
            out.insert(id);
            if out.len() >= 64 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    // 共享测试夹具：共享缓存内存库 + FusedResult 构造收敛为两 helper
    // （三测试复制曾让 fixture 漂移）。
    fn test_pool(tag: &str) -> SqlitePool {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let db = format!(
            "file:memoria_cooccur_{tag}_{}_{}?mode=memory&cache=shared",
            std::process::id(),
            seq
        );
        let pool = crate::storage::create_pool(&db, 4).expect("pool");
        crate::storage::init_core_tables(&pool).expect("core tables");
        pool
    }

    /// 初始分常量——断言 `> INIT_SCORE` 显式依赖 boost 保持正数，常数被调为
    /// 零/负时失败信息自解释（而非与初始分 0.5 的隐式耦合）。
    const INIT_SCORE: f64 = 0.5;

    fn mk_result(id: &str, content: &str) -> FusedResult {
        FusedResult {
            memory_id: id.into(),
            content: content.into(),
            rrf_score: INIT_SCORE,
            source: "test".into(),
            ..Default::default()
        }
    }

    // 真实调用 rerank_by_cooccurrence（旧断言空 Vec 恒真，函数被删也通过）。
    // no-op 契约：空结果集空进空出、不 panic。注意本测试无法区分空集提前返回
    // 与静默失败路径（load_memory_entities 对空 id 短路、match_query_entities
    // 吞缺表错误）——只验证行为本身；查询路径覆盖由 rerank_with_data 承担。
    #[test]
    fn empty_results_noop() {
        let pool = crate::storage::create_pool(":memory:", 1).expect("pool");
        let mut results: Vec<FusedResult> = Vec::new();
        rerank_by_cooccurrence(&pool, "agent/test", "测试查询", &mut results);
        assert!(results.is_empty(), "empty input must stay empty");
    }

    // #R59 test/medium：**真实查询路径**——init_core_tables 建表（entities/
    // entity_mentions），seed 实体与共现后，共现的 memory 应被提前；此前注释声称
    // "验证内存池查询路径"但空集提前返回 + 无 schema，查询逻辑从未被执行
    // （let Ok else 兜底吞错），测试在查询逻辑完全损坏时也会通过。
    #[test]
    fn rerank_with_data() {
        let pool = test_pool("data");
        let conn = pool.get().expect("conn");
        // entity_mentions.memory_id 有外键引用 memories——先插 memories 再插引用。
        conn.execute(
            "INSERT INTO memories (id, namespace, content, importance) \
             VALUES ('m_a', 'agent/test', '甲记忆', 3), ('m_b', 'agent/test', '乙记忆', 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary) \
             VALUES ('e1', 'agent/test', 'person', '张三', '[]', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace) \
             VALUES ('e1', 'm_a', '提及张三', 'agent/test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace) \
             VALUES ('e1', 'm_b', '也提及张三', 'agent/test')",
            [],
        )
        .unwrap();
        drop(conn);
        // #R61 test/medium：**负控制**——m_c 无任何实体提及（与 m_a/m_b 相同初始
        // rrf_score 0.5）：boosting 记忆应排在它前面（排序契约真正被检验；此前
        // m_a/m_b boost 相同、sort_by 不改变相对序，断言在 sort_by 被删/反向时
        // 也通过）。
        let mut results: Vec<FusedResult> = vec![
            mk_result("m_a", "甲记忆"),
            mk_result("m_b", "乙记忆"),
            mk_result("m_c", "丙记忆（无实体）"),
        ];
        rerank_by_cooccurrence(&pool, "agent/test", "张三", &mut results);
        // #R60 test/medium：断言**可观察效果**——提及实体的记忆 boost 应用后
        // rrf_score 抬升（>0.5 初始值）且 source 带 cooccur 标记（仅 rerank 路径
        // 追加）；此断言在 load_memory_entities 静默失败或 boost 逻辑被删时必红
        // （此前 ids.contains 恒真，正是要消除的假覆盖）。
        // #R62 test/low 注释修正：match_query_entities 失败时 PAIRWISE_BOOST 仍会
        // 触发（m_a/m_b 同实体），query-hit 路径未被本断言独立验证（负控制 m_c
        // 无 boost、排最后）。负控制 m_c 分开断言。
        let boosted: Vec<&FusedResult> = results
            .iter()
            .filter(|r| r.memory_id != "m_c")
            .collect();
        assert!(
            boosted
                .iter()
                .all(|r| r.rrf_score > INIT_SCORE && r.source.contains("cooccur")),
            "cooccur boost must be applied: {:?}",
            results
                .iter()
                .map(|r| (&r.memory_id, r.rrf_score, &r.source))
                .collect::<Vec<_>>()
        );
        // 负控制：无实体提及的 m_c 排在最后（排序契约）。
        assert_eq!(
            results.last().map(|r| r.memory_id.as_str()),
            Some("m_c"),
            "negative control must rank last: {:?}",
            results.iter().map(|r| r.memory_id.as_str()).collect::<Vec<_>>()
        );
    }

    // #R63 test/medium：**query-hit 路径独立验证**——m_d 仅被查询实体 e1 提及
    // （无 pairwise 重叠），其 boost 只能来自 QUERY_HIT_BOOST；此前 m_a/m_b 共享
    // 实体，PAIRWISE_BOOST 独立满足断言（query-hit 分支被删也不红）。
    #[test]
    fn query_hit_boost_alone() {
        let pool = test_pool("qh");
        let conn = pool.get().expect("conn");
        conn.execute(
            "INSERT INTO memories (id, namespace, content, importance)              VALUES ('m_d', 'agent/test', '丁记忆', 3),
                    ('m_e', 'agent/test', '戊记忆', 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)              VALUES ('e1', 'agent/test', 'person', '张三', '[]', NULL)",
            [],
        )
        .unwrap();
        // 仅 m_d 提及 e1（m_e 无任何提及）——m_d 的 boost 只可能来自 query-hit。
        conn.execute(
            "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace)              VALUES ('e1', 'm_d', '提及张三', 'agent/test')",
            [],
        )
        .unwrap();
        drop(conn);
        let mut results: Vec<FusedResult> = vec![mk_result("m_d", "丁记忆"), mk_result("m_e", "戊记忆")];
        rerank_by_cooccurrence(&pool, "agent/test", "张三", &mut results);
        let d = results.iter().find(|r| r.memory_id == "m_d").expect("m_d");
        let e = results.iter().find(|r| r.memory_id == "m_e").expect("m_e");
        // m_d 有 query-hit boost（rrf_score 抬升且 source 带 cooccur）；m_e 无
        // boost（无标记）。
        // #R64 test/low：**排序断言**替代精确浮点相等——boosted 必须排 unboosted
        // 前（直接表达契约；无浮点运算的 no-boost 分支被未来算术改动触碰时
        // assert_eq! 会假红）。
        assert!(d.rrf_score > INIT_SCORE && d.source.contains("cooccur"), "query-hit boost missing: {:?}", (&d.memory_id, d.rrf_score, &d.source));
        assert!(!e.source.contains("cooccur"), "m_e must stay unboosted");
        let d_pos = results.iter().position(|r| r.memory_id == "m_d").unwrap();
        let e_pos = results.iter().position(|r| r.memory_id == "m_e").unwrap();
        assert!(d_pos < e_pos, "boosted m_d must outrank unboosted m_e (d@{d_pos}, e@{e_pos})");
    }

    // #R63 test/low：**单结果分支**——len<2 且 query 非空时跳过早退、仍走查询
    // 路径（guard 回归如改成 || 会静默失去 query-hit）。
    #[test]
    fn single_result_with_query_runs_query_path() {
        let pool = test_pool("sr");
        let conn = pool.get().expect("conn");
        conn.execute(
            "INSERT INTO memories (id, namespace, content, importance)              VALUES ('m_f', 'agent/test', '己记忆', 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities(id, namespace, entity_type, name, aliases, summary)              VALUES ('e1', 'agent/test', 'person', '李四', '[]', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_mentions(entity_id, memory_id, context, namespace)              VALUES ('e1', 'm_f', '提及李四', 'agent/test')",
            [],
        )
        .unwrap();
        drop(conn);
        let mk_f = || mk_result("m_f", "己记忆");
        let mut results: Vec<FusedResult> = vec![mk_f()];
        rerank_by_cooccurrence(&pool, "agent/test", "李四", &mut results);
        // 单结果 + 匹配查询 → query-hit boost 生效（guard 未把单结果短路）。
        let f = &results[0];
        assert!(
            f.rrf_score > INIT_SCORE && f.source.contains("cooccur"),
            "single-result query-hit boost missing: {:?}",
            (&f.memory_id, f.rrf_score, &f.source)
        );
        // #R66 test/low 注释修正：空查询 + 单结果下**任何路径都无法 boost**
        // （match_query_entities 空 + 单记忆 peer_overlap 恒 0），断言只文档化
        // no-boost 契约、不判别 guard 分支本身（guard 删除也通过）。
        let mut empty_q: Vec<FusedResult> = vec![mk_f()];
        rerank_by_cooccurrence(&pool, "agent/test", "  ", &mut empty_q);
        let f0 = &empty_q[0];
        assert_eq!(f0.rrf_score, INIT_SCORE, "empty query must not boost");
        assert!(!f0.source.contains("cooccur"), "empty query must not add marker");
    }
}
