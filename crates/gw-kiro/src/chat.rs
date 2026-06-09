//! KiroProvider::chat 的上游发包 + eventstream→Anthropic SSE 桥接。
//!
//! 真实金标准(test-cred-free.json 实测 generateAssistantResponse 200):
//! 响应 `application/vnd.amazon.eventstream`,frame 序列:
//! - `assistantResponseEvent` `{"content":"...","modelId":"..."}` — 文本(可多帧增量)
//! - `reasoningContentEvent` `{"text":..,"signature":..}` — Opus 原生 thinking 独立通道
//! - `tokenUsageEvent` `{"uncachedInputTokens":..,"cacheReadInputTokens":..,..}` — 精确计量
//! - `contextUsageEvent` `{"contextUsagePercentage":..}` — 上下文占比(tokenUsage 缺席时回退)
//! - `meteringEvent` `{"unit":"credit","usage":..}` — credit 计费(v53 已不用于缓存反推)
//!
//! 计费(v53 统一走 prefix 模拟器):report_total 取 tokenUsage 真值 > contextUsage 估算 >
//! 模拟器 sim_total;cache_read 上报 = 模拟器命中比例 × report_total × 倍率夹限
//! (见 [`crate::usage`]);output 逐帧累加(thinking+正文)。thinking 签名透传见
//! [`crate::signature`],inline `<thinking>` 解析见 [`crate::inline_thinking`]。

use std::sync::Arc;

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{ChatRequest, ChatStream, ChatUsage, SseEvent, StreamItem};
use serde_json::json;

use crate::converter;
use crate::error_map::classify_chat_error;
use crate::kiro_types::request::KiroRequest;
use crate::parser::decoder::EventStreamDecoder;

const DEFAULT_KIRO_VERSION: &str = "0.12.155";
const DEFAULT_OS: &str = "darwin";
const DEFAULT_NODE: &str = "20.0.0";

/// api 区域:`api_region` > `region` > us-east-1。
fn api_region(account: &Account) -> String {
    account
        .extra_str("api_region")
        .filter(|s| !s.is_empty())
        .or_else(|| account.extra_str("region").filter(|s| !s.is_empty()))
        .unwrap_or("us-east-1")
        .to_string()
}

