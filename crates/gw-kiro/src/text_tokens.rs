//! 文本 token 估算 —— 🔵 搬自旧 `src/token.rs::count_tokens` + `stream.rs::estimate_tokens`。
//!
//! 纯本地字符级估算,**零网络**:cache_sim 用 [`count_tokens`] 给每条消息估 token
//! (公共前缀命中量),流式收尾用 [`estimate_output_tokens`] 累加 completion。
//!
//! 注意与本 crate 的 [`crate::token`](auth 刷新)同名概念不同:那是 OAuth refreshToken
//! 刷新,本模块是计费用的文本 token 估算,故另起 `text_tokens` 名避免混淆。
//!
//! 计算规则(逐字节对齐旧代码,保证 cache_sim 跨项目同口径):
//! - 非西文字符(中日韩等)按 4 字符单位、西文按 1;4 单位 = 1 token;
//! - 再按档位非线性放大(<100 ×1.5 … ≥800 ×1.0),贴合真实 tokenizer 短文本高开销。

/// 判断字符是否为非西文字符(中日韩/阿拉伯等);西文含 ASCII 与拉丁扩展区。
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        '\u{0000}'..='\u{007F}'
        | '\u{0080}'..='\u{00FF}'
        | '\u{0100}'..='\u{024F}'
        | '\u{1E00}'..='\u{1EFF}'
        | '\u{2C60}'..='\u{2C7F}'
        | '\u{A720}'..='\u{A7FF}'
        | '\u{AB30}'..='\u{AB6F}'
    )
}

/// 估算一段文本的 token 数(本地字符级,非线性档位放大)。
///
/// 🔵 与旧 `token::count_tokens` 逐字节一致:cache_sim 的指纹 token、命中量、total
/// 都依赖它,**不可改动**否则跨轮/跨项目命中比例失真。
pub fn count_tokens(text: &str) -> u64 {
    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    (if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    }) as u64
}

/// 估算一段产出文本的 output token(中文 ~1.5 字/token、英文 ~4 字/token,至少 1)。
///
/// 🔵 搬自旧 `stream.rs::estimate_tokens`:流式逐帧累加 completion(thinking + 正文 +
/// tool_use input 都计入 output)。与 [`count_tokens`] 取不同公式是因 output 估算
/// 历史上独立演化,保持各自行为以贴合旧线上计费值。
pub fn estimate_output_tokens(text: &str) -> i64 {
    let mut chinese_count: i64 = 0;
    let mut other_count: i64 = 0;
    for c in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;
    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_counts_one_unit_per_char() {
        // 纯英文短串:char_units = len,tokens = len/4,<100 档 ×1.5
        let t = count_tokens("hello world"); // 11 chars → 2.75 → ×1.5 = 4
        assert_eq!(t, 4);
    }

    #[test]
    fn cjk_counts_four_units_per_char() {
        // 4 个中文 → 16 units → 4 tokens → <100 档 ×1.5 = 6
        assert_eq!(count_tokens("你好世界"), 6);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn long_text_no_multiplier() {
        // ≥800 token 档 ×1.0:4000 ascii → 1000 units → 1000 tokens(无放大)
        let big = "a".repeat(4000);
        assert_eq!(count_tokens(&big), 1000);
    }

    #[test]
    fn output_estimate_min_one() {
        assert_eq!(estimate_output_tokens(""), 1, "至少 1");
        assert_eq!(estimate_output_tokens("hi"), 1); // (2+3)/4 = 1
    }

    #[test]
    fn output_estimate_chinese_vs_english() {
        // 3 个中文 → (3*2+2)/3 = 2;8 个英文 → (8+3)/4 = 2
        assert_eq!(estimate_output_tokens("你好吗"), 2);
        assert_eq!(estimate_output_tokens("abcdefgh"), 2);
    }
}
