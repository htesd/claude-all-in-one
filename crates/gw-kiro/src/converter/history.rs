//! 历史构建、user/assistant 合并、thinking 前缀与结构化输出指令。

use std::collections::HashMap;
use super::{ContentBlock, ConversionError, MessagesRequest, EMPTY_CONTENT_PLACEHOLDER, MEDIA_ONLY_PLACEHOLDER};
// 跨子模块调用(经 mod.rs 的 `use <sub>::*` 提升到 converter 根,故走 super::)
use super::{normalized_client_system, request_has_chunked_tools, process_message_content, map_tool_name};
use crate::kiro_types::conversation::{AssistantMessage, HistoryAssistantMessage, HistoryUserMessage, Message, UserInputMessageContext, UserMessage};
use crate::kiro_types::tool::ToolUseEntry;

/// 生成thinking标签前缀
pub(super) fn generate_thinking_prefix(req: &MessagesRequest) -> Option<String> {
    if let Some(t) = &req.thinking {
        if t.thinking_type == "enabled" {
            return Some(format!(
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                t.budget_tokens
            ));
        } else if t.thinking_type == "adaptive" {
            let effort = req
                .output_config
                .as_ref()
                .map(|c| c.effort.as_str())
                .unwrap_or("high");
            return Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort>",
                effort
            ));
        }
    }
    None
}

/// 检查内容是否已包含thinking标签
pub(super) fn has_thinking_tags(content: &str) -> bool {
    content.contains("<thinking_mode>")
        || content.contains("<max_thinking_length>")
        || content.contains("<thinking_effort>")
}

/// 块2b:把 thinking 前缀注入**当前轮** user content 前面(不进 system/history)。
///
/// 🟢 借鉴 static_flow `thinking.rs:apply_thinking_prefix_to_current_turn` + 🔵 我方注入模板。
/// 动机:thinking 的 budget/effort 是高波动参数,放进 system 折叠块(history[0])会让
/// 缓存前缀随每轮 thinking 配置抖动 → 毒化 Kiro prefix cache。只注入当前轮则前缀恒定。
/// 我方 conversationId 锚点本就只取 client_system(不含 thinking),与此一致(零锚点抖动)。
///
/// `has_thinking_tags` 守卫:content 已含标签则不重复注入。空 content 直接用前缀。
pub(super) fn apply_thinking_prefix_to_current_turn(req: &MessagesRequest, content: &mut String) {
    let Some(prefix) = generate_thinking_prefix(req) else {
        return;
    };
    if has_thinking_tags(content) {
        return;
    }
    *content = if content.is_empty() {
        prefix
    } else {
        format!("{prefix}\n{content}")
    };
}

