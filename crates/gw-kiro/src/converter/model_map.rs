//! 模型名映射与上下文窗口 —— **单一权威表**。
//!
//! `KIRO_MODELS` 是唯一事实源:对外裸名 ↔ 上游 Kiro 模型 ↔ 身份短名 ↔ 上下文窗口 ↔
//! thinking 能力 ↔ 日期快照别名。`/v1/models` 目录、chat 路由(`map_model`)、身份规范化
//! (`normalize::requested_model_identity`)、窗口推导(`get_context_window_size`)**全部从此表派生**,
//! 杜绝历史上"四处列表不一致 → 公告了 chat 却 None / 真实代号泄漏"的漂移。
//!
//! 四种对外形态由 [`resolve_base`] 统一归一到基础行:plain / `-thinking` / 日期 /
//! 日期`-thinking`。

/// 一个基础 Kiro 模型(对外裸名维度)。日期/thinking 变体由它派生,不单列。
pub struct KiroModel {
    /// 对外裸名(客户端看到、NewAPI 拉取),如 `claude-opus-4-8`。
    pub advertised_id: &'static str,
    /// 展示名,如 `Claude Opus 4.8`。
    pub display_name: &'static str,
    /// 上游真实 Kiro 模型 id(发包用),如 `claude-opus-4.8`。
    pub kiro_model: &'static str,
    /// 身份行短名(`You are powered by the model named {short}`),防 `claude-quince` 泄漏。
    pub identity_short: &'static str,
    /// 上下文窗口 token 数。
    pub context_window: i32,
    /// 该裸名是否支持 thinking(thinking 变体恒 true,此处是 plain 的天然能力)。
    pub supports_thinking: bool,
    /// 官方日期快照别名(有才填),如 `claude-opus-4-5-20251101`;用于公告目录。
    pub dated_alias: Option<&'static str>,
}

/// **权威表**。新增/下线模型只改这里——派生点自动跟随。
pub const KIRO_MODELS: &[KiroModel] = &[
    KiroModel {
        // 2026-07-25 上游新增。与 sonnet-5 同规律:modelId 是主版本裸名
        // `claude-opus-5`(无 x.y 点号,与 4.x 各行不同)。
        advertised_id: "claude-opus-5",
        display_name: "Claude Opus 5",
        kiro_model: "claude-opus-5",
        identity_short: "Opus 5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        kiro_model: "claude-opus-4.8",
        identity_short: "Opus 4.8",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        kiro_model: "claude-opus-4.7",
        identity_short: "Opus 4.7",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        kiro_model: "claude-opus-4.6",
        identity_short: "Opus 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        kiro_model: "claude-opus-4.5",
        identity_short: "Opus 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-opus-4-5-20251101"),
    },
    KiroModel {
        // 2026-07-02 上游 ListAvailableModels 实测新增(标注 experimental preview),
        // modelId 本身就是 `claude-sonnet-5`(无 x.y 点号,与其余行不同)。
        advertised_id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        kiro_model: "claude-sonnet-5",
        identity_short: "Sonnet 5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        kiro_model: "claude-sonnet-4.6",
        identity_short: "Sonnet 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        kiro_model: "claude-sonnet-4.5",
        identity_short: "Sonnet 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-sonnet-4-5-20250929"),
    },
    KiroModel {
        advertised_id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        kiro_model: "claude-haiku-4.5",
        identity_short: "Haiku 4.5",
        context_window: 200_000,
        supports_thinking: false,
        dated_alias: Some("claude-haiku-4-5-20251001"),
    },
];

/// 去掉末尾的 `-YYYYMMDD` 日期段(末段恰好 8 位 ASCII 数字才剥;无 regex)。
/// 例:`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`;`claude-opus-4-8` 原样
/// (末段 "8" 非 8 位数字,不误剥)。
fn strip_date_suffix(id: &str) -> &str {
    match id.rfind('-') {
        Some(pos) => {
            let tail = &id[pos + 1..];
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                &id[..pos]
            } else {
                id
            }
        }
        None => id,
    }
}

