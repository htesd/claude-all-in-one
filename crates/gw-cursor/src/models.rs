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

use gw_core::account::Account;
use gw_core::model::ModelInfo;
use serde_json::Value;

use crate::run::Model;

/// 兜底模型。
///
/// 取 `default`(UI 的 "Auto",路由到 Composer)而不是某个 claude —— §4 实测:
/// 第三方前沿模型(claude / gpt)在 pro 号上会因计费额度耗尽被拒并自动降级,
/// 而 Cursor 自家模型不受此限。兜底就该挑最不容易被拒的那个。
pub const DEFAULT_MODEL: &str = "default";

/// Run 端点的完整模型目录。
///
/// 2026-08-10 扩充到 33 项:来源是真机 Cursor 客户端 `state.vscdb` 里服务器下发的
/// `availableDefaultModels2` 菜单(线上名 + 每模型 parameterDefinitions 全在里面),
/// 比 §4 抓包时的 8 项全得多 —— 当初那张表只是抓包客户端版本的菜单快照。
/// 参数默认值按菜单的 parameterDefinitions 选取(claude 系 thinking=true/effort=high,
/// gpt 系 reasoning=medium,fast=false),都在该模型声明支持的参数集合内;
/// 上游若报参数非法,先核对这里与最新菜单的差异。
///
/// 尾部再合并 [`set_extra_models`] 热加载的追加项:同名覆盖内置条目,新名追加。
pub fn catalog() -> Vec<Model> {
    let mut v = base_catalog();
    let extras = extra_models();
    if !extras.is_empty() {
        let names: std::collections::HashSet<&str> =
            extras.iter().map(|m| m.name.as_str()).collect();
        v.retain(|m| !names.contains(m.name.as_str()));
        v.extend(extras);
    }
    v
}

/// 热加载的追加模型(进程级全局)。worker 的 30s 设置环每跳回写一次;
/// 非 cursor 进程从不读目录,写到它身上是无害 no-op。
static EXTRA_MODELS: std::sync::RwLock<Vec<Model>> = std::sync::RwLock::new(Vec::new());

/// 整表替换热追加项(空 vec = 全撤,回纯内置目录)。
///
/// 菜单位置由调用方按配置里的 `menu` 标志决定(`Model::probe()` = 可被点名但
/// 不进 `1.14` 清单);本函数只做原样替换,不擅自改标志。
pub fn set_extra_models(models: Vec<Model>) {
    *EXTRA_MODELS.write().unwrap() = models;
}

fn extra_models() -> Vec<Model> {
    EXTRA_MODELS.read().unwrap().clone()
}

