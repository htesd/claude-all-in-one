//! gw-dario OAuth 上号:PKCE authorize + `authorization_code` 换码。
//!
//! 与 [`crate::token::refresh`] 同一 token 端点、同一 `client_id`(CC 公开客户端,纯 PKCE 无 secret)。
//! **换码出口**由调用方 [`crate::DarioProvider::oauth_exchange`] 经 `egress_client_for` 选——和该号
//! 将来 refresh/chat 走**同一** egress → 铸 token IP == 刷新 IP == 发包 IP(铸≠发=关联封号)。
//!
//! 铸币的 consent(浏览器登录同意)那一跳由操作员人肉完成(caio 无密码、不 headless 登录),
//! 其 IP **不**纳入保证(authorize code 数秒失效,Anthropic 关联的是持久的铸/刷/发三步)。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::format_rfc3339_z;
use crate::token::{classify_refresh, now_unix, CLIENT_ID, TOKEN_URL};

/// CC 公开客户端的 authorize 端点(claude.ai)。
pub(crate) const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// Anthropic 特判的 manual redirect:登录后页面直接显示 code 供复制(免 caio 自建回调)。
pub(crate) const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// CC 申请的 scope(与真实 Claude Code 一致,过 overage 分类)。
pub(crate) const SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 32 字节 CSPRNG 随机 → base64url-no-pad(43 字符)。
/// 用两个 v4 UUID(内部走 getrandom CSPRNG)拼足 32 字节,免新增 `rand` 依赖。
fn random_b64url_32() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE:返回 `(code_verifier, code_challenge)`,challenge = base64url(SHA256(verifier))。
pub fn gen_pkce() -> (String, String) {
    let verifier = random_b64url_32();
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// CSRF/绑定用 state(够长——authorize 端点会拒过短 state 报 "Invalid request format")。
pub fn gen_state() -> String {
    random_b64url_32()
}

/// 构造 authorize URL(manual 模式:登录后显示 code 供复制)。纯函数,`/start` 在 router 直接调,不发网络。
pub fn build_authorize_url(challenge: &str, state: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL const 必合法");
    url.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

/// manual 回调页给的串常是 `code#state`;拆出 `(code, Option<state>)`(均 trim)。
/// 无 `#` 则视为纯 code、无 state。
pub fn parse_manual_code(pasted: &str) -> (String, Option<String>) {
    let s = pasted.trim();
    match s.split_once('#') {
        Some((code, state)) => (
            code.trim().to_string(),
            Some(state.trim().to_string()).filter(|x| !x.is_empty()),
        ),
        None => (s.to_string(), None),
    }
}

/// `authorization_code` → token set。返回要并入账号 `extra` 的字段
/// `{access_token, refresh_token, expires_at}`。
///
/// **出口约束**:`client` 必须由 `egress_client_for` 选出(=该号将来 refresh/chat 的同一出口)。
pub(crate) async fn exchange(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<serde_json::Map<String, Value>, UpstreamError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("dario oauth 换码连接失败: {e}")))?;
    let status = resp.status();
    let json: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let err = json.get("error").and_then(|v| v.as_str()).unwrap_or("");
        return Err(UpstreamError::new(
            classify_refresh(status.as_u16(), &json),
            format!("dario oauth 换码 {} {err}", status.as_u16()),
        )
        .with_status(status.as_u16()));
    }
    parse_token_set(&json, now_unix())
}

/// 纯函数:从 token 响应抽 `{access_token, refresh_token, expires_at}`(注入 `now_unix` 便于测试)。
/// onboarding 与 refresh 不同——access_token 与 refresh_token **都必须有**(拿不到 refresh_token
/// 等于白上号,token 过期即死)。`expires_in` 缺省则不写 `expires_at`。
pub(crate) fn parse_token_set(
    resp: &Value,
    now_unix: i64,
) -> Result<serde_json::Map<String, Value>, UpstreamError> {
    let access = resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let e = resp.get("error").and_then(|v| v.as_str()).unwrap_or("no access_token");
            UpstreamError::new(UpstreamErrorKind::TokenInvalid, format!("dario oauth 换码缺 access_token: {e}"))
        })?;
    let refresh = resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "dario oauth 换码缺 refresh_token(无法长期持有)",
            )
        })?;
    let mut m = serde_json::Map::new();
    m.insert("access_token".into(), Value::String(access.to_string()));
    m.insert("refresh_token".into(), Value::String(refresh.to_string()));
    if let Some(ttl) = resp.get("expires_in").and_then(|v| v.as_i64()) {
        m.insert("expires_at".into(), Value::String(format_rfc3339_z(now_unix + ttl)));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_len_and_challenge_is_s256() {
        let (v, c) = gen_pkce();
        // 32 字节 base64url-no-pad = 43 字符(PKCE 要求 43..=128)。
        assert_eq!(v.len(), 43, "verifier 长度");
        assert!((43..=128).contains(&v.len()));
        assert_ne!(v, c);
        // challenge 必须 = base64url(SHA256(verifier))。
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes()));
        assert_eq!(c, expect);
        // base64url-no-pad:无 '+' '/' '='。
        assert!(!v.contains('+') && !v.contains('/') && !v.contains('='));
        assert!(!c.contains('+') && !c.contains('/') && !c.contains('='));
    }

    #[test]
    fn pkce_each_call_is_random() {
        assert_ne!(gen_pkce().0, gen_pkce().0);
        assert_ne!(gen_state(), gen_state());
        assert!(gen_state().len() >= 32);
    }

    #[test]
    fn authorize_url_has_all_params() {
        let url = build_authorize_url("CHAL", "STATE");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        // redirect_uri 与 scope 经 URL 编码后仍可解出原值。
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(REDIRECT_URI));
        assert_eq!(q.get("scope").map(String::as_str), Some(SCOPES));
    }

    #[test]
    fn parse_manual_code_splits_code_hash_state() {
        assert_eq!(parse_manual_code("abc#xyz"), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_manual_code("  abc#xyz  "), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_manual_code("justcode"), ("justcode".into(), None));
        // 空 state 段折叠为 None(避免误判 state 匹配)。
        assert_eq!(parse_manual_code("abc#"), ("abc".into(), None));
    }

    #[test]
    fn token_set_full() {
        let r = serde_json::json!({"access_token":"at","refresh_token":"rt","expires_in":3600});
        let m = parse_token_set(&r, 1_780_531_200).unwrap();
        assert_eq!(m.get("access_token").unwrap(), "at");
        assert_eq!(m.get("refresh_token").unwrap(), "rt");
        // 1_780_531_200 + 3600 = 1_780_534_800 = 2026-06-04T01:00:00Z
        assert_eq!(m.get("expires_at").unwrap(), "2026-06-04T01:00:00Z");
    }

    #[test]
    fn token_set_requires_refresh_token() {
        // onboarding 拿不到 refresh_token 必须报错(否则号白上)。
        let r = serde_json::json!({"access_token":"at","expires_in":3600});
        assert!(parse_token_set(&r, 0).is_err());
    }

    #[test]
    fn token_set_requires_access_token() {
        let r = serde_json::json!({"refresh_token":"rt"});
        assert!(parse_token_set(&r, 0).is_err());
    }

    #[test]
    fn token_set_no_expires_in_omits_expires_at() {
        let r = serde_json::json!({"access_token":"at","refresh_token":"rt"});
        let m = parse_token_set(&r, 99999).unwrap();
        assert!(!m.contains_key("expires_at"));
    }
}