/// 把对外请求名(plain / `-thinking` / 日期 / 日期`-thinking`)归一到权威表的基础行。
/// 顺序:先剥 `-thinking`,再剥日期,最后对 `advertised_id` 精确匹配。
pub fn resolve_base(requested: &str) -> Option<&'static KiroModel> {
    let lower = requested.to_lowercase();
    let no_thinking = lower.strip_suffix("-thinking").unwrap_or(&lower);
    let base = strip_date_suffix(no_thinking);
    KIRO_MODELS.iter().find(|m| m.advertised_id == base)
}

/// 模型映射:对外名 → 上游 Kiro 模型 id。
///
/// 先走权威表([`resolve_base`]),未命中**保留子串兜底**(老客户端/未列名,如旧 opus-4.5
/// 异名仍可路由),都不中返回 None。
pub fn map_model(model: &str) -> Option<String> {
    if let Some(m) = resolve_base(model) {
        return Some(m.kiro_model.to_string());
    }
    map_model_substring(&model.to_lowercase())
}

/// 子串兜底映射(权威表未命中时;严格对照版本号)。
fn map_model_substring(model_lower: &str) -> Option<String> {
    if model_lower.contains("sonnet") {
        // 2026-05-29: Kiro 上游尚未支持 sonnet-4.8（INVALID_MODEL_ID），未来上线再加 4-8 分支
        if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-sonnet-4.5".to_string())
        } else if model_lower.contains("sonnet-5") || model_lower.contains("sonnet5") {
            // 2026-07-02: 上游新增 claude-sonnet-5（无 x.y 点号,权威表已列;此兜底覆盖异名写法,
            // 如带前缀/后缀的未列名 `openrouter/claude-sonnet-5-preview`)。
            // ⚠️必须锚定 `sonnet-5` 邻接串,不能用裸 `contains('5')`——否则历史名
            // `claude-3-5-sonnet`(Claude 3.5 Sonnet,sonnet 在尾部)会被误吞成 sonnet-5。
            Some("claude-sonnet-5".to_string())
        } else {
            None
        }
    } else if model_lower.contains("opus") {
        if model_lower.contains("opus-5") || model_lower.contains("opus5") {
            // 2026-07-25: 上游新增 claude-opus-5(无 x.y 点号,权威表已列;此兜底覆盖
            // 异名写法,如 `openrouter/claude-opus-5-preview`)。
            // ⚠️必须锚定 `opus-5` 邻接串,不能用裸 contains("5")——否则未来的
            // `claude-opus-4-5`/`4.5` 会被误吞(它们在下面各自分支处理)。
            // 本分支置于最前:opus-5 的写法不含 4-x,与下方分支互斥。
            Some("claude-opus-5".to_string())
        } else if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-opus-4.6".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else {
            None
        }
    } else if model_lower.contains("haiku") {
        // haiku 历史一直兜底到 4.5（Kiro 暂无更新版本，含 4.8）
        Some("claude-haiku-4.5".to_string())
    } else {
        None
    }
}

