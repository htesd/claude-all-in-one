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
    /// 该模型**实际可用**的 thinking effort 档位,升序。空 = 上游没给
    /// `additionalModelRequestFieldsSchema` → **一个字节都不该发**该字段。
    ///
    /// 逐模型不同,见 [`EFFORTS_WITH_XHIGH`] / [`EFFORTS_NO_XHIGH`] 的说明。
    pub effort_levels: &'static [&'static str],
    /// schema 里标注的 `default`。请求档位不可用时回落到它(对齐真客户端 `A7`)。
    pub default_effort: Option<&'static str>,
}

/// 5 档全集。opus-5 / sonnet-5 / opus-4.8 / opus-4.7 实测为此形态。
const EFFORTS_WITH_XHIGH: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// **4 档,没有 `xhigh`**。opus-4.6 / sonnet-4.6 实测为此形态 —— 4.6 系直接从 `high`
/// 跳到 `max`。此前 caio 对所有 Opus 硬编码 `xhigh`,对这两个模型就是发了一个
/// 上游 enum 里根本不存在的值。
const EFFORTS_NO_XHIGH: &[&str] = &["low", "medium", "high", "max"];

/// 上游没有 `additionalModelRequestFieldsSchema` 的模型(4.5 系、haiku)。
/// 真客户端此时 `additionalModelRequestFields: undefined`(`extension.js:223145`),
/// 我们也必须一个字段都不发。
const EFFORTS_NONE: &[&str] = &[];

/// **权威表**。新增/下线模型只改这里——派生点自动跟随。
///
/// `effort_levels` / `default_effort` 抄自 2026-07-28 用真号打
/// `ListAvailableModels` 取回的 `additionalModelRequestFieldsSchema`
/// (存档 `/root/caio-backup/kiro-models-1.0.212.json`)。**不要凭印象改** ——
/// 发一个该模型 enum 里没有的档位既可能 400,也是可被规则化的形态差异。
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
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
    },
    KiroModel {
        advertised_id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        kiro_model: "claude-opus-4.8",
        identity_short: "Opus 4.8",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
    },
    KiroModel {
        advertised_id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        kiro_model: "claude-opus-4.7",
        identity_short: "Opus 4.7",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        // 全表唯一 default 不是 high 的模型(上游 schema 逐字如此)。
        default_effort: Some("xhigh"),
    },
    KiroModel {
        advertised_id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        kiro_model: "claude-opus-4.6",
        identity_short: "Opus 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_NO_XHIGH,
        default_effort: Some("high"),
    },
    KiroModel {
        advertised_id: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        kiro_model: "claude-opus-4.5",
        identity_short: "Opus 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-opus-4-5-20251101"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
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
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        kiro_model: "claude-sonnet-4.6",
        identity_short: "Sonnet 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_NO_XHIGH,
        default_effort: Some("high"),
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        kiro_model: "claude-sonnet-4.5",
        identity_short: "Sonnet 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-sonnet-4-5-20250929"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
    },
    KiroModel {
        advertised_id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        kiro_model: "claude-haiku-4.5",
        identity_short: "Haiku 4.5",
        context_window: 200_000,
        supports_thinking: false,
        dated_alias: Some("claude-haiku-4-5-20251001"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
    },
];

/// 把一个**已归一**的 effort 档位夹到该模型实际支持的档位上。
///
/// 返回 `None` = 该模型上游没有 effort schema → 调用方**不得**发
/// `additionalModelRequestFields`(与真客户端 `additionalModelRequestFields: undefined` 同形)。
///
/// 夹取规则对齐真客户端 `A7`(`extension.js:140071-140076`)与设置面板
/// (`:340057`):**在 enum 里就原样用,不在就回落 schema 的 `default`**。
/// 客户端在没有 `default` 时取 `levels[0]`,这里同。
///
/// 唯一的自主决定:回落时若 `default` 反而比请求的**更弱**(如 4.6 系请求 `xhigh`
/// 回落到 `high`),我们仍照客户端来 —— 宁可少一档,也不擅自升到 `max` 制造
/// 真客户端不会出现的形态。
pub fn clamp_effort_for_model(model: &str, requested: &str) -> Option<&'static str> {
    let m = lookup_model(model)?;
    if m.effort_levels.is_empty() {
        return None;
    }
    if let Some(hit) = m
        .effort_levels
        .iter()
        .find(|lv| lv.eq_ignore_ascii_case(requested))
    {
        return Some(*hit);
    }
    // 不可用 → 回落 schema default;default 也不在表里(不该发生)时取最低档。
    m.default_effort
        .and_then(|d| m.effort_levels.iter().find(|lv| **lv == d).copied())
        .or_else(|| m.effort_levels.first().copied())
}

