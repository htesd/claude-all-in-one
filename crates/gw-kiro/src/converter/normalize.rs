//! 块1a:system 净化升级 —— 精确 billing strip + role=system 消息三级分流 + model identity 规范化。
//!
//! 🟢 借鉴 static_flow `converter/{identity,system,normalize}.rs`,但**保留我方既有优势**:
//! - top-level `system` 字段仍折叠进 history[0](我方稳定 conversationId 依赖此口径,不动)。
//! - conversationId 锚点继续走 [`super::session::normalized_client_system`](pre-routing),
//!   本模块的路由/identity 规范化**不参与**身份哈希,保证 conversationId 跨轮稳定、零迁移。
//!
//! 本模块只新增两件事:
//! 1. billing strip 收窄到真正的 CC 滚动指纹行(误伤更小)。
//! 2. 处理 **messages 数组里 `role=="system"` 的消息**(代理链中段注入)——旧逻辑会把它们
//!    静默丢弃(history 构建只认 user/assistant),这里改为三级分流:稳定前缀提升、已知动态
//!    噪声丢弃、其余按序保留为 user 上下文。

use crate::anthropic_types::Message;

const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
const CLAUDE_CODE_CLI_IDENTITY_LINE: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
const CLAUDE_AGENT_SDK_IDENTITY_LINE: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const MODEL_IDENTITY_PREFIX: &str = "You are powered by the model named ";
const MODEL_IDENTITY_DELIMITER: &str = ". The exact model ID is ";

/// 判定一行是否为 Claude Code 的滚动 billing 指纹行。
///
/// 现象:CC 在 system prompt 拼 `x-anthropic-billing-header: cc_version=...; cc_entrypoint=cli; cch=<5位16进制>;`,
/// 其中 `cch` 是每请求都变的 body 哈希 → 即使 Kiro 真做 prompt cache,这行也让命中率永远 0。
///
/// 收窄判据(🟢 static_flow `identity.rs:369-374`):行首(忽略前导空白与大小写)是
/// `x-anthropic-billing-header:`,**且**含 `cc_version=`/`cc_entrypoint=`/`cch=` 之一。
/// 比旧版"删任意 `x-anthropic-` 前缀行"误伤更小——用户正文里碰巧的同前缀行不再被误删。
fn is_billing_fingerprint_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let prefix = BILLING_HEADER_PREFIX.as_bytes();
    // 按字节比较前缀,避免在多字节字符边界上做 str 切片 panic。
    if trimmed.len() < prefix.len() || !trimmed.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        return false;
    }
    // 前缀是纯 ASCII,prefix.len() 必落在 char 边界,可安全切片。
    let rest = &trimmed[prefix.len()..];
    rest.contains("cc_version=") || rest.contains("cc_entrypoint=") || rest.contains("cch=")
}

/// 删除 system 文本里的 CC 滚动 billing 指纹行(保留其余内容与换行结构)。
///
/// 用 `split_inclusive('\n')` 保留每行的尾随换行,使非指纹行**逐字节原样**保留——
/// 这保证 conversationId 锚点(同样调用本函数)在真实 CC 流量上与旧版字节一致(真实流量里
/// 唯一的 `x-anthropic-` 行就是 billing header)。
pub(super) fn strip_rolling_fingerprints(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        if is_billing_fingerprint_line(line) {
            continue;
        }
        out.push_str(line);
    }
    out
}

// === model identity 规范化 ===

/// 由客户端请求的 model 名给出 (短名, 规范 model_id)。未知模型返回 None(不改写)。
/// model_id 取去掉 `-thinking` 后缀的规范名(owned,避免借用入参生命周期)。
fn requested_model_identity(model: &str) -> Option<(&'static str, String)> {
    let id = model.strip_suffix("-thinking").unwrap_or(model);
    let short = match id {
        "claude-opus-4-8" => "Opus 4.8",
        "claude-opus-4-7" => "Opus 4.7",
        "claude-opus-4-6" => "Opus 4.6",
        "claude-sonnet-4-6" => "Sonnet 4.6",
        "claude-sonnet-4-5" => "Sonnet 4.5",
        "claude-haiku-4-5" => "Haiku 4.5",
        _ => return None,
    };
    Some((short, id.to_string()))
}

