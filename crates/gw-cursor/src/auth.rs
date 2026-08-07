//! Cursor session token 刷新(`refresh_auth` 的实现)。
//!
//! 逆向自本机 Cursor 3.14.27 的 `workbench.desktop.main.js`(`_performAccessTokenRefresh`)。
//! 是一条标准 OAuth2 refresh_token 流,不是 ConnectRPC:
//!
//! ```text
//! POST https://api2.cursor.sh/oauth/token
//! content-type: application/json
//! x-cursor-client-type: glass
//! {"grant_type":"refresh_token","client_id":"<常量>","refresh_token":"<rt>"}
//! ```
//!
//! ## 两个反直觉但实测如此的点
//!
//! **① 新的 access_token 同时就是新的 refresh_token。** 客户端拿到响应后调的是
//! `storeAccessRefreshToken(c.access_token, c.access_token)` —— **两个参数是同一个值**。
//! 所以 `state.vscdb` 里 `cursorAuth/accessToken` 与 `cursorAuth/refreshToken` 长度
//! 一模一样(本机实测都是 415 字符),它们本来就是同一个 JWT。
//! 别去响应里找 `refresh_token` 字段,没有那个字段。
//!
//! **② 刷新余量极大:1272 小时 ≈ 53 天。** 客户端的判断是
//! `exp*1000 - now > 1272h` 就跳过刷新。也就是说 token 有效期很长(约 60 天),
//! 只在剩余不足 53 天时才换。所以 `refresh_auth` 不需要高频调用。
//!
//! ## 出口纪律
//!
//! 刷新**必须与推理走同一个出口**(PROTOCOL §7)。调用方要传该账号的代理 client,
//! 不能图省事用进程默认 client —— 刷新 IP ≠ 发包 IP 是已知的关联封号维度。

use gw_core::error::{UpstreamError, UpstreamErrorKind};

/// OAuth token 端点。注意是 `api2.cursor.sh`,与推理用的 `agentn.api5.cursor.sh` 不是一个域。
pub const OAUTH_TOKEN_URL: &str = "https://api2.cursor.sh/oauth/token";

/// 桌面端 OAuth client_id(bundle 里的 `authClientId` 常量,非密钥,随客户端分发)。
pub const AUTH_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";

/// 客户端自报类型。与 Run 请求头一致取 `glass`(bundle:`isGlass ? "glass" : "ide"`)。
const CLIENT_TYPE: &str = "glass";

/// 真客户端用 20 秒 AbortController。
const TIMEOUT_SECS: u64 = 20;

/// 客户端认为「还早,不用换」的余量:1272 小时。
pub const REFRESH_MARGIN_SECS: i64 = 1272 * 60 * 60;

/// 刷新结果。两个字段值相同 —— 见模块头 ①。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: String,
}

/// 用 refresh_token 换一份新凭据。
///
/// `client` 必须是**该账号专属出口**的 client。
pub async fn refresh(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<Refreshed, UpstreamError> {
    let rt = refresh_token.trim();
    if rt.is_empty() {
        return Err(UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            "cursor 账号没有 refresh_token,无法刷新",
        ));
    }

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .header("content-type", "application/json")
        .header("x-cursor-client-type", CLIENT_TYPE)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": AUTH_CLIENT_ID,
            "refresh_token": rt,
        }))
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("Cursor 刷新 token 请求失败: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| UpstreamError::network(format!("读 Cursor 刷新响应失败: {e}")))?;

    if !status.is_success() {
        let kind = match status.as_u16() {
            400 | 401 | 403 => UpstreamErrorKind::TokenInvalid,
            429 => UpstreamErrorKind::RateLimited,
            500..=599 => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        };
        return Err(UpstreamError::new(
            kind,
            format!(
                "Cursor 刷新 token {}: {}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            ),
        )
        .with_status(status.as_u16()));
    }

    parse_refresh_response(&text)
}

/// 解析刷新响应体。
///
/// 单独拆出来是为了能在没有网络的单测里覆盖三条分支(正常 / shouldLogout / 缺字段)。
pub fn parse_refresh_response(body: &str) -> Result<Refreshed, UpstreamError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("Cursor 刷新响应不是 JSON: {e}"),
        )
    })?;

    // 服务端可以在 200 里要求登出(策略违规 / 号被踢)。这是**终态**,再刷也没用。
    if v.get("shouldLogout").and_then(|b| b.as_bool()) == Some(true) {
        let why = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("未给出原因");
        return Err(UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            format!("Cursor 要求该号登出({why}),凭据已终态失效"),
        ));
    }

    let token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::Other,
                "Cursor 刷新响应缺少 access_token".to_string(),
            )
        })?;

    Ok(Refreshed {
        access_token: token.to_string(),
        // 见模块头 ①:响应里没有 refresh_token 字段,新 access_token 兼任。
        refresh_token: token.to_string(),
    })
}

