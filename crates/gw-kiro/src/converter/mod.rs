//! Anthropic → Kiro 协议转换器(入口 + 共享类型/常量)。
//!
//! 🔵 搬运自旧 `src/anthropic/converter.rs`(3214 行单文件),按职责拆分为子模块:
//! - [`model_map`] 模型映射  - [`session`] conversationId 派生  - [`content`] 内容抽取
//! - [`tools`] 工具转换  - [`pairing`] tool 配对清理  - [`history`] 历史构建
//! - [`cache_point`] cachePoint 实验开关(dormant)
//! 本文件保留入口 `convert_request`、`ConversionResult/Error` 与跨模块共享常量。
#![allow(clippy::doc_lazy_continuation)] // 迁移注释原样保留,不重排

use std::collections::HashMap;

use crate::anthropic_types::{ContentBlock, MessagesRequest};
use crate::kiro_types::conversation::{
    CachePoint, ClientCacheConfig, ConversationState, CurrentMessage, Message,
    UserInputMessage, UserInputMessageContext,
};

mod cache_point;
mod content;
mod document_name;
mod history;
mod model_map;
mod normalize;
mod pairing;
mod session;
mod shed;
mod tool_id;
mod tools;

// 重导出子模块项,使本文件(及测试)无需逐一限定路径即可调用。
pub use model_map::{
    advertised_models, clamp_effort_for_model, effort_drift, get_context_window_size, map_model,
    AdvertisedModel, EffortDrift,
};
pub use shed::{shed_history_media, MediaShed};
/// 实验开关热应用入口(供 [`crate::KiroProvider::apply_hot_settings`] 调用)。
pub(crate) use cache_point::set_experimental_flags;
/// thinking 签名发射开关(供 [`crate::chat`] 在收尾 thinking 块时判定是否附 signature)。
pub(crate) use cache_point::thinking_signature_enabled;
/// 上游端点开关(供 [`crate::headers::runtime_base_url`] 判定走 runtime.kiro.dev 还是 q.amazonaws.com)。
pub(crate) use cache_point::q_endpoint_enabled;
/// 历史 thinking 保留轮数热应用入口(供 [`crate::KiroProvider::apply_hot_settings`] 调用)。
pub(crate) use history::set_history_thinking_turns;
use cache_point::*;
use content::*;
use document_name::dedup_document_names;
use history::*;
use pairing::*;
use session::*;
use tool_id::rewrite_duplicate_tool_use_ids;
use tools::*;
// normalize 的项在本文件以显式路径调用;glob 仅给测试模块用(避免非测试构建 unused 警告)。
#[cfg(test)]
use normalize::*;

/// 空内容占位符。Kiro 要求 message.content 非空(否则 400 Improperly formed request)。
/// 纯 tool_use 回合或上游空响应残留时,用单空格兜底保持 schema 合法。
pub(crate) const EMPTY_CONTENT_PLACEHOLDER: &str = " ";

/// 媒体附件(image/document)无文本时的占位引导语(实测纯 PDF/纯图无文本 → Kiro 400)。
pub(crate) const MEDIA_ONLY_PLACEHOLDER: &str = "Please analyze the attached file.";

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 工具防御性修复字段表：发往上游的工具短名 → 该工具 input_schema 中 type 为
    /// array/object 的顶层字段集合。供流式收尾解包被模型双重编码成字符串的参数
    /// （如 AskUserQuestion.questions）。无 array/object 字段的工具不入表。
    pub tool_repair_fields: HashMap<String, std::collections::HashSet<String>>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
        }
    }
}

impl std::error::Error for ConversionError {}

/// conversationId 派生的 system 锚点字节数。
///
/// system 折叠块从前到后混了三类内容:① agent 角色(开头)② 机器/用户特征(CLAUDE.md
/// 路径,char ~2500)③ 滚动参数(`Today's date`,char ~57400)。取前 8192 字节恰覆盖 ①②
/// (区分 agent 与用户)、又远在 ③ 之前(不被日期误拆)。实测 1024 会误并不同用户、全文会
/// 跨午夜误拆健康会话;4096–8192 是 false-merge/false-split 同时最低的区间。
const SYSTEM_ANCHOR_PREFIX_LEN: usize = 8192;

