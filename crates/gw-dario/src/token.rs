use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use crate::format_rfc3339_z;

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 将刷新响应写回账号(纯函数,注入 now_unix 便于测试)。
/// - 有 access_token 则覆盖;无则 Err(TokenInvalid)。
/// - 有 refresh_token 才覆盖(没有就保留旧的)。
/// - 有 expires_in(秒)才写 expires_at(RFC3339 Z)。
pub(crate) fn apply_refresh(
    mut account: Account,
    resp: &serde_json::Value,
    now_unix: i64,
) -> Result<Account, UpstreamError> {
    let access = resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let e = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("no access_token");
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                format!("dario refresh 失败: {e}"),
            )
        })?;
    account
        .extra
        .insert("access_token".into(), serde_json::json!(access));
    if let Some(rt) = resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        account
            .extra
            .insert("refresh_token".into(), serde_json::json!(rt));
    }
    if let Some(ttl) = resp.get("expires_in").and_then(|v| v.as_i64()) {
        account.extra.insert(
            "expires_at".into(),
            serde_json::json!(format_rfc3339_z(now_unix + ttl)),
        );
    }
    Ok(account)
}

/// 刷新端点非 2xx 的分类:瞬时(429/5xx)≠ 永久禁号(invalid_grant)。
/// report_failure(TokenInvalid)=永久禁号(disabled_until=None),不能给瞬时错误。
pub(crate) fn classify_refresh_status(code: u16) -> UpstreamErrorKind {
    match code {
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::TokenInvalid, // 400/401 invalid_grant 等:真死 token
    }
}

/// OAuth refresh_token → 新 access_token。
/// POST https://platform.claude.com/v1/oauth/token,Content-Type: application/x-www-form-urlencoded,
/// 恰 3 字段:grant_type / refresh_token / client_id(无 client_secret,纯 PKCE)。
pub(crate) async fn refresh(
    client: &reqwest::Client,
    account: &Account,
) -> Result<Account, UpstreamError> {
    let refresh_token = account
        .extra_str("refresh_token")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "dario account missing refresh_token",
            )
        })?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("dario refresh 连接失败: {e}")))?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let err = json
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(
            UpstreamError::new(
                classify_refresh_status(status.as_u16()),
                format!("dario refresh {} {err}", status.as_u16()),
            )
            .with_status(status.as_u16()),
        );
    }
    apply_refresh(account.clone(), &json, now_unix())
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::error::UpstreamErrorKind;
    use std::collections::BTreeMap;

    fn acct() -> Account {
        let mut e = BTreeMap::new();
        e.insert("refresh_token".into(), serde_json::json!("old-rt"));
        e.insert("access_token".into(), serde_json::json!("old-at"));
        Account {
            account_id: "d1".into(),
            provider: "claude-dario".into(),
            max_concurrency: 2,
            disabled: false,
            extra: e,
        }
    }

    #[test]
    fn apply_updates_tokens_and_expiry_z() {
        let r = serde_json::json!({"access_token":"new-at","refresh_token":"new-rt","expires_in":3600});
        let u = apply_refresh(acct(), &r, 1_780_531_200).unwrap();
        assert_eq!(u.extra_str("access_token"), Some("new-at"));
        assert_eq!(u.extra_str("refresh_token"), Some("new-rt"));
        // 1_780_531_200 + 3600 = 1_780_534_800 = 2026-06-04T01:00:00Z
        assert_eq!(u.extra_str("expires_at"), Some("2026-06-04T01:00:00Z"));
    }

    #[test]
    fn apply_keeps_old_refresh_when_absent() {
        let r = serde_json::json!({"access_token":"new-at","expires_in":3600});
        assert_eq!(
            apply_refresh(acct(), &r, 0).unwrap().extra_str("refresh_token"),
            Some("old-rt")
        );
    }

    #[test]
    fn apply_errors_without_access_token() {
        assert!(
            apply_refresh(acct(), &serde_json::json!({"error":"invalid_grant"}), 0).is_err()
        );
    }

    #[test]
    fn classify_transient_vs_permanent() {
        assert_eq!(classify_refresh_status(429), UpstreamErrorKind::RateLimited);
        assert_eq!(classify_refresh_status(503), UpstreamErrorKind::ServerError);
        assert_eq!(classify_refresh_status(400), UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_refresh_status(401), UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn apply_no_expires_in_leaves_expires_at_unset() {
        let r = serde_json::json!({"access_token":"new-at"});
        let u = apply_refresh(acct(), &r, 99999).unwrap();
        assert!(u.extra_str("expires_at").is_none());
    }
}