/// 从 JWT 载荷里读 `exp`(秒)。解不出返回 `None`。
///
/// 只做 base64url 解码 + 取字段,**不验签** —— 我们不是资源服务器,只想知道啥时候该换。
pub fn token_expires_at(jwt: &str) -> Option<i64> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("exp").and_then(|e| e.as_i64())
}

/// Unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`。
///
/// gw-app 的 `has_fresh_token` 认这个形态(见 worker 的 `parse_rfc3339_unix`)。
/// **不写它的后果**:`has_fresh_token` 对缺失 `expires_at` 的号「视为永鲜」→
/// 从不主动刷新 → 每个过期号都要先吃一次 401/403 才被动刷。而 403 的分类本身就是
/// 一个雷区(出口 IP 被拦也是 403),能不走到那一步就别走。
///
/// 算法是 Howard Hinnant 的 civil-from-days(纯算术,不引 chrono),与
/// gw-kiro 的 `token::format_unix_utc` 同源 —— 那个是 `pub(crate)`,而 gw-cursor
/// 有意不依赖 gw-kiro,故各带一份。将来若真做「统一格式模块」,这是该收进去的候选。
pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
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

/// 是否该刷新了。对齐客户端逻辑:剩余寿命不足 [`REFRESH_MARGIN_SECS`] 就换。
///
/// 解不出 `exp` 时返回 `true` —— 宁可多刷一次,也别让一个我们读不懂的 token 悄悄过期。
pub fn needs_refresh(access_token: &str, now_unix: i64) -> bool {
    match token_expires_at(access_token) {
        Some(exp) => exp - now_unix <= REFRESH_MARGIN_SECS,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn format_unix_utc_matches_known_epoch() {
        // 与 gw-kiro token.rs 的同名函数用同一个金标准,保证两边写出的形态一致。
        assert_eq!(format_unix_utc(1_780_531_200), "2026-06-04T00:00:00Z");
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
    }

    fn jwt_with_exp(exp: i64) -> String {
        let payload = serde_json::json!({"sub": "u", "exp": exp}).to_string();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("aGVhZGVy.{b64}.c2ln")
    }

    #[test]
    fn parses_access_token_and_reuses_it_as_refresh_token() {
        // 这条守的是模块头 ①:别去找不存在的 refresh_token 字段。
        let r = parse_refresh_response(r#"{"access_token":"NEWTOKEN"}"#).unwrap();
        assert_eq!(r.access_token, "NEWTOKEN");
        assert_eq!(r.refresh_token, "NEWTOKEN");
    }

    #[test]
    fn should_logout_is_terminal_token_invalid() {
        let e = parse_refresh_response(
            r#"{"shouldLogout":true,"error":"SIGN_IN_POLICY_VIOLATION","access_token":"x"}"#,
        )
        .unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
        assert!(e.message.contains("SIGN_IN_POLICY_VIOLATION"));
        // shouldLogout 优先于 access_token:即使给了 token 也不能用
    }

    #[test]
    fn should_logout_false_is_not_an_error() {
        let r = parse_refresh_response(r#"{"shouldLogout":false,"access_token":"T"}"#).unwrap();
        assert_eq!(r.access_token, "T");
    }

    #[test]
    fn missing_or_empty_access_token_errors() {
        assert!(parse_refresh_response("{}").is_err());
        assert!(parse_refresh_response(r#"{"access_token":""}"#).is_err());
        assert!(parse_refresh_response(r#"{"access_token":"   "}"#).is_err());
        assert!(parse_refresh_response("not json").is_err());
    }

    #[test]
    fn reads_exp_from_jwt_payload() {
        assert_eq!(token_expires_at(&jwt_with_exp(1_800_000_000)), Some(1_800_000_000));
        assert_eq!(token_expires_at("garbage"), None);
        assert_eq!(token_expires_at("a.b.c"), None);
        assert_eq!(token_expires_at(""), None);
    }

    #[test]
    fn needs_refresh_matches_client_margin() {
        let now = 1_700_000_000i64;
        // 剩余寿命刚好超过 1272 小时 → 不用换
        let far = jwt_with_exp(now + REFRESH_MARGIN_SECS + 60);
        assert!(!needs_refresh(&far, now));
        // 刚好等于余量 → 该换了(客户端是 > 才跳过)
        let edge = jwt_with_exp(now + REFRESH_MARGIN_SECS);
        assert!(needs_refresh(&edge, now));
        // 已过期
        assert!(needs_refresh(&jwt_with_exp(now - 1), now));
        // 读不懂的 token 一律当作要换
        assert!(needs_refresh("opaque-token", now));
    }

    #[test]
    fn margin_is_1272_hours() {
        // bundle 常量 uqi = 1272*60*60*1e3(毫秒)。写死在这里防止手滑改错量级。
        assert_eq!(REFRESH_MARGIN_SECS, 4_579_200);
    }
}
