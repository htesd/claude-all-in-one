//! tool_use/tool_result 配对校验与孤儿清理(防 Kiro 400)。

use super::EMPTY_USER_CONTENT_PLACEHOLDER;
use crate::kiro_types::conversation::Message;
use crate::kiro_types::tool::ToolResult;

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
pub(super) fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
pub(super) fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                let original_len = tool_uses.len();
                tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

                // 如果移除后为空，设置为 None
                if tool_uses.is_empty() {
                    assistant_msg.assistant_response_message.tool_uses = None;
                } else if tool_uses.len() != original_len {
                    tracing::debug!(
                        "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                        original_len - tool_uses.len()
                    );
                }
            }
        }
    }
}

/// 从历史消息中移除孤立的 tool_result
///
/// 与 `remove_orphaned_tool_uses` 对称：Kiro API 要求每个 tool_result 必须有对应的
/// tool_use，否则返回 400 Bad Request。客户端反复 auto-compact 长对话时可能压掉发起
/// tool_use 的 assistant 消息却保留其 tool_result，残留成 history 中段的孤儿。
/// 此函数先收集 history 中所有 tool_use_id，再删掉 user 消息里 tool_use_id 不在其中的
/// tool_result。
pub(super) fn remove_orphaned_tool_results(history: &mut [Message]) {
    use std::collections::HashSet;

    // 1. 收集 history 中所有 tool_use_id（此时孤立 tool_use 已被前一步移除）
    let all_tool_use_ids: HashSet<String> = history
        .iter()
        .filter_map(|msg| match msg {
            Message::Assistant(a) => a.assistant_response_message.tool_uses.as_ref(),
            Message::User(_) => None,
        })
        .flatten()
        .map(|tu| tu.tool_use_id.clone())
        .collect();

    // 2. 删掉 user 消息里没有对应 tool_use 的 tool_result
    let mut removed_ids: Vec<String> = Vec::new();
    let mut repaired_empty = 0usize;
    for msg in history.iter_mut() {
        if let Message::User(user_msg) = msg {
            let uim = &mut user_msg.user_input_message;
            uim.user_input_message_context.tool_results.retain(|r| {
                let keep = all_tool_use_ids.contains(&r.tool_use_id);
                if !keep {
                    removed_ids.push(r.tool_use_id.clone());
                }
                keep
            });
            // 修复排序隐患：merge_user_messages 当初对"无文本但有 tool_result"的回合
            // 故意保留空 content（合法工具结果回合）。若这里把它仅有的 tool_result 作为孤儿
            // 删光，该消息就变成"彻底空"（content/tool_results/images 全空），Kiro 会判 400。
            // 此处补 user 专用占位符兜底（纯空白 user content 同样被 Kiro 拒，
            // 见 EMPTY_USER_CONTENT_PLACEHOLDER），保证清理后不残留非法消息。
            if uim.content.trim().is_empty()
                && uim.user_input_message_context.tool_results.is_empty()
                && uim.images.is_empty()
                && uim.documents.is_empty()
            {
                uim.content = EMPTY_USER_CONTENT_PLACEHOLDER.to_string();
                repaired_empty += 1;
            }
        }
    }

    if !removed_ids.is_empty() {
        tracing::warn!(
            "从历史中移除了 {} 个孤立的 tool_result（无对应 tool_use，客户端压缩残留）：{:?}；其中 {} 条 user 消息因此变空已补占位符",
            removed_ids.len(),
            removed_ids,
            repaired_empty
        );
    }
}
