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

/// 账号被上游封禁/暂停的 403 响应体特征(Kiro `TEMPORARILY_SUSPENDED` 等)。
///
/// 只命中明确的封禁词:纯 token 过期/未授权体(ExpiredToken/Unauthorized/invalid_grant)
/// **不**命中,保持「刷新同号」行为。命中则归 `TemporarilyBlocked`(冷却自愈 + 不换号),
/// 杜绝把"被封请求"换号扩散到健康号(2026-06 雪崩根因)。
///
/// ⚠️ 部署前用 139 `request_logs` 里真实被封号的 403 body 复核此标记串(见计划 §B)。
/// `pub(crate)`:token.rs 的刷新错误分类也复用它(403+suspend 刷新失败 → 临时冷却而非永久禁号)。
pub(crate) fn is_account_suspended(body: &str) -> bool {
    body.to_ascii_lowercase().contains("suspend")
}

/// 400 响应体是否为 Kiro 的 `INVALID_MODEL_ID`(该账号不支持所请求的模型:模型在其
/// 区域/订阅档未上线)。归 `ModelNotAvailable`——不惩罚账号 + 换号到有该模型的号。
fn is_invalid_model_id(body: &str) -> bool {
    body.to_ascii_lowercase().contains("invalid_model_id")
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
        // 400 + INVALID_MODEL_ID:该号不支持所请求的模型(模型在其区域/订阅档未上线,
        //   如 eu-central-1 号点 claude-sonnet-5)→ ModelNotAvailable:**不惩罚账号**
        //   (否则计失败刷禁健康号)+ 换号到有该模型的号(见 gw_core::UpstreamErrorKind)。
        400 if is_invalid_model_id(body) => UpstreamErrorKind::ModelNotAvailable,
        // 其余"invalid model"文案(无 INVALID_MODEL_ID):当瞬时故障可重试(ServerError)。
        400 if is_transient_invalid_model(body) => UpstreamErrorKind::ServerError,
        400 => UpstreamErrorKind::BadRequest,
        401 => UpstreamErrorKind::TokenInvalid,
        // 403:区分「账号被封禁/暂停」与「access_token 失效」——这是 2026-06 雪崩的关键。
        // - 封禁(TEMPORARILY_SUSPENDED 等)→ TemporarilyBlocked:冷却自愈 + **不换号**
        //   (worth_switching_account=false),否则换号把同一请求扩散到健康号 → 封全池。
        // - 其余 403(token 过期/撤销)→ TokenInvalid:刷新同号,失败再换。
        403 if is_account_suspended(body) => UpstreamErrorKind::TemporarilyBlocked,
        403 => UpstreamErrorKind::TokenInvalid,
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
    fn invalid_model_id_400_is_model_not_available_and_switches() {
        // INVALID_MODEL_ID = 该号不支持此模型 → ModelNotAvailable(不惩罚账号 + 换号)。
        let e = classify_chat_error(400, r#"{"reason":"INVALID_MODEL_ID"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
        // 换号到有该模型的号(非 BadRequest 直接返回)。
        assert!(e.kind.worth_switching_account());
    }

    #[test]
    fn transient_invalid_model_without_id_stays_server_error() {
        // 无 INVALID_MODEL_ID 的"invalid model"文案:仍当瞬时故障重试(ServerError)。
        let e = classify_chat_error(400, "the model is invalid right now");
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
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
        // 无封禁标记的 403 → TokenInvalid(刷新同号)。
        assert_eq!(classify_chat_error(403, "").kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn suspended_403_is_temporarily_blocked_not_token_invalid() {
        // 封号 403 → TemporarilyBlocked:冷却自愈 + **不换号**(杜绝雪崩扩散)。
        let e = classify_chat_error(403, r#"{"reason":"TEMPORARILY_SUSPENDED"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::TemporarilyBlocked);
        assert!(!e.kind.worth_switching_account(), "封号请求绝不能换号扩散到健康号");
        // 纯 token 失效的 403 不命中封禁标记,仍走刷新同号。
        let t = classify_chat_error(403, "Unauthorized: access token expired");
        assert_eq!(t.kind, UpstreamErrorKind::TokenInvalid);
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