/// 规范化 system 文本里的 model identity 行,统一成
/// `You are powered by the model named {short}. The exact model ID is {id}.`。
///
/// 🟢 static_flow `identity.rs:319-358`。两种情形:① 已有 identity 行 → 原位替换(保留缩进);
/// ② 无 identity 行但有 CC/Agent-SDK 身份行 → 在其后插入一行。未知模型或都没有 → 原样返回。
/// 防止 Kiro 上游的真实模型代号(claude-quince)泄漏给客户端,对齐声明的官方模型名。
pub(super) fn normalize_model_identity(content: String, model: &str) -> String {
    let Some((short, model_id)) = requested_model_identity(model) else {
        return content;
    };
    let replacement =
        format!("{MODEL_IDENTITY_PREFIX}{short}{MODEL_IDENTITY_DELIMITER}{model_id}.");
    let has_identity = content.lines().any(|l| {
        let t = l.trim_start();
        t.contains(MODEL_IDENTITY_PREFIX) && t.contains(MODEL_IDENTITY_DELIMITER)
    });
    let mut replaced = false;
    let mut inserted = false;
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.contains(MODEL_IDENTITY_PREFIX) && trimmed.contains(MODEL_IDENTITY_DELIMITER)
            {
                replaced = true;
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}{replacement}")
            } else if !has_identity
                && !replaced
                && !inserted
                && (trimmed == CLAUDE_CODE_CLI_IDENTITY_LINE
                    || trimmed == CLAUDE_AGENT_SDK_IDENTITY_LINE)
            {
                inserted = true;
                format!("{line}\n{replacement}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// 续:role=system 消息三级分流(见下方 Edit 追加)
// __ROUTING_PLACEHOLDER__

// === messages 数组里 role=="system" 消息的三级分流 ===

/// 把 Anthropic message 的 content(string 或 text-block 数组)拼成纯文本。
/// 非文本块忽略(system 消息一般纯文本)。
fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(t) = item.get("text").and_then(serde_json::Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// 稳定 system 前缀:SessionStart hook 注入 或 含 CC/Agent-SDK 身份行。
/// 这类内容跨轮稳定,提升进 top-level system(折叠 history[0],利于 prefix cache)。
fn is_stable_system_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("SessionStart hook additional context:")
        || text.lines().map(str::trim_start).any(|l| {
            l == CLAUDE_CODE_CLI_IDENTITY_LINE || l == CLAUDE_AGENT_SDK_IDENTITY_LINE
        })
}

/// 已知动态噪声:每轮变化、对模型无信息增量,丢弃以稳住缓存前缀。
fn is_dynamic_system_noise(text: &str) -> bool {
    text.trim_start()
        .starts_with("The task tools haven't been used recently.")
}

/// interrupted-user 特例:`The user sent a new message while you were working:` 开头的
/// system 消息,实为用户中途插话被框成 system。提取正文(遇 `\n\nIMPORTANT:` 截断)转 user。
fn interrupted_user_payload(text: &str) -> Option<String> {
    let body = text
        .trim_start()
        .strip_prefix("The user sent a new message while you were working:")?;
    let payload = body
        .split_once("\n\nIMPORTANT:")
        .map_or(body, |(p, _)| p)
        .trim();
    (!payload.is_empty()).then(|| payload.to_string())
}

/// 三级分流的产物(仅当确实存在 role=system 消息时返回 Some)。
pub(super) struct RoutedMessages {
    /// 路由后的 user/assistant 消息序列(role=system 已被剔除或转 user)。
    pub messages: Vec<Message>,
    /// 提升到 top-level 的稳定 system 文本(追加到 req.system 之后再折叠 history[0])。
    pub promoted_system: Vec<String>,
}

/// 处理 messages 数组里的 `role=="system"` 消息(代理链中段注入),三级分流:
/// - **StablePrefix** → 提升进 `promoted_system`(后续折叠进 history[0] 稳定区)
/// - **DynamicNoise** → 丢弃
/// - **interrupted-user** → 提取正文转 `role="user"` 按序保留
/// - **其余未知** → 包 `<system_context>...</system_context>` 转 `role="user"` 按序保留(保位置语义)
/// - 空 system → 丢弃
///
/// user/assistant 消息原样保留。**无 system-role 消息时返回 None**(常见情形零拷贝,
/// 调用方继续用原 borrowed slice)。
/// 🟢 借鉴 static_flow `normalize.rs:212-356`,但只做 system-role 这一层(我方 top-level system
/// 折叠逻辑不变,不引入 static_flow 的 developer-role / 整条 normalize 管线)。
pub(super) fn route_system_role_messages(messages: &[Message]) -> Option<RoutedMessages> {
    if !messages.iter().any(|m| m.role == "system") {
        return None;
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut promoted = Vec::new();
    for msg in messages {
        if msg.role != "system" {
            out.push(msg.clone());
            continue;
        }
        let text = strip_rolling_fingerprints(&message_text(&msg.content));
        if text.trim().is_empty() {
            continue; // 空 system 丢弃
        }
        if is_stable_system_prefix(&text) {
            promoted.push(text);
        } else if let Some(payload) = interrupted_user_payload(&text) {
            out.push(Message {
                role: "user".to_string(),
                content: serde_json::Value::String(payload),
            });
        } else if is_dynamic_system_noise(&text) {
            // 丢弃
        } else {
            out.push(Message {
                role: "user".to_string(),
                content: serde_json::Value::String(format!(
                    "<system_context>\n{text}\n</system_context>"
                )),
            });
        }
    }

    Some(RoutedMessages {
        messages: out,
        promoted_system: promoted,
    })
}
