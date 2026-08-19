//! Prefix 缓存命中模拟器 —— 🔵 搬自旧 `src/kiro/cache_sim.rs`(v53,含 chunk 指纹修复)。
//!
//! ## 为什么需要它
//!
//! Kiro 上游对部分模型(opus-4-7 / 4-8 等)在 `tokenUsageEvent` 里**下发真实的**
//! `cacheReadInputTokens`,那是最准的命中值;但另一些模型上游**不下发**,只能模拟。
//! v53 起统一走本模拟器(砍掉真值/metering 反推三层),产出稳定可控的命中曲线。
//!
//! 本模块**按 Anthropic prompt prefix cache 的真实工作原理**模拟命中:
//!
//! 1. **同模型才命中**:换模型 → 缓存键变 → 整段 miss。
//! 2. **基于处理后上下文**:用 `build_history` 之后真正发给 Kiro 的消息序列
//!    (system + history + currentMessage),不是用户原始请求。
//! 3. **tokenize 后比对**:用 [`crate::text_tokens::count_tokens`] 给每条消息估 token,
//!    与上一轮指纹序列求**最长公共前缀**,公共前缀覆盖的 token 数 = cache_read。
//! 4. **5 分钟 TTL**:对齐 Anthropic ephemeral 缓存;条目过期 → 下次冷启动全 miss。
//!
//! ## 架构
//!
//! - [`prefix_cache_read`]:**纯函数**,算最长公共前缀的 token 数。无副作用、易测。
//! - [`CacheSimStore`]:按 `session_key` 索引的内存状态表(LRU + TTL),线程安全。
//! - [`observe`] 业务入口——传会话键/模型/本轮指纹,返回模拟命中(同时存指纹供下轮比对)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 缓存条目存活时间默认值(秒),对齐 Anthropic ephemeral prompt cache(约 5 分钟)。
const DEFAULT_ENTRY_TTL_SECS: u64 = 300;

/// 状态表最多保留的会话数默认值(LRU 淘汰),防止内存无界增长。
const DEFAULT_MAX_SESSIONS: usize = 4096;

/// 单条消息切 chunk 指纹的字节阈值。超过此长度的消息按此大小切多个 chunk,使公共前缀
/// 匹配精确到 chunk 级、对齐 Kiro 的 token 级 prefix cache。约 2KB ≈ 500–700 token。
const FINGERPRINT_CHUNK_BYTES: usize = 2048;

/// 单条消息的指纹:内容哈希 + 该消息的估算 token 数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgFingerprint {
    /// 消息内容的 64-bit 哈希(FNV-1a,碰撞概率对计费近似足够低)。
    pub hash: u64,
    /// 该消息的估算 token 数。
    pub tokens: u32,
}

/// 计算字符串的 FNV-1a 64-bit 哈希。指纹只用于"同位置消息是否相同"的相等判断,
/// 不涉及安全,FNV 快且零依赖。
fn fnv1a(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// 从一段文本构造消息指纹(哈希 + token 估算)。仅测试使用:生产路径走
/// [`push_chunk_fingerprints`]/[`chunk_hash`](带位置编码与 chunk 切分)。
#[cfg(test)]
pub fn fingerprint(text: &str) -> MsgFingerprint {
    MsgFingerprint {
        hash: fnv1a(text),
        tokens: crate::text_tokens::count_tokens(text).min(u32::MAX as u64) as u32,
    }
}

/// **纯函数**:给定上一轮与本轮指纹序列,算最长公共前缀的 token 总数。
///
/// 这是真实 prefix cache 的核心:缓存命中的是从头连续相同的那段消息;一旦某条变了
/// (或本轮更长的新增部分),其后全部算未命中。返回命中 token 数(公共前缀消息 token 之和)。
pub fn prefix_cache_read(prev: &[MsgFingerprint], curr: &[MsgFingerprint]) -> u64 {
    let mut hit: u64 = 0;
    for (a, b) in prev.iter().zip(curr.iter()) {
        if a.hash == b.hash {
            hit += b.tokens as u64;
        } else {
            break;
        }
    }
    hit
}

/// 单会话的缓存状态:上一轮发给 Kiro 的指纹序列 + 模型 + 最后访问时间。
#[derive(Debug, Clone)]
struct SessionEntry {
    model: String,
    prev: Vec<MsgFingerprint>,
    last_seen: Instant,
}

/// 模拟结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimResult {
    /// 模拟的 cache_read token 数(冷启动 / 换模型 / 无公共前缀 → 0)。
    pub cache_read_tokens: u32,
    /// 本轮上下文总 token(公共前缀 + 新增),便于上层算比例 / 校验。
    pub total_tokens: u32,
}

