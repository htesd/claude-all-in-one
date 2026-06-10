//! gw-kiro —— Kiro provider 实现。
//!
//! Phase 0:仅 stub,证明 [`gw_core::Provider`] 可被实现、可装进 registry。
//! 真实能力(wire/converter/cache_sim/stream/token/machine_id)在 P1+ 按
//! `docs/IMPROVEMENTS.md` 逐块搬运/借鉴。
//!
//! ## 已搬运资产(🔵=原样搬旧 kiro.rs,不重写)
//! - [`parser`]:AWS Event Stream 帧/header/CRC/decoder 状态机
//!   (来自旧 `src/kiro/parser/*`,抗分片/半包/坏包)。
//! - [`anthropic_types`]:Anthropic Messages 输入类型(旧 `src/anthropic/types.rs`)。
//! - [`kiro_types`]:Kiro 请求输出类型(旧 `src/kiro/model/requests/*`)。

pub mod anthropic_types;
pub mod cache_sim;
pub mod chat;
pub mod converter;
pub mod error_map;
pub mod headers;
pub mod inline_thinking;
pub mod kiro_types;
pub mod machine_id;
pub mod parser;
pub mod signature;
pub mod text_tokens;
pub mod thinking_policy;
pub mod token;
pub mod usage;

use std::sync::Arc;

use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::UpstreamError;
use gw_core::model::{MachineIdentity, ModelInfo};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};

/// 缓存计费参数(v53:multiplier/cap/floor)。worker 从 `system.yaml` 的 `cache` 段注入,
/// 替代旧 kiro.rs 的 admin 热调控制面(本项目 Phase 4 再做真正热调,当前为启动期定值)。
///
/// 缺省值对齐 [`crate::usage`] 的 DEFAULT_*(1.8 / 0.9 / 0.0):冷启动如实报 0、不造假。
#[derive(Debug, Clone, Copy)]
pub struct CacheBilling {
    pub read_multiplier: f64,
    pub cap_ratio: f64,
    pub floor_ratio: f64,
}

impl Default for CacheBilling {
    fn default() -> Self {
        Self {
            read_multiplier: usage::DEFAULT_CACHE_READ_MULTIPLIER,
            cap_ratio: usage::DEFAULT_CACHE_CAP_RATIO,
            floor_ratio: usage::DEFAULT_CACHE_FLOOR_RATIO,
        }
    }
}

impl CacheBilling {
    /// 从 provider 工厂的 `cfg` JSON 解析(worker 把 `system.cache` 序列化进来)。
    /// 字段缺失/类型不符 → 各自回退默认,**不 fail**(billing 参数容错优先于严格)。
    fn from_cfg(cfg: &serde_json::Value) -> Self {
        let d = Self::default();
        let get = |k: &str, fallback: f64| cfg.get(k).and_then(|v| v.as_f64()).unwrap_or(fallback);
        Self {
            read_multiplier: get("read_multiplier", d.read_multiplier),
            cap_ratio: get("cap_ratio", d.cap_ratio),
            floor_ratio: get("floor_ratio", d.floor_ratio),
        }
    }
}

/// 账号字段 schema(驱动 admin 表单 + accounts.yaml 校验)。
///
/// 字段全集对齐 static_flow `KiroAuthRecord`,覆盖 Social / IdC / API Key 三种凭据。
/// **防封关键**:`machine_id` —— Social 号(KiroManager 导出)必须带原始真机指纹,
/// 否则按 `sha256("KotlinNativeAPI/"+refresh_token)` 派生,会随 rolling token 漂移触发风控。
const KIRO_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, false),
    FieldSpec::new("profile_arn", "Profile ARN", FieldType::String, false),
    FieldSpec::new("region", "区域", FieldType::String, false)
        .with_help("AWS 区域,默认 us-east-1;同时兜底 api/auth 区域"),
    FieldSpec::new("machine_id", "设备指纹 (machineId)", FieldType::String, false)
        .with_help("Social 号防封关键:填原始真机 machineId(64 hex 或 UUID);留空则按 refresh_token 派生,会随 token 刷新漂移"),
    FieldSpec::new("auth_method", "认证方式", FieldType::String, false)
        .with_help("social / idc;留空按 client_id+secret 自动判定"),
    FieldSpec::new("kiro_provider", "身份来源", FieldType::String, false)
        .with_help("github / google / builderid / enterprise;用于缺失 profileArn 时取固定兜底"),
    FieldSpec::new("client_id", "IdC Client ID", FieldType::String, false)
        .with_help("IdC(企业 SSO)账号必填,与 Client Secret 成对"),
    FieldSpec::new("client_secret", "IdC Client Secret", FieldType::Password, false)
        .with_help("IdC 账号必填;走 AWS OIDC 刷新,不易封"),
    FieldSpec::new("kiro_version", "客户端版本", FieldType::String, false)
        .with_help("⚠️ UA 里的 KiroIDE 版本,默认 0.12.155;改它而不同步 OS/Node 版本会造成现实不存在的指纹组合,非必要勿动"),
];

