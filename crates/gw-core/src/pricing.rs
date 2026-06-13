//! 按 Anthropic 标准价把 token 用量折算成 USD 成本(admin 成本看板用)。
//!
//! 价格按模型族(opus/sonnet/haiku)分档,子串匹配——caio 对外模型名都是
//! `claude-opus-4.x` / `claude-sonnet-4.x` / `claude-haiku-4.5`,够用。
//! 未识别的模型返回 `None`(成本按 0 计、前端可标"未计价"),不瞎猜价。
//!
//! ⚠️ 本模块只做**纯算术**:`cost_usd` 把传入的四个**已分桶**数量各自乘单价相加,
//! 自身**不做任何减法/归一**。它对"input 桶"的语义无知——传进来的 input 必须已是
//! Anthropic 口径的"未命中缓存输入"。
//! caio 的存储口径与此**不同**:`usage_records.input_tokens` 存的是**总上下文**
//! (含 cache_read 子集,见 chat.rs 收尾)。因此调用方(`admin/usage.rs::model_cost`)
//! 必须先 `input - cache_read` 得到未命中输入再传入,否则缓存 token 会被重复计费。
//! 减法的责任在调用方,不在本模块。

/// 单模型四档单价(USD / 1M tokens)。
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
}

impl ModelPrice {
    /// 四桶 token × 各自单价 / 1e6,得本组用量的 USD 成本。
    /// `input` 必须是**未命中缓存**的输入量(调用方负责从总上下文里扣除 cache_read);
    /// 本方法不做减法,见模块头注释。
    pub fn cost_usd(&self, input: u64, output: u64, cache_read: u64, cache_creation: u64) -> f64 {
        const PER: f64 = 1_000_000.0;
        (input as f64) * self.input / PER
            + (output as f64) * self.output / PER
            + (cache_read as f64) * self.cache_read / PER
            + (cache_creation as f64) * self.cache_creation / PER
    }
}

// Anthropic 标准价(2026-06 口径,USD/1M)。与 kiro.rs PricingConfig 对齐。
const OPUS: ModelPrice = ModelPrice { input: 5.0, output: 25.0, cache_read: 0.5, cache_creation: 6.25 };
const SONNET: ModelPrice = ModelPrice { input: 3.0, output: 15.0, cache_read: 0.3, cache_creation: 3.75 };
const HAIKU: ModelPrice = ModelPrice { input: 1.0, output: 5.0, cache_read: 0.1, cache_creation: 1.25 };

/// 按模型名(不分大小写、子串)取单价;未识别 → `None`。
pub fn price_for(model: &str) -> Option<ModelPrice> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        Some(OPUS)
    } else if m.contains("sonnet") {
        Some(SONNET)
    } else if m.contains("haiku") {
        Some(HAIKU)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_matches_family_case_insensitive() {
        assert!((price_for("claude-opus-4-8").unwrap().input - 5.0).abs() < 1e-9);
        assert!((price_for("Claude-Sonnet-4.6").unwrap().output - 15.0).abs() < 1e-9);
        assert!((price_for("claude-haiku-4-5-20251001").unwrap().cache_read - 0.1).abs() < 1e-9);
        assert!(price_for("gpt-4").is_none());
    }

    #[test]
    fn cost_sums_four_buckets() {
        // opus: 1M in *5 + 1M out *25 + 1M cr *0.5 + 1M cc *6.25 = 36.75
        let c = OPUS.cost_usd(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((c - 36.75).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn cost_zero_for_empty() {
        assert_eq!(SONNET.cost_usd(0, 0, 0, 0), 0.0);
    }
}
