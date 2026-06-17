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
        401 => UpstreamErrorKind::TokenInvalid,
        // 403 + 封禁标记 → TemporarilyBlocked(冷却自愈,不永久禁号):账号被临时封禁时
        // 刷新端点也会 403,若归 TokenInvalid 会把"临时封禁"升级成"永久禁用",封解了也救不回。
        403 if crate::error_map::is_account_suspended(body) => UpstreamErrorKind::TemporarilyBlocked,
        403 => UpstreamErrorKind::TokenInvalid,
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

/// RFC3339 时刻字符串 → Unix 秒(纯算术,无 chrono;`format_unix_utc` 的逆)。
/// 支持 `YYYY-MM-DDTHH:MM:SS[.fff]` 后接 `Z` 或 `±HH:MM` / `±HHMM` / `±HH`。
/// 用于把 kirogo 的 `timestamp`(令牌签发时刻)转成基准,再加 `expiresIn` 得绝对过期。
///
/// **严格 + 安全**(对抗审查加固):脏/恶意输入一律返回 None(调用方退回"无 expires_at,
/// 按需刷新"),绝不 panic、绝不把错误时刻静默写进 extra。具体:① 非 ASCII 直接拒(RFC3339
/// 纯 ASCII;同时杜绝字节切片越界 panic);② 年限 1970..=9999(防 days*86400 溢出);
/// ③ 按月/闰年校验日;④ 时/分/秒、时区时/分范围校验;⑤ 多余冒号段 / 非数字小数秒拒绝。
pub(crate) fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    if !s.is_ascii() {
        return None;
    }
    let (datetime, offset_secs) = split_tz_offset(s)?;
    let t = datetime.find(['T', 't'])?;
    let (date, time) = (&datetime[..t], &datetime[t + 1..]);
    // 日期 YYYY-MM-DD(恰好三段)。
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1970..=9999).contains(&y) || !(1..=12).contains(&mo) {
        return None;
    }
    if d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    // 时间 HH:MM:SS[.fff](恰好三段冒号;小数秒可选、必须全数字、忽略其值)。
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec_part = tp.next()?;
    if tp.next().is_some() {
        return None;
    }
    let (sec_str, frac) = match sec_part.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (sec_part, None),
    };
    if let Some(f) = frac {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let se: i64 = sec_str.parse().ok()?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&se) {
        return None;
    }
    // y ≤ 9999 → days_from_civil*86400 ≤ ~2.6e11,不溢出;offset 已限幅。
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_secs)
}

/// 拆时区偏移:返回(去掉偏移的 datetime 部分, 偏移秒数)。`Z`/`z` = 0。
/// 调用方已保证 `s` 全 ASCII,故 `off[..2]` 等字节切片在字符边界上安全。
fn split_tz_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((stripped, 0));
    }
    let t = s.find(['T', 't'])?;
    // 偏移符号必在 'T' 之后的时间部分(否则会误匹配日期里的 '-')。
    let rel = s[t + 1..].rfind(['+', '-'])?;
    let sign_idx = t + 1 + rel;
    let sign: i64 = if s.as_bytes()[sign_idx] == b'+' { 1 } else { -1 };
    let off = &s[sign_idx + 1..];
    let (oh, om) = match off.split_once(':') {
        Some((a, b)) => (a.parse::<i64>().ok()?, b.parse::<i64>().ok()?),
        None if off.len() == 4 => (off[..2].parse().ok()?, off[2..].parse().ok()?),
        None if off.len() == 2 => (off.parse::<i64>().ok()?, 0),
        None => return None,
    };
    if !(0..=23).contains(&oh) || !(0..=59).contains(&om) {
        return None;
    }
    Some((&s[..sign_idx], sign * (oh * 3600 + om * 60)))
}

/// 某年某月的天数(含闰年)。非法月返回 0(令日校验失败)。
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// 1970-01-01 起的天数(Howard-Hinnant days_from_civil;`format_unix_utc` 内联算法的逆)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

    #[test]
    fn parse_rfc3339_to_unix_roundtrip_and_offsets() {
        // epoch + 往返
        assert_eq!(parse_rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_to_unix("2026-06-04T00:00:00Z"), Some(1_780_531_200));
        // round-trip 任意时刻
        let t = 1_780_531_200 + 66_046;
        assert_eq!(parse_rfc3339_to_unix(&format_unix_utc(t)), Some(t));
        // 时区偏移:+08:00 比 UTC 早 8h(同墙钟 → unix 小 28800)
        let z = parse_rfc3339_to_unix("2026-06-07T10:20:46Z").unwrap();
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46+08:00"), Some(z));
        // 负偏移 + 紧凑写法 + 小数秒
        assert_eq!(
            parse_rfc3339_to_unix("2026-06-07T05:20:46-05:00"),
            parse_rfc3339_to_unix("2026-06-07T10:20:46Z")
        );
        assert_eq!(
            parse_rfc3339_to_unix("2026-06-07T18:20:46+0800"),
            parse_rfc3339_to_unix("2026-06-07T18:20:46+08:00")
        );
        assert_eq!(
            parse_rfc3339_to_unix("2026-06-07T18:20:46.958+08:00"),
            parse_rfc3339_to_unix("2026-06-07T18:20:46+08:00")
        );
        // 非法
        assert_eq!(parse_rfc3339_to_unix("not a date"), None);
        assert_eq!(parse_rfc3339_to_unix("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn parse_rfc3339_to_unix_rejects_garbage() {
        // 非法日(按月/闰年校验)
        assert_eq!(parse_rfc3339_to_unix("2026-02-31T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_to_unix("2026-02-29T00:00:00Z"), None, "2026 非闰年");
        assert!(parse_rfc3339_to_unix("2024-02-29T00:00:00Z").is_some(), "2024 闰年合法");
        assert_eq!(parse_rfc3339_to_unix("2026-04-31T00:00:00Z"), None, "4 月只有 30 天");
        // 时区越界
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46+24:00"), None);
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46+08:99"), None);
        // 多余冒号段 / 非数字小数秒
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46:999Z"), None);
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46.fooZ"), None);
        // 时分秒越界
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T24:00:00Z"), None);
        // 非 ASCII offset:返回 None,**绝不 panic**(对抗审查 #1 字节切片越界)。
        assert_eq!(parse_rfc3339_to_unix("2026-06-07T18:20:46+€a"), None);
        // 年限外(防 days*86400 溢出)
        assert_eq!(parse_rfc3339_to_unix("0001-01-01T00:00:00Z"), None);
    }
}
