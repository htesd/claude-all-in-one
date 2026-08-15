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

/// external_idp(Azure AD 租户)token endpoint 的宿主白名单——对齐 Kiro-Go 参考实现
/// `auth/kiro_sso.go` 的 `allowedExternalIdpIssuerSuffixes`。这是刷新流程唯一的
/// SSRF / refresh_token 外泄防线:`token_endpoint` 来自导入的凭据 JSON(可能是号商/
/// 第三方工具导出的半可信文件),一份被篡改的 tokenEndpoint 指向攻击者主机,会让
/// 服务器把账号的 refresh_token 当作 POST body 发过去。
const ALLOWED_EXTERNAL_IDP_HOST_SUFFIXES: &[&str] =
    &[".microsoftonline.com", ".microsoftonline.us", ".microsoftonline.cn"];

/// 校验一个 external_idp token endpoint 是否落在白名单主机上:https-only、非 IP
/// 字面量、host 落在 [`ALLOWED_EXTERNAL_IDP_HOST_SUFFIXES`] 任一后缀内。
///
/// 两处调用,纵深防御:①`import.rs` 导入/派生 token_endpoint 时——校验不过就不写入
/// extra(账号仍正常导入,只是缺 token_endpoint,留给运维在 admin 手动核实补全,不让
/// 整条导入因一个坏字段失败);②本文件 `refresh_external_idp` 发出请求前——即便账号
/// 是运维绕过导入、直接在 admin 面板 PATCH 写入的,发请求前仍会再挡一次(对齐 Kiro-Go
/// `postExternalIdpToken` 的同一份"outbound-POST 边界再校验一次"的防御纵深)。
pub(crate) fn validate_external_idp_endpoint(raw_url: &str) -> Result<(), &'static str> {
    let parsed = url::Url::parse(raw_url.trim()).map_err(|_| "invalid URL")?;
    if parsed.scheme() != "https" {
        return Err("must be https");
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            let host_lower = host.to_ascii_lowercase();
            if ALLOWED_EXTERNAL_IDP_HOST_SUFFIXES.iter().any(|suf| host_lower.ends_with(suf)) {
                Ok(())
            } else {
                Err("host is not allow-listed")
            }
        }
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => Err("host must not be an IP literal"),
        None => Err("URL has no host"),
    }
}

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

