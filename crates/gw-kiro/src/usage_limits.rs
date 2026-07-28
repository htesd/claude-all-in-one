//! Kiro 配额查询(`getUsageLimits`)—— 🔵 移植 kiro.rs，**只读**。
//!
//! 用于 admin 账号页展示"已用 / 上限 / 剩余积分"。这是只读 GET,不发推理包、
//! 不触发计费 —— 用户已确认这类查询不会招封号(见 memory:no-chat-test-on-real-accounts)。
//!
//! 金标准 = **拆包 Kiro 1.0.212 真实客户端**(2026-07-28 重新对齐,详见 [`quota_endpoint`]):
//! - `GET https://management.{region}.kiro.dev/Get-Usage-Limits?origin=AI_EDITOR[&profileArn=...]`
//! - control-plane UA(`api/kirocontrolplanebearer#1.0.0`)。
//! - 累加 base + 激活的 freeTrial/bonus 得总 used / limit(对齐 kiro.rs 便捷方法)。
//!
//! 旧形态(`q.{region}.amazonaws.com/getUsageLimits`,带 `resourceType`)保留在
//! `KIRO_LEGACY_QUOTA_ENDPOINT=1` 后面 —— 它在 1.0.212 的 bundle 里虽然仍有 SDK 代码,
//! 但**没有任何调用点**(全树只有一处 `new yh(`,走的是 control-plane)。

use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::AccountQuota;
use serde::Deserialize;

use crate::headers;
use crate::machine_id;

const DEFAULT_REGION: &str = "us-east-1";

/// 查询账号配额。account 须已带有效 access_token(调用方先 ensure_credentialed)。
pub async fn get_account_quota(
    client: &reqwest::Client,
    account: &Account,
) -> Result<AccountQuota, UpstreamError> {
    let access_token = headers::bearer_token(account).ok_or_else(|| {
        UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            "配额查询缺少凭据(access_token / kiro_api_key)",
        )
    })?;

    let region = account
        .extra_str("region")
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGION);
    let ep = quota_endpoint(region);
    let host = ep.host;
    let machine = machine_id::generate_from_account(account);
    let version = headers::kiro_version(account);
    let (x_amz_ua, ua) = if ep.legacy {
        runtime_user_agents(&version, &machine)
    } else {
        control_plane_user_agents(&version, &machine)
    };

    // 用 reqwest::Url 拼 query(自动 percent-encode profileArn 里的 `:` `/`)。
    let mut url = reqwest::Url::parse(&format!("https://{host}{}", ep.path))
        .map_err(|e| UpstreamError::network(format!("配额 URL 构造失败: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("origin", "AI_EDITOR");
        // `resourceType` 只在旧端点发 —— 1.0.212 的 control-plane 调用只传
        // `{origin, profileArn}`(`extension.js:337368-337371`),多发一个参数就是
        // 一个每次轮询都出现的稳定差异。
        if ep.legacy {
            q.append_pair("resourceType", "AGENTIC_REQUEST");
        }
        if let Some(arn) = headers::resolve_profile_arn(account) {
            q.append_pair("profileArn", &arn);
        }
    }

    let rb = client
        .get(url)
        .header("x-amz-user-agent", x_amz_ua)
        .header("user-agent", ua)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("authorization", format!("Bearer {access_token}"))
        .header("connection", "close");
    // external_idp(Azure AD)号必须带 TokenType 头,否则 getUsageLimits 吃 403。
    let rb = headers::apply_external_idp_token_type(rb, account);
    // API Key 凭据同理:带 `TokenType: API_KEY`,否则上游按 OAuth 处理、报 400「Invalid
    // profileArn」(实测)。与 external_idp 互斥,bearer 已由 bearer_token 取自 kiro_api_key。
    let rb = if crate::machine_id::is_api_key_credential(account) {
        rb.header("TokenType", "API_KEY")
    } else {
        rb
    };
    let resp = rb
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("配额查询请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let kind = match status.as_u16() {
            // 封禁 403(TEMPORARILY_SUSPENDED 等)→ TemporarilyBlocked:冷却自愈、不永久禁号,
            // 且不命中 try_fetch_quota 的 TokenInvalid 兜底(避免对封号又多打刷新/ListProfiles
            // 制造异常指纹)。对齐 error_map.rs/token.rs 已有的 suspend 检测——此前 usage_limits
            // 缺这道:封号做配额查询被误判 TokenInvalid → report_failure 永久禁用本可 1h 自愈的号。
            403 if crate::error_map::is_account_suspended(&body) => {
                UpstreamErrorKind::TemporarilyBlocked
            }
            401 | 403 => UpstreamErrorKind::TokenInvalid,
            429 => UpstreamErrorKind::RateLimited,
            500..=599 => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        };
        return Err(UpstreamError::new(
            kind,
            format!("配额查询失败: {} {}", status.as_u16(), body),
        )
        .with_status(status.as_u16()));
    }

    let data: UsageLimitsResponse = resp
        .json()
        .await
        .map_err(|e| UpstreamError::network(format!("配额响应解析失败: {e}")))?;

    Ok(data.into_account_quota())
}

