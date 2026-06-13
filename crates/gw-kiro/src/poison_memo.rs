//! 毒报文备忘录：上游确定性 400 的请求体指纹缓存。
//!
//! 🔵 搬运自 kiro.rs v63(400 重试风暴四联修)。背景:一个客户的会话史带大 PDF/图,
//! 序列化后超 Kiro 的报文体积上限 → 上游确定性 400 "Improperly formed request"。即便
//! 我们已把 400 透传给客户端(caio 的 `BadRequest`→客户端 400),仍有**无视 400 仍重试**
//! 的客户端(此次事故的 NewAPI 上游就是),会反复打同一坏 payload 刷 error 恐触发风控。
//!
//! 本模块兜住这类客户端:同一字节级 payload 已被上游确定性 400 过,TTL 内再来直接
//! 本地 400(`BadRequest`,不发上游、不惩罚账号),把 error 暴露压到 0。
//!
//! 设计:
//! - 指纹 = 请求体 SHA-256(碰撞概率密码学可忽略,不会误伤不同 payload);
//! - 调用方持有指纹(避免对 MB 级 body 做 clone):chat 路径算一次 fingerprint,
//!   发包前 `is_poisoned(&fp)`、发包后确定性 400 时 `remember(fp)`,body 可放心 move 进发包;
//! - 绝对 TTL(默认 10 分钟),到期放行一次重新探测上游,仍 400 则再记;
//! - 容量上限 512,超限淘汰最老条目;锁中毒时 into_inner 恢复——旁路组件绝不 panic 主路径。

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 备忘录条目 TTL:到期后放行一次,让上游重新裁决(防止误记永久封禁某 payload)。
const POISON_TTL: Duration = Duration::from_secs(600);
/// 容量上限:超限淘汰最老条目(正常运行时条目数应接近 0,这是防御性护栏)。
const POISON_CAP: usize = 512;

/// 请求体指纹(SHA-256)。
pub type Fingerprint = [u8; 32];

type MemoMap = HashMap<Fingerprint, Instant>;

fn memo() -> MutexGuard<'static, MemoMap> {
    static MEMO: OnceLock<Mutex<MemoMap>> = OnceLock::new();
    // 锁中毒恢复:备忘录是纯 HashMap,任意中间态都安全可用,绝不让旁路组件 panic 主路径。
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 算请求体指纹(发包前算一次,持有它即可,无需 clone body)。
pub fn fingerprint(body: &str) -> Fingerprint {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    h.finalize().into()
}

/// 查询:此请求体是否在 TTL 内被上游确定性 400 过。
pub fn is_poisoned(fp: &Fingerprint) -> bool {
    let mut map = memo();
    match map.get(fp) {
        Some(at) if at.elapsed() < POISON_TTL => true,
        Some(_) => {
            map.remove(fp);
            false
        }
        None => false,
    }
}

/// 记录:此请求体刚被上游确定性 400 拒绝。
pub fn remember(fp: Fingerprint) {
    let now = Instant::now();
    let mut map = memo();
    map.retain(|_, at| at.elapsed() < POISON_TTL);
    if map.len() >= POISON_CAP {
        if let Some((&oldest, _)) = map.iter().min_by_key(|(_, at)| **at) {
            map.remove(&oldest);
        }
    }
    map.insert(fp, now);
    tracing::warn!(
        "毒报文备忘录:记入请求体指纹(TTL {}s 内同 payload 将本地 400,不再打上游),当前条目 {}",
        POISON_TTL.as_secs(),
        map.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_byte_exact() {
        let a = fingerprint("hello-world");
        assert_eq!(a, fingerprint("hello-world"), "同内容指纹相同");
        assert_ne!(a, fingerprint("hello-worlD"), "差一个字节指纹不同");
    }

    #[test]
    fn memo_roundtrip_ttl_and_miss() {
        let fp = fingerprint(&format!("poison-body-{}", std::process::id()));
        assert!(!is_poisoned(&fp), "未记录前不应命中");
        remember(fp);
        assert!(is_poisoned(&fp), "记录后应命中");
        let other = fingerprint(&format!("clean-body-{}", std::process::id()));
        assert!(!is_poisoned(&other), "不同 payload 不应命中");
    }
}
