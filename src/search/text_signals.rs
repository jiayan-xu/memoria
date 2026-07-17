//! P2 / M2.1：轻量 text_signals 抽取与检索加成（O5：无 cross-encoder、无 LLM）。
//!
//! 从记忆正文确定性提取 numeric / date / update 信号，供：
//! - `memory_context` ledger 显式化（O6）
//! - hybrid 检索后小幅加成重排（与 cooccur 同级，保守幅度）
//! - P2.2c：`signal:*` tags 持久化（写入 remember；读时合并回 text_signals）

use crate::search::rrf::FusedResult;
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::{json, Value};

const MAX_NUMBERS: usize = 8;
const MAX_DATES: usize = 4;
const MAX_UPDATE_MARKERS: usize = 4;

const NUMBER_HIT_BOOST: f64 = 0.010;
const DATE_HIT_BOOST: f64 = 0.015;
const MAX_TOTAL_BOOST: f64 = 0.05;

/// P2.2c：tags 持久化前缀（对齐 O3 `occurred:` 风格）。
pub const SIGNAL_NUM_PREFIX: &str = "signal:num:";
pub const SIGNAL_DATE_PREFIX: &str = "signal:date:";
pub const SIGNAL_UPDATE_PREFIX: &str = "signal:update:";

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

/// P2.2c：`MEMORIA_TEXT_SIGNALS_PERSIST=0/false/off` 关闭写入 tags 持久化（读时仍解析已有 tag）。
pub fn text_signals_persist_enabled() -> bool {
  match std::env::var("MEMORIA_TEXT_SIGNALS_PERSIST") {
    Ok(v) => {
      let t = v.trim().to_ascii_lowercase();
      !(t == "0" || t == "false" || t == "off" || t == "no")
    }
    Err(_) => true,
  }
}

/// 从 tags JSON 解析已持久化的 `signal:*` 信号。
pub fn parse_persisted_signal_tags(tags_json: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
  let tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
  let mut numbers = Vec::new();
  let mut dates = Vec::new();
  let mut update_markers = Vec::new();
  for t in tags {
    let s = t.trim();
    if let Some(v) = s.strip_prefix(SIGNAL_NUM_PREFIX) {
      push_unique(&mut numbers, v);
    } else if let Some(v) = s.strip_prefix(SIGNAL_DATE_PREFIX) {
      push_unique(&mut dates, v);
    } else if let Some(v) = s.strip_prefix(SIGNAL_UPDATE_PREFIX) {
      push_unique(&mut update_markers, v);
    }
  }
  (numbers, dates, update_markers)
}

/// 把 text_signals JSON 转为 `signal:*` tags（上限与读时抽取一致）。
pub fn signal_tags_from_value(signals: &Value) -> Vec<String> {
  let mut out = Vec::new();
  if let Some(nums) = signals["numbers"].as_array() {
    for n in nums.iter().take(MAX_NUMBERS) {
      if let Some(s) = n.as_str() {
        out.push(format!("{SIGNAL_NUM_PREFIX}{s}"));
      }
    }
  }
  if let Some(dates) = signals["dates"].as_array() {
    for d in dates.iter().take(MAX_DATES) {
      if let Some(s) = d.as_str() {
        out.push(format!("{SIGNAL_DATE_PREFIX}{s}"));
      }
    }
  }
  if let Some(markers) = signals["update_markers"].as_array() {
    for m in markers.iter().take(MAX_UPDATE_MARKERS) {
      if let Some(s) = m.as_str() {
        out.push(format!("{SIGNAL_UPDATE_PREFIX}{s}"));
      }
    }
  }
  out
}

