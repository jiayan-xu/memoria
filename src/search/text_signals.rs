//! P2 / M2.1：轻量 text_signals 抽取与检索加成（O5：无 cross-encoder、无 LLM）。
//!
//! 从记忆正文确定性提取 numeric / date / update 信号，供：
//! - `memory_context` ledger 显式化（O6）
//! - hybrid 检索后小幅加成重排（与 cooccur 同级，保守幅度）

use crate::search::rrf::FusedResult;
use serde_json::{json, Value};

const MAX_NUMBERS: usize = 8;
const MAX_DATES: usize = 4;
const MAX_UPDATE_MARKERS: usize = 4;

const NUMBER_HIT_BOOST: f64 = 0.010;
const DATE_HIT_BOOST: f64 = 0.015;
const MAX_TOTAL_BOOST: f64 = 0.05;

static UPDATE_MARKERS: &[&str] = &[
  "更新",
  "变更",
  "改为",
  "改成",
  "调整",
  "升级",
  "目前",
  "现在",
  "之前",
  "原先",
  "最新",
  "supersed",
  "updated",
  "changed",
  "currently",
  "previously",
];

/// Phase B 共现 rerank 同级：`MEMORIA_TEXT_SIGNALS_RERANK=0/false/off` 关闭。
pub fn text_signals_rerank_enabled() -> bool {
  match std::env::var("MEMORIA_TEXT_SIGNALS_RERANK") {
    Ok(v) => {
      let t = v.trim().to_ascii_lowercase();
      !(t == "0" || t == "false" || t == "off" || t == "no")
    }
    Err(_) => true,
  }
}

/// 从正文 + tags + 已解析 occurred 抽取轻量 text_signals（不落库、不写列）。
pub fn extract_text_signals(content: &str, tags_json: &str, occurred_date: Option<&str>) -> Value {
  let mut numbers = extract_numbers(content);
  let mut dates = extract_dates(content);
  let mut update_markers = extract_update_markers(content);

  if let Some(tag_date) = occurred_date {
    if !tag_date.is_empty() && !dates.iter().any(|d| d == tag_date) {
      dates.push(tag_date.to_string());
    }
  }

  // tags 中的 supersede / pattern 提示更新语义
  let tags_lower = tags_json.to_ascii_lowercase();
  if tags_lower.contains("supersed") || tags_lower.contains("\"pattern\"") {
    push_unique(&mut update_markers, "supersede_chain");
  }

  truncate(&mut numbers, MAX_NUMBERS);
  truncate(&mut dates, MAX_DATES);
  truncate(&mut update_markers, MAX_UPDATE_MARKERS);

  json!({
    "numbers": numbers,
    "dates": dates,
    "update_markers": update_markers,
  })
}

/// hybrid 检索：查询中的数字/日期与候选正文信号重叠则小幅加成。
pub fn rerank_by_text_signals(query: &str, results: &mut Vec<FusedResult>) {
  if !text_signals_rerank_enabled() || results.is_empty() {
    return;
  }
  let q_nums = extract_numbers(query);
  let q_dates = extract_dates(query);
  if q_nums.is_empty() && q_dates.is_empty() {
    return;
  }

  for r in results.iter_mut() {
    let sig = extract_text_signals(&r.content, "[]", None);
    let nums = sig["numbers"].as_array();
    let dates = sig["dates"].as_array();
    let mut boost = 0.0;

    if let (Some(qn), Some(cn)) = ((!q_nums.is_empty()).then_some(&q_nums), nums) {
      for q in qn {
        if cn.iter().any(|v| v.as_str() == Some(q.as_str())) {
          boost += NUMBER_HIT_BOOST;
        }
      }
    }
    if let (Some(qd), Some(cd)) = ((!q_dates.is_empty()).then_some(&q_dates), dates) {
      for q in qd {
        if cd.iter().any(|v| v.as_str() == Some(q.as_str())) {
          boost += DATE_HIT_BOOST;
        }
      }
    }

    if boost > MAX_TOTAL_BOOST {
      boost = MAX_TOTAL_BOOST;
    }
    if boost > 0.0 {
      r.rrf_score += boost;
      if !r.source.contains("text_signals") {
        r.source = format!("{};text_signals", r.source);
      }
    }
  }

  results.sort_by(|a, b| {
    b.rrf_score
      .partial_cmp(&a.rrf_score)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
}

fn extract_numbers(text: &str) -> Vec<String> {
  let mut out = Vec::new();
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if !bytes[i].is_ascii_digit() {
      i += 1;
      continue;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
      i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
      let dot = i;
      i += 1;
      if i < bytes.len() && bytes[i].is_ascii_digit() {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
          i += 1;
        }
      } else {
        i = dot;
      }
    }
    let num = &text[start..i];
    if num.len() >= 1 && num.len() <= 24 {
      push_unique(&mut out, num);
    }
  }
  out
}

