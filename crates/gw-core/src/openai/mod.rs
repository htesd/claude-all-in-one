//! OpenAI 线缆适配 —— **只在边界**,不污染主链路。
//!
//! `docs/ARCHITECTURE.md:194` 定的规矩:内部 IR 永远是 Anthropic Messages;
//! 要接 OpenAI 客户端就「在边界做适配」。这个模块就是那道边界。
//!
//! ```text
//!  OpenAI 请求 ──[chat_req / resp_req]──▶ Anthropic body ──▶ 既有全链路 ──▶ provider
//!  OpenAI 线缆 ◀──[chat_out / resp_out]── Anthropic SSE  ◀──────────────────┘
//! ```
//!
//! **为什么放 gw-core**:与 [`crate::fold`] 同一性质 —— 只依赖标准线缆协议、
//! 与任何 provider 无关,写一次给所有上层用。这里没有 I/O,没有 axum。
//!
//! **谁在用**:目前只有 cursor 家族的 worker 挂这两条入口。kiro / dario /
//! claude-subprocess 走原生 Anthropic,一个字节都不经过这里。

pub mod chat_out;
pub mod chat_req;
pub mod error;
pub mod inbound;
pub mod resp_out;
pub mod resp_req;
pub mod usage;

use serde_json::Value;
use sha2::{Digest, Sha256};

pub use error::{openai_error_body, openai_error_type};
pub use inbound::ConvertError;

/// 本次响应要说哪种线缆。
///
/// [`Wire::Anthropic`] 是默认值,也是**除 cursor 之外全部流量**走的那条 ——
/// 它必须与引入本模块之前逐字节相同,任何形状漂移都是对生产主链路的回归。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wire {
    /// 原生 Anthropic Messages(`/v1/messages`)。
    #[default]
    Anthropic,
    /// OpenAI ChatCompletions(`/v1/chat/completions`)。
    ///
    /// `include_usage` 来自客户端的 `stream_options.include_usage`:只有它为真时,
    /// 流末尾才补那条 `choices:[]` 的用量帧。不问自发会噎住严格按 `choices[0]`
    /// 解析的客户端。
    OpenAiChat { include_usage: bool },
    /// OpenAI Responses(`/v1/responses`)。
    OpenAiResponses,
}

impl Wire {
    /// 是不是 OpenAI 系线缆(错误形状、保活帧、终止帧都按这个分叉)。
    pub fn is_openai(self) -> bool {
        !matches!(self, Wire::Anthropic)
    }
}

/// 一条待下发的 SSE 帧(线缆中立:gw-core 不认识 axum)。
///
/// `data` 是**已经序列化好的文本**而不是 [`Value`],这样 `data: [DONE]` 这种
/// 非 JSON 的终止帧走同一条路,调用方不需要为它开特例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFrame {
    /// SSE 的 `event:` 名。`None` = 不写 event 行 —— ChatCompletions 就不写,
    /// 写了反而会让部分只认 `data:` 的解析器把帧丢掉。
    pub event: Option<String>,
    /// `data:` 行的内容。
    pub data: String,
}

impl WireFrame {
    /// 只有 data 行的帧(ChatCompletions 全部如此)。
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// 带 event 名的帧(Responses 全部如此)。
    pub fn named(event: impl Into<String>, data: &Value) -> Self {
        Self {
            event: Some(event.into()),
            data: data.to_string(),
        }
    }
}

/// 入站转换的产物:Anthropic body + 出站该用的线缆。
#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    /// 转换后的 Anthropic Messages 请求体(交给既有链路的唯一权威源)。
    pub body: Value,
    /// 出站转换用哪种线缆(由入站格式决定,不由客户端选)。
    pub wire: Wire,
    /// 被丢掉的托管工具类型(见 [`inbound::ToolSet::dropped`])。
    ///
    /// 调用方**应该打一条日志**:客户端声明了我方没有的能力、我方照常回答,
    /// 这种降级在响应里几乎看不见,不留痕就等于永远查不到。
    pub dropped_tools: Vec<String>,
}

