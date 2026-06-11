//! Kiro 动态 profileArn 发现(`ListAvailableProfiles`)—— 🔵 对齐 static_flow
//! `kiro_refresh.rs::fetch_profile_arn_from_backend`。
//!
//! ## 为什么需要
//!
//! 企业 / 外部 IdP(external_idp)账号的 `generateAssistantResponse` 与 `getUsageLimits`
//! **都要求** profileArn,但它不在凭据里——social(github/google)和 BuilderId 有固定
//! 共享 ARN 可兜底([`crate::headers::fixed_profile_arn`]),企业/IdC 号则必须**运行时**
//! 向后端 `ListAvailableProfiles` 查询。kiro.rs 没实现这步(依赖导入时凭据自带 profileArn),
//! 本模块补上,让企业号即插即用。
//!
//! ## 端点
//!
//! `POST https://q.{region}.amazonaws.com/ListAvailableProfiles`,body `{nextToken?}`,
//! runtime UA(与 getUsageLimits 同 client),响应 `{profiles:[{arn}], nextToken}`。
//! 跨候选区域查询(IdC 按 auth region 的 partition 过滤;external_idp 查全部标准区),
//! 取第一个非空 arn。只读发现调用,**不发推理包**,安全。

use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use serde::{Deserialize, Serialize};

use crate::headers;
use crate::machine_id;
use crate::usage_limits::runtime_user_agents;

/// 标准商业区(对齐 static_flow `KIRO_STANDARD_PROFILE_REGIONS`)。
const STANDARD_PROFILE_REGIONS: &[&str] = &["us-east-1", "eu-central-1"];

/// 发现该账号的 profileArn(若需要)。返回:
/// - `Ok(None)`:账号已有显式 profile_arn,或可用固定兜底(social/builderid)——无需发现;
///   或后端无可用 profile。
/// - `Ok(Some(arn))`:经 ListAvailableProfiles 发现的 profileArn(调用方应持久化进账号)。
///
/// account 须已带有效 access_token(调用方先 ensure_credentialed)。
pub async fn discover_profile_arn(
    client: &reqwest::Client,
    account: &Account,
) -> Result<Option<String>, UpstreamError> {
    // 已有显式 profileArn 或固定兜底 → 不需要发现(省一次网络)。
    if account
        .extra_str("profile_arn")
        .is_some_and(|s| !s.trim().is_empty())
        || headers::fixed_profile_arn(account).is_some()
    {
        return Ok(None);
    }

    let access_token = account
        .extra_str("access_token")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "ListAvailableProfiles 缺少 access_token",
            )
        })?;

    let machine = machine_id::generate_from_account(account);
    let version = headers::kiro_version(account);
    let (x_amz_ua, ua) = runtime_user_agents(&version, &machine);

    // 候选区域:逐个查,第一个返回非空 arn 即用。单区失败(网络/4xx)跳过下一区,
    // 全部失败才向上报错(让调用方据 kind 决定惩罚与否)。
    let mut last_err: Option<UpstreamError> = None;
    for region in candidate_regions(account) {
        match fetch_first_profile(client, &region, access_token, &x_amz_ua, &ua).await {
            Ok(Some(arn)) => return Ok(Some(arn)),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(None), // 所有区都成功返回但无 profile。
    }
}

/// 候选查询区域:IdC 账号优先其 auth region,再补标准区(去重);其余用标准区。
/// (简化版 static_flow partition 过滤:商业区单一 partition,够用。)
fn candidate_regions(account: &Account) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(r) = account.extra_str("region").filter(|s| !s.trim().is_empty()) {
        out.push(r.to_string());
    }
    for r in STANDARD_PROFILE_REGIONS {
        if !out.iter().any(|x| x == r) {
            out.push((*r).to_string());
        }
    }
    out
}

/// 查单个区域的 ListAvailableProfiles,翻页取第一个非空 arn。
async fn fetch_first_profile(
    client: &reqwest::Client,
    region: &str,
    access_token: &str,
    x_amz_ua: &str,
    ua: &str,
) -> Result<Option<String>, UpstreamError> {
    let host = format!("q.{region}.amazonaws.com");
    let url = format!("https://{host}/ListAvailableProfiles");
    let mut next_token: Option<String> = None;

    loop {
        let resp = client
            .post(&url)
            .header("x-amz-user-agent", x_amz_ua)
            .header("user-agent", ua)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {access_token}"))
            .header("connection", "close")
            .json(&ListAvailableProfilesRequest {
                next_token: next_token.clone(),
            })
            .send()
            .await
            .map_err(|e| UpstreamError::network(format!("ListAvailableProfiles 请求失败: {e}")))?;

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
                format!("ListAvailableProfiles 失败: {} {}", status.as_u16(), body),
            )
            .with_status(status.as_u16()));
        }

        let payload: ListAvailableProfilesResponse = resp
            .json()
            .await
            .map_err(|e| UpstreamError::network(format!("ListAvailableProfiles 解析失败: {e}")))?;

        if let Some(arn) = payload
            .profiles
            .into_iter()
            .filter_map(|p| p.arn)
            .map(|a| a.trim().to_string())
            .find(|a| !a.is_empty())
        {
            return Ok(Some(arn));
        }

        match payload
            .next_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(t) => next_token = Some(t.to_string()),
            None => return Ok(None),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListAvailableProfilesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAvailableProfilesResponse {
    #[serde(default)]
    profiles: Vec<ListAvailableProfile>,
    #[serde(default)]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAvailableProfile {
    #[serde(default)]
    arn: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn acct(extra: &[(&str, &str)]) -> Arc<Account> {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Arc::new(Account {
            account_id: "k".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: map,
        })
    }

    #[tokio::test]
    async fn skips_discovery_when_explicit_profile_arn() {
        let a = acct(&[("profile_arn", "arn:aws:codewhisperer:us-east-1:1:profile/X"), ("access_token", "t")]);
        assert_eq!(discover_profile_arn(&reqwest::Client::new(), &a).await.unwrap(), None);
    }

    #[tokio::test]
    async fn skips_discovery_when_fixed_fallback_applies() {
        // github → 有固定 social ARN 兜底,无需发现。
        let a = acct(&[("kiro_provider", "github"), ("access_token", "t")]);
        assert_eq!(discover_profile_arn(&reqwest::Client::new(), &a).await.unwrap(), None);
    }

    #[test]
    fn candidate_regions_dedups_and_prioritizes_account_region() {
        let a = acct(&[("region", "us-east-1")]);
        assert_eq!(candidate_regions(&a), vec!["us-east-1", "eu-central-1"]);
        let a2 = acct(&[("region", "ap-southeast-1")]);
        assert_eq!(candidate_regions(&a2), vec!["ap-southeast-1", "us-east-1", "eu-central-1"]);
        let a3 = acct(&[]);
        assert_eq!(candidate_regions(&a3), vec!["us-east-1", "eu-central-1"]);
    }
}