/// External IdP(Azure AD 租户)OAuth2 token 端点响应。原生 snake_case —— 这是 OAuth2
/// 规范本身的字段命名,不是 Kiro/我方约定,故不用 `rename_all = "camelCase"`(与
/// Social/IdC 两个 Kiro 自家端点的 camelCase 响应刻意不同,不能套同一 derive 属性)。
#[derive(Debug, Deserialize, Default)]
struct ExternalIdpTokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
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
/// 分流优先级:external_idp(按 auth_method 显式标记)> IdC(client_id+secret)> social。
/// external_idp 必须最先判——Azure AD 应用注册也可能带 client_secret(机密客户端场景),
/// 若不优先判会被 `is_idc` 抢先命中、误路由到 AWS SSO OIDC 端点,刷新必然失败(该账号
/// 的 refresh_token 是 Azure 颁发的,AWS 侧根本不认)。
pub async fn refresh_auth(
    client: &reqwest::Client,
    account: &Account,
) -> Result<RefreshedAuth, UpstreamError> {
    let refresh_token = account
        .extra_str("refresh_token")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::TokenInvalid, "账号缺少 refresh_token"))?;

    if crate::headers::is_external_idp(account) {
        refresh_external_idp(client, account, refresh_token).await
    } else if is_idc(account) {
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

/// External IdP(Azure AD 租户)刷新:OAuth2 refresh_token grant,public client(请求里
/// 不带 client_secret),`application/x-www-form-urlencoded` POST 到租户自己的 token
/// endpoint(`account.extra["token_endpoint"]`,导入时由 `import.rs` 派生或直接读入)——
/// 与 Kiro 自家 JSON body 的 social/IdC 刷新是完全不同的协议,不能复用两者的请求构造。
/// 对齐 Kiro-Go 参考实现 `auth/oidc.go` 的 `refreshExternalIdpToken`/`postExternalIdpToken`
/// (字段名、grant_type、scope 可选性均逐一核对过)。
async fn refresh_external_idp(
    client: &reqwest::Client,
    account: &Account,
    refresh_token: &str,
) -> Result<RefreshedAuth, UpstreamError> {
    let client_id = account.extra_str("client_id").unwrap_or_default();
    let token_endpoint = account.extra_str("token_endpoint").unwrap_or_default();
    if client_id.is_empty() || token_endpoint.is_empty() {
        return Err(UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            "external_idp 刷新缺少 client_id 或 token_endpoint(账号需在 admin 补全)",
        ));
    }
    // 纵深防御:import.rs 已经校验过白名单,但账号也可能是运维绕过导入、直接在
    // admin 面板 PATCH 写入的 token_endpoint——发出携带 refresh_token 的请求前
    // 再挡一次,绝不把账号密钥 POST 到白名单外的主机。
    if let Err(reason) = validate_external_idp_endpoint(token_endpoint) {
        return Err(UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            format!("external_idp token_endpoint 被拒绝({reason}):{token_endpoint}"),
        ));
    }
    let scope = account.extra_str("scope").unwrap_or_default();

    let mut form: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    if !scope.is_empty() {
        form.push(("scope", scope));
    }

    let resp = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("external_idp 刷新请求失败: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| UpstreamError::network(format!("external_idp 刷新响应读取失败: {e}")))?;

    // 对齐 Kiro-Go:JSON 解析失败不立即报错,退回全空结构体,交给下面的状态码/空
    // access_token 统一判定(畸形响应体本身也是一种失败信号,不需要单独处理)。
    let data: ExternalIdpTokenResponse = serde_json::from_str(&body).unwrap_or_default();

    if !status.is_success() || data.access_token.is_empty() {
        return Err(classify_refresh_error(status.as_u16(), &body, "external_idp"));
    }

    Ok(RefreshedAuth {
        access_token: data.access_token,
        // Azure AD 部分租户滚动 refresh_token,部分原样不返回;缺失时保留旧值,不能让
        // 账号在下一轮刷新时丢 refresh_token(对齐 Kiro-Go 同一注释的处理)。
        refresh_token: Some(data.refresh_token.unwrap_or_else(|| refresh_token.to_string())),
        // IdP 不下发 profileArn(靠 ListAvailableProfiles 另行解析,caio 暂未实现该探测);
        // None 让上层 `Provider::refresh_auth` 保留账号已有的 profile_arn,不覆盖成空
        // (lib.rs 的写回逻辑是 `if let Some(arn) = refreshed.profile_arn { ... }`)。
        profile_arn: None,
        // 上限护栏(30 天):token_endpoint 来自半可信的导入文件,一个恶意/畸形的
        // expires_in(如 i64::MAX)会让 expires_at_rfc3339 内部 `now + expires_in`
        // 溢出(该函数按普通 `+` 计算,不是 saturating),且即便侥幸不溢出也会让
        // has_fresh_token 误判成"永久新鲜"、账号再也不会被刷新——clamp 到一个远超
        // 真实 OAuth2 令牌寿命的合理上限,纵深防御。
        expires_at: data.expires_in.map(|secs| expires_at_rfc3339(secs.clamp(0, 30 * 24 * 3600))),
    })
}

