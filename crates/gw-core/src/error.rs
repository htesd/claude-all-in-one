//! 上游错误分类
//!
//! 调度层(gw-app)看 [`UpstreamErrorKind`] + 自己跟踪的 committed 状态
//! (首字节是否已写出客户端)决定动作:换号 / 冷却 / 直接返回。
//! 借鉴 ALLinOne `errors.py` 与 static_flow kiro_dispatch 的状态码映射。
//!
//! **审查 H1**:能否透明重试 = "错误类型可换号" AND "尚未向客户端写出字节"。
//! 后半句是 gw-app 转发层的运行时事实,**不**编码进错误对象(旧设计的
//! `retryable_pre_stream` bool 在流开始后就是个谎言)。错误只描述"上游怎么了"。

use std::fmt;

/// 上游错误种类 —— 决定调度动作的唯一依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    /// 凭据失效 / 被撤销(401/403 刷新后仍失败,或 invalid_grant)。
    /// 动作:标记账号 invalid,切号。
    TokenInvalid,
    /// 限流(429,或 402 月度额度)。动作:冷却该号一段时间,切号。
    RateLimited,
    /// 临时封禁 / 上游主动拒绝服务。动作:较长冷却,切号。
    TemporarilyBlocked,
    /// 配额耗尽(日/月)。动作:标记该号配额耗尽,切号。
    QuotaExhausted,
    /// 网络错误(连接失败/超时/重置)。动作:瞬时重试或切号。
    Network,
    /// 上游 5xx。动作:瞬时重试或切号。
    ServerError,
    /// 请求本身非法(400 Improperly formed / schema 错误)。
    /// 动作:**不换号**(换号也一样错),直接返回客户端。
    BadRequest,
    /// 该账号不支持所请求的模型(400 `INVALID_MODEL_ID`):模型在该号的区域/订阅档
    /// 未上线(如 eu-central-1 号点 claude-sonnet-5)。**非账号健康问题**。
    /// 动作:**不惩罚账号**(不计失败/不禁用)+ 换号到有该模型的号重试;调度层还会记
    /// `(账号,模型)` 不可用(见 scheduler `mark_model_unavailable`),后续选号直接跳过该号,
    /// 避免亲和反复选中同一不支持的号死循环。
    ModelNotAvailable,
    /// 上游 200 空流(Kiro 首包截断等)。动作:见 empty-fallback 策略。
    EmptyResponse,
    /// 其他未分类。动作:保守切号一次。
    Other,
}

impl UpstreamErrorKind {
    /// 该错误是否意味着"换个账号可能成功"——调度层据此决定是否换号重试。
    ///
    /// 返回 `false` 的三类**绝不换号**(否则把同一请求扩散到健康号 → 雪崩封号,
    /// 2026-06 大面积封号根因):
    /// - `BadRequest`:请求本身非法,换号一样错。
    /// - `EmptyResponse`:上游对该**内容**的确定性空流(疑 guardrail);换号救不回且放大
    ///   成多条 error 招封号(实战已证,见 caio-empty-response-not-fixable)。
    /// - `TemporarilyBlocked`:账号被上游封禁/暂停;封禁号自身冷却自愈即可,把同一(被封
    ///   内容/高频)请求喂给健康号正是雪崩根因。
    ///
    /// 其余(RateLimited/QuotaExhausted/TokenInvalid/ServerError/Network/Other)仍可换号,
    /// 但受 `messages()` 的 `max_switch_attempts` 硬上限约束(默认 2,不再走遍全组)。
    pub fn worth_switching_account(&self) -> bool {
        !matches!(
            self,
            UpstreamErrorKind::BadRequest
                | UpstreamErrorKind::EmptyResponse
                | UpstreamErrorKind::TemporarilyBlocked
        )
    }

    /// 该错误是否应让账号进入冷却。
    pub fn should_cooldown(&self) -> bool {
        matches!(
            self,
            UpstreamErrorKind::RateLimited
                | UpstreamErrorKind::TemporarilyBlocked
                | UpstreamErrorKind::QuotaExhausted
        )
    }
}

impl fmt::Display for UpstreamErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            UpstreamErrorKind::TokenInvalid => "token_invalid",
            UpstreamErrorKind::RateLimited => "rate_limited",
            UpstreamErrorKind::TemporarilyBlocked => "temporarily_blocked",
            UpstreamErrorKind::QuotaExhausted => "quota_exhausted",
            UpstreamErrorKind::Network => "network",
            UpstreamErrorKind::ServerError => "server_error",
            UpstreamErrorKind::BadRequest => "bad_request",
            UpstreamErrorKind::ModelNotAvailable => "model_not_available",
            UpstreamErrorKind::EmptyResponse => "empty_response",
            UpstreamErrorKind::Other => "other",
        };
        f.write_str(s)
    }
}

/// 上游错误。
///
/// 只描述"上游发生了什么"。能否透明重试由 gw-app 结合 committed 状态判断
/// (见模块文档 H1),故此处**不**含 retryable 标志。
#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    /// 上游 HTTP 状态码(若有)。
    pub status_code: Option<u16>,
}

impl UpstreamError {
    pub fn new(kind: UpstreamErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status_code: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status_code = Some(status);
        self
    }

    /// 便捷构造:网络错误。
    pub fn network(message: impl Into<String>) -> Self {
        Self::new(UpstreamErrorKind::Network, message)
    }

    /// 便捷构造:非法请求(不换号)。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(UpstreamErrorKind::BadRequest, message)
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status_code {
            Some(code) => write!(f, "[{}|{}] {}", self.kind, code, self.message),
            None => write!(f, "[{}] {}", self.kind, self.message),
        }
    }
}

impl std::error::Error for UpstreamError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_not_worth_switching() {
        assert!(!UpstreamErrorKind::BadRequest.worth_switching_account());
        assert!(UpstreamErrorKind::RateLimited.worth_switching_account());
    }

    #[test]
    fn content_and_ban_kinds_not_worth_switching() {
        // 内容/封禁确定性 → 绝不换号(2026-06 雪崩防护)。
        assert!(!UpstreamErrorKind::EmptyResponse.worth_switching_account());
        assert!(!UpstreamErrorKind::TemporarilyBlocked.worth_switching_account());
        // 这些换个号可能成功 → 仍可换号(但受 max_switch_attempts 硬上限约束)。
        assert!(UpstreamErrorKind::TokenInvalid.worth_switching_account());
        assert!(UpstreamErrorKind::QuotaExhausted.worth_switching_account());
        assert!(UpstreamErrorKind::ServerError.worth_switching_account());
        assert!(UpstreamErrorKind::Network.worth_switching_account());
        assert!(UpstreamErrorKind::Other.worth_switching_account());
        // ModelNotAvailable 换号到有该模型的号(非 BadRequest 直接返回)。
        assert!(UpstreamErrorKind::ModelNotAvailable.worth_switching_account());
    }

    #[test]
    fn cooldown_kinds() {
        assert!(UpstreamErrorKind::QuotaExhausted.should_cooldown());
        assert!(!UpstreamErrorKind::Network.should_cooldown());
    }

    #[test]
    fn display_includes_status() {
        let e = UpstreamError::new(UpstreamErrorKind::ServerError, "boom").with_status(503);
        assert_eq!(e.to_string(), "[server_error|503] boom");
    }
}
