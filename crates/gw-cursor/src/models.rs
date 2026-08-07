//! 模型目录与「对外模型名 → Cursor 模型名」映射。
//!
//! ⚠️ **2026-08-07 整体重写**:旧版这张表用的是退役端点 `AvailableModels` 的点分名
//! (`claude-4.5-sonnet` / `claude-4.6-opus-max`)。`agent.v1.AgentService/Run`
//! 用的是**另一套名字**(`default` / `grok-4.5` / `claude-opus-5` …,见
//! `PROTOCOL-agent-run.md` §4)。旧表在新端点下会把每个请求的模型名都写成一个
//! 上游根本不存在的字符串。
//!
//! 另有一处细节容易翻车:UI 上的 **"Auto" 线上名叫 `default`,不是 `auto`** ——
//! 发 `auto` 会拿到 `ERROR_BAD_MODEL_NAME`。

use gw_core::model::ModelInfo;

use crate::run::Model;

/// 兜底模型。
///
/// 取 `default`(UI 的 "Auto",路由到 Composer)而不是某个 claude —— §4 实测:
/// 第三方前沿模型(claude / gpt)在 pro 号上会因计费额度耗尽被拒并自动降级,
/// 而 Cursor 自家模型不受此限。兜底就该挑最不容易被拒的那个。
pub const DEFAULT_MODEL: &str = "default";

/// Run 端点的完整模型目录(本号实测,PROTOCOL §4)。
///
/// 参数值的来源分两类:
/// - §4 明确给出的(`grok-4.5` 的 effort=high/fast=false、`composer-2.5` 的 fast=true、
///   `gpt-5.6-sol` 的 context=272k/reasoning=medium)—— 照抄。
/// - §4 只给了参数**名**没给值的(claude 系的 effort/fast、`gpt-5.6-terra` 的三个)——
///   按同族已知值补全,标注见下。这些是**推断值**,若上游报参数非法,先从这里查。
pub fn catalog() -> Vec<Model> {
    vec![
        Model::new("default"),
        Model::with_params("grok-4.5", &[("effort", "high"), ("fast", "false")]),
        Model::with_params("composer-2.5", &[("fast", "true")]),
        Model::with_params(
            "claude-opus-5",
            // effort/fast 的值是推断(§4 只列了参数名)
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "claude-sonnet-5",
            &[("thinking", "true"), ("context", "300k"), ("effort", "high")],
        ),
        Model::with_params(
            "claude-fable-5",
            &[("thinking", "true"), ("context", "300k"), ("effort", "high")],
        ),
        Model::with_params("gpt-5.6-sol", &[("context", "272k"), ("reasoning", "medium")]),
        Model::with_params(
            "gpt-5.6-terra",
            // 三个参数值都是推断(§4 只写了 "context, reasoning, fast")
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
    ]
}

/// 按 Cursor 侧模型名取目录条目(含参数);不认识则给一个无参条目。
pub fn model_by_name(cursor_name: &str) -> Model {
    catalog()
        .into_iter()
        .find(|m| m.name == cursor_name)
        .unwrap_or_else(|| Model::new(cursor_name))
}

/// 把对外模型名(Anthropic 连字符名,或已经是 Cursor 名)映射为 Cursor Run 侧模型名。
pub fn to_cursor_model(name: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        return DEFAULT_MODEL.to_string();
    }
    // 已经是 Run 目录里的名字 → 原样。
    if catalog().iter().any(|m| m.name == n) {
        return n.to_string();
    }

    let lower = n.to_ascii_lowercase();
    // 上游客户端(Claude Code / Anthropic SDK)发的是 claude-opus-5 / claude-sonnet-4-6 这类。
    // 按**族**归一,而不是逐个别名硬编码 —— 上游随时会出新的小版本号。
    let mapped = if lower.contains("opus") {
        "claude-opus-5"
    } else if lower.contains("fable") || lower.contains("mythos") {
        "claude-fable-5"
    } else if lower.contains("sonnet") {
        "claude-sonnet-5"
    } else if lower.contains("haiku") {
        // Run 目录里没有 haiku 档。降到 composer(Cursor 自家的快模型),
        // 而不是升到 sonnet —— 客户点 haiku 是要便宜快,不是要更强。
        "composer-2.5"
    } else if lower.starts_with("gpt-") || lower.starts_with("o1") || lower.starts_with("o3") {
        "gpt-5.6-sol"
    } else if lower.contains("grok") {
        "grok-4.5"
    } else if lower.contains("gemini") {
        // 目录里没有 gemini,交给服务端路由。
        DEFAULT_MODEL
    } else {
        DEFAULT_MODEL
    };
    mapped.to_string()
}

