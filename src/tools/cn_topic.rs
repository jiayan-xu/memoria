//! 中文 topic 双归一化器（Phase A P0-3，吸收自 WeKnora `internal/types/memory.go`，MIT）。
//!
//! 两个归一化器**故意不同**，不能共用：
//! - [`normalize_topic_key`]（保序）：只剥真正无信息的字与尾缀。topic 身份上
//!   词序是身份的一部分——排序去重会把「门店排班管理」和「门店的排班管理」
//!   判成不同主题（多一个字），却把任意 anagram 判成同一主题，两个方向都错。
//!   同义词、不同措辞这类非表面变化留给召回/解析层（它们不止字符串比较可用）。
//! - [`normalize_memory_key`]（token 排序去重）：记忆项冲突检测用。「偏好 数据库」
//!   与「数据库 偏好」是同一主题。CJK 无分词，每个汉字自成一个 token；
//!   连续西文字母/数字聚成一个词 token。
//!
//! 配套 [`topic_similarity`]（字符 bigram 的 Dice 系数：中文无词分隔，bigram 比
//! 整词稳；Dice 比 Jaccard 宽容一方更长——「排班管理」vs「门店排班管理」的常见
//! 情形）与低熵门控 [`topic_is_specific_enough`]（两字标签上一个共享 bigram 就
//! 占了大半分数，模糊匹配主要产假合并；短标签应落回更慢更准的解析层）。

const TOPIC_NOISE_RUNES: &[char] = &[
    '的', '了', '地', '得', '之', '与', '和', '及', '在', '是', '有', '个', '等', '对', '于',
];

const TOPIC_NOISE_WORDS: &[&str] = &[
    "相关问题", "相关", "问题", "方面", "情况", "事宜", "工作", "方向",
];

fn is_han(c: char) -> bool {
    // CJK 统一表意文字 + 扩展 A（覆盖 unicode.Han 的绝大多数实际输入）
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}')
}

/// topic 身份键：保序、剥噪声字与一个尾缀限定词，截断到 120 字。
pub fn normalize_topic_key(topic: &str) -> String {
    let lowered = topic.trim().to_lowercase();
    if lowered.is_empty() {
        return String::new();
    }
    let mut key = String::new();
    for c in lowered.chars() {
        if TOPIC_NOISE_RUNES.contains(&c) {
            continue;
        }
        if is_han(c) || c.is_alphabetic() || c.is_numeric() {
            key.push(c);
        }
    }
    for suffix in TOPIC_NOISE_WORDS {
        if let Some(trimmed) = key.strip_suffix(suffix) {
            if !trimmed.is_empty() {
                key = trimmed.to_string();
                break;
            }
        }
    }
    if key.chars().count() > 120 {
        key.chars().take(120).collect()
    } else {
        key
    }
}

/// 记忆项冲突键：token 集合身份（词序无关）。key 为空时回退用 content。
pub fn normalize_memory_key(key: &str, content: &str) -> String {
    let candidate = if key.trim().is_empty() {
        content
    } else {
        key
    }
    .to_lowercase();

    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in candidate.chars() {
        if is_han(c) {
            // CJK 无词分隔，每个汉字自成 token
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            words.push(c.to_string());
        } else if c.is_alphabetic() || c.is_numeric() {
            current.push(c);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }

    words.sort_unstable();
    words.dedup();
    words.join(" ")
}

fn topic_bigrams(topic: &str) -> Vec<String> {
    let runes: Vec<char> = normalize_topic_key(topic).chars().collect();
    if runes.is_empty() {
        return Vec::new();
    }
    if runes.len() == 1 {
        return vec![runes[0].to_string()];
    }
    runes.windows(2).map(|w| w.iter().collect()).collect()
}

/// 两个 topic 标签的 bigram Dice 相似度 ∈ [0,1]。
pub fn topic_similarity(a: &str, b: &str) -> f64 {
    let left = topic_bigrams(a);
    let right = topic_bigrams(b);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let mut shared = 0usize;
    for g in &left {
        if right.contains(g) {
            shared += 1;
        }
    }
    2.0 * shared as f64 / (left.len() + right.len()) as f64
}

/// 低熵门控：归一后不足 4 字的标签不做模糊匹配（假合并主要来源）。
pub fn topic_is_specific_enough(topic: &str) -> bool {
    normalize_topic_key(topic).chars().count() >= 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_key_strips_noise_and_suffix() {
        assert_eq!(
            normalize_topic_key("门店的排班管理"),
            normalize_topic_key("门店排班管理")
        );
        assert_eq!(normalize_topic_key("PostgreSQL 连接池问题"), "postgresql连接池");
        assert_eq!(normalize_topic_key("Redis"), "redis");
        assert_eq!(normalize_topic_key("  固废 车次 异常  "), "固废车次异常");
    }

    #[test]
    fn topic_key_preserves_order_anagram_sensitive() {
        // 词序是身份：anagram 不得判同（这正是不能与 memory_key 共用的原因）
        assert_ne!(normalize_topic_key("abc"), normalize_topic_key("cba"));
    }

    #[test]
    fn memory_key_order_insensitive() {
        assert_eq!(
            normalize_memory_key("偏好 数据库", ""),
            normalize_memory_key("数据库 偏好", "")
        );
        // 汉字逐字 token + 西文整词 token 混排
        assert_eq!(
            normalize_memory_key("Redis 缓存策略", ""),
            normalize_memory_key("缓存 策略 redis", "")
        );
        // key 为空回退 content
        assert_eq!(normalize_memory_key("", "生产库已迁到 PostgreSQL"), normalize_memory_key("生产库已迁到 PostgreSQL", ""));
    }

    #[test]
    fn dice_similarity_elaboration() {
        // 模型扩写是常见情形：短标签 vs 长标签应仍有可观相似度
        let s = topic_similarity("排班管理", "门店排班管理");
        assert!(s > 0.5, "sim={s}");
        // 无关主题应低
        let s2 = topic_similarity("排班管理", "固废清运");
        assert!(s2 < 0.2, "sim={s2}");
        assert_eq!(topic_similarity("", "任意"), 0.0);
    }

    #[test]
    fn low_entropy_gate() {
        assert!(!topic_is_specific_enough("会议"));
        assert!(!topic_is_specific_enough("ab"));
        assert!(topic_is_specific_enough("周会安排"));
        assert!(topic_is_specific_enough("PostgreSQL"));
    }
}
