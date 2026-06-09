//! Provider trait —— 新增上游只需实现它(借鉴 ALLinOne 的"扩展一处")。
//!
//! 关键设计:**内部 IR = Anthropic Messages**(不是 OpenAI)。
//! 主链路 Claude Code(Anthropic)→ kiro-gw → 上游(Kiro / claude-subprocess /
//! claude-dario)全程 Anthropic 家族,近乎零转换,保住 thinking 签名透传与
//! cache_read 计费。见 docs/ARCHITECTURE.md §3.1。
//!
//! ## 流模型(对抗审查 H2/H3 后定稿)
//!
//! `chat()` 永远返回一个事件流 [`ChatStream`],其 Item 是 [`StreamItem`]:
//! - `Sse(SseEvent)`:转发给客户端的 Anthropic SSE 事件(线缆协议)。
//! - `Usage(ChatUsage)`:**结构化终结用量**(含 cache_read),provider 解析
//!   上游时顺手交出,gw-app 直接拿去计费,无需 re-parse SSE JSON。
//!
//! 流式 vs 非流式是 **gw-app 的关注点**:provider 一律产流,gw-app 对非流式
//! 请求折叠该流为单个 Anthropic Messages 响应(折叠逻辑只写一次)。
//!
//! ## 重试与 committed 状态(H1)
//!
//! 「首字节是否已写出给客户端」由 **gw-app 转发层**跟踪,不是 provider/错误
//! 的职责。[`UpstreamError`] 只表达"上游发生了什么";能否透明重试由 gw-app
//! 结合 committed 状态 + [`crate::error::UpstreamErrorKind`] 决定。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::account::{Account, FieldSpec};
use crate::error::UpstreamError;
use crate::model::ModelInfo;

/// 一次对话请求(Anthropic-native)。
///
/// `body` 保留客户端原始 Anthropic Messages JSON,**是唯一权威源**;provider
/// 内部按需转换为上游格式。`model` / `stream` 仅为从 body 解析出的便捷副本,
/// 出现分歧时以 `body` 为准(切勿在构造后单独改 model/stream 而不改 body)。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 对外模型 id(便捷副本,权威在 `body["model"]`)。
    pub model: String,
    /// 是否流式(便捷副本,权威在 `body["stream"]`)。
    pub stream: bool,
    /// 原始 Anthropic 请求体(权威源,provider 负责转换为上游格式)。
    pub body: serde_json::Value,
}

impl ChatRequest {
    pub fn from_anthropic_body(body: serde_json::Value) -> Self {
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
        Self {
            model,
            stream,
            body,
        }
    }
}

/// 一个 SSE 事件(Anthropic 流式协议)。
///
/// `event` 是 SSE 的事件名(如 `message_start` / `content_block_delta`),
/// `data` 是其 JSON 负载。worker 据此序列化为 `event: ...\ndata: ...\n\n`。
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 序列化为 SSE 线缆格式。
    ///
    /// 返回 `Err` 表示 `data` 无法序列化(理论上 `Value` 不会失败,但**不静默
    /// 吞错**——审查 M4:旧实现 `unwrap_or_default()` 会发出语法合法却语义损坏的
    /// `data: ` 空帧。调用方应把序列化失败当作内部错误处理)。
    pub fn to_wire(&self) -> Result<String, serde_json::Error> {
        let data = serde_json::to_string(&self.data)?;
        Ok(format!("event: {}\ndata: {}\n\n", self.event, data))
    }
}

/// 一次调用的结构化终结用量(计费一等公民,H2)。
///
/// provider 在解析上游流时产出(Kiro 的权威 usage 在末个事件;subprocess 在
/// `result` 事件;dario 在 message_delta)。gw-app 据此组装 store 的 UsageRecord,
/// 无需重新解析已转好的 Anthropic SSE JSON。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// `Provider::chat` 产出的流元素。
#[derive(Debug, Clone)]
pub enum StreamItem {
    /// 转发给客户端的 Anthropic SSE 事件。
    Sse(SseEvent),
    /// 结构化终结用量(gw-app 路由到 UsageSink,不转发客户端)。
    Usage(ChatUsage),
}

/// Provider::chat 返回的事件流。
///
/// 流正常结束 = `None`;中途上游错误 = `Err(UpstreamError)`(能否重试由
/// gw-app 结合 committed 状态判断,见模块文档 H1)。
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamItem, UpstreamError>> + Send>>;

