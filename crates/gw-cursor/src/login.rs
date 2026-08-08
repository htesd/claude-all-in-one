//! Cursor 官方登录流(PKCE + 轮询)—— 上号不用手抄 `state.vscdb`。
//!
//! 逆向自本机 Cursor 3.14.27 的 `loginLink` + `getPollingEndpoint`,与真客户端**同款**:
//!
//! ```text
//! 1. verifier  = base64url_nopad(random 32B)
//!    challenge = base64url_nopad(sha256(verifier))     ← PKCE S256
//!    uuid      = uuid v4                              ← 本次登录流的 id
//!
//! 2. 操作员在浏览器打开:
//!    https://cursor.com/loginDeepControl
//!      ?challenge=<challenge>&uuid=<uuid>&mode=login&supportsSelectedTeamLogin=true
//!
//! 3. 轮询 GET https://api2.cursor.sh/auth/poll?uuid=<uuid>&verifier=<verifier>
//!      404 → 还没授权,继续等
//!      403 + error=SIGN_IN_POLICY_VIOLATION → 被企业策略拒,终态
//!      200 → { accessToken, refreshToken, authId?, selectedTeamId? }
//! ```
//!
//! ## 与 [`crate::auth`] 刷新流的一个关键区别
//!
//! 刷新响应里**没有** `refresh_token` 字段(新 access_token 兼任);而登录轮询响应里
//! `accessToken` 与 `refreshToken` 是**两个独立字段**(bundle: `se.accessToken && se.refreshToken`)。
//! 别把两处的处理逻辑合并。
//!
//! ## 为什么不照抄真客户端的轮询节奏
//!
//! 真客户端是 `setInterval(…, 500)` + `te >= 30` 放弃,即 **500ms × 30 = 15 秒**。
//! 那是给「客户端已登录、只是换 team」的秒级场景用的。我们的场景是**运维手工上号**:
//! 要开浏览器、登邮箱/GitHub、点授权,15 秒根本不够。所以这里放宽到
//! [`POLL_INTERVAL`] × [`POLL_MAX_ATTEMPTS`],并且**由调用方分多次调用**
//! (见 [`poll_once`]),而不是在一个 HTTP 请求里阻塞几分钟 —— 后者会被
//! admin 面板的前端超时掐断,操作员看到的是「失败」但其实登录成功了。

use gw_core::error::{UpstreamError, UpstreamErrorKind};

/// 登录页(操作员在浏览器打开的那个)。
pub const LOGIN_PAGE_URL: &str = "https://cursor.com/loginDeepControl";

/// 轮询端点。与刷新同域(`api2.cursor.sh`),**不是**推理用的 `agentn.api5`。
pub const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";

/// 轮询间隔建议值(给调用方参考;本模块不自己睡)。
pub const POLL_INTERVAL_SECS: u64 = 2;

/// 建议的最大轮询次数:2s × 150 = 5 分钟,够操作员登一次浏览器。
pub const POLL_MAX_ATTEMPTS: u32 = 150;

/// 单次轮询的 HTTP 超时。真客户端没设(靠 interval 兜),我们设一个免得挂死。
const TIMEOUT_SECS: u64 = 15;

/// 企业策略拒登的错误码(bundle 里的 `awh` 常量)。
const ERR_SIGN_IN_POLICY: &str = "SIGN_IN_POLICY_VIOLATION";

/// 一次登录流的本地状态。**`verifier` 是秘密**,只在轮询时发给上游,别落日志。
#[derive(Debug, Clone)]
pub struct LoginFlow {
    /// 本次流的 id,同时传给登录页与轮询端点。
    pub uuid: String,
    /// PKCE verifier —— 证明「来轮询的人就是发起登录的人」。
    pub verifier: String,
    /// 操作员要打开的完整 URL。
    pub login_url: String,
}

/// 轮询的三种结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// 用户还没在浏览器里点完授权(上游 404)。继续等。
    Pending,
    /// 拿到凭据了。
    Done {
        access_token: String,
        refresh_token: String,
        /// 上游返回的账号标识,可用来给 account_id 起个有意义的默认值。
        auth_id: Option<String>,
    },
}

/// 发起一次登录流:生成 PKCE 对与 URL。**不发任何网络请求。**
pub fn start() -> LoginFlow {
    let verifier = b64url(&random_32());
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let uuid = uuid::Uuid::new_v4().to_string();
    // `mode=login` 与 `supportsSelectedTeamLogin=true` 逐字对齐真客户端。
    // 真客户端在 Glass 构建下还会追 `&surface=glass`;我们不是 Glass 界面,不发。
    let login_url = format!(
        "{LOGIN_PAGE_URL}?challenge={challenge}&uuid={uuid}&mode=login&supportsSelectedTeamLogin=true"
    );
    LoginFlow {
        uuid,
        verifier,
        login_url,
    }
}

