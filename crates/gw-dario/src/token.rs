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

/// 刷新端点非 2xx 的分类:只有 `invalid_grant`(或等价撤销)才永久禁号。
///
/// 旧实现把所有 4xx 均映射 `TokenInvalid` = 永久禁号。但 WAF 502/400、
/// `server_error`、`temporarily_unavailable` 等都是瞬时错误,永久禁号会
/// 白白丢掉健康账号。只有响应体 `error` 字段明确为 `invalid_grant`(或
/// `token_revoked`)才表示 token 已被真正撤销。
///
/// 调用方须已将响应体 parse 为 `serde_json::Value`(失败时传 `Null` 即可,
/// 视为"非 invalid_grant" → `Other`)。
pub(crate) fn classify_refresh(code: u16, body: &serde_json::Value) -> UpstreamErrorKind {
    match code {
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => {
            // Permanent revocation signals.  Anything else (server_error,
            // temporarily_unavailable, HTML WAF pages → Null body, etc.) is
            // treated as transient (Other) to avoid permanently banning
            // accounts that just hit a momentary upstream hiccup.
            let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
            if err == "invalid_grant" || err == "token_revoked" {
                UpstreamErrorKind::TokenInvalid
            } else {
                UpstreamErrorKind::Other
            }
        }
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
                classify_refresh(status.as_u16(), &json),
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
        // 429 / 5xx are always transient regardless of body.
        assert_eq!(classify_refresh(429, &serde_json::Value::Null), UpstreamErrorKind::RateLimited);
        assert_eq!(classify_refresh(503, &serde_json::Value::Null), UpstreamErrorKind::ServerError);
        // 400 + invalid_grant → permanent (token truly revoked).
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::TokenInvalid
        );
        // 400/401 without invalid_grant → Other (transient; do NOT ban the account).
        assert_eq!(classify_refresh(400, &serde_json::Value::Null), UpstreamErrorKind::Other);
        assert_eq!(classify_refresh(401, &serde_json::Value::Null), UpstreamErrorKind::Other);
    }

    #[test]
    fn classify_refresh_only_invalid_grant_is_permanent() {
        // Explicit invalid_grant → TokenInvalid (permanent ban).
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::TokenInvalid
        );
        // token_revoked is another well-known permanent revocation signal.
        assert_eq!(
            classify_refresh(401, &serde_json::json!({"error": "token_revoked"})),
            UpstreamErrorKind::TokenInvalid
        );
        // server_error (transient upstream failure) → Other, NOT TokenInvalid.
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "server_error"})),
            UpstreamErrorKind::Other
        );
        // temporarily_unavailable (e.g. maintenance) → Other.
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "temporarily_unavailable"})),
            UpstreamErrorKind::Other
        );
        // Non-JSON body (WAF HTML → parsed as Null) → Other.
        assert_eq!(
            classify_refresh(401, &serde_json::Value::Null),
            UpstreamErrorKind::Other
        );
        // 429 → RateLimited regardless of body.
        assert_eq!(
            classify_refresh(429, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::RateLimited
        );
        // 503 → ServerError regardless of body.
        assert_eq!(
            classify_refresh(503, &serde_json::Value::Null),
            UpstreamErrorKind::ServerError
        );
    }

    #[test]
    fn apply_no_expires_in_leaves_expires_at_unset() {
        let r = serde_json::json!({"access_token":"new-at"});
        let u = apply_refresh(acct(), &r, 99999).unwrap();
        assert!(u.extra_str("expires_at").is_none());
    }
}
