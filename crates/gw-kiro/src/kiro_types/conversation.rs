//! 对话类型定义
//!
//! 定义 Kiro API 中对话相关的类型，包括消息、历史记录等

use serde::{Deserialize, Serialize};

use super::tool::{Tool, ToolResult, ToolUseEntry};

/// 对话状态
///
/// Kiro API 请求中的核心结构，包含当前消息和历史记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationState {
    /// 代理延续 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_continuation_id: Option<String>,
    /// 代理任务类型（通常为 "vibe"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_task_type: Option<String>,
    /// 聊天触发类型（"MANUAL" 或 "AUTO"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_trigger_type: Option<String>,
    /// 当前消息
    pub current_message: CurrentMessage,
    /// 会话 ID
    pub conversation_id: String,
    /// 历史消息列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
}

impl ConversationState {
    /// 创建新的对话状态
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self {
            agent_continuation_id: None,
            agent_task_type: None,
            chat_trigger_type: None,
            current_message: CurrentMessage::default(),
            conversation_id: conversation_id.into(),
            history: Vec::new(),
        }
    }

    /// 设置代理延续 ID
    pub fn with_agent_continuation_id(mut self, id: impl Into<String>) -> Self {
        self.agent_continuation_id = Some(id.into());
        self
    }

    /// 设置代理任务类型
    pub fn with_agent_task_type(mut self, task_type: impl Into<String>) -> Self {
        self.agent_task_type = Some(task_type.into());
        self
    }

    /// 设置聊天触发类型
    pub fn with_chat_trigger_type(mut self, trigger_type: impl Into<String>) -> Self {
        self.chat_trigger_type = Some(trigger_type.into());
        self
    }

    /// 设置当前消息
    pub fn with_current_message(mut self, message: CurrentMessage) -> Self {
        self.current_message = message;
        self
    }

    /// 添加历史消息
    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        self.history = history;
        self
    }
}

/// 当前消息容器
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentMessage {
    /// 用户输入消息
    pub user_input_message: UserInputMessage,
}

impl CurrentMessage {
    /// 创建新的当前消息
    pub fn new(user_input_message: UserInputMessage) -> Self {
        Self { user_input_message }
    }
}

/// 用户输入消息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputMessage {
    /// 用户输入消息上下文。
    ///
    /// **空对象必须省略**(与下方历史 `UserMessage` 同口径)。拆包 Kiro 1.0.212
    /// `extension.js:228708`:普通 human 消息**根本不带**这个字段,只有工具结果
    /// (`{toolResults}`,:228717)或末条消息带工具定义(`{tools}`,:228745)时才创建。
    ///
    /// caio 此前无条件序列化,于是**每个无工具请求都多发一个 `"userInputMessageContext":{}`**
    /// —— 真客户端从不产生的稳定多余字段,是比"少发偶发字段"更好统计的指纹。
    /// 历史消息一直是对的(`is_default_context`),只有当前消息漏了。
    ///
    /// 缓存安全:当前消息位于前缀**之后**,改它不动已缓存的 history 前缀。
    ///
    /// `KIRO_LEGACY_WIRE=1` 时谓词恢复旧行为(空也照发 `{}`,见
    /// [`skip_current_message_context`])—— 旧形态(0.12.155 时代)这个字段是无条件发送的。
    #[serde(default, skip_serializing_if = "skip_current_message_context")]
    pub user_input_message_context: UserInputMessageContext,
    /// 消息内容
    pub content: String,
    /// 模型 ID
    pub model_id: String,
    /// 图片列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<KiroImage>,
    /// 文档列表（PDF / Office / 文本附件）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<KiroDocument>,
    /// 消息来源（通常为 "AI_EDITOR"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// 缓存标记点（翻译自 Anthropic cache_control，告诉 Kiro 后端这之前的内容必须缓存）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<CachePoint>,
    /// 客户端缓存配置（与 cache_point 配套使用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cache_config: Option<ClientCacheConfig>,
}

impl UserInputMessage {
    /// 创建新的用户输入消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            user_input_message_context: UserInputMessageContext::default(),
            content: content.into(),
            model_id: model_id.into(),
            images: Vec::new(),
            documents: Vec::new(),
            origin: Some("AI_EDITOR".to_string()),
            cache_point: None,
            client_cache_config: None,
        }
    }

    /// 设置消息上下文
    pub fn with_context(mut self, context: UserInputMessageContext) -> Self {
        self.user_input_message_context = context;
        self
    }

    /// 添加图片
    pub fn with_images(mut self, images: Vec<KiroImage>) -> Self {
        self.images = images;
        self
    }

    /// 添加文档
    pub fn with_documents(mut self, documents: Vec<KiroDocument>) -> Self {
        self.documents = documents;
        self
    }

    /// 设置来源
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