/// 配额查询的目标端点。
pub(crate) struct QuotaEndpoint {
    pub host: String,
    /// 含前导斜杠的路径。**大小写与连字符都是有意义的**(Smithy `@http` trait 逐字)。
    pub path: &'static str,
    /// 是否为旧 CodeWhisperer 形态(决定 UA 与是否发 `resourceType`)。
    pub legacy: bool,
}

/// 解析配额端点。默认走 1.0.212 真实客户端的 control-plane 形态;
/// `KIRO_LEGACY_QUOTA_ENDPOINT=1` 切回旧的 `q.*.amazonaws.com`(应急回退,不改镜像)。
///
/// 证据(`extensions/kiro.kiro-agent/dist/extension.js`,1.0.212):
/// - 域名解析器 `bi2` `:217247` → `https://management.${region}.kiro.dev`;
/// - 操作 schema `:218514` → `{ http: ["GET", "/Get-Usage-Limits", 200] }`
///   (注意是**连字符大驼峰**路径,不是 `/getUsageLimits`);
/// - 唯一调用点 `:337368` `new yh({origin, profileArn})`,走
///   `KiroControlPlaneBearerService`;
/// - bundle 里那份 `AmazonCodeWhispererService.GetUsageLimits`(`:420314`)
///   **全树零调用点** —— 老端点只是还没下线,真客户端已经不打了。
pub(crate) fn quota_endpoint(region: &str) -> QuotaEndpoint {
    quota_endpoint_from(region, gw_core::env_flag("KIRO_LEGACY_QUOTA_ENDPOINT"))
}

/// 纯逻辑(开关注入便于测试;读 env 的测试会与并行用例互相污染)。
fn quota_endpoint_from(region: &str, legacy: bool) -> QuotaEndpoint {
    if legacy {
        QuotaEndpoint {
            host: format!("q.{region}.amazonaws.com"),
            path: "/getUsageLimits",
            legacy: true,
        }
    } else {
        QuotaEndpoint {
            host: format!("management.{region}.kiro.dev"),
            path: "/Get-Usage-Limits",
            legacy: false,
        }
    }
}

/// control-plane UA 对(x-amz-user-agent, user-agent)。
///
/// AWS SDK JS 的 UA 中间件(`extension.js:356884-356910`)按固定顺序拼:
/// `aws-sdk-js/{clientVersion} ua/2.1 os/… lang/js md/nodejs#… api/{serviceId}#{clientVersion}
///  m/{features} {customUserAgent}`,其中 `api/` 段会被**强制小写**(`:356925`),
/// `customUserAgent` 里的空格会被转成 `-`(`:356844-356846`)——所以尾巴仍是
/// `KiroIDE-{ver}-{machineId}` 而不是空格分隔。
///
/// 本 client 的 `serviceId = "KiroControlPlaneBearer"`(`:218559`)、
/// 包版本 `1.0.0`(`@amzn/kiro-control-plane-bearer-client`,`:217301-217303`)。
pub(crate) fn control_plane_user_agents(version: &str, machine_id: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/1.0.0 KiroIDE-{version}-{machine_id}");
    let ua = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/kirocontrolplanebearer#1.0.0 \
         m/N,E KiroIDE-{version}-{machine_id}",
        headers::DEFAULT_SYSTEM_VERSION,
        headers::DEFAULT_NODE_VERSION,
    );
    (x_amz, ua)
}

