//! Anthropic API 类型定义

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

// === 错误响应 ===

/// API 错误响应
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

/// 错误详情
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl ErrorResponse {
    /// 创建新的错误响应
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    /// 创建认证错误响应
    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid API key")
    }
}

// === Models 端点类型 ===

/// 模型信息
#[derive(Debug, Serialize)]
pub struct Model {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub model_type: String,
    pub max_tokens: i32,
}

/// 模型列表响应
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

// === Messages 端点类型 ===

/// 最大思考预算 tokens
const MAX_BUDGET_TOKENS: i32 = 24576;

/// Thinking 配置
#[derive(Debug, Deserialize, Clone)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub thinking_type: String,
    /// 显示模式(如 `summarized`)。Anthropic 客户端用它表达"要看到思考摘要"。
    /// 用于 surface 开关:adaptive 模式下 display==summarized 才向客户端暴露 thinking 块。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(
        default = "default_budget_tokens",
        deserialize_with = "deserialize_budget_tokens"
    )]
    pub budget_tokens: i32,
}

impl Thinking {
    /// 是否启用了 thinking（enabled 或 adaptive）—— 即"给上游开思考"(upstream switch)。
    pub fn is_enabled(&self) -> bool {
        self.thinking_type == "enabled" || self.thinking_type == "adaptive"
    }

    /// 是否向客户端暴露 Anthropic thinking 块（surface switch,与 upstream 解耦）。
    ///
    /// 🟢 借鉴 static_flow `exposes_anthropic_thinking`(types.rs:79),但口径对齐我方:
    /// - `enabled`:总是暴露(客户端显式要 thinking)。
    /// - `adaptive`:仅当 `display=="summarized"`(客户端要思考摘要)或带 `output_config`
    ///   (我方 override 给 Opus 默认开 adaptive 时会同时带 OutputConfig,代表"要思考")才暴露。
    /// - 其它:不暴露。
    ///
    /// 解耦动机:`hidden_thinking = is_enabled && !exposes` —— 给上游开思考保智力,
    /// 但不把 thinking 块吐给客户端(如某些客户端只要最终答案、不要推理流)。
    pub fn exposes_anthropic_thinking(&self, output_config: Option<&OutputConfig>) -> bool {
        match self.thinking_type.as_str() {
            "enabled" => true,
            "adaptive" => self.display.as_deref() == Some("summarized") || output_config.is_some(),
            _ => false,
        }
    }
}

fn default_budget_tokens() -> i32 {
    20000
}
fn deserialize_budget_tokens<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = i32::deserialize(deserializer)?;
    Ok(value.min(MAX_BUDGET_TOKENS))
}

/// OutputConfig 配置
#[derive(Debug, Deserialize, Clone)]
pub struct OutputConfig {
    /// 思考强度。`None` = 客户端未指定 → 用 [`OutputConfig::effective_effort`] 的默认。
    /// 对齐 static_flow:`Option<String>`,缺省回退 "xhigh"(深推理),而非旧的写死 "high"
    /// (后者只产 ~43 字符的桩推理;xhigh 约 3560 字符)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 结构化输出格式（`{type:"json_schema", schema:{...}}`）。
    /// 客户端要求模型输出严格符合 schema 的 JSON 时携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
}

/// 结构化输出格式描述
#[derive(Debug, Deserialize, Clone)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    pub format_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

impl OutputConfig {
    /// 若配置了 json_schema 结构化输出，返回其 schema。
    pub fn json_schema(&self) -> Option<&serde_json::Value> {
        self.format
            .as_ref()
            .filter(|f| f.format_type == "json_schema")
            .and_then(|f| f.schema.as_ref())
    }

    /// 有效思考强度:客户端显式值经白名单归一后返回(非法值回退 [`DEFAULT_EFFORT`])。
    /// 对齐 static_flow `effective_effort`,但叠加合法化(见 [`normalize_effort`])。
    pub fn effective_effort(&self) -> &'static str {
        normalize_effort(self.effort.as_deref()).0
    }
}