/// 用户输入消息上下文
///
/// 包含工具定义和工具执行结果
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputMessageContext {
    /// 工具执行结果列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    /// 可用工具列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

impl UserInputMessageContext {
    /// 创建新的消息上下文
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置工具列表
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// 设置工具结果
    pub fn with_tool_results(mut self, results: Vec<ToolResult>) -> Self {
        self.tool_results = results;
        self
    }
}

/// Kiro 图片
///
/// API 中使用的图片格式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroImage {
    /// 图片格式（"jpeg", "png", "gif", "webp"）
    pub format: String,
    /// 图片数据源
    pub source: KiroImageSource,
}

impl KiroImage {
    /// 从 base64 数据创建图片
    pub fn from_base64(format: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            format: format.into(),
            source: KiroImageSource { bytes: data.into() },
        }
    }
}

/// Kiro 图片数据源
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImageSource {
    /// base64 编码的图片数据
    pub bytes: String,
}

/// Kiro 文档（PDF / Office / 文本类附件）
///
/// Kiro/CodeWhisperer 上游原生支持文档附件，结构与图片并列：
/// `documents[*] = { name, format, source: { bytes } }`。
/// 翻译自 Anthropic 的 `{type:"document", source:{type:base64, media_type, data}}` 块。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroDocument {
    /// 文档名称（去重也按 name；缺省用 "document"）
    pub name: String,
    /// 文档格式（"pdf" / "csv" / "doc" / "docx" / "xls" / "xlsx" / "html" / "txt" / "md"）
    pub format: String,
    /// 文档数据源
    pub source: KiroDocumentSource,
}

impl KiroDocument {
    /// 从 base64 数据创建文档
    pub fn from_base64(
        name: impl Into<String>,
        format: impl Into<String>,
        data: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            format: format.into(),
            source: KiroDocumentSource { bytes: data.into() },
        }
    }
}

/// Kiro 文档数据源
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroDocumentSource {
    /// base64 编码的文档数据
    pub bytes: String,
}

/// 历史消息
///
/// 可以是用户消息或助手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// 用户消息
    User(HistoryUserMessage),
    /// 助手消息
    Assistant(HistoryAssistantMessage),
}

/// 历史用户消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryUserMessage {
    /// 用户输入消息
    pub user_input_message: UserMessage,
}

impl HistoryUserMessage {
    /// 创建新的历史用户消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            user_input_message: UserMessage::new(content, model_id),
        }
    }
}

/// 用户消息（历史记录中使用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// 消息内容
    pub content: String,
    /// 模型 ID
    pub model_id: String,
    /// 消息来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// 图片列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<KiroImage>,
    /// 文档列表（PDF / Office / 文本附件）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<KiroDocument>,
    /// 用户输入消息上下文
    #[serde(default, skip_serializing_if = "is_default_context")]
    pub user_input_message_context: UserInputMessageContext,
    /// 缓存标记点（翻译自 Anthropic cache_control）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<CachePoint>,
    /// 客户端缓存配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cache_config: Option<ClientCacheConfig>,
}

/// 缓存标记点
///
/// Kiro/CodeWhisperer 兼容字段，源自 Anthropic 协议的 `cache_control`。
/// 告诉 Kiro 后端：这条消息（及之前的前缀）应当被缓存复用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePoint {
    /// EPHEMERAL = 短期缓存（约 5 分钟，对齐 Anthropic 默认）
    /// PERSISTENT = 长期缓存（约 1 小时）
    #[serde(rename = "type")]
    pub cache_type: String,
}

impl CachePoint {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "EPHEMERAL".to_string(),
        }
    }

    pub fn with_type(cache_type: impl Into<String>) -> Self {
        Self {
            cache_type: cache_type.into(),
        }
    }
}

/// 客户端缓存配置
///
/// 与 `cache_point` 配套使用，启用 prefix cache 机制。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCacheConfig {
    pub use_prompt_cache: bool,
}

impl Default for ClientCacheConfig {
    fn default() -> Self {
        Self {
            use_prompt_cache: true,
        }
    }
}

fn is_default_context(ctx: &UserInputMessageContext) -> bool {
    ctx.tools.is_empty() && ctx.tool_results.is_empty()
}

/// 当前消息( [`UserInputMessage`] )空 context 的省略谓词。
///
/// 1.0.212 形态:空即省略(对齐真客户端)。`KIRO_LEGACY_WIRE=1`:空也照发 `{}`
/// —— 那才是 0.12.155 时代的旧行为。历史消息( [`UserMessage`] )两个时代都省略,
/// 仍走上面的 [`is_default_context`],不经过这里。
///
/// serde 谓词无法注入参数,只能在序列化点读 env(每次序列化一次 getenv,
/// 与 converter/history.rs 热路径的现有 env 读取同口径);决策本体拆成纯函数以便测试。
fn skip_current_message_context(ctx: &UserInputMessageContext) -> bool {
    skip_context_decision(is_default_context(ctx), crate::wire_profile::legacy_wire())
}

