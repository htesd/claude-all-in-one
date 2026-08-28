//! KiroProvider::chat 的上游发包 + eventstream→Anthropic SSE 桥接。
//!
//! 真实金标准(test-cred-free.json 实测 generateAssistantResponse 200):
//! 响应 `application/vnd.amazon.eventstream`,frame 序列:
//! - `assistantResponseEvent` `{"content":"...","modelId":"..."}` — 文本(可多帧增量)
//! - `reasoningContentEvent` — Opus 原生 thinking 独立通道。payload 是 Smithy
//!   `ReasoningContent` **union**:常见一支 `{"text":..,"signature":..}`,另一支带
//!   `redactedContent`(安全遮蔽的加密推理;拆包 1.0.369 客户端确实从响应读
//!   `reasoningRedactedContent`)。**我方目前只实现前一支下行**,后一支仅抓样存证
//!   (见 [`crate::redacted_probe`]),其 signature 被显式丢弃以免污染归属。
//! - `tokenUsageEvent` `{"uncachedInputTokens":..,"cacheReadInputTokens":..,..}` — 精确计量
//! - `contextUsageEvent` `{"contextUsagePercentage":..}` — 上下文占比(tokenUsage 缺席时回退)
//! - `meteringEvent` `{"unit":"credit","usage":..}` — credit 计费(v53 已不用于缓存反推)
//!
//! 计费(v53 统一走 prefix 模拟器):report_total 取 tokenUsage 真值 > contextUsage 估算 >
//! 模拟器 sim_total;cache_read 上报 = 模拟器命中比例 × report_total × 倍率夹限
//! (见 [`crate::usage`]);output 逐帧累加(thinking+正文)。thinking 签名透传见
//! [`crate::signature`],inline `<thinking>` 解析见 [`crate::inline_thinking`]。

use std::collections::HashMap;
use std::collections::HashSet;
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

/// 出站体积护栏:序列化体积超 `max_body_bytes` 时,从 history 剔最老媒体瘦身后重序列化;
/// 仍超限或无媒体可剔则 `Err(BadRequest)`(不发上游、不惩罚账号)。返回最终请求体字符串。
///
/// 🔵 对齐 kiro.rs v63 `enforce_upstream_body_limit`。无 headroom:caio 序列化的 body 即
/// 最终出站字节(profileArn 已在序列化前注入,发包路径只加 header 不改 body)。
fn enforce_body_limit(
    kiro_req: &mut KiroRequest,
    body: String,
    max_body_bytes: usize,
) -> Result<String, UpstreamError> {
    if body.len() <= max_body_bytes {
        return Ok(body);
    }
    let need = body.len() - max_body_bytes;
    let shed = converter::shed_history_media(&mut kiro_req.conversation_state.history, need);
    if shed.dropped_documents + shed.dropped_images == 0 {
        // 没有可剔的历史媒体(超大的是文本/工具/当前消息),无法自动修复
        // 文案由我方生成、只含字节数,对外可见(客户据此自己减附件)。
        return Err(UpstreamError::bad_request_visible(format!(
            "请求体 {} 字节超出体积上限 {} 字节,且无历史媒体可剔除以瘦身;请减少附件或对话历史",
            body.len(),
            max_body_bytes
        )));
    }
    let reshrunk = serde_json::to_string(kiro_req).map_err(|e| {
        UpstreamError::new(UpstreamErrorKind::Other, format!("体积护栏重序列化失败: {e}"))
    })?;
    if reshrunk.len() > max_body_bytes {
        return Err(UpstreamError::bad_request_visible(format!(
            "请求体剔除历史媒体后仍 {} 字节,超出体积上限 {} 字节;请减少当前消息附件",
            reshrunk.len(),
            max_body_bytes
        )));
    }
    Ok(reshrunk)
}

/// api 区域:`api_region` > `region` > us-east-1。
fn api_region(account: &Account) -> String {
    account
        .extra_str("api_region")
        .filter(|s| !s.is_empty())
        .or_else(|| account.extra_str("region").filter(|s| !s.is_empty()))
        .unwrap_or("us-east-1")
        .to_string()
}

/// 渲染"发往 Kiro 前"的完整请求体 JSON,仅用于请求日志(调试)展示。
///
/// 纯函数:复刻 [`chat_stream`] 的转换 + thinking 覆写 + profileArn 注入,但**不跑 cache_sim、
/// 不做体积护栏、不发送上游**——无副作用,可在落库的 detach 异步任务里安全调用,绝不碰热路径。
/// 任何阶段失败都返回带说明的占位串(绝不 panic、不阻断日志落库)。
pub fn render_kiro_payload(req: &ChatRequest, account: &Account) -> String {
    let mut messages_req: crate::anthropic_types::MessagesRequest =
        match serde_json::from_value(req.body.clone()) {
            Ok(m) => m,
            Err(e) => return format!("<解析 Anthropic 请求体失败: {e}>"),
        };
    crate::thinking_policy::override_thinking_from_model_name(&mut messages_req);
    // 上游 conversationId 按**账号**加盐:换号必须换 ID,否则同一个 conversationId
    // 会横跨一串 AWS 账号(号 ~50min 就死,而客户端会话跑几小时)——那是账号池最强的
    // 特征之一。调度亲和键不受影响(它走 scope="" 的口径)。
    // 盐必须与**真正发出去**的那次一致(见 `chat_stream`),否则落库的 payload 里的
    // conversationId 与上游实际收到的不是同一个,排查时对不上。machineId 用同一个
    // 按账号的派生口径(`generate_from_account`,与 provider 的 machine_identity 同源)。
    let scope = account_scope(
        &account.account_id,
        &crate::machine_id::generate_from_account(account),
    );
    let conversion = match converter::convert_request(&messages_req, &scope) {
        Ok(c) => c,
        Err(e) => return format!("<转换失败: {e}>"),
    };
    // legacy 线缆形态:思考强度走旧文本标签(结构化字段不发,发了就是时代错位的混搭),
    // body 顶层 agentMode 省略(0.12.155 时代没有该字段)。见 wire_profile。
    let legacy = crate::wire_profile::legacy_wire();
    let kiro_req = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: crate::headers::resolve_profile_arn(account),
        additional_model_request_fields: if legacy {
            None
        } else {
            crate::thinking_policy::additional_model_request_fields(&messages_req)
        },
        agent_mode: if legacy {
            None
        } else {
            Some(crate::kiro_types::request::DEFAULT_AGENT_MODE.to_string())
        },
    };
    serde_json::to_string_pretty(&kiro_req).unwrap_or_else(|e| format!("<序列化失败: {e}>"))
}

/// 发起一次 generateAssistantResponse,返回 Anthropic SSE 事件流。
///
/// - `client`:worker 的 egress client(固定出口 IP)。
/// - `machine_id`:本账号设备指纹(由 KiroProvider::machine_identity 派生)。
/// 账号作用域盐。**空串在 `convert_request` 里的语义是「不加盐」,所以绝不能把一个
/// 可能为空的值直接传进去。**
///
/// `account_id` 是 accounts 表的主键,正常不该为空;但 SQLite 允许空串主键,而遗留/
/// 导入异常/数据损坏的号真有可能带着 `account_id=""` 进来。那时两个后果都是静默的:
/// 消息路径退回无盐 ID,而 **metadata 路径会把客户端的 session UUID 原文发给上游** ——
/// 恰好是这次改动要消除的那条最强关联特征,却没有任何日志或报错暴露它。
///
/// 所以这里退回 `machine_id`:它同样是**按账号**的(显式配置或按凭据派生),且保证非空。
/// 换号一样换盐,伪装不破;同时吵一声,让这个本不该存在的号被发现。
fn account_scope(account_id: &str, machine_id: &str) -> String {
    if !account_id.is_empty() {
        return account_id.to_string();
    }
    tracing::warn!(
        "kiro 账号的 account_id 为空(数据异常),conversationId 的账号盐退回 machineId。\
         请检查该号的入库来源 —— 空主键的号不该存在"
    );
    machine_id.to_string()
}

/// 剥离全历史 assistant 消息的 `reasoningContent`(`THINKING_SIGNATURE_INVALID` 兜底,
/// 对齐官方客户端 Z4:验签失败 → 剥 reasoning 重试一次)。返回是否有实际剥离
/// (没有则重试无意义,直接走原错误路径)。
fn strip_reasoning_from_history(req: &mut KiroRequest) -> bool {
    let mut stripped = false;
    for msg in &mut req.conversation_state.history {
        if let crate::kiro_types::conversation::Message::Assistant(a) = msg {
            if a.assistant_response_message.reasoning_content.is_some() {
                a.assistant_response_message.reasoning_content = None;
                stripped = true;
            }
        }
    }
    stripped
}

