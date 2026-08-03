//! 毒报文备忘录：上游确定性 400 的请求体指纹缓存。
//!
//! 🔵 搬运自 kiro.rs v63(400 重试风暴四联修)。背景:一个客户的会话史带大 PDF/图,
//! 序列化后超 Kiro 的报文体积上限 → 上游确定性 400 "Improperly formed request"。即便
//! 我们已把 400 透传给客户端(caio 的 `BadRequest`→客户端 400),仍有**无视 400 仍重试**
//! 的客户端(此次事故的 NewAPI 上游就是),会反复打同一坏 payload 刷 error 恐触发风控。
//!
//! 本模块兜住这类客户端:同一字节级 payload 已被上游**在多个不同账号上**确定性 400 过,
//! TTL 内再来直接本地 400(`BadRequest`,不发上游、不惩罚账号),把 error 暴露压到 0。
//!
//! ## 为什么必须「多账号确认」(2026-08-02 线上事故修复)
//!
//! 原实现只要**任意一个账号**回 400 且报文含 `"Improperly formed"` 就记毒,当时认为这个
//! 短语只出现在真正的报文格式/体积问题上、与账号无关。**这个前提是错的**:
//!
//! 线上 `krs-52`(`profileArn` 与 `machineId` 均缺失的 idc 号)对**任何**请求都回
//! `{"message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}` —— 账号问题
//! 穿着报文格式问题的外衣。后果是:该坏号被调度到谁的请求上,谁的请求体就被全局毒化
//! 600 秒,之后连健康账号都不再尝试,用户被反复告知"请修改请求",而实际换个号本来就能成功。
//!
//! 实测证据:同一份 body 在 `request_logs` 里 **59 次成功、76 次失败**,解压后发给上游的
//! 报文逐字段一致 —— 请求体相同而结果不同,变量在账号侧,不在报文。
//!
//! 修法:条目记录**哪些账号在这份 body 上失败过**,只有**不同账号数 ≥
//! [`MIN_DISTINCT_ACCOUNTS`]** 时才判定为真·毒报文。单账号反复失败只说明那个账号坏,
//! 交给账号生命周期(冷却/禁用)处理,不牵连 body。
//!
//! 设计:
//! - 指纹 = 请求体 SHA-256(碰撞概率密码学可忽略,不会误伤不同 payload);
//! - 调用方持有指纹(避免对 MB 级 body 做 clone):chat 路径算一次 fingerprint,
//!   发包前 `poisoned_reason(&fp)`、发包后确定性 400 时 `remember(fp, account, 上游原文)`;
//! - 绝对 TTL(默认 10 分钟),到期放行一次重新探测上游,仍 400 则再记;
//! - 容量上限 512,超限淘汰最老条目;锁中毒时 into_inner 恢复——旁路组件绝不 panic 主路径。

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 备忘录条目 TTL:到期后放行一次,让上游重新裁决(防止误记永久封禁某 payload)。
const POISON_TTL: Duration = Duration::from_secs(600);
/// 容量上限:超限淘汰最老条目(正常运行时条目数应接近 0,这是防御性护栏)。
const POISON_CAP: usize = 512;
/// 判定「真·毒报文」所需的**不同账号**数。
///
/// 取 1 = 退回旧行为(单账号即毒),会把坏账号的 400 误判成坏报文 —— 见模块文档的
/// krs-52 事故。2 是能区分「账号坏」与「报文坏」的最小取值:一份 body 在两个**不同**
/// 账号上都被上游拒绝,才有理由相信问题出在 body 本身。
const MIN_DISTINCT_ACCOUNTS: usize = 2;

/// 请求体指纹(SHA-256)。
pub type Fingerprint = [u8; 32];

/// 一份 body 的失败记录。
struct Entry {
    /// 在这份 body 上失败过的**不同**账号。达到 [`MIN_DISTINCT_ACCOUNTS`] 才算毒。
    accounts: HashSet<String>,
    /// 最近一次失败时刻(TTL 与容量淘汰都以它为准)。
    at: Instant,
    /// 上游最近一次的原始报文。判毒后透传给客户端,不再用臆测文案掩盖真因。
    upstream_msg: String,
}

type MemoMap = HashMap<Fingerprint, Entry>;

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

