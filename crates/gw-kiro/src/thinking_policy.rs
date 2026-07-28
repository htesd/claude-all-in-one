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

/// `enabled` 模式的 budget → effort 档位映射。
///
/// 1.0.212 的线缆里**没有 budget 概念了** —— 思考强度只有档位(`additionalModelRequestFields`)。
/// 但客户端(Claude Code / opencode)仍按 Anthropic 语义发 `budget_tokens`,得翻译过去。
/// 分界取客户端实际发的值:opencode 1024、Claude Code 2048 / 25600 / 52428。
///
/// 产出的是**全集**档位,可能该模型并不支持(如 4.6 系没有 `xhigh`);
/// 按模型的夹取统一在 [`effective_effort`] 出口做一次,这里不管。
fn budget_to_effort(budget: i32) -> &'static str {
    match budget {
        b if b < 2048 => "low",
        b if b < 8192 => "medium",
        b if b < 24576 => "high",
        _ => "xhigh",
    }
}

/// 本次请求对档位的诉求。
enum EffortWish {
    /// 完全不发 `additionalModelRequestFields`。
    Omit,
    /// 明确要这一档(合法值,可能该模型不支持,由夹取决定)。
    Level(&'static str),
    /// 客户端给了**脏值**,没有可用诉求 → 交给模型自己的 schema `default`。
    ///
    /// 关键:**不能**在这里先替它填一个全局默认(此前填 `xhigh`)。填了之后夹取
    /// 只看见一个合法值,支持 `xhigh` 的模型(opus-5 / 4.8)就会照发 `xhigh`,
    /// 而真客户端 `A7` 对未知档位是回落到**该模型的** `default`(= `high`)。
    /// 把"脏值"这个状态一路带到模型边界再决策,才和客户端一致。
    ModelDefault,
}

/// 请求**期望**的档位(未按模型夹取)。
fn desired_effort(req: &MessagesRequest) -> EffortWish {
    let Some(t) = req.thinking.as_ref() else {
        return EffortWish::Omit;
    };
    match t.thinking_type.as_str() {
        // 客户端明确不要思考 → 不发字段(与 1.0.212 的 undefined 同形)。
        "disabled" => EffortWish::Omit,
        "adaptive" => {
            let raw = req.output_config.as_ref().and_then(|c| c.effort.as_deref());
            let (effort, fell_back) = crate::anthropic_types::normalize_effort(raw);
            if fell_back {
                tracing::warn!(
                    requested = ?raw,
                    valid = ?crate::anthropic_types::VALID_EFFORTS,
                    "非法 thinking effort，改用该模型 schema 的默认档位"
                );
                return EffortWish::ModelDefault;
            }
            EffortWish::Level(effort)
        }
        // 预算制:翻译成档位(上游已不认 budget)。
        "enabled" => EffortWish::Level(budget_to_effort(t.budget_tokens)),
        _ => EffortWish::Omit,
    }
}

/// 本次请求的**有效思考档位** —— `additionalModelRequestFields` 的唯一来源。
///
/// `None` = 不发该字段(等价于让上游按默认走)。三种情形都归到这里,因为真客户端
/// 在这三种情形下发的都是 `additionalModelRequestFields: undefined`:
/// 1. 客户端明确 `thinking.type=disabled`;
/// 2. 该模型上游没有 effort schema(4.5 系 / haiku —— `extension.js:223145` 的
///    `effortLevel && effortSchemaPath` 短路);
/// 3. 模型名压根不在权威表里(未知模型,宁可不发也不猜)。
///
/// 注意本函数须在 [`override_thinking_from_model_name`] **之后**调用:那步会给未配置的
/// Opus 请求补上 adaptive + 默认档。
pub fn effective_effort(req: &MessagesRequest) -> Option<&'static str> {
    let want = match desired_effort(req) {
        EffortWish::Omit => return None,
        EffortWish::Level(l) => Some(l),
        EffortWish::ModelDefault => None,
    };
    let got = crate::converter::clamp_effort_for_model(&req.model, want)?;
    if want != Some(got) {
        tracing::debug!(
            model = %req.model,
            wanted = ?want,
            used = got,
            "所请求的思考档位该模型不支持（或未指定），已按上游 schema 回落"
        );
    }
    Some(got)
}