/// 发起一次 generateAssistantResponse,返回 Anthropic SSE 事件流。
///
/// - `client`:worker 的 egress client(固定出口 IP)。
/// - `machine_id`:本账号设备指纹(由 KiroProvider::machine_identity 派生)。
pub async fn chat_stream(
    client: reqwest::Client,
    account: Arc<Account>,
    machine_id: String,
    req: ChatRequest,
    cache_billing: crate::CacheBilling,
) -> Result<ChatStream, UpstreamError> {
    // 1. 解析 Anthropic body → 强类型 MessagesRequest
    let mut messages_req: crate::anthropic_types::MessagesRequest =
        serde_json::from_value(req.body.clone())
            .map_err(|e| UpstreamError::bad_request(format!("Anthropic 请求体解析失败: {e}")))?;

    // 1.5 块2a:按模型名覆写 thinking 配置(Opus 全系默认开 adaptive 保智力;
    // 非 Opus -thinking 后缀 enabled;结构化输出与 thinking 互斥)。🔵 kiro.rs 稳定经验。
    crate::thinking_policy::override_thinking_from_model_name(&mut messages_req);

    // 2. converter:Anthropic → Kiro ConversationState
    let conversion = converter::convert_request(&messages_req).map_err(|e| {
        // UnsupportedModel / EmptyMessages 都是请求本身问题 → BadRequest(不换号)
        UpstreamError::bad_request(format!("转换失败: {e}"))
    })?;

    // 2.5 Prefix 缓存命中模拟(v53:billing 唯一路径)。🔵 对齐 kiro.rs handlers.rs:1558——
    // 在把 conversation_state move 进 KiroRequest 前算一次。
    //
    // session_key 必须按**账号**隔离(审查 Architect#1):kiro.rs 是单进程单账号池,
    // 仅用 conversationId 即可;kiro-gw 一个 worker 固定一组多账号,真实 Kiro prefix cache
    // 是 per-account 后端会话隔离的。若两账号撞同一派生 conversationId(同 system+前2条 user),
    // 仅用 convId 作键会让 A 账号的前缀污染 B 账号的命中估算 → 误报缓存折扣/串号计费。
    // 故 key = account_id + '\x1f' + conversationId,与上游缓存粒度对齐。
    let sim_cache: (i32, i32) = {
        let cs = &conversion.conversation_state;
        let session_key = format!("{}\x1f{}", account.account_id, cs.conversation_id);
        let fps = crate::cache_sim::fingerprints_from_state(cs);
        let sim = crate::cache_sim::observe(&session_key, &req.model, fps);
        (sim.cache_read_tokens as i32, sim.total_tokens as i32)
    };

    // 3. 组装顶层 KiroRequest(注入 profileArn)
    let profile_arn = account.extra_str("profile_arn").map(|s| s.to_string());
    let kiro_req = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: profile_arn.clone(),
    };
    let body = serde_json::to_string(&kiro_req)
        .map_err(|e| UpstreamError::new(UpstreamErrorKind::Other, format!("序列化 KiroRequest 失败: {e}")))?;

    // 4. access_token(P1:要求账号已持有有效 token;刷新/重试由 scheduler 层负责)
    let access_token = account
        .extra_str("access_token")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(UpstreamErrorKind::TokenInvalid, "账号缺少 access_token")
        })?
        .to_string();

    // 5. 发包(金标准请求头)
    let region = api_region(&account);
    let url = format!("https://q.{region}.amazonaws.com/generateAssistantResponse");
    let version = account
        .extra_str("kiro_version")
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_KIRO_VERSION);
    let x_amz_ua = format!("aws-sdk-js/1.0.34 KiroIDE-{version}-{machine_id}");
    let ua = format!(
        "aws-sdk-js/1.0.34 ua/2.1 os/{DEFAULT_OS} lang/js md/nodejs#{DEFAULT_NODE} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{version}-{machine_id}"
    );

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-amzn-codewhisperer-optout", "true")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amz-user-agent", &x_amz_ua)
        .header("user-agent", &ua)
        .header("host", format!("q.{region}.amazonaws.com"))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("Authorization", format!("Bearer {access_token}"))
        .body(body)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("generateAssistantResponse 请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Err(classify_chat_error(status.as_u16(), &body_text));
    }

    // 6. 流式读响应字节 → eventstream 解码 → Anthropic SSE StreamItem
    let model = req.model.clone();
    // thinking 是否启用(经 override_thinking 覆写后的有效值):决定 inline `<thinking>` 解析激活。
    let thinking_enabled = messages_req
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);
    let byte_stream = resp.bytes_stream();
    Ok(Box::pin(eventstream_to_anthropic(
        byte_stream,
        model,
        thinking_enabled,
        sim_cache,
        cache_billing,
    )))
}

/// 把上游字节流(AWS eventstream)桥接成 Anthropic SSE StreamItem 流。
///
/// 时序:message_start → [thinking 块(reasoningContentEvent / inline `<thinking>`)→
/// signature_delta → stop] → text 块 → stop → message_delta → message_stop + Usage。
/// 🔵 reasoning/签名/inline 状态机逻辑搬自 kiro.rs(已上线稳定)stream.rs。
///
/// `sim_cache = (hit_tokens, sim_total)`:prefix 缓存模拟器输出,收尾据此 + report_total
/// 算上报 cache_read(v53 计费,见 [`crate::usage::reported_cache_read`])。
/// `cache_billing`:multiplier/cap/floor 参数(从 system.yaml 注入)。
fn eventstream_to_anthropic(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    thinking_enabled: bool,
    sim_cache: (i32, i32),
    cache_billing: crate::CacheBilling,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    async_stream_like(byte_stream, model, thinking_enabled, sim_cache, cache_billing)
}