/// 按会话键索引的缓存状态表(LRU + TTL)。TTL 与容量上限运行时可热调(config.cache)。
pub struct CacheSimStore {
    inner: Mutex<HashMap<String, SessionEntry>>,
    ttl_secs: AtomicU64,
    max_sessions: AtomicUsize,
}

impl CacheSimStore {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl_secs: AtomicU64::new(DEFAULT_ENTRY_TTL_SECS),
            max_sessions: AtomicUsize::new(DEFAULT_MAX_SESSIONS),
        }
    }

    /// 热调条目 TTL(秒,0 视为 1 避免立即过期)。
    pub fn set_ttl_secs(&self, secs: u64) {
        self.ttl_secs.store(secs.max(1), Ordering::Relaxed);
    }

    /// 热调最大会话数(0 视为 1)。
    pub fn set_max_sessions(&self, n: usize) {
        self.max_sessions.store(n.max(1), Ordering::Relaxed);
    }

    /// 当前生效的 TTL(秒)—— 权威 live 值(admin 读回用)。
    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs.load(Ordering::Relaxed)
    }

    /// 当前生效的最大会话数 —— 权威 live 值。
    pub fn max_sessions_value(&self) -> usize {
        self.max_sessions.load(Ordering::Relaxed)
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.load(Ordering::Relaxed).max(1))
    }

    fn max_sessions(&self) -> usize {
        self.max_sessions.load(Ordering::Relaxed).max(1)
    }

    /// 观测一次请求:返回模拟 cache_read,并把本轮指纹存为下一轮的"上一轮"。
    ///
    /// 命中条件(全满足才有 cache_read > 0):会话键已有上一轮记录、模型与上一轮相同、
    /// 上一轮未过 TTL、本轮与上一轮存在非空公共前缀。
    ///
    /// `now` 显式传入便于测试;生产用 [`observe`] 包装传 `Instant::now()`。
    pub fn observe_at(
        &self,
        session_key: &str,
        model: &str,
        curr: Vec<MsgFingerprint>,
        now: Instant,
    ) -> SimResult {
        let total_tokens: u64 = curr.iter().map(|m| m.tokens as u64).sum();
        let ttl = self.ttl();
        let cap = self.max_sessions();

        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let cache_read = match map.get(session_key) {
            Some(entry)
                if entry.model == model && now.duration_since(entry.last_seen) <= ttl =>
            {
                prefix_cache_read(&entry.prev, &curr)
            }
            _ => 0,
        };

        map.insert(
            session_key.to_string(),
            SessionEntry {
                model: model.to_string(),
                prev: curr,
                last_seen: now,
            },
        );

        if map.len() > cap {
            evict(&mut map, now, ttl, cap);
        }

        SimResult {
            cache_read_tokens: cache_read.min(total_tokens).min(u32::MAX as u64) as u32,
            total_tokens: total_tokens.min(u32::MAX as u64) as u32,
        }
    }
}

/// 清理过期条目;若清理后仍超量,按 last_seen 最旧优先淘汰到容量内。
fn evict(map: &mut HashMap<String, SessionEntry>, now: Instant, ttl: Duration, cap: usize) {
    map.retain(|_, e| now.duration_since(e.last_seen) <= ttl);
    while map.len() > cap {
        if let Some(oldest_key) = map
            .iter()
            .min_by_key(|(_, e)| e.last_seen)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest_key);
        } else {
            break;
        }
    }
}

