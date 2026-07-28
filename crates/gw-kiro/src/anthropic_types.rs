//! Anthropic API 类型定义

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// 客户端完全未带 output_config 时的默认思考强度 —— **顶格**。
///
/// 2026-07-28 隔离栈剂量反应实测(claude-opus-5,同一道证明题,只发新字段、不带旧标签,
/// 数 SSE `thinking_delta` 帧):
/// | effort | 帧数 | 均值 |
/// |---|---|---|
/// | `low`   | 123, 121        | 122  |
/// | `xhigh` | 894, 509, 529   | 644  |
/// | `max`   | 1345, 855       | 1100 |
/// 单调、无重叠 —— `additionalModelRequestFields.effort` 是**真正在起作用的旋钮**,
/// 且 `max` 比 `xhigh` 还多出约 1.7 倍思考量。
///
/// 从 `xhigh` 提到 `max` 是**合规**的加深途径:`max` 就在上游 enum 里、真客户端也发得出,
/// 不像正文里塞 `<thinking_effort>` 标签那样制造真客户端不会有的指纹。
///
/// 注意这是 caio 的**策略默认**(客户端没说话时用什么),与"客户端给了脏值时回落到该模型
/// schema 的 `default`"是两件事 —— 后者见 `thinking_policy::EffortWish::ModelDefault`。
/// 也注意档位还要过 `clamp_effort_for_model` 按模型夹一次;`max` 在所有带 schema 的模型
/// 上都存在(含没有 `xhigh` 的 4.6 系),所以这个默认值对全系模型都能原样落地。
pub const DEFAULT_EFFORT: &str = "max";

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
/// - `None` / 纯空白:未指定 → `(DEFAULT_EFFORT, false)`(默认,非告警情形);
/// - 命中 [`VALID_EFFORTS`](大小写不敏感):`(该档, false)`;
/// - 非空且不命中:`(DEFAULT_EFFORT, true)`(调用方据 bool 决定是否告警)。
///
/// 只管"是不是合法档位",**不管该模型有没有这一档** —— 后者见 `model_effort_levels`。
pub fn normalize_effort(raw: Option<&str>) -> (&'static str, bool) {
    let s = match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return (DEFAULT_EFFORT, false),
    };
    for v in VALID_EFFORTS {
        if v.eq_ignore_ascii_case(s) {
            return (*v, false);
        }
    }
    (DEFAULT_EFFORT, true)
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

    #[test]
    fn none_is_default_no_fallback_flag() {
        assert_eq!(normalize_effort(None), (DEFAULT_EFFORT, false));
    }

    #[test]
    fn blank_is_default_no_fallback_flag() {
        // 纯空白视为未指定:默认 xhigh,不算"非法回退"(不告警)。
        assert_eq!(normalize_effort(Some("")), (DEFAULT_EFFORT, false));
        assert_eq!(normalize_effort(Some("   ")), (DEFAULT_EFFORT, false));
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
        // 非空但不在白名单 → 回退 xhigh 且标记 true(调用方据此告警)。
        assert_eq!(normalize_effort(Some("ultra")), (DEFAULT_EFFORT, true));
        assert_eq!(normalize_effort(Some("999")), (DEFAULT_EFFORT, true));
        assert_eq!(normalize_effort(Some("high; drop")), (DEFAULT_EFFORT, true));
    }

    #[test]
    fn effective_effort_delegates_to_normalize() {
        let oc = OutputConfig { effort: Some("garbage".to_string()), format: None };
        assert_eq!(oc.effective_effort(), DEFAULT_EFFORT);
        let oc2 = OutputConfig { effort: Some("low".to_string()), format: None };
        assert_eq!(oc2.effective_effort(), "low");
    }
}
