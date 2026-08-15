//! Prefix 缓存命中模拟器 —— 移植自 `gw-kiro::cache_sim`,口径逐条对齐。
//!
//! ## 为什么 cursor 也需要它
//!
//! Cursor 上游的 `1.14` 用量帧带 `{input, output, cached}`,但生产实测(2026-08-11,
//! 85 条成功请求仅 4 条 cached>0)Cursor 后端**几乎不按 Anthropic 语义建/报缓存**:
//! 连续两条请求 71.7% 字节级公共前缀、客户端带 `cache_control: ephemeral` 断点,
//! 上游照样自报 cached=0。Run 协议请求侧没有 cache_control 概念可传,建不建缓存
//! 完全是 Cursor 服务端行为,我方不可控。
//!
//! 结果是同一批客户在 kiro 通道(有 cache_sim)账单缓存命中 90%+,到 cursor 通道
//! 变 0% 命中、全价 input,扣费口径不一致。本模块按 kiro 同一家口径模拟命中,
//! 让两条通道的客户账单可比。**这是计费策略,不是事实断言**:模拟值绝不写
//! `real_cache_read_tokens`(对账列只认上游自报)。
//!
//! ## 口径(与 gw-kiro 逐条一致)
//!
//! 1. **同模型才命中**:换模型 → 缓存键变 → 整段 miss。
//! 2. **按账号 + 会话双键隔离**:`account_id \x1f conversation_id`。Cursor 服务端
//!    会话本来就是 per-account(ConvRegistry 换号降级重铺),模拟缓存的边界必须
//!    一样,否则换号后把旧号的前缀当成新号命中 = 无依据的折扣。
//! 3. **基于稳定语义层**:`system` + `tools` + `to_turns` 的逐条消息
//!    (`fold_history` 之前),不是 wire 字节。折叠形态里
//!    `</conversation_history>` 闭合标签和本轮问题横在历史与尾部之间,第二轮
//!    整段不再是上一轮的字节前缀;而逐消息指纹跨轮逐字节稳定(Anthropic
//!    客户端全量重发历史,`extract_text` 确定性渲染、不吃 `cache_control`
//!    等易变字段)。tools 每轮原样重发、位置固定,是真实可缓存前缀,进指纹
//!    (kiro 排除 tools 是因为它的 tools 只挂 currentMessage,这边没那个缝)。
//!    工具回路的「[继续]…」复述和文档注入都是折叠后拼的 wire 尾巴,不进指纹
//!    (保守方向:只少估、不多估缓存)。
//! 4. **tokenize 后比对**:每条消息/每 2KB chunk 一个指纹(内容哈希 + 位置编码),
//!    与上一轮指纹序列求最长公共前缀,覆盖 token 数 = cache_read。
//! 5. **5 分钟 TTL**:对齐 Anthropic ephemeral 缓存;过期 → 下次冷启动全 miss。
//! 6. **peek/commit 两步 + 代际 CAS**:构造时 peek 拿计费值,成功收尾才 commit
//!    (与 ConvRegistry confirm/forget 同语义;内建工具截断路径也算成功 ——
//!    截的是响应,请求轮服务端已处理,与 confirm 保持一致)。commit 带代际号
//!    compare-and-set:同会话并发请求后完成的提交直接丢弃,不乱序覆盖。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 缓存条目存活时间(秒),对齐 Anthropic ephemeral prompt cache(约 5 分钟)。
const ENTRY_TTL_SECS: u64 = 300;

/// 状态表最多保留的会话数(LRU 淘汰),防止内存无界增长。
const MAX_SESSIONS: usize = 4096;

/// 单条消息切 chunk 指纹的字节阈值。真实 prefix cache 是 token 级的:长文本末尾
/// 变一字节,前面部分仍然命中。整条单指纹会在末尾一变整条 miss(误报全价);
/// 2KB ≈ 500–700 token,与 kiro 同一阈值同一份理由。
const FINGERPRINT_CHUNK_BYTES: usize = 2048;