/// 默认思考强度的**编译期兜底**。运行期实际用的是 [`default_effort()`](fn@default_effort)
/// (可经设置面板热改),本常量只是它的初值与锁中毒/脏值时的回退。
///
/// 定义在 `gw_core::config::DEFAULT_THINKING_EFFORT`,这里只是别名 —— 它同时是配置 schema
/// 的默认值,两处各写一份字面量必然漂移。
///
/// **2026-07-30 由 `max` 降到 `high`,原因是延迟 —— 用户反馈"太慢了"。**
/// 深度不是免费的,同一道证明题下的实测(claude-opus-5,只发新字段、不带旧标签):
/// | effort | `thinking_delta` 帧数 | 签名加密体 | 端到端耗时 |
/// |---|---|---|---|
/// | `low`   | 122(123, 121)      | 10744 B | —    |
/// | `high`  | —                   | 15940 B | 95s  |
/// | `xhigh` | 644(894, 509, 529) | 21984 B | 124s |
/// | `max`   | 1100(1345, 855)    | —       | —    |
/// 帧数与签名长度都随档位单调,`additionalModelRequestFields.effort` 是**真正在起作用的
/// 旋钮**;但反过来说 `max` 那约 1.7 倍于 `xhigh` 的思考量,也就是约 1.7 倍的等待与输出计费。
/// `high` 的加密体是 `xhigh` 的 73%、耗时 95s vs 124s,是深度/延迟的折中点。
///
/// ⚠️ **`high` 不是"桩推理"**。旧注释曾断言 high 只产桩,2026-07-28 实测已推翻(见上表)。
/// 也别拿**可见** thinking 文本长度判断深度:同批实测里 high 的可见摘要只有 1579 字符
/// (全场最短),而它的加密体反而比 low 大 48%。要看深度就看签名长度和耗时。
///
/// 这个值只在**客户端没说话**时用。显式点了 `max` 的请求照原样上 wire、不降级
/// (见 `thinking_policy` 的 `client_max_effort_reaches_the_wire_undowngraded`)。
///
/// 注意这是 caio 的**策略默认**,与"客户端给了脏值时回落到该模型 schema 的 `default`"
/// 是两件事 —— 后者见 `thinking_policy::EffortWish::ModelDefault`。
///
/// 档位还要过 `clamp_effort_for_model` 按模型夹一次。`high` 在**所有**带 schema 的模型上
/// 都存在(含没有 `xhigh` 的 4.6 系),所以对全系都能原样落地;且它正好等于除 4.7 以外
/// 所有模型 schema 里的 `default`。**唯一的例外是 opus-4.7**(schema `default` 是 `xhigh`):
/// 对它我们现在会显式发一个比上游默认**更低**的档 —— 这是本次降档的本意,不是 bug。
pub const DEFAULT_EFFORT: &str = gw_core::config::DEFAULT_THINKING_EFFORT.as_str();

/// 运行期默认档位。设置面板改 → DB overlay → worker 30s 轮询 →
/// [`crate::KiroProvider::apply_hot_settings`] → [`set_default_effort`],**无需重启**。
///
/// 与 `converter/cache_point.rs` 的实验开关同款进程级全局。为什么不用依赖注入:转换层
/// (`thinking_policy` / `converter`)是一组自由函数,`chat_stream` 与 `render_kiro_payload`
/// 都不持有 provider 句柄,把参数一路穿下去要改到 gw-app 的请求日志路径。
fn runtime_default_effort() -> &'static RwLock<&'static str> {
    static G: OnceLock<RwLock<&'static str>> = OnceLock::new();
    G.get_or_init(|| RwLock::new(canonical_effort(DEFAULT_EFFORT).unwrap_or("high")))
}

/// 在 [`VALID_EFFORTS`] 里找与 `s` 等价(大小写不敏感)的**静态**串。
/// 不读全局,故可安全用于 [`runtime_default_effort`] 的初始化。
fn canonical_effort(s: &str) -> Option<&'static str> {
    VALID_EFFORTS.iter().copied().find(|v| v.eq_ignore_ascii_case(s))
}

/// 当前生效的默认档位。锁中毒时回退编译期 [`DEFAULT_EFFORT`](保守:维持出厂行为)。
pub fn default_effort() -> &'static str {
    runtime_default_effort()
        .read()
        .map(|g| *g)
        .unwrap_or(DEFAULT_EFFORT)
}

/// 热改默认档位。返回归一后的档位;`raw` 不是合法档位时返回 `None` 且**不改动**当前值。
///
/// 校验放在这里是因为 DB overlay 可以被手工改(绕过 admin 的 `PUT /settings` 校验),
/// 一个脏档位打到上游会换来 400。调用方据 `None` 决定告警。
pub fn set_default_effort(raw: &str) -> Option<&'static str> {
    let canonical = canonical_effort(raw.trim())?;
    if let Ok(mut g) = runtime_default_effort().write() {
        *g = canonical;
    }
    Some(canonical)
}