/// Kiro provider。持有 worker 注入的 egress client(固定出口 IP)。
pub struct KiroProvider {
    family: &'static str,
    /// 本 worker 的 egress HTTP client(所有上游请求走同一出口)。
    egress_client: reqwest::Client,
    /// 缓存计费参数(从 system.yaml 注入,见 [`CacheBilling`])。
    cache_billing: CacheBilling,
}

impl KiroProvider {
    pub fn new(egress_client: reqwest::Client) -> Self {
        Self {
            family: "kiro",
            egress_client,
            cache_billing: CacheBilling::default(),
        }
    }

    /// 显式指定缓存计费参数(测试 / 配置注入用)。
    pub fn with_cache_billing(mut self, billing: CacheBilling) -> Self {
        self.cache_billing = billing;
        self
    }

    /// registry 工厂:接收 worker 的 egress client(固定出口 IP),注入 provider。
    /// `cfg` 携带 `system.cache` 段(read_multiplier/cap_ratio/floor_ratio)。
    pub fn from_config(
        cfg: &serde_json::Value,
        egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        Ok(Arc::new(
            Self::new(egress_client).with_cache_billing(CacheBilling::from_cfg(cfg)),
        ))
    }
}

impl KiroProvider {
    /// 该账号的设备指纹(machineId/UA)—— **Kiro 专属**,不在通用 Provider trait 内
    /// (审查 H5b:subprocess/dario 无 machineId 概念)。发包指纹的单一事实来源。
    ///
    /// machineId 派生见 [`machine_id::generate_from_account`](crate::machine_id)
    /// (🔵 照搬旧公式:social/OAuth = sha256("KotlinNativeAPI/"+refreshToken))。
    /// UA 把 machineId 嵌在末尾:`...KiroIDE-{version}-{machineId}`(封号根因点)。
    pub fn machine_identity(&self, account: &Account) -> MachineIdentity {
        let machine_id = machine_id::generate_from_account(account);
        // 客户端版本:账号可显式覆盖,否则用默认。
        let client_version = headers::kiro_version(account);
        // UA 与实际发包(headers 模块)同源,避免版本号漂移(此前写死 1.0.0 是陷阱)。
        let (x_amz_user_agent, user_agent) =
            headers::streaming_user_agents(&client_version, &machine_id);
        MachineIdentity {
            user_agent,
            x_amz_user_agent,
            machine_id,
            client_version,
        }
    }
}

