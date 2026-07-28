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

use crate::anthropic_types::{MessagesRequest, OutputConfig, Thinking, DEFAULT_EFFORT};

const DEFAULT_THINKING_BUDGET: i32 = 20000;

/// `enabled` 模式的**思考预算下限**。低于此值抬到此值;`0` 关闭本策略。
/// 经 `KIRO_MIN_THINKING_BUDGET` 覆盖。
///
/// 为什么要有:实测生产客户端自带的 budget 差异极大 —— opencode **18/18 条都发 1024**
/// (实测 signature 加密体 5800,只有 xhigh 21984 的 26%,简单问题秒回),而 Claude Code
/// 发 25600。我们按 opus 计费,不该让客户端把它降级成浅思考。
///
/// **这是抬"上限"不是设"目标"**:`max_thinking_length` 是天花板,模型按需取用。实测
/// budget=1024 与 adaptive-low 的总输出是 1621 vs 1820 token,不是成倍增长。
const DEFAULT_MIN_THINKING_BUDGET: i32 = 8192;

/// 抬预算时必须给**答案**留出的 token 余量。
///
/// Anthropic 语义里 thinking 预算算在 `max_tokens` 之内,budget 逼近甚至超过 max_tokens
/// 会让答案没地方写(或被上游判非法)。所以抬到的值还要被 `max_tokens - 本余量` 夹一次,
/// 夹不下就**保持客户端原值不动**。
const ANSWER_HEADROOM_TOKENS: i32 = 1024;

fn min_thinking_budget() -> i32 {
    static V: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("KIRO_MIN_THINKING_BUDGET")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(DEFAULT_MIN_THINKING_BUDGET)
    })
}

