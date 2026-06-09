//! 块2a:thinking 入口策略 —— 按模型名覆写 thinking 配置(保智力)。
//!
//! 🔵 搬自 kiro.rs(已上线稳定)`anthropic/handlers.rs:override_thinking_from_model_name`。
//! 在请求进入 converter 之前覆写 `req.thinking`/`req.output_config`,让 Opus 全系默认开思考。
//!
//! 规则(优先级从上到下,与 kiro.rs 逐字对齐):
//! - **结构化输出(json_schema)与 thinking 互斥**:客户端要 json_schema 且未显式带 thinking 时,
//!   跳过 thinking 注入(隐藏工具方案要模型只调工具,与推理流冲突,强开会让结构化输出失效)。
//! - **Opus 全系(4.6/4.7/4.8 及后续)默认 adaptive + effort,无需 `-thinking` 后缀**:
//!   客户端已显式配 thinking 则尊重;否则 effort 用 output_config.effort 缺省 "high"。
//! - **非 Opus 但模型名含 `-thinking` 后缀**(sonnet/haiku 等):enabled + 固定 budget。
//! - 其余:不动。
//!
//! 历史教训(kiro.rs 注释):旧实现要求模型名必含 "thinking" 且 adaptive 硬编码仅 opus-4.6,
//! 导致 4.7/4.8 带后缀也退化 enabled、effort 传不进、不带后缀完全无思维链。改为"Opus 系列
//! 默认 adaptive"避免逐版本硬编码过时,也省去客户端配后缀。

use crate::anthropic_types::{MessagesRequest, OutputConfig, Thinking};

const DEFAULT_THINKING_BUDGET: i32 = 20000;
const DEFAULT_EFFORT: &str = "high";

/// 按模型名覆写 thinking 配置。在 converter 之前调用,直接 mutate 请求。
pub fn override_thinking_from_model_name(req: &mut MessagesRequest) {
    let model_lower = req.model.to_lowercase();
    let is_opus = model_lower.contains("opus");
    let has_thinking_suffix = model_lower.contains("thinking");

    // 结构化输出(json_schema)与 thinking 互斥:客户端要 json_schema 且未显式带 thinking → 不强开。
    let wants_structured_output = req
        .output_config
        .as_ref()
        .and_then(|c| c.json_schema())
        .is_some();
    if wants_structured_output && req.thinking.is_none() {
        tracing::info!(
            model = %req.model,
            "检测到 json_schema 结构化输出请求，跳过默认 thinking 注入（互斥）"
        );
        return;
    }

    if is_opus {
        // 客户端已显式配置 thinking 则尊重之,不覆写。
        if req.thinking.is_some() {
            return;
        }
        // effort 客户端传入优先,缺省 high。
        let effort = req
            .output_config
            .as_ref()
            .map(|c| c.effort.clone())
            .unwrap_or_else(|| DEFAULT_EFFORT.to_string());

        tracing::info!(
            model = %req.model,
            thinking_type = "adaptive",
            effort = %effort,
            "Opus 模型默认开启 adaptive 思维链"
        );

        req.thinking = Some(Thinking {
            thinking_type: "adaptive".to_string(),
            display: None,
            budget_tokens: DEFAULT_THINKING_BUDGET,
        });
        req.output_config = Some(OutputConfig {
            effort,
            format: None,
        });
    } else if has_thinking_suffix {
        // 非 Opus 但带 -thinking 后缀:enabled + 固定 budget。
        tracing::info!(
            model = %req.model,
            thinking_type = "enabled",
            "非 Opus 模型名含 thinking 后缀，覆写为 enabled"
        );
        req.thinking = Some(Thinking {
            thinking_type: "enabled".to_string(),
            display: None,
            budget_tokens: DEFAULT_THINKING_BUDGET,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic_types::OutputFormat;

    fn req(model: &str, thinking: Option<Thinking>, oc: Option<OutputConfig>) -> MessagesRequest {
        MessagesRequest {
            model: model.to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking,
            output_config: oc,
            metadata: None,
            context_management: None,
        }
    }

    #[test]
    fn opus_defaults_to_adaptive_without_suffix() {
        let mut r = req("claude-opus-4-8", None, None);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.expect("opus 应默认开 thinking");
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(r.output_config.unwrap().effort, "high");
    }

    #[test]
    fn opus_respects_explicit_client_thinking() {
        let explicit = Thinking { thinking_type: "enabled".to_string(), display: None, budget_tokens: 5000 };
        let mut r = req("claude-opus-4-8", Some(explicit), None);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.unwrap();
        assert_eq!(t.thinking_type, "enabled", "客户端显式 thinking 应被尊重");
        assert_eq!(t.budget_tokens, 5000);
    }

    #[test]
    fn opus_uses_client_effort() {
        let oc = OutputConfig { effort: "xhigh".to_string(), format: None };
        let mut r = req("claude-opus-4-8", None, Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.output_config.unwrap().effort, "xhigh", "effort 客户端优先");
    }

    #[test]
    fn non_opus_with_thinking_suffix_enabled() {
        let mut r = req("claude-sonnet-4-5-thinking", None, None);
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().thinking_type, "enabled");
    }

    #[test]
    fn non_opus_without_suffix_untouched() {
        let mut r = req("claude-sonnet-4-5", None, None);
        override_thinking_from_model_name(&mut r);
        assert!(r.thinking.is_none(), "非 Opus 无后缀不应注入 thinking");
    }

    #[test]
    fn structured_output_excludes_thinking() {
        let oc = OutputConfig {
            effort: "high".to_string(),
            format: Some(OutputFormat {
                format_type: "json_schema".to_string(),
                schema: Some(serde_json::json!({"type": "object"})),
            }),
        };
        let mut r = req("claude-opus-4-8", None, Some(oc));
        override_thinking_from_model_name(&mut r);
        assert!(r.thinking.is_none(), "结构化输出应与 thinking 互斥,不注入");
    }

    #[test]
    fn structured_output_with_explicit_thinking_still_injects() {
        // 客户端同时显式带了 thinking + json_schema:thinking.is_some() → 不触发互斥跳过,
        // 走 Opus 分支但因已有 thinking 而尊重客户端。
        let explicit = Thinking { thinking_type: "adaptive".to_string(), display: None, budget_tokens: 8000 };
        let oc = OutputConfig {
            effort: "high".to_string(),
            format: Some(OutputFormat {
                format_type: "json_schema".to_string(),
                schema: Some(serde_json::json!({"type": "object"})),
            }),
        };
        let mut r = req("claude-opus-4-8", Some(explicit), Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().budget_tokens, 8000, "显式 thinking 应保留");
    }
}