/// 配额查询用的 runtime UA 对(x-amz-user-agent, user-agent)。对齐 kiro.rs:
/// 用 `api/codewhispererruntime#1.0.0 m/N,E`(非 streaming 的 codewhispererstreaming)。
/// `pub(crate)`:ListAvailableProfiles(profiles 模块)同走 runtime UA,复用;
/// 配额路径只在 `KIRO_LEGACY_QUOTA_ENDPOINT=1` 回退时才用它。
pub(crate) fn runtime_user_agents(version: &str, machine_id: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/1.0.0 KiroIDE-{version}-{machine_id}");
    let ua = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E \
         KiroIDE-{version}-{machine_id}",
        headers::DEFAULT_SYSTEM_VERSION,
        headers::DEFAULT_NODE_VERSION,
    );
    (x_amz, ua)
}

// ===== 响应模型(🔵 移植 kiro.rs usage_limits.rs,只保留计算所需字段)=====

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageLimitsResponse {
    #[serde(default)]
    subscription_info: Option<SubscriptionInfo>,
    #[serde(default)]
    usage_breakdown_list: Vec<UsageBreakdown>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionInfo {
    #[serde(default)]
    subscription_title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageBreakdown {
    #[serde(default)]
    current_usage_with_precision: f64,
    #[serde(default)]
    bonuses: Vec<Bonus>,
    #[serde(default)]
    free_trial_info: Option<FreeTrialInfo>,
    #[serde(default)]
    usage_limit_with_precision: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bonus {
    #[serde(default)]
    current_usage: f64,
    #[serde(default)]
    usage_limit: f64,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FreeTrialInfo {
    #[serde(default)]
    current_usage_with_precision: f64,
    #[serde(default)]
    free_trial_status: Option<String>,
    #[serde(default)]
    usage_limit_with_precision: f64,
}

impl UsageLimitsResponse {
    /// 总上限 = base + 激活的 freeTrial + 激活的 bonus(对齐 kiro.rs usage_limit())。
    fn total_limit(&self) -> f64 {
        let Some(b) = self.usage_breakdown_list.first() else {
            return 0.0;
        };
        let mut total = b.usage_limit_with_precision;
        if let Some(t) = &b.free_trial_info {
            if t.free_trial_status.as_deref() == Some("ACTIVE") {
                total += t.usage_limit_with_precision;
            }
        }
        for bonus in &b.bonuses {
            if bonus.status.as_deref() == Some("ACTIVE") {
                total += bonus.usage_limit;
            }
        }
        total
    }

    /// 总已用 = base + 激活的 freeTrial + 激活的 bonus(对齐 kiro.rs current_usage())。
    fn total_used(&self) -> f64 {
        let Some(b) = self.usage_breakdown_list.first() else {
            return 0.0;
        };
        let mut total = b.current_usage_with_precision;
        if let Some(t) = &b.free_trial_info {
            if t.free_trial_status.as_deref() == Some("ACTIVE") {
                total += t.current_usage_with_precision;
            }
        }
        for bonus in &b.bonuses {
            if bonus.status.as_deref() == Some("ACTIVE") {
                total += bonus.current_usage;
            }
        }
        total
    }

    fn into_account_quota(self) -> AccountQuota {
        let mut q = AccountQuota::from_used_limit(self.total_used(), self.total_limit());
        q.currency = self
            .subscription_info
            .and_then(|s| s.subscription_title);
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端点形态逐字对齐 1.0.212(`extension.js:217247` 域名 + `:218514` 的 http trait)。
    /// 路径大小写/连字符是 Smithy `@http` 里写死的,写错了不是"风格问题"而是 404。
    #[test]
    fn quota_endpoint_defaults_to_control_plane() {
        let ep = quota_endpoint_from("us-east-1", false);
        assert_eq!(ep.host, "management.us-east-1.kiro.dev");
        assert_eq!(ep.path, "/Get-Usage-Limits", "连字符大驼峰路径,不是 /getUsageLimits");
        assert!(!ep.legacy);
        // region 参与域名拼装,不能写死 us-east-1。
        assert_eq!(quota_endpoint_from("eu-west-1", false).host, "management.eu-west-1.kiro.dev");
        // 回退开关仍能拿到旧形态(应急用,不改镜像)。
        let old = quota_endpoint_from("us-east-1", true);
        assert_eq!(old.host, "q.us-east-1.amazonaws.com");
        assert_eq!(old.path, "/getUsageLimits");
        assert!(old.legacy, "legacy 标志决定是否发 resourceType 与用哪套 UA");
    }

    #[test]
    fn control_plane_ua_matches_sdk_assembly_order() {
        let (x_amz, ua) = control_plane_user_agents("1.0.212", &"a".repeat(64));
        // x-amz-user-agent 只含 aws-sdk-* 段 + customUserAgent(:356902-356905)。
        assert_eq!(x_amz, format!("aws-sdk-js/1.0.0 KiroIDE-1.0.212-{}", "a".repeat(64)));
        // api/ 段被 SDK 强制小写(:356925),且用的是 control-plane 的 serviceId。
        assert!(ua.contains("api/kirocontrolplanebearer#1.0.0"), "实际={ua}");
        assert!(!ua.contains("codewhispererruntime"), "别把 runtime 的 serviceId 带到控制面: {ua}");
        // 顺序不变量:api/ 段在 m/ 之前,customUserAgent 恒在最末(:356894/:356901)。
        let api = ua.find("api/").expect("缺 api 段");
        let m = ua.find(" m/").expect("缺 m 段");
        let kiro = ua.find("KiroIDE-").expect("缺 KiroIDE 尾巴");
        assert!(api < m && m < kiro, "UA 段序须为 api → m → KiroIDE,实际={ua}");
    }

    #[test]
    fn runtime_ua_matches_kiro_rs_shape() {
        let (x_amz, ua) = runtime_user_agents("0.12.155", &"a".repeat(64));
        assert_eq!(x_amz, format!("aws-sdk-js/1.0.0 KiroIDE-0.12.155-{}", "a".repeat(64)));
        assert!(ua.contains("api/codewhispererruntime#1.0.0 m/N,E"), "应为 runtime UA: {ua}");
        assert!(ua.contains(&format!("KiroIDE-0.12.155-{}", "a".repeat(64))));
    }

    #[test]
    fn parses_and_sums_quota() {
        // 基础 1000 / 已用 10236.75(PRO 超额场景),无激活 bonus。
        let json = serde_json::json!({
            "usageBreakdownList": [{
                "currentUsageWithPrecision": 10236.75,
                "usageLimitWithPrecision": 1000.0,
                "bonuses": [],
                "freeTrialInfo": null
            }],
            "subscriptionInfo": {"subscriptionTitle": "KIRO PRO"}
        });
        let resp: UsageLimitsResponse = serde_json::from_value(json).unwrap();
        let q = resp.into_account_quota();
        assert_eq!(q.used, 10236.75);
        assert_eq!(q.limit, 1000.0);
        assert_eq!(q.remaining, -9236.75, "超额 remaining 为负=已超出多少(不再 clamp 到 0)");
        assert!(q.percent_used > 1000.0, "已用 1023%");
        assert_eq!(q.currency.as_deref(), Some("KIRO PRO"));
    }

    #[test]
    fn sums_active_bonus_and_skips_expired() {
        let json = serde_json::json!({
            "usageBreakdownList": [{
                "currentUsageWithPrecision": 100.0,
                "usageLimitWithPrecision": 1000.0,
                "bonuses": [
                    {"currentUsage": 50.0, "usageLimit": 500.0, "status": "ACTIVE"},
                    {"currentUsage": 9.0, "usageLimit": 999.0, "status": "EXPIRED"}
                ],
                "freeTrialInfo": null
            }]
        });
        let resp: UsageLimitsResponse = serde_json::from_value(json).unwrap();
        let q = resp.into_account_quota();
        assert_eq!(q.limit, 1500.0, "base 1000 + 激活 bonus 500");
        assert_eq!(q.used, 150.0, "base 100 + 激活 bonus 50");
        assert_eq!(q.remaining, 1350.0);
    }

    #[test]
    fn empty_breakdown_is_zero() {
        let resp: UsageLimitsResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        let q = resp.into_account_quota();
        assert_eq!(q.used, 0.0);
        assert_eq!(q.limit, 0.0);
        assert_eq!(q.remaining, 0.0);
    }
}
