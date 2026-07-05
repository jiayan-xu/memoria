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