/// 从 **OpenAI 形状**的请求体派生一个稳定的会话提示,供 router 做 session→worker 亲和。
///
/// ⚠️ 这**不是** provider 的账号亲和键。账号亲和由 worker 在转换**之后**用
/// [`crate::provider::Provider::affinity_key`] 从 Anthropic body 派生,与这里无关。
/// 这里只解决一件事:router 的 [`Value`] 里没有 Anthropic 的 `metadata.user_id`,
/// 取不到会话就只能按负载轮,多开一个同组 worker 时同一会话会在 worker 之间弹,
/// 把下游的账号亲和整个打散。
///
/// 锚 = **系统提示 + 第一条 user 文本**。
///
/// ⚠️ **只取第一条**,这里与 kiro 的「前两条 user」故意不同。kiro 那个键要跟上游的
/// conversationId 对齐,所以照抄了它的口径 —— 代价是**第一轮到第二轮之间会变一次**
/// (第一轮只有一条 user)。而这里唯一的用途是让同一会话稳定落同一个 worker,
/// 从第一轮起就稳定才有意义:第二轮改选 worker,正是缓存最该命中的那一刻把它打散
/// (对抗评审 Architect#7)。取第一条 user 则 N 轮恒定。
pub fn session_hint(body: &Value) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut users = 0usize;
    let mut any = false;

    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        hasher.update(s.as_bytes());
        hasher.update([0]);
        any = true;
    }

    // ChatCompletions 用 `messages`,Responses 用 `input`;两者的条目都带 `role`。
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array);
    let Some(items) = items else {
        // `input` 是裸字符串的最简形态。
        let s = body.get("input").and_then(Value::as_str)?;
        hasher.update(s.as_bytes());
        return Some(hex16(hasher.finalize().as_slice()));
    };

    for it in items {
        let role = it.get("role").and_then(Value::as_str).unwrap_or("");
        let text = collect_text(it.get("content"));
        if text.is_empty() {
            continue;
        }
        match role {
            "system" | "developer" => {
                hasher.update(text.as_bytes());
                hasher.update([0]);
                any = true;
            }
            "user" if users < 1 => {
                users += 1;
                hasher.update(text.as_bytes());
                hasher.update([0]);
                any = true;
            }
            _ => {}
        }
        if users >= 1 {
            break;
        }
    }

    any.then(|| hex16(hasher.finalize().as_slice()))
}

/// 把 content(字符串或分片数组)里的**文本**拼起来,忽略图片等非文本分片。
fn collect_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 同一会话追加轮次后提示不变() {
        let round1 = json!({"messages":[
            {"role":"system","content":"S"},
            {"role":"user","content":"Q1"},
        ]});
        let round2 = json!({"messages":[
            {"role":"system","content":"S"},
            {"role":"user","content":"Q1"},
            {"role":"assistant","content":"A1"},
            {"role":"user","content":"Q2"},
        ]});
        let round3 = json!({"messages":[
            {"role":"system","content":"S"},
            {"role":"user","content":"Q1"},
            {"role":"assistant","content":"A1"},
            {"role":"user","content":"Q2"},
            {"role":"assistant","content":"A2"},
            {"role":"user","content":"Q3"},
        ]});
        let a = session_hint(&round1).unwrap();
        let b = session_hint(&round2).unwrap();
        let c = session_hint(&round3).unwrap();
        // **从第一轮起**就必须恒定。第二轮改选 worker 正是缓存最该命中的那一刻把它打散,
        // 所以这里只锚第一条 user(与 kiro 的前两条口径故意不同)。
        assert_eq!(a, b, "第二轮就变 = 亲和形同虚设");
        assert_eq!(b, c);
    }

    #[test]
    fn 不同会话提示不同() {
        let x = json!({"messages":[{"role":"user","content":"A"}]});
        let y = json!({"messages":[{"role":"user","content":"B"}]});
        assert_ne!(session_hint(&x), session_hint(&y));
    }

    #[test]
    fn responses_的_input_两种形态都能派生() {
        let s = json!({"model":"m","input":"hello"});
        let arr = json!({"model":"m","input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]});
        assert!(session_hint(&s).is_some());
        assert!(session_hint(&arr).is_some());
        // instructions 参与锚定 —— 换系统提示词就是另一个会话。
        let with_inst = json!({"model":"m","instructions":"I","input":"hello"});
        assert_ne!(session_hint(&s), session_hint(&with_inst));
    }

    #[test]
    fn 没有可锚定内容时不硬造一个提示() {
        assert_eq!(session_hint(&json!({"model":"m"})), None);
        assert_eq!(session_hint(&json!({"model":"m","messages":[]})), None);
    }

    #[test]
    fn 默认线缆是_anthropic() {
        assert_eq!(Wire::default(), Wire::Anthropic);
        assert!(!Wire::Anthropic.is_openai());
        assert!(Wire::OpenAiResponses.is_openai());
        assert!(Wire::OpenAiChat { include_usage: false }.is_openai());
    }
}
