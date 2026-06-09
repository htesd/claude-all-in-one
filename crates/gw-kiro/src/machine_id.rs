//! 设备指纹(machineId)生成 —— 🔵 搬运旧 `src/kiro/machine_id.rs` 派生算法。
//!
//! **封号铁律**(见 memory rewrite-recon-findings / ¥900 教训):同一账号的
//! machineId 必须跨"激活/刷新/发包"始终一致,否则触发风控。machineId 嵌在上游
//! User-Agent 末尾(`KiroIDE-{version}-{machineId}`)。
//!
//! 核心派生公式**逐字节照搬旧代码**(改一个字节 = 换设备指纹 = 封号):
//! - OAuth(social/IdC):`sha256("KotlinNativeAPI/" + refreshToken)`
//! - API Key:`sha256("KiroAPIKey/" + apiKey)`
//! - 显式 machineId:normalize(64hex 直用 / UUID 去横线重复一次)
//! - 兜底:`sha256("KiroFallback/" + random_uuid)`,按账号 id 进程内缓存
//!
//! 与旧代码的差异(🟢适配,非改算法):输入源从旧 `KiroCredentials`/`Config` 改为
//! [`gw_core::account::Account`] 的 `extra` 字段(`machine_id` / `refresh_token` /
//! `kiro_api_key` / `auth_method`),字段名与旧凭证对齐,派生公式不变。

use std::collections::HashMap;
use std::sync::OnceLock;

use gw_core::account::Account;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// 兜底 machineId 缓存(按账号 id 分桶,进程生命周期内稳定)。
/// key 为 `account.account_id`;保证同一账号多次调用返回同一兜底值。
static FALLBACK_MACHINE_IDS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// 标准化 machineId 格式(🔵 逐字节照搬旧 normalize_machine_id)。
///
/// - 64 字符十六进制:直接返回
/// - UUID 格式(去连字符 32 hex):重复一次补齐到 64 字符
/// - 其他:None
fn normalize_machine_id(machine_id: &str) -> Option<String> {
    let trimmed = machine_id.trim();

    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(trimmed.to_string());
    }

    let without_dashes: String = trimmed.chars().filter(|c| *c != '-').collect();
    if without_dashes.len() == 32 && without_dashes.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("{without_dashes}{without_dashes}"));
    }

    None
}

/// 账号是否为 API Key 凭据(🟢 适配:旧 KiroCredentials::is_api_key_credential)。
///
/// 规则与旧代码一致:`auth_method == "api_key"`,或显式带了非空 `kiro_api_key`。
/// API Key 与 refreshToken 两条派生路径**互斥**,不回落。
fn is_api_key_credential(account: &Account) -> bool {
    if account.extra_str("auth_method") == Some("api_key") {
        return true;
    }
    account
        .extra_str("kiro_api_key")
        .is_some_and(|k| !k.is_empty())
}

/// 根据账号生成 machineId。优先级(🔵 与旧 generate_from_credentials 一致):
/// 1. 账号显式 `machine_id`(格式合法时)
/// 2. 按凭据类型派生(api_key / refresh_token 互斥)
/// 3. 兜底:随机种子派生,按 account_id 进程内缓存
///
/// 注:旧代码还有"全局 config.machineId"这一级,新架构无全局 machineId 配置
/// (每账号自持),故省略该级——其余优先级与派生公式逐字节一致。
pub fn generate_from_account(account: &Account) -> String {
    // 1. 账号显式 machineId
    if let Some(machine_id) = account.extra_str("machine_id") {
        if let Some(normalized) = normalize_machine_id(machine_id) {
            return normalized;
        }
    }

    // 2. 按凭据类型派生(互斥,不回落)
    if is_api_key_credential(account) {
        if let Some(api_key) = account.extra_str("kiro_api_key") {
            if !api_key.is_empty() {
                return sha256_hex(&format!("KiroAPIKey/{api_key}"));
            }
        }
    } else if let Some(refresh_token) = account.extra_str("refresh_token") {
        if !refresh_token.is_empty() {
            return sha256_hex(&format!("KotlinNativeAPI/{refresh_token}"));
        }
    }

    // 3. 兜底
    fallback_machine_id(account)
}

