//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust,ignore
/// use kiro_rs::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// 模型专属请求字段 —— **思考强度的现行载体**(Kiro 1.0.212 起)。
    ///
    /// 形态由每个模型的 `additionalModelRequestFieldsSchema.schemaPath` 决定,拆包实测
    /// 客户端只生成两种(`extension.js` 的 `qe8`):
    /// - `{"output_config": {"effort": "<档位>"}}` —— Anthropic 系(caio 全部模型走这条)
    /// - `{"reasoning": {"effort": "<档位>"}}` —— 另一系
    ///
    /// 旧版把思考强度写成正文里的 `<thinking_mode>/<thinking_effort>` 文本标签,
    /// 1.0.212 客户端已**完全不发**那些标签(全 app 树零命中)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<serde_json::Value>,
    /// 代理模式 —— **body 字段,必发**。
    ///
    /// 拆包 1.0.212 `extension.js:228361` / `:228453`:非流式与流式两条路径都**无条件**
    /// 传 `agentMode: this.agentMode`,没有 `undefined` 分支。普通聊天恒为 `"vibe"`
    /// (`:224216` 的兜底 `return "vibe"`)。
    ///
    /// caio 此前只发同名 header `x-amzn-kiro-agent-mode`,body 里没有 —— 真客户端两处都发,
    /// 长期只有一处是稳定可规则化的顶层形态差异。
    ///
    /// 刻意**不用 `Option`**:做成可空又会造出"缺字段"的新形态,与修它的初衷相反。
    /// 缓存安全:位于 `conversationState` 之外,不参与前缀。
    ///
    /// `serde(default)` 只作用于**反序列化**(读旧报文/测试夹具时容错),
    /// 序列化侧无 `skip_serializing_if`,因此**永远发**。
    #[serde(default = "default_agent_mode")]
    pub agent_mode: String,
}

/// 普通网关流量的代理模式。Kiro 客户端在非 hook/spec/summarization 场景恒取此值。
pub const DEFAULT_AGENT_MODE: &str = "vibe";

fn default_agent_mode() -> String {
    DEFAULT_AGENT_MODE.to_string()
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 线缆形态回归:三个顶层字段必须**都在**,且无工具的普通消息**不得**带
    /// `userInputMessageContext`(拆包 1.0.212 `extension.js:228708` 逐字对照)。
    #[test]
    fn wire_shape_matches_kiro_1_0_212() {
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, UserInputMessage,
        };
        let cs = ConversationState::new("conv-1")
            .with_current_message(CurrentMessage::new(UserInputMessage::new("hi", "claude-opus-5")));
        let req = KiroRequest {
            conversation_state: cs,
            profile_arn: Some("arn:x".into()),
            additional_model_request_fields: Some(
                serde_json::json!({"output_config":{"effort":"xhigh"}}),
            ),
            agent_mode: DEFAULT_AGENT_MODE.to_string(),
        };
        let s = serde_json::to_string(&req).unwrap();
        // 必发的顶层字段
        assert!(s.contains(r#""agentMode":"vibe""#), "agentMode 必发,实际={s}");
        assert!(s.contains(r#""additionalModelRequestFields""#), "思考档位字段必发");
        assert!(s.contains(r#""profileArn""#));
        // **不得**多发的字段 —— 真客户端普通消息压根没有它
        assert!(
            !s.contains("userInputMessageContext"),
            "无工具的普通消息不该带空 context(这是每请求都有的稳定指纹),实际={s}"
        );
        // 反向:带工具结果时仍要发出来,别把功能一起省没了。
        let mut m = UserInputMessage::new("hi", "claude-opus-5");
        m.user_input_message_context.tool_results =
            vec![crate::kiro_types::tool::ToolResult::success("tu-1", "ok")];
        let s2 = serde_json::to_string(&m).unwrap();
        assert!(s2.contains("userInputMessageContext"), "有工具结果时必须发出,实际={s2}");
    }

    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
    }
}
