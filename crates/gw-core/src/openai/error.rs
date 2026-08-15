//! 错误形状:Anthropic → OpenAI。
//!
//! 一条纪律必须跨协议保持:**对外只发中性文案,上游原文只进日志**。
//! 与 worker 的 `sanitize_upstream_error_payload` 同源 —— 这里只换外壳,
//! 不负责挑文案,文案由调用方从 [`crate::error::UpstreamErrorKind::client_message`]
//! 取好再传进来。

use serde_json::{json, Value};

/// HTTP 状态码 → OpenAI 的 `error.type`。
///
/// 与 worker 的 `error_type_for_status` 一一对应(那边是 Anthropic 分类法)。
/// **type 必须跟着 status 走**:两者打架会让客户端 SDK 的重试判断与状态码判断冲突,
/// 这是 2026-08-07 那次事故的教训,换协议不换纪律。
pub fn openai_error_type(status: u16) -> &'static str {
    match status {
        400 | 404 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        // 503(池子暂时没号)与 529(模型级过载)在 OpenAI 分类法里没有专名,
        // `server_error` 是语义最近的,客户端 SDK 对它的处置正是退避重试。
        _ => "server_error",
    }
}

/// 构造 OpenAI 错误体 `{"error":{message,type,param,code}}`。
///
/// `param` / `code` 缺席时显式写 `null` 而不是省略键:OpenAI 官方响应总是带这四个键,
/// 有客户端(含若干 Go 侧中转)按定长结构体反序列化,少键会直接解析失败。
pub fn openai_error_body(
    status: u16,
    message: &str,
    param: Option<&str>,
    code: Option<&str>,
) -> Value {
    json!({
        "error": {
            "message": message,
            "type": openai_error_type(status),
            "param": param,
            "code": code,
        }
    })
}

/// 「上游没发完成事件就 EOF」的错误载荷(Anthropic 形状,交给
/// [`from_anthropic_error`] 换壳)。
///
/// **为什么必须是错误、不能是成功终态**:传输层 EOF 与协议层完成是**两件事**。
/// 原生 Anthropic 线缆下,上游半截断流的客户端会看到流在没有 `message_stop` 的情况下
/// 结束 —— 它**能**认出这是截断。OpenAI 线缆若在这里补一个 `finish_reason:"stop"` /
/// `response.completed`,就把一个可检测的截断变成了静默的假成功,客户端会把半截答案
/// 当完整结果用。这正是 worker 侧 `saw_message_stop` / `idle_aborted` 那套字段
/// (以及 2026-07 那次「库里记成 200 成功」)在防的东西,换协议不换纪律。
///
/// 文案是**我方的诊断**,不含任何上游报文,不存在泄漏面。
pub fn truncated_stream_payload() -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": "上游未发送完成事件即结束,响应不完整",
        }
    })
}

/// 把 **Anthropic 形状**的错误载荷改写成 OpenAI 形状。
///
/// 用在两处:非流式折叠拿到 `error` 事件、流中出现 `error` 事件。
/// 调用方应先把它交给 worker 的 `sanitize_upstream_error_payload` 中性化,
/// 本函数只做形状转换,**不做脱敏**。
pub fn from_anthropic_error(data: &Value, status: u16) -> Value {
    let message = data
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("upstream error");
    // Anthropic 的 error.type 与状态码同源(见 worker 的 error_type_for_status),
    // 状态码拿不到时用它反推,比一律 server_error 准。
    let status = match data.pointer("/error/type").and_then(Value::as_str) {
        Some("invalid_request_error") | Some("not_found_error") => 400,
        Some("authentication_error") => 401,
        Some("permission_error") => 403,
        Some("rate_limit_error") => 429,
        _ => status,
    };
    openai_error_body(status, message, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态码到类型的映射跟_worker_那套一一对应() {
        assert_eq!(openai_error_type(400), "invalid_request_error");
        assert_eq!(openai_error_type(401), "authentication_error");
        assert_eq!(openai_error_type(403), "permission_error");
        assert_eq!(openai_error_type(404), "invalid_request_error");
        assert_eq!(openai_error_type(413), "invalid_request_error");
        assert_eq!(openai_error_type(429), "rate_limit_error");
        assert_eq!(openai_error_type(502), "server_error");
        assert_eq!(openai_error_type(503), "server_error");
        assert_eq!(openai_error_type(529), "server_error");
    }

    #[test]
    fn 四个键一个都不能少() {
        let b = openai_error_body(400, "bad", Some("model"), None);
        let e = b.get("error").unwrap().as_object().unwrap();
        // 按定长结构体反序列化的客户端会因为少键直接解析失败。
        for k in ["message", "type", "param", "code"] {
            assert!(e.contains_key(k), "缺键 {k}");
        }
        assert_eq!(e["param"], json!("model"));
        assert_eq!(e["code"], Value::Null);
    }

    #[test]
    fn anthropic_错误体按_type_反推状态码() {
        let anth = json!({"type":"error","error":{"type":"rate_limit_error","message":"慢点"}});
        let out = from_anthropic_error(&anth, 502);
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(out["error"]["message"], "慢点");
    }

    #[test]
    fn 认不出的_type_回落调用方给的状态码() {
        let anth = json!({"type":"error","error":{"type":"api_error","message":"炸了"}});
        assert_eq!(from_anthropic_error(&anth, 502)["error"]["type"], "server_error");
        assert_eq!(from_anthropic_error(&anth, 429)["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn 完全畸形的载荷也给出可解析的错误体() {
        let out = from_anthropic_error(&json!({"nope": 1}), 502);
        assert_eq!(out["error"]["type"], "server_error");
        assert_eq!(out["error"]["message"], "upstream error");
    }
}