/// 轮询一次。**不睡不循环** —— 由调用方按 [`POLL_INTERVAL_SECS`] 节奏重试。
///
/// 这样切分是因为 admin 面板那边一个 HTTP 请求不能挂几分钟(前端会超时,
/// 而操作员其实已经授权成功了,凭据就白扔了)。
pub async fn poll_once(
    client: &reqwest::Client,
    flow: &LoginFlow,
) -> Result<PollOutcome, UpstreamError> {
    let url = format!(
        "{POLL_URL}?uuid={}&verifier={}",
        urlencode(&flow.uuid),
        urlencode(&flow.verifier)
    );
    let resp = client
        .get(&url)
        .header("x-cursor-client-type", crate::wire::CLIENT_TYPE)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("Cursor 登录轮询失败: {e}")))?;

    let status = resp.status().as_u16();
    // 404 = 还没授权。这是**正常等待态**,不是错误 —— 真客户端也是这么判的。
    if status == 404 {
        return Ok(PollOutcome::Pending);
    }
    let text = resp.text().await.unwrap_or_default();

    if status == 403 {
        let denied = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
        let msg = match denied.as_deref() {
            Some(ERR_SIGN_IN_POLICY) => {
                "该账号被企业登录策略(MDM sign-in policy)拒绝,换个号".to_string()
            }
            Some(other) => format!("Cursor 拒绝登录: {other}"),
            None => "Cursor 拒绝登录(403)".to_string(),
        };
        return Err(UpstreamError::new(UpstreamErrorKind::TokenInvalid, msg).with_status(403));
    }
    if !(200..300).contains(&status) {
        return Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!(
                "Cursor 登录轮询 {status}: {}",
                text.chars().take(200).collect::<String>()
            ),
        )
        .with_status(status));
    }

    parse_poll_body(&text)
}

/// 解析 200 响应体。拆出来是为了能在无网络的单测里覆盖各种残缺形态。
pub fn parse_poll_body(body: &str) -> Result<PollOutcome, UpstreamError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("Cursor 登录轮询响应不是 JSON: {e}"),
        )
    })?;
    let pick = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // 真客户端的判据是 `se.accessToken && se.refreshToken` —— **两个都有**才算成功。
    // 只有其一时它走 "Response missing tokens" 分支继续等,我们照办。
    match (pick("accessToken"), pick("refreshToken")) {
        (Some(access_token), Some(refresh_token)) => Ok(PollOutcome::Done {
            access_token,
            refresh_token,
            auth_id: pick("authId"),
        }),
        _ => Ok(PollOutcome::Pending),
    }
}

fn random_32() -> [u8; 32] {
    // 与 wire::random_hex 同源:uuid 的 fast-rng 底下就是 getrandom。
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 只转义会破坏 query 的字符。uuid 与 base64url 本来就是安全字符集,
/// 但 verifier 万一将来换编码,这层能兜住。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_produces_pkce_pair_and_url() {
        let f = start();
        // verifier 32 字节 → base64url 无 padding 是 43 字符
        assert_eq!(f.verifier.len(), 43);
        assert!(!f.verifier.contains('='), "base64url 不带 padding");
        assert!(!f.verifier.contains('+') && !f.verifier.contains('/'));
        assert_eq!(f.uuid.len(), 36);

        // URL 逐字对齐真客户端的参数集
        assert!(f.login_url.starts_with(LOGIN_PAGE_URL));
        assert!(f.login_url.contains("mode=login"));
        assert!(f.login_url.contains("supportsSelectedTeamLogin=true"));
        assert!(f.login_url.contains(&format!("uuid={}", f.uuid)));
        // ⚠️ verifier 是秘密,**绝不能出现在给操作员的 URL 里**(只有 challenge 能)
        assert!(
            !f.login_url.contains(&f.verifier),
            "verifier 泄漏进登录页 URL = PKCE 失去意义"
        );
        assert!(f.login_url.contains("challenge="));
    }

    #[test]
    fn challenge_is_sha256_of_verifier() {
        let f = start();
        let expect = b64url(&sha256(f.verifier.as_bytes()));
        assert!(
            f.login_url.contains(&format!("challenge={expect}")),
            "challenge 必须是 verifier 的 S256"
        );
    }

    #[test]
    fn each_flow_is_unique() {
        let (a, b) = (start(), start());
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.uuid, b.uuid);
    }

    #[test]
    fn poll_body_with_both_tokens_is_done() {
        let r = parse_poll_body(
            r#"{"accessToken":"AT","refreshToken":"RT","authId":"auth|123","selectedTeamId":7}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            PollOutcome::Done {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                auth_id: Some("auth|123".into()),
            }
        );
    }

    #[test]
    fn poll_body_missing_either_token_keeps_waiting() {
        // 真客户端的判据是两个都有;缺一个走 "Response missing tokens" 继续等。
        for body in [
            r#"{"accessToken":"AT"}"#,
            r#"{"refreshToken":"RT"}"#,
            r#"{}"#,
            r#"{"accessToken":"","refreshToken":"RT"}"#,
            r#"{"accessToken":"  ","refreshToken":"  "}"#,
        ] {
            assert_eq!(
                parse_poll_body(body).unwrap(),
                PollOutcome::Pending,
                "body={body} 应视为等待而不是成功"
            );
        }
    }

    #[test]
    fn poll_body_non_json_errors() {
        assert!(parse_poll_body("<html>502</html>").is_err());
    }

    #[test]
    fn auth_id_is_optional() {
        let r = parse_poll_body(r#"{"accessToken":"A","refreshToken":"R"}"#).unwrap();
        match r {
            PollOutcome::Done { auth_id, .. } => assert!(auth_id.is_none()),
            other => panic!("应为 Done,实际 {other:?}"),
        }
    }

    #[test]
    fn urlencode_leaves_safe_chars_and_escapes_others() {
        assert_eq!(urlencode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        // 一个真实形态的 verifier 过一遍应当原样
        let f = start();
        assert_eq!(urlencode(&f.verifier), f.verifier);
        assert_eq!(urlencode(&f.uuid), f.uuid);
    }

    #[test]
    fn poll_interval_is_gentler_than_real_client() {
        // 真客户端 500ms×30=15s,对人工上号太短。我们的窗口必须显著更长。
        let window = POLL_INTERVAL_SECS * POLL_MAX_ATTEMPTS as u64;
        assert!(window >= 240, "轮询窗口 {window}s 太短,操作员登不完浏览器");
        assert!(POLL_INTERVAL_SECS >= 1, "别比真客户端更激进地打上游");
    }
}
