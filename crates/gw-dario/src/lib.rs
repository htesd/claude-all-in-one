//! gw-dario —— Claude OAuth 凭证经本机 dario sidecar 直连 api.anthropic.com 的 Provider。
mod chat;
mod credentials;
mod datefmt;
mod token;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
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

/// 校验并**规范化** dario per-account 出口代理 URL。chat 与 refresh 共用此函数,
/// 保证两条路径对同一字符串得到**字节一致**的判定与缓存键(消除"chat 放行/refresh
/// 回退"的不对称——审查 Skeptic#4/Architect#2)。仅 http(s):dario sidecar 不支持
/// socks。⚠️ 该代理必须是**固定单出口 IP**(非 rotating/backconnect)——refresh(reqwest)
/// 与 chat(dario/Bun)是各自独立连接,只有静态出口才能保证刷新 IP==发包 IP(Skeptic#2)。
pub(crate) fn normalize_dario_proxy(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|e| format!("invalid proxy URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("dario proxy must be http(s); got {s} (socks unsupported by sidecar)")),
    }
    if url.host_str().unwrap_or("").is_empty() {
        return Err("proxy URL missing host".to_string());
    }
    Ok(url.as_str().to_string())
}

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
    /// 注入的本组 egress client(worker 级默认出口):账号无 extra.proxy 时 refresh_auth 用它。
    egress_client: reqwest::Client,
    /// 按账号 extra.proxy → refresh client 缓存。chat 经 x-dario-upstream-proxy 让 dario
    /// 按账号出口;refresh_auth 必须走**同一** proxy(否则刷新 IP≠发包 IP,关联封号)。
    /// key = 规范化 proxy URL;reqwest::Client 内部 Arc,clone 廉价。
    proxy_clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl DarioProvider {
    pub fn new(cfg: DarioConfig) -> Self {
        Self::with_clients(cfg, reqwest::Client::new(), reqwest::Client::new())
    }
    pub fn with_clients(cfg: DarioConfig, sidecar_client: reqwest::Client, egress_client: reqwest::Client) -> Self {
        Self { cfg, sidecar_client, egress_client, proxy_clients: Mutex::new(HashMap::new()) }
    }

    /// refresh_auth 的出口 client。**fail-closed**(审查 Skeptic#1/Architect#1 共识阻断点):
    /// - 账号**未设** proxy → worker 级默认 egress_client(与 chat 默认出口一致);
    /// - 设了**合法 http(s)** proxy → 走该代理(与 chat 的 x-dario-upstream-proxy **同一**
    ///   规范化 URL → 同出口);
    /// - 设了但**非法/socks/构造失败** → **返回 Err 拒绝刷新**,绝不静默回退默认 IP
    ///   (回退 = 刷新 IP≠发包 IP = 正中关联封号)。刷新失败 = 该号暂不可用(token 过期后
    ///   下线),是**安全**的失败,远好于换 IP 刷新。
    /// 锁内同步构造(reqwest builder 无 await),消除并发重复 build;poison 用 into_inner 恢复。
    fn egress_client_for(&self, account: &Account) -> Result<reqwest::Client, UpstreamError> {
        let raw = match account.extra_str("proxy").map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => p,
            None => return Ok(self.egress_client.clone()),
        };
        let norm = normalize_dario_proxy(raw).map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::BadRequest,
                format!("dario account extra.proxy 非法({e}),拒绝刷新以防换 IP 刷新"),
            )
        })?;
        let mut cache = self.proxy_clients.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache.get(&norm) {
            return Ok(c.clone());
        }
        let proxy = reqwest::Proxy::all(&norm).map_err(|e| {
            UpstreamError::new(UpstreamErrorKind::BadRequest, format!("dario proxy 解析失败: {e}"))
        })?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                UpstreamError::new(UpstreamErrorKind::Other, format!("build dario proxy client: {e}"))
            })?;
        cache.insert(norm, client.clone());
        Ok(client)
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

    /// 会话亲和:MVP 阶段返回 `None`(无亲和)。
    ///
    /// `affinity_from_body` 基于**首条用户消息文本**哈希——但同一会话后续轮次首条
    /// 文本不变,而 dario 是 OAuth 直连(多账号池尚未稳定),基于首条文本的 key
    /// 与 Anthropic prompt cache 的会话粒度并不对齐。多号阶段再启用时,应以
    /// 真实 session_id(由调度层下发到 `CallCtx::session_id`)为 key,而非文本哈希。
    fn affinity_key(&self, _req: &ChatRequest) -> Option<String> {
        None
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        chat::chat_via_sidecar(&self.cfg, &self.sidecar_client, req, ctx).await
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        let client = self.egress_client_for(account)?; // fail-closed:非法 proxy 直接拒绝刷新
        token::refresh(&client, account).await
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

    #[test]
    fn egress_client_for_failclosed_and_caches() {
        use std::collections::BTreeMap;
        fn acct(proxy: Option<&str>) -> Account {
            let mut e = BTreeMap::new();
            if let Some(p) = proxy { e.insert("proxy".into(), serde_json::json!(p)); }
            Account { account_id: "d1".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, extra: e }
        }
        let p = DarioProvider::new(DarioConfig::default());
        // 无 proxy → Ok(worker 默认出口),缓存不增长。
        assert!(p.egress_client_for(&acct(None)).is_ok());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0);
        // socks → 不支持 → Err(fail-closed:不回退默认 IP,不缓存)。
        assert!(p.egress_client_for(&acct(Some("socks5://1.2.3.4:1080"))).is_err());
        // 非法(缺 host)→ Err。
        assert!(p.egress_client_for(&acct(Some("http://"))).is_err());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0);
        // http(含内嵌 basic auth)→ Ok,缓存一份;同 URL 二次复用不新增。
        assert!(p.egress_client_for(&acct(Some("http://caio:pw@45.77.219.188:13128"))).is_ok());
        assert!(p.egress_client_for(&acct(Some("http://caio:pw@45.77.219.188:13128"))).is_ok());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
    }

    #[test]
    fn normalize_dario_proxy_rules() {
        assert!(normalize_dario_proxy("http://h:8080").is_ok());
        assert!(normalize_dario_proxy("https://user:pw@h:8443").is_ok());
        assert!(normalize_dario_proxy("socks5://h:1080").is_err());
        assert!(normalize_dario_proxy("http://").is_err());
        assert!(normalize_dario_proxy("not a url").is_err());
    }
}