/// 按对外名找权威表行:先精确归一([`resolve_base`]),再走子串兜底反查 `kiro_model`。
/// 兜底这一步让老客户端的异名(`openrouter/claude-opus-5-preview`)也能拿到正确档位表。
fn lookup_model(model: &str) -> Option<&'static KiroModel> {
    if let Some(m) = resolve_base(model) {
        return Some(m);
    }
    let mapped = map_model_substring(&model.to_lowercase())?;
    KIRO_MODELS.iter().find(|m| m.kiro_model == mapped)
}

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

    /// 权威表自洽:声明了档位就必须声明 default,且 default 必须真在档位里;
    /// 没档位的行不许留 default。防止手改表时两列脱节。
    #[test]
    fn effort_table_is_self_consistent() {
        for m in KIRO_MODELS {
            if m.effort_levels.is_empty() {
                assert!(
                    m.default_effort.is_none(),
                    "{} 无档位表却留了 default_effort",
                    m.advertised_id
                );
                continue;
            }
            let d = m.default_effort.unwrap_or_else(|| {
                panic!("{} 有档位表但缺 default_effort", m.advertised_id)
            });
            assert!(
                m.effort_levels.contains(&d),
                "{} 的 default_effort={d} 不在它自己的档位表 {:?} 里",
                m.advertised_id,
                m.effort_levels
            );
            // 每个档位都必须是全集里的合法值,否则会发出上游不认的串。
            for lv in m.effort_levels {
                assert!(
                    crate::anthropic_types::VALID_EFFORTS.contains(lv),
                    "{} 的档位 {lv} 不在 VALID_EFFORTS 全集里",
                    m.advertised_id
                );
            }
        }
    }

    #[test]
    fn clamp_effort_matches_upstream_schema() {
        // 支持的档位原样返回。
        assert_eq!(clamp_effort_for_model("claude-opus-5", "max"), Some("max"));
        assert_eq!(clamp_effort_for_model("claude-opus-5", "xhigh"), Some("xhigh"));
        // 4.6 系没有 xhigh → 回落到它 schema 的 default(high),而不是硬发 xhigh。
        assert_eq!(clamp_effort_for_model("claude-opus-4-6", "xhigh"), Some("high"));
        assert_eq!(clamp_effort_for_model("claude-sonnet-4-6", "xhigh"), Some("high"));
        // 但 4.6 系**有** max,顶格请求不该被连累。
        assert_eq!(clamp_effort_for_model("claude-sonnet-4-6", "max"), Some("max"));
        // 4.7 是全表唯一 default=xhigh 的模型。
        assert_eq!(clamp_effort_for_model("claude-opus-4-7", "nonsense"), Some("xhigh"));
        // 无 schema 的模型 → None(调用方据此完全不发该字段)。
        assert_eq!(clamp_effort_for_model("claude-opus-4-5", "high"), None);
        assert_eq!(clamp_effort_for_model("claude-haiku-4-5", "low"), None);
        // 未列名的异名走子串兜底,仍能拿到正确的档位表。
        assert_eq!(
            clamp_effort_for_model("openrouter/claude-opus-5-preview", "max"),
            Some("max")
        );
        // 完全未知的模型 → None,不猜。
        assert_eq!(clamp_effort_for_model("gpt-4o", "high"), None);
        // 大小写不敏感(上游只认小写形态,这里归一到表里的静态串)。
        assert_eq!(clamp_effort_for_model("claude-opus-5", "MAX"), Some("max"));
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