/// 单条消息/单个 chunk 的指纹:内容哈希 + 估算 token 数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgFingerprint {
    /// 内容的 64-bit 哈希(FNV-1a + 位置编码,碰撞概率对计费近似足够低)。
    pub hash: u64,
    /// 该指纹覆盖的估算 token 数。
    pub tokens: u32,
}

/// FNV-1a 64-bit。只用于相等判断,不涉及安全,快且零依赖。
fn fnv1a(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// **纯函数**:上一轮与本轮指纹序列的最长公共前缀覆盖的 token 总数。
///
/// 缓存命中的是从头连续相同的那段;一旦某个 chunk 变了(或之后的新增部分),
/// 其后全部算未命中。
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

/// 单会话的缓存状态:上一轮指纹序列 + 模型 + 最后访问时间 + 代际号。
#[derive(Debug, Clone)]
struct SessionEntry {
    model: String,
    prev: Vec<MsgFingerprint>,
    last_seen: Instant,
    /// 代际号:peek 时读出、commit 时比对(compare-and-set)。同会话的两个并发
    /// 请求都基于同一旧状态 peek,后完成的 commit 会因代际不匹配被丢弃 ——
    /// 否则乱序覆盖会让模拟表里的「上一轮」与服务端真实会话顺序脱节。
    gen: u64,
}

/// 模拟结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimResult {
    /// 模拟的 cache_read token 数(冷启动 / 换模型 / 换账号 / 无公共前缀 → 0)。
    pub cache_read_tokens: u32,
    /// 本轮指纹覆盖的总 token(公共前缀 + 新增),便于上层封顶/校验。
    pub total_tokens: u32,
}

/// 按会话键索引的缓存状态表(LRU + TTL)。
pub struct CacheSimStore {
    inner: Mutex<HashMap<String, SessionEntry>>,
    ttl_secs: AtomicU64,
    max_sessions: AtomicUsize,
    /// 代际号分配器(全局单调递增即可,只用于同键的新旧比较)。
    next_gen: AtomicU64,
}