/// 全局单例状态表。
pub fn global() -> &'static CacheSimStore {
    static STORE: OnceLock<CacheSimStore> = OnceLock::new();
    STORE.get_or_init(CacheSimStore::new)
}

/// 业务入口:用全局状态表观测一次请求(`now = Instant::now()`)。
pub fn observe(session_key: &str, model: &str, curr: Vec<MsgFingerprint>) -> SimResult {
    global().observe_at(session_key, model, curr, Instant::now())
}

/// 从处理后的 [`ConversationState`] 抽取指纹序列。
///
/// 顺序严格对齐发给 Kiro 的真实 prefix:`history[0..]` + `currentMessage`。
///
/// **关键(v53 修复"用户提问轮"崩盘)**:每条消息的指纹只取其**稳定语义内容**
/// —— role + 正文 + tool_results(id/状态/内容) + tool_uses(id/name/input) + 图片/文档
/// 计数。**刻意忽略两类东西**:① `tools` 列表(几百个工具定义,不是对话内容,只挂在
/// currentMessage 上);② 容器结构差异(`UserInputMessage` vs `UserMessage`/`AssistantMessage`)。
///
/// 为什么必须这样:同一句用户输入,本轮在 `currentMessage`(带 tools、`UserInputMessage`
/// 结构),下一轮沉淀进 `history`(不带 tools、`UserMessage` 结构)。若按整条 JSON 算指纹,
/// 同一句话在两轮指纹必然不同 → 公共前缀在 history/current 接缝处 break → 每个"用户提问轮"
/// 被算成低命中甚至 0(线上间歇崩盘的真正机理)。改取稳定语义内容后跨轮指纹一致,前缀平滑增长。
///
/// 注意:**不含** `conversationId` / `agentContinuationId` 等会话级元数据。
pub fn fingerprints_from_state(
    state: &crate::kiro_types::conversation::ConversationState,
) -> Vec<MsgFingerprint> {
    use crate::kiro_types::conversation::Message;
    let mut fps = Vec::with_capacity(state.history.len() + 1);
    for (idx, msg) in state.history.iter().enumerate() {
        let canon = match msg {
            Message::User(u) => canon_user(
                &u.user_input_message.content,
                &u.user_input_message.user_input_message_context.tool_results,
                u.user_input_message.images.len(),
                u.user_input_message.documents.len(),
            ),
            Message::Assistant(a) => canon_assistant(
                &a.assistant_response_message.content,
                a.assistant_response_message.tool_uses.as_deref(),
                a.assistant_response_message.reasoning_content.as_ref(),
            ),
        };
        push_chunk_fingerprints(&canon, idx as u32, &mut fps);
    }
    // current message 的逻辑位置 = history.len():它下一轮沉淀进 history 时正好是这个 index,
    // 故 msg_idx 跨轮稳定(history append-only,前面消息不变)。
    let cur = &state.current_message.user_input_message;
    let canon = canon_user(
        &cur.content,
        &cur.user_input_message_context.tool_results,
        cur.images.len(),
        cur.documents.len(),
    );
    push_chunk_fingerprints(&canon, state.history.len() as u32, &mut fps);
    fps
}