/// 单次调用的上下文(由调度层装配,provider 只读)。
///
/// 携带本次请求选中的账号、会话与缓存亲和 key。account 已由 scheduler 在组内
/// 选好;出口 IP 由进程(worker)固定,provider 无需关心绑定细节。
/// 客户端 key / usage 归属由 gw-app 持有(provider 只产 [`ChatUsage`] token 数)。
pub struct CallCtx {
    /// 本次选中的账号。
    pub account: Arc<Account>,
    /// 会话标识(用于 conversationId 锚定)。
    pub session_id: String,
    /// 缓存亲和 key(provider 的 cache-hit 模拟据此观测/命中)。
    pub cache_key: String,
}

/// 上游 Provider 抽象。实现此 trait 即可接入一个新上游。
///
/// 线程模型:同一 `Provider` 实例被多请求共享(`Arc<dyn Provider>`),
/// 自身状态应视为不可变;per-request 状态走 [`CallCtx`]。
///
/// **注意**:设备指纹(machineId/UA)是 Kiro 特有概念,不在本 trait 内
/// (subprocess/dario 用 OAuth token + HOME 隔离,无 machineId)。需要的 provider
/// 自行以专属方法暴露。见 [`crate::model::MachineIdentity`](Kiro-only)。
#[async_trait]
pub trait Provider: Send + Sync {
    /// 家族标识,如 `"kiro"` / `"claude-subprocess"` / `"claude-dario"`。
    fn family(&self) -> &'static str;

    /// 账号字段定义:驱动 admin 前端表单 + accounts.yaml 校验。
    fn account_schema(&self) -> &'static [FieldSpec];

    /// 加载期校验一个账号字段是否完整可用(fail-fast,审查 M5)。
    ///
    /// 默认通过;provider 覆盖以在 worker 启动时拒绝缺关键字段的账号
    /// (如 Kiro 缺 refresh_token / machine_id),避免跑到首个真实请求才深处炸。
    fn validate_account(&self, _account: &Account) -> Result<(), UpstreamError> {
        Ok(())
    }

    /// 列出支持的模型(catalog / `/v1/models`)。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError>;

    /// 核心:吃 Anthropic-native 请求,吐 [`StreamItem`] 事件流。
    ///
    /// 失败时返回 `Err(UpstreamError)`;首包前的错误可被 gw-app 透明重试
    /// (committed 状态由 gw-app 跟踪)。
    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError>;

    /// 从请求派生**会话亲和键**(worker 据此把同会话钉到组内同一账号)。
    ///
    /// 默认 `None` = 无亲和(每次按负载选号)。Kiro 覆盖为派生的 conversationId
    /// (与上游 prefix cache 的会话粒度同源),保证同会话稳定命中同账号缓存。
    /// 此键应**只依赖请求内容**(无副作用),且与 provider 实际发往上游的会话标识一致。
    fn affinity_key(&self, _req: &ChatRequest) -> Option<String> {
        None
    }

    /// 刷新账号凭据(token),返回更新后的 `Account`。
    ///
    /// **回写契约(H4)**:返回的 `Account` 由 **gw-app 负责写回 store**;provider
    /// 只做刷新计算,不自行持久化。**并发契约**:同一账号同时只允许一个 in-flight
    /// refresh(gw-app 以 per-account 锁/单飞去重保证),避免两请求并发刷新互相
    /// 覆盖 token。出口 IP 已由进程固定,实现无需关心绑定。
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_parses_model_and_stream() {
        let body = serde_json::json!({
            "model": "claude-opus-4-8",
            "stream": true,
            "messages": []
        });
        let req = ChatRequest::from_anthropic_body(body);
        assert_eq!(req.model, "claude-opus-4-8");
        assert!(req.stream);
    }

    #[test]
    fn chat_request_defaults_non_stream() {
        let body = serde_json::json!({"model": "m"});
        let req = ChatRequest::from_anthropic_body(body);
        assert!(!req.stream);
    }

    #[test]
    fn sse_event_wire_format() {
        let e = SseEvent::new("message_start", serde_json::json!({"type": "message_start"}));
        let wire = e.to_wire().unwrap();
        assert!(wire.starts_with("event: message_start\n"));
        assert!(wire.contains("data: {"));
        assert!(wire.ends_with("\n\n"));
    }

    #[test]
    fn chat_usage_defaults_zero() {
        let u = ChatUsage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.cache_read_tokens, 0);
    }
}