/// 生成结构化输出指令（当客户端请求 json_schema 输出时）。
///
/// Kiro 上游无原生 response_format 字段，改用 system 指令约束模型只输出
/// 严格符合 schema 的 JSON。强模型（Opus）遵从度高。该指令仅在 thinking
/// 未启用时注入（与 thinking 互斥，已在 handlers 层保证）。
pub(super) fn structured_output_instruction(req: &MessagesRequest) -> Option<String> {
    let schema = req.output_config.as_ref()?.json_schema()?;
    let schema_str = serde_json::to_string(schema).ok()?;
    Some(format!(
        "You must respond with ONLY a single JSON value that strictly conforms to this JSON Schema. \
         Do not include any explanatory text, markdown code fences, or prose before or after the JSON. \
         Output the raw JSON object only.\n\nJSON Schema:\n{}",
        schema_str
    ))
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - **当前轮之前**的历史消息切片(由 convert_request 经
///   `current_user_message_range` 切出,即 `messages[..current_range.start]`)。
///   注意:本切片**不含**当前轮,故下方整段迭代、不再截掉末尾。
/// * `model_id` - 已映射的 Kiro 模型 ID
/// * `promoted_system` - 块1a 三级分流从 messages 数组提升上来的稳定 system 文本,
///   追加到 top-level system 之后一起折叠进 history[0]。
pub(super) fn build_history(req: &MessagesRequest, messages: &[crate::anthropic_types::Message], model_id: &str, promoted_system: &[String], tool_name_map: &mut HashMap<String, String>) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 结构化输出指令（客户端请求 json_schema 时；与 thinking 互斥）
    // 注意(块2b):thinking 前缀**不再**在此注入 system 折叠块,改为注入当前轮 user content
    // (见 convert_request 调 apply_thinking_prefix_to_current_turn),避免高波动的
    // budget/effort 放进 history[0] 毒化 prefix cache。
    let structured_instruction = structured_output_instruction(req);

    // 1. 处理系统消息
    // 先归一化出客户端系统提示文本（无 system 或空 system 都视为空串）。
    // 注意：deserialize_system 会把 `"system":""` 解析成 Some([""]), 故这里统一用
    // is_empty 判断，避免"空 system 但有结构化指令"时整块被跳过（漏注入）。
    // v55：抽成 normalized_client_system，与 conversationId 身份哈希同口径复用。
    // 块1a:追加三级分流提升上来的稳定 system 文本(SessionStart/身份行),再做 model identity 规范化。
    let mut client_system = normalized_client_system(req);
    for promoted in promoted_system {
        client_system = if client_system.is_empty() {
            promoted.clone()
        } else {
            format!("{}\n{}", client_system, promoted)
        };
    }
    if !client_system.is_empty() {
        client_system = super::normalize::normalize_model_identity(client_system, &req.model);
    }

    // 仅当有真实系统提示、或需要注入结构化输出指令时，才构建系统消息块。
    if !client_system.is_empty() || structured_instruction.is_some() {
        // 追加分块写入策略——仅当请求里确实带了 Write/Edit 工具、且有真实系统提示时才注入。
        // 该策略文案约束 Write/Edit 分块行为，对干净客户端注入会污染行为被检测识别。
        let mut system_content = client_system;
        if !system_content.is_empty() && request_has_chunked_tools(req) {
            system_content = format!("{}\n{}", system_content, SYSTEM_CHUNKED_POLICY);
        }

        // 追加结构化输出指令（如有）
        let final_content = if let Some(ref instr) = structured_instruction {
            if system_content.is_empty() {
                instr.clone()
            } else {
                format!("{}\n\n{}", system_content, instr)
            }
        } else {
            system_content
        };

        // 系统消息作为 user + assistant 配对
        let user_msg = HistoryUserMessage::new(final_content, model_id);
        history.push(Message::User(user_msg));

        let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
        history.push(Message::Assistant(assistant_msg));
    }

    // 2. 处理常规消息历史
    // messages 已由调用方切掉当前轮(尾部连续 user),此处整段迭代,不再截尾。
    // 收集并配对消息
    let mut user_buffer: Vec<&crate::anthropic_types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&crate::anthropic_types::Message> = Vec::new();

    for msg in messages {
        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
pub(super) fn merge_user_messages(
    messages: &[&crate::anthropic_types::Message],
    model_id: &str,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_documents = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, documents, tool_results) = process_message_content(&msg.content)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_documents.extend(documents);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 兜底（与当前消息同规则）：带 image/document 无文本必须补引导语（Kiro 400）；
    // 全空补单空格；仅 tool_results（无媒体）保留空文本。
    let content = if !content.trim().is_empty() {
        content
    } else if !all_images.is_empty() || !all_documents.is_empty() {
        tracing::warn!(
            "历史 user 消息带媒体（image/document）但无文本，已补引导语占位以避免 Kiro 400"
        );
        MEDIA_ONLY_PLACEHOLDER.to_string()
    } else if all_tool_results.is_empty() {
        tracing::warn!(
            "历史 user 消息为空（无 text/tool_result/image/document），已用占位符兜底以避免 Kiro 400"
        );
        EMPTY_CONTENT_PLACEHOLDER.to_string()
    } else {
        content
    };
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_documents.is_empty() {
        user_msg = user_msg.with_documents(all_documents);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
pub(super) fn convert_assistant_message(
    msg: &crate::anthropic_types::Message,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    // 注意：本函数仅用于**历史** assistant 消息（build_history 调用），不触及当前轮。
    // 历史里的 thinking 被刻意丢弃 —— 见下方 final_content 处的说明（缓存稳定性）。
    let mut thinking_content = String::new();
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                let mapped_name = map_tool_name(&name, tool_name_map);
                                tool_uses.push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    // 历史 assistant 内容构建 —— **刻意不拼接 thinking 块**。
    //
    // 根因（v49 修复）：Claude Code 等客户端会做 thinking 滚动裁剪——只在请求历史里
    // 保留最近几轮的 `<thinking>`，更早的 assistant 消息其 thinking 被移除。若我们原样
    // 透传，则同一条历史 assistant 消息会随对话推进从"带 thinking"变成"不带"，**内容
    // 跨轮抖动 → 打断 Kiro prefix cache → 该点之后全部缓存失效**。实测某 thinking 会话
    // 命中率因此被打到 0.24–0.36（健康会话 0.78）。
    //
    // 既然客户端本就在裁剪历史 thinking，我们干脆**统一丢弃所有历史 thinking**，让历史
    // 前缀跨轮恒定，缓存稳定。这只影响发给 Kiro 的"历史推理文本"（客户端已在裁），
    // **不影响当前轮的 thinking 能力**（当前轮不经本函数；响应侧 thinking_delta/签名照常）。
    //
    // 格式：仅 `text内容`（含 tool_use 时正文可空，用占位符）。thinking_content 仅用于
    // 下方的"是否曾有内容"判断，不进最终文本。
    let _ = &thinking_content; // 明示：thinking 已被刻意丢弃，不参与 final_content
    let final_content = if !text_content.is_empty() {
        text_content
    } else {
        // text 为空（thinking 已丢弃，不再作为兜底内容）。
        // - 有 tool_use：正常的"纯工具调用"回合，用空格占位（Kiro 要求 content 非空）。
        // - 无 tool_use：彻底空的 assistant 消息，几乎都是上游空响应/断流后被客户端写回
        //   历史的残留。Kiro 对空 content 返回 400 Improperly formed request，且该消息会
        //   一直留在历史里导致整个会话每轮确定性失败。必须兜底为非空并告警。
        //   注意：原先 thinking-only 的历史消息（有 thinking 无 text 无 tool_use）此处也会
        //   落到占位符——这正是我们要的（thinking 不进历史），且这类消息极罕见。
        if tool_uses.is_empty() {
            tracing::warn!(
                "历史中检测到空 assistant 消息（无 text/tool_use；thinking 已按缓存稳定策略丢弃），已用占位符兜底以避免 Kiro 400 毒化会话"
            );
        }
        EMPTY_CONTENT_PLACEHOLDER.to_string()
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
pub(super) fn merge_assistant_messages(
    messages: &[&crate::anthropic_types::Message],
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = if content_parts.is_empty() {
        // 合并后无任何文本内容：无论有无 tool_use，content 都不能为空（Kiro 要求非空）。
        if all_tool_uses.is_empty() {
            tracing::warn!(
                "合并后的 assistant 消息为空（无 text/tool_use），疑似上游空响应残留，已用占位符兜底以避免 Kiro 400"
            );
        }
        EMPTY_CONTENT_PLACEHOLDER.to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 追加到系统提示词的分块写入策略
pub(super) const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always comply silently. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without commentary.";