/// 根据模型名返回上下文窗口大小。权威表优先,兜底子串(1M 仅 sonnet-5/sonnet-4.6/opus-4.6/4.7/4.8)。
pub fn get_context_window_size(model: &str) -> i32 {
    if let Some(m) = resolve_base(model) {
        return m.context_window;
    }
    match map_model_substring(&model.to_lowercase()) {
        Some(mapped)
            if mapped == "claude-opus-5"
                || mapped == "claude-sonnet-5"
                || mapped == "claude-sonnet-4.6"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}

/// 对外公告的一个模型条目(由权威表展开:plain / -thinking / 日期 / 日期-thinking)。
pub struct AdvertisedModel {
    pub id: String,
    pub display_name: String,
    pub supports_thinking: bool,
}

/// 从权威表生成 `/v1/models` 完整对外目录。每个基础模型展开:
/// plain、`-thinking`;若有 dated_alias 再加 日期名、`日期名-thinking`。
pub fn advertised_models() -> Vec<AdvertisedModel> {
    let mut out = Vec::with_capacity(KIRO_MODELS.len() * 4);
    for m in KIRO_MODELS {
        // plain
        out.push(AdvertisedModel {
            id: m.advertised_id.to_string(),
            display_name: m.display_name.to_string(),
            supports_thinking: m.supports_thinking,
        });
        // -thinking(thinking 变体恒声明 supports_thinking=true)
        out.push(AdvertisedModel {
            id: format!("{}-thinking", m.advertised_id),
            display_name: format!("{} (thinking)", m.display_name),
            supports_thinking: true,
        });
        // 日期别名 + 日期-thinking
        if let Some(dated) = m.dated_alias {
            let date_label = dated_label(dated);
            out.push(AdvertisedModel {
                id: dated.to_string(),
                display_name: format!("{}{date_label}", m.display_name),
                supports_thinking: m.supports_thinking,
            });
            out.push(AdvertisedModel {
                id: format!("{dated}-thinking"),
                display_name: format!("{} (thinking){date_label}", m.display_name),
                supports_thinking: true,
            });
        }
    }
    out
}

/// 从日期别名末段 `-YYYYMMDD` 生成展示后缀 ` (YYYY-MM-DD)`;无法解析则空串。
fn dated_label(dated: &str) -> String {
    match dated.rfind('-') {
        Some(pos) => {
            let tail = &dated[pos + 1..];
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                format!(" ({}-{}-{})", &tail[0..4], &tail[4..6], &tail[6..8])
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不变量:每个**公告**对外名都必须能 `map_model`(否则公告了 chat 拿不到)且有正窗口。
    #[test]
    fn every_advertised_model_maps() {
        for am in advertised_models() {
            assert!(
                map_model(&am.id).is_some(),
                "公告模型 {} 未能 map_model → /v1/models 列了但 chat 会 None",
                am.id
            );
            assert!(get_context_window_size(&am.id) >= 200_000, "{} 窗口异常", am.id);
        }
    }

    #[test]
    fn advertised_has_no_duplicate_ids() {
        let mut ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "advertised_models 有重复 id");
    }

    #[test]
    fn advertised_includes_thinking_and_dated() {
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-opus-4-8".to_string()));
        assert!(ids.contains(&"claude-opus-4-8-thinking".to_string()));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929".to_string()));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929-thinking".to_string()));
        assert!(ids.contains(&"claude-haiku-4-5-20251001".to_string()));
        // 展开数从表派生(有 dated_alias=4 形态,否则 2),而非硬钉常量——
        // 加/减模型或日期别名时测试仍校验"展开逻辑"而非快照(审查 Minimalist#3)。
        let expected: usize = KIRO_MODELS
            .iter()
            .map(|m| if m.dated_alias.is_some() { 4 } else { 2 })
            .sum();
        assert_eq!(ids.len(), expected);
    }

    #[test]
    fn map_model_resolves_thinking_and_dated() {
        assert_eq!(map_model("claude-opus-4-8-thinking").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(
            map_model("claude-opus-4-5-20251101-thinking").as_deref(),
            Some("claude-opus-4.5")
        );
        assert_eq!(
            map_model("claude-sonnet-4-5-20250929").as_deref(),
            Some("claude-sonnet-4.5")
        );
    }

    #[test]
    fn map_model_substring_fallback_still_works() {
        // 未列于权威表的异名(老客户端)仍走子串兜底。
        assert_eq!(map_model("anthropic/claude-3.5-sonnet-4.6").as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(map_model("claude-opus-4.7-beta").as_deref(), Some("claude-opus-4.7"));
        assert!(map_model("gpt-4o").is_none());
    }

    #[test]
    fn opus_5_resolves_via_table_and_fallback() {
        // 权威表精确匹配(plain/-thinking)。
        assert_eq!(map_model("claude-opus-5").as_deref(), Some("claude-opus-5"));
        assert_eq!(map_model("claude-opus-5-thinking").as_deref(), Some("claude-opus-5"));
        assert_eq!(resolve_base("claude-opus-5").map(|m| m.identity_short), Some("Opus 5"));
        assert_eq!(get_context_window_size("claude-opus-5"), 1_000_000);
        // 子串兜底(未列全名的异名)也给 1M 窗口。
        assert_eq!(map_model("openrouter/claude-opus-5-preview").as_deref(), Some("claude-opus-5"));
        assert_eq!(get_context_window_size("openrouter/claude-opus-5-preview"), 1_000_000);
        // 关键边界:opus-5 分支置于最前,但绝不能吞掉 4.x 各版本。
        assert_eq!(map_model("claude-opus-4-8").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(map_model("claude-opus-4-7").as_deref(), Some("claude-opus-4.7"));
        assert_eq!(map_model("claude-opus-4-6").as_deref(), Some("claude-opus-4.6"));
        assert_eq!(map_model("claude-opus-4-5").as_deref(), Some("claude-opus-4.5"));
        assert_eq!(map_model("claude-opus-4.5").as_deref(), Some("claude-opus-4.5"));
        // 日期变体仍归到 4-5 基础行,不被 opus-5 误吞。
        assert_eq!(map_model("claude-opus-4-5-20251101").as_deref(), Some("claude-opus-4.5"));
    }

    #[test]
    fn sonnet_5_resolves_via_table_and_fallback() {
        // 权威表精确匹配(plain/-thinking)。
        assert_eq!(map_model("claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("claude-sonnet-5-thinking").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(resolve_base("claude-sonnet-5").map(|m| m.identity_short), Some("Sonnet 5"));
        // 子串兜底(未列全名的异名),且不误吞 sonnet-4.5/4.6。
        assert_eq!(map_model("openrouter/claude-sonnet-5-preview").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("claude-sonnet-4-5").as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(map_model("claude-sonnet-4-6").as_deref(), Some("claude-sonnet-4.6"));
        // 历史名 claude-3-5-sonnet(Claude 3.5 Sonnet)含 sonnet+散落的 5,绝不能被误吞成 sonnet-5。
        // 它不含 "4-5"/"4.5"/"sonnet-5",落到子串兜底的 None(该老模型 Kiro 本就不支持,冤案排除)。
        assert_eq!(map_model("claude-3-5-sonnet").as_deref(), None);
        assert_eq!(map_model("claude-3-5-sonnet-20241022").as_deref(), None);
    }

    #[test]
    fn context_window_from_table_and_fallback() {
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
        assert_eq!(get_context_window_size("claude-opus-4-8-thinking"), 1_000_000);
        assert_eq!(get_context_window_size("claude-sonnet-4-5-20250929"), 200_000);
        assert_eq!(get_context_window_size("claude-haiku-4-5"), 200_000);
        assert_eq!(get_context_window_size("claude-sonnet-5"), 1_000_000);
        assert_eq!(get_context_window_size("openrouter/claude-sonnet-5-preview"), 1_000_000);
    }

    #[test]
    fn strip_date_suffix_boundaries() {
        assert_eq!(strip_date_suffix("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        // 裸名末段非 8 位数字,不误剥。
        assert_eq!(strip_date_suffix("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(strip_date_suffix("claude-opus-4-5"), "claude-opus-4-5");
        // 7 位或 9 位都不剥。
        assert_eq!(strip_date_suffix("claude-x-2025092"), "claude-x-2025092");
    }

    #[test]
    fn resolve_base_handles_all_four_forms() {
        let want = "claude-sonnet-4-5";
        for form in [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-thinking",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929-thinking",
        ] {
            assert_eq!(resolve_base(form).map(|m| m.advertised_id), Some(want), "{form}");
        }
    }
}
