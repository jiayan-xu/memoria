//! 写路径敏感信息脱敏（Phase A P0-1，吸收自 WeKnora `internal/types/memory.go`，MIT）。
//!
//! 威胁模型：记忆会被每轮重发到模型（system prompt / 召回注入），落在这里的凭据
//! 不是「被保存」而是「被反复外发」。所以在写入第一道就剥掉，而不是等召回时再过滤。
//!
//! 模式清单刻意具体而非聪明：上一版宽松匹配的教训是把普通长订单号搅碎、却仍留下
//! 身份证尾段——两头皆输（用户丢了正确记忆、留下了敏感的那条）。
//!
//! Rust regex 与 Go 的语义差异：本 crate 的 `\b` 是 Unicode 词边界（汉字算词字符），
//! 但中文模式仍不加 `\b`——「密钥」出现在句中时前后都是词字符、本就没有边界；
//! 值域以 CJK 标点截断同理：中文没有空格，贪婪 `\S+` 会吞掉整句。

use regex::Regex;
use std::sync::OnceLock;

/// 被移除内容的显式占位符：用户浏览记忆列表时应能看出「有东西被摘掉了」，
/// 而不是被静默搅碎。
pub const PLACEHOLDER: &str = "【已隐藏】";

/// 紧急关闭开关：`MEMORIA_REDACT=0` 时写路径原样入库（脱敏误伤业务数据时临时放行）。
pub fn redact_enabled() -> bool {
    std::env::var("MEMORIA_REDACT").ok().as_deref() != Some("0")
}

static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

fn patterns() -> &'static [Regex] {
    PATTERNS.get_or_init(|| {
        let specs: &[&str] = &[
            // Provider tokens，按各自文档化的前缀匹配
            r"\bsk-[A-Za-z0-9_\-]{16,}",
            r"\bsk_(live|test)_[A-Za-z0-9]{16,}",
            r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
            r"\bgithub_pat_[A-Za-z0-9_]{20,}",
            r"\b(AKIA|ASIA)[0-9A-Z]{16}",
            r"\bxox[baprs]-[A-Za-z0-9\-]{10,}",
            r"\bAIza[0-9A-Za-z_\-]{35}",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            // 自称密钥的赋值：值止于空白或 CJK 标点（中文无空格，贪婪匹配会连
            // 整句一起吞掉）。`(?i)` 只作用于 ASCII 关键词，中文键名走下一条。
            r"(?i)\b(password|passwd|pwd|secret|token|api[_\- ]?key|access[_\- ]?key)\b\s*[:=＝：]\s*[^\s，。、；：！？,;]+",
            // 中文键名赋值，故意不加 `\b`（句中无边界）；「是为」允许自然语句省略冒号
            r"(密码|口令|密钥|秘钥)\s*[:=＝：是为]?\s*[^\s，。、；：！？,;]+",
            // 大陆身份证：锚定合理出生日期，避免误伤长订单号等 18 位串
            r"\b[1-9]\d{5}(19|20)\d{2}(0[1-9]|1[0-2])(0[1-9]|[12]\d|3[01])\d{3}[\dXx]\b",
            // 银行卡号（可含空格/短横分组）
            r"\b\d{4}[ \-]?\d{4}[ \-]?\d{4}[ \-]?\d{2,7}\b",
            // 大陆手机号
            r"\b1[3-9]\d{9}\b",
            // 40+ 位不透明高熵串：未识别 token 的典型长相
            r"\b[A-Za-z0-9_\-]{40,}\b",
        ];
        specs
            .iter()
            .filter_map(|s| Regex::new(s).ok())
            .collect()
    })
}

/// 剥掉凭据与身份证号。第二个返回值报告是否有内容被移除。
pub fn redact_sensitive(content: &str) -> (String, bool) {
    let mut redacted = content.to_string();
    for re in patterns() {
        redacted = re.replace_all(&redacted, PLACEHOLDER).into_owned();
    }
    let changed = redacted != content;
    (redacted, changed)
}

/// 内容被剥到只剩占位符时不再值得存储——存下来的是一条占位符而非记忆。
/// 阈值：去掉占位符后有效字符（去空白）不足 6 个。
pub fn is_mostly_redacted(content: &str) -> bool {
    let remaining: usize = content
        .replace(PLACEHOLDER, "")
        .trim()
        .chars()
        .count();
    remaining < 6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_tokens_redacted() {
        let (out, changed) =
            redact_sensitive("用 sk-abcdefghijklmnopqrst 更新了配置");
        assert!(changed);
        assert!(out.contains(PLACEHOLDER));
        assert!(!out.contains("sk-abcdefghijklmnopqrst"));
    }

    #[test]
    fn chinese_key_assignment_stops_at_cjk_punct() {
        // 值止于句号：整句其余部分必须保留
        let (out, changed) = redact_sensitive("数据库密码是 Hunter2#2026。每周一轮换。");
        assert!(changed);
        assert!(out.contains(PLACEHOLDER));
        assert!(out.contains("每周一轮换"), "句尾不应被吞: {out}");
        assert!(!out.contains("Hunter2"));
    }

    #[test]
    fn english_key_assignment_redacted() {
        let (out, changed) = redact_sensitive("API_KEY=abcdefgh12345678 in .env");
        assert!(changed);
        assert!(!out.contains("abcdefgh12345678"));
    }

    #[test]
    fn resident_id_redacted_but_order_number_not() {
        let (out, changed) = redact_sensitive("身份证 11010519900307891X 已登记");
        assert!(changed);
        assert!(!out.contains("11010519900307891X"));
        // 18 位但出生日期段非法（13 月 45 日）→ 长订单号，不应命中
        let (out2, changed2) = redact_sensitive("订单号 11010513904507891X 请核对");
        assert!(!changed2, "订单号被误伤: {out2}");
    }

    #[test]
    fn phone_redacted() {
        let (_, changed) = redact_sensitive("联系 13812345678 确认");
        assert!(changed);
    }

    #[test]
    fn high_entropy_blob_redacted() {
        // 44 位十六进制串（≥40 阈值；注意 base64 尾部 '=' 不在字符类内，串长按
        // 去掉填充符计），未识别 token 的典型长相
        let (out, changed) = redact_sensitive("blob: a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2");
        assert!(changed);
        assert!(out.contains(PLACEHOLDER));
    }

    #[test]
    fn normal_text_untouched() {
        let (out, changed) = redact_sensitive("门店排班管理系统本周上线，覆盖华东全部门店。");
        assert!(!changed);
        assert_eq!(out, "门店排班管理系统本周上线，覆盖华东全部门店。");
    }

    #[test]
    fn mostly_redacted_detection() {
        assert!(is_mostly_redacted("密码 【已隐藏】"));
        assert!(!is_mostly_redacted(
            "数据库密码是 【已隐藏】，轮换流程见运维手册"
        ));
    }
}
