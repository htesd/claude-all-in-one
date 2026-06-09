//! Kiro API 请求类型(🔵 搬运自旧 `src/kiro/model/requests/`)。
//!
//! 发往 Kiro `generateAssistantResponse` 的请求体结构:
//! `KiroRequest { conversationState: ConversationState, profileArn }`。
//! converter 把 Anthropic Messages 装配成这些类型。

pub mod conversation;
pub mod request;
pub mod tool;
