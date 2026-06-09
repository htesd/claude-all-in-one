//! gw-claude-subprocess —— 通过 spawn `claude -p` 反代 Claude 的 Provider。
//!
//! 集成提示:在 `gw-app/src/registry.rs` 加 `reg.register("claude-subprocess", ClaudeSubprocessProvider::from_config)`。
//!
//! P0 仅打通最小可用链路:
//! - 账号级 HOME 隔离 spawn `claude -p --output-format stream-json`
//! - `stream_event` 直接转内部 Anthropic SSE
//! - `result.usage` 转结构化 `ChatUsage`
//! - prompt 先退化为“最后一条 user text”
//!
//! P1 待补:
//! - 完整多轮 messages / system prompt 拼装
//! - 更完整 usage / overage / stderr 分类
//! - 可配置 flags（如 `--dangerously-skip-permissions` / `--max-budget-usd`）

mod ndjson;
mod spawn;

use std::sync::Arc;

use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::UpstreamError;
use gw_core::model::ModelInfo;
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};
use spawn::{spawn_chat_stream, ClaudeSubprocessCommand};

const CLAUDE_SUBPROCESS_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("home_dir", "HOME 目录", FieldType::String, true),
    FieldSpec::new("model", "默认模型", FieldType::String, false),
];

pub struct ClaudeSubprocessProvider {
    family: &'static str,
}

impl ClaudeSubprocessProvider {
    pub fn new() -> Self {
        Self {
            family: "claude-subprocess",
        }
    }

    /// registry 工厂(忽略 egress client:subprocess 走 claude CLI 自管网络,不用 reqwest)。
    pub fn from_config(
        _cfg: &serde_json::Value,
        _egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        Ok(Arc::new(Self::new()))
    }
}

impl Default for ClaudeSubprocessProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ClaudeSubprocessProvider {
    fn family(&self) -> &'static str {
        self.family
    }

    fn account_schema(&self) -> &'static [FieldSpec] {
        CLAUDE_SUBPROCESS_ACCOUNT_SCHEMA
    }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        let home_dir = account.extra_str("home_dir").unwrap_or_default().trim();
        if home_dir.is_empty() {
            return Err(UpstreamError::bad_request(
                "claude-subprocess account missing required home_dir",
            ));
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        let mk = |id: &str, display: &str| {
            let mut model = ModelInfo::new(id);
            model.display_name = Some(display.into());
            model.supports_thinking = true;
            model.supports_tools = true;
            model.supports_vision = false;
            model
        };

        Ok(vec![
            mk("claude-opus-4-8", "Claude Opus 4.8 (subprocess)"),
            mk("claude-sonnet-4-5", "Claude Sonnet 4.5 (subprocess)"),
            mk("claude-haiku-4-5", "Claude Haiku 4.5 (subprocess)"),
        ])
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        self.validate_account(&ctx.account)?;

        let home_dir = ctx
            .account
            .extra_str("home_dir")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| UpstreamError::bad_request("claude-subprocess account missing home_dir"))?
            .to_string();

        // P0 简化:仅提取最后一条 user message 里的 text 内容。
        // 这样先把 subprocess 通道打通;P1 再补完整多轮/system 拼装策略。
        let prompt = extract_prompt_from_body(&req.body)?;
        let model = ctx
            .account
            .extra_str("model")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| if req.model.trim().is_empty() { None } else { Some(req.model.clone()) });

        let stream = spawn_chat_stream(ClaudeSubprocessCommand {
            home_dir,
            prompt,
            model,
        })?;
        Ok(Box::pin(stream))
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        // subprocess 通道的 token 生命周期由 `claude` CLI 自己管理。
        // P0 不做主动刷新,直接把账号原样返回给调度层。
        Ok(account.clone())
    }
}

fn extract_prompt_from_body(body: &serde_json::Value) -> Result<String, UpstreamError> {
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| UpstreamError::bad_request("Anthropic body missing messages array"))?;

    let prompt = messages
        .iter()
        .rev()
        .find(|msg| msg.get("role").and_then(|v| v.as_str()) == Some("user"))
        .and_then(extract_text_content)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| UpstreamError::bad_request("no user text content found for subprocess prompt"))?;

    Ok(prompt)
}

fn extract_text_content(message: &serde_json::Value) -> Option<&str> {
    match message.get("content")? {
        serde_json::Value::String(text) => Some(text.as_str()),
        serde_json::Value::Array(blocks) => blocks.iter().rev().find_map(|block| {
            (block.get("type").and_then(|v| v.as_str()) == Some("text"))
                .then(|| block.get("text").and_then(|v| v.as_str()))
                .flatten()
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn account_with_extra(extra: BTreeMap<String, serde_json::Value>) -> Account {
        Account {
            account_id: "acct-1".into(),
            provider: "claude-subprocess".into(),
            max_concurrency: 1,
            disabled: false,
            extra,
        }
    }

    #[test]
    fn family_matches_contract() {
        let provider = ClaudeSubprocessProvider::new();
        assert_eq!(provider.family(), "claude-subprocess");
    }

    #[test]
    fn account_schema_contains_required_home_dir() {
        let provider = ClaudeSubprocessProvider::new();
        let schema = provider.account_schema();
        assert!(schema.iter().any(|field| field.name == "home_dir" && field.required));
        assert!(schema.iter().any(|field| field.name == "model" && !field.required));
    }

    #[test]
    fn validate_account_requires_home_dir() {
        let provider = ClaudeSubprocessProvider::new();
        let account = account_with_extra(BTreeMap::new());
        let err = provider.validate_account(&account).unwrap_err();
        assert_eq!(err.kind.to_string(), "bad_request");
        assert!(err.message.contains("home_dir"));
    }

    #[test]
    fn validate_account_accepts_home_dir() {
        let provider = ClaudeSubprocessProvider::new();
        let mut extra = BTreeMap::new();
        extra.insert("home_dir".into(), serde_json::json!("/srv/claude-accounts/1"));
        let account = account_with_extra(extra);
        assert!(provider.validate_account(&account).is_ok());
    }

    #[test]
    fn extract_prompt_uses_last_user_text_block() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "first"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
                {"role": "user", "content": [{"type": "text", "text": "final prompt"}]}
            ]
        });
        assert_eq!(extract_prompt_from_body(&body).unwrap(), "final prompt");
    }
}