/// P2.2c：合并 `signal:*` tags（同前缀替换；保留 occurred / 业务 tag）。
pub fn merge_signal_tags(tags_json: &str, content: &str, occurred_date: Option<&str>) -> String {
  if !text_signals_persist_enabled() {
    return tags_json.to_string();
  }
  let signals = extract_text_signals(content, "[]", occurred_date);
  let signal_tags = signal_tags_from_value(&signals);
  let mut tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
  tags.retain(|t| {
    let s = t.trim();
    !s.starts_with(SIGNAL_NUM_PREFIX)
      && !s.starts_with(SIGNAL_DATE_PREFIX)
      && !s.starts_with(SIGNAL_UPDATE_PREFIX)
  });
  tags.extend(signal_tags);
  serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

/// 从正文 + tags + 已解析 occurred 抽取轻量 text_signals。
/// P2.2c：已持久化的 `signal:*` tags 合并进结果（便于检索/回归）。
pub fn extract_text_signals(content: &str, tags_json: &str, occurred_date: Option<&str>) -> Value {
  let mut numbers = extract_numbers(content);
  let mut dates = extract_dates(content);
  let mut update_markers = extract_update_markers(content);

  if let Some(tag_date) = occurred_date {
    if !tag_date.is_empty() && !dates.iter().any(|d| d == tag_date) {
      dates.push(tag_date.to_string());
    }
  }

  // P2.2：相对中文日期读时解析（锚点 UTC 今天；不写 event_time / 不落库）
  let anchor = chrono::Utc::now().date_naive();
  for d in resolve_relative_dates(content, anchor) {
    push_unique(&mut dates, &d);
  }

  // P2.2c：读时合并 tags 中已持久化的 signal:*
  let (p_nums, p_dates, p_updates) = parse_persisted_signal_tags(tags_json);
  for n in p_nums {
    push_unique(&mut numbers, &n);
  }
  for d in p_dates {
    push_unique(&mut dates, &d);
  }
  for m in p_updates {
    push_unique(&mut update_markers, &m);
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
  let mut q_dates = extract_dates(query);
  let anchor = chrono::Utc::now().date_naive();
  for d in resolve_relative_dates(query, anchor) {
    push_unique(&mut q_dates, &d);
  }
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

/// P2.2：相对中文日期 → `YYYY-MM-DD`（读时、锚点由调用方传入，默认 UTC 今天）。
pub fn resolve_relative_dates(content: &str, anchor: NaiveDate) -> Vec<String> {
  let mut out = Vec::new();

  if content.contains("今天")
    || content.contains("今日")
    || content.contains("当日")
    || content.contains("当天")
  {
    push_unique(&mut out, &fmt_date(anchor));
  }
  if content.contains("昨天") || content.contains("昨日") {
    push_unique(&mut out, &fmt_date(anchor - Duration::days(1)));
  }
  if content.contains("明天") || content.contains("明日") {
    push_unique(&mut out, &fmt_date(anchor + Duration::days(1)));
  }
  if content.contains("前天") {
    push_unique(&mut out, &fmt_date(anchor - Duration::days(2)));
  }
  if content.contains("后天") {
    push_unique(&mut out, &fmt_date(anchor + Duration::days(2)));
  }

  if content.contains("上周") {
    push_unique(&mut out, &fmt_date(start_of_week(anchor) - Duration::days(7)));
  }
  if content.contains("本周") || content.contains("这周") {
    push_unique(&mut out, &fmt_date(start_of_week(anchor)));
  }

  resolve_weekday_phrases(content, anchor, &mut out);

  if content.contains("上月") || content.contains("上个月") {
    push_unique(&mut out, &fmt_date(first_of_month(prev_month(anchor))));
  }
  if content.contains("本月") || content.contains("这个月") {
    push_unique(&mut out, &fmt_date(first_of_month(anchor)));
  }
  if content.contains("下月") || content.contains("下个月") {
    push_unique(&mut out, &fmt_date(first_of_month(next_month(anchor))));
  }
  if content.contains("今年") {
    push_unique(
      &mut out,
      &fmt_date(NaiveDate::from_ymd_opt(anchor.year(), 1, 1).unwrap_or(anchor)),
    );
  }
  if content.contains("去年") {
    push_unique(
      &mut out,
      &fmt_date(NaiveDate::from_ymd_opt(anchor.year() - 1, 1, 1).unwrap_or(anchor)),
    );
  }

  out
}

fn fmt_date(d: NaiveDate) -> String {
  d.format("%Y-%m-%d").to_string()
}

fn start_of_week(d: NaiveDate) -> NaiveDate {
  d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
  NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

fn prev_month(d: NaiveDate) -> NaiveDate {
  if d.month() == 1 {
    NaiveDate::from_ymd_opt(d.year() - 1, 12, 1).unwrap_or(d)
  } else {
    NaiveDate::from_ymd_opt(d.year(), d.month() - 1, 1).unwrap_or(d)
  }
}

fn next_month(d: NaiveDate) -> NaiveDate {
  if d.month() == 12 {
    NaiveDate::from_ymd_opt(d.year() + 1, 1, 1).unwrap_or(d)
  } else {
    NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1).unwrap_or(d)
  }
}

fn resolve_weekday_phrases(content: &str, anchor: NaiveDate, out: &mut Vec<String>) {
  static PAIRS: &[(&str, Weekday)] = &[
    ("一", Weekday::Mon),
    ("二", Weekday::Tue),
    ("三", Weekday::Wed),
    ("四", Weekday::Thu),
    ("五", Weekday::Fri),
    ("六", Weekday::Sat),
    ("日", Weekday::Sun),
  ];
  for (ch, wd) in PAIRS {
    if content.contains(&format!("上周{}", ch)) {
      push_unique(out, &fmt_date(weekday_in_week(anchor, *wd, -1)));
    }
    if content.contains(&format!("本周{}", ch)) || content.contains(&format!("这周{}", ch)) {
      push_unique(out, &fmt_date(weekday_in_week(anchor, *wd, 0)));
    }
  }
}

fn weekday_in_week(anchor: NaiveDate, target: Weekday, week_offset: i64) -> NaiveDate {
  let monday = start_of_week(anchor) + Duration::days(week_offset * 7);
  monday + Duration::days(target.num_days_from_monday() as i64)
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
  fn relative_dates_resolved() {
    let anchor = NaiveDate::from_ymd_opt(2026, 7, 16).unwrap(); // 周四
    let sig = extract_text_signals("上周三开会，昨天登记 120 吨", "[]", None);
    let dates = sig["dates"].as_array().unwrap();
    assert!(
      dates.iter().any(|d| d.as_str() == Some("2026-07-09")),
      "上周三={:?}",
      dates
    );
    assert!(
      dates.iter().any(|d| d.as_str() == Some("2026-07-15")),
      "昨天={:?}",
      dates
    );
    let direct = resolve_relative_dates("本周五交付", anchor);
    assert!(direct.iter().any(|d| d == "2026-07-18"));
  }

  #[test]
  fn signal_tags_persist_and_read_back() {
    let merged = merge_signal_tags("[]", "2026-07-01 进厂 120 吨，改为应急", None);
    let tags: Vec<String> = serde_json::from_str(&merged).unwrap();
    assert!(tags.iter().any(|t| t == "signal:num:120"));
    assert!(tags.iter().any(|t| t == "signal:date:2026-07-01"));
    assert!(tags.iter().any(|t| t == "signal:update:改为"));

    let sig = extract_text_signals("正文无数字", &merged, None);
    let nums = sig["numbers"].as_array().unwrap();
    assert!(nums.iter().any(|n| n.as_str() == Some("120")));
  }

  #[test]
  fn merge_signal_tags_preserves_occurred() {
    let merged = merge_signal_tags(
      r#"["fact","occurred:2026-07-10"]"#,
      "库存 88 件",
      Some("2026-07-10"),
    );
    let tags: Vec<String> = serde_json::from_str(&merged).unwrap();
    assert!(tags.iter().any(|t| t == "occurred:2026-07-10"));
    assert!(tags.iter().any(|t| t == "signal:num:88"));
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