impl CacheSimStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl_secs: AtomicU64::new(ENTRY_TTL_SECS),
            max_sessions: AtomicUsize::new(MAX_SESSIONS),
            next_gen: AtomicU64::new(1),
        }
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.load(Ordering::Relaxed).max(1))
    }

    fn max_sessions(&self) -> usize {
        self.max_sessions.load(Ordering::Relaxed).max(1)
    }

    /// 读一半:只算「若上一轮已建立,本轮能命中多少」,**不写状态**。
    ///
    /// 返回模拟结果 + 当前条目的代际号(无条目 = 0),代际号要原样传给
    /// [`Self::commit_at`] 做 compare-and-set:同会话并发请求里,后完成的
    /// commit 若发现代际已被别人推进,直接丢弃,防止乱序覆盖。
    ///
    /// 生产路径在请求构造时调用它拿计费值;状态提交见 [`Self::commit_at`]。
    /// 拆开的原因:模拟状态必须与 `ConvRegistry` 的成功语义一致 —— 本轮失败时
    /// 服务端没落下这一轮(注册表 forget,下轮 Opening 重铺),若失败前就把指纹
    /// 写进模拟表,会出现「服务端冷启动重铺、本地却按热状态折扣」的错位。
    pub fn peek_at(
        &self,
        session_key: &str,
        model: &str,
        curr: &[MsgFingerprint],
        now: Instant,
    ) -> (SimResult, u64) {
        let total_tokens: u64 = curr.iter().map(|m| m.tokens as u64).sum();
        let ttl = self.ttl();
        // 毒化即恢复:纯计费缓存,绝不能让它变成请求的 panic 点。
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (cache_read, gen) = match map.get(session_key) {
            Some(entry) => {
                let hit = if entry.model == model && now.duration_since(entry.last_seen) <= ttl {
                    prefix_cache_read(&entry.prev, curr)
                } else {
                    0
                };
                (hit, entry.gen)
            }
            None => (0, 0),
        };
        (
            SimResult {
                cache_read_tokens: cache_read.min(total_tokens).min(u32::MAX as u64) as u32,
                total_tokens: total_tokens.min(u32::MAX as u64) as u32,
            },
            gen,
        )
    }

    /// 写一半:本轮**成功收尾**后把指纹存为下一轮的"上一轮"。失败轮绝不调用。
    ///
    /// `expect_gen` 必须是本轮 peek 读到的代际号;表中条目已被别的并发请求推进
    /// (或被淘汰)时本次提交直接丢弃(返回 false)——乱序覆盖会让模拟表的
    /// 「上一轮」与服务端真实会话顺序脱节,宁可少记一轮(保守方向:下个请求
    /// 照常 peek,只是命中按别人的上一轮算)。
    pub fn commit_at(
        &self,
        session_key: &str,
        model: &str,
        curr: Vec<MsgFingerprint>,
        now: Instant,
        expect_gen: u64,
    ) -> bool {
        let ttl = self.ttl();
        let cap = self.max_sessions();
        let gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let cur_gen = map.get(session_key).map(|e| e.gen).unwrap_or(0);
        if cur_gen != expect_gen {
            return false;
        }
        map.insert(
            session_key.to_string(),
            SessionEntry {
                model: model.to_string(),
                prev: curr,
                last_seen: now,
                gen,
            },
        );
        if map.len() > cap {
            evict(&mut map, now, ttl, cap);
        }
        true
    }

    /// 观测一次请求:peek + commit 合一,**仅供测试**摆状态用;
    /// 生产必须走 peek/commit 两步(见 [`Self::peek_at`])。
    #[cfg(test)]
    pub fn observe_at(
        &self,
        session_key: &str,
        model: &str,
        curr: Vec<MsgFingerprint>,
        now: Instant,
    ) -> SimResult {
        let (r, gen) = self.peek_at(session_key, model, &curr, now);
        self.commit_at(session_key, model, curr, now, gen);
        r
    }
}

/// 清理过期条目;仍超量则按 last_seen 最旧优先淘汰到容量内。
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

/// 业务入口(读):用全局状态表算本轮模拟 cache_read,不写状态。
/// 返回的代际号要原样传给 [`commit`]。
pub fn peek(session_key: &str, model: &str, curr: &[MsgFingerprint]) -> (SimResult, u64) {
    global().peek_at(session_key, model, curr, Instant::now())
}

/// 业务入口(写):本轮成功收尾后,用全局状态表提交本轮指纹。
/// `expect_gen` 来自本轮的 [`peek`];代际不匹配 = 并发乱序,丢弃。
pub fn commit(session_key: &str, model: &str, curr: Vec<MsgFingerprint>, expect_gen: u64) -> bool {
    global().commit_at(session_key, model, curr, Instant::now(), expect_gen)
}

