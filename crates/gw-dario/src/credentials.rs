use std::collections::BTreeMap;
use crate::format_rfc3339_z;

/// CC 格式:{"claudeAiOauth":{"accessToken","refreshToken","expiresAt"(unix ms),...}}
pub fn parse_cc_credentials(text: &str) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    let oauth = v.get("claudeAiOauth").ok_or("缺 claudeAiOauth 块")?;
    let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap_or_default();
    let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap_or_default();
    if access.is_empty() && refresh.is_empty() { return Err("accessToken 与 refreshToken 均空".into()); }
    let mut extra = BTreeMap::new();
    if !access.is_empty() { extra.insert("access_token".into(), serde_json::json!(access)); }
    if !refresh.is_empty() { extra.insert("refresh_token".into(), serde_json::json!(refresh)); }
    if let Some(ms) = oauth.get("expiresAt").and_then(|v| v.as_i64()) {
        extra.insert("expires_at".into(), serde_json::json!(format_rfc3339_z(ms / 1000)));
    }
    Ok(extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn parses_claude_ai_oauth() {
        let t = r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":1780531200000}}"#;
        let e = parse_cc_credentials(t).unwrap();
        assert_eq!(e.get("access_token").unwrap(), "at");
        assert_eq!(e.get("refresh_token").unwrap(), "rt");
        assert_eq!(e.get("expires_at").unwrap(), "2026-06-04T00:00:00Z"); // ms→s→Z
    }
    #[test] fn errors_without_oauth_block() { assert!(parse_cc_credentials(r#"{"foo":1}"#).is_err()); }
    #[test]
    fn errors_when_both_tokens_empty() {
        assert!(parse_cc_credentials(r#"{"claudeAiOauth":{"accessToken":"","refreshToken":""}}"#).is_err());
    }
}