/// 把 Anthropic 的 `thinking` 请求映射进模型参数。
///
/// Anthropic 侧:`{"thinking":{"type":"enabled"|"disabled","budget_tokens":N}}`。
/// Cursor 侧只有两个相关旋钮:`thinking`(true/false)与 `effort`(low/medium/high)。
///
/// **只改目录里本来就有的键。** 目录是抓包实物,给一个模型塞它没声明过的参数是在猜,
/// 而猜错会让整个请求被 `invalid_argument` 拒掉 —— 那是比"推理档不对"糟得多的失败。
/// 所以 `composer-2.5` / `gpt-5.6-*`(没有 `thinking` 键)不受影响。
///
/// `budget_tokens` → `effort` 的分档是**我方定的映射**,不是上游文档:
/// Anthropic 的最小思考预算是 1024,常见值 2k–4k(浅)/ 8k–16k(中)/ 32k+(深)。
pub fn apply_thinking_pref(model: &mut Model, thinking: Option<&serde_json::Value>) {
    let kind = thinking
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str());

    // ⚠️ **客户端没提 thinking 时保持目录默认(claude 系是 true),不要改成 false。**
    //
    // 绝大多数流量(不开 thinking 的 Claude Code / opencode)走的是这一支。而
    // 2026-08-07 的 A/B 实测显示:这个参数**无论 true 还是 false,上游都不发 `1.4` 帧**
    // (见 PROTOCOL 关于 thinking 杠杆的记录)—— 也就是说我方不知道它到底在控制什么。
    // 对一个作用未知的旋钮,把主流量的取值从抓包实物的 `true` 改成 `false`,
    // 是拿全部请求去赌一个没验证过的假设。只在客户**明确说不要**时才发 false。
    match kind {
        Some("disabled") => {
            for (k, v) in model.params.iter_mut() {
                if k == "thinking" {
                    *v = "false".to_string();
                }
            }
            return;
        }
        Some("enabled") => {
            for (k, v) in model.params.iter_mut() {
                if k == "thinking" {
                    *v = "true".to_string();
                }
            }
        }
        // 没提 / 不认识的取值 → 一律不动目录默认。
        _ => return,
    }
    // 预算 → effort 档位。没给预算就保留目录默认(claude 系是 high)。
    let Some(budget) = thinking
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64())
    else {
        return;
    };
    let tier = match budget {
        0..=4_095 => "low",
        4_096..=16_383 => "medium",
        _ => "high",
    };
    for (k, v) in model.params.iter_mut() {
        if k == "effort" {
            *v = tier.to_string();
        }
    }
}

/// `/v1/models` 目录。
pub fn list() -> Vec<ModelInfo> {
    catalog()
        .into_iter()
        .map(|m| {
            let ctx = m
                .params
                .iter()
                .find(|(k, _)| k == "context")
                .and_then(|(_, v)| parse_context(v))
                .unwrap_or(200_000);
            let mut info = ModelInfo::new(&m.name);
            info.display_name = Some(format!("{} (cursor)", m.name));
            info.context_length = Some(ctx);
            info.supports_tools = true;
            info.supports_vision = true;
            info
        })
        .collect()
}