#[async_trait]
impl Provider for KiroProvider {
    fn family(&self) -> &'static str {
        self.family
    }

    fn account_schema(&self) -> &'static [FieldSpec] {
        KIRO_ACCOUNT_SCHEMA
    }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        // 当前只支持 social/IdC(均需 refresh_token);API Key 路径尚未实现 token 换取流程
        // (审查 Skeptic#1/Architect#1:此前 schema 暴露 kiro_api_key 但调用链不支持,
        // 现撤下并在此明确拒绝,避免"加载通过、首请求才报缺 refresh_token"的边界破裂)。
        let has_refresh = account.extra_str("refresh_token").is_some_and(|s| !s.is_empty());
        if !has_refresh {
            return Err(UpstreamError::bad_request(format!(
                "Kiro 账号 '{}' 缺少 refresh_token(social/IdC 必需;API Key 凭据暂不支持)",
                account.account_id
            )));
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        // P1:返回 converter 已支持映射的模型集合(对外名)。真实 ListAvailableModels 留 P2。
        let mk = |id: &str, thinking: bool| {
            let mut m = ModelInfo::new(id);
            m.supports_thinking = thinking;
            m.supports_tools = true;
            m.supports_vision = true;
            m
        };
        Ok(vec![
            mk("claude-opus-4-8", true),
            mk("claude-sonnet-4-5", true),
            mk("claude-haiku-4-5", false),
        ])
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        // 设备指纹(machineId 嵌 UA);egress client 固定出口 IP。
        let machine_id = self.machine_identity(&ctx.account).machine_id;
        chat::chat_stream(
            self.egress_client.clone(),
            ctx.account.clone(),
            machine_id,
            req,
            self.cache_billing,
        )
        .await
    }

    /// 会话亲和键 = 派生的 conversationId(与上游 prefix cache 的会话粒度同源)。
    /// worker 据此把同会话钉到组内同账号,最大化 Kiro 缓存命中(按账号隔离)。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        converter::affinity_key_from_body(&req.body)
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        // 用 egress client 刷新(social/IdC 自动分流),把新 token 写回 Account.extra 返回。
        // 持久化由 gw-app 负责(契约 H4)。
        let refreshed = token::refresh_auth(&self.egress_client, account).await?;
        let mut updated = account.clone();
        // 冻结 machineId(用**旧** refresh_token 派生,务必在覆盖 rolling token 之前):
        // 否则下次刷新后 machineId 随新 token 漂移 = 换设备 = 封号。见 machine_id::freeze。
        if machine_id::freeze_machine_id_if_absent(&mut updated) {
            tracing::info!(
                account_id = %updated.account_id,
                "已冻结派生 machineId 防 rolling token 漂移;若该号由 Kiro IDE/KiroManager 导出,\
                 建议在账号里填真机 machine_id 以彻底规避风控"
            );
        }
        updated.extra.insert(
            "access_token".into(),
            serde_json::Value::String(refreshed.access_token),
        );
        if let Some(rt) = refreshed.refresh_token {
            updated
                .extra
                .insert("refresh_token".into(), serde_json::Value::String(rt));
        }
        if let Some(arn) = refreshed.profile_arn {
            updated
                .extra
                .insert("profile_arn".into(), serde_json::Value::String(arn));
        }
        if let Some(exp) = refreshed.expires_at {
            updated
                .extra
                .insert("expires_at".into(), serde_json::Value::String(exp));
        }
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn account_with(extra: &[(&str, &str)]) -> Arc<Account> {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Arc::new(Account {
            account_id: "k-test".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: map,
        })
    }

    #[tokio::test]
    async fn list_models_returns_known_models() {
        let p = KiroProvider::new(client());
        let models = p.list_models().await.unwrap();
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
    }

    #[test]
    fn machine_identity_valid_from_refresh_token() {
        let p = KiroProvider::new(client());
        let id = p.machine_identity(&account_with(&[("refresh_token", "rt_xyz")]));
        assert!(id.is_valid_machine_id());
        assert!(id.user_agent.contains(&id.machine_id));
        assert!(id.user_agent.contains("KiroIDE-"));
    }

    #[test]
    fn cache_billing_from_cfg_parses_and_falls_back() {
        // 完整 cfg → 全部采用。
        let full = serde_json::json!({"read_multiplier": 2.5, "cap_ratio": 0.8, "floor_ratio": 0.2});
        let b = CacheBilling::from_cfg(&full);
        assert_eq!(b.read_multiplier, 2.5);
        assert_eq!(b.cap_ratio, 0.8);
        assert_eq!(b.floor_ratio, 0.2);
        // 部分缺失 → 缺的回退默认,有的采用。
        let partial = serde_json::json!({"cap_ratio": 0.7});
        let b = CacheBilling::from_cfg(&partial);
        assert_eq!(b.cap_ratio, 0.7);
        assert_eq!(b.read_multiplier, CacheBilling::default().read_multiplier);
        // Null / 非对象 → 全默认,不 panic。
        let b = CacheBilling::from_cfg(&serde_json::Value::Null);
        assert_eq!(b.read_multiplier, CacheBilling::default().read_multiplier);
        assert_eq!(b.floor_ratio, CacheBilling::default().floor_ratio);
    }

    #[test]
    fn validate_account_requires_credential() {
        let p = KiroProvider::new(client());
        // 无 refresh_token / kiro_api_key → 拒绝
        assert!(p.validate_account(&account_with(&[])).is_err());
        // 有 refresh_token → 通过
        assert!(p.validate_account(&account_with(&[("refresh_token", "x")])).is_ok());
    }

    #[tokio::test]
    async fn chat_without_access_token_errors_token_invalid() {
        let p = KiroProvider::new(client());
        let req = ChatRequest::from_anthropic_body(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let ctx = CallCtx {
            account: account_with(&[("refresh_token", "rt")]), // 无 access_token
            session_id: "s".into(),
            cache_key: "c".into(),
        };
        let err = p.chat(req, &ctx).await.err().expect("缺 access_token 应报错");
        assert_eq!(err.kind, gw_core::error::UpstreamErrorKind::TokenInvalid);
    }
}
