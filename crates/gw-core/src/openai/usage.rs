//! 用量口径转换:Anthropic 线缆上的 `usage` → 两种 OpenAI 形状。
//!
//! **数据源是 SSE 事件里的 usage,不是 [`crate::provider::ChatUsage`]**。
//! 这样两个出站转换器就是「Anthropic 事件流」的纯函数,不用从 worker 额外接线,
//! 也不会与下发给客户端的内容产生分歧(客户看到什么,用量就按什么算)。
//!
//! ## 口径(容易踩)
//!
//! Anthropic 线缆上的 `input_tokens` **不含**缓存部分,缓存另记
//! `cache_read_input_tokens` / `cache_creation_input_tokens`。
//! 而 OpenAI 的 `prompt_tokens` 是**总输入**,`prompt_tokens_details.cached_tokens`
//! 只是其中的子集。所以这里必须**相加**,不能直接搬 `input_tokens` ——
//! 直接搬会让缓存命中高的请求在 NewAPI 侧少记一大截输入。
//!
//! ## 对 cursor 的核对(2026-08-14)
//!
//! 这条口径依赖发流方遵守上面那个约定。当前唯一走 OpenAI 线缆的是 cursor:
//! `gw-cursor/src/chat.rs` 的 `delta_usage_json` 发 `input_tokens = 总输入 − cache_read`
//! 并单列 `cache_read_input_tokens`,**且明确「绝不出 `cache_creation_input_tokens`」**
//! (cursor 没有缓存创建计数)。所以 `input + cache_read + cache_creation`
//! 正好还原总输入,`cache_creation` 那一项恒为 0,没有重复计算。
//!
//! ⚠️ 若将来把 OpenAI 入口开给别的 provider,**先核对它发的 `input_tokens`
//! 是不是净额**。发全额的 provider 会在这里被多算一遍缓存 = NewAPI 侧超收
//! (对抗评审 Skeptic#6 指出的隐患)。

use serde_json::{json, Value};

/// 跨事件累积的用量。`message_start` 给输入侧,`message_delta` 给最终输出侧。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageAccum {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl UsageAccum {
    /// 并入一个 Anthropic `usage` 对象(`message_start.message.usage` 或
    /// `message_delta.usage`)。
    ///
    /// 逐字段取**较大值**而不是覆盖:`message_delta` 通常只带 `output_tokens`,
    /// 覆盖会把 `message_start` 给的输入侧清成 0;而 `output_tokens` 在
    /// `message_start` 里常是占位的 1,取大值正好被最终值顶掉。
    pub fn merge(&mut self, usage: &Value) {
        let get = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.input = self.input.max(get("input_tokens"));
        self.output = self.output.max(get("output_tokens"));
        self.cache_read = self.cache_read.max(get("cache_read_input_tokens"));
        self.cache_creation = self.cache_creation.max(get("cache_creation_input_tokens"));
    }

    /// 总输入 = 未缓存输入 + 缓存读 + 缓存写。见模块文档的口径说明。
    pub fn prompt_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output)
    }

    /// 有没有拿到过任何用量(全 0 = 上游一个 usage 字段都没给)。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// ChatCompletions 形状。
    pub fn chat_json(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens(),
            "completion_tokens": self.output,
            "total_tokens": self.total_tokens(),
            "prompt_tokens_details": {"cached_tokens": self.cache_read},
            "completion_tokens_details": {"reasoning_tokens": 0},
        })
    }

    /// Responses 形状。
    ///
    /// `reasoning_tokens` 恒 0:Anthropic 线缆把思考文本算进 `output_tokens`,
    /// 不单独给数。报一个猜出来的值会让客户按它对账,那比留 0 更糟。
    pub fn responses_json(&self) -> Value {
        json!({
            "input_tokens": self.prompt_tokens(),
            "input_tokens_details": {"cached_tokens": self.cache_read},
            "output_tokens": self.output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": self.total_tokens(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缓存部分计入总输入_而不是被丢掉() {
        let mut u = UsageAccum::default();
        u.merge(&json!({"input_tokens": 100, "cache_read_input_tokens": 900,
                        "cache_creation_input_tokens": 50, "output_tokens": 1}));
        u.merge(&json!({"output_tokens": 42}));
        assert_eq!(u.prompt_tokens(), 1050);
        assert_eq!(u.output, 42);
        assert_eq!(u.total_tokens(), 1092);

        let c = u.chat_json();
        assert_eq!(c["prompt_tokens"], json!(1050));
        assert_eq!(c["completion_tokens"], json!(42));
        assert_eq!(c["total_tokens"], json!(1092));
        assert_eq!(c["prompt_tokens_details"]["cached_tokens"], json!(900));

        let r = u.responses_json();
        assert_eq!(r["input_tokens"], json!(1050));
        assert_eq!(r["input_tokens_details"]["cached_tokens"], json!(900));
        assert_eq!(r["output_tokens"], json!(42));
    }

    #[test]
    fn message_delta_只带输出时不清掉输入() {
        let mut u = UsageAccum::default();
        u.merge(&json!({"input_tokens": 10, "output_tokens": 1}));
        u.merge(&json!({"output_tokens": 7}));
        assert_eq!(u.input, 10, "覆盖式合并会把输入清 0,这里必须取较大值");
        assert_eq!(u.output, 7);
    }

    #[test]
    fn 一个字段都没给时判空() {
        let mut u = UsageAccum::default();
        assert!(u.is_empty());
        u.merge(&json!({}));
        assert!(u.is_empty());
        u.merge(&json!({"output_tokens": 1}));
        assert!(!u.is_empty());
    }
}