/// 从稳定语义层抽取指纹序列:`system` → `tools` → `to_turns` 逐条消息
/// (**fold_history 之前**的形态 —— 折叠是 wire concern,见模块注释第 3 条),
/// 与 Anthropic 请求体的物理顺序一致。
///
/// `est` 是与用量估算同一个 token 估算器(`chat::est_text_tokens`),保证
/// Σfingerprint.tokens 与 fallback 的 input 估算同口径,封顶才有意义。
///
/// 工具定义**进**指纹(与 kiro 的取舍不同):kiro 那边 tools 只挂在 currentMessage
/// 上、history 里没有,逐条指纹会在接缝处错配,只能排除;cursor 这边 tools 每轮
/// 原样重发、位置固定在 system 之后,是真实的可缓存前缀(Claude Code 工具清单
/// 上万 token,排除了会系统性低估命中)。文档注入/「[继续]」复述是折叠后拼的
/// wire 尾巴,仍不进指纹(保守方向:只少估、不多估)。
pub fn fingerprints_from_context(
    system: &str,
    tools: &[crate::run::ToolDef],
    turns: &[crate::run::Turn],
    est: impl Fn(&str) -> u64,
) -> Vec<MsgFingerprint> {
    let mut fps = Vec::with_capacity(turns.len() + 2);
    let sys_canon = format!("S\x1f{system}");
    push_chunk_fingerprints(&sys_canon, 0, &est, &mut fps);
    // 工具 canon:name/description/schema 按请求原序拼接。中途换工具集 → 此处
    // 失配 → 前缀断在 tools,后面的消息全 miss —— 这正是真实缓存的语义。
    let mut tools_canon = String::from("T");
    for t in tools {
        tools_canon.push('\x1f');
        tools_canon.push_str(&t.name);
        tools_canon.push('\x1e');
        tools_canon.push_str(&t.description);
        tools_canon.push('\x1e');
        tools_canon.push_str(&t.schema);
    }
    push_chunk_fingerprints(&tools_canon, 1, &est, &mut fps);
    for (i, t) in turns.iter().enumerate() {
        let canon = format!("{}\x1f{}", if t.is_user { "U" } else { "A" }, t.text);
        // msg_idx 从 2 起:0=system、1=tools。位置编入哈希,防跨位误命中。
        push_chunk_fingerprints(&canon, (i + 2) as u32, &est, &mut fps);
    }
    fps
}