/// 把 `enabled` 模式里过低的思考预算抬到下限。
///
/// 只动 `enabled`:
/// - `adaptive` 的深度由 effort 决定,budget 字段不参与,抬了没意义;
/// - `disabled` **不碰** —— 实测生产 48 条 disabled **全是 Claude Code 的内部杂务**
///   (16 条 `max_tokens=64` 的会话标题生成、30 条 haiku 的技能路由),给它们开思考是
///   纯烧钱 + 拖慢 UI,对"用户用上聪明的 Claude"零贡献。
///
/// 返回是否发生了抬升(仅供日志)。
fn raise_low_thinking_budget(req: &mut MessagesRequest) -> bool {
    let floor = min_thinking_budget();
    if floor <= 0 {
        return false;
    }
    let Some(t) = req.thinking.as_mut() else {
        return false;
    };
    if t.thinking_type != "enabled" || t.budget_tokens >= floor {
        return false;
    }
    // 给答案留余量后的可用上限。max_tokens 太小(杂务类小请求)时 cap 会 <= 原值,
    // 此时**保持原样**——宁可不抬,也不能把答案挤没。
    let cap = req.max_tokens.saturating_sub(ANSWER_HEADROOM_TOKENS);
    let target = floor.min(cap);
    if target <= t.budget_tokens {
        return false;
    }
    let from = t.budget_tokens;
    t.budget_tokens = target;
    tracing::info!(
        model = %req.model,
        from,
        to = target,
        max_tokens = req.max_tokens,
        "客户端思考预算过低，已抬到下限(上限抬升，非强制消耗)"
    );
    true
}

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

    // 预算下限:必须在下面"客户端已配 thinking 就 return"之前跑,否则自带 enabled 的请求
    // (opencode 全量、部分 Claude Code)会直接绕过本策略——那正是要修的那批。
    // 与模型无关:opencode 也可能点 sonnet。
    raise_low_thinking_budget(req);

    if is_opus {
        // 客户端已显式配置 thinking 则尊重之,不覆写。
        if req.thinking.is_some() {
            return;
        }
        // effort 客户端传入优先,缺省 xhigh(顶格档)。
        //
        // ⚠️ 旧注释说「high 仅产桩推理」——**这是错的**,2026-07-28 实测已推翻:同一道
        // 证明题下 high 的签名加密体(真 CoT 载体)15940 字节、耗时 95s,是 xhigh
        // (21984 / 124s)的 73%,不是桩。别据此把 high 当废档。
        // 另注:**可见 thinking 文本长度不能用来判断思考深度**——同批实测里 high 的可见
        // 摘要只有 1579 字符(全场最短),而它的加密体反而比 low(10744)大 48%。要看深度
        // 就看 signature 长度和耗时,两者随 effort 严格单调。
        // 此处保留客户端**原始**串(不归一/不告警):合法化与回退统一在
        // `generate_thinking_prefix`(wire 注入唯一出口)做,避免双重告警 + 双重归一。
        let effort = req
            .output_config
            .as_ref()
            .and_then(|c| c.effort.clone())
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
            effort: Some(effort),
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
    fn opus_defaults_to_adaptive_xhigh_without_suffix() {
        let mut r = req("claude-opus-4-8", None, None);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.expect("opus 应默认开 thinking");
        assert_eq!(t.thinking_type, "adaptive");
        // 缺省 effort 应为 xhigh(深推理),非旧的 high(桩推理)。
        assert_eq!(r.output_config.unwrap().effort.as_deref(), Some("xhigh"));
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
        let oc = OutputConfig { effort: Some("low".to_string()), format: None };
        let mut r = req("claude-opus-4-8", None, Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.output_config.unwrap().effort.as_deref(), Some("low"), "effort 客户端优先");
    }

    #[test]
    fn opus_with_output_config_but_no_effort_defaults_xhigh() {
        // 客户端带 output_config 但 effort 缺省(None)→ effective_effort 回退 xhigh。
        let oc = OutputConfig { effort: None, format: None };
        let mut r = req("claude-opus-4-8", None, Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.output_config.unwrap().effort.as_deref(), Some("xhigh"));
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
            effort: None,
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
            effort: None,
            format: Some(OutputFormat {
                format_type: "json_schema".to_string(),
                schema: Some(serde_json::json!({"type": "object"})),
            }),
        };
        let mut r = req("claude-opus-4-8", Some(explicit), Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().budget_tokens, 8000, "显式 thinking 应保留");
    }

    // ── 思考预算下限(2026-07-28) ───────────────────────────────────────────
    // 背景:生产实测 opencode **18/18 条**都发 budget=1024,只有 xhigh 加密体的 26%,
    // 用户体感"秒回不像在思考"。这些用例把该策略的边界逐条钉死。

    fn enabled(budget: i32) -> Thinking {
        Thinking { thinking_type: "enabled".into(), display: None, budget_tokens: budget }
    }

    fn req_mt(model: &str, thinking: Option<Thinking>, max_tokens: i32) -> MessagesRequest {
        let mut r = req(model, thinking, None);
        r.max_tokens = max_tokens;
        r
    }

    #[test]
    fn low_enabled_budget_is_raised_to_floor() {
        // opencode 的真实形状:enabled + 1024,max_tokens 充裕。
        let mut r = req_mt("claude-opus-5", Some(enabled(1024)), 32000);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.unwrap();
        assert_eq!(t.budget_tokens, DEFAULT_MIN_THINKING_BUDGET, "过低预算应抬到下限");
        assert_eq!(t.thinking_type, "enabled", "只抬预算,不改模式");
    }

    #[test]
    fn budget_above_floor_is_left_alone() {
        // 反向:Claude Code 的 25600 已高于下限,一个字节都不该动 —— 抬"下限"不是设"目标"。
        let mut r = req_mt("claude-opus-5", Some(enabled(25600)), 64000);
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().budget_tokens, 25600, "高于下限的预算必须原样保留");
    }

    #[test]
    fn small_max_tokens_request_is_not_raised() {
        // Claude Code 的会话标题生成:max_tokens=64。抬预算会把答案挤没,必须原样放行。
        let mut r = req_mt("claude-opus-5", Some(enabled(1024)), 64);
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().budget_tokens, 1024, "小 max_tokens 的杂务请求不许抬");
    }

    #[test]
    fn raise_is_clamped_by_answer_headroom() {
        // max_tokens=4096 → 抬到 4096-1024=3072 而不是 8192,给答案留出空间。
        let mut r = req_mt("claude-opus-5", Some(enabled(1024)), 4096);
        override_thinking_from_model_name(&mut r);
        let got = r.thinking.unwrap().budget_tokens;
        assert_eq!(got, 4096 - ANSWER_HEADROOM_TOKENS, "抬升须被 max_tokens 余量夹住");
        assert!(got < DEFAULT_MIN_THINKING_BUDGET, "夹住后必然低于下限,否则夹了个寂寞");
    }

    #[test]
    fn disabled_and_adaptive_are_untouched() {
        // disabled:生产 48 条全是 Claude Code 内部杂务(标题生成 / haiku 技能路由),
        // 给它们开思考是纯烧钱 + 拖慢 UI。
        let dis = Thinking { thinking_type: "disabled".into(), display: None, budget_tokens: 0 };
        let mut r = req_mt("claude-opus-5", Some(dis), 32000);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.unwrap();
        assert_eq!(t.thinking_type, "disabled", "disabled 必须原样,绝不强行开思考");
        assert_eq!(t.budget_tokens, 0);

        // adaptive:深度由 effort 决定,budget 字段不参与,抬了没意义反而误导。
        let ad = Thinking { thinking_type: "adaptive".into(), display: None, budget_tokens: 512 };
        let mut r2 = req_mt("claude-opus-5", Some(ad), 32000);
        override_thinking_from_model_name(&mut r2);
        assert_eq!(r2.thinking.unwrap().budget_tokens, 512, "adaptive 的 budget 不该被动");
    }

    #[test]
    fn floor_applies_to_non_opus_too() {
        // opencode 也可能点 sonnet;下限与模型无关。
        let mut r = req_mt("claude-sonnet-5", Some(enabled(1024)), 32000);
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.unwrap().budget_tokens, DEFAULT_MIN_THINKING_BUDGET);
    }

    #[test]
    fn max_effort_alias_maps_to_xhigh_without_warning() {
        use crate::anthropic_types::normalize_effort;
        // 生产 30 分钟 103 次 `max` 被当非法值回退并刷告警。它是同义词,不是脏值。
        let (eff, fell_back) = normalize_effort(Some("max"));
        assert_eq!(eff, "xhigh");
        assert!(!fell_back, "max 是同义翻译,不该报'非法回退'告警");
        // 大小写不敏感。
        assert_eq!(normalize_effort(Some("MAX")), ("xhigh", false));
        // 反向:真正的脏值仍要回退且报警,别把闸门放开了。
        let (eff2, fb2) = normalize_effort(Some("ludicrous"));
        assert_eq!(eff2, DEFAULT_EFFORT);
        assert!(fb2, "未知档位必须仍走回退+告警");
    }
}