/// 流状态机:管理块索引与 thinking/text 块的惰性开闭 + reasoning 签名。
///
/// Anthropic 时序铁律:thinking 块必须在 text 块**之前**且独立闭合,thinking 块 stop 前
/// 必发一个 signature_delta。块按出现顺序惰性开启(不预先固定 index 0=text),
/// 故 reasoning 先到时拿 index 0、text 拿 index 1;无 reasoning 时 text 拿 index 0。
struct BlockTracker {
    /// 下一个可用 content_block 索引。
    next_index: usize,
    /// 当前 thinking(reasoning)块是否开着。
    reasoning_active: bool,
    /// thinking 块索引(开着时有效)。
    thinking_index: Option<usize>,
    /// text 块索引(已开则有效;text 一旦开就保持到收尾)。
    text_index: Option<usize>,
    /// 是否见过原生 reasoning(见过后正文绕过任何 inline 解析,直接 text_delta)。
    native_reasoning_seen: bool,
    /// 关闭 thinking 块时透传的签名(已重写 f6→官方名;上游无签名则 None)。
    reasoning_signature: Option<String>,
    /// 客户端请求的官方模型名(签名重写/合成用)。
    model: String,
    /// 累积的 thinking 文本(上游无签名时用于合成签名;native 与 inline 共用)。
    thinking_text: String,
    /// 是否启用 thinking(决定 inline `<thinking>` 解析是否激活)。
    thinking_enabled: bool,
    /// inline `<thinking>` 正文解析器(非 Opus 模型 thinking 走正文标签)。
    inline: crate::inline_thinking::InlineThinkingParser,
    /// 累积 output token 估算(thinking + 正文,逐帧累加)。🔵 对齐 kiro.rs estimate_tokens
    /// 在 process_assistant_response / process_reasoning_content 处累加原始内容串。
    output_tokens: i64,
}

impl BlockTracker {
    fn new(model: String, thinking_enabled: bool) -> Self {
        Self {
            next_index: 0,
            reasoning_active: false,
            thinking_index: None,
            text_index: None,
            native_reasoning_seen: false,
            reasoning_signature: None,
            model,
            thinking_text: String::new(),
            thinking_enabled,
            inline: crate::inline_thinking::InlineThinkingParser::new(),
            output_tokens: 0,
        }
    }

    /// 处理 reasoningContentEvent:捕获签名 + 逐片发 thinking_delta。
    fn on_reasoning(&mut self, text: &str, signature: Option<&str>) -> Vec<SseEvent> {
        // 1) 先捕获签名(上游在 thinking 流最后一帧单独下发 {"signature":...},无 text)。
        //    把暴露 Bedrock 渠道的模型代号(claude-quince,f2.f1.f6)换成官方名,保留加密体;
        //    重写失败则原样透传。必须在 text.is_empty() 早返回前处理。
        if let Some(sig) = signature {
            if !sig.is_empty() {
                let fixed = crate::signature::rewrite_model_in_signature(sig, &self.model)
                    .unwrap_or_else(|| sig.to_string());
                self.reasoning_signature = Some(fixed);
            }
        }
        if text.is_empty() {
            return Vec::new();
        }
        // 2) 顺序保护:text 块已开且无活跃 reasoning → reasoning 迟到,丢弃(避免非法块顺序)。
        if self.text_index.is_some() && !self.reasoning_active {
            tracing::warn!("reasoningContentEvent 迟于正文到达,已丢弃以避免非法块顺序");
            return Vec::new();
        }
        self.native_reasoning_seen = true;
        self.thinking_text.push_str(text);
        // output 计 thinking(对齐 kiro.rs:reasoning 文本也累加进 output_tokens)。
        self.output_tokens += crate::text_tokens::estimate_output_tokens(text);

        let mut events = Vec::new();
        // 3) 开 thinking 块(首片段或重开)+ 发 delta。
        events.extend(self.open_thinking_block_if_needed());
        events.push(self.thinking_delta(text));
        events
    }