/// 刷新错误分类(对齐旧代码:400+invalid_grant+Invalid refresh token = 永久失效)。
fn classify_refresh_error(status: u16, body: &str, flow: &str) -> UpstreamError {
    // AWS/Kiro 侧的 invalid_grant 错误文案固定为下面这句;但 Azure AD 的
    // error_description 是自由文本(如 "AADSTS70008: ... expired due to inactivity ..."),
    // 不含这句——external_idp 流单独放宽成只认 OAuth2 错误码本身,否则 Azure 侧已失效的
    // refresh_token 会被误判成 `Other` 而非 `TokenInvalid`,账号不会被正确标记失效、
    // 陷入反复刷新失败的死循环。其余流(social/IdC)维持原有精确匹配,不放宽。
    let is_invalid_grant = body.contains("\"invalid_grant\"")
        && (flow == "external_idp" || body.contains("Invalid refresh token provided"));
    if status == 400 && is_invalid_grant {
        // Azure 的 invalid_grant 不只代表"refresh_token 真被撤销/过期"——租户开启 MFA/
        // 条件访问策略变更/应用被要求重新同意时也会返回同一个 error 码,只是
        // error_description 不同(AADSTS 码)。这些场景都需要人工用 Kiro-Go 等工具
        // 交互式重新登录才能恢复(无人值守的 refresh_token-only 流程本来就救不回来,
        // 标记 TokenInvalid + 禁用账号仍是唯一安全动作),但错误消息里带上原始 body,
        // 让运维能分清"真死了"还是"只是需要重新登录",而不是被"已失效"这句话误导
        // 成以为号商给的号本身就是坏的。
        let message = if flow == "external_idp" {
            format!("{flow} refreshToken 刷新被拒 (invalid_grant),需人工交互式重新登录后更新账号: {body}")
        } else {
            format!("{flow} refreshToken 已失效 (invalid_grant)")
        };
        return UpstreamError::new(UpstreamErrorKind::TokenInvalid, message).with_status(status);
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
            created_at: 0,
            extra: map,
        }
    }

    #[test]
    fn validate_external_idp_endpoint_accepts_allowlisted_hosts() {
        assert!(validate_external_idp_endpoint(
            "https://login.microsoftonline.com/tenant/oauth2/v2.0/token"
        )
        .is_ok());
        assert!(validate_external_idp_endpoint("https://login.microsoftonline.us/t/oauth2/v2.0/token").is_ok());
        assert!(validate_external_idp_endpoint("https://login.microsoftonline.cn/t/oauth2/v2.0/token").is_ok());
    }

    #[test]
    fn validate_external_idp_endpoint_rejects_non_https() {
        let e = validate_external_idp_endpoint("http://login.microsoftonline.com/t/oauth2/v2.0/token");
        assert!(e.is_err());
    }

    #[test]
    fn validate_external_idp_endpoint_rejects_non_allowlisted_host() {
        // 攻击场景本尊:一份被篡改的 tokenEndpoint 指向攻击者主机。
        assert!(validate_external_idp_endpoint("https://attacker.example/collect").is_err());
    }

    #[test]
    fn validate_external_idp_endpoint_rejects_ip_literals() {
        assert!(validate_external_idp_endpoint("https://169.254.169.254/token").is_err(), "云环境元数据地址必须拒绝");
        assert!(validate_external_idp_endpoint("https://127.0.0.1/token").is_err());
        assert!(validate_external_idp_endpoint("https://[::1]/token").is_err());
    }

    #[test]
    fn validate_external_idp_endpoint_suffix_is_anchored_not_substring() {
        // 后缀匹配必须锚定在真实子域边界(前导 '.'),否则 "evil-microsoftonline.com" 这种
        // 攻击者自行注册的域名会被误判成合法子域——对齐 Kiro-Go 同一注释里点名的攻击场景。
        assert!(validate_external_idp_endpoint("https://evil-microsoftonline.com/token").is_err());
        assert!(validate_external_idp_endpoint("https://microsoftonline.com.attacker.example/token").is_err());
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
    fn classify_azure_invalid_grant_without_aws_wording_is_token_invalid() {
        // Azure AD 真实 400 响应文案(AADSTS 错误码,不含 AWS 那句 "Invalid refresh token
        // provided"),external_idp 流必须仍判成 TokenInvalid,否则失效号会被反复重试。
        let body = r#"{"error":"invalid_grant","error_description":"AADSTS70008: The provided authorization code or refresh token has expired due to inactivity."}"#;
        let e = classify_refresh_error(400, body, "external_idp");
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn classify_azure_style_invalid_grant_not_relaxed_for_other_flows() {
        // 放宽只作用于 external_idp;social/IdC 维持原有精确匹配,不因为这次改动扩大误判面。
        let body = r#"{"error":"invalid_grant","error_description":"AADSTS70008: expired"}"#;
        let e = classify_refresh_error(400, body, "social");
        assert_ne!(e.kind, UpstreamErrorKind::TokenInvalid);
    }

    #[tokio::test]
    async fn external_idp_takes_priority_over_idc_when_auth_method_set() {
        // 账号同时带 client_id+client_secret(单看这两个字段会被 is_idc 判定为真)和
        // auth_method=external_idp,但缺 token_endpoint。若分流正确优先走 external_idp,
        // 会在发出任何网络请求前就因缺 token_endpoint 报错;若被误路由到 refresh_idc,
        // 则会尝试对 oidc.us-east-1.amazonaws.com 发起真实网络请求,不会是这条特定错误。
        let client = reqwest::Client::new();
        let a = acct(&[
            ("auth_method", "external_idp"),
            ("client_id", "c"),
            ("client_secret", "s"),
            ("refresh_token", "rt"),
        ]);
        let err = refresh_auth(&client, &a).await.unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::TokenInvalid);
        assert!(
            err.message.contains("token_endpoint"),
            "应报缺 token_endpoint,证明分流走了 external_idp 分支而非 IdC: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn external_idp_missing_client_id_short_circuits_too() {
        let client = reqwest::Client::new();
        let a = acct(&[
            ("auth_method", "external_idp"),
            ("refresh_token", "rt"),
            ("token_endpoint", "https://login.microsoftonline.com/tenant/oauth2/v2.0/token"),
        ]);
        let err = refresh_auth(&client, &a).await.unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::TokenInvalid);
        assert!(err.message.contains("client_id"));
    }

    #[tokio::test]
    async fn external_idp_refresh_rejects_non_allowlisted_token_endpoint_before_any_request() {
        // 纵深防御的第二道:即便账号是运维绕过导入直接 PATCH 写入的恶意 token_endpoint,
        // refresh_external_idp 也必须在发出任何网络请求(带着 refresh_token 的那个)前拒绝。
        let client = reqwest::Client::new();
        let a = acct(&[
            ("auth_method", "external_idp"),
            ("client_id", "c"),
            ("refresh_token", "rt"),
            ("token_endpoint", "https://attacker.example/collect"),
        ]);
        let err = refresh_auth(&client, &a).await.unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::TokenInvalid);
        assert!(err.message.contains("被拒绝"), "{}", err.message);
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