fn extract_dates(text: &str) -> Vec<String> {
  let mut out = Vec::new();
  let bytes = text.as_bytes();
  let mut i = 0;
  while i + 10 <= bytes.len() {
    if is_iso_date_at(text, i) {
      push_unique(&mut out, &text[i..i + 10]);
      i += 10;
      continue;
    }
    if i + 10 <= bytes.len()
      && bytes[i].is_ascii_digit()
      && bytes[i + 1].is_ascii_digit()
      && bytes[i + 2].is_ascii_digit()
      && bytes[i + 3].is_ascii_digit()
      && bytes[i + 4] == b'/'
      && bytes[i + 5].is_ascii_digit()
      && bytes[i + 6].is_ascii_digit()
      && bytes[i + 7] == b'/'
      && bytes[i + 8].is_ascii_digit()
      && bytes[i + 9].is_ascii_digit()
    {
      let d = format!(
        "{}-{}-{}",
        &text[i..i + 4],
        &text[i + 5..i + 7],
        &text[i + 8..i + 10]
      );
      push_unique(&mut out, &d);
      i += 10;
      continue;
    }
    i += 1;
  }
  out
}

fn is_iso_date_at(text: &str, i: usize) -> bool {
  let b = text.as_bytes();
  if i + 10 > b.len() {
    return false;
  }
  b[i..i + 4].iter().all(|c| c.is_ascii_digit())
    && b[i + 4] == b'-'
    && b[i + 5].is_ascii_digit()
    && b[i + 6].is_ascii_digit()
    && b[i + 7] == b'-'
    && b[i + 8].is_ascii_digit()
    && b[i + 9].is_ascii_digit()
}

fn extract_update_markers(text: &str) -> Vec<String> {
  let lower = text.to_ascii_lowercase();
  let mut out = Vec::new();
  for m in UPDATE_MARKERS {
    if lower.contains(&m.to_ascii_lowercase()) {
      push_unique(&mut out, m);
    }
  }
  out
}

fn push_unique(v: &mut Vec<String>, s: &str) {
  if s.is_empty() {
    return;
  }
  if !v.iter().any(|x| x == s) {
    v.push(s.to_string());
  }
}

fn truncate(v: &mut Vec<String>, max: usize) {
  v.truncate(max);
}

#[cfg(test)]
mod unit_tests {
  use super::*;

  #[test]
  fn extract_numbers_and_dates() {
    let sig = extract_text_signals("2026-07-01 进厂 120 吨，改为应急模式", "[]", None);
    let nums = sig["numbers"].as_array().unwrap();
    let dates = sig["dates"].as_array().unwrap();
    assert!(nums.iter().any(|n| n.as_str() == Some("120")));
    assert!(nums.iter().any(|n| n.as_str() == Some("2026")));
    assert!(dates.iter().any(|d| d.as_str() == Some("2026-07-01")));
    let markers = sig["update_markers"].as_array().unwrap();
    assert!(markers.iter().any(|m| m.as_str() == Some("改为")));
  }

  #[test]
  fn occurred_tag_merged_into_dates() {
    let sig = extract_text_signals("无 inline 日期", r#"["occurred:2025-03-15"]"#, Some("2025-03-15"));
    let dates = sig["dates"].as_array().unwrap();
    assert!(dates.iter().any(|d| d.as_str() == Some("2025-03-15")));
  }

  #[test]
  fn rerank_boosts_numeric_overlap() {
    let mut results = vec![
      FusedResult {
        memory_id: "a".into(),
        content: "库存 120 吨".into(),
        rrf_score: 0.5,
        source: "keyword".into(),
        signal_scores: vec![],
      },
      FusedResult {
        memory_id: "b".into(),
        content: "无关天气".into(),
        rrf_score: 0.51,
        source: "keyword".into(),
        signal_scores: vec![],
      },
    ];
    rerank_by_text_signals("120 吨", &mut results);
    assert_eq!(results[0].memory_id, "a");
    assert!(results[0].source.contains("text_signals"));
  }
}
