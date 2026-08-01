//! FTS5 full-text search operations (delegated to search::keyword).
//! Phase 1.4: FTS5 query logic lives in search/keyword.rs.
//! This module provides only tokenize() for search/keyword.rs.
#![allow(dead_code)]

use jieba_rs::Jieba;
use std::sync::OnceLock;

static JIEBA: OnceLock<Jieba> = OnceLock::new();

fn jieba() -> &'static Jieba {
    JIEBA.get_or_init(|| Jieba::new())
}

/// Tokenize Chinese text with jieba, returning space-separated tokens.
/// Always uses jieba (handles mixed Chinese/English correctly).
pub fn tokenize(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let words = jieba().cut(text, true);
    words.join(" ")
}

/// Tokenize a query for FTS5 `MATCH`.
///
/// 根因（多次迭代定位）：
/// 1. 索引用 jieba 分词（「权限」作为整体 token 入索引），故查询也必须用 jieba，不能用 unicode61 的逐字切分。
/// 2. 本机 FTS5 构建中，**空格分隔的多个 term 被当作 AND**（实测 `MATCH 'agent 权限'` ≈ 仅「权限」命中、
///    而 `MATCH 'agent OR 权限'` = 39953），导致原 `tokenize()` 用空格连接多词时几乎恒为空。
///    → 用 jieba 切词后以显式 ` OR ` 连接，得到「或」宽召回。
/// 3. **带连字符/标点的 token 会破坏 FTS5 查询语法**：`MATCH 'agent-core'` 直接报
///    `no such column: core`（FTS5 把 `-` 后的 `core` 当列名解析），导致整条 FTS 查询失败、
///    keyword 通道恒空。→ 对每个 jieba token 用双引号包裹（`"agent-core"`），
///    FTS5 将其当作字符串字面量（phrase）安全匹配；嵌入的 `"` 用 `""` 转义。
/// 4. **代码符号 token 与索引拆存错配（2026-07-24 定位，keyword 通道 0% 召回根因）**：
///    jieba-rs 把 `RUST_REWRITE_PLAN.md` / `FEISHU_ALLOW_GROUPS` / `_sync_exceptions.py` /
///    `agent.core.Agent.run` 等当作**整体 token** 保留 → 生成 `"RUST_REWRITE_PLAN.md"` 短语；
///    但 FTS **索引**把这些符号按 `_`/`.`/`/`/`-` 拆开存储（rust/rewrite/plan/md）。
///    DB 实测：`MATCH '"RUST_REWRITE_PLAN.md"'` → 0 命中；`MATCH '"RUST" OR "REWRITE" OR "PLAN" OR "md"'` → 命中。
///    → 对含 `_`/`.`/`/`/`-` 的 token，**额外拆出子 token**（保留整体短语 + 拆词），使查询 token 与索引拆存对齐。
///    FTS5 MATCH 对 ASCII 大小写不敏感，子 token 大小写无关。
pub fn tokenize_for_fts(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 判断 token 是否值得作为 FTS OR 项：
    // - 丢弃纯分隔符/标点（. _ / - [ ] ( ) * : 等）与空串；
    //   [pattern] 前缀被 jieba 切成 '[' 'pattern' ']' —— 方括号是全库 [pattern] 记忆
    //   共有的泛化 token，OR 宽召回下 1240 条同质记忆全部命中，目标 BM25 rank 被挤出
    //   keyword 通道 limit，导致 kw_bm25=None 融合被碾压（2026-08-01 召回率根因定位）。
    // - 丢弃单 CJK 字（jieba-rs 对未登录词会拆成单字，噪声爆棚，淹没有区分度词）；
    // - 其余（≥2 字符的中文/英文/数字/混合）保留。
    let keep = |t: &str| -> bool {
        let t = t.trim();
        if t.is_empty() {
            return false;
        }
        // [pattern] 前缀的残留英文 token：jieba 切出 '[' 'pattern' ']'，标点已被上面过滤，
        // 但 'pattern' 是英文词会漏过——它是全库 [pattern] 记忆共有的泛化 token，
        // OR 宽召回下让同质记忆互相淹没（2026-08-01 召回率根因）。
        if t.eq_ignore_ascii_case("pattern") {
            return false;
        }
        if t.chars().all(|c| {
            c == '_' || c == '.' || c == '/' || c == '-' || c == '['
                || c == ']' || c == '(' || c == ')' || c == '*' || c == ':'
                || c == '|' || c == '，' || c == '。' || c == '、' || c == '：'
                || c == '（' || c == '）' || c == '“' || c == '”' || c == '；'
                || c == '!' || c == '？' || c == '％' || c == '%'
        }) {
            return false; // 纯标点/分隔符
        }
        let chars: Vec<char> = t.chars().collect();
        let all_cjk = chars.iter().all(|c| (0x4E00..=0x9FFF).contains(&(*c as u32)));
        if all_cjk && chars.len() < 2 {
            return false; // 单 CJK 字噪声
        }
        true
    };
    let push = |out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, tok: &str| {
        let t = tok.trim();
        if !keep(t) {
            return;
        }
        let q = format!("\"{}\"", t.replace('"', "\"\"")); // 引号包裹避免连字符/标点破坏 FTS5 语法
        if seen.insert(q.clone()) {
            out.push(q);
        }
    };
    for w in jieba().cut(text, true) {
        let w = w.to_string();
        if w.trim().is_empty() {
            continue;
        }
        // 先保留整体短语（兼容索引中仍以整体存在的 token）
        push(&mut out, &mut seen, &w);
        // 代码符号额外拆子 token，对齐索引拆存；拆出的子 token 仍经 keep() 过滤
        if w.contains('_') || w.contains('.') || w.contains('/') || w.contains('-') {
            for sub in w.split(|c| c == '_' || c == '.' || c == '/' || c == '-') {
                push(&mut out, &mut seen, sub);
            }
        }
    }
    out.join(" OR ")
}