/// 为缺失派生材料的账号生成兜底 machineId(🔵 照搬旧 fallback_machine_id)。
/// 按 account_id 进程内缓存;进程重启重新随机;不持久化;首次 warn。
fn fallback_machine_id(account: &Account) -> String {
    let cache = FALLBACK_MACHINE_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock();
    if let Some(existing) = map.get(&account.account_id) {
        return existing.clone();
    }

    let seed = Uuid::new_v4();
    let derived = sha256_hex(&format!("KiroFallback/{seed}"));
    tracing::warn!(
        account_id = %account.account_id,
        "账号缺少派生材料(kiro_api_key/refresh_token 均不可用),使用随机兜底 machineId(进程内稳定)"
    );
    map.insert(account.account_id.clone(), derived.clone());
    derived
}

/// SHA256 → 十六进制字符串(🔵 照搬)。
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex_encode(&hasher.finalize())
}

/// hex 编码(不引入 hex crate,与旧 hex::encode 输出一致:小写)。
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
    use std::collections::BTreeMap;

    fn account_with(extra: &[(&str, &str)]) -> Account {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Account {
            account_id: "test-acct".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: map,
        }
    }

    #[test]
    fn sha256_hex_known_value() {
        // 钉死 hex 编码与旧 hex::encode 一致
        assert_eq!(
            sha256_hex("test"),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn refresh_token_derivation_matches_formula() {
        // 钉死 social/OAuth 派生公式:sha256("KotlinNativeAPI/" + refreshToken)
        // 用固定假 token(非真实凭证)验证公式字节级一致。
        let acct = account_with(&[("refresh_token", "test_refresh_token")]);
        let mid = generate_from_account(&acct);
        assert_eq!(mid.len(), 64);
        assert_eq!(mid, sha256_hex("KotlinNativeAPI/test_refresh_token"));
    }

    #[test]
    fn api_key_derivation_matches_formula() {
        let acct = account_with(&[("kiro_api_key", "ksk_test_api_key")]);
        let mid = generate_from_account(&acct);
        assert_eq!(mid, sha256_hex("KiroAPIKey/ksk_test_api_key"));
    }

    #[test]
    fn api_key_and_refresh_token_mutually_exclusive() {
        // 同时有 api_key 和 refresh_token → 走 api_key 分支
        let acct = account_with(&[
            ("kiro_api_key", "ksk_test"),
            ("refresh_token", "should_not_be_used"),
        ]);
        assert_eq!(generate_from_account(&acct), sha256_hex("KiroAPIKey/ksk_test"));
    }

    #[test]
    fn api_key_auth_method_empty_falls_to_fallback_not_refresh() {
        // auth_method=api_key 但 kiro_api_key 缺失:不回落 refresh_token,走兜底
        let acct = account_with(&[
            ("auth_method", "api_key"),
            ("refresh_token", "should_not_be_used"),
        ]);
        let mid = generate_from_account(&acct);
        assert_eq!(mid.len(), 64);
        assert_ne!(mid, sha256_hex("KotlinNativeAPI/should_not_be_used"));
    }

    #[test]
    fn explicit_machine_id_64hex_passthrough() {
        let acct = account_with(&[("machine_id", &"a".repeat(64))]);
        assert_eq!(generate_from_account(&acct), "a".repeat(64));
    }

    #[test]
    fn explicit_machine_id_uuid_normalized() {
        let acct = account_with(&[("machine_id", "2582956e-cc88-4669-b546-07adbffcb894")]);
        assert_eq!(
            generate_from_account(&acct),
            "2582956ecc884669b54607adbffcb8942582956ecc884669b54607adbffcb894"
        );
    }

    #[test]
    fn normalize_rejects_invalid() {
        assert!(normalize_machine_id("invalid").is_none());
        assert!(normalize_machine_id(&"g".repeat(64)).is_none());
    }

    #[test]
    fn fallback_stable_per_account() {
        let acct = account_with(&[]);
        let a = generate_from_account(&acct);
        let b = generate_from_account(&acct);
        assert_eq!(a, b, "同账号兜底应稳定");
        assert_eq!(a.len(), 64);
    }
}
