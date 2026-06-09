//! 路由上下文与 key 派生。
//!
//! - `session_id`:决定 router 把请求转发到哪个 worker(会话亲和)。
//!   从 Anthropic `metadata.user_id` 提取,提不到则按报文内容派生稳定 hash,
//!   仍提不到才随机。这保证同一会话稳定命中同一 worker → 同一组号 → 缓存/IP 稳定。
//! - `cache_key`:worker 内部组内账号的缓存亲和(同前缀打同号)。

use sha2::{Digest, Sha256};

/// 一次请求的路由上下文。
#[derive(Debug, Clone)]
pub struct RoutingContext {
    /// 鉴权通过的客户端 key id。
    pub client_key_id: String,
    /// 会话标识(router 据此选 worker)。
    pub session_id: String,
    /// 会话标识的来源(用于诊断/日志)。
    pub session_source: SessionSource,
    /// 对外模型 id。
    pub model: String,
    /// 缓存亲和 key(worker 内部选号用)。
    pub cache_key: String,
    /// 是否流式。
    pub stream: bool,
}

/// session_id 的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    /// 从 Anthropic metadata.user_id 显式提取。
    Metadata,
    /// 报文内容派生的稳定 hash(metadata 缺失时)。
    DerivedFromContent,
    /// 随机兜底(连内容都无法派生时)。
    Random,
}

/// 从 Anthropic `metadata.user_id` 提取 session_id。
///
/// 支持两种形态(对齐 static_flow session.rs):
/// - JSON 串里含 `"session_id"` 字段
/// - legacy 字符串 `session_<uuid>` / `user_<...>_session_<...>`
///
/// 返回 None 表示无法从 metadata 提取。
pub fn extract_session_from_metadata(user_id: &str) -> Option<String> {
    let trimmed = user_id.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 形态 1:JSON 串
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }

    // 形态 2:子串 `session_<token>`
    if let Some(pos) = trimmed.find("session_") {
        let rest = &trimmed[pos + "session_".len()..];
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !token.is_empty() {
            return Some(format!("session_{token}"));
        }
    }

    None
}

/// 短 hex 摘要(取 SHA-256 前 16 字节 = 32 hex 字符)。
pub fn short_hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]); // 分隔,避免拼接歧义
    }
    let digest = h.finalize();
    hex_encode(&digest[..16])
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_json_metadata() {
        let uid = r#"{"session_id":"abc-123","other":1}"#;
        assert_eq!(extract_session_from_metadata(uid), Some("abc-123".into()));
    }

    #[test]
    fn extract_from_legacy_session_substring() {
        let uid = "user_42_session_deadbeef-99";
        assert_eq!(
            extract_session_from_metadata(uid),
            Some("session_deadbeef-99".into())
        );
    }

    #[test]
    fn extract_none_when_absent() {
        assert_eq!(extract_session_from_metadata(""), None);
        assert_eq!(extract_session_from_metadata("plain-user"), None);
    }

    #[test]
    fn short_hash_stable_and_sized() {
        let a = short_hash(&["model", "history"]);
        let b = short_hash(&["model", "history"]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn short_hash_separator_prevents_collision() {
        // ["ab","c"] 与 ["a","bc"] 不应碰撞
        assert_ne!(short_hash(&["ab", "c"]), short_hash(&["a", "bc"]));
    }
}