/// `"300k"` → `300_000`。
fn parse_context(v: &str) -> Option<u32> {
    let t = v.trim();
    match t.strip_suffix(['k', 'K']) {
        Some(num) => num.trim().parse::<u32>().ok().map(|n| n * 1000),
        None => t.parse::<u32>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_protocol_section_4() {
        let names: Vec<String> = catalog().into_iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            vec![
                "default",
                "grok-4.5",
                "composer-2.5",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
            ]
        );
    }

    #[test]
    fn auto_is_not_a_valid_model_name() {
        // UI 的 "Auto" 线上名是 `default`;发 `auto` 会被判 ERROR_BAD_MODEL_NAME。
        assert!(!catalog().iter().any(|m| m.name == "auto"));
        assert_eq!(to_cursor_model("auto"), "default");
    }

    #[test]
    fn retired_dotted_names_no_longer_leak_through() {
        // 旧表会把这些原样透传/改写成 claude-4.5-sonnet,而 Run 端点不认。
        assert_eq!(to_cursor_model("claude-4.5-sonnet"), "claude-sonnet-5");
        assert_eq!(to_cursor_model("claude-4.6-opus-max"), "claude-opus-5");
        assert!(!to_cursor_model("claude-4.5-haiku").starts_with("claude-4"));
    }

    #[test]
    fn maps_anthropic_names_by_family() {
        assert_eq!(to_cursor_model("claude-opus-5"), "claude-opus-5");
        assert_eq!(to_cursor_model("claude-opus-4-8"), "claude-opus-5");
        assert_eq!(to_cursor_model("claude-sonnet-4-5"), "claude-sonnet-5");
        assert_eq!(to_cursor_model("claude-fable-5"), "claude-fable-5");
        assert_eq!(to_cursor_model("claude-haiku-4-5"), "composer-2.5");
        assert_eq!(to_cursor_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(to_cursor_model("gpt-4o"), "gpt-5.6-sol");
        assert_eq!(to_cursor_model("grok-4.5"), "grok-4.5");
    }

    #[test]
    fn unknown_and_empty_fall_back_to_default() {
        assert_eq!(to_cursor_model("totally-unknown"), DEFAULT_MODEL);
        assert_eq!(to_cursor_model(""), DEFAULT_MODEL);
        assert_eq!(to_cursor_model("   "), DEFAULT_MODEL);
        assert_eq!(DEFAULT_MODEL, "default");
    }

    /// claude 系必须带 `thinking` 参数 —— 它是我方能拉的唯一「要推理」的杠杆。
    #[test]
    fn claude_models_declare_thinking() {
        for name in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            let m = model_by_name(name);
            let th = m.params.iter().find(|(k, _)| k == "thinking");
            assert!(th.is_some(), "{name} 没声明 thinking 参数");
            assert_eq!(th.unwrap().1, "true", "{name} 的 thinking 值");
        }
    }

    #[test]
    fn thinking_pref_flips_the_catalog_value() {
        let on = serde_json::json!({"type":"enabled"});
        let off = serde_json::json!({"type":"disabled"});
        let get = |m: &Model, k: &str| {
            m.params.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
        };

        let mut m = model_by_name("claude-sonnet-5");
        apply_thinking_pref(&mut m, Some(&off));
        assert_eq!(get(&m, "thinking").as_deref(), Some("false"), "客户不要推理就别发 true");

        // ⭐ 没提 thinking 的请求(绝大多数流量)**必须保持目录默认**。
        // 这个参数的实际作用未验证(A/B 实测 true/false 都不产生 `1.4` 帧),
        // 拿主流量去改一个作用未知的旋钮是赌博。
        let mut m = model_by_name("claude-sonnet-5");
        apply_thinking_pref(&mut m, None);
        assert_eq!(get(&m, "thinking").as_deref(), Some("true"), "没提就别动抓包实物的值");
        let mut m = model_by_name("claude-sonnet-5");
        apply_thinking_pref(&mut m, Some(&serde_json::json!({"type":"weird"})));
        assert_eq!(get(&m, "thinking").as_deref(), Some("true"), "不认识的取值也别动");

        let mut m = model_by_name("claude-sonnet-5");
        apply_thinking_pref(&mut m, Some(&on));
        assert_eq!(get(&m, "thinking").as_deref(), Some("true"));
        assert_eq!(get(&m, "effort").as_deref(), Some("high"), "没给预算保留目录默认");
    }

    #[test]
    fn budget_tokens_maps_to_effort_tiers() {
        let get = |m: &Model| m.params.iter().find(|(k, _)| k == "effort").map(|(_, v)| v.clone());
        for (budget, want) in [(1024u64, "low"), (2000, "low"), (8000, "medium"), (32000, "high")] {
            let mut m = model_by_name("claude-opus-5");
            apply_thinking_pref(&mut m, Some(&serde_json::json!({
                "type":"enabled","budget_tokens":budget})));
            assert_eq!(get(&m).as_deref(), Some(want), "budget={budget}");
        }
    }

    #[test]
    fn thinking_pref_never_invents_params_the_catalog_lacks() {
        // composer / gpt 目录里没有 thinking/effort —— 塞进去会被上游 invalid_argument 拒。
        for name in ["composer-2.5", "gpt-5.6-sol", "default"] {
            let before = model_by_name(name).params.len();
            let mut m = model_by_name(name);
            apply_thinking_pref(&mut m, Some(&serde_json::json!({
                "type":"enabled","budget_tokens":32000})));
            assert_eq!(m.params.len(), before, "{name} 的参数数量不该变");
            assert!(!m.params.iter().any(|(k, _)| k == "thinking"), "{name} 不该凭空多出 thinking");
        }
    }

    #[test]
    fn model_by_name_carries_params() {
        let m = model_by_name("grok-4.5");
        assert_eq!(
            m.params,
            vec![
                ("effort".to_string(), "high".to_string()),
                ("fast".to_string(), "false".to_string())
            ]
        );
        // 不认识的名字给无参条目,不 panic
        assert!(model_by_name("nope").params.is_empty());
    }

    #[test]
    fn list_reports_real_context_windows() {
        let l = list();
        let by = |id: &str| l.iter().find(|m| m.id == id).unwrap().clone();
        assert_eq!(by("claude-opus-5").context_length, Some(300_000));
        assert_eq!(by("gpt-5.6-sol").context_length, Some(272_000));
        // 没有 context 参数的走 200k 兜底
        assert_eq!(by("default").context_length, Some(200_000));
        assert_eq!(l.len(), catalog().len());
    }

    #[test]
    fn parse_context_handles_k_suffix() {
        assert_eq!(parse_context("300k"), Some(300_000));
        assert_eq!(parse_context("272K"), Some(272_000));
        assert_eq!(parse_context("128000"), Some(128_000));
        assert_eq!(parse_context("junk"), None);
    }
}
