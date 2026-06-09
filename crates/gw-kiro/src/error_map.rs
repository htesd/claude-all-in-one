//! generateAssistantResponse 的错误分类 —— 🟢 借鉴 static_flow `kiro_dispatch.rs:1258`。
//!
//! 把上游 HTTP 状态码 + 响应体映射为 [`UpstreamErrorKind`],调度层(gw-app)据此
//! 决定换号/冷却/直接返回(见 [`gw_core::error`])。
//!
//! static_flow 的映射(我方对齐):
//! - 402 + 月度额度文案 → QuotaExhausted(换号)
//! - 429 + 日额度文案 → RateLimited(冷却换号);其余 429 → RateLimited
//! - 400 + invalid model(瞬时)→ 视为可重试(我方归 ServerError 让其重试);其余 400 → BadRequest(不换号)
//! - 401/403 → TokenInvalid(先刷新同号,失败再换)
//! - 408 / 5xx → ServerError(瞬时重试或换号)
//! - 其他 → Other

use gw_core::error::{UpstreamError, UpstreamErrorKind};

/// 月度额度耗尽的响应体特征(static_flow is_monthly_request_limit 同义)。
fn is_monthly_limit(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("monthly") && (b.contains("limit") || b.contains("quota"))
}

/// 日额度限制的响应体特征。
fn is_daily_limit(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("daily") && b.contains("limit")
}

/// 400 但属于"瞬时 invalid model"(上游偶发,换号/重试可恢复)而非请求本身非法。
fn is_transient_invalid_model(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("invalid") && b.contains("model")
}

/// 把 generateAssistantResponse 的失败响应分类。
///
/// `status` = HTTP 状态码,`body` = 响应体文本(可空)。
pub fn classify_chat_error(status: u16, body: &str) -> UpstreamError {
    let kind = match status {
        402 if is_monthly_limit(body) => UpstreamErrorKind::QuotaExhausted,
        402 => UpstreamErrorKind::QuotaExhausted, // 402 默认按额度耗尽处理
        429 if is_daily_limit(body) => UpstreamErrorKind::RateLimited,
        429 => UpstreamErrorKind::RateLimited,
        // 400:瞬时 invalid model 可重试(归 ServerError),否则请求非法(BadRequest 不换号)
        400 if is_transient_invalid_model(body) => UpstreamErrorKind::ServerError,
        400 => UpstreamErrorKind::BadRequest,
        401 | 403 => UpstreamErrorKind::TokenInvalid,
        408 => UpstreamErrorKind::ServerError,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    UpstreamError::new(kind, format!("kiro generateAssistantResponse 失败: {status} {body}"))
        .with_status(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_402_is_quota_exhausted() {
        let e = classify_chat_error(402, r#"{"message":"Monthly request limit reached"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::QuotaExhausted);
    }

    #[test]
    fn plain_429_is_rate_limited() {
        let e = classify_chat_error(429, "Too many requests");
        assert_eq!(e.kind, UpstreamErrorKind::RateLimited);
    }

    #[test]
    fn invalid_model_400_is_retryable_server_error() {
        let e = classify_chat_error(400, r#"{"reason":"INVALID_MODEL_ID"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
        // ServerError 值得换号重试(非 BadRequest)
        assert!(e.kind.worth_switching_account());
    }

    #[test]
    fn plain_400_is_bad_request_no_switch() {
        let e = classify_chat_error(400, "Improperly formed request.");
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
        assert!(!e.kind.worth_switching_account(), "请求非法换号无意义");
    }

    #[test]
    fn unauthorized_is_token_invalid() {
        assert_eq!(classify_chat_error(401, "").kind, UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_chat_error(403, "").kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn server_5xx_is_server_error() {
        assert_eq!(classify_chat_error(503, "unavailable").kind, UpstreamErrorKind::ServerError);
    }

    #[test]
    fn status_preserved() {
        assert_eq!(classify_chat_error(429, "x").status_code, Some(429));
    }
}
