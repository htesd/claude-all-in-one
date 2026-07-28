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
    /// **模型级**过载(上游明确告知容量不足,如 Kiro 5xx + `MODEL_TEMPORARILY_UNAVAILABLE`)。
    /// **非账号健康问题**——上游是在说"这个模型现在没容量",与用哪个号无关。
    ///
    /// 动作:**不惩罚账号**(不计失败/不禁用)+ **不换号**(容量是模型级的,换号打的还是同一个
    /// 模型端点,纯粹扩散错误、白烧另一个号的配额、还丢掉会话 cache 亲和)+ 由调用方做
    /// **同号退避重试**(见 `messages()` 的 `overload_backoff`)。对外映射 529 `overloaded_error`。
    ///
    /// 与 `ServerError` 拆开的理由(2026-07-25 opus-5 实测):上游容量抖动被记进账号连续失败
    /// 计数,35 秒内把 7 个健康号禁光并触发全灭自愈;而换号重试对它毫无用处——177 次上游
    /// 500 只救回 19 次(19%)。
    Overloaded,
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
    /// 返回 `false` 的四类**绝不换号**(否则把同一请求扩散到健康号 → 雪崩封号,
    /// 2026-06 大面积封号根因):
    /// - `BadRequest`:请求本身非法,换号一样错。
    /// - `EmptyResponse`:上游对该**内容**的确定性空流(疑 guardrail);换号救不回且放大
    ///   成多条 error 招封号(实战已证,见 caio-empty-response-not-fixable)。
    /// - `TemporarilyBlocked`:账号被上游封禁/暂停;封禁号自身冷却自愈即可,把同一(被封
    ///   内容/高频)请求喂给健康号正是雪崩根因。
    /// - `Overloaded`:模型级容量不足,与账号无关。换号打的还是同一个模型端点,纯扩散;
    ///   有效手段是**同号退避重试**(见 [`Self::worth_same_account_backoff`])。
    ///
    /// 其余(RateLimited/QuotaExhausted/TokenInvalid/ServerError/Network/Other)仍可换号,
    /// 但受 `messages()` 的 `max_switch_attempts` 硬上限约束(默认 2,不再走遍全组)。
    pub fn worth_switching_account(&self) -> bool {
        !matches!(
            self,
            UpstreamErrorKind::BadRequest
                | UpstreamErrorKind::EmptyResponse
                | UpstreamErrorKind::TemporarilyBlocked
                | UpstreamErrorKind::Overloaded
        )
    }

    /// 该错误是否值得**在同一个号上**退避后重试(而非换号)。
    ///
    /// 只有 `Overloaded`:上游容量抖动是秒级自愈的,等一下再打同一个号即可,而且
    /// 保住会话 cache 亲和(实测一次请求 `cache_read` 可达 10.7 万 token,换号全部重算)。
    ///
    /// **爆炸半径 = 1 个号**,比换号重试(默认 2 个号)更小,故与 2026-06 防雪崩的
    /// `max_switch_attempts` 硬上限**不冲突**,无需放开后者。
    pub fn worth_same_account_backoff(&self) -> bool {
        matches!(self, UpstreamErrorKind::Overloaded)
    }

    /// 该错误是否**不应记入账号健康**(不计连续失败、不触发 `TooManyFailures` 禁用)。
    ///
    /// - `BadRequest`:请求本身非法,不是账号的错。
    /// - `ModelNotAvailable`:该号只是没上线这个模型,它仍能服务其它模型。
    /// - `Overloaded`:上游模型没容量,与账号无关。**2026-07-25 opus-5 事故正是漏了这条**——
    ///   上游 5xx 记进 `failure_count`,35 秒内禁光 7 个健康号(禁用对**所有模型**生效,
    ///   连带 opus-4-6/sonnet-5 一起挂)。
    pub fn spares_account_health(&self) -> bool {
        matches!(
            self,
            UpstreamErrorKind::BadRequest
                | UpstreamErrorKind::ModelNotAvailable
                | UpstreamErrorKind::Overloaded
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

    /// **对外**中性文案 —— 按类别给客户端一句可操作的话,不含任何上游厂商 / 接口名 /
    /// 原始报文 / 账号线索。
    ///
    /// 为什么要有这层:上游报错里带着强指纹(接口名 `generateAssistantResponse`、
    /// reason 码 `USER_REQUEST_RATE_EXCEEDED` 等),原样透传等于把渠道来源告诉客户,
    /// 定价与渠道选择就没得谈了。诊断信息一条不少地留在 [`UpstreamError::message`],
    /// 只进日志。
    ///
    /// 措辞刻意只区分**客户能做什么**(重试 / 改请求 / 找管理员),不泄露我方内部状态。
    pub fn client_message(&self) -> &'static str {
        match self {
            // 客户改请求即可解。
            UpstreamErrorKind::BadRequest => "请求无效,请检查请求体后重试",
            UpstreamErrorKind::ModelNotAvailable => "当前模型不可用,请更换模型后重试",
            // 稍后重试可能恢复。
            UpstreamErrorKind::RateLimited => "请求过于频繁,请稍后重试",
            UpstreamErrorKind::Overloaded => "服务繁忙,请稍后重试",
            UpstreamErrorKind::Network | UpstreamErrorKind::ServerError => {
                "服务暂时不可用,请稍后重试"
            }
            UpstreamErrorKind::TemporarilyBlocked => "服务暂时不可用,请稍后重试",
            UpstreamErrorKind::EmptyResponse => "服务未返回内容,请重试",
            // 要人介入。
            UpstreamErrorKind::QuotaExhausted => "服务额度已用尽,请联系管理员",
            UpstreamErrorKind::TokenInvalid => "服务鉴权异常,请联系管理员",
            UpstreamErrorKind::Other => "服务异常,请稍后重试",
        }
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
            UpstreamErrorKind::Overloaded => "overloaded",
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
    /// **内部**诊断文本:含上游原始报文、接口名、账号线索。只进日志与 admin,
    /// **绝不**直接发给客户端 —— 对外一律走 [`Self::client_message`]。
    pub message: String,
    /// 上游 HTTP 状态码(若有)。
    pub status_code: Option<u16>,
    /// 允许对外展示的详情。`None` = 用 [`UpstreamErrorKind::client_message`] 的中性兜底。
    ///
    /// **私有,且只有 [`Self::bad_request_visible`] 一个入口。** 对抗评审三个镜头一致指出:
    /// 只要留一个「把任意 String 登记为对外文案」的公开 API,迟早有人顺手把上游响应体
    /// 传进去,fail-closed 就退化成靠自觉。收窄到单一构造器后,任何新的对外展示需求都得
    /// 显式改这里 —— 改动可见,才评审得动。
    client_detail: Option<String>,
}

impl UpstreamError {
    pub fn new(kind: UpstreamErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status_code: None,
            client_detail: None,
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

    /// 便捷构造:非法请求(不换号)。文案仅进日志;要对外可见用
    /// [`Self::bad_request_visible`]。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(UpstreamErrorKind::BadRequest, message)
    }

    /// 便捷构造:**我方本地**判定的非法请求,同一句话既进日志也发客户端。
    ///
    /// **这是把文案送到客户端的唯一入口。** 只用于「客户改请求就能解」且文案由我方完全
    /// 掌控的场景(体积超限、报文解析失败、模型不支持……)。凡是引用了上游响应体、
    /// 上游接口名、账号标识的,一律用 [`Self::bad_request`] —— 那条路只进日志。
    pub fn bad_request_visible(message: impl Into<String>) -> Self {
        let m = message.into();
        let mut e = Self::bad_request(m.clone());
        e.client_detail = Some(m);
        e
    }

    /// 发给客户端的消息 —— 唯一对外出口。
    ///
    /// 没登记 `client_detail` 就退回按类别的中性文案:宁可少说,不可说漏。
    pub fn client_message(&self) -> String {
        self.client_detail
            .clone()
            .unwrap_or_else(|| self.kind.client_message().to_string())
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

    /// 对外文案 **fail-closed**:没显式登记就退回中性文案,内部诊断一个字都不外泄。
    #[test]
    fn client_message_defaults_to_neutral_and_hides_internal_detail() {
        let e = UpstreamError::new(
            UpstreamErrorKind::ServerError,
            r#"kiro generateAssistantResponse 失败: 503 {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#,
        )
        .with_status(503);
        let out = e.client_message();
        assert_eq!(out, "服务暂时不可用,请稍后重试");
        // 反向断言:内部 message 必须原样保留,否则日志排查就瞎了。
        assert!(e.message.contains("generateAssistantResponse"), "内部诊断不该被抹掉");
    }

    /// 登记过的本地文案照发 —— 客户能据此自己改请求,这类信息不该被中性文案吃掉。
    #[test]
    fn registered_client_detail_is_passed_through() {
        let e = UpstreamError::bad_request_visible("请求体 12 字节超出上限 10 字节");
        assert_eq!(e.client_message(), "请求体 12 字节超出上限 10 字节");
        assert_eq!(e.message, e.client_message(), "本地文案两边同源");
        // 反向:同 kind 但没登记的,仍是中性文案(证明不是按 kind 放行的)。
        let hidden = UpstreamError::bad_request("Kiro 账号 'foo@bar' 缺少凭据");
        assert_eq!(hidden.client_message(), "请求无效,请检查请求体后重试");
        assert!(!hidden.client_message().contains("foo@bar"), "账号标识绝不能外泄");
    }

    /// 全类别扫一遍:任何 kind 的中性文案都不许带厂商 / 接口指纹。
    #[test]
    fn no_kind_leaks_vendor_fingerprint() {
        use UpstreamErrorKind as K;
        const KINDS: [K; 11] = [
            K::TokenInvalid,
            K::RateLimited,
            K::TemporarilyBlocked,
            K::QuotaExhausted,
            K::Network,
            K::ServerError,
            K::Overloaded,
            K::BadRequest,
            K::ModelNotAvailable,
            K::EmptyResponse,
            K::Other,
        ];
        for k in KINDS {
            let m = k.client_message().to_ascii_lowercase();
            for bad in ["kiro", "codewhisperer", "amazon", "aws", "generateassistantresponse"] {
                assert!(!m.contains(bad), "{k} 的对外文案泄露了 `{bad}`: {m}");
            }
            assert!(!k.client_message().is_empty(), "{k} 缺对外文案");
        }
    }
}