/// 从 Anthropic 请求派生稳定 conversationId(v55:metadata.user_id 优先,否则
/// system 锚点 + 前2条 user 哈希)。**单一实现**:`convert_request` 与 worker 会话亲和
/// 都走它,保证 router/cache_sim/账号亲和三处身份链同源(审查 #131③)。
///
/// `messages` 应为 prefill 预处理后的切片(末尾 user);body 级入口见 [`affinity_key_from_body`]。
/// `scope` = 账号作用域盐(见 [`derive_conversation_id_from_messages`] 的长注释)。
/// **调度亲和键传空串**,发包时传 `account_id`。
pub fn derive_conversation_id(
    req: &MessagesRequest,
    messages: &[crate::anthropic_types::Message],
    scope: &str,
) -> String {
    let from_metadata = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id));
    match from_metadata {
        // ⚠️ **metadata 那条路也必须加盐。** 那时 ID 是客户端 session id 的**原文**,
        // 不加盐等于把「同一个客户端会话」这件事在多个 AWS 账号之间明文关联起来 ——
        // 这是三条路里最强的池化特征。加盐后仍然「同号恒等」,亲和与缓存都不受影响
        // (调度键走的是 scope 为空的那条,见 `affinity_key_from_body`)。
        Some(sid) if !scope.is_empty() => scope_session_id(&sid, scope),
        Some(sid) => sid,
        None => {
            let sys_full = normalized_client_system(req);
            let sys_anchor = safe_prefix(&sys_full, SYSTEM_ANCHOR_PREFIX_LEN);
            derive_conversation_id_from_messages(messages, sys_anchor, scope)
        }
    }
}

/// 把客户端给的 session id 按账号重新派生成 UUID 形态。
///
/// 输出必须仍然长得像 Kiro 客户端会发的值(UUID),所以走同一套「sha256 前 16 字节排成
/// UUID」的成型逻辑,而不是简单拼前缀 —— 拼前缀会让 ID 变成一个上游从没见过的形状。
fn scope_session_id(session_id: &str, scope: &str) -> String {
    session::uuid_from_parts(&[b"sid:", session_id.as_bytes(), b"acct:", scope.as_bytes()])
}