/// 把有效档位包成 Kiro 的 `additionalModelRequestFields`。
///
/// **形态逐字对齐真客户端的生成函数**(`extension.js:222579` 的 `qe8`),它整个函数体就是:
/// ```js
/// switch (schemaPath) {
///   case "output_config": return { output_config: { effort } };
///   case "reasoning":     return { reasoning:     { effort } };
/// }
/// ```
/// caio 服务的模型全是 Anthropic 系,`schemaPath` 恒为 `output_config`
/// (gpt-5.6 系才走 `reasoning`,caio 不转发它们)。
///
/// ⚠️ **不要补 `thinking` / `max_tokens`。** 上游 schema 里确实声明了这两个属性,但客户端
/// **从不填** —— `qe8` 只产 `effort` 一个键。补了就是比真客户端多发字段,与做这件事的
/// 初衷(消除可规则化的形态差异)正好相反。
///
/// 2026-07-28 真机 A/B 验证 `output_config.effort` 确实生效(claude-opus-5,同一道题):
/// low 137 帧 reasoning / 33s,xhigh 1406 帧 / 169s。
pub fn additional_model_request_fields(req: &MessagesRequest) -> Option<serde_json::Value> {
    let effort = effective_effort(req)?;
    Some(serde_json::json!({ "output_config": { "effort": effort } }))
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
    fn opus_defaults_to_adaptive_top_tier_without_suffix() {
        let mut r = req("claude-opus-4-8", None, None);
        override_thinking_from_model_name(&mut r);
        let t = r.thinking.expect("opus 应默认开 thinking");
        assert_eq!(t.thinking_type, "adaptive");
        // 缺省 effort = 顶格 `max`。2026-07-28 剂量反应实测 max 比 xhigh 还多约 1.7 倍
        // 思考量,且 max 走的是真客户端也在用的合规通道(见 DEFAULT_EFFORT 文档)。
        assert_eq!(r.output_config.unwrap().effort.as_deref(), Some(DEFAULT_EFFORT));
        assert_eq!(DEFAULT_EFFORT, "max", "默认档若被改动,这里要连同上面的实测依据一起重估");
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
    fn opus_with_output_config_but_no_effort_defaults_to_top_tier() {
        // 客户端带 output_config 但 effort 缺省(None)→ 回退 caio 策略默认(顶格)。
        let oc = OutputConfig { effort: None, format: None };
        let mut r = req("claude-opus-4-8", None, Some(oc));
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.output_config.unwrap().effort.as_deref(), Some(DEFAULT_EFFORT));
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

    // ── 对齐 Kiro 1.0.212:结构化思考字段 ───────────────────────────────────
    // 2026-07-28 真机 A/B(claude-opus-5,同一道证明题):
    //   新字段 low  → 137  个 reasoningContentEvent 帧 / 33s
    //   新字段 xhigh→ 1406 个                         / 169s
    // 证明 schemaPath=output_config 对 Anthropic 系模型生效。

    #[test]
    fn adaptive_effort_lands_in_output_config() {
        let mut r = req_mt("claude-opus-5", None, 32000);
        r.output_config = Some(OutputConfig { effort: Some("high".into()), format: None });
        override_thinking_from_model_name(&mut r);
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(v, serde_json::json!({"output_config":{"effort":"high"}}));
    }

    #[test]
    fn disabled_emits_no_field_at_all() {
        // 与真实客户端在无 effortLevel 时 `additionalModelRequestFields: undefined` 同形。
        let dis = Thinking { thinking_type: "disabled".into(), display: None, budget_tokens: 0 };
        let mut r = req_mt("claude-opus-5", Some(dis), 32000);
        override_thinking_from_model_name(&mut r);
        assert!(additional_model_request_fields(&r).is_none(), "disabled 不该发该字段");
    }

    #[test]
    fn budget_is_translated_to_effort_tier() {
        // 上游 1.0.212 线缆已无 budget 概念,必须翻译成档位,否则客户端的强度意图全丢。
        // 这里测**纯映射**:走完整路径会先被预算下限抬一手(见下面那条组合用例)。
        for (budget, want) in [(1024, "low"), (2048, "medium"), (8192, "high"), (25600, "xhigh")] {
            assert_eq!(budget_to_effort(budget), want, "budget={budget}");
        }
        // 单调性:客户端调高预算绝不该反而变浅。
        let tiers: Vec<&str> = [1024, 4096, 16384, 65536].iter().map(|b| budget_to_effort(*b)).collect();
        let rank = |e: &str| crate::anthropic_types::VALID_EFFORTS.iter().position(|v| *v == e).unwrap();
        assert!(
            tiers.windows(2).all(|w| rank(w[0]) < rank(w[1])),
            "映射必须严格单调递增,实际={tiers:?}"
        );
    }

    #[test]
    fn full_path_respects_client_budget_above_floor() {
        // 高于下限的预算走完整路径:不被抬,按原值翻译。
        let mut r = req_mt("claude-opus-5", Some(enabled(25600)), 64000);
        override_thinking_from_model_name(&mut r);
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(v["output_config"]["effort"], "xhigh", "实际={v}");
    }

    #[test]
    fn opencode_shape_after_floor_lands_on_high() {
        // opencode 实际形状 budget=1024:先被下限抬到 8192,再翻译成 high。
        // 两个策略叠起来的净效果 —— 这条把它钉死,免得日后改了下限却没人发现档位跟着变。
        let mut r = req_mt("claude-opus-5", Some(enabled(1024)), 32000);
        override_thinking_from_model_name(&mut r);
        assert_eq!(r.thinking.as_ref().unwrap().budget_tokens, DEFAULT_MIN_THINKING_BUDGET);
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(v["output_config"]["effort"], "high");
    }

    #[test]
    fn client_max_effort_reaches_the_wire_undowngraded() {
        // 生产每 300 条约 33 条发 `max`。它是**上游 enum 里的最高档**,不是同义词 ——
        // 曾被当非法值映射成 xhigh,等于把顶格请求静默降一级。
        let mut r = req_mt("claude-opus-5", None, 32000);
        r.output_config = Some(OutputConfig { effort: Some("max".into()), format: None });
        override_thinking_from_model_name(&mut r);
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(v["output_config"]["effort"], "max", "max 必须原样上 wire,实际={v}");

        // 反向:真正的脏值仍要回退且报警,别把闸门放开了。
        use crate::anthropic_types::normalize_effort;
        let (eff, fb) = normalize_effort(Some("ludicrous"));
        assert_eq!(eff, DEFAULT_EFFORT);
        assert!(fb, "未知档位必须仍走回退+告警");
    }

    // ── 按模型夹取档位(2026-07-28,依据真实 ListAvailableModels schema)──────────

    #[test]
    fn model_without_xhigh_falls_back_to_its_schema_default() {
        // opus-4.6 的 enum 是 [low,medium,high,max] —— **没有 xhigh**。
        // 注意:caio 的策略默认现在是 `max`,而 4.6 恰好**有** max,走默认路径测不到夹取。
        // 所以这里**显式**请求 xhigh,才真正压到"该模型不支持此档"的分支。
        let mut r = req_mt("claude-opus-4-6", None, 32000);
        r.output_config = Some(OutputConfig { effort: Some("xhigh".into()), format: None });
        override_thinking_from_model_name(&mut r);
        assert_eq!(
            r.output_config.as_ref().unwrap().effort.as_deref(),
            Some("xhigh"),
            "策略层原样保留客户端的 xhigh,夹取只发生在 wire 出口"
        );
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(
            v["output_config"]["effort"], "high",
            "4.6 系无 xhigh，须回落到它 schema 的 default=high(对齐客户端 A7),实际={v}"
        );
        // 反向:同一个 xhigh 打到支持它的模型上必须原样保留,别夹过头。
        let mut r2 = req_mt("claude-opus-4-8", None, 32000);
        r2.output_config = Some(OutputConfig { effort: Some("xhigh".into()), format: None });
        override_thinking_from_model_name(&mut r2);
        let v2 = additional_model_request_fields(&r2).unwrap();
        assert_eq!(v2["output_config"]["effort"], "xhigh", "4.8 有 xhigh，不该被夹,实际={v2}");
        // 默认路径:4.6 也有 max,所以策略默认能原样落地,不触发回落。
        let mut r3 = req_mt("claude-opus-4-6", None, 32000);
        override_thinking_from_model_name(&mut r3);
        let v3 = additional_model_request_fields(&r3).unwrap();
        assert_eq!(v3["output_config"]["effort"], "max", "默认顶格档在 4.6 上也成立,实际={v3}");
    }

    #[test]
    fn model_without_effort_schema_emits_no_field() {
        // opus-4.5 / sonnet-4.5 / haiku-4.5 上游没有 additionalModelRequestFieldsSchema,
        // 真客户端此时 `additionalModelRequestFields: undefined`(extension.js:223145 短路)。
        for model in ["claude-opus-4-5", "claude-sonnet-4-5-thinking", "claude-haiku-4-5"] {
            let mut r = req_mt(model, Some(enabled(25600)), 64000);
            override_thinking_from_model_name(&mut r);
            assert!(
                additional_model_request_fields(&r).is_none(),
                "{model} 上游无 effort schema，一个字段都不该发"
            );
        }
    }

    #[test]
    fn unknown_model_emits_no_field() {
        // 不在权威表里的模型:档位表未知 → 宁可不发,也不猜一个可能非法的值。
        let mut r = req_mt("some-vendor/unknown-model", Some(enabled(25600)), 64000);
        override_thinking_from_model_name(&mut r);
        assert!(additional_model_request_fields(&r).is_none(), "未知模型不该猜档位");
    }

    #[test]
    fn max_survives_on_models_lacking_xhigh() {
        // 4.6 系虽无 xhigh,但**有** max。客户端顶格请求必须完整送达,不能被"没有 xhigh"
        // 连累成 high —— 这正是 max 当初被当同义词的连锁伤害。
        // sonnet 非 Opus,策略层不会替它注入 thinking,所以这里显式给 adaptive(模拟客户端自带)。
        let adaptive = Thinking { thinking_type: "adaptive".into(), display: None, budget_tokens: 0 };
        let mut r = req_mt("claude-sonnet-4-6", Some(adaptive), 32000);
        r.output_config = Some(OutputConfig { effort: Some("max".into()), format: None });
        override_thinking_from_model_name(&mut r);
        let v = additional_model_request_fields(&r).unwrap();
        assert_eq!(v["output_config"]["effort"], "max", "实际={v}");
    }
}