/// Kiro 支持的 thinking effort 档位**全集**,由低到高。客户端传入须命中其一(大小写不敏感)
/// 才透传上游;非空但非法的值会被回退到 [`DEFAULT_EFFORT`],避免脏 effort 串打到 Kiro 触发 400。
///
/// ⚠️ **`max` 是合法的最高档,不是同义词。** 2026-07-28 从真实 `ListAvailableModels` 取回的
/// `additionalModelRequestFieldsSchema` 逐字为
/// `"effort": {"enum": ["low","medium","high","xhigh","max"], "default": "high"}`
/// (claude-opus-5 / sonnet-5 / opus-4.8 / opus-4.7)。本表此前漏了 `max` 并把它当同义词
/// 映射成 `xhigh`,等于把客户端**顶格**的请求(生产每 300 条约 33 条)静默**降一级**。
///
/// **这是全集,不是任一模型的可用集** —— 档位**逐模型不同**(如 4.6 系没有 `xhigh`)。
/// 实际发包前必须再经 [`crate::converter::model_effort_levels`] 按模型夹一次。
pub const VALID_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// 归一客户端 effort 到合法档位。返回 `(归一后的 &'static str, 是否因非法而回退)`:
/// - `None` / 纯空白:未指定 → `(当前默认档, false)`(默认,非告警情形);
/// - 命中 [`VALID_EFFORTS`](大小写不敏感):`(该档, false)`;
/// - 非空且不命中:`(当前默认档, true)`(调用方据 bool 决定是否告警)。
///
/// "当前默认档"取 [`default_effort()`](fn@default_effort)(设置面板可热改),不是编译期常量。
/// 只管"是不是合法档位",**不管该模型有没有这一档** —— 后者见 `model_effort_levels`。
pub fn normalize_effort(raw: Option<&str>) -> (&'static str, bool) {
    normalize_effort_with(default_effort(), raw)
}

/// [`normalize_effort`] 的**纯函数**版:显式给定"未指定/非法时用哪档",不读全局。
///
/// 归一逻辑全在这里,本模块单测走这个入口 —— 否则测试要改进程级全局,与并发跑的其它用例
/// 互相污染。"热改真的生效"由 `tests/thinking_effort_hot_reload.rs` 覆盖(独立进程,
/// 可以放心改全局)。
///
/// **故意保持私有**:`fallback` 不校验合法性,公开出去等于给外部一条绕过档位白名单、
/// 把任意串送上 wire 的路(对抗审查 Minimalist#1)。crate 内的唯一调用方是
/// [`normalize_effort`],它喂进来的是 [`default_effort()`](fn@default_effort),恒为合法档位。
fn normalize_effort_with(fallback: &'static str, raw: Option<&str>) -> (&'static str, bool) {
    let s = match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return (fallback, false),
    };
    match canonical_effort(s) {
        Some(hit) => (hit, false),
        None => (fallback, true),
    }
}

/// Claude Code 请求中的 metadata
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// 用户 ID，格式如: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
    pub user_id: Option<String>,
}

/// Messages 请求体
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: i32,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, deserialize_with = "deserialize_system")]
    pub system: Option<Vec<SystemMessage>>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<serde_json::Value>,
    pub thinking: Option<Thinking>,
    pub output_config: Option<OutputConfig>,
    /// Claude Code 请求中的 metadata，包含 session 信息
    pub metadata: Option<Metadata>,
    /// Anthropic context-management beta（"remote compact"）：上游应对历史做裁剪
    /// 的指令，形如 `{edits:[{type:"clear_tool_uses_20250605", ...}]}`。
    /// Kiro 后端不原生支持此能力（已对照同生态 kiro-gateway / AIClient-2-API 确认
    /// 无人实现），收到时仅记日志、不报错——客户端通常会在本地回退应用同等裁剪。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
}