/// 会话亲和键入口:从原始 Anthropic body 派生 conversationId(供 worker 选号亲和)。
///
/// 与 `convert_request` 同口径:解析失败 / 模型不支持 / 空消息 → `None`(调用方退化为
/// 无亲和的 LRU 选号)。复刻 prefill 截断,使派生出的 key 与真正发包时的 conversationId 一致。
pub fn affinity_key_from_body(body: &serde_json::Value) -> Option<String> {
    let req: MessagesRequest = serde_json::from_value(body.clone()).ok()?;
    if req.messages.is_empty() {
        return None;
    }
    let messages: &[_] = if req.messages.last().is_some_and(|m| m.role != "user") {
        let last_user_idx = req.messages.iter().rposition(|m| m.role == "user")?;
        &req.messages[..=last_user_idx]
    } else {
        &req.messages
    };
    // ⚠️ **亲和键不加盐**:它是调度层的 key(选号、缓存模拟、钉扎),必须只依赖请求内容 ——
    // worker 是**先算这个键、再选号**的,这里拿不到也不该拿到账号。
    // 账号盐只作用于真正发给上游的 conversationId(见 `convert_request` 的 `scope`)。
    Some(derive_conversation_id(&req, messages, ""))
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(
    req: &MessagesRequest,
    // 账号作用域盐(通常是 `account_id`)。空串 = 不加盐(仅测试/亲和键口径)。
    // 见 `session::derive_conversation_id_from_messages`:换号必须换上游 ID,否则同一个
    // conversationId 会横跨多个 AWS 账号,那是账号池最强的特征之一。
    scope: &str,
) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model)
        .ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. 预处理 prefill：如果末尾是 assistant，静默丢弃并截断到最后一条 user
    // Claude 4.x 已弃用 assistant prefill，Kiro API 也不支持
    let messages: &[_] = if req.messages.last().is_some_and(|m| m.role != "user") {
        tracing::info!("检测到末尾 assistant 消息（prefill），静默丢弃");
        let last_user_idx = req
            .messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        &req.messages[..=last_user_idx]
    } else {
        &req.messages
    };

    // 3. 生成会话 ID 和代理 ID。conversationId 派生逻辑抽到 [`derive_conversation_id`]
    //    (worker 会话亲和也复用它,保证 router/sim/亲和三处身份链同源)。
    let conversation_id = derive_conversation_id(req, messages, scope);
    // 【2026-06-15 修正·真实缓存全 miss 根因再定位】
    // 2026-06-13 曾删除 agentContinuationId,理由"kiro.rs/static_flow 都不发、发了破缓存"。
    // 复核证伪:kiro.rs **生产一直在发**稳定的 agentContinuationId(converter.rs:670,实测注释
    // 稳定值 → metering 降 ~36%),且 caio 自身 2026-05-24 命中 A/B 基线**也含**此字段(6-13 才删)。
    // 删除后线上实测 caio 真实命中 **0%**、credit **+49% vs kiro.rs**(后者同上游 ~43% 命中)。
    // 故恢复**稳定**派生,经 `agent_continuation_enabled()` 开关上 wire(默认关,生产做可逆 A/B):
    // 稳定 conversationId(本 crate 已保证)+ 稳定 agentContinuationId = kiro.rs proven 配置。
    // 实际附挂在 `with_agent_continuation_metadata`(纯函数,便于直接测两个分支)。

    // 3.1 跨轮重复 tool_use_id 重写(🟢 static_flow)。客户端反复 auto-compact 后,同一
    // tool_use_id 可能在两个各自已完成的 assistant 轮里各出现一次,直发会让 Kiro 400。
    // **必须在 conversationId 派生之后**:身份哈希走原始 messages,与 worker 的
    // `affinity_key_from_body`(同样基于原始 body)同源;改写只作用于发往 Kiro 的 wire 报文。
    // 无重复时返回 None,继续用原 borrowed slice(零拷贝)。畸形输入不报错,交 pairing 兜底。
    let deduped = rewrite_duplicate_tool_use_ids(messages);
    let messages: &[_] = deduped.as_deref().unwrap_or(messages);

    // 3.2 文档名去重 + 净化:Bedrock 要求 document name 全局唯一且限定字符集,否则 400
    // INVALID_DOCUMENT_NAME(duplicate document names)。多份无名附件都兜成 "document" 是
    // 最常见触发。零拷贝快路径,时序与 tool_id 一致(conversationId 派生之后,不扰动身份/亲和/缓存)。
    let doc_deduped = dedup_document_names(messages);
    let messages: &[_] = doc_deduped.as_deref().unwrap_or(messages);

    // 3.5 块1a:处理 messages 数组里 role=="system" 的消息(代理链中段注入)。
    // 三级分流:稳定前缀提升进 promoted_system / 动态噪声丢弃 / interrupted-user 与未知转 user。
    // **必须在 conversationId 派生之后**:身份哈希走 pre-routing 的 top-level system(见上),
    // 路由产物不参与身份哈希,故 conversationId 跨轮稳定、零迁移。
    // 无 role=system 消息时返回 None,继续用原 borrowed slice(零拷贝)。
    let routed = normalize::route_system_role_messages(messages);
    let (messages, promoted_system): (&[_], &[String]) = match &routed {
        Some(r) => (&r.messages, &r.promoted_system),
        None => (messages, &[]),
    };

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 锚定当前轮范围:尾部连续 user 消息整体作为 current turn(🟢 static_flow)。
    // 旧逻辑只取最后一条,会把代理拆分的同一轮前半截误归 history 毒化缓存前缀。
    // messages 经 prefill 预处理末尾必为 user,current_user_message_range 必返 Some。
    let current_range = current_user_message_range(messages)
        .ok_or(ConversionError::EmptyMessages)?;
    let current_messages = &messages[current_range.clone()];
    let history_messages = &messages[..current_range.start];

    // 合并当前轮的多条连续 user 消息(text/tool_result 分条发等)为单一内容。
    let (text_content, images, documents, tool_results) =
        merge_current_message_content(current_messages)?;

    // 6. 转换工具定义（超长名称自动缩短并记录映射；同时按 schema 收集需修复的 array/object 字段）
    let mut tool_name_map = HashMap::new();
    let mut tool_repair_fields: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut tools = convert_tools(&req.tools, &mut tool_name_map, &mut tool_repair_fields);

    // 7. 构建历史消息（当前轮之前的所有消息;需要先构建,以便收集历史中使用的工具）
    // promoted_system: 块1a 从 messages 数组提升上来的稳定 system 文本,折叠进 history[0]。
    let mut history = build_history(req, history_messages, &model_id, promoted_system, &mut tool_name_map)?;

    // 8. 验证并过滤 tool_use/tool_result 配对
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 9. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 9.5 从历史中移除孤立的 tool_result（有结果但找不到对应的 tool_use）
    // 客户端（如 Claude Code）反复 auto-compact 长对话时，可能压掉发起 tool_use 的
    // assistant 消息却保留其 tool_result，残留成埋在 history 中段的孤儿。
    // 上游 Kiro 对这种 result-without-use 同样返回 400 Improperly formed request，
    // 这里反向清理：删掉 history 里任何 tool_use_id 无对应 tool_use 的 tool_result。
    remove_orphaned_tool_results(&mut history);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if !existing_tool_names.contains(&tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 10.5 多模态 schema 兼容(🟢 static_flow):请求(当前轮或历史)含图片时,Kiro 会拒绝带
    // anyOf/oneOf/$defs 等复杂关键字的工具 schema(400 Improperly formed request)。把这类 schema
    // 整体降级为宽松 object schema;无图片则原样不动。在工具放置前做,current/history 放置都覆盖。
    // 用**转换后**的图判定(caio 丢弃 url/file 图、只留 base64 上 wire):current=已转 `images`,
    // history=已转 KiroImage。避免按原始报文误判(带 url 图但 wire 无图却误降级,审查 Skeptic#2)。
    let has_images = !images.is_empty()
        || history
            .iter()
            .any(|m| matches!(m, Message::User(u) if !u.user_input_message.images.is_empty()));
    apply_multimodal_tool_schema_compatibility(&mut tools, has_images);

    // 11. 构建 UserInputMessageContext
    // 记录是否有工具结果（validated_tool_results 随后会被移动进 context，这里先存一份布尔量
    // 供后续当前消息的空内容兜底判断使用）。
    let has_tool_results = !validated_tool_results.is_empty();
    // 工具放置策略：
    // - 默认：放 currentMessage（每轮全价重发，无法缓存）
    // - 实验开关开启且有历史用户消息：放 history[0] 前缀，进可缓存区
    let place_in_history = tools_in_prefix_enabled()
        && !tools.is_empty()
        && matches!(history.first(), Some(Message::User(_)));

    let mut context = UserInputMessageContext::new();
    if place_in_history {
        if let Some(Message::User(h)) = history.first_mut() {
            h.user_input_message.user_input_message_context.tools = std::mem::take(&mut tools);
            tracing::info!(
                "实验[tools_in_prefix]: {} 个工具已放入 history[0] 前缀（尝试命中缓存）",
                h.user_input_message.user_input_message_context.tools.len()
            );
        }
    } else if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息（content 兜底）
    // Kiro 的 content 非空约束分两种情形：
    //   - 带 image/document 但无文本 → **必须**补非空 content（实测纯 PDF/纯图无文本 → 400，
    //     见 MEDIA_ONLY_PLACEHOLDER）。补一句引导语，让模型明确分析附件。
    //   - text、tool_results、images、documents 全空 → 补单空格（纯空回合，客户端异常）。
    //   - 无文本但有 tool_results（且无媒体）→ 正常工具结果回合，Kiro 接受空文本，不补（否则污染）。
    let content = if !text_content.trim().is_empty() {
        text_content
    } else if !images.is_empty() || !documents.is_empty() {
        tracing::warn!(
            "当前 user 消息带媒体（image/document）但无文本，已补引导语占位以避免 Kiro 400"
        );
        MEDIA_ONLY_PLACEHOLDER.to_string()
    } else if !has_tool_results {
        tracing::warn!(
            "当前 user 消息为空（无 text/tool_result/image/document），已用占位符兜底以避免 Kiro 400"
        );
        EMPTY_CONTENT_PLACEHOLDER.to_string()
    } else {
        // 仅有 tool_results（无媒体、无文本）：正常工具结果回合，保留空文本。
        text_content
    };

    // 块2b:thinking 前缀注入**当前轮** content(不进 system/history,避免毒化缓存前缀)。
    // 在兜底之后注入:即便当前轮是纯 tool_result/媒体占位,带 thinking 时也应让上游开思考。
    let mut content = content;
    // [EXP] 若已把 thinking 注入 history[0],当前轮不再重复注入
    if std::env::var("KIRO_THINKING_IN_HISTORY0").is_err() {
        apply_thinking_prefix_to_current_turn(req, &mut content);
    }

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    if !documents.is_empty() {
        user_input = user_input.with_documents(documents);
    }

    // 实验：翻译 cache_control → cachePoint。env flag 控制，默认关闭。
    // 当前轮可能含多条连续 user 消息,任一条带 cache_control 即视为命中。
    let want_cache_point = cache_point_enabled()
        && current_messages
            .iter()
            .any(|m| anthropic_message_has_cache_control(&m.content));
    if want_cache_point && cache_point_placement() == "current" {
        user_input.cache_point = Some(CachePoint::with_type(cache_point_type()));
        if cache_point_with_config() {
            user_input.client_cache_config = Some(ClientCacheConfig::default());
        }
        tracing::info!(
            "实验[cache_point]: currentMessage 打 cachePoint type={} config={}",
            cache_point_type(),
            cache_point_with_config()
        );
    }

    let current_message = CurrentMessage::new(user_input);

    // 实验变体：把 cachePoint 打在 history 最后一条 user 上（前缀缓存语义）。
    if want_cache_point && cache_point_placement() == "history" {
        if let Some(Message::User(h)) = history
            .iter_mut()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
        {
            h.user_input_message.cache_point = Some(CachePoint::with_type(cache_point_type()));
            if cache_point_with_config() {
                h.user_input_message.client_cache_config = Some(ClientCacheConfig::default());
            }
            tracing::info!(
                "实验[cache_point]: history 末尾 user 打 cachePoint type={} config={}",
                cache_point_type(),
                cache_point_with_config()
            );
        }
    }

    // 13. 构建 ConversationState。chat_trigger_type=MANUAL 恒发;agentContinuationId + agentTaskType
    // 仅在开关启用时上 wire(默认关,见上方根因注释 + `agent_continuation_enabled`)。
    let conversation_state = with_agent_continuation_metadata(
        ConversationState::new(conversation_id)
            .with_chat_trigger_type(chat_trigger_type)
            .with_current_message(current_message)
            .with_history(history),
        agent_continuation_enabled(),
    );

    if !tool_name_map.is_empty() {
        tracing::info!(
            "工具名称映射: {} 个超长名称已缩短",
            tool_name_map.len()
        );
    }

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        tool_repair_fields,
    })
}

/// 按开关把**稳定** agentContinuationId(从 `state.conversation_id` 派生,与 conversationId 同源)
/// + agentTaskType="vibe" 附到 conversationState —— 复刻 kiro.rs proven 配置(见根因注释)。
///
/// **纯函数**(开关值由调用方传入,便于直接测两个分支,不依赖进程级实验全局):
/// - `enabled=false`(默认):原样返回,两字段保持 None → 序列化时 **完全省略**(零 wire 变化);
/// - `enabled=true`:附 agentContinuationId(稳定)+ agentTaskType="vibe"。
fn with_agent_continuation_metadata(state: ConversationState, enabled: bool) -> ConversationState {
    if !enabled {
        return state;
    }
    let acid = derive_agent_continuation_id(&state.conversation_id);
    state
        .with_agent_continuation_id(acid)
        .with_agent_task_type("vibe")
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}


#[cfg(test)]
mod tests;