/// 查询:此请求体是否已被**多个不同账号**确认为非法。
///
/// 返回 `Some(上游原始报文)` = 判毒,调用方据此本地 400 并把该报文透传给客户端;
/// `None` = 放行。只有一个账号失败过时返回 `None` —— 那是账号问题,该换号重试而非拦下 body。
pub fn poisoned_reason(fp: &Fingerprint) -> Option<String> {
    let mut map = memo();
    match map.get(fp) {
        Some(e) if e.at.elapsed() >= POISON_TTL => {
            map.remove(fp);
            None
        }
        Some(e) if e.accounts.len() >= MIN_DISTINCT_ACCOUNTS => Some(e.upstream_msg.clone()),
        _ => None,
    }
}

/// 记录:此请求体刚在 `account_id` 上被上游确定性 400 拒绝。
///
/// `upstream_msg` 是上游原始报文,判毒后会透传给客户端。
pub fn remember(fp: Fingerprint, account_id: &str, upstream_msg: &str) {
    let now = Instant::now();
    let mut map = memo();
    map.retain(|_, e| e.at.elapsed() < POISON_TTL);
    // 淘汰只在**新增**条目且已满时进行:给已有条目累计账号不该挤掉别人。
    if !map.contains_key(&fp) && map.len() >= POISON_CAP {
        if let Some((&oldest, _)) = map.iter().min_by_key(|(_, e)| e.at) {
            map.remove(&oldest);
        }
    }
    let e = map.entry(fp).or_insert_with(|| Entry {
        accounts: HashSet::new(),
        at: now,
        upstream_msg: String::new(),
    });
    e.accounts.insert(account_id.to_string());
    e.at = now;
    e.upstream_msg = upstream_msg.to_string();
    let distinct = e.accounts.len();
    let entries = map.len();
    if distinct >= MIN_DISTINCT_ACCOUNTS {
        tracing::warn!(
            distinct_accounts = distinct,
            entries,
            "毒报文备忘录:同一请求体已在 {distinct} 个不同账号上被上游拒绝,判定为报文问题,\
             TTL {}s 内本地 400 不再打上游",
            POISON_TTL.as_secs()
        );
    } else {
        // 单账号失败**不判毒**,只留痕。绝大多数情况这是账号坏(profileArn 缺失等),
        // 换号即可成功;当成报文问题会误伤所有用户的同款请求。
        tracing::debug!(
            account = account_id,
            entries,
            "毒报文备忘录:仅 1 个账号在此请求体上失败,暂不判毒(需 {} 个不同账号)",
            MIN_DISTINCT_ACCOUNTS
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 全局备忘录是进程级单例,测试并发跑,必须保证每个用例的指纹互不相同。
    fn fresh(tag: &str) -> Fingerprint {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        fingerprint(&format!(
            "{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn fingerprint_is_byte_exact() {
        let a = fingerprint("hello-world");
        assert_eq!(a, fingerprint("hello-world"), "同内容指纹相同");
        assert_ne!(a, fingerprint("hello-worlD"), "差一个字节指纹不同");
    }

    #[test]
    fn single_account_failure_does_not_poison() {
        let fp = fresh("single");
        assert!(poisoned_reason(&fp).is_none(), "未记录前不应命中");
        remember(fp, "acct-A", "Improperly formed request.");
        assert!(
            poisoned_reason(&fp).is_none(),
            "单账号失败不应判毒 —— 那是账号问题,换号本可成功(2026-08-02 krs-52 事故)"
        );
        // 同一账号反复失败仍不判毒:它只证明这个账号一直坏。
        remember(fp, "acct-A", "Improperly formed request.");
        remember(fp, "acct-A", "Improperly formed request.");
        assert!(poisoned_reason(&fp).is_none(), "同账号重复失败不该累计成毒");
    }

    #[test]
    fn two_distinct_accounts_poison_and_carry_upstream_text() {
        let fp = fresh("double");
        remember(fp, "acct-A", "Improperly formed request.");
        remember(
            fp,
            "acct-B",
            "Improperly formed request. reason=REQUEST_BODY_INVALID",
        );
        let reason = poisoned_reason(&fp).expect("两个不同账号都失败应判毒");
        assert!(
            reason.contains("REQUEST_BODY_INVALID"),
            "判毒后应透传上游最近一次原始报文,实际: {reason}"
        );
    }

    #[test]
    fn distinct_payloads_do_not_interfere() {
        let a = fresh("iso-a");
        let b = fresh("iso-b");
        remember(a, "acct-A", "boom");
        remember(a, "acct-B", "boom");
        assert!(poisoned_reason(&a).is_some(), "a 应判毒");
        assert!(poisoned_reason(&b).is_none(), "不同 payload 不应受牵连");
    }
}