pub async fn chat_stream(
    client: reqwest::Client,
    account: Arc<Account>,
    machine_id: String,
    req: ChatRequest,
    cache_billing: crate::CacheBilling,
    max_body_bytes: usize,
) -> Result<ChatStream, UpstreamError> {
    // 1. 解析 Anthropic body → 强类型 MessagesRequest
    let mut messages_req: crate::anthropic_types::MessagesRequest =
        serde_json::from_value(req.body.clone())
            // serde 报的是 **Anthropic 请求体自身**的字段路径,不涉上游身份 → 对外可见。
            .map_err(|e| {
                UpstreamError::bad_request_visible(format!("Anthropic 请求体解析失败: {e}"))
            })?;

    // 1.5 块2a:按模型名覆写 thinking 配置(Opus 全系默认开 adaptive 保智力;
    // 非 Opus -thinking 后缀 enabled;结构化输出与 thinking 互斥)。🔵 kiro.rs 稳定经验。
    crate::thinking_policy::override_thinking_from_model_name(&mut messages_req);

    // 2. converter:Anthropic → Kiro ConversationState
    let conversion = converter::convert_request(
        &messages_req,
        &account_scope(&account.account_id, &machine_id),
    )
    .map_err(|e| {
        // UnsupportedModel / EmptyMessages 都是请求本身问题 → BadRequest(不换号)。
        // 文案只有「模型不支持: <客户请求的模型名>」/「消息列表为空」→ 对外可见。
        UpstreamError::bad_request_visible(format!("转换失败: {e}"))
    })?;
    // 工具防御性修复字段表(随流式状态机走到收尾解包双编码参数)。在 conversation_state
    // 被移入 KiroRequest 前先取出。
    let tool_repair_fields = conversion.tool_repair_fields;

    // 3. 组装顶层 KiroRequest(注入 profileArn:显式值 > 按 idp 固定兜底,对齐 static_flow)
    let profile_arn = crate::headers::resolve_profile_arn(&account);
    // 思考强度走 1.0.212 的结构化字段(旧的正文文本标签见 converter::history 的开关)。
    // legacy 线缆形态:结构化字段与 body 顶层 agentMode 都不发(见 wire_profile)。
    let legacy = crate::wire_profile::legacy_wire();
    let amrf = if legacy {
        None
    } else {
        let amrf = crate::thinking_policy::additional_model_request_fields(&messages_req);
        if let Some(v) = &amrf {
            tracing::debug!(model = %messages_req.model, fields = %v, "additionalModelRequestFields");
        }
        amrf
    };
    let mut kiro_req = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: profile_arn.clone(),
        additional_model_request_fields: amrf,
        agent_mode: if legacy {
            None
        } else {
            Some(crate::kiro_types::request::DEFAULT_AGENT_MODE.to_string())
        },
    };
    let mut body = serde_json::to_string(&kiro_req)
        .map_err(|e| UpstreamError::new(UpstreamErrorKind::Other, format!("序列化 KiroRequest 失败: {e}")))?;

    // 3.5 出站体积护栏(🔵 kiro.rs v63):超上游报文上限 → 先从 history 剔最老媒体瘦身,
    // 仍超限则本地 BadRequest(不发上游、不惩罚账号)。整个会话从"超限即每轮必 400 毒化"
    // 变为"自动瘦身续命"。剔除会就地改 kiro_req.conversation_state.history,故后续 cache_sim
    // 观测到的是真正发出的 state(见 3.6)。
    body = enforce_body_limit(&mut kiro_req, body, max_body_bytes)?;

    // 3.6 Prefix 缓存命中模拟(v53:billing 唯一路径)。🔵 对齐 kiro.rs。
    // **必须在体积护栏之后**观测(审查 Architect#2):护栏剔除历史媒体后,这里观测的是
    // **真正发给上游的** conversation_state,否则会按未剔除口径高估 input/cache_read tokens
    // 落库计费,且把从未发出的前缀写进模拟缓存状态污染后续轮。
    //
    // session_key 必须按**账号**隔离:claude-all-in-one 一个 worker 固定一组多账号,真实 Kiro
    // prefix cache 是 per-account 后端会话隔离的。若两账号撞同一派生 conversationId(同 system+
    // 前2条 user),仅用 convId 作键会让 A 账号前缀污染 B 账号命中估算 → 误报折扣/串号计费。
    // 故 key = account_id + '\x1f' + conversationId,与上游缓存粒度对齐。
    let mut sim_cache: (i32, i32) = {
        let cs = &kiro_req.conversation_state;
        let session_key = format!("{}\x1f{}", account.account_id, cs.conversation_id);
        let fps = crate::cache_sim::fingerprints_from_state(cs);
        let sim = crate::cache_sim::observe(&session_key, &req.model, fps);
        (sim.cache_read_tokens as i32, sim.total_tokens as i32)
    };

    // 3.7 毒报文备忘录(🔵 kiro.rs v63):同字节级 payload 近期已在**多个不同账号**上被上游
    // 确定性 400 过 → 本地 BadRequest 拦截,不再打上游。兜住无视 400 仍重试的客户端。
    //
    // 「多账号」是 2026-08-02 修的:单账号 400 不判毒,否则一个坏号(如 profileArn 缺失的
    // krs-52,对任何请求都回 "Improperly formed")会把它碰到的每一份 body 全局毒化 600s。
    // 文案直接透传上游原文 —— 旧文案臆测"减少附件或历史",而实测失败请求平均 91 条消息、
    // 成功请求 242 条,体积与此无关,那句提示只会把人引向错误方向。
    let poison_fp = crate::poison_memo::fingerprint(&body);
    if let Some(upstream_msg) = crate::poison_memo::poisoned_reason(&poison_fp) {
        return Err(UpstreamError::bad_request_visible(format!(
            "该请求体已在多个账号上被上游确定性拒绝(HTTP 400),重试相同请求不会成功。上游原文: {upstream_msg}"
        )));
    }

    // 4. bearer token(apikey→kiro_api_key,social/IdC→access_token;刷新/重试由 scheduler 层负责)
    let access_token = crate::headers::bearer_token(&account)
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "账号缺少凭据(access_token / kiro_api_key)",
            )
        })?
        .to_string();

    // 5. 发包(金标准请求头,逐字对齐 static_flow —— 见 [`crate::headers`])。
    //    端点 runtime.{region}.kiro.dev(env 可覆盖);UA/Accept/条件头集中在 headers 模块。
    let region = api_region(&account);
    let base_url = crate::headers::runtime_base_url(&region);
    let url = format!("{base_url}/generateAssistantResponse");
    let version = crate::headers::kiro_version(&account);

    let rb = crate::headers::apply_streaming_headers(
        client.post(&url),
        &account,
        &base_url,
        &access_token,
        &machine_id,
        &version,
    );
    let resp = rb
        .body(body)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("generateAssistantResponse 请求失败: {e}")))?;

    let status = resp.status();
    let mut resp = resp;
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        // 验签失败兜底(2026-08-19,对齐官方客户端 Z4):历史里带去的 reasoningContent
        // 签名过期/不被上游接受时,剥掉全历史 reasoningContent 原样重试一次 ——
        // 请求本体没坏,只是推理存证失效,不应让整个会话失败。
        if status.as_u16() == 400
            && body_text.contains("THINKING_SIGNATURE_INVALID")
            && strip_reasoning_from_history(&mut kiro_req)
        {
            tracing::warn!(
                "上游 THINKING_SIGNATURE_INVALID,已剥离全历史 reasoningContent 重试一次"
            );
            let stripped_body = serde_json::to_string(&kiro_req).map_err(|e| {
                UpstreamError::new(
                    UpstreamErrorKind::Other,
                    format!("序列化 KiroRequest(剥离 reasoning 重试)失败: {e}"),
                )
            })?;
            let rb2 = crate::headers::apply_streaming_headers(
                client.post(&url),
                &account,
                &base_url,
                &access_token,
                &machine_id,
                &version,
            );
            let resp2 = rb2.body(stripped_body).send().await.map_err(|e| {
                UpstreamError::network(format!("generateAssistantResponse 重试请求失败: {e}"))
            })?;
            let status2 = resp2.status();
            if status2.is_success() {
                // codex 审查:首次观测提交的是**带 reasoning** 的指纹,而实际被上游接受的
                // 是剥离后的报文 —— 用剥离后的 state 重新观测一次,让本轮计费与缓存状态
                // 都反映真实发出的字节(observe 同 key 覆盖旧状态,且与上游真实缓存的
                // 前缀形态一致:上游记住的也是剥离版)。
                let cs = &kiro_req.conversation_state;
                let session_key = format!("{}\x1f{}", account.account_id, cs.conversation_id);
                let fps = crate::cache_sim::fingerprints_from_state(cs);
                let sim = crate::cache_sim::observe(&session_key, &req.model, fps);
                sim_cache = (sim.cache_read_tokens as i32, sim.total_tokens as i32);
                resp = resp2;
            } else {
                let body2 = resp2.text().await.unwrap_or_default();
                return Err(classify_chat_error(status2.as_u16(), &body2));
            }
        } else {
            let err = classify_chat_error(status.as_u16(), &body_text);
            // 只记**报文格式/体积类**的确定性 400("Improperly formed request")。
            //
            // ⚠️ 短语门**不足以**区分「报文坏」与「账号坏」:2026-08-02 实测,`profileArn` 缺失的
            // 账号(krs-52)对任何请求都回同样的 "Improperly formed request." —— 账号问题穿着
            // 报文问题的外衣。所以这里只做**记录**,真正判毒由 `poison_memo` 按「同一 body 在
            // ≥2 个不同账号上都失败」裁决;单账号失败仅留痕,交给账号生命周期处理。
            if status.as_u16() == 400
                && err.kind == UpstreamErrorKind::BadRequest
                && body_text.contains("Improperly formed")
            {
                crate::poison_memo::remember(poison_fp, &account.account_id, body_text.trim());
            }
            return Err(err);
        }
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
        tool_repair_fields,
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
    tool_repair_fields: HashMap<String, HashSet<String>>,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    async_stream_like(
        byte_stream,
        model,
        thinking_enabled,
        sim_cache,
        cache_billing,
        tool_repair_fields,
    )
}

/// 流状态机:管理块索引与 thinking/text 块的惰性开闭 + reasoning 签名。
///
/// Anthropic 时序铁律:thinking 块必须在 text 块**之前**且独立闭合;thinking 块 stop 前
/// **默认**发一个 signature_delta——除非 `emit_signature=false`(多上游反代关签名,见该字段),
/// 此时 thinking 块照常 start/delta/stop 但不带 signature。块按出现顺序惰性开启(不预先固定
/// index 0=text),故 reasoning 先到时拿 index 0、text 拿 index 1;无 reasoning 时 text 拿 index 0。
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
    /// 关闭 thinking 块时透传的签名(**原样透传上游字节,不改写 f6**;上游无签名则 None
    /// → 走 `synthesize_signature` 合成)。2026-08-19 起停止改写:签名要随客户端历史回传
    /// 上行验签,改写过的会吃 400 THINKING_SIGNATURE_INVALID。
    reasoning_signature: Option<String>,
    /// 客户端请求的官方模型名。**仅** `synthesize_signature` 用(上游无签名时的兜底);
    /// 真签名已不再按它改写 f6。
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
    /// 是否出现过 tool_use(stop_reason 优先级判定用)。
    has_tool_use: bool,
    /// 当前开着的 tool_use 块 (块索引, toolUseId)。块不可交错——任一时刻至多一个块开着,
    /// 开新块前必关当前块。同一 toolUseId 的增量多帧匹配此项,只 start 一次。
    open_tool: Option<(usize, String)>,
    /// 已 stop 的 toolUseId 集合:用于忽略 stop 后迟到/重复帧(防向已关闭块再发 delta/stop)。
    stopped_tools: HashSet<String>,
    /// 是否开过 thinking 块(native 或 inline;thinking-only 兜底与空响应判定用)。
    thinking_opened: bool,
    /// 是否开过 text 块(text_index 会被 tool_use 关闭重置,故单独记"曾开过")。
    text_opened: bool,
    /// finish() 判定为 thinking-only:整流只有 thinking 块 → 收尾强制 stop_reason=max_tokens。
    thinking_only_max_tokens: bool,
    /// 工具防御性修复字段表（短名 → array/object 字段集）。空 = 不修复任何工具。
    /// 见 [`crate::tool_repair`]:模型偶发把 array/object 参数双重编码成字符串。
    tool_repair_fields: HashMap<String, HashSet<String>>,
    /// 当前开着的 tool_use 若需修复,缓冲其 input(配待修复字段集)。None = 逐帧透传。
    /// caio 任一时刻至多一个块开着(见 open_tool),故单 Option 即可对应当前工具。
    tool_buf: Option<ToolInputBuffer>,
    /// 关闭 thinking 块时是否发 `signature_delta`。默认 **true**(现状:带签名)。
    /// prod 路径据 [`crate::converter::thinking_signature_enabled`] 每请求读一次设入;关掉后
    /// thinking 块照常输出但不带签名(多上游反代防 Kiro 合成签名漂到真 Anthropic/Bedrock 被拒)。
    emit_signature: bool,
}