    /// 开 thinking 块(若未开),返回 content_block_start(若发生)。共用于 native/inline 路径。
    fn open_thinking_block_if_needed(&mut self) -> Vec<SseEvent> {
        if self.reasoning_active {
            return Vec::new();
        }
        let idx = self.next_index;
        self.next_index += 1;
        self.thinking_index = Some(idx);
        self.reasoning_active = true;
        vec![SseEvent::new(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"thinking","thinking":""}}),
        )]
    }

    /// 发一个 thinking_delta(块须已开)。
    fn thinking_delta(&self, text: &str) -> SseEvent {
        let idx = self.thinking_index.unwrap_or(0);
        SseEvent::new(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,
                   "delta":{"type":"thinking_delta","thinking":text}}),
        )
    }

    /// 关闭开着的 thinking 块(若有):发 signature_delta + content_block_stop。
    /// 时序铁律:stopped 块不能再 delta,故 signature 必须在 stop 前。
    /// native 与 inline 路径共用:有真签名(已重写)用之,否则按累积 thinking 文本合成。
    fn close_reasoning_if_open(&mut self) -> Vec<SseEvent> {
        if !self.reasoning_active {
            return Vec::new();
        }
        let mut events = Vec::new();
        if let Some(idx) = self.thinking_index {
            let signature = match &self.reasoning_signature {
                Some(s) => s.clone(),
                None => crate::signature::synthesize_signature(&self.model, &self.thinking_text),
            };
            events.push(SseEvent::new(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,
                       "delta":{"type":"signature_delta","signature":signature}}),
            ));
            events.push(SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            ));
        }
        self.reasoning_active = false;
        self.thinking_index = None;
        self.reasoning_signature = None;
        events
    }

    /// 开 text 块(若未开)+ 发 text_delta。
    fn push_text(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.text_index.is_none() {
            let idx = self.next_index;
            self.next_index += 1;
            self.text_index = Some(idx);
            events.push(SseEvent::new(
                "content_block_start",
                json!({"type":"content_block_start","index":idx,
                       "content_block":{"type":"text","text":""}}),
            ));
        }
        let idx = self.text_index.unwrap();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,
                   "delta":{"type":"text_delta","text":text}}),
        ));
        events
    }

    /// 处理 assistantResponseEvent 正文。
    /// - 已见 native reasoning:正文是纯文本,直接发(先关 reasoning 块)。
    /// - thinking 启用但无 native:正文里可能含 inline `<thinking>` 标签,过 inline 解析器。
    /// - thinking 未启用:纯文本直发。
    fn on_text(&mut self, text: &str) -> Vec<SseEvent> {
        // output 计正文(对齐 kiro.rs process_assistant_response:在分流/标签解析前按
        // 原始 content 串累加,thinking 标签也计入 output——与 estimate_tokens 同点位)。
        if !text.is_empty() {
            self.output_tokens += crate::text_tokens::estimate_output_tokens(text);
        }
        // native 优先:见过原生 reasoning 后正文绕过 inline 解析。
        if self.native_reasoning_seen || !self.thinking_enabled {
            let mut events = self.close_reasoning_if_open();
            if !text.is_empty() {
                events.extend(self.push_text(text));
            }
            return events;
        }
        // inline 路径:喂解析器,翻译分段指令。
        let segments = self.inline.feed(text);
        self.apply_inline(segments)
    }

    /// 把 inline 解析器的分段指令翻译成 SSE 事件,复用块开闭/签名逻辑。
    fn apply_inline(&mut self, segments: Vec<crate::inline_thinking::InlineEvent>) -> Vec<SseEvent> {
        use crate::inline_thinking::InlineEvent;
        let mut events = Vec::new();
        for seg in segments {
            match seg {
                InlineEvent::Text(t) => events.extend(self.push_text(&t)),
                InlineEvent::ThinkingStart => events.extend(self.open_thinking_block_if_needed()),
                InlineEvent::ThinkingDelta(t) => {
                    self.thinking_text.push_str(&t);
                    events.extend(self.open_thinking_block_if_needed());
                    events.push(self.thinking_delta(&t));
                }
                InlineEvent::ThinkingEnd => events.extend(self.close_reasoning_if_open()),
            }
        }
        events
    }

    /// 收尾:冲洗 inline 残留,再关掉任何开着的块(reasoning 优先闭合,再关 text)。
    fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.thinking_enabled && !self.native_reasoning_seen {
            let tail = self.inline.finish();
            events.extend(self.apply_inline(tail));
        }
        events.extend(self.close_reasoning_if_open());
        if let Some(idx) = self.text_index.take() {
            events.push(SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            ));
        }
        events
    }
}

