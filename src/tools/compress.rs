//! F4：摄入压缩蒸馏（启发式，不调 LLM，守 H1/H2）。
//! 超长内容 → 抽取「首句 + 高频关键词句 + 末句」压缩版存 content，原文进 raw_ref。

use crate::storage::fts5::tokenize_for_fts;
use std::collections::HashMap;

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
}

/// 是否以中文为主（用于选择压缩阈值）。
fn is_mostly_chinese(text: &str) -> bool {
    let mut cjk = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        if is_cjk(c) {
            cjk += 1;
        }
    }
    total > 0 && cjk * 2 >= total
}

/// 按中英文句末标点切句。
fn split_sentences(text: &str) -> Vec<String> {
    let mut sents = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if "。！？!?；;\n".contains(c) {
            let t = cur.trim().to_string();
            if !t.is_empty() {
                sents.push(t);
            }
            cur.clear();
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        sents.push(t);
    }
    sents
}

/// 计算 token 词频（jieba 分词，去短噪声词）。
fn token_freq(text: &str) -> HashMap<String, usize> {
    let mut freq = HashMap::new();
    for tok in tokenize_for_fts(text).split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c));
        if t.chars().count() < 2 {
            continue;
        }
        *freq.entry(t.to_string()).or_insert(0) += 1;
    }
    freq
}

/// 压缩：返回 (压缩后内容, 原文 Option)。短内容原样返回 (原文=None)。
pub fn distill(content: &str) -> (String, Option<String>) {
    let cn = is_mostly_chinese(content);
    let threshold: usize = if cn {
        std::env::var("MEMORIA_DISTILL_MAX_CHARS_CN")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(600)
    } else {
        std::env::var("MEMORIA_DISTILL_MAX_CHARS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1200)
    };
    if content.chars().count() <= threshold {
        return (content.to_string(), None);
    }

    let sents = split_sentences(content);
    if sents.len() <= 1 {
        // 单句且超长：直接截断（保留开头），原文进 raw_ref
        let truncated: String = content.chars().take(threshold).collect();
        return (truncated, Some(content.to_string()));
    }

    let freq = token_freq(content);
    let last = sents.len() - 1;
    // 选关键句：首句 + 词频最高的前 K 句（排除首尾）+ 末句
    let mut by_score: Vec<(usize, usize)> = sents
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 0 && *i != last)
        .map(|(i, s)| {
            let score: usize = tokenize_for_fts(s)
                .split_whitespace()
                .map(|tok| {
                    let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c));
                    *freq.get(t).unwrap_or(&0)
                })
                .sum();
            (i, score)
        })
        .collect();
    by_score.sort_by(|a, b| b.1.cmp(&a.1));

    let mut key_idx: Vec<usize> = vec![0];
    if last > 0 {
        key_idx.push(last);
    }
    let k = 3;
    for (i, _) in by_score.into_iter().take(k) {
        key_idx.push(i);
    }
    key_idx.sort_unstable();
    key_idx.dedup();

    let mut compressed: String = key_idx
        .iter()
        .map(|&i| sents[i].clone())
        .collect::<Vec<_>>()
        .join("");
    if compressed.chars().count() > threshold {
        compressed = compressed.chars().take(threshold).collect();
    }
    if compressed.trim().is_empty() {
        compressed = content.chars().take(threshold).collect();
    }
    (compressed, Some(content.to_string()))
}