/// 把一条 canon 消息切成若干 **chunk 指纹**追加进 `out`。
///
/// 【为什么分 chunk】真实 Kiro/Bedrock prefix cache 是 **token 级**的:一条 47K 字符的
/// system 折叠块,末尾某处(skill 路径、`Today's date` 跨午夜)变一字节,Kiro 仍命中前面
/// 4 万字符、只 miss 尾部。旧实现整条当**单个**指纹,末尾一变整条 miss(线上 10% 大请求
/// Kiro 真命中却被报 0、用户全价)。改按固定字节切 chunk 后,公共前缀精确到 chunk 级
/// (真实报文重放:旧 0% → 新 58.7%,与 Kiro metering 实测 ~0.5-0.6 吻合)。
///
/// 切分按 **UTF-8 字符边界**;短消息(≤1 chunk)仍单指纹,行为与旧版一致。
///
/// 【防误命中】`msg_idx` 与 chunk 序号一起**编入哈希**:否则裸 substring 在序列错位时,
/// 一条 user 的尾 chunk 可能与另一条的 chunk 字节相同而误判公共前缀(false-high → 少计费)。
///
/// 【token 守恒】整条只 `count_tokens` 一次,再按各 chunk **字节占比**分摊(count_tokens
/// 非线性,逐 chunk count 会失真)。保证 Σchunk.tokens == 整条 token。
fn push_chunk_fingerprints(canon: &str, msg_idx: u32, out: &mut Vec<MsgFingerprint>) {
    let total_tokens = crate::text_tokens::count_tokens(canon).min(u32::MAX as u64);
    if canon.len() <= FINGERPRINT_CHUNK_BYTES {
        out.push(MsgFingerprint {
            hash: chunk_hash(canon, msg_idx, 0),
            tokens: total_tokens as u32,
        });
        return;
    }
    let bytes = canon.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let mut end = (start + FINGERPRINT_CHUNK_BYTES).min(bytes.len());
        while end < bytes.len() && !canon.is_char_boundary(end) {
            end -= 1;
        }
        ranges.push((start, end));
        start = end;
    }
    let total_bytes = bytes.len() as u64;
    let mut assigned: u64 = 0;
    for (ci, (s, e)) in ranges.iter().enumerate() {
        let tok = if ci == ranges.len() - 1 {
            total_tokens - assigned
        } else {
            let t = total_tokens * (*e - *s) as u64 / total_bytes;
            assigned += t;
            t
        };
        out.push(MsgFingerprint {
            hash: chunk_hash(&canon[*s..*e], msg_idx, ci as u32),
            tokens: tok.min(u32::MAX as u64) as u32,
        });
    }
}

/// chunk 指纹哈希:内容 + 位置(msg_idx, chunk_idx)。位置编入哈希防跨位误命中。
fn chunk_hash(text: &str, msg_idx: u32, chunk_idx: u32) -> u64 {
    let mut h = fnv1a(text);
    h ^= (msg_idx as u64).rotate_left(32) ^ chunk_idx as u64;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    h
}

/// 规范化一条 user 消息为稳定语义字符串(current 与 history 走同一逻辑)。
/// 忽略 tools 列表与容器结构差异,使同一内容跨轮指纹一致。
fn canon_user(
    content: &str,
    tool_results: &[crate::kiro_types::tool::ToolResult],
    n_images: usize,
    n_documents: usize,
) -> String {
    let mut s = String::with_capacity(content.len() + 64);
    s.push_str("U\x1f");
    s.push_str(content);
    for tr in tool_results {
        s.push_str("\x1ftr:");
        s.push_str(&tr.tool_use_id);
        s.push('\x1e');
        if let Some(st) = &tr.status {
            s.push_str(st);
        }
        s.push('\x1e');
        if let Ok(c) = serde_json::to_string(&tr.content) {
            s.push_str(&c);
        }
    }
    if n_images > 0 {
        s.push_str(&format!("\x1fimg:{}", n_images));
    }
    if n_documents > 0 {
        s.push_str(&format!("\x1fdoc:{}", n_documents));
    }
    s
}