/// 内置目录(2026-08-10 对齐的 33 项 + 代码内转正的探测项)。
fn base_catalog() -> Vec<Model> {
    vec![
        Model::new("default"),
        Model::with_params("grok-4.5", &[("effort", "high"), ("fast", "false")]),
        // 2026-08-13:xAI 已发 grok-4.6(08-07);同日实测 Cursor 上游**可用**
        // (pro2 号真实请求 200 出字)。仍保持 probe() 不进 `1.14` 清单:线上菜单快照
        // 仍是 33 项,等下次对齐真机菜单时再决定要不要转正。
        Model::with_params("grok-4.6", &[("effort", "high"), ("fast", "false")]).probe(),
        Model::with_params("composer-2.5", &[("fast", "true")]),
        // ── claude 5 系 ──
        Model::with_params(
            "claude-opus-5",
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
        // ── claude 4.x 系 ──
        Model::with_params(
            "claude-opus-4-8",
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "claude-opus-4-7",
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "claude-opus-4-6",
            &[("thinking", "true"), ("context", "200k"), ("effort", "high")],
        ),
        Model::with_params("claude-opus-4-5", &[("thinking", "true")]),
        Model::with_params(
            "claude-sonnet-4-6",
            &[("thinking", "true"), ("context", "200k"), ("effort", "high")],
        ),
        Model::with_params(
            "claude-sonnet-4-5",
            &[("thinking", "true"), ("context", "200k")],
        ),
        Model::with_params(
            "claude-sonnet-4",
            &[("thinking", "true"), ("context", "200k")],
        ),
        Model::with_params("claude-haiku-4-5", &[("thinking", "true")]),
        // ── gpt 系 ──
        Model::with_params(
            "gpt-5.6-sol",
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
        Model::with_params(
            "gpt-5.6-terra",
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
        Model::with_params(
            "gpt-5.6-luna",
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
        Model::with_params(
            "gpt-5.5",
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
        Model::with_params(
            "gpt-5.4",
            &[("context", "272k"), ("reasoning", "medium"), ("fast", "false")],
        ),
        Model::with_params("gpt-5.4-mini", &[("reasoning", "medium")]),
        Model::with_params("gpt-5.4-nano", &[("reasoning", "medium")]),
        Model::with_params(
            "gpt-5.3-codex",
            &[("reasoning", "high"), ("fast", "false")],
        ),
        Model::with_params("gpt-5.2", &[("reasoning", "medium"), ("fast", "false")]),
        Model::with_params("gpt-5.1", &[("reasoning", "medium")]),
        Model::new("gpt-5-mini"),
        // ── gemini / 其他 ──
        Model::with_params("gemini-3.6-flash", &[("effort", "medium")]),
        Model::new("gemini-3.1-pro"),
        Model::new("gemini-3.5-flash"),
        Model::new("gemini-3-flash"),
        Model::new("gemini-2.5-flash"),
        Model::with_params("kimi-k3", &[("reasoning", "high")]),
        Model::new("kimi-k2.7-code"),
        Model::with_params("glm-5.2", &[("reasoning", "high")]),
    ]
}

/// 按 Cursor 侧模型名取目录条目(含参数);不认识则给一个无参条目。
pub fn model_by_name(cursor_name: &str) -> Model {
    catalog()
        .into_iter()
        .find(|m| m.name == cursor_name)
        .unwrap_or_else(|| Model::new(cursor_name))
}

/// 把对外模型名(Anthropic 连字符名,或已经是 Cursor 名)映射为 Cursor Run 侧模型名;
/// **认不出的名字返回 None**。
///
/// None 只给「不认识」:目录不中、剥日期后缀不中、也不属于任何已知家族。
/// 已知家族的**有意**降级不算(目录外 haiku → composer、目录外 gemini → 交服务端
/// 路由),那是「认识但降级」。消费方:chat.rs 在请求路径对 None 直接 400 ——
/// 旧行为是未知名静默回退 `default`,叠加含 `default` 的白名单时任意拼错的
/// 模型名都会放行,客户端**静默拿到另一个模型**(交接规格第 8 条危险点);
/// `account_supports` 对受限号按 fail-closed 处理 None。
pub fn resolve_cursor_model(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty() {
        // 空名是「没说要哪个」而不是「要了一个不存在的」——维持交给服务端路由。
        return Some(DEFAULT_MODEL.to_string());
    }
    // 已经是 Run 目录里的名字 → 原样。
    if catalog().iter().any(|m| m.name == n) {
        return Some(n.to_string());
    }
    // Anthropic 日期后缀(claude-sonnet-4-5-20250929):剥掉 `-YYYYMMDD` 再查一次目录 ——
    // 目录已有 claude-sonnet-4-5 这类真身,剥缀命中就该用真身,而不是落到下面的
    // 按族归并被静默升到 5 系(审查 r1 中危)。剥缀后仍不中的才走族归一。
    if let Some(base) = n.rsplit_once('-')
        .filter(|(_, suf)| suf.len() == 8 && suf.bytes().all(|b| b.is_ascii_digit()))
        .map(|(base, _)| base)
    {
        if catalog().iter().any(|m| m.name == base) {
            return Some(base.to_string());
        }
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
        // 目录外的 haiku(如更新的小版本)降到 composer(Cursor 自家的快模型),
        // 而不是升到 sonnet —— 客户点 haiku 是要便宜快,不是要更强。
        // (目录内的 claude-haiku-4-5 已在上面精确命中透传,到不了这里。)
        "composer-2.5"
    } else if lower.starts_with("gpt-") || lower.starts_with("o1") || lower.starts_with("o3") {
        "gpt-5.6-sol"
    } else if lower.contains("grok") {
        "grok-4.5"
    } else if lower.contains("gemini") {
        // 目录外的 gemini 交给服务端路由 —— 有意降级,不是未知名。
        DEFAULT_MODEL
    } else {
        // 任何已知家族都不沾边 = 真未知。绝不静默回退 default。
        return None;
    };
    Some(mapped.to_string())
}

/// [`resolve_cursor_model`] 的兼容包装:认不出回退 `default`。
/// 只服务展示/估算类调用与测试;**请求路径必须用 resolve 版**并对 None 报 400,
/// 否则未知名叠加含 `default` 的白名单会让客户端静默拿到另一个模型。
pub fn to_cursor_model(name: &str) -> String {
    resolve_cursor_model(name).unwrap_or_else(|| DEFAULT_MODEL.to_string())
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
        // `adaptive`(Claude Code)与 `enabled` 一样要开上游思考 —— 否则收侧透传了
        // 思考块、上游却按「客户没要」关档,两边口径拧巴。
        Some("enabled" | "adaptive") => {
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

/// 账号级模型白名单(`extra.model_allowlist`,通用键;纯匹配器在
/// [`gw_core::account::model_allowlist_allows`],这里只负责 cursor 的模型名归一)。
///
/// ## 为什么需要它
///
/// Cursor 账号的**模型权限不齐**:一部分号只有自家模型(`composer` / `default`)
/// 和 `grok`,claude / gpt 这些第三方前沿模型要另外的计费额度。不过滤的后果是
/// 每个 claude 请求都可能落到一个没有权限的号上,换来一次
/// `ERROR_RATE_LIMITED_CHANGEABLE`(带 `autoSwitchToModel`)—— 调度层虽然会把
/// `(号,模型)` 记 6 小时不可用并换号重试,但那是**先失败一次才学会**:每个新
/// 上线的号、每次 6h TTL 过期,都要再赔一次首包前的失败与重试延迟。
///
/// 静态白名单把这次失败挪到调度前:不支持的号从合格集直接剔除
/// (见 [`Provider::account_supports_model`](gw_core::provider::Provider)),
/// 一次上游请求都不发。两者是互补的 —— 白名单管**已知**的权限差异,
/// 学习机制兜住**未申报**的(配错了、或上游中途收权限)。
///
/// ## 归一口径
///
/// 判定前先过 [`resolve_cursor_model`]:白名单写的是 Run 侧名字,而客户发来的是
/// `claude-sonnet-4-5-20250929` 这类带日期后缀 / 族别名的名字。少了这一步,
/// 白名单 `claude-*` 会把带日期后缀的放行(碰巧前缀也对)但把
/// `claude-4.5-sonnet`(旧点分名,归一到 `claude-sonnet-5`)挡掉,口径全靠运气。
/// 受限号遇到**解析不出**的名字按 fail-closed(false)—— 请求路径反正会对
/// 未知名报 400(见 chat.rs),这里只是不让受限号进候选集。
///
/// 语义细节(缺失/null=不限、空表/类型错=全禁、末尾通配、CSV 兼容)见
/// [`gw_core::account::model_allowlist_allows`] —— 单一事实来源,这里不复制。
pub fn account_supports(account: &Account, requested: &str) -> bool {
    use gw_core::account::{model_allowlist_allows, MODEL_ALLOWLIST_KEY};
    // 快路径:未配置(键缺失/null)= 不限,连模型名归一都不用做 ——
    // 调度器在锁内对每个候选号调用,全权限号(多数)不该付 catalog() 的钱。
    match account.extra.get(MODEL_ALLOWLIST_KEY) {
        None | Some(Value::Null) => return true,
        _ => {}
    }
    match resolve_cursor_model(requested) {
        Some(upstream) => model_allowlist_allows(account, &upstream),
        None => false,
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

    /// catalog 快照断言与热追加测试共享的串行锁:EXTRA_MODELS 是进程全局,
    /// 并行跑会互相污染。
    static CATALOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn catalog_matches_server_menu() {
        let _g = CATALOG_TEST_LOCK.lock().unwrap();
        // 2026-08-10:对齐真机客户端服务器下发的 availableDefaultModels2(33 项)。
        // 例外:grok-4.6 是 2026-08-13 加的**探测项**(不在 33 项菜单快照里,见 catalog 注释)。
        let names: Vec<String> = catalog().into_iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            vec![
                "default",
                "grok-4.5",
                "grok-4.6",
                "composer-2.5",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-opus-4-5",
                "claude-sonnet-4-6",
                "claude-sonnet-4-5",
                "claude-sonnet-4",
                "claude-haiku-4-5",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.4-nano",
                "gpt-5.3-codex",
                "gpt-5.2",
                "gpt-5.1",
                "gpt-5-mini",
                "gemini-3.6-flash",
                "gemini-3.1-pro",
                "gemini-3.5-flash",
                "gemini-3-flash",
                "gemini-2.5-flash",
                "kimi-k3",
                "kimi-k2.7-code",
                "glm-5.2",
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
        // 目录里已有的精确名原样透传(2026-08-10 目录扩充后 4.x 系都在)。
        assert_eq!(to_cursor_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(to_cursor_model("claude-sonnet-4-5"), "claude-sonnet-4-5");
        assert_eq!(to_cursor_model("claude-fable-5"), "claude-fable-5");
        assert_eq!(to_cursor_model("claude-haiku-4-5"), "claude-haiku-4-5");
        // 目录外的名字按族归一到旗舰。
        assert_eq!(to_cursor_model("claude-opus-9-9"), "claude-opus-5");
        assert_eq!(to_cursor_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(to_cursor_model("gpt-4o"), "gpt-5.6-sol");
        assert_eq!(to_cursor_model("grok-4.5"), "grok-4.5");
    }

    #[test]
    fn dated_suffix_matches_catalog_base_before_family_fallback() {
        // Anthropic 日期后缀:剥掉 -YYYYMMDD 命中目录真身,不被静默升到 5 系(审查 r1 中危)。
        assert_eq!(to_cursor_model("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(to_cursor_model("claude-opus-4-6-20260101"), "claude-opus-4-6");
        assert_eq!(to_cursor_model("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        // 剥缀也不中的才族归一。
        assert_eq!(to_cursor_model("claude-sonnet-9-9-20990101"), "claude-sonnet-5");
        // 非 8 位数字后缀不剥(避免误伤正常名字)。
        assert_eq!(to_cursor_model("claude-sonnet-4-5-thinking"), "claude-sonnet-5");
    }

    #[test]
    fn unknown_and_empty_fall_back_to_default() {
        // resolve 版:真未知返回 None(请求路径据此 400,绝不静默换模型 ——
        // 交接规格第 8 条危险点);空名 = 「没说要哪个」,仍交服务端路由。
        assert_eq!(resolve_cursor_model("totally-unknown"), None);
        assert_eq!(resolve_cursor_model("").as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(resolve_cursor_model("   ").as_deref(), Some(DEFAULT_MODEL));
        // 已知家族的**有意**降级不是未知:目录外 gemini 交服务端路由。
        assert_eq!(resolve_cursor_model("gemini-9.9-ultra").as_deref(), Some(DEFAULT_MODEL));
        // 兼容包装(展示/估算类调用用)维持旧回退。
        assert_eq!(to_cursor_model("totally-unknown"), DEFAULT_MODEL);
        assert_eq!(to_cursor_model(""), DEFAULT_MODEL);
        assert_eq!(DEFAULT_MODEL, "default");
    }

    /// claude 系必须带 `thinking` 参数 —— 它是我方能拉的唯一「要推理」的杠杆。
    #[test]
    fn claude_models_declare_thinking() {
        for m in catalog() {
            if m.name.starts_with("claude-") {
                let th = m.params.iter().find(|(k, _)| k == "thinking");
                assert!(th.is_some(), "{} 没声明 thinking 参数", m.name);
                assert_eq!(th.unwrap().1, "true", "{} 的 thinking 值", m.name);
            }
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

        // adaptive(Claude Code)必须显式开 true,不能掉进「不认识就不动」后碰巧依赖默认。
        let mut m = model_by_name("claude-fable-5");
        for (k, v) in m.params.iter_mut() {
            if k == "thinking" {
                *v = "false".to_string();
            }
        }
        apply_thinking_pref(&mut m, Some(&serde_json::json!({"type": "adaptive"})));
        assert_eq!(
            get(&m, "thinking").as_deref(),
            Some("true"),
            "adaptive 必须打开上游 thinking"
        );
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
        // 与 extras 测试共用锁:它会把 grok-4.5 的参数临时换掉。
        let _g = CATALOG_TEST_LOCK.lock().unwrap();
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
    fn grok46_selectable_but_not_in_menu() {
        // 与 extras 测试共用锁:它会临时往目录里加探测项。
        let _g = CATALOG_TEST_LOCK.lock().unwrap();
        // 探测项(2026-08-13):精确名透传,不被族归一吞回 grok-4.5;参数照抄 grok-4.5。
        assert_eq!(to_cursor_model("grok-4.6"), "grok-4.6");
        let m = model_by_name("grok-4.6");
        assert_eq!(
            m.params,
            vec![
                ("effort".to_string(), "high".to_string()),
                ("fast".to_string(), "false".to_string())
            ]
        );
        // 但它不进 1.14 清单,且目前目录里只有它一个探测项。
        assert!(!m.menu_visible);
        assert!(
            catalog()
                .iter()
                .filter(|x| !x.menu_visible)
                .all(|x| x.name == "grok-4.6"),
            "探测项应该只有 grok-4.6"
        );
    }

    #[test]
    fn extra_models_override_and_append() {
        let _g = CATALOG_TEST_LOCK.lock().unwrap();
        // panic 也要清空全局,别污染同进程的其它测试。
        struct ExtraGuard;
        impl Drop for ExtraGuard {
            fn drop(&mut self) {
                set_extra_models(Vec::new());
            }
        }
        let _cleanup = ExtraGuard;

        set_extra_models(vec![
            Model::with_params("grok-4.7", &[("effort", "high")]).probe(),
            // 同名覆盖内置项(连参数一起换)。
            Model::with_params("grok-4.5", &[("fast", "true")]),
        ]);
        let names: Vec<String> = catalog().into_iter().map(|m| m.name).collect();
        assert!(names.iter().any(|n| n == "grok-4.7"), "新名要追加进目录");
        assert_eq!(
            names.iter().filter(|n| *n == "grok-4.5").count(),
            1,
            "同名覆盖不该留两份"
        );
        assert_eq!(
            model_by_name("grok-4.5").params,
            vec![("fast".to_string(), "true".to_string())],
            "覆盖要连参数一起换"
        );
        assert!(!model_by_name("grok-4.7").menu_visible, "probe 标志要保留");
        // 目录内精确名透传(不被族归一吞回 grok-4.5)。
        assert_eq!(to_cursor_model("grok-4.7"), "grok-4.7");
        // /v1/models 的 list() 也走同一目录,热加项要可见。
        assert!(list().iter().any(|m| m.id == "grok-4.7"));

        drop(_cleanup);
        assert!(!catalog().iter().any(|m| m.name == "grok-4.7"), "清空后要回纯内置目录");
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

    fn acct_models(v: serde_json::Value) -> Account {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(gw_core::account::MODEL_ALLOWLIST_KEY.to_string(), v);
        Account {
            account_id: "c1".into(),
            provider: "cursor".into(),
            max_concurrency: 2,
            disabled: false,
            created_at: 0,
            extra,
        }
    }

    #[test]
    fn 未配白名单等于不限() {
        // fail-open 只给「未配置」:键缺失,或值为 null(admin 清除白名单的
        // 落库形态 —— merge_account_extra 是读-合-写,写 null 不删键)。
        let mut a = acct_models(serde_json::Value::Null);
        assert!(account_supports(&a, "claude-opus-5"));
        assert!(account_supports(&a, "grok-4.5"));
        a.extra.clear();
        assert!(account_supports(&a, "claude-opus-5"));
    }

    #[test]
    fn 空表与类型错全禁() {
        // ⚠️ 与旧语义相反(旧版把空串/空数组当「未配 = 不限」)。写侧拒绝这些
        // 形态,能出现只可能是手改 DB/YAML —— 运维写空值的本意多半是「全禁」,
        // 当成「全放」是要出事的(gpt-5.6-sol 评审定稿,交接规格第 4 条)。
        assert!(!account_supports(&acct_models(serde_json::json!("")), "claude-opus-5"));
        assert!(!account_supports(&acct_models(serde_json::json!("  , ;")), "claude-opus-5"));
        assert!(!account_supports(&acct_models(serde_json::json!([])), "claude-opus-5"));
        assert!(!account_supports(&acct_models(serde_json::json!(42)), "claude-opus-5"));
    }

    #[test]
    fn 只有自家模型的号挡掉_claude_放行_grok() {
        let a = acct_models(serde_json::json!("default,composer*,grok*"));
        assert!(account_supports(&a, "default"));
        assert!(account_supports(&a, "composer-2.5"));
        assert!(account_supports(&a, "grok-4.5"));
        assert!(account_supports(&a, "grok-4.6"));
        assert!(!account_supports(&a, "claude-opus-5"));
        assert!(!account_supports(&a, "gpt-5.6-sol"));
    }

    #[test]
    fn 判定前先过归一_带日期后缀与旧点分名口径一致() {
        // 白名单写 Run 侧名字,客户发来的是带日期后缀 / 旧点分名 / 族别名。
        // 少了 resolve_cursor_model 这一步,`claude-4.5-sonnet` 会被字面比对挡掉。
        let a = acct_models(serde_json::json!("claude-*"));
        assert!(account_supports(&a, "claude-sonnet-4-5-20250929"));
        assert!(account_supports(&a, "claude-4.5-sonnet"), "旧点分名归一到 claude-sonnet-5");
        assert!(account_supports(&a, "claude-opus-9-9"), "族归一到 claude-opus-5");
        assert!(!account_supports(&a, "grok-4.5"));

        // 未知名字 resolve 不出 → 受限号 fail-closed:白名单含 default 也不放行 ——
        // 旧语义(未知名先归一成 default 再判)正是规格第 8 条要拆的「拼错名字
        // 静默拿到另一个模型」。请求路径对未知名统一 400(见 chat.rs),
        // 这里只保证受限号不进候选集。
        let g = acct_models(serde_json::json!("grok*"));
        assert!(!account_supports(&g, "totally-unknown"));
        assert!(!account_supports(
            &acct_models(serde_json::json!("grok*,default")),
            "totally-unknown"
        ));
        // 未配置的号不受影响:走快路径放行,由请求路径统一 400。
        let mut open = acct_models(serde_json::Value::Null);
        open.extra.clear();
        assert!(account_supports(&open, "totally-unknown"));
    }

    #[test]
    fn 全名精确匹配不吃前缀() {
        // 没写 `*` 就是全等:`grok-4.5` 不该顺带放行 `grok-4.6`(新模型要显式上架)。
        let a = acct_models(serde_json::json!("grok-4.5"));
        assert!(account_supports(&a, "grok-4.5"));
        assert!(!account_supports(&a, "grok-4.6"));
    }

    #[test]
    fn 大小写与_json_数组写法都收() {
        let a = acct_models(serde_json::json!(["GROK-4.5", " Default "]));
        assert!(account_supports(&a, "grok-4.5"));
        assert!(account_supports(&a, "default"));
        assert!(!account_supports(&a, "claude-opus-5"));
    }
}