/// 需修复工具的 input 累积缓冲。`fields` 在首帧据工具短名解析自 `tool_repair_fields`，
/// 与 `buf` 一起留存,避免续帧/收尾帧无 name 时无法再查表。
struct ToolInputBuffer {
    buf: String,
    fields: HashSet<String>,
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
            has_tool_use: false,
            open_tool: None,
            stopped_tools: HashSet::new(),
            thinking_opened: false,
            text_opened: false,
            thinking_only_max_tokens: false,
            tool_repair_fields: HashMap::new(),
            tool_buf: None,
            emit_signature: true,
        }
    }

    /// 注入工具防御性修复字段表(converter 据各工具 input_schema 算得)。
    /// 仅 prod 路径调用;测试默认空表 = 不修复,行为与改动前一致。
    fn set_tool_repair_fields(&mut self, fields: HashMap<String, HashSet<String>>) {
        self.tool_repair_fields = fields;
    }

    /// 设置是否在 thinking 块上附 signature。prod 路径每请求据进程级热开关
    /// [`crate::converter::thinking_signature_enabled`] 读一次设入;测试可直接置 false 验证。
    fn set_emit_signature(&mut self, v: bool) {
        self.emit_signature = v;
    }

    /// 当前是否有 thinking 块开着。**仅供 redacted 抓样记录上下文**
    /// (对抗评审 r2 #5:样本要能回答"明文与 redacted 是否共存、谁先谁后")——
    /// redacted 帧到达时若已有块开着,说明二者共存于同一段推理。
    fn reasoning_block_open(&self) -> bool {
        self.reasoning_active
    }

    /// 处理 reasoningContentEvent:捕获签名 + 逐片发 thinking_delta。
    fn on_reasoning(&mut self, text: &str, signature: Option<&str>) -> Vec<SseEvent> {
        // 1) 先捕获签名(上游在 thinking 流最后一帧单独下发 {"signature":...},无 text)。
        //    **原样透传,不再改写 f6 模型代号**(2026-08-19 起):签名要随客户端历史
        //    回传上行做结构化 reasoningContent 回放,上游验签只认原代号,改写过的
        //    签名会吃 400 THINKING_SIGNATURE_INVALID(探针实测)。f6 代号泄漏给
        //    第三方检测平台的风险,用户已明确不关心(用户体验优先)。
        //    历史遗留的改写签名(本次改动之前下发的,f6=官方名)在回传时按「代号不匹配」
        //    **丢弃**,不做反写 —— 反写方案已评估否决:改写是破坏性覆盖(原 f6 字节不复存在),
        //    反写只能照代号表重建,而重写用的是**客户端请求名**、与上游实际服务模型不恒等,
        //    猜错即 400。宁可丢一轮推理。见 history.rs::build_reasoning_content。
        if let Some(sig) = signature {
            if !sig.is_empty() {
                self.reasoning_signature = Some(sig.to_string());
            }
        }
        if text.is_empty() {
            return Vec::new();
        }
        // 2) 顺序保护:已产出过正文/工具块且无活跃 reasoning → reasoning 迟到,丢弃。
        //    用单调的 text_opened/has_tool_use(而非 text_index——它会在 text→tool 边界被
        //    close_active_block 置 None,导致迟到 reasoning 误判为合法而在 text/tool 后重开 thinking)。
        if (self.text_opened || self.has_tool_use) && !self.reasoning_active {
            tracing::warn!("reasoningContentEvent 迟于正文/工具到达,已丢弃以避免非法块顺序");
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

    /// 关闭当前开着的内容块(reasoning / text / tool 中至多一个)。块不可交错的不变量:
    /// 开任何新块前都先调它。三个 close_*_if_open 各自在未开时是 no-op,故至多一个产出事件。
    fn close_active_block(&mut self) -> Vec<SseEvent> {
        let mut events = self.close_reasoning_if_open();
        events.extend(self.close_text_if_open());
        events.extend(self.close_open_tool());
        events
    }

    /// 关闭开着的 tool_use 块(若有):先冲洗缓冲(修复后)的 input,再发 content_block_stop
    /// + 记入 stopped_tools。这统一了正常 stop 与"新工具强关旧工具 / finish 收尾未 stop"边界——
    /// 缓冲路径只在此处下发一次完整(已解包修复)的 input_json_delta。
    fn close_open_tool(&mut self) -> Vec<SseEvent> {
        match self.open_tool.take() {
            Some((idx, id)) => {
                let mut events = Vec::new();
                if let Some(tb) = self.tool_buf.take() {
                    let repaired = crate::tool_repair::repair_str(&tb.buf, &tb.fields);
                    if !repaired.is_empty() {
                        self.output_tokens += (repaired.len() as i64 + 3) / 4;
                        events.push(SseEvent::new(
                            "content_block_delta",
                            json!({"type":"content_block_delta","index":idx,
                                   "delta":{"type":"input_json_delta","partial_json":repaired}}),
                        ));
                    }
                }
                self.stopped_tools.insert(id);
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":idx}),
                ));
                events
            }
            None => {
                // open_tool 为 None 时不应有缓冲;防御性清理,避免泄漏到下一个工具。
                self.tool_buf = None;
                Vec::new()
            }
        }
    }

    /// 开 thinking 块(若未开),返回 content_block_start(若发生)。共用于 native/inline 路径。
    /// 开块前关掉任何开着的 text/tool 块(thinking 必须独立,不与其它块交错)。
    fn open_thinking_block_if_needed(&mut self) -> Vec<SseEvent> {
        if self.reasoning_active {
            return Vec::new();
        }
        let mut events = self.close_active_block();
        let idx = self.next_index;
        self.next_index += 1;
        self.thinking_index = Some(idx);
        self.reasoning_active = true;
        self.thinking_opened = true;
        events.push(SseEvent::new(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"thinking","thinking":""}}),
        ));
        events
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
    /// native 与 inline 路径共用:有真签名则**原样透传**,否则按累积 thinking 文本合成。
    fn close_reasoning_if_open(&mut self) -> Vec<SseEvent> {
        if !self.reasoning_active {
            return Vec::new();
        }
        let mut events = Vec::new();
        if let Some(idx) = self.thinking_index {
            // emit_signature 关时:thinking 块照常闭合(content_block_stop),但**不发**
            // signature_delta。Kiro **合成**签名是 Kiro 专用、对真 Anthropic/Bedrock 验签非法,
            // (真签名自 2026-08-19 起原样透传、跨平台本就可验;此处风险只剩合成签名)
            // 多上游反代里跨通道漂移会被拒 THINKING_SIGNATURE_INVALID(见 converter::thinking_signature_enabled)。
            if self.emit_signature {
                let signature = match &self.reasoning_signature {
                    Some(s) => s.clone(),
                    None => crate::signature::synthesize_signature(&self.model, &self.thinking_text),
                };
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":idx,
                           "delta":{"type":"signature_delta","signature":signature}}),
                ));
            }
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

    /// 开 text 块(若未开)+ 发 text_delta。开块前关掉任何开着的 reasoning/tool 块。
    fn push_text(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.text_index.is_none() {
            events.extend(self.close_active_block());
            let idx = self.next_index;
            self.next_index += 1;
            self.text_index = Some(idx);
            self.text_opened = true;
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

    /// 关闭开着的 text 块(若有)。tool_use 开块前与收尾共用:块不可交错,
    /// 开新块前必须先 stop 当前块;text_index 置 None,后续正文会另开新 text 块。
    fn close_text_if_open(&mut self) -> Vec<SseEvent> {
        match self.text_index.take() {
            Some(idx) => vec![SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            )],
            None => Vec::new(),
        }
    }

    /// 处理 toolUseEvent:开/续 tool_use 块,input 增量透传为 input_json_delta。
    /// 🔵 对齐 kiro.rs stream.rs process_tool_use。claude-all-in-one 不做 tool 名重映射
    /// (请求侧原样转发客户端 tool 定义,上游回原名),无需 tool_name_map。
    ///
    /// **顺序流式假设**:Kiro 按 `stop` 字段分隔顺序流式工具(一个工具 stop=true 后才下一个),
    /// 与参考实现 kiro.rs 一致。Anthropic SSE 不允许块交错,故若上游异常交错(t1→t2→t1),
    /// 开 t2 时会强关 t1 并 tombstone(stopped_tools),迟到的 t1 续帧被忽略——保证 SSE 合法,
    /// 代价是该极端情形下 t1 的 JSON 被截断。真正支持交错需缓冲各工具 input 到 stop 再顺序吐出,
    /// 属未发生场景的复杂度,按 [[subtract-before-you-add]] 暂不实现。
    fn on_tool_use(&mut self, name: &str, tool_use_id: &str, input: &str, stop: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // 已 stop 的 tool id:忽略迟到/重复帧,避免向已关闭块再发 delta/stop(非法 SSE)。
        if self.stopped_tools.contains(tool_use_id) {
            tracing::warn!("toolUseEvent 命中已结束的 tool id {tool_use_id},忽略迟到帧");
            return events;
        }
        // 是否为当前开着工具的增量续帧(同 id)。续帧只带 input 增量,复用块,不重开。
        let is_continuation =
            matches!(&self.open_tool, Some((_, open_id)) if open_id == tool_use_id);
        if !is_continuation {
            // 新工具调用。首帧必须带 name(agentic 客户端按工具名路由);缺 name = 上游畸形帧,
            // 丢弃而非产出空名块(空名块客户端无法路由,却会以 stop_reason=tool_use 假成功收尾)。
            if name.is_empty() {
                tracing::warn!("新 toolUseEvent(id {tool_use_id})缺 name,丢弃畸形帧");
                return events;
            }
            // 正文/思考必须在 tool_use 之前:先冲洗 inline thinking 缓冲(否则缓冲内容会被
            // 推到 finish() 在 tool_use 之后才吐出,块顺序倒置)。🔵 对齐 kiro.rs process_tool_use。
            if self.thinking_enabled && !self.native_reasoning_seen {
                let segs = self.inline.flush();
                events.extend(self.apply_inline(segs));
            }
            // 关闭当前开着的块(reasoning/text/其它 tool),保证块不交错。
            events.extend(self.close_active_block());
        }
        self.has_tool_use = true;
        let idx = match &self.open_tool {
            Some((i, _)) => *i, // 续帧:复用开着的块索引
            None => {
                let i = self.next_index;
                self.next_index += 1;
                self.open_tool = Some((i, tool_use_id.to_string()));
                // 首帧据工具短名查修复表:含 array/object 字段 → 开缓冲(续帧复用),否则透传。
                self.tool_buf = self
                    .tool_repair_fields
                    .get(name)
                    .filter(|f| !f.is_empty())
                    .map(|f| ToolInputBuffer {
                        buf: String::new(),
                        fields: f.clone(),
                    });
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({"type":"content_block_start","index":i,
                           "content_block":{"type":"tool_use","id":tool_use_id,"name":name,"input":{}}}),
                ));
                i
            }
        };
        if let Some(tb) = self.tool_buf.as_mut() {
            // 缓冲路径:累积 input,先不下发 delta;close_open_tool 在 stop 时解包修复后一次性下发。
            tb.buf.push_str(input);
        } else if !input.is_empty() {
            // 逐帧透传路径(无 array/object 字段的工具,零行为变更)。
            // tool input 计入 output(对齐 kiro.rs:(len+3)/4 估算)。
            self.output_tokens += (input.len() as i64 + 3) / 4;
            events.push(SseEvent::new(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,
                       "delta":{"type":"input_json_delta","partial_json":input}}),
            ));
        }
        if stop {
            // close_open_tool 统一收尾:冲洗缓冲(修复后)+ 发 content_block_stop + tombstone。
            events.extend(self.close_open_tool());
        }
        events
    }

    /// 本次响应是否产出过实质内容(正文 token / 工具调用 / 原生 reasoning / inline thinking)。
    /// 🔵 对齐 kiro.rs produced_any_content;再 OR 上 thinking_opened 覆盖 inline 路径。
    /// 用于检测"上游 200 但零事件"的空响应。
    fn produced_any_content(&self) -> bool {
        self.output_tokens > 0
            || self.has_tool_use
            || self.native_reasoning_seen
            || self.thinking_opened
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
    /// thinking-only 兜底在此判定:整流只产出 thinking 块 → 模型把 token 预算耗在
    /// 思考上,置 thinking_only_max_tokens 并补发一个空格 text 块(完整 start/delta/stop),
    /// 保证 content 数组含 text 块(🔵 kiro.rs stream.rs:1466-1475)。
    fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.thinking_enabled && !self.native_reasoning_seen {
            let tail = self.inline.finish();
            events.extend(self.apply_inline(tail));
        }
        events.extend(self.close_reasoning_if_open());
        // 关闭未收到 stop=true 的开着的 tool_use 块(否则 message_delta 前留半开块,违反时序)。
        events.extend(self.close_open_tool());
        if self.thinking_enabled && self.thinking_opened && !self.text_opened && !self.has_tool_use {
            self.thinking_only_max_tokens = true;
            events.extend(self.push_text(" "));
        }
        events.extend(self.close_text_if_open());
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
    tool_repair_fields: HashMap<String, HashSet<String>>,
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
        tracker.set_tool_repair_fields(tool_repair_fields);
        // 多上游反代:据热开关决定是否给 thinking 块附签名(默认开;关掉防 Kiro 合成签名漂到
        // 真 Anthropic/Bedrock 通道被拒)。每请求读一次,设置面板改后下个请求即生效。
        tracker.set_emit_signature(crate::converter::thinking_signature_enabled());
        let mut decoder = EventStreamDecoder::new();
        // 本响应内已抓样的 redacted 帧序号(对抗评审 r2 #5:样本要能回答"顺序与共存")。
        let mut reasoning_seq: u32 = 0;
        // 显式 stop_reason(model_context_window_exceeded / max_tokens)。None = 收尾按
        // tool_use > end_turn 优先级推导(🔵 kiro.rs stream.rs get_stop_reason)。
        let mut stop_reason: Option<String> = None;

        // report_total:计费基准 token。优先 tokenUsageEvent 真值(uncached+cacheRead),
        // 退而 contextUsageEvent(pct×窗口),再退模拟器 sim_total。
        let mut report_total: Option<i32> = None;
        // 诊断/优化信号(不参与计费):真实上游命中 + Kiro 原生计费。
        // real_cache_read = tokenUsageEvent.cacheReadInputTokens(真号在 Kiro 服务端的真实
        // prefix cache 命中);metering_credit = meteringEvent.usage(真号本次真实积分消耗)。
        // 二者落进请求日志的"真 / credit"列,供前端看每条请求的真实命中与真实成本来优化缓存。
        let mut real_cache_read: i64 = 0;
        let mut metering_credit: f64 = 0.0;
        // upstream_cut 检测(封号前兆软信号,StreamItem::UpstreamCut 的发射条件):
        // - saw_upstream_payload:见过**真实上游事件帧**(不含我方合成的 message_start;
        //   合成首帧在读上游字节前就发出,拿它当"上游已接受"会把空响应误判成掐流,
        //   codex 对抗评审#3)。
        // - saw_terminal:见过正常终止信号 = metadataEvent(带 stopReason,Kiro 正常
        //   收尾帧)/ meteringEvent(流末尾的原生计费帧)/ 上游显式 stop 条件
        //   (ContentLengthExceeded、context 超限 → stop_reason 置位)。
        // EOF 时 saw_upstream_payload && !saw_terminal → 流是被上游**静默掐断**的。
        let mut saw_upstream_payload = false;
        let mut saw_terminal = false;
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
                        // 异常/错误帧按 :message-type 识别——异常帧常无 :event-type,
                        // 只有 :message-type=exception + :exception-type,按 event_type
                        // 匹配会漏进 `_ => {}` 被静默吞掉(🔵 kiro.rs events/base.rs 同路由)。
                        match frame.message_type().unwrap_or("event") {
                            "exception" => {
                                let ex = frame.exception_type().unwrap_or("unknown");
                                if ex == "ContentLengthExceededException" {
                                    // 正常 max_tokens 截断:模型已产出内容到上限,非失败,
                                    // 继续收尾(🔵 kiro.rs stream.rs:856-874)。
                                    stop_reason = Some("max_tokens".to_string());
                                } else {
                                    let _ = tx
                                        .send(Err(UpstreamError::new(
                                            UpstreamErrorKind::ServerError,
                                            format!("上游异常 {ex}: {}", frame.payload_as_str()),
                                        )))
                                        .await;
                                    return;
                                }
                                continue;
                            }
                            "error" => {
                                let code = frame.error_code().unwrap_or("unknown");
                                let _ = tx
                                    .send(Err(UpstreamError::new(
                                        UpstreamErrorKind::ServerError,
                                        format!("上游错误 {code}: {}", frame.payload_as_str()),
                                    )))
                                    .await;
                                return;
                            }
                            _ => {}
                        }
                        // 走到这里 = 真实解出的上游事件帧(exception/error 帧已在上面
                        // 提前 return/continue;ContentLengthExceeded 虽 continue 跳过本行,
                        // 但它会置 stop_reason,按终止信号处理,不影响判定)。
                        saw_upstream_payload = true;
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
                                    // 抓样(2026-08-20,对抗评审 #1 + 用户决策"先监视不处理"):
                                    // `redactedContent` 是 `ReasoningContent` union 的一支,
                                    // **搭在本事件 payload 里**、不需要独立事件类型 —— 拆包
                                    // 1.0.369 的客户端确实从响应读 `reasoningRedactedContent`
                                    // (见 `oe12` / `withReasoningContent` 第 4 参)。
                                    // 我方**尚未实现该支下行**(Anthropic 文档只给非流式块形状,
                                    // 未描述 SSE 形状,猜错=发畸形流),所以这里只存证不处理:
                                    // 本段推理仍会被下方 `on_reasoning` 在 text 为空处丢弃。
                                    // 详见 crate::redacted_probe 的模块文档。
                                    let has_redacted =
                                        crate::redacted_probe::payload_has_redacted(&v);
                                    if has_redacted {
                                        crate::redacted_probe::capture(
                                            &v,
                                            &model,
                                            reasoning_seq,
                                            tracker.reasoning_block_open(),
                                        );
                                        reasoning_seq += 1;
                                    }
                                    // 【对抗评审 r2 #1 High】redacted 帧的 `signature` **绝不能**
                                    // 进 tracker。`on_reasoning` 在 `text.is_empty()` **之前**就捕获
                                    // 签名,所以序列
                                    //   thinking A → signature A → redacted R + signature R
                                    // 会让 R 的签名覆盖 A,收尾下发「thinking A + signature R」——
                                    // 这不是丢一段推理,是**制造错误的密文归属**:客户端把它写进历史,
                                    // 下一轮上行验签失败 → 400 THINKING_SIGNATURE_INVALID → 剥全部
                                    // 历史 reasoning 重试。比缺失严重得多。
                                    // 「只观测不处理」必须**真的**零副作用:text 照常透传(若有),
                                    // 但签名按 None 处理。
                                    let sig = if has_redacted { None } else { sig };
                                    for ev in tracker.on_reasoning(text, sig) {
                                        if tx.send(Ok(StreamItem::Sse(ev))).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            Some("toolUseEvent") => {
                                // 上游 payload(camelCase):{name, toolUseId,
                                // input(String,默认"",可增量部分 JSON), stop(bool,默认 false)}。
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                                    let id =
                                        v.get("toolUseId").and_then(|x| x.as_str()).unwrap_or("");
                                    let input =
                                        v.get("input").and_then(|x| x.as_str()).unwrap_or("");
                                    let stop =
                                        v.get("stop").and_then(|x| x.as_bool()).unwrap_or(false);
                                    if id.is_empty() {
                                        tracing::warn!("toolUseEvent 缺 toolUseId,已丢弃");
                                    } else {
                                        for ev in tracker.on_tool_use(name, id, input, stop) {
                                            if tx.send(Ok(StreamItem::Sse(ev))).await.is_err() {
                                                return;
                                            }
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
                                    // 留住真实命中(诊断/优化用,不进计费)。
                                    real_cache_read = cr.max(0);
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
                                        stop_reason =
                                            Some("model_context_window_exceeded".to_string());
                                    }
                                }
                            }
                            Some("meteringEvent") => {
                                // Kiro 原生计费(真号本次真实积分消耗,单位通常 "credit")。v53 不靠它
                                // 反推缓存,但留作请求日志的"真实 credit"信号——判断 Kiro 服务端有没有
                                // 应用缓存折扣、每条请求真号到底烧了多少。仅记录,不参与上报计费。
                                // 流末尾帧:见过它 = 上游完整跑完了本次计费,视作正常终止信号。
                                saw_terminal = true;
                                if let Ok(v) = frame.payload_as_json::<serde_json::Value>() {
                                    if let Some(u) = v.get("usage").and_then(|x| x.as_f64()) {
                                        metering_credit = u;
                                    }
                                }
                            }
                            Some("metadataEvent") => {
                                // Kiro 正常收尾帧(带 stopReason):**正常终止信号**。
                                // 它的缺席正是「静默掐流」的判据(见收尾处 UpstreamCut 发射)。
                                // stopReason 的具体值不进 SSE(收尾仍按既有优先级推导 stop_reason)。
                                saw_terminal = true;
                            }
                            // event-type 异常名兜底(:message-type=event 但 event-type 是异常名
                            // 的奇异帧;常规异常帧已在上方按 :message-type 路由)。
                            // ContentLengthExceeded 同样按 max_tokens 正常收尾,其余报错终止,
                            // 不再补发 message_delta/message_stop/Usage:避免「error 后又正常
                            // 收尾计费」的自相矛盾序列(🔵 对齐 kiro.rs stream.rs:1394 注释)。
                            Some(other) if other.contains("xception") || other.contains("rror") => {
                                if other.contains("ContentLengthExceeded") {
                                    stop_reason = Some("max_tokens".to_string());
                                } else {
                                    let _ = tx
                                        .send(Err(UpstreamError::new(
                                            UpstreamErrorKind::ServerError,
                                            format!(
                                                "上游异常事件 {other}: {}",
                                                frame.payload_as_str()
                                            ),
                                        )))
                                        .await;
                                    return;
                                }
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

        // --- upstream_cut 发射(封号前兆软信号,only-EOF 路径)---
        // 判定:见过真实上游事件帧,但直到底层 EOF 都没见到任何正常终止信号
        // (metadataEvent / meteringEvent / 显式 stop 条件)→ 流是被上游静默掐断的,
        // 而非正常收尾。实测这种流之后 4-20 分钟账号吃 TEMPORARILY_SUSPENDED。
        // worker 收到后只记录 + 软冷却,不转发客户端;下方 finish 合成收尾照常发出
        // (客户端仍拿到完整形态的消息,掐流事实由 error_kind=upstream_cut 落库)。
        // 只发一次;正常 finish 路径(有终止信号)不发。
        // 传输错误(读流 Err)不发:那条路已有 Err → report_failure 的既有口径兜底,
        // 再发会让同一次故障进两套体系(codex 对抗评审#2:隔离原则)。
        if saw_upstream_payload && !saw_terminal && stop_reason.is_none() {
            tracing::warn!(model = %model,
                "上游静默掐流:见过 payload 但无终止事件就 EOF,上报 upstream_cut");
            let _ = tx.send(Ok(StreamItem::UpstreamCut)).await;
        }

        // --- 收尾:关掉任何开着的块(thinking/text)+ thinking-only 兜底 ---
        let finish_events = tracker.finish();

        // ⑤ 空响应检测(v60 契约):到达此处 = 流无失败;零实质产出且无显式 stop_reason
        // (max_tokens/context 超限的零产出是合法退化,不算空)→ 终态 Err(EmptyResponse),
        // 跳过 message_delta/message_stop/Usage(message_start 已急发无妨,不构成非法序列)。
        // worker 的 Err 路径会 report_failure → scheduler v58 阈值冷却,并向客户端转发
        // 终态 SSE error,Anthropic 客户端自行重试。不做任何换 ID 重发(v60 已删,有害)。
        if !tracker.produced_any_content() && stop_reason.is_none() {
            tracing::warn!("检测到空响应:上游 200 但零内容产出");
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::EmptyResponse,
                    "上游空响应(零内容产出)",
                )))
                .await;
            return;
        }
        for ev in finish_events {
            let _ = tx.send(Ok(StreamItem::Sse(ev))).await;
        }
        // thinking-only:finish() 判定整流只有 thinking 块 → max_tokens(模型把预算耗在思考上)。
        // 仅在无显式 stop_reason 时填:context 超限(model_context_window_exceeded)等显式终态
        // 优先,不被 thinking-only 兜底覆盖(显式 stop_reason 优先契约)。
        if tracker.thinking_only_max_tokens && stop_reason.is_none() {
            stop_reason = Some("max_tokens".to_string());
        }
        // stop_reason 优先级:显式 > tool_use > end_turn(🔵 kiro.rs get_stop_reason)。
        let stop_reason = stop_reason.unwrap_or_else(|| {
            if tracker.has_tool_use {
                "tool_use".to_string()
            } else {
                "end_turn".to_string()
            }
        });

        // --- 计费收尾(v53 统一走模拟器,见 usage.rs)---
        let output_tokens = tracker.output_tokens.min(i32::MAX as i64) as i32;
        // report_total 优先级:tokenUsage 真值 > contextUsage 估算 > 模拟器 sim_total 兜底。
        let final_input_tokens = report_total.unwrap_or(sim_total).max(0);
        // 零产出保护:本轮没交付任何实质内容(正文/工具调用/思考)则不计缓存
        // (用户没拿到东西不该为缓存付费)。
        //
        // ⚠️ 判据必须是 `produced_any_content()` 而不是 `output_tokens <= 0`:
        // 裸工具调用(空参数 `{}` 的 tool_use)output_tokens 恰为 0,但用户实实在在
        // 收到了 tool_use 并据此执行 —— 旧判据会把整轮的缓存折扣误清。
        // (2026-08-24 生产实测:agentbridge 轮询循环里 get_messages({}) 每轮 327k
        // 输入被全价计费,panel 显示"无缓存命中",而上游明明有缓存。)
        // 真正的零事件空流自有 EmptyResponse 兜底(400,见零产出窥探)。
        let zero_output = !tracker.produced_any_content();
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
                real_cache_read_tokens: real_cache_read.max(0) as u64,
                metering_credit,
            })))
            .await;
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // ===== 出站体积护栏 enforce_body_limit(🔵 kiro.rs v63 移植)=====
    mod body_limit {
        use super::super::enforce_body_limit;
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, KiroDocument, Message,
            UserInputMessage, UserMessage,
        };
        use crate::kiro_types::request::KiroRequest;
        use gw_core::error::UpstreamErrorKind;

        fn req_with_history_doc(b64_len: usize) -> KiroRequest {
            let mut hist = UserMessage::new("analyze this", "claude-opus-4.8");
            hist.documents
                .push(KiroDocument::from_base64("doc", "pdf", "A".repeat(b64_len)));
            let cs = ConversationState::new("conv")
                .with_current_message(CurrentMessage::new(UserInputMessage::new(
                    "hello",
                    "claude-opus-4.8",
                )))
                .with_history(vec![Message::User(HistoryUserMessage { user_input_message: hist })]);
            KiroRequest { conversation_state: cs, profile_arn: None, additional_model_request_fields: None, agent_mode: Some("vibe".into()) }
        }

        #[test]
        fn under_limit_passthrough_unchanged() {
            let mut req = req_with_history_doc(100);
            let body = serde_json::to_string(&req).unwrap();
            let original = body.clone();
            let out = enforce_body_limit(&mut req, body, 1_000_000).unwrap();
            assert_eq!(out, original, "未超限不应改动请求体");
            // 历史媒体应原样保留
            if let Message::User(u) = &req.conversation_state.history[0] {
                assert_eq!(u.user_input_message.documents.len(), 1);
            }
        }

        #[test]
        fn over_limit_sheds_history_media_and_passes() {
            let mut req = req_with_history_doc(50_000);
            let body = serde_json::to_string(&req).unwrap();
            assert!(body.len() > 10_000);
            let out = enforce_body_limit(&mut req, body, 10_000).unwrap();
            assert!(out.len() <= 10_000, "剔除后应低于上限,实际 {}", out.len());
            assert!(out.contains("attachment omitted"), "剔除回合应带占位说明");
            if let Message::User(u) = &req.conversation_state.history[0] {
                assert!(u.user_input_message.documents.is_empty());
            }
        }

        #[test]
        fn over_limit_no_sheddable_media_rejects_bad_request() {
            // 超大的是当前消息文本(history 无媒体可剔)→ BadRequest
            let mut hist = UserMessage::new("x".repeat(50_000), "claude-opus-4.8");
            hist.documents.clear();
            let cs = ConversationState::new("conv")
                .with_current_message(CurrentMessage::new(UserInputMessage::new(
                    "hi",
                    "claude-opus-4.8",
                )))
                .with_history(vec![Message::User(HistoryUserMessage { user_input_message: hist })]);
            let mut req = KiroRequest { conversation_state: cs, profile_arn: None, additional_model_request_fields: None, agent_mode: Some("vibe".into()) };
            let body = serde_json::to_string(&req).unwrap();
            let err = enforce_body_limit(&mut req, body, 10_000).unwrap_err();
            assert_eq!(err.kind, UpstreamErrorKind::BadRequest, "无媒体可剔应 BadRequest");
        }
    }

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

    /// 【对抗评审 r2 #1 High】redacted 帧的签名**绝不能**覆盖合法 thinking 的签名。
    ///
    /// 真实序列:`thinking A → signature A → redacted R + signature R`。
    /// `on_reasoning` 在 `text.is_empty()` **之前**捕获签名,所以若把 R 的签名喂进来,
    /// 收尾会下发「thinking A + signature R」—— 制造错误的密文归属,客户端写进历史后
    /// 下一轮上行验签失败 → 400 → 剥全历史 reasoning 重试。比丢一段推理严重得多。
    /// 调用点(`async_stream_like` 的 reasoningContentEvent 分支)据 `payload_has_redacted`
    /// 把该帧的 sig 置 None,本测试锁住"置 None 后归属正确"。
    #[test]
    fn redacted_frame_signature_must_not_overwrite_thinking_signature() {
        const SIG_A: &str = "Ev4BCmMIDhABGAIqQDLCxOcAxIGpEWzaBVN/7Rhnn7KPNqmlN3pQgWXeogdRhOlKAvxTylSWauMzkhf1NcylYW38yAUC463X+Bvj1YMyDWNsYXVkZS1xdWluY2U4AEIIdGhpbmtpbmcSDJZPrLrFRh2MFQgTIRoMLunMMbV2gAt9AB3FIjAfpHy8DkJKmF8LaQs9OEJhpMGgRwQvd6qHoPV5Rz2jXdeuhTBoQnCIMS44GqTamasqSZscuKHM930rQ31rcriqFj3AzLv8RnxlyFiu/fdDdt9YiFKtO38Cy4iqw35ZEKQr9J0/Mkru/S451tutqRClvGDgnIrJ2N0D3dcYAQ==";
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_reasoning("thinking A", None); // 开块 + 发 delta
        t.on_reasoning("", Some(SIG_A)); // 签名帧(无 text)
        // redacted 帧:调用点已把 sig 置 None,text 仍照常透传(此例为空)。
        t.on_reasoning("", None);
        let close = t.close_reasoning_if_open();
        let sig = close
            .iter()
            .find(|e| tag(e) == "content_block_delta:signature_delta")
            .expect("应有 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sig, SIG_A, "必须仍是 thinking A 自己的签名");
    }

    /// 反证:证明上面那条抑制是**承重**的 —— 若把 redacted 帧的签名喂进 tracker,
    /// 归属确实会被污染。这条测试存在的意义是:有人日后"简化"掉调用点的
    /// `let sig = if has_redacted { None } else { sig };` 时,能立刻看到代价。
    #[test]
    fn unsuppressed_redacted_signature_would_corrupt_attribution() {
        const SIG_A: &str = "Ev4BCmMIDhABGAIqQDLCxOcAxIGpEWzaBVN/7Rhnn7KPNqmlN3pQgWXeogdRhOlKAvxTylSWauMzkhf1NcylYW38yAUC463X+Bvj1YMyDWNsYXVkZS1xdWluY2U4AEIIdGhpbmtpbmcSDJZPrLrFRh2MFQgTIRoMLunMMbV2gAt9AB3FIjAfpHy8DkJKmF8LaQs9OEJhpMGgRwQvd6qHoPV5Rz2jXdeuhTBoQnCIMS44GqTamasqSZscuKHM930rQ31rcriqFj3AzLv8RnxlyFiu/fdDdt9YiFKtO38Cy4iqw35ZEKQr9J0/Mkru/S451tutqRClvGDgnIrJ2N0D3dcYAQ==";
        const SIG_R: &str = "cmVkYWN0ZWQtc2lnbmF0dXJlLW5vdC1vdXJz";
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_reasoning("thinking A", None);
        t.on_reasoning("", Some(SIG_A));
        t.on_reasoning("", Some(SIG_R)); // ← 未抑制:模拟 bug
        let close = t.close_reasoning_if_open();
        let sig = close
            .iter()
            .find(|e| tag(e) == "content_block_delta:signature_delta")
            .expect("应有 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sig, SIG_R, "未抑制时确实会被覆盖 —— 这就是必须抑制的理由");
        assert_ne!(sig, SIG_A);
    }

    #[test]
    fn real_signature_passed_through_untouched() {
        // 上游给真签名 → 关闭块时**原样透传**(2026-08-19 起不再改写 f6 代号:
        // 签名要随历史回传上行验签,改写过的会吃 400 THINKING_SIGNATURE_INVALID)。
        const REAL_SIG: &str = "Ev4BCmMIDhABGAIqQDLCxOcAxIGpEWzaBVN/7Rhnn7KPNqmlN3pQgWXeogdRhOlKAvxTylSWauMzkhf1NcylYW38yAUC463X+Bvj1YMyDWNsYXVkZS1xdWluY2U4AEIIdGhpbmtpbmcSDJZPrLrFRh2MFQgTIRoMLunMMbV2gAt9AB3FIjAfpHy8DkJKmF8LaQs9OEJhpMGgRwQvd6qHoPV5Rz2jXdeuhTBoQnCIMS44GqTamasqSZscuKHM930rQ31rcriqFj3AzLv8RnxlyFiu/fdDdt9YiFKtO38Cy4iqw35ZEKQr9J0/Mkru/S451tutqRClvGDgnIrJ2N0D3dcYAQ==";
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_reasoning("think", Some(REAL_SIG));
        let close = t.close_reasoning_if_open();
        let sig_ev = close
            .iter()
            .find(|e| tag(e) == "content_block_delta:signature_delta")
            .expect("应有 signature_delta");
        let sig = sig_ev.data["delta"]["signature"].as_str().unwrap();
        assert_eq!(sig, REAL_SIG, "真签名必须逐字节透传,不得改写");
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
    fn signature_suppressed_when_emit_disabled_even_with_real_upstream_sig() {
        // emit_signature=false:即便上游给了真签名,关闭 thinking 块时也**不发** signature_delta,
        // 但仍发 content_block_stop(块照常闭合)。多上游反代:不向会话注入跨通道非法签名。
        const REAL_SIG: &str = "Ev4BCmMIDhABGAIqQDLCxOcAxIGpEWzaBVN/7Rhnn7KPNqmlN3pQgWXeogdRhOlKAvxTylSWauMzkhf1NcylYW38yAUC463X+Bvj1YMyDWNsYXVkZS1xdWluY2U4AEIIdGhpbmtpbmcSDJZPrLrFRh2MFQgTIRoMLunMMbV2gAt9AB3FIjAfpHy8DkJKmF8LaQs9OEJhpMGgRwQvd6qHoPV5Rz2jXdeuhTBoQnCIMS44GqTamasqSZscuKHM930rQ31rcriqFj3AzLv8RnxlyFiu/fdDdt9YiFKtO38Cy4iqw35ZEKQr9J0/Mkru/S451tutqRClvGDgnIrJ2N0D3dcYAQ==";
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.set_emit_signature(false);
        t.on_reasoning("think", Some(REAL_SIG));
        let close = t.close_reasoning_if_open();
        assert!(
            !close.iter().any(|e| e.data["delta"]["type"] == "signature_delta"),
            "关签名时不应发 signature_delta"
        );
        assert!(
            close.iter().any(|e| e.data["type"] == "content_block_stop"),
            "thinking 块仍须正常闭合(content_block_stop)"
        );
    }

    #[test]
    fn strip_reasoning_from_history_clears_all_and_reports() {
        use crate::kiro_types::conversation::{
            AssistantMessage, ConversationState, HistoryAssistantMessage, HistoryUserMessage,
            Message, ReasoningContent, ReasoningText,
        };
        let mut req = KiroRequest {
            conversation_state: ConversationState::new("conv-strip"),
            profile_arn: None,
            additional_model_request_fields: None,
            agent_mode: None,
        };
        // 无 reasoning 时不剥、报 false
        assert!(!strip_reasoning_from_history(&mut req));

        let with_reasoning =
            AssistantMessage::new("答").with_reasoning_content(ReasoningContent::ReasoningText {
                reasoning_text: ReasoningText {
                    text: "推".into(),
                    signature: "sig".into(),
                },
            });
        req.conversation_state
            .history
            .push(Message::User(HistoryUserMessage::new("问", "claude-opus-4.8")));
        req.conversation_state
            .history
            .push(Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: with_reasoning,
            }));
        req.conversation_state
            .history
            .push(Message::Assistant(HistoryAssistantMessage::new("纯文本")));

        assert!(strip_reasoning_from_history(&mut req), "有 reasoning 应报 true");
        for m in &req.conversation_state.history {
            if let Message::Assistant(a) = m {
                assert!(
                    a.assistant_response_message.reasoning_content.is_none(),
                    "剥离后不得残留 reasoningContent"
                );
            }
        }
        // 再剥一次:无操作,报 false
        assert!(!strip_reasoning_from_history(&mut req));
        // 剥离后的序列化不得再含 reasoningContent 键
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("reasoningContent"), "实际: {s}");
    }

    #[test]
    fn signature_suppressed_when_emit_disabled_no_upstream_sig() {        // emit_signature=false 且上游无签名:不合成、不发 signature_delta,仍闭合块。
        let mut t = BlockTracker::new("claude-opus-4-6".to_string(), false);
        t.set_emit_signature(false);
        t.on_reasoning("reasoning text here", None);
        let close = t.close_reasoning_if_open();
        assert!(
            !close.iter().any(|e| e.data["delta"]["type"] == "signature_delta"),
            "关签名且无上游签名时也不应合成/发 signature_delta"
        );
        assert!(
            close.iter().any(|e| e.data["type"] == "content_block_stop"),
            "thinking 块仍须正常闭合(content_block_stop)"
        );
    }

    #[test]
    fn signature_emitted_by_default_when_enabled() {
        // 默认 emit_signature=true:行为与改动前一致,仍发 signature_delta(回归保护)。
        let mut t = BlockTracker::new("claude-opus-4-6".to_string(), false);
        t.on_reasoning("reasoning text here", None);
        let close = t.close_reasoning_if_open();
        assert!(
            close.iter().any(|e| e.data["delta"]["type"] == "signature_delta"),
            "默认应发 signature_delta(保留现状)"
        );
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

    // ---- ①B tool_use 响应路径 ----

    #[test]
    fn tool_use_after_reasoning_closes_thinking_first() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_reasoning("思考", None));
        all.extend(t.on_tool_use("get_weather", "tooluse_1", r#"{"city":"sf"}"#, true));
        let seq = tags(&all);
        assert_eq!(
            seq,
            vec![
                "content_block_start:thinking",
                "content_block_delta:thinking_delta",
                "content_block_delta:signature_delta",
                "content_block_stop",
                "content_block_start:tool_use",
                "content_block_delta:input_json_delta",
                "content_block_stop",
            ],
            "reasoning 后直接 tool_use 应先闭 thinking 块"
        );
        assert!(t.has_tool_use);
    }

    #[test]
    fn tool_use_block_carries_id_name_and_empty_input() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let evs = t.on_tool_use("get_weather", "tooluse_1", "", false);
        let start = evs.iter().find(|e| e.event == "content_block_start").unwrap();
        assert_eq!(start.data["content_block"]["type"], "tool_use");
        assert_eq!(start.data["content_block"]["id"], "tooluse_1");
        assert_eq!(start.data["content_block"]["name"], "get_weather");
        assert_eq!(start.data["content_block"]["input"], serde_json::json!({}));
        assert_eq!(evs.len(), 1, "空 input 且未 stop 时只应有 start");
    }

    fn repair_tracker(tool: &str, field: &str) -> BlockTracker {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut fields = HashSet::new();
        fields.insert(field.to_string());
        let mut map = HashMap::new();
        map.insert(tool.to_string(), fields);
        t.set_tool_repair_fields(map);
        t
    }

    #[test]
    fn tool_input_repair_unwraps_double_encoded_questions() {
        // 复刻线上 bug:questions 被模型双重编码成 JSON 字符串。
        let mut t = repair_tracker("AskUserQuestion", "questions");
        let evs = t.on_tool_use(
            "AskUserQuestion",
            "tq",
            r#"{"questions":"[{\"header\":\"H\"}]"}"#,
            true,
        );
        let delta = evs
            .iter()
            .find(|e| e.event == "content_block_delta")
            .expect("应有 input_json_delta");
        let pj = delta.data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(pj).unwrap();
        assert!(v["questions"].is_array(), "questions 应被解包成数组: {pj}");
        assert_eq!(v["questions"][0]["header"], "H");
        assert!(evs.iter().any(|e| e.event == "content_block_stop"));
    }

    #[test]
    fn tool_input_repair_across_fragmented_frames() {
        // 分帧:首帧带 name 无 stop,中间续 input,末帧 stop=true;缓冲路径只在 stop 发一次 delta。
        let mut t = repair_tracker("AskUserQuestion", "questions");
        let mut all = Vec::new();
        all.extend(t.on_tool_use("AskUserQuestion", "t1", r#"{"questions":"[{\"hea"#, false));
        all.extend(t.on_tool_use("AskUserQuestion", "t1", r#"der\":\"H\"}]"}"#, false));
        all.extend(t.on_tool_use("AskUserQuestion", "t1", "", true));
        let deltas: Vec<_> = all
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .collect();
        assert_eq!(deltas.len(), 1, "缓冲路径应只在 stop 时发一次 delta");
        let pj = deltas[0].data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(pj).unwrap();
        assert!(v["questions"].is_array());
        assert_eq!(v["questions"][0]["header"], "H");
    }

    #[test]
    fn non_repair_tool_streams_verbatim() {
        // 未注册 repair 字段的工具 → 逐帧透传,partial_json 原样。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let raw = r#"{"command":"ls"}"#;
        let evs = t.on_tool_use("Bash", "b1", raw, true);
        let delta = evs
            .iter()
            .find(|e| e.event == "content_block_delta")
            .unwrap();
        assert_eq!(
            delta.data["delta"]["partial_json"].as_str().unwrap(),
            raw,
            "透传路径应逐字保留 partial_json"
        );
    }

    #[test]
    fn tool_input_repair_flushes_on_finish_without_stop() {
        // 边界:缓冲工具未收到 stop,流结束 finish() 收尾应冲洗(修复后)再关块。
        let mut t = repair_tracker("AskUserQuestion", "questions");
        let mut all = Vec::new();
        all.extend(t.on_tool_use(
            "AskUserQuestion",
            "t1",
            r#"{"questions":"[{\"header\":\"H\"}]"}"#,
            false,
        ));
        all.extend(t.finish());
        let delta = all
            .iter()
            .find(|e| e.event == "content_block_delta"
                && e.data["delta"]["type"] == "input_json_delta")
            .expect("finish 应冲洗缓冲的 tool input");
        let pj = delta.data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(pj).unwrap();
        assert!(v["questions"].is_array());
        assert!(all.iter().any(|e| e.event == "content_block_stop"));
    }

    #[test]
    fn tool_use_closes_open_text_block_first() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_text("先有正文"));
        all.extend(t.on_tool_use("f", "t1", "{}", true));
        let seq = tags(&all);
        assert_eq!(
            seq,
            vec![
                "content_block_start:text",
                "content_block_delta:text_delta",
                "content_block_stop",
                "content_block_start:tool_use",
                "content_block_delta:input_json_delta",
                "content_block_stop",
            ],
            "开新 tool_use 块前必须先关开着的 text 块"
        );
        let tool_start = all
            .iter()
            .find(|e| e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use")
            .unwrap();
        assert_eq!(tool_start.data["index"], 1);
    }

    #[test]
    fn tool_use_incremental_frames_share_one_block() {
        // 同一 toolUseId 增量多帧:仅一次 start,多次 input_json_delta,stop=true 才 stop。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_tool_use("f", "t1", r#"{"a":"#, false));
        all.extend(t.on_tool_use("f", "t1", "1}", true));
        let starts = all.iter().filter(|e| e.event == "content_block_start").count();
        let deltas = all
            .iter()
            .filter(|e| tag(e) == "content_block_delta:input_json_delta")
            .count();
        let stops = all.iter().filter(|e| e.event == "content_block_stop").count();
        assert_eq!((starts, deltas, stops), (1, 2, 1));
        let parts: Vec<&str> = all
            .iter()
            .filter(|e| tag(e) == "content_block_delta:input_json_delta")
            .map(|e| e.data["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(parts.join(""), r#"{"a":1}"#, "partial_json 应原样增量透传");
    }

    #[test]
    fn tool_use_input_counts_into_output_tokens() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_tool_use("f", "t1", "12345678", true); // (8+3)/4 = 2
        assert_eq!(t.output_tokens, 2);
    }

    #[test]
    fn unstopped_tool_block_is_closed_at_finish() {
        // tool_use 没收到 stop=true 就流结束 → finish() 必须补 content_block_stop,
        // 否则 message_delta 前留半开块,违反 SSE 时序。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_tool_use("f", "t1", "{}", false)); // stop=false
        all.extend(t.finish());
        let stops = all.iter().filter(|e| e.event == "content_block_stop").count();
        let starts = all
            .iter()
            .filter(|e| e.event == "content_block_start")
            .count();
        assert_eq!(starts, 1, "应只有一个 tool_use start");
        assert_eq!(stops, 1, "finish 必须关闭未 stop 的 tool_use 块");
        assert_eq!(all.last().unwrap().event, "content_block_stop");
    }

    #[test]
    fn two_concurrent_tools_do_not_interleave() {
        // t1(stop=false) 后 t2(stop=false):开 t2 前必须先关 t1,块不可交错。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_tool_use("f1", "t1", "{}", false));
        all.extend(t.on_tool_use("f2", "t2", "{}", false));
        let seq = tags(&all);
        // 期望:t1 start, t1 delta, t1 stop(因 t2 到来被迫关闭), t2 start, t2 delta
        assert_eq!(
            seq,
            vec![
                "content_block_start:tool_use",
                "content_block_delta:input_json_delta",
                "content_block_stop",
                "content_block_start:tool_use",
                "content_block_delta:input_json_delta",
            ],
            "并发工具帧不得交错:开 t2 前必须 stop t1"
        );
        // t1 与 t2 用不同块索引
        let starts: Vec<i64> = all
            .iter()
            .filter(|e| e.event == "content_block_start")
            .map(|e| e.data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 1]);
    }

    #[test]
    fn stopped_tool_id_ignores_later_frames() {
        // 同一 toolUseId 在 stop 后再来帧 → 忽略,不向已关闭块再发 delta/stop。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut first = t.on_tool_use("f", "t1", "{}", true);
        let stops_first = first.iter().filter(|e| e.event == "content_block_stop").count();
        assert_eq!(stops_first, 1);
        first.clear();
        let late = t.on_tool_use("f", "t1", "more", true);
        assert!(
            late.is_empty(),
            "已 stop 的 tool id 再来帧应被忽略,得到: {:?}",
            tags(&late)
        );
    }

    #[test]
    fn new_tool_missing_name_is_dropped() {
        // 新 tool id 首帧缺 name(agentic 客户端按名路由)→ 丢弃畸形帧,不产出空名块。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let evs = t.on_tool_use("", "t1", "{}", true);
        assert!(evs.is_empty(), "缺 name 的新 tool 帧应被丢弃");
        assert!(!t.has_tool_use, "丢弃的 tool 不应置 has_tool_use");
        assert!(!t.produced_any_content());
    }

    #[test]
    fn late_reasoning_after_text_then_tool_is_dropped() {
        // 回归:tool 关闭 text 后 text_index 置 None;迟到 native reasoning 不得借此重开
        // thinking 块(违反 thinking 在前)。drop-guard 必须用单调的 text_opened/has_tool_use。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_text("正文");
        t.on_tool_use("f", "t1", "{}", true); // 关闭 text(text_index→None)+ 输出并 stop tool
        let late = t.on_reasoning("迟到思考", None);
        assert!(
            late.is_empty(),
            "text/tool 之后迟到的 reasoning 应丢弃,不得重开 thinking 块: {:?}",
            tags(&late)
        );
    }

    #[test]
    fn interleaved_tool_frames_stay_valid_sse_but_truncate() {
        // Kiro 实际按 stop 分隔顺序流式工具。若上游异常交错 t1→t2→t1,我方保证 SSE 合法
        // (开 t2 前强关 t1),代价=被强关的 t1 续帧丢弃(JSON 截断)。锁定此取舍。
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let mut all = Vec::new();
        all.extend(t.on_tool_use("f1", "t1", "{\"a\":", false));
        all.extend(t.on_tool_use("f2", "t2", "{}", false)); // 强关 t1
        let late = t.on_tool_use("f1", "t1", "1}", true); // t1 已被强关 → 忽略
        assert!(late.is_empty(), "被强关的 t1 续帧应忽略,保持 SSE 合法");
        let seq = tags(&all);
        assert_eq!(
            seq,
            vec![
                "content_block_start:tool_use",        // t1
                "content_block_delta:input_json_delta",
                "content_block_stop",                  // t1 因 t2 到来被强关
                "content_block_start:tool_use",        // t2
                "content_block_delta:input_json_delta",
            ],
            "交错工具帧不得产生交错块"
        );
    }

    #[test]
    fn inline_thinking_end_tag_before_tool_no_tag_leak() {
        // M-1:</thinking> 无 \n\n 收尾即遇 tool_use,flush 必须识别结束标签,
        // 不能把 </thinking> 当 thinking 内容泄漏。
        let mut t = BlockTracker::new("claude-sonnet-4-5".to_string(), true);
        let mut all = Vec::new();
        all.extend(t.on_text("<thinking>\nabc</thinking>"));
        all.extend(t.on_tool_use("f", "t1", "{}", true));
        let thinking: String = all
            .iter()
            .filter(|e| tag(e) == "content_block_delta:thinking_delta")
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(thinking, "abc", "thinking 内容不应含泄漏的 </thinking> 标签");
        let seq = tags(&all);
        let th = seq.iter().position(|s| s == "content_block_start:thinking");
        let tl = seq.iter().position(|s| s == "content_block_start:tool_use");
        assert!(th.is_some() && tl.is_some() && th < tl, "thinking 块应在 tool 块前: {seq:?}");
    }

    #[test]
    fn inline_pending_text_flushed_before_tool_use() {
        // 非 Opus + thinking:inline 解析器缓冲的正文必须在 tool_use 块之前吐出,
        // 不能等到 finish() 才补在 tool_use 之后(块顺序倒置)。
        let mut t = BlockTracker::new("claude-sonnet-4-5".to_string(), true);
        let mut all = Vec::new();
        all.extend(t.on_text("答案")); // 短于 <thinking> 窗口 → 全缓冲,无事件
        assert!(all.is_empty(), "短正文应被 inline 解析器缓冲");
        all.extend(t.on_tool_use("f", "t1", "{}", true));
        let seq = tags(&all);
        let text_start = seq.iter().position(|s| s == "content_block_start:text");
        let tool_start = seq.iter().position(|s| s == "content_block_start:tool_use");
        assert!(
            text_start.is_some() && tool_start.is_some() && text_start < tool_start,
            "缓冲正文应在 tool_use 块前: {seq:?}"
        );
    }

    // ---- ⑤D 空响应判定 ----

    #[test]
    fn produced_any_content_false_for_untouched_tracker() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        let _ = t.finish();
        assert!(!t.produced_any_content());
    }

    #[test]
    fn produced_any_content_true_after_tool_use_only() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), false);
        t.on_tool_use("f", "t1", "", false);
        assert!(t.produced_any_content());
    }

    // ---- E thinking-only → max_tokens 兜底 ----

    #[test]
    fn thinking_only_stream_appends_space_text_and_forces_max_tokens() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), true);
        let mut all = Vec::new();
        all.extend(t.on_reasoning("只想不说", None));
        all.extend(t.finish());
        assert!(t.thinking_only_max_tokens, "纯 thinking 流应强制 max_tokens");
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
            "thinking-only 应补一个完整的空格 text 块"
        );
        let text_delta = all
            .iter()
            .find(|e| tag(e) == "content_block_delta:text_delta")
            .unwrap();
        assert_eq!(text_delta.data["delta"]["text"], " ");
    }

    #[test]
    fn text_present_does_not_force_max_tokens() {
        let mut t = BlockTracker::new("claude-opus-4-8".to_string(), true);
        t.on_reasoning("思考", None);
        t.on_text("回答");
        let _ = t.finish();
        assert!(!t.thinking_only_max_tokens);
    }

    // ---- 流级测试:eventstream 字节流 → SSE 全链 ----

    /// 构造一个 CRC 正确的 AWS eventstream 帧(string 类型 header)。
    fn es_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        use crate::parser::crc::crc32;
        let mut headers_raw = Vec::new();
        for (name, val) in headers {
            headers_raw.push(name.len() as u8);
            headers_raw.extend_from_slice(name.as_bytes());
            headers_raw.push(7u8); // String
            headers_raw.extend_from_slice(&(val.len() as u16).to_be_bytes());
            headers_raw.extend_from_slice(val.as_bytes());
        }
        let header_len = headers_raw.len() as u32;
        let total_len = (12 + headers_raw.len() + payload.len() + 4) as u32;
        let mut prelude8 = Vec::new();
        prelude8.extend_from_slice(&total_len.to_be_bytes());
        prelude8.extend_from_slice(&header_len.to_be_bytes());
        let prelude_crc = crc32(&prelude8);
        let mut msg = Vec::new();
        msg.extend_from_slice(&prelude8);
        msg.extend_from_slice(&prelude_crc.to_be_bytes());
        msg.extend_from_slice(&headers_raw);
        msg.extend_from_slice(payload);
        let msg_crc = crc32(&msg);
        msg.extend_from_slice(&msg_crc.to_be_bytes());
        msg
    }

    /// 把若干帧字节作为上游流跑完整 async_stream_like,收集所有产物。
    async fn run_stream(
        frames: Vec<Vec<u8>>,
        thinking: bool,
    ) -> Vec<Result<StreamItem, UpstreamError>> {
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
            frames.into_iter().map(|f| Ok(bytes::Bytes::from(f))).collect();
        let byte_stream = futures::stream::iter(chunks);
        let s = async_stream_like(
            byte_stream,
            "claude-opus-4-8".to_string(),
            thinking,
            (0, 100),
            crate::CacheBilling::default(),
            HashMap::new(),
        );
        futures::StreamExt::collect::<Vec<_>>(s).await
    }

    fn sse_events(items: &[Result<StreamItem, UpstreamError>]) -> Vec<SseEvent> {
        items
            .iter()
            .filter_map(|i| match i {
                Ok(StreamItem::Sse(e)) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn empty_upstream_yields_empty_response_error_without_normal_finale() {
        let items = run_stream(vec![], false).await;
        let evs = sse_events(&items);
        assert!(evs.iter().any(|e| e.event == "message_start"));
        assert!(
            !evs.iter().any(|e| e.event == "message_delta"),
            "空响应不应发 message_delta"
        );
        assert!(
            !evs.iter().any(|e| e.event == "message_stop"),
            "空响应不应发 message_stop"
        );
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::Usage(_)))),
            "空响应不应发 Usage"
        );
        let err = items.iter().find_map(|i| i.as_ref().err()).expect("应有终态 Err");
        assert_eq!(err.kind, UpstreamErrorKind::EmptyResponse);
    }

    #[tokio::test]
    async fn content_length_exceeded_finishes_as_max_tokens_not_error() {
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            br#"{"content":"partial output"}"#,
        );
        // 异常帧:无 :event-type,只有 :message-type=exception + :exception-type。
        let f2 = es_frame(
            &[(":message-type", "exception"), (":exception-type", "ContentLengthExceededException")],
            br#"{"message":"Input is too long"}"#,
        );
        let items = run_stream(vec![f1, f2], false).await;
        assert!(
            items.iter().all(|i| i.is_ok()),
            "ContentLengthExceeded 是正常 max_tokens 截断,不应作为流错误 abort"
        );
        let evs = sse_events(&items);
        let delta = evs.iter().find(|e| e.event == "message_delta").expect("应正常收尾");
        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
        assert!(evs.iter().any(|e| e.event == "message_stop"));
    }

    #[tokio::test]
    async fn other_exception_still_aborts_as_error() {
        let f = es_frame(
            &[(":message-type", "exception"), (":exception-type", "InternalServerException")],
            br#"{"message":"boom"}"#,
        );
        let items = run_stream(vec![f], false).await;
        let err = items
            .iter()
            .find_map(|i| i.as_ref().err())
            .expect("非 ContentLength 异常应 abort");
        assert_eq!(err.kind, UpstreamErrorKind::ServerError);
        let evs = sse_events(&items);
        assert!(!evs.iter().any(|e| e.event == "message_stop"));
    }

    #[tokio::test]
    async fn thinking_only_with_context_exceeded_keeps_context_stop_reason() {
        // 纯 thinking 且 contextUsage 100%:显式 model_context_window_exceeded 优先,
        // thinking-only 兜底不得把它覆盖成 max_tokens(显式 stop_reason 优先契约)。
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "reasoningContentEvent")],
            "{\"text\":\"只在思考\"}".as_bytes(),
        );
        let f2 = es_frame(
            &[(":message-type", "event"), (":event-type", "contextUsageEvent")],
            br#"{"contextUsagePercentage":100.0}"#,
        );
        let items = run_stream(vec![f1, f2], true).await;
        let evs = sse_events(&items);
        let delta = evs.iter().find(|e| e.event == "message_delta").expect("应正常收尾");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "model_context_window_exceeded",
            "context 超限的显式 stop_reason 不应被 thinking-only 兜底覆盖"
        );
    }

    #[tokio::test]
    async fn open_block_then_other_exception_emits_no_normal_finale() {
        // #6 契约锁定:thinking 块开着时遇非 ContentLength 异常 → 终态 Err,
        // 不补 message_delta/message_stop/Usage(clean-error,不把失败流伪装成完整消息)。
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "reasoningContentEvent")],
            "{\"text\":\"思考中\"}".as_bytes(),
        );
        let f2 = es_frame(
            &[(":message-type", "exception"), (":exception-type", "InternalServerException")],
            br#"{"message":"boom"}"#,
        );
        let items = run_stream(vec![f1, f2], true).await;
        let evs = sse_events(&items);
        assert!(!evs.iter().any(|e| e.event == "message_delta"), "异常后不应补 message_delta");
        assert!(!evs.iter().any(|e| e.event == "message_stop"), "异常后不应补 message_stop");
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::Usage(_)))),
            "异常后不应补 Usage"
        );
        let err = items.iter().find_map(|i| i.as_ref().err()).expect("应有终态 Err");
        assert_eq!(err.kind, UpstreamErrorKind::ServerError);
    }

    #[tokio::test]
    async fn tool_use_event_end_to_end_sets_stop_reason_tool_use() {
        let f = es_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            br#"{"name":"get_weather","toolUseId":"t1","input":"{\"city\":\"sf\"}","stop":true}"#,
        );
        let items = run_stream(vec![f], false).await;
        let evs = sse_events(&items);
        assert!(
            evs.iter().any(|e| e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"),
            "toolUseEvent 应转成 tool_use 块"
        );
        let delta = evs.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");
        assert!(items.iter().any(|i| matches!(i, Ok(StreamItem::Usage(_)))));
    }

    // ---- upstream_cut(静默掐流前兆)发射条件 ----

    #[tokio::test]
    async fn silent_cut_after_payload_emits_upstream_cut_once() {
        // 掐流形态:见过真实上游 payload,但无终止事件就 EOF。
        let f = es_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            r#"{"content":"半截回答"}"#.as_bytes(),
        );
        let items = run_stream(vec![f], false).await;
        let cuts = items
            .iter()
            .filter(|i| matches!(i, Ok(StreamItem::UpstreamCut)))
            .count();
        assert_eq!(cuts, 1, "掐流应恰好上报一次 UpstreamCut");
        // UpstreamCut 在合成收尾之前发出;客户端侧 finale 仍完整(行为不变)。
        let pos_cut = items
            .iter()
            .position(|i| matches!(i, Ok(StreamItem::UpstreamCut)))
            .unwrap();
        let pos_stop = items
            .iter()
            .position(|i| matches!(i, Ok(StreamItem::Sse(e)) if e.event == "message_stop"))
            .expect("掐流后仍合成 message_stop 收尾");
        assert!(pos_cut < pos_stop, "UpstreamCut 必须先于合成收尾");
    }

    #[tokio::test]
    async fn normal_finale_with_metadata_event_emits_no_cut() {
        // 正常收尾:metadataEvent(带 stopReason)是终止信号,不算掐流。
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            r#"{"content":"完整回答"}"#.as_bytes(),
        );
        let f2 = es_frame(
            &[(":message-type", "event"), (":event-type", "metadataEvent")],
            br#"{"stopReason":"end_turn"}"#,
        );
        let items = run_stream(vec![f1, f2], false).await;
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::UpstreamCut))),
            "有终止事件的正常收尾不得报 UpstreamCut"
        );
        let evs = sse_events(&items);
        assert!(evs.iter().any(|e| e.event == "message_stop"));
    }

    #[tokio::test]
    async fn metering_event_marks_terminal_no_cut() {
        // meteringEvent 是流末尾的原生计费帧:见过 = 上游完整跑完,不算掐流。
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            r#"{"content":"完整回答"}"#.as_bytes(),
        );
        let f2 = es_frame(
            &[(":message-type", "event"), (":event-type", "meteringEvent")],
            br#"{"usage":1.5,"unit":"credit"}"#,
        );
        let items = run_stream(vec![f1, f2], false).await;
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::UpstreamCut))),
            "meteringEvent 已属终止信号"
        );
    }

    #[tokio::test]
    async fn explicit_stop_condition_emits_no_cut() {
        // ContentLengthExceeded = 上游显式声明的终止条件(max_tokens),非掐流。
        let f1 = es_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            br#"{"content":"partial output"}"#,
        );
        let f2 = es_frame(
            &[(":message-type", "exception"), (":exception-type", "ContentLengthExceededException")],
            br#"{"message":"Input is too long"}"#,
        );
        let items = run_stream(vec![f1, f2], false).await;
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::UpstreamCut))),
            "显式 stop 条件的截断不得报 UpstreamCut"
        );
    }

    #[tokio::test]
    async fn empty_upstream_emits_no_cut() {
        // 零 payload(连合成首帧外的真实上游帧都没有)→ 空响应,不是掐流:
        // 上游可能压根没接受请求,不能记到封号前兆上(评审#3)。
        let items = run_stream(vec![], false).await;
        assert!(
            !items.iter().any(|i| matches!(i, Ok(StreamItem::UpstreamCut))),
            "无真实 payload 不算掐流"
        );
    }
}
