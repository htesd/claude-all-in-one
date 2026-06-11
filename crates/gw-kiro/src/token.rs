//! Kiro token 刷新 —— 🟢 适配旧 `token_manager.rs` 的 social/IdC 刷新流程。
//!
//! 与旧代码差异:① HTTP client 由 worker 的 egress 传入(不在此 build,保证出口IP
//! 一致);② 凭证字段来自 [`gw_core::account::Account`] 的 extra(而非 KiroCredentials);
//! ③ 不含 MultiTokenManager 调度状态机(那归 gw-app scheduler)。**URL / 请求头 /
//! body 形态逐字节对齐旧代码 + 真实金标准实测**(见 memory:social 刷新已实测 200)。
//!
//! 金标准(test-cred-free.json 实测):
//! - Social:`POST https://prod.{region}.auth.desktop.kiro.dev/refreshToken`,
//!   body `{"refreshToken": "..."}`,响应 `{accessToken, refreshToken, profileArn, expiresIn}`。
//! - **rolling token**:每次刷新返回新 refreshToken,调用方必须存盘新值。

use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use serde::{Deserialize, Serialize};

use crate::machine_id;

/// 默认 region(对齐旧 default_region)。
const DEFAULT_REGION: &str = "us-east-1";

/// 刷新得到的新凭证材料(调用方据此更新 Account.extra 并存盘)。
#[derive(Debug, Clone)]
pub struct RefreshedAuth {
    pub access_token: String,
    /// rolling:通常每次都返回新值,必须存盘覆盖旧 refresh_token。
    pub refresh_token: Option<String>,
    pub profile_arn: Option<String>,
    /// 过期时刻(RFC3339);由 expires_in 秒数换算。
    pub expires_at: Option<String>,
}

/// Social 刷新请求体(camelCase → `{"refreshToken": "..."}`)。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialRefreshRequest {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocialRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    profile_arn: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// IdC(AWS SSO OIDC)刷新请求体。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IdcRefreshRequest {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    grant_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdcRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    profile_arn: Option<String>,
}

/// 账号 auth 区域:`auth_region` > `region` > 默认 us-east-1(对齐旧 effective_auth_region)。
fn auth_region(account: &Account) -> String {
    account
        .extra_str("auth_region")
        .filter(|s| !s.is_empty())
        .or_else(|| account.extra_str("region").filter(|s| !s.is_empty()))
        .unwrap_or(DEFAULT_REGION)
        .to_string()
}

/// 是否 IdC 凭证:同时带非空 client_id + client_secret(对齐旧分流)。
fn is_idc(account: &Account) -> bool {
    account.extra_str("client_id").is_some_and(|s| !s.is_empty())
        && account
            .extra_str("client_secret")
            .is_some_and(|s| !s.is_empty())
}

/// 刷新账号 token。client 由调用方(worker)按 egress 提供,保证出口IP一致。
///
/// 自动按 client_id/secret 是否存在分流 social / IdC(对齐旧 refresh_token 分发)。
pub async fn refresh_auth(
    client: &reqwest::Client,
    account: &Account,
) -> Result<RefreshedAuth, UpstreamError> {
    let refresh_token = account
        .extra_str("refresh_token")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::TokenInvalid, "账号缺少 refresh_token"))?;

    if is_idc(account) {
        refresh_idc(client, account, refresh_token).await
    } else {
        refresh_social(client, account, refresh_token).await
    }
}

async fn refresh_social(
    client: &reqwest::Client,
    account: &Account,
    refresh_token: &str,
) -> Result<RefreshedAuth, UpstreamError> {
    let region = auth_region(account);
    let url = format!("https://prod.{region}.auth.desktop.kiro.dev/refreshToken");
    let domain = format!("prod.{region}.auth.desktop.kiro.dev");
    let machine = machine_id::generate_from_account(account);
    let version = crate::headers::kiro_version(account);

    let resp = client
        .post(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", format!("KiroIDE-{version}-{machine}"))
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &domain)
        .header("Connection", "close")
        .json(&SocialRefreshRequest {
            refresh_token: refresh_token.to_string(),
        })
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("social 刷新请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_refresh_error(status.as_u16(), &body, "social"));
    }

    let data: SocialRefreshResponse = resp
        .json()
        .await
        .map_err(|e| UpstreamError::network(format!("social 刷新响应解析失败: {e}")))?;

    Ok(RefreshedAuth {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        profile_arn: data.profile_arn,
        expires_at: data.expires_in.map(expires_at_rfc3339),
    })
}