/// 纯逻辑:空 context 且非 legacy 形态才省略。
fn skip_context_decision(is_default: bool, legacy: bool) -> bool {
    is_default && !legacy
}

impl UserMessage {
    /// 创建新的用户消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            model_id: model_id.into(),
            origin: Some("AI_EDITOR".to_string()),
            images: Vec::new(),
            documents: Vec::new(),
            user_input_message_context: UserInputMessageContext::default(),
            cache_point: None,
            client_cache_config: None,
        }
    }

    /// 设置图片
    pub fn with_images(mut self, images: Vec<KiroImage>) -> Self {
        self.images = images;
        self
    }

    /// 设置文档
    pub fn with_documents(mut self, documents: Vec<KiroDocument>) -> Self {
        self.documents = documents;
        self
    }

    /// 设置上下文
    pub fn with_context(mut self, context: UserInputMessageContext) -> Self {
        self.user_input_message_context = context;
        self
    }
}

/// 历史助手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryAssistantMessage {
    /// 助手响应消息
    pub assistant_response_message: AssistantMessage,
}

impl HistoryAssistantMessage {
    /// 创建新的历史助手消息
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            assistant_response_message: AssistantMessage::new(content),
        }
    }
}

/// 助手消息（历史记录中使用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// 响应内容
    pub content: String,
    /// 工具使用列表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<Vec<ToolUseEntry>>,
    /// 结构化推理内容(思考链)—— 1.0.212 官方客户端的历史上传通道。
    ///
    /// 上线形态由 Smithy 模型定义:`reasoningContent` 是 union,
    /// 一支 `{reasoningText: {text, signature}}`,另一支 `{redactedContent: <base64>}`。
    /// 2026-08-19 探针实测:上游验签(`THINKING_SIGNATURE_INVALID`)且**只认签名**,
    /// `text` 字段被忽略(空文本+真签名,模型仍能还原推理内容)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<ReasoningContent>,
}

/// 历史 assistant 消息的结构化推理内容(对应 Smithy `ReasoningContent` union)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReasoningContent {
    /// 带签名的推理文本(签名是加密载体,text 仅作形态对齐)。
    ReasoningText {
        #[serde(rename = "reasoningText")]
        reasoning_text: ReasoningText,
    },
    /// Anthropic `redacted_thinking` 块的原样映射(base64 密文)。
    Redacted {
        #[serde(rename = "redactedContent")]
        redacted_content: String,
    },
}

/// Smithy `ReasoningText` 结构:推理文本 + 上游签名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningText {
    pub text: String,
    pub signature: String,
}

impl AssistantMessage {
    /// 创建新的助手消息
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_uses: None,
            reasoning_content: None,
        }
    }

    /// 设置工具使用
    pub fn with_tool_uses(mut self, tool_uses: Vec<ToolUseEntry>) -> Self {
        self.tool_uses = Some(tool_uses);
        self
    }

    /// 挂上结构化推理内容(历史 thinking 的上行通道)。
    pub fn with_reasoning_content(mut self, reasoning: ReasoningContent) -> Self {
        self.reasoning_content = Some(reasoning);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_context_decision_truth_table() {
        // 1.0.212 形态(legacy=false):空省略、非空发;legacy 形态:空也照发。
        assert!(skip_context_decision(true, false), "1.0.212 空 context 省略");
        assert!(!skip_context_decision(false, false), "非空必发");
        assert!(!skip_context_decision(true, true), "legacy 空 context 照发");
        assert!(!skip_context_decision(false, true), "legacy 非空必发");
    }

    #[test]
    fn test_conversation_state_new() {
        let state = ConversationState::new("conv-123")
            .with_agent_task_type("vibe")
            .with_chat_trigger_type("MANUAL");

        assert_eq!(state.conversation_id, "conv-123");
        assert_eq!(state.agent_task_type, Some("vibe".to_string()));
        assert_eq!(state.chat_trigger_type, Some("MANUAL".to_string()));
    }

    #[test]
    fn test_user_input_message() {
        let msg = UserInputMessage::new("Hello", "claude-3-5-sonnet").with_origin("AI_EDITOR");

        assert_eq!(msg.content, "Hello");
        assert_eq!(msg.model_id, "claude-3-5-sonnet");
        assert_eq!(msg.origin, Some("AI_EDITOR".to_string()));
    }

    #[test]
    fn test_history_serialize() {
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-3-5-sonnet")),
            Message::Assistant(HistoryAssistantMessage::new("Hi! How can I help you?")),
        ];

        let json = serde_json::to_string(&history).unwrap();
        assert!(json.contains("userInputMessage"));
        assert!(json.contains("assistantResponseMessage"));
    }

    #[test]
    fn test_conversation_state_serialize() {
        let state = ConversationState::new("conv-123")
            .with_agent_task_type("vibe")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                "Hello",
                "claude-3-5-sonnet",
            )));

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"conversationId\":\"conv-123\""));
        assert!(json.contains("\"agentTaskType\":\"vibe\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }
}