/// 用 async-stream 风格手写状态机(不引入 async-stream crate:用 channel + task)。
fn async_stream_like(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    thinking_enabled: bool,
    sim_cache: (i32, i32),
    cache_billing: crate::CacheBilling,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);

    tokio::spawn(async move {
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        // 计费状态(v53)。提前取出 sim 结果:sim_total 也作 message_start 的 input_tokens
        // 上估(对齐 kiro.rs:message_start 先给本地估算,message_delta 末尾再发权威拆分)。
        let (sim_hit, sim_total) = sim_cache;
        // --- message_start ---
        // input_tokens 用 sim_total(本轮上下文总 token 估算);output 先占 1(对齐 kiro.rs)。
        // 权威计费拆分(uncached/cache_read/cache_creation)在收尾的 message_delta 下发,
        // NewAPI 以 message_delta 为准——故此处仅作首包 UI 估算,不影响计费。
        let start = SseEvent::new(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": msg_id, "type": "message", "role": "assistant",
                    "model": model, "content": [],
                    "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": sim_total.max(0), "output_tokens": 1}
                }
            }),
        );
        if tx.send(Ok(StreamItem::Sse(start))).await.is_err() {
            return;
        }

        let mut tracker = BlockTracker::new(model.clone(), thinking_enabled);
        let mut decoder = EventStreamDecoder::new();
        let mut stop_reason = "end_turn".to_string();

        // report_total:计费基准 token。优先 tokenUsageEvent 真值(uncached+cacheRead),
        // 退而 contextUsageEvent(pct×窗口),再退模拟器 sim_total。
        let mut report_total: Option<i32> = None;
        let window = crate::converter::get_context_window_size(&model);
        futures::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx
                        .send(Err(UpstreamError::network(format!("读取上游流失败: {e}"))))
                        .await;
                    return;
                }
            };
            if decoder.feed(&bytes).is_err() {
                let _ = tx
                    .send(Err(UpstreamError::new(
                        UpstreamErrorKind::Other,
                        "eventstream 缓冲溢出",
                    )))
                    .await;
                return;
            }
            // 解出所有完整 frame
            loop {
                match decoder.decode() {
                    Ok(Some(frame)) => {
                        let evt = frame.event_type().map(|s| s.to_string());
                        match evt.as_deref() {
                            Some("assistantResponseEvent") => {
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    if let Some(text) = v.get("content").and_then(|c| c.as_str()) {
                                        for ev in tracker.on_text(text) {
                                            if tx.send(Ok(StreamItem::Sse(ev))).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            Some("reasoningContentEvent") => {
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                    let sig = v.get("signature").and_then(|s| s.as_str());
                                    for ev in tracker.on_reasoning(text, sig) {
                                        if tx.send(Ok(StreamItem::Sse(ev))).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            Some("tokenUsageEvent") => {
                                // Kiro 精确 token 计量(含 thinking)。🔵 对齐 kiro.rs stream.rs:822——
                                // report_total 真值 = uncached+cacheRead,覆盖 contextUsage 推算。
                                // cacheWrite 不取:v53 统一模型不单列 cache_creation(见收尾),取了
                                // 反而会被 build_usage_json 从 uncached 再减一遍 → 过度计费。
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    let uncached = v
                                        .get("uncachedInputTokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0);
                                    let cr = v
                                        .get("cacheReadInputTokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0);
                                    let total_input = uncached + cr;
                                    if total_input > 0 {
                                        report_total = Some(total_input.min(i32::MAX as i64) as i32);
                                    }
                                }
                            }
                            Some("contextUsageEvent") => {
                                // 据上下文占比推算 input token(tokenUsage 缺席时的回退基准)。
                                // 🔵 对齐 kiro.rs stream.rs:803——pct×窗口;100% 触发上下文超限。
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    let pct = v
                                        .get("contextUsagePercentage")
                                        .and_then(|x| x.as_f64())
                                        .unwrap_or(0.0);
                                    let est = (pct * window as f64 / 100.0) as i32;
                                    // 仅在 tokenUsage 尚未给出真值时用估算填充(真值优先)。
                                    if report_total.is_none() && est > 0 {
                                        report_total = Some(est);
                                    }
                                    if pct >= 100.0 {
                                        stop_reason = "model_context_window_exceeded".into();
                                    }
                                }
                            }
                            Some("meteringEvent") => {
                                // v53 已不靠 metering 反推缓存;此处仅吞掉(留兼容)。
                            }
                            // 上游异常 frame(Exception/Error)→ 报错并终止。
                            // 不再补发 message_delta/message_stop/Usage:避免「error 后又正常
                            // 收尾计费」的自相矛盾序列(🔵 对齐 kiro.rs stream.rs:1394 注释)。
                            Some(other) if other.contains("xception") || other.contains("rror") => {
                                let _ = tx
                                    .send(Err(UpstreamError::new(
                                        UpstreamErrorKind::ServerError,
                                        format!("上游异常事件 {other}: {}", frame.payload_as_str()),
                                    )))
                                    .await;
                                return;
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => break, // 需要更多字节
                    Err(_) => {
                        // 单帧坏包:decoder 内部已尝试恢复,继续
                        break;
                    }
                }
            }
        }

        // --- 收尾:关掉任何开着的块(thinking/text) ---
        for ev in tracker.finish() {
            let _ = tx.send(Ok(StreamItem::Sse(ev))).await;
        }

        // --- 计费收尾(v53 统一走模拟器,见 usage.rs)---
        let output_tokens = tracker.output_tokens.min(i32::MAX as i64) as i32;
        // report_total 优先级:tokenUsage 真值 > contextUsage 估算 > 模拟器 sim_total 兜底。
        let final_input_tokens = report_total.unwrap_or(sim_total).max(0);
        // 零输出保护:本轮无产出则不计缓存(用户没拿到东西不该为缓存付费)。
        let zero_output = output_tokens <= 0;
        let cache_read = if zero_output {
            0
        } else {
            crate::usage::reported_cache_read(
                final_input_tokens,
                sim_hit,
                sim_total,
                cache_billing.read_multiplier,
                cache_billing.cap_ratio,
                cache_billing.floor_ratio,
            )
        };
        // cache_creation:v53 统一模型不单列(对齐 kiro.rs finalize 恒置 0)。
        let cache_creation = 0;

        let usage_json = crate::usage::build_usage_json(
            final_input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
        );
        let _ = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                json!({"type":"message_delta",
                       "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                       "usage": usage_json}),
            ))))
            .await;
        let _ = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_stop",
                json!({"type":"message_stop"}),
            ))))
            .await;
        // 结构化 usage(gw-app 路由到 UsageSink 入库):input_tokens 取总上下文(库里记总量,
        // 与 SSE 的 uncached 拆分口径不同——SSE 给客户端看拆分,库记总账)。
        let _ = tx
            .send(Ok(StreamItem::Usage(ChatUsage {
                input_tokens: final_input_tokens.max(0) as u64,
                output_tokens: output_tokens.max(0) as u64,
                cache_read_tokens: cache_read.max(0) as u64,
                cache_creation_tokens: cache_creation.max(0) as u64,
            })))
            .await;
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // 收集事件的 (event名, delta类型 或 block类型) 便于断言时序。
    fn tag(ev: &SseEvent) -> String {
        let d = &ev.data;
        if let Some(t) = d.get("delta").and_then(|x| x.get("type")).and_then(|x| x.as_str()) {
            format!("{}:{}", ev.event, t)
        } else if let Some(t) = d
            .get("content_block")
            .and_then(|x| x.get("type"))
            .and_then(|x| x.as_str())
        {
            format!("{}:{}", ev.event, t)
        } else {
            ev.event.clone()
        }
    }

    fn tags(evs: &[SseEvent]) -> Vec<String> {
        evs.iter().map(tag).collect()
    }

    #[test]
    fn reasoning_then_text_orders_thinking_before_text() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_reasoning("思考片段", None));
        all.extend(t.on_text("正文")); // 应先关 thinking(带 signature_delta+stop)再开 text
        all.extend(t.finish());
        let seq = tags(&all);
        assert_eq!(
            seq,
            vec![
                "content_block_start:thinking",
                "content_block_delta:thinking_delta",
                "content_block_delta:signature_delta",
                "content_block_stop",
                "content_block_start:text",
                "content_block_delta:text_delta",
                "content_block_stop",
            ],
            "thinking 块必须在 text 块前并独立闭合(带 signature_delta)"
        );
    }

    #[test]
    fn thinking_index_zero_text_index_one() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let r = t.on_reasoning("x", None);
        // 首个 content_block_start 的 index=0 且为 thinking
        assert_eq!(r[0].data["index"], 0);
        assert_eq!(r[0].data["content_block"]["type"], "thinking");
        let tx = t.on_text("y");
        // text 块拿 index 1(close_reasoning 的事件在前)
        let start = tx.iter().find(|e| e.event == "content_block_start").unwrap();
        assert_eq!(start.data["index"], 1);
        assert_eq!(start.data["content_block"]["type"], "text");
    }

    #[test]
    fn text_only_uses_index_zero() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let evs = t.on_text("hello");
        assert_eq!(evs[0].data["index"], 0);
        assert_eq!(evs[0].data["content_block"]["type"], "text");
    }

    #[test]
    fn real_signature_rewritten_not_synthesized() {
        // 上游给真签名(claude-quince)→ 关闭块时应是重写后的(含官方名,无 quince)。
        const REAL_SIG: &str = "Ev4BCmMIDhABGAIqQDLCxOcAxIGpEWzaBVN/7Rhnn7KPNqmlN3pQgWXeogdRhOlKAvxTylSWauMzkhf1NcylYW38yAUC463X+Bvj1YMyDWNsYXVkZS1xdWluY2U4AEIIdGhpbmtpbmcSDJZPrLrFRh2MFQgTIRoMLunMMbV2gAt9AB3FIjAfpHy8DkJKmF8LaQs9OEJhpMGgRwQvd6qHoPV5Rz2jXdeuhTBoQnCIMS44GqTamasqSZscuKHM930rQ31rcriqFj3AzLv8RnxlyFiu/fdDdt9YiFKtO38Cy4iqw35ZEKQr9J0/Mkru/S451tutqRClvGDgnIrJ2N0D3dcYAQ==";
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_reasoning("think", Some(REAL_SIG));
        let close = t.close_reasoning_if_open();
        let sig_ev = close
            .iter()
            .find(|e| tag(e) == "content_block_delta:signature_delta")
            .expect("应有 signature_delta");
        let sig = sig_ev.data["delta"]["signature"].as_str().unwrap();
        let raw = base64::engine::general_purpose::STANDARD.decode(sig).unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("claude-opus-4-8"), "签名应含官方模型名");
        assert!(!s.contains("claude-quince"), "签名不应残留 claude-quince");
    }

    #[test]
    fn no_signature_synthesizes_one() {
        // 上游无签名 → 合成一个(含官方名)。
        let mut t = BlockTracker::new("claude-opus-4-6".to_string(), false);
        t.on_reasoning("reasoning text here", None);
        let close = t.close_reasoning_if_open();
        let sig_ev = close
            .iter()
            .find(|e| tag(e) == "content_block_delta:signature_delta")
            .expect("应有 signature_delta");
        let sig = sig_ev.data["delta"]["signature"].as_str().unwrap();
        assert!(!sig.is_empty(), "无上游签名时应合成非空签名");
        let raw = base64::engine::general_purpose::STANDARD.decode(sig).unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("claude-opus-4-6"));
    }

    #[test]
    fn late_reasoning_after_text_is_dropped() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_text("正文先到");
        let late = t.on_reasoning("迟到的推理", None);
        assert!(late.is_empty(), "text 已开后迟到的 reasoning 应被丢弃");
    }

    #[test]
    fn output_tokens_accumulate_text_and_reasoning() {
        // output 应同时计 thinking(reasoning)与正文,逐帧累加。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let before = t.output_tokens;
        assert_eq!(before, 0);
        t.on_reasoning("some reasoning text", None);
        let after_reasoning = t.output_tokens;
        assert!(after_reasoning > 0, "reasoning 文本应计入 output");
        t.on_text("the actual answer");
        assert!(
            t.output_tokens > after_reasoning,
            "正文应在 reasoning 基础上继续累加 output"
        );
    }

    #[test]
    fn empty_text_does_not_add_output() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_text("");
        assert_eq!(t.output_tokens, 0, "空正文不应增加 output");
    }
}
