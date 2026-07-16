//! Self-Evolution 护栏 — 吸收 HMS 的四条确定性控制规则（Phase A / P0+）。
//!
//! HMS 在答案时刻按关键词触发启发式控制（非 ML），覆盖长程记忆推理的典型失败模式：
//! 去重计数 / 相对日期落地 / 金额差值校准 / 当前态vs历史态仲裁。
//! 这里作为 recall 侧 prompt 护栏注记返回，零 LLM 调用、纯规则。
//! 对应 HMS `organizer._controls`。

/// 中英文关键词 → 控制规则注记。
/// 返回命中的规则文本列表（可能为空）。
pub fn guardrails(query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    let mut notes: Vec<String> = Vec::new();

    let count_kw = [
        "how many", "total", "count", "数量", "总数", "几个", "多少个", "累计",
    ];
    if count_kw.iter().any(|k| q.contains(k)) {
        notes.push(
            "COUNT_TOTAL_DEDUP: 先枚举唯一事件再计数，避免把同一事件的多次提及重复累加。".to_string(),
        );
    }

    let date_kw = [
        "ago", "before", "after", "last", "next", "yesterday", "tomorrow", "前", "后", "之前",
        "之后", "昨天", "明天", "上周", "本周", "这周", "上月", "上个月", "下个月", "下月",
        "去年", "今年", "本月", "今天", "今日", "当日", "当天", "近年", "相对",
    ];
    if date_kw.iter().any(|k| q.contains(k)) {
        notes.push(
            "RELATIVE_DATE_GROUNDING: 以问题时间为锚点解析相对日期，落到具体记忆的 occurred/mentioned 区间。".to_string(),
        );
    }

    let amount_kw = [
        "how much", "difference", "cost", "spent", "amount", "差额", "花了多少", "差多少",
        "成本", "余额", "多少钱",
    ];
    if amount_kw.iter().any(|k| q.contains(k)) {
        notes.push(
            "AMOUNT_DIFFERENCE_CALIBRATION: 仅当差值两侧数据齐全时才计算，缺一侧时保守表述而非硬算。".to_string(),
        );
    }

    let state_kw = [
        "current", "latest", "previous", "initially", "before", "当前", "最新", "之前", "最初",
        "原先", "现在", "过去",
    ];
    if state_kw.iter().any(|k| q.contains(k)) {
        notes.push(
            "CURRENT_PREVIOUS_ARBITRATION: 当前态取最新有效版本(occurred 最近)，历史态取最旧版本，勿混用。".to_string(),
        );
    }

    notes
}