/// 规范化一条 assistant 消息为稳定语义字符串。
///
/// `reasoning_content`(2026-08-19 结构化历史上传)必须入指纹:它在真实报文里占字节,
/// 滑窗挂/摘、剥离重试都会改变真实 prefix —— 漏掉它会在 reasoning 摘除后仍误判后续
/// 历史为缓存命中(高报 cache_read)。
fn canon_assistant(
    content: &str,
    tool_uses: Option<&[crate::kiro_types::tool::ToolUseEntry]>,
    reasoning_content: Option<&crate::kiro_types::conversation::ReasoningContent>,
) -> String {
    let mut s = String::with_capacity(content.len() + 64);
    s.push_str("A\x1f");
    s.push_str(content);
    if let Some(tus) = tool_uses {
        for tu in tus {
            s.push_str("\x1ftu:");
            s.push_str(&tu.tool_use_id);
            s.push('\x1e');
            s.push_str(&tu.name);
            s.push('\x1e');
            if let Ok(inp) = serde_json::to_string(&tu.input) {
                s.push_str(&inp);
            }
        }
    }
    if let Some(rc) = reasoning_content {
        use crate::kiro_types::conversation::ReasoningContent;
        match rc {
            ReasoningContent::ReasoningText { reasoning_text } => {
                s.push_str("\x1frt:");
                s.push_str(&reasoning_text.text);
                s.push('\x1e');
                s.push_str(&reasoning_text.signature);
            }
            ReasoningContent::Redacted { redacted_content } => {
                s.push_str("\x1frc:");
                s.push_str(redacted_content);
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_assistant_reasoning_changes_fingerprint() {
        // codex 审查:reasoning_content 必须入指纹 —— 挂/摘/换签名都要产生不同 canon,
        // 否则滑窗摘掉 reasoning 后模拟器仍按旧前缀判命中,高报 cache_read。
        use crate::kiro_types::conversation::{ReasoningContent, ReasoningText};
        let base = canon_assistant("答", None, None);
        let with_rt = canon_assistant(
            "答",
            None,
            Some(&ReasoningContent::ReasoningText {
                reasoning_text: ReasoningText {
                    text: "推".into(),
                    signature: "sigA".into(),
                },
            }),
        );
        let other_sig = canon_assistant(
            "答",
            None,
            Some(&ReasoningContent::ReasoningText {
                reasoning_text: ReasoningText {
                    text: "推".into(),
                    signature: "sigB".into(),
                },
            }),
        );
        let redacted = canon_assistant(
            "答",
            None,
            Some(&ReasoningContent::Redacted {
                redacted_content: "abc".into(),
            }),
        );
        assert_ne!(base, with_rt, "挂 reasoning 必须改变 canon");
        assert_ne!(with_rt, other_sig, "换签名必须改变 canon");
        assert_ne!(with_rt, redacted, "reasoning 两种形态必须区分");
    }

    fn fp(hash: u64, tokens: u32) -> MsgFingerprint {
        MsgFingerprint { hash, tokens }
    }

    #[test]
    fn empty_prefix_is_zero() {
        assert_eq!(prefix_cache_read(&[], &[fp(1, 10)]), 0);
        assert_eq!(prefix_cache_read(&[fp(1, 10)], &[]), 0);
    }

    #[test]
    fn full_common_prefix_sums_tokens() {
        let prev = vec![fp(1, 10), fp(2, 20), fp(3, 30)];
        let curr = vec![fp(1, 10), fp(2, 20), fp(3, 30), fp(4, 40)];
        assert_eq!(prefix_cache_read(&prev, &curr), 60);
    }

    #[test]
    fn divergence_stops_prefix() {
        let prev = vec![fp(1, 10), fp(2, 20), fp(3, 30)];
        let curr = vec![fp(1, 10), fp(99, 20), fp(3, 30)];
        assert_eq!(prefix_cache_read(&prev, &curr), 10);
    }

    #[test]
    fn first_message_divergence_zero_hit() {
        let prev = vec![fp(1, 10), fp(2, 20)];
        let curr = vec![fp(99, 10), fp(2, 20)];
        assert_eq!(prefix_cache_read(&prev, &curr), 0);
    }

    #[test]
    fn cold_start_is_miss() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let r = store.observe_at("sess-a", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        assert_eq!(r.cache_read_tokens, 0, "首轮冷启动应 0 命中");
        assert_eq!(r.total_tokens, 150);
    }

    #[test]
    fn second_turn_hits_growing_prefix() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        let t1 = t0 + Duration::from_secs(5);
        let r = store.observe_at(
            "s",
            "opus-4-7",
            vec![fp(1, 100), fp(2, 50), fp(3, 30), fp(4, 20)],
            t1,
        );
        assert_eq!(r.cache_read_tokens, 150, "应命中前两条 100+50");
        assert_eq!(r.total_tokens, 200);
    }

    #[test]
    fn model_switch_is_full_miss() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        let r = store.observe_at(
            "s",
            "opus-4-6",
            vec![fp(1, 100), fp(2, 50), fp(3, 30)],
            t0 + Duration::from_secs(5),
        );
        assert_eq!(r.cache_read_tokens, 0, "换模型应全 miss");
    }

    #[test]
    fn ttl_expiry_is_cold_again() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        let r = store.observe_at(
            "s",
            "opus-4-7",
            vec![fp(1, 100), fp(2, 50), fp(3, 30)],
            t0 + Duration::from_secs(DEFAULT_ENTRY_TTL_SECS + 1),
        );
        assert_eq!(r.cache_read_tokens, 0, "TTL 过期应冷启动 0 命中");
    }

    #[test]
    fn within_ttl_still_hits() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        let r = store.observe_at(
            "s",
            "opus-4-7",
            vec![fp(1, 100), fp(2, 50), fp(3, 30)],
            t0 + Duration::from_secs(DEFAULT_ENTRY_TTL_SECS - 1),
        );
        assert_eq!(r.cache_read_tokens, 150, "TTL 边界内应命中");
    }

    #[test]
    fn different_sessions_isolated() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s1", "opus-4-7", vec![fp(1, 100)], t0);
        let r = store.observe_at("s2", "opus-4-7", vec![fp(1, 100), fp(2, 50)], t0);
        assert_eq!(r.cache_read_tokens, 0, "不同会话应隔离");
    }

    #[test]
    fn fingerprint_stable_and_distinct() {
        let a = fingerprint("hello world");
        let b = fingerprint("hello world");
        let c = fingerprint("hello worle");
        assert_eq!(a.hash, b.hash, "相同文本指纹应一致");
        assert_ne!(a.hash, c.hash, "不同文本指纹应不同");
        assert!(a.tokens > 0);
    }

    #[test]
    fn short_message_is_single_chunk() {
        let mut out = Vec::new();
        push_chunk_fingerprints("short message", 0, &mut out);
        assert_eq!(out.len(), 1, "短消息应只产 1 个指纹");
        assert_eq!(out[0].tokens, crate::text_tokens::count_tokens("short message") as u32);
    }

    #[test]
    fn long_message_splits_into_chunks() {
        let big = "x".repeat(FINGERPRINT_CHUNK_BYTES * 5 + 100);
        let mut out = Vec::new();
        push_chunk_fingerprints(&big, 0, &mut out);
        assert_eq!(out.len(), 6, "5*CHUNK+100 应切成 6 个 chunk");
        assert!(out.iter().all(|f| f.tokens > 0));
    }

    #[test]
    fn chunk_tokens_conserve_whole_message_count() {
        let big = "y".repeat(FINGERPRINT_CHUNK_BYTES * 4 + 777);
        let mut out = Vec::new();
        push_chunk_fingerprints(&big, 0, &mut out);
        let sum: u64 = out.iter().map(|f| f.tokens as u64).sum();
        assert_eq!(
            sum,
            crate::text_tokens::count_tokens(&big),
            "分 chunk 的 token 之和必须等于整条估算(守恒)"
        );
    }

    #[test]
    fn chunk_fingerprint_encodes_position() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        push_chunk_fingerprints("same content here", 3, &mut a);
        push_chunk_fingerprints("same content here", 7, &mut b);
        assert_ne!(a[0].hash, b[0].hash, "相同内容不同 msg_idx 指纹必须不同");
    }

    #[test]
    fn tail_change_only_breaks_tail_chunk() {
        let head = "A".repeat(FINGERPRINT_CHUNK_BYTES * 10);
        let prev_msg = format!("{head}TAIL_OLD");
        let curr_msg = format!("{head}TAIL_NEW");
        let mut prev = Vec::new();
        push_chunk_fingerprints(&prev_msg, 0, &mut prev);
        let mut curr = Vec::new();
        push_chunk_fingerprints(&curr_msg, 0, &mut curr);

        let hit = prefix_cache_read(&prev, &curr);
        let total: u64 = curr.iter().map(|f| f.tokens as u64).sum();
        assert!(hit > 0, "尾部变动不应让整条 miss");
        assert!(hit < total, "尾部 chunk 应 miss");
        let last_chunk_tokens = curr.last().unwrap().tokens as u64;
        assert_eq!(hit, total - last_chunk_tokens, "应恰好命中除最后 chunk 外的全部");
    }

    #[test]
    fn chunk_split_respects_utf8_boundary() {
        let unit = "中文🦀";
        let big = unit.repeat(FINGERPRINT_CHUNK_BYTES);
        let mut out = Vec::new();
        push_chunk_fingerprints(&big, 0, &mut out);
        assert!(out.len() > 1, "应切成多个 chunk");
        assert!(out.iter().all(|f| f.tokens > 0));
    }

    #[test]
    fn chunked_long_system_hits_across_turns_in_store() {
        let head = "S".repeat(FINGERPRINT_CHUNK_BYTES * 8);
        let mut prev = Vec::new();
        push_chunk_fingerprints(&format!("{head}date:0603"), 0, &mut prev);
        let mut curr = Vec::new();
        push_chunk_fingerprints(&format!("{head}date:0604"), 0, &mut curr);

        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("sess", "opus-4-8", prev, t0);
        let r = store.observe_at("sess", "opus-4-8", curr, t0 + Duration::from_secs(1));
        assert!(r.cache_read_tokens > 0, "尾部日期变动不应让超长 system 整条 miss");
    }

    #[test]
    fn cache_read_clamped_to_total() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("s", "m", vec![fp(1, 100), fp(2, 100)], t0);
        let r = store.observe_at("s", "m", vec![fp(1, 100)], t0 + Duration::from_secs(1));
        assert!(r.cache_read_tokens <= r.total_tokens);
        assert_eq!(r.cache_read_tokens, 100);
    }

    #[test]
    fn eviction_keeps_within_capacity() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        for i in 0..(DEFAULT_MAX_SESSIONS + 100) {
            store.observe_at(
                &format!("sess-{i}"),
                "m",
                vec![fp(i as u64, 10)],
                t0 + Duration::from_millis(i as u64),
            );
        }
        let len = store.inner.lock().unwrap().len();
        assert!(len <= DEFAULT_MAX_SESSIONS, "淘汰后应不超过容量, len={len}");
    }

    #[test]
    fn fingerprints_from_state_orders_history_then_current() {
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, HistoryAssistantMessage, HistoryUserMessage,
            Message, UserInputMessage,
        };
        let mut state = ConversationState::new("conv-x");
        state.history = vec![
            Message::User(HistoryUserMessage::new("sys+u1", "opus-4-7")),
            Message::Assistant(HistoryAssistantMessage::new("a1")),
        ];
        let mut cur = UserInputMessage::default();
        cur.content = "u2".to_string();
        cur.model_id = "opus-4-7".to_string();
        state.current_message = CurrentMessage::new(cur);

        let fps = fingerprints_from_state(&state);
        assert_eq!(fps.len(), 3);
        assert!(fps.iter().all(|f| f.tokens > 0));
        assert_ne!(fps[0].hash, fps[1].hash);
        assert_ne!(fps[1].hash, fps[2].hash);
    }

    #[test]
    fn fingerprints_change_when_history_grows() {
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, Message, UserInputMessage,
        };
        let mut s1 = ConversationState::new("c");
        s1.history = vec![Message::User(HistoryUserMessage::new("u1", "m"))];
        let mut c1 = UserInputMessage::default();
        c1.content = "q1".into();
        s1.current_message = CurrentMessage::new(c1);
        let f1 = fingerprints_from_state(&s1);

        let mut s2 = ConversationState::new("c");
        s2.history = vec![Message::User(HistoryUserMessage::new("u1", "m"))];
        let f2 = fingerprints_from_state(&s2);
        assert_eq!(f1[0].hash, f2[0].hash, "相同首条 history 指纹应稳定");
    }

    #[test]
    fn current_message_fingerprint_stable_after_sinking_into_history() {
        // v53 核心回归:同一句用户输入,本轮在 currentMessage(带 tools,UserInputMessage
        // 结构),下一轮沉淀进 history(不带 tools,UserMessage 结构)。两者指纹必须相同。
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, Message, UserInputMessage,
            UserInputMessageContext, UserMessage,
        };
        use crate::kiro_types::tool::{InputSchema, Tool, ToolSpecification};

        let make_tool = |name: &str| Tool {
            tool_specification: ToolSpecification {
                name: name.to_string(),
                description: "x".to_string(),
                input_schema: InputSchema::from_json(serde_json::json!({"type": "object"})),
            },
        };

        let mut cur = UserInputMessage::default();
        cur.content = "解释这段代码".to_string();
        cur.model_id = "opus-4-8".to_string();
        cur.user_input_message_context = UserInputMessageContext {
            tool_results: vec![],
            tools: (0..295).map(|i| make_tool(&format!("tool_{i}"))).collect(),
        };
        let mut s_cur = ConversationState::new("c");
        s_cur.current_message = CurrentMessage::new(cur);
        let f_cur = fingerprints_from_state(&s_cur);
        let cur_fp = *f_cur.last().unwrap();

        let mut sunk = UserMessage::new("解释这段代码", "opus-4-8");
        sunk.user_input_message_context = UserInputMessageContext::default();
        let mut s_next = ConversationState::new("c");
        s_next.history = vec![Message::User(HistoryUserMessage {
            user_input_message: sunk,
        })];
        let mut cur2 = UserInputMessage::default();
        cur2.content = "下一个问题".to_string();
        s_next.current_message = CurrentMessage::new(cur2);
        let f_next = fingerprints_from_state(&s_next);
        let sunk_fp = f_next[0];

        assert_eq!(
            cur_fp.hash, sunk_fp.hash,
            "同一句话在 current(带295 tools) 与沉淀进 history 后指纹必须一致——缓存不崩的命门"
        );
        assert_eq!(cur_fp.tokens, sunk_fp.tokens, "token 数也应一致");
    }

    #[test]
    fn tools_list_does_not_affect_fingerprint() {
        use crate::kiro_types::conversation::{
            ConversationState, CurrentMessage, UserInputMessage, UserInputMessageContext,
        };
        use crate::kiro_types::tool::{InputSchema, Tool, ToolSpecification};
        let make_tool = |name: &str| Tool {
            tool_specification: ToolSpecification {
                name: name.to_string(),
                description: "x".to_string(),
                input_schema: InputSchema::from_json(serde_json::json!({"type": "object"})),
            },
        };
        let mut a = UserInputMessage::default();
        a.content = "hi".into();
        let mut b = a.clone();
        a.user_input_message_context = UserInputMessageContext {
            tool_results: vec![],
            tools: vec![],
        };
        b.user_input_message_context = UserInputMessageContext {
            tool_results: vec![],
            tools: (0..50).map(|i| make_tool(&format!("t{i}"))).collect(),
        };
        let mut sa = ConversationState::new("c");
        sa.current_message = CurrentMessage::new(a);
        let mut sb = ConversationState::new("c");
        sb.current_message = CurrentMessage::new(b);
        assert_eq!(
            fingerprints_from_state(&sa).last().unwrap().hash,
            fingerprints_from_state(&sb).last().unwrap().hash,
            "tools 多寡不应影响指纹"
        );
    }
}