/// 把一条 canon 文本切成若干 **chunk 指纹**追加进 `out`。
///
/// 切分按 UTF-8 字符边界;短文本(≤1 chunk)仍单指纹。`msg_idx` 与 chunk 序号
/// 一起编入哈希:否则裸 substring 在序列错位时会跨位误命中(false-high → 少计费)。
///
/// 【token 守恒】整条只估一次 token,再按各 chunk 字节占比分摊(估算非线性,
/// 逐 chunk 估会失真),保证 Σchunk.tokens == 整条 token。
fn push_chunk_fingerprints(
    canon: &str,
    msg_idx: u32,
    est: &impl Fn(&str) -> u64,
    out: &mut Vec<MsgFingerprint>,
) {
    let total_tokens = est(canon).min(u32::MAX as u64);
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

/// chunk 指纹哈希:内容 + 位置(msg_idx, chunk_idx)。
fn chunk_hash(text: &str, msg_idx: u32, chunk_idx: u32) -> u64 {
    let mut h = fnv1a(text);
    h ^= (msg_idx as u64).rotate_left(32) ^ chunk_idx as u64;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Turn;

    /// 与生产同款的估算器(ASCII/4 + 非 ASCII/1)。
    fn est(s: &str) -> u64 {
        let mut ascii = 0u64;
        let mut other = 0u64;
        for c in s.chars() {
            if c.is_ascii() {
                ascii += 1;
            } else {
                other += 1;
            }
        }
        ascii.div_ceil(4) + other
    }

    fn fp(hash: u64, tokens: u32) -> MsgFingerprint {
        MsgFingerprint { hash, tokens }
    }

    fn turn(text: &str, is_user: bool) -> Turn {
        Turn {
            text: text.to_string(),
            is_user,
        }
    }

    fn tool(name: &str) -> crate::run::ToolDef {
        crate::run::ToolDef {
            name: name.into(),
            description: format!("{name} desc"),
            schema: "{\"type\":\"object\"}".into(),
        }
    }

    /// 同会话并发:两个请求基于同一旧状态 peek,后完成的 commit 必须被
    /// 代际 CAS 拒掉,表里的「上一轮」保持是先完成那个的(审查高危#1)。
    #[test]
    fn concurrent_out_of_order_commit_is_rejected() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        // A、B 同时 peek(都看到空表,gen=0)
        let (ra, gen_a) = store.peek_at("s", "m", &[fp(1, 100)], t0);
        let (_rb, gen_b) = store.peek_at("s", "m", &[fp(2, 200)], t0);
        assert_eq!((ra.cache_read_tokens, gen_a, gen_b), (0, 0, 0));
        // B 先完成:提交成功,代际推进。
        assert!(store.commit_at("s", "m", vec![fp(2, 200)], t0 + Duration::from_secs(1), gen_b));
        // A 后完成:代际已变,提交必须被拒,表内容仍是 B 的。
        assert!(!store.commit_at("s", "m", vec![fp(1, 100)], t0 + Duration::from_secs(2), gen_a));
        let (r, _g) = store.peek_at("s", "m", &[fp(2, 200), fp(3, 50)], t0 + Duration::from_secs(3));
        assert_eq!(r.cache_read_tokens, 200, "下一个请求应命中 B 的内容(先完成者)");
    }

    /// 串行轮次代际链不断:每轮 commit 用本轮 peek 的 gen,必须永远成功。
    #[test]
    fn serial_rounds_commit_chain_never_breaks() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let mut prev: Vec<MsgFingerprint> = vec![fp(1, 10)];
        for i in 0..5 {
            let (_r, gen) = store.peek_at("s", "m", &prev, t0 + Duration::from_secs(i * 10));
            prev.push(fp(100 + i as u64, 20));
            assert!(
                store.commit_at("s", "m", prev.clone(), t0 + Duration::from_secs(i * 10 + 1), gen),
                "第 {i} 轮串行 commit 必须成功"
            );
        }
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
    fn cold_start_is_miss() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let r = store.observe_at("sess-a", "fable-5", vec![fp(1, 100), fp(2, 50)], t0);
        assert_eq!(r.cache_read_tokens, 0, "首轮冷启动应 0 命中");
        assert_eq!(r.total_tokens, 150);
    }

    #[test]
    fn second_turn_hits_prev_prefix() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("sess-a", "fable-5", vec![fp(1, 100), fp(2, 50)], t0);
        let r = store.observe_at(
            "sess-a",
            "fable-5",
            vec![fp(1, 100), fp(2, 50), fp(3, 30)],
            t0 + Duration::from_secs(10),
        );
        assert_eq!(r.cache_read_tokens, 150);
        assert_eq!(r.total_tokens, 180);
    }

    #[test]
    fn model_change_is_miss() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("sess-a", "fable-5", vec![fp(1, 100)], t0);
        let r = store.observe_at("sess-a", "gpt-5.6", vec![fp(1, 100)], t0 + Duration::from_secs(10));
        assert_eq!(r.cache_read_tokens, 0, "换模型必须整段 miss");
    }

    #[test]
    fn account_change_is_miss() {
        // 键含 account_id:同一会话内容换号 = 冷启动(服务端会话属于旧号)。
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("acc-A\x1fconv-1", "fable-5", vec![fp(1, 100)], t0);
        let r = store.observe_at(
            "acc-B\x1fconv-1",
            "fable-5",
            vec![fp(1, 100)],
            t0 + Duration::from_secs(10),
        );
        assert_eq!(r.cache_read_tokens, 0, "换账号必须冷启动");
    }

    #[test]
    fn ttl_expiry_is_miss() {
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        store.observe_at("sess-a", "fable-5", vec![fp(1, 100)], t0);
        let r = store.observe_at(
            "sess-a",
            "fable-5",
            vec![fp(1, 100)],
            t0 + Duration::from_secs(ENTRY_TTL_SECS + 1),
        );
        assert_eq!(r.cache_read_tokens, 0, "TTL 过期应冷启动");
    }

    #[test]
    fn long_text_chunk_prefix_is_partial_hit() {
        // 长文本末尾变了:前面 chunk 仍命中,只 miss 尾部(整条单指纹会全 miss)。
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let base = "x".repeat(FINGERPRINT_CHUNK_BYTES * 3);
        let fps1 = fingerprints_from_context("", &[], &[turn(&base, true)], est);
        store.observe_at("sess-a", "fable-5", fps1, t0);

        let mut modified = base.clone();
        modified.push_str("尾巴变了");
        let fps2 = fingerprints_from_context("", &[], &[turn(&modified, true)], est);
        let r = store.observe_at("sess-a", "fable-5", fps2, t0 + Duration::from_secs(10));
        assert!(r.cache_read_tokens > 0, "前缀 chunk 应命中");
        assert!(
            r.cache_read_tokens < r.total_tokens,
            "尾部修改的部分不该命中: {} < {}",
            r.cache_read_tokens,
            r.total_tokens
        );
    }

    #[test]
    fn appended_turn_extends_prefix() {
        // 逐消息形态:第二轮多一条 assistant 回复,第一轮内容整体是逻辑前缀 → 全命中。
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let fps1 = fingerprints_from_context("sys", &[], &[turn("问题一", true)], est);
        let first = store.observe_at("sess-a", "fable-5", fps1, t0);
        assert_eq!(first.cache_read_tokens, 0);

        let fps2 = fingerprints_from_context(
            "sys",
            &[],
            &[turn("问题一", true), turn("回答一", false)],
            est,
        );
        let second = store.observe_at("sess-a", "fable-5", fps2, t0 + Duration::from_secs(30));
        assert_eq!(
            second.cache_read_tokens, first.total_tokens,
            "第一轮内容应整体命中: got {}, prev total {}",
            second.cache_read_tokens, first.total_tokens
        );
        assert!(
            second.cache_read_tokens < second.total_tokens,
            "本轮新增不能算命中"
        );
    }

    #[test]
    fn tools_change_breaks_prefix_after_system() {
        // 工具清单变了 → 前缀断在 tools 块,后面的消息全部 miss,只剩 system 命中
        // (真实缓存语义:tools 在请求体里位于 system 与 messages 之间)。
        let store = CacheSimStore::new();
        let t0 = Instant::now();
        let fps1 = fingerprints_from_context("sys", &[tool("read")], &[turn("问", true)], est);
        let first = store.observe_at("sess-a", "fable-5", fps1, t0);
        let fps2 =
            fingerprints_from_context("sys", &[tool("write")], &[turn("问", true)], est);
        let r = store.observe_at("sess-a", "fable-5", fps2, t0 + Duration::from_secs(10));
        let sys_tokens = est("S\x1fsys") as u32;
        assert_eq!(
            r.cache_read_tokens, sys_tokens,
            "换工具后只剩 system 命中: got {}, first total {}",
            r.cache_read_tokens, first.total_tokens
        );
        // 工具不变时同一会话整段命中(对照)。
        let fps3 =
            fingerprints_from_context("sys", &[tool("write")], &[turn("问", true)], est);
        let r3 = store.observe_at("sess-a", "fable-5", fps3, t0 + Duration::from_secs(20));
        assert!(r3.cache_read_tokens >= r.total_tokens, "工具不变应整段命中");
    }

    #[test]
    fn lru_evicts_oldest() {
        let store = CacheSimStore::new();
        store.max_sessions.store(2, Ordering::Relaxed);
        let t0 = Instant::now();
        store.observe_at("s1", "m", vec![fp(1, 10)], t0);
        store.observe_at("s2", "m", vec![fp(2, 10)], t0 + Duration::from_secs(1));
        store.observe_at("s3", "m", vec![fp(3, 10)], t0 + Duration::from_secs(2));
        // 先验证存货:s2 还在表里 → 应命中(注意任何 observe 都会写状态,
        // 先测 s1 的冷启动会把 s2 挤出去,顺序不能反)。
        let r2 = store.observe_at("s2", "m", vec![fp(2, 10)], t0 + Duration::from_secs(3));
        assert_eq!(r2.cache_read_tokens, 10, "s2 应仍在表内");
        // s1 最老,在 s3 插入时就该被淘汰 → 再观测是冷启动
        let r = store.observe_at("s1", "m", vec![fp(1, 10)], t0 + Duration::from_secs(4));
        assert_eq!(r.cache_read_tokens, 0, "LRU 淘汰后应冷启动");
    }
}
