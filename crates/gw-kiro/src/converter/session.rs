//! 会话标识与 conversationId 稳定派生(v55:system 锚点 + 前2条 user 哈希)。

use sha2::{Digest, Sha256};
use super::MessagesRequest;

/// 当前轮的消息范围(尾部连续 user 消息的起止下标,半开区间 `start..end`)。
///
/// 🟢 借鉴 static_flow `current_user_message_range`:从最后一条 user 消息向前回溯,
/// 把**尾部连续的 user 消息**整体作为"当前轮"。end = 最后一条 user 的下标+1,
/// start = 从该处向前直到遇到非 user 消息为止。
///
/// 动机:Claude Code / 代理链常把一轮拆成多条连续 user(text 与 tool_result 分条发),
/// 旧逻辑"只取最后一条作 current"会把同一当前轮的前半截误归 history,使 history 前缀
/// 随本轮内容抖动 → 毒化 Kiro prefix cache。把尾部连续 user 整体锚定为当前轮可避免。
///
/// 前置条件:`messages` 经过 prefill 预处理,末尾必为 user(调用方保证)。若无任何 user
/// 消息返回 None(调用方应已在更早处用 EmptyMessages 拦截)。
pub(super) fn current_user_message_range(
    messages: &[crate::anthropic_types::Message],
) -> Option<std::ops::Range<usize>> {
    let end = messages.iter().rposition(|m| m.role == "user").map(|i| i + 1)?;
    let mut start = end - 1;
    while start > 0 && messages[start - 1].role == "user" {
        start -= 1;
    }
    Some(start..end)
}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
pub(super) fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if session_part.len() >= 36 {
            let uuid_str = &session_part[..36];
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
pub(super) fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 归一化客户端 system 文本，用于 agent 身份区分。
///
/// 与 `build_history` 里 client_system 的口径**完全一致**（strip_rolling_fingerprints
/// 逐段剥除 + `\n` join），保证用于身份哈希的文本和实际折叠进 history[0] 的 system 同源。
/// 无 system 或空 system 都返回空串。
///
/// 注(块1a):strip_rolling_fingerprints 已上收到 [`super::normalize`] 作为单一实现
/// (精确 billing strip)。conversationId 锚点继续走本函数(pre-routing 的 top-level system),
/// 不含块1a 新增的 role=system 路由产物,故 conversationId 跨轮稳定。
pub(super) fn normalized_client_system(req: &MessagesRequest) -> String {
    req.system
        .as_ref()
        .map(|system| {
            system
                .iter()
                .map(|s| super::normalize::strip_rolling_fingerprints(&s.text))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 按 UTF-8 字符边界安全截取 `s` 的前 `max_bytes` 字节（不在多字节字符中间切断）。
/// 返回的子串字节长度 ≤ max_bytes。
pub(super) fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // 从 max_bytes 往前找最近的 char 边界
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

///
/// Anthropic 客户端（如 Claude Code）通常不传 metadata.user_id 或不带 session_id，
/// 导致每次都生成新 UUID，使 Kiro 后端无法做 prompt prefix cache，token 全额计费。
///
/// 取最早若干条 user 消息内容做 SHA-256，截 16 字节排成 UUID v4 形态。
/// 同一对话的连续 turn 前缀稳定 → 命中同一 conversationId 槽。
/// /compact 等真正重建上下文的场景，第一条 user 内容会变，自然换槽，符合语义。
///
/// 【v55】叠加 `system_anchor`（客户端 system 的稳定前缀）：
/// Kiro 报文无独立 system 字段，system 折叠进 history[0] 排最前。Claude Code 在同一终端
/// 会话里跑多个 agent（主对话 / `/title` / security-monitor），它们 **system 不同但前 2 条
/// user 开头相同**（都含同一段 `<session>` 终端内容），旧逻辑只看前 2 条 user 会把它们归并成
/// 同一 conversationId，交错请求时 history[0] 反复横跳、prefix cache 互踩全 miss
/// （实测：被污染 ID 承载 49% 请求、占 35% 真实成本）。
/// 把 system 前缀喂进哈希后，不同 agent 得到不同 conversationId、各自独立缓存槽。
///
/// `system_anchor` 取 system **前若干字节前缀**（由调用点 `safe_prefix` 截断，当前 8192 字节）：
/// system 折叠块从前到后混了三类内容——agent 角色（开头）、机器/用户特征（CLAUDE.md 路径，
/// char ~2500）、滚动参数（`Today's date`，char ~57400）。取前 8192 字节恰覆盖前两者、避开第三者，
/// 故既能分开 agent/用户、又不会被每日变化的日期误拆健康会话。**空 / 纯空白 system 时哈希输入
/// 与旧版逐字节一致**，无 system 的老会话 conversationId 不变，零迁移成本。
pub(super) fn derive_conversation_id_from_messages(
    messages: &[crate::anthropic_types::Message],
    system_anchor: &str,
) -> String {
    let mut hasher = Sha256::new();
    // v55：先把 system 锚点喂进哈希。
    // 回归约束（关键）：**空 / 纯空白 system 时哈希输入必须与旧版逐字节一致**（旧版只哈希
    // 前 2 条 user：`user1\x00user2\x00`），否则无 system 的老会话会冷启动。故用 trim 判断，
    // `Some(["",""])` join 成 "\n"、纯空白等都视同无 system，跳过 sys 段。
    // 防碰撞（Skeptic #2）：system 段用 8 字节大端**长度前缀**框定，而非仅靠 `\x00` 分隔——
    // SystemMessage.text 可含 ``，仅用 NUL 分隔可被构造跨段碰撞；长度前缀使边界不可伪造。
    if !system_anchor.trim().is_empty() {
        let bytes = system_anchor.as_bytes();
        hasher.update(b"sys:");
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    // 取前 2 条 user 消息（一般是 system 之外最稳定的对话开场）
    let mut taken = 0;
    for msg in messages {
        if msg.role == "user" {
            // content 是 serde_json::Value，序列化后哈希
            let s = serde_json::to_string(&msg.content).unwrap_or_default();
            hasher.update(s.as_bytes());
            hasher.update(b"\x00");
            taken += 1;
            if taken >= 2 {
                break;
            }
        }
    }
    let digest = hasher.finalize();
    // 取前 16 字节排成 UUID 字符串（不强求真 v4 标志位，Kiro 只看字符串相等）
    let bytes: [u8; 16] = digest[..16].try_into().expect("sha256 has 32 bytes");
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
        u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_be_bytes(bytes[6..8].try_into().unwrap()),
        u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
        u64::from_be_bytes({
            let mut b = [0u8; 8];
            b[2..].copy_from_slice(&bytes[10..16]);
            b
        }) & 0x0000_ffff_ffff_ffff,
    )
}

// 注:agentContinuationId 派生已删除——自造该 ID 上 wire 会让 Kiro 绕过 conversationId 前缀
// 缓存致全 miss(2026-06-13 实锤),static_flow/kiro.rs 均不发。详见 converter/mod.rs 根因注释。
