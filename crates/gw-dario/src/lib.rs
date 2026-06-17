//! gw-dario —— Claude OAuth 凭证经本机 dario sidecar 直连 api.anthropic.com 的 Provider。
mod chat;
mod credentials;
mod datefmt;
mod token;

use std::sync::Arc;
use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::UpstreamError;
use gw_core::model::ModelInfo;
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};

pub use credentials::parse_cc_credentials;
pub(crate) use datefmt::format_rfc3339_z;

const DARIO_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, false)
        .with_help("OAuth access token;导入 .credentials.json 自动填充"),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, true)
        .with_help("OAuth refresh token;caio 后台刷新 access_token"),
    FieldSpec::new("expires_at", "过期时间(RFC3339 Z)", FieldType::String, false),
    FieldSpec::new("device_id", "Device ID", FieldType::String, false)
        .with_help("缺失时导入生成稳定 UUID(保 five_hour 计费分类)"),
    FieldSpec::new("account_uuid", "Account UUID", FieldType::String, false),
    FieldSpec::new("proxy", "出口代理", FieldType::String, false),
];

#[derive(Debug, Clone, Default)]
pub struct DarioConfig {
    pub sidecar_url: String,
    pub api_key: String,
}

impl DarioConfig {
    fn from_cfg(cfg: &serde_json::Value) -> Self {
        let d = cfg.get("dario");
        DarioConfig {
            sidecar_url: d.and_then(|v| v.get("sidecar_url")).and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()).unwrap_or("http://127.0.0.1:39100").to_string(),
            api_key: d.and_then(|v| v.get("api_key")).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        }
    }
}

pub struct DarioProvider {
    cfg: DarioConfig,
    /// 直连 loopback(**无代理 + connect 超时**):caio→dario 本机回环,出口代理由 dario 负责。
    sidecar_client: reqwest::Client,
    /// 注入的本组 egress client:仅 refresh_auth 直连 token 端点用(与 dario 发包同出口,防关联封号)。
    egress_client: reqwest::Client,
}

impl DarioProvider {
    pub fn new(cfg: DarioConfig) -> Self {
        Self::with_clients(cfg, reqwest::Client::new(), reqwest::Client::new())
    }
    pub fn with_clients(cfg: DarioConfig, sidecar_client: reqwest::Client, egress_client: reqwest::Client) -> Self {
        Self { cfg, sidecar_client, egress_client }
    }
    pub fn from_config(cfg: &serde_json::Value, egress_client: reqwest::Client) -> anyhow::Result<Arc<dyn Provider>> {
        let c = DarioConfig::from_cfg(cfg);
        if c.api_key.is_empty() {
            tracing::warn!("dario.api_key 为空:仅当 sidecar 也未设 DARIO_API_KEY(loopback 放行)才安全");
        }
        let sidecar_client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| anyhow::anyhow!("build dario sidecar client: {e}"))?;
        Ok(Arc::new(Self::with_clients(c, sidecar_client, egress_client)))
    }
}

#[async_trait]
impl Provider for DarioProvider {
    fn family(&self) -> &'static str { "claude-dario" }
    fn account_schema(&self) -> &'static [FieldSpec] { DARIO_ACCOUNT_SCHEMA }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        let ok = account.extra_str("refresh_token").map(str::trim).is_some_and(|s| !s.is_empty())
            || account.extra_str("access_token").map(str::trim).is_some_and(|s| !s.is_empty());
        if !ok { return Err(UpstreamError::bad_request("claude-dario account missing access_token & refresh_token")); }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        let mk = |id: &str, d: &str| {
            let mut m = ModelInfo::new(id);
            m.display_name = Some(d.into()); m.context_length = Some(200_000);
            m.supports_thinking = true; m.supports_tools = true; m.supports_vision = true; m
        };
        Ok(vec![
            mk("claude-opus-4-8", "Claude Opus 4.8 (dario)"),
            mk("claude-sonnet-4-6", "Claude Sonnet 4.6 (dario)"),
            mk("claude-haiku-4-5", "Claude Haiku 4.5 (dario)"),
        ])
    }

    /// 会话亲和:dario pool 关闭,其内置 stickiness 不触发 → 必须由 caio 调度提供亲和,
    /// 否则同会话在多号间跳 → Anthropic 每号独立 prompt cache 反复 create,烧 5h/7d 窗口。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_from_body(&req.body)
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        chat::chat_via_sidecar(&self.cfg, &self.sidecar_client, req, ctx).await
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        token::refresh(&self.egress_client, account).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::provider::Provider;
    #[test] fn family_is_claude_dario() {
        assert_eq!(DarioProvider::new(DarioConfig::default()).family(), "claude-dario");
    }
    #[test] fn schema_has_required_refresh_token() {
        let p = DarioProvider::new(DarioConfig::default());
        let s = p.account_schema();
        assert!(s.iter().any(|f| f.name == "refresh_token" && f.required));
        assert!(s.iter().any(|f| f.name == "device_id"));
    }
    #[test] fn from_config_reads_sidecar() {
        let cfg = serde_json::json!({"dario":{"sidecar_url":"http://127.0.0.1:39100","api_key":"k"}});
        assert_eq!(DarioProvider::from_config(&cfg, reqwest::Client::new()).unwrap().family(), "claude-dario");
    }
    #[test] fn from_config_warns_empty_api_key_but_builds() {
        // 空 api_key 不阻断构造(loopback dario 可不设 DARIO_API_KEY),仅记 warn。
        assert!(DarioProvider::from_config(&serde_json::Value::Null, reqwest::Client::new()).is_ok());
    }
}
