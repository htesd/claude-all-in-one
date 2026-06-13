//! Kiro 配额查询(`getUsageLimits`)—— 🔵 移植 kiro.rs，**只读**。
//!
//! 用于 admin 账号页展示"已用 / 上限 / 剩余积分"。这是只读 GET,不发推理包、
//! 不触发计费 —— 用户已确认这类查询不会招封号(见 memory:no-chat-test-on-real-accounts)。
//!
//! 金标准对齐 kiro.rs `token_manager.rs get_usage_limits`:
//! - `GET https://q.{region}.amazonaws.com/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST[&profileArn=...]`
//! - runtime UA(`api/codewhispererruntime#1.0.0 m/N,E`,与发包的 streaming UA 不同 client,
//!   但 machineId 同一冻结值 → 同设备)。
//! - 累加 base + 激活的 freeTrial/bonus 得总 used / limit(对齐 kiro.rs 便捷方法)。

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
    let access_token = account
        .extra_str("access_token")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(UpstreamErrorKind::TokenInvalid, "配额查询缺少 access_token")
        })?;

    let region = account
        .extra_str("region")
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGION);
    let host = format!("q.{region}.amazonaws.com");
    let machine = machine_id::generate_from_account(account);
    let version = headers::kiro_version(account);
    let (x_amz_ua, ua) = runtime_user_agents(&version, &machine);

    // 用 reqwest::Url 拼 query(自动 percent-encode profileArn 里的 `:` `/`)。
    let mut url = reqwest::Url::parse(&format!("https://{host}/getUsageLimits"))
        .map_err(|e| UpstreamError::network(format!("配额 URL 构造失败: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("origin", "AI_EDITOR");
        q.append_pair("resourceType", "AGENTIC_REQUEST");
        if let Some(arn) = headers::resolve_profile_arn(account) {
            q.append_pair("profileArn", &arn);
        }
    }

    let resp = client
        .get(url)
        .header("x-amz-user-agent", x_amz_ua)
        .header("user-agent", ua)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("authorization", format!("Bearer {access_token}"))
        .header("connection", "close")
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("配额查询请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let kind = match status.as_u16() {
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

/// 配额查询用的 runtime UA 对(x-amz-user-agent, user-agent)。对齐 kiro.rs:
/// 用 `api/codewhispererruntime#1.0.0 m/N,E`(非 streaming 的 codewhispererstreaming)。
/// `pub(crate)`:ListAvailableProfiles(profiles 模块)同走 runtime UA,复用。
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