/// 反序列化 system 字段，支持字符串或数组格式
fn deserialize_system<'de, D>(deserializer: D) -> Result<Option<Vec<SystemMessage>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 创建一个 visitor 来处理 string 或 array
    struct SystemVisitor;

    impl<'de> serde::de::Visitor<'de> for SystemVisitor {
        type Value = Option<Vec<SystemMessage>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or an array of system messages")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(vec![SystemMessage {
                text: value.to_string(),
            }]))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut messages = Vec::new();
            while let Some(msg) = seq.next_element()? {
                messages.push(msg);
            }
            Ok(if messages.is_empty() {
                None
            } else {
                Some(messages)
            })
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            serde::de::Deserialize::deserialize(deserializer)
        }
    }

    deserializer.deserialize_any(SystemVisitor)
}

/// 消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    /// 可以是 string 或 ContentBlock 数组
    pub content: serde_json::Value,
}

/// 系统消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMessage {
    pub text: String,
}

/// 工具定义
///
/// 支持两种格式：
/// 1. 普通工具：{ name, description, input_schema }
/// 2. WebSearch 工具：{ type: "web_search_20250305", name: "web_search", max_uses: 8 }
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    /// 工具类型，如 "web_search_20250305"（可选，仅 WebSearch 工具）
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    /// 工具名称
    #[serde(default)]
    pub name: String,
    /// 工具描述（普通工具必需，WebSearch 工具可选）
    #[serde(default)]
    pub description: String,
    /// 输入参数 schema（普通工具必需，WebSearch 工具无此字段）
    #[serde(default)]
    pub input_schema: HashMap<String, serde_json::Value>,
    /// 最大使用次数（仅 WebSearch 工具）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i32>,
}

/// 内容块
#[derive(Debug, Deserialize, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ImageSource>,
    /// Anthropic 缓存控制（例如 `{"type": "ephemeral"}`），用于翻译成 Kiro cachePoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
}

/// 图片数据源
///
/// Anthropic API 支持多种 source 形态：
/// - `{type:"base64", media_type:"image/png", data:"..."}` ← 最常见，Kiro 直接吃
/// - `{type:"url", url:"https://..."}` ← 较新，需要我们抓取后转 base64
/// - `{type:"file", file_id:"..."}` ← 走 file API，目前不支持
///
/// 字段全部 optional 以确保各种形态都能反序列化成功（不再因缺字段把整个 image block 丢掉）。
#[derive(Debug, Deserialize, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

// === Count Tokens 端点类型 ===

/// Token 计数请求
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_system"
    )]
    pub system: Option<Vec<SystemMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