async fn refresh_idc(
    client: &reqwest::Client,
    account: &Account,
    refresh_token: &str,
) -> Result<RefreshedAuth, UpstreamError> {
    let region = auth_region(account);
    let url = format!("https://oidc.{region}.amazonaws.com/token");
    let client_id = account.extra_str("client_id").unwrap_or_default();
    let client_secret = account.extra_str("client_secret").unwrap_or_default();
    // IdC 刷新头集合/顺序/UA 逐字对齐 static_flow refresh_idc(含 accept: */*)。
    let version = crate::headers::kiro_version(account);
    let rb = crate::headers::apply_idc_refresh_headers(client.post(&url), &region, &version);

    let resp = rb
        .json(&IdcRefreshRequest {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: refresh_token.to_string(),
            grant_type: "refresh_token".to_string(),
        })
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("IdC 刷新请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_refresh_error(status.as_u16(), &body, "IdC"));
    }

    let data: IdcRefreshResponse = resp
        .json()
        .await
        .map_err(|e| UpstreamError::network(format!("IdC 刷新响应解析失败: {e}")))?;

    Ok(RefreshedAuth {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        profile_arn: data.profile_arn,
        expires_at: data.expires_in.map(expires_at_rfc3339),
    })
}

/// 刷新错误分类(对齐旧代码:400+invalid_grant+Invalid refresh token = 永久失效)。
fn classify_refresh_error(status: u16, body: &str, flow: &str) -> UpstreamError {
    if status == 400
        && body.contains("\"invalid_grant\"")
        && body.contains("Invalid refresh token provided")
    {
        return UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            format!("{flow} refreshToken 已失效 (invalid_grant)"),
        )
        .with_status(status);
    }
    let kind = match status {
        401 | 403 => UpstreamErrorKind::TokenInvalid,
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    UpstreamError::new(kind, format!("{flow} token 刷新失败: {status} {body}")).with_status(status)
}

/// expires_in 秒 → RFC3339 过期时刻(UTC)。不引入 chrono,用 std 计算。
fn expires_at_rfc3339(expires_in: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let target = now + expires_in.max(0);
    // 简单 RFC3339(UTC):仅用于本地过期判断,精度到秒。
    format_unix_utc(target)
}

/// Unix 秒 → "YYYY-MM-DDTHH:MM:SSZ"(纯算术,避免 chrono 依赖)。
pub(crate) fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 起的天数 → 年月日(civil from days 算法,Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn acct(extra: &[(&str, &str)]) -> Account {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Account {
            account_id: "t".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: map,
        }
    }

    #[test]
    fn region_priority() {
        assert_eq!(auth_region(&acct(&[])), "us-east-1");
        assert_eq!(auth_region(&acct(&[("region", "eu-west-1")])), "eu-west-1");
        assert_eq!(
            auth_region(&acct(&[("region", "eu-west-1"), ("auth_region", "ap-southeast-1")])),
            "ap-southeast-1"
        );
    }

    #[test]
    fn idc_vs_social_split() {
        assert!(!is_idc(&acct(&[("refresh_token", "x")])), "无 client_id = social");
        assert!(
            is_idc(&acct(&[("client_id", "c"), ("client_secret", "s")])),
            "带 client_id+secret = IdC"
        );
        assert!(!is_idc(&acct(&[("client_id", "c")])), "只有 client_id 不算 IdC");
    }

    #[test]
    fn social_refresh_request_serializes_camelcase() {
        let body = serde_json::to_string(&SocialRefreshRequest {
            refresh_token: "rt123".into(),
        })
        .unwrap();
        assert_eq!(body, r#"{"refreshToken":"rt123"}"#);
    }

    #[test]
    fn classify_invalid_grant_is_token_invalid() {
        let body = r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#;
        let e = classify_refresh_error(400, body, "social");
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let e = classify_refresh_error(429, "slow down", "social");
        assert_eq!(e.kind, UpstreamErrorKind::RateLimited);
    }

    #[test]
    fn format_unix_utc_known_epoch() {
        // 2026-06-04T00:00:00Z = 1780531200
        assert_eq!(format_unix_utc(1_780_531_200), "2026-06-04T00:00:00Z");
        // epoch
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
    }
}
