//! cachePoint / tools-in-prefix 实验开关(默认关,见各 fn 注释)。
//!
//! `tools_in_prefix` / `cache_point` 两个 on/off 开关可经**设置面板热控**(DB overlay →
//! 30s settings 轮询 → [`crate::KiroProvider::apply_hot_settings`] → [`set_experimental_flags`]);
//! env(`KIRO_TOOLS_IN_PREFIX` / `KIRO_CACHE_POINT`)仍作启动默认(后向兼容)。其余子参数
//! (cachePoint type/place/config)是 dormant 实验的高级旋钮,保持 env-only。
#![allow(clippy::doc_lazy_continuation)] // 迁移注释原样保留,不重排

use std::sync::{OnceLock, RwLock};

/// 可热控的实验开关快照。
#[derive(Clone, Copy)]
struct ExperimentalFlags {
    tools_in_prefix: bool,
    cache_point: bool,
    /// 发稳定 agentContinuationId+vibe(真实缓存命中 A/B)。
    agent_continuation: bool,
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 进程级实验开关(默认从 env 取,settings 热应用经 [`set_experimental_flags`] 覆盖)。
fn experimental() -> &'static RwLock<ExperimentalFlags> {
    static G: OnceLock<RwLock<ExperimentalFlags>> = OnceLock::new();
    G.get_or_init(|| {
        RwLock::new(ExperimentalFlags {
            tools_in_prefix: env_flag("KIRO_TOOLS_IN_PREFIX"),
            cache_point: env_flag("KIRO_CACHE_POINT"),
            agent_continuation: env_flag("KIRO_AGENT_CONTINUATION"),
        })
    })
}

/// 热应用实验开关(worker 30s settings 轮询经 apply_hot_settings 调用)。
pub(crate) fn set_experimental_flags(tools_in_prefix: bool, cache_point: bool, agent_continuation: bool) {
    if let Ok(mut g) = experimental().write() {
        g.tools_in_prefix = tools_in_prefix;
        g.cache_point = cache_point;
        g.agent_continuation = agent_continuation;
    }
}

/// 实验开关：把 tools 放进 history[0] 前缀（可被 Kiro prefix cache 缓存），
/// 而非每轮全价重发的 currentMessage。默认关(env `KIRO_TOOLS_IN_PREFIX=1` 或设置面板启用)。
///
/// 背景：tools(数万 token)放在 currentMessage 时排在增长的 history 之后，
/// 永远落在"缓存分歧点之后"被全价重算。挪到 history[0] 理论上能进稳定前缀。
/// **未验证 Kiro 后端是否仍会向当前轮提供这些工具**——实测会让部分客户端工具调用失效,
/// 金标准 static_flow 亦不用此招,默认保持关闭。
pub(super) fn tools_in_prefix_enabled() -> bool {
    experimental().read().map(|g| g.tools_in_prefix).unwrap_or(false)
}

/// 实验开关：将 Anthropic `cache_control` 翻译为 Kiro `cachePoint`。
/// 默认关(env `KIRO_CACHE_POINT=1` 或设置面板启用)。
///
/// 【2026-05-24 实测结论 —— 已证实是 no-op，默认保持关闭】
/// 用独立可写 DB 做 A/B（多轮对话 + 唯一 nonce 冷启动，每配置连发 3 次）：
///   - BASELINE（无 cachePoint）：冷 call cached=0 metering=0.0732，热 call cached=7100 metering=0.0394
///   - cachePoint(type=default)  ：冷/热数字与 BASELINE **完全一致**
/// 即 Kiro 按 conversationId 自动做 prefix-cache，显式 cachePoint marker 对缓存零影响。
/// 另：`type` 必须是 `"default"`（Bedrock 约定），用 `EPHEMERAL`/`PERSISTENT` 会被
/// Kiro 拒为 400 "Improperly formed request."。
/// 真正降低输入的杠杆是「稳定 conversationId」（见本文件 derive_conversation_id_*），
/// 不是 cachePoint。代码保留为 dormant 实验，供 Python 迁移参考，勿在 prod 开启。
pub(super) fn cache_point_enabled() -> bool {
    experimental().read().map(|g| g.cache_point).unwrap_or(false)
}

/// 实验开关：把**稳定的** agentContinuationId(+ agentTaskType="vibe")发进 conversationState。
/// 默认关(env `KIRO_AGENT_CONTINUATION=1` 启用)。
///
/// 【为什么是开关、为什么默认关】2026-06-13 caio 误判"发此字段即破缓存"删除,线上随后实测
/// 真实缓存命中 **0%**、credit **+49% vs kiro.rs**;而 kiro.rs 生产**一直在发**稳定值并拿到
/// ~43% 命中(其代码实测注释:稳定 agentContinuationId → metering 降 ~36%)。本开关用于在生产做
/// 可逆 A/B:默认关(部署零行为变化),灰度时置 1 复刻 kiro.rs proven 配置、对比 real-hit。
/// 注:必须配 **稳定** conversationId(本 crate 已保证),否则每轮新值反而 miss(kiro.rs 实测)。
/// 走 RwLock 实验全局(与 tools_in_prefix/cache_point 同款),可经设置面板/API **热控**——
/// A/B 期间零重启、零丢请求即可翻开关回滚(env `KIRO_AGENT_CONTINUATION` 作启动默认)。
pub(super) fn agent_continuation_enabled() -> bool {
    experimental().read().map(|g| g.agent_continuation).unwrap_or(false)
}

/// 实验参数：cachePoint 的 type 值。默认 EPHEMERAL。
/// 可设 "default" / "PERSISTENT" / "EPHEMERAL" 等以探测 Kiro 接受的枚举。
pub(super) fn cache_point_type() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| std::env::var("KIRO_CACHE_POINT_TYPE").unwrap_or_else(|_| "EPHEMERAL".to_string()))
}

/// 实验参数：是否附带 clientCacheConfig。默认 true。
/// 设 `KIRO_CACHE_POINT_CONFIG=0` 可只发 cachePoint、不发 clientCacheConfig。
pub(super) fn cache_point_with_config() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("KIRO_CACHE_POINT_CONFIG")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true)
    })
}

/// 实验参数：cachePoint 放置位置。默认 "current"（currentMessage）。
/// 设 "history" 则改打在 history 最后一条 user 上（探测前缀缓存语义）。
pub(super) fn cache_point_placement() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| std::env::var("KIRO_CACHE_POINT_PLACE").unwrap_or_else(|_| "current".to_string()))
}

/// 检测 Anthropic 消息 content 中是否含有 `cache_control` 标记。
///
/// content 可能是 string 或 ContentBlock 数组。string 不会带 cache_control；
/// 数组里只要任一 block 含 `cache_control`，即认为这条消息标记了"缓存到此处"。
pub(super) fn anthropic_message_has_cache_control(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::Array(arr) => arr.iter().any(|item| {
            item.as_object()
                .map(|obj| obj.contains_key("cache_control"))
                .unwrap_or(false)
        }),
        _ => false,
    }
}