/// Token 计数响应
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensResponse {
    pub input_tokens: i32,
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    /// 回退档位一律走**纯函数**入口断言。`normalize_effort` 读的是可热改的进程级全局,
    /// 拿它断言等于让用例依赖全局当下的值 —— 并发跑的其它用例一改就互相污染。
    const FB: &str = "high";

    #[test]
    fn none_is_default_no_fallback_flag() {
        assert_eq!(normalize_effort_with(FB, None), (FB, false));
        // 未被热改时,读全局的入口与编译期兜底一致。
        assert_eq!(normalize_effort(None), (DEFAULT_EFFORT, false));
    }

    #[test]
    fn blank_is_default_no_fallback_flag() {
        // 纯空白视为未指定:用默认档,不算"非法回退"(不告警)。
        assert_eq!(normalize_effort_with(FB, Some("")), (FB, false));
        assert_eq!(normalize_effort_with(FB, Some("   ")), (FB, false));
    }

    #[test]
    fn valid_levels_pass_through() {
        assert_eq!(normalize_effort(Some("low")), ("low", false));
        assert_eq!(normalize_effort(Some("medium")), ("medium", false));
        assert_eq!(normalize_effort(Some("high")), ("high", false));
        assert_eq!(normalize_effort(Some("xhigh")), ("xhigh", false));
        assert_eq!(normalize_effort(Some("max")), ("max", false));
    }

    /// 回归:`max` 曾被当成"客户端方言"映射成 `xhigh`,把顶格请求静默降一级。
    /// 真实 schema 的 enum 是 `[low,medium,high,xhigh,max]` —— 它是**最高档**。
    #[test]
    fn max_is_the_top_tier_not_an_alias_of_xhigh() {
        let (eff, fell_back) = normalize_effort(Some("max"));
        assert_eq!(eff, "max", "max 必须原样透传,不得降级成 xhigh");
        assert!(!fell_back, "max 是合法档位,不该被标记为非法回退");
        assert_eq!(normalize_effort(Some("MAX")), ("max", false), "大小写不敏感");
        // 排序不变量:max 必须排在 xhigh 之后(全表按强度升序,下游按下标比大小)。
        let pos = |e: &str| VALID_EFFORTS.iter().position(|v| *v == e).unwrap();
        assert!(pos("max") > pos("xhigh"), "max 在全表里必须高于 xhigh,实际={VALID_EFFORTS:?}");
    }

    #[test]
    fn valid_levels_case_and_whitespace_insensitive() {
        // 大小写不敏感 + 去前后空白后命中,归一为白名单里的标准小写形态(透传 wire 干净)。
        assert_eq!(normalize_effort(Some("HIGH")), ("high", false));
        assert_eq!(normalize_effort(Some("  XHigh ")), ("xhigh", false));
    }

    #[test]
    fn illegal_value_falls_back_with_flag() {
        // 非空但不在白名单 → 回退默认档且标记 true(调用方据此告警)。
        assert_eq!(normalize_effort_with(FB, Some("ultra")), (FB, true));
        assert_eq!(normalize_effort_with(FB, Some("999")), (FB, true));
        assert_eq!(normalize_effort_with(FB, Some("high; drop")), (FB, true));
    }

    #[test]
    fn effective_effort_delegates_to_normalize() {
        let oc = OutputConfig { effort: Some("garbage".to_string()), format: None };
        assert_eq!(oc.effective_effort(), default_effort(), "脏值回退当前默认档");
        let oc2 = OutputConfig { effort: Some("low".to_string()), format: None };
        assert_eq!(oc2.effective_effort(), "low");
    }

    /// 编译期兜底必须与 gw-core 的配置默认值是**同一个**值(前者是后者的别名)。
    /// 若哪天有人在 gw-kiro 这边写回字面量,这条会立刻挂。
    #[test]
    fn compile_time_fallback_is_the_gw_core_constant() {
        assert_eq!(DEFAULT_EFFORT, gw_core::config::DEFAULT_THINKING_EFFORT.as_str());
        assert!(
            canonical_effort(DEFAULT_EFFORT).is_some(),
            "兜底档位本身必须是合法档位,否则出厂就会发一个上游不认的值"
        );
    }

    /// 档位表有两份:本 crate 的 `VALID_EFFORTS`(热路径要 `&'static str`,还要做逐模型夹取)
    /// 与 gw-core 的 `ThinkingEffort`(配置 schema 的边界校验)。**必须逐项相等** ——
    /// 只在一处加档位,会变成"面板能选、wire 拒收"或反过来。
    #[test]
    fn valid_efforts_matches_gw_core_enum_item_by_item() {
        let from_enum: Vec<&str> = gw_core::config::ThinkingEffort::ALL
            .iter()
            .map(|e| e.as_str())
            .collect();
        assert_eq!(
            VALID_EFFORTS, from_enum.as_slice(),
            "两份档位表漂了:VALID_EFFORTS={VALID_EFFORTS:?} vs ThinkingEffort::ALL={from_enum:?}"
        );
    }

    #[test]
    fn canonical_effort_maps_to_static_table_entries() {
        assert_eq!(canonical_effort("max"), Some("max"));
        assert_eq!(canonical_effort("MAX"), Some("max"), "大小写不敏感");
        assert_eq!(canonical_effort("XHigh"), Some("xhigh"));
        assert_eq!(canonical_effort("ultra"), None);
        assert_eq!(canonical_effort(""), None);
    }

    /// 热改入口的**校验**语义。故意只用"当前值"做成功用例 —— 真把全局改成别的值会污染
    /// 并发跑的其它用例(见 [`normalize_effort_with`] 的注释)。
    #[test]
    fn set_default_effort_validates_and_leaves_value_untouched_on_garbage() {
        let before = default_effort();

        // 合法输入:返回归一后的静态串,并落到全局(这里设成当前值,可观察状态不变)。
        assert_eq!(set_default_effort(before), Some(before));
        assert_eq!(default_effort(), before);

        // 非法输入:返回 None 且**不得**改动当前值 —— 手改 DB 塞脏档位时的兜底。
        assert_eq!(set_default_effort("ludicrous"), None);
        assert_eq!(set_default_effort(""), None);
        assert_eq!(set_default_effort("   "), None);
        assert_eq!(default_effort(), before, "非法输入不该动全局");
    }
}
