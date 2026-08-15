//! gw-dario —— Claude OAuth 凭证经本机 dario sidecar 直连 api.anthropic.com 的 Provider。
mod chat;
mod credentials;
mod datefmt;
pub mod oauth;
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
    /// 每账号 5h/7d 利用率快照:chat 从 sidecar 透传的 Anthropic 限额头捕获写入,
    /// `account_quota` 只读返回(OAuth/Max 无只读用量接口,只能从真实流量捕获)。
    ratelimit: chat::RateLimitStore,
}

impl DarioProvider {
    pub fn new(cfg: DarioConfig) -> Self {
        Self::with_clients(cfg, reqwest::Client::new(), reqwest::Client::new())
    }
    pub fn with_clients(cfg: DarioConfig, sidecar_client: reqwest::Client, egress_client: reqwest::Client) -> Self {
        Self {
            cfg,
            sidecar_client,
            egress_client,
            proxy_clients: Mutex::new(HashMap::new()),
            ratelimit: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// refresh_auth 的出口 client。**fail-closed**(审查 Skeptic#1/Architect#1 共识阻断点):
    /// - 账号**未设** proxy → worker 级默认 egress_client(与 chat 默认出口一致);
    /// - 设了**合法 http(s)** proxy → 走该代理(与 chat 的 x-dario-upstream-proxy **同一**
    ///   规范化 URL → 同出口);
    /// - 设了但**非法/socks/构造失败** → **返回 Err 拒绝刷新**,绝不静默回退默认 IP
    ///   (回退 = 刷新 IP≠发包 IP = 正中关联封号)。刷新失败 = 该号暂不可用(token 过期后
    ///   下线),是**安全**的失败,远好于换 IP 刷新。
    /// 锁内同步构造(reqwest builder 无 await),消除并发重复 build;poison 用 into_inner 恢复。
    /// 按 **proxy 字符串**选出口 client(fail-closed)。`egress_client_for(account)`(refresh)与
    /// `oauth_exchange`(铸 token)**共用**此函数,保证「同一 proxy 串 → 同一出口 client」——egress
    /// 选择只依赖 proxy 串这一个输入,消除对 Account 其它字段的隐藏耦合(审查 Architect#4)。
    /// - `None`/空 → worker 级默认 egress_client(与 chat 默认出口一致);
    /// - 合法 http(s) → 该代理(与 chat 的 x-dario-upstream-proxy 同一规范化 URL → 同出口);
    /// - 非法/socks/构造失败 → **Err 拒绝**,绝不静默回退默认 IP(回退=换 IP=关联封号)。
    fn client_for_proxy(&self, raw_proxy: Option<&str>) -> Result<reqwest::Client, UpstreamError> {
        let raw = match raw_proxy.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => p,
            None => return Ok(self.egress_client.clone()),
        };
        let norm = normalize_dario_proxy(raw).map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::BadRequest,
                format!("dario proxy 非法({e}),拒绝以防换 IP(刷新/铸 token 出口必须==发包出口)"),
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

    /// refresh_auth 的出口 client：仅取该账号 `extra.proxy`,委托 [`Self::client_for_proxy`]。
    fn egress_client_for(&self, account: &Account) -> Result<reqwest::Client, UpstreamError> {
        self.client_for_proxy(account.extra_str("proxy"))
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

    /// 会话亲和键:优先取 `metadata.user_id` 里的 Claude Code 原生 session_id(UUID),
    /// 回退首条用户消息文本哈希。多号下让同一会话钉到组内同账号,最大化 Anthropic
    /// prompt cache 命中(按账号隔离)。详见 [`chat::affinity_key_from_body`]。
    ///
    /// 注:session_id 在请求 **body** 内(`metadata.user_id`),不受 caio router 第一跳
    /// 丢客户端 header 影响,故无需依赖 `CallCtx::session_id` 下发。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_key_from_body(&req.body)
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        chat::chat_via_sidecar(&self.cfg, &self.sidecar_client, self.ratelimit.clone(), req, ctx).await
    }

    /// 配额是本地廉价读(从聊天流量捕获的内存快照,无上游往返)→ 告诉 gw-app 别拿"昂贵调用"
    /// 的 TTL 节流(尤其别让陈旧 None 挡住刚捕获的快照,见 Provider::quota_is_local)。
    fn quota_is_local(&self) -> bool {
        true
    }

    /// 只读返回该账号最近一次从聊天流量捕获的 5h/7d 利用率快照(无积分概念,见 [`chat::DarioRateLimit`])。
    /// 未发过请求的号返回 `None`(面板显示「—」),因 Anthropic OAuth/Max 无只读用量接口、
    /// 且红线禁止真号探测,只能从真实流量响应头捕获。worker 配额缓存按 TTL 调本方法,纯读不发包。
    async fn account_quota(&self, account: &Account) -> Result<Option<gw_core::provider::AccountQuota>, UpstreamError> {
        let m = self.ratelimit.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.get(&account.account_id).and_then(|s| s.to_quota()))
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        let client = self.egress_client_for(account)?; // fail-closed:非法 proxy 直接拒绝刷新
        token::refresh(&client, account).await
    }

    /// OAuth 上号:`authorization_code` → token set。
    /// **复用 `egress_client_for`**(同 fail-closed + 同缓存):换码出口由该号 `proxy` 决定,
    /// 与它将来 refresh/chat 字节一致的同一 egress → 铸 token IP == 刷新 IP == 发包 IP。
    /// proxy 非法(socks/缺 host)直接 Err,绝不静默走默认 IP 换码。
    async fn oauth_exchange(
        &self,
        proxy: Option<&str>,
        code: &str,
        verifier: &str,
    ) -> Result<serde_json::Value, UpstreamError> {
        // 换码出口 = 该号 proxy 选出的 client = 它将来 refresh/chat 的同一出口(同一 proxy 串)。
        let client = self.client_for_proxy(proxy)?;
        let fields = oauth::exchange(&client, code, verifier).await?;
        Ok(serde_json::Value::Object(fields))
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
            Account { account_id: "d1".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, created_at: 0, extra: e }
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

    #[tokio::test]
    async fn oauth_exchange_failclosed_on_socks_proxy() {
        // socks proxy 非法 → egress_client_for 直接 Err(在任何换码网络请求之前),
        // 绝不静默走默认 IP 换码(铸 token IP≠发包 IP = 关联封号)。
        let p = DarioProvider::new(DarioConfig::default());
        let r = p
            .oauth_exchange(Some("socks5://1.2.3.4:1080"), "code", "verifier")
            .await;
        assert!(r.is_err(), "socks proxy 必须 fail-closed");
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
