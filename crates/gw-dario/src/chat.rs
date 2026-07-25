use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{
    AccountQuota, CallCtx, ChatRequest, ChatStream, ChatUsage, QuotaWindow, SseEvent, StreamItem,
};
use crate::DarioConfig;

// ── 5h/7d 配额快照(从 Anthropic 经 sidecar 透传的响应头捕获)─────────────────────

/// 每账号最近一次的 Anthropic 限额利用率快照。`util5h`/`util7d` 是**分数**(0–1,原始头值),
/// 转 `AccountQuota` 时 ×100 变百分比;**`None` = 本窗口未知**(该响应没带这个头),与"0%"区分
/// (避免 5h-only 响应把未知的 7d 显示成 0%,对抗审查 #2)。OAuth/Max 没有只读用量接口,
/// 只能从真实聊天流量的响应头(`anthropic-ratelimit-unified-5h/7d-utilization`,由 dario sidecar
/// 原样转发)捕获。
#[derive(Debug, Clone, Default)]
pub(crate) struct DarioRateLimit {
    /// 5 小时滚动窗口利用率(分数 0–1);`None` = 本窗口未知。
    pub util5h: Option<f64>,
    /// 7 天滚动窗口利用率(分数 0–1);`None` = 本窗口未知。
    pub util7d: Option<f64>,
    /// 窗口重置 unix 秒(Anthropic 给统一的 reset,各窗口共用)。
    pub reset: Option<i64>,
    /// 限额状态串(`anthropic-ratelimit-unified-status`,如 allowed/rejected),作标签展示。
    pub status: String,
}

/// 每账号快照表:account_id → 最近一次利用率。chat 写(merge)、account_quota 读。
/// ⚠️ 进程生命周期缓存,按 account_id 键;账号删除/改名后旧键不主动清理(对抗审查 #4:
/// 受 admin 改动量而非请求量约束,数量级=账号数,接受不剪枝)。
pub(crate) type RateLimitStore = Arc<Mutex<HashMap<String, DarioRateLimit>>>;

impl DarioRateLimit {
    /// 用一份新解析的快照**就地合并**:只覆盖本次响应真带到的字段,缺失字段保留旧值
    /// (5h-only 响应不会把已知的 7d 抹掉,对抗审查 #2)。
    pub(crate) fn merge_present(&mut self, fresh: &DarioRateLimit) {
        if fresh.util5h.is_some() {
            self.util5h = fresh.util5h;
        }
        if fresh.util7d.is_some() {
            self.util7d = fresh.util7d;
        }
        if fresh.reset.is_some() {
            self.reset = fresh.reset;
        }
        if !fresh.status.is_empty() {
            self.status = fresh.status.clone();
        }
    }

    /// 转成 admin 面板用的 `AccountQuota`:**只发已知窗口**(未知窗口不渲染,不伪造 0%)。
    /// 两窗口都未知 → `None`(面板显示「—」)。
    pub(crate) fn to_quota(&self) -> Option<AccountQuota> {
        let mut windows = Vec::new();
        if let Some(u) = self.util5h {
            windows.push(QuotaWindow { label: "5h".into(), percent_used: u * 100.0, reset_at: self.reset });
        }
        if let Some(u) = self.util7d {
            windows.push(QuotaWindow { label: "7d".into(), percent_used: u * 100.0, reset_at: self.reset });
        }
        if windows.is_empty() {
            return None;
        }
        let label = if self.status.is_empty() { None } else { Some(self.status.clone()) };
        Some(AccountQuota::from_windows(windows, label))
    }
}

/// 从 sidecar 透传回来的响应头解析 5h/7d 利用率。无 5h **且** 无 7d 头(如非 messages 响应或
/// 上游未带限额头)→ `None`,调用方**保留旧快照**(不拿空响应抹掉已知用量)。
pub(crate) fn parse_dario_ratelimit(headers: &reqwest::header::HeaderMap) -> Option<DarioRateLimit> {
    let get_f = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
    };
    let util5h = get_f("anthropic-ratelimit-unified-5h-utilization");
    let util7d = get_f("anthropic-ratelimit-unified-7d-utilization");
    if util5h.is_none() && util7d.is_none() {
        return None;
    }
    let reset = headers
        .get("anthropic-ratelimit-unified-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok());
    let status = headers
        .get("anthropic-ratelimit-unified-status")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some(DarioRateLimit { util5h, util7d, reset, status })
}

// ── Task 3.1: Usage accumulator ──────────────────────────────────────────────

/// SSE usage accumulator.  Observes `message_start` + `message_delta` events
/// and exposes the final `ChatUsage` for billing.
/// Implements `Default` + `std::mem::take` so mid-stream errors can still
/// emit a partial usage snapshot without cloning.
#[derive(Default)]
pub(crate) struct UsageAcc {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

impl UsageAcc {
    /// Absorb a parsed SSE event.  Idempotent across non-usage events.
    pub(crate) fn observe(&mut self, event: &str, data: &serde_json::Value) {
        let u = match event {
            "message_start" => data.get("message").and_then(|m| m.get("usage")),
            "message_delta" => data.get("usage"),
            _ => None,
        };
        let Some(u) = u else { return };
        if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) {
            self.input = v;
        }
        if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
            self.output = v;
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
            self.cache_read = v;
        }
        if let Some(v) = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
            self.cache_creation = v;
        }
    }

    /// Consume the accumulator and produce a `ChatUsage`.
    /// `real_cache_read_tokens` and `metering_credit` are 0 for non-Kiro providers.
    pub(crate) fn into_usage(self) -> ChatUsage {
        ChatUsage {
            input_tokens: self.input,
            output_tokens: self.output,
            cache_read_tokens: self.cache_read,
            cache_creation_tokens: self.cache_creation,
            real_cache_read_tokens: 0,
            metering_credit: 0.0,
        }
    }
}

// ── Task 3.1: SSE frame splitter ─────────────────────────────────────────────

/// Split `buf` (bytes) into complete SSE frames separated by `\n\n` and return
/// `(frames, leftover_bytes)`.  Each frame is `(event_name, data_string)`.
///
/// Operating on bytes (not lossy-decoded strings) is **critical** for
/// multi-byte UTF-8 characters (CJK / emoji) that span chunk boundaries.
/// `String::from_utf8_lossy` would silently replace the straddle bytes with
/// U+FFFD replacement characters, corrupting the content forwarded to clients.
/// Here we accumulate raw bytes; only after finding a complete `\n\n`-terminated
/// frame do we decode to `&str` via `std::str::from_utf8`.  A valid, complete
/// SSE frame must be well-formed UTF-8.
///
/// Partial frames at the tail are returned as `Vec<u8>` and must be
/// prepended to the next chunk.
pub(crate) fn drain_sse_frames_bytes(buf: &[u8]) -> (Vec<(String, String)>, Vec<u8>) {
    let mut frames = Vec::new();
    let mut start = 0usize;

    while start < buf.len() {
        // Find the `\n\n` (0x0A 0x0A) separator that terminates an SSE message.
        let search = &buf[start..];
        let rel = search
            .windows(2)
            .position(|w| w[0] == b'\n' && w[1] == b'\n');

        let Some(rel_pos) = rel else {
            break; // No complete frame yet; leave remaining bytes as leftover.
        };

        let frame_end = start + rel_pos;
        let frame_bytes = &buf[start..frame_end];

        match std::str::from_utf8(frame_bytes) {
            Ok(frame_str) => {
                let (mut event, mut data) = (String::new(), String::new());
                for line in frame_str.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(v.trim());
                    }
                }
                if !event.is_empty() || !data.is_empty() {
                    frames.push((event, data));
                }
            }
            Err(e) => {
                tracing::warn!(
                    valid_up_to = e.valid_up_to(),
                    "dario SSE frame contains invalid UTF-8, skipping frame"
                );
            }
        }

        start = frame_end + 2; // skip the "\n\n"
    }

    let leftover = buf[start..].to_vec();
    (frames, leftover)
}

/// Legacy string-based frame splitter (kept for unit-test backward compat;
/// streaming code uses `drain_sse_frames_bytes` instead).
#[allow(dead_code)]
pub(crate) fn drain_sse_frames(buf: &str) -> (Vec<(String, String)>, String) {
    let mut frames = Vec::new();
    let mut rest = buf;
    while let Some(idx) = rest.find("\n\n") {
        let (frame, after) = rest.split_at(idx);
        let (mut event, mut data) = (String::new(), String::new());
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.trim());
            }
        }
        if !event.is_empty() || !data.is_empty() {
            frames.push((event, data));
        }
        rest = &after[2..]; // skip the "\n\n"
    }
    (frames, rest.to_string())
}

// ── 转发 body 归一 ───────────────────────────────────────────────────────────

/// 转发给 dario sidecar 前对客户端 body 的归一:
/// ① 强制 `stream=true`(dario 恒以 SSE 返回,与下游是否要流无关);
/// ② 强制 `service_tier="standard_only"` —— **屏蔽 Claude Code `/fast` 优先级档**。
///    `/fast` = 客户端发 `service_tier:"auto"`(opt-in priority,更快但**额外计费**,
///    Max 订阅也照扣)。dario 是唯一 verbatim 透传 body 的路径,不拦就会原样上真 Anthropic
///    在用户 Max 号上多花钱。`"standard_only"` 是官方两个合法请求值之一(另一个就是 "auto"),
///    强制设它=只能走普通档,不会 400。
///    ⚠️ 这是**速度/计费档**,与 `output_config.effort`(思考强度)、`thinking` 完全无关,
///    本函数绝不触碰它们——thinking 强度由客户端原样保留。
fn prepare_forwarded_body(mut body: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::Object(ref mut m) = body {
        m.insert("stream".into(), serde_json::Value::Bool(true));
        m.insert("service_tier".into(), serde_json::Value::String("standard_only".into()));
    }
    body
}

// ── 会话亲和键 ───────────────────────────────────────────────────────────────

/// 会话亲和键入口(worker 据此把同会话钉到组内同账号,对齐 Anthropic prompt cache 的会话粒度)。
///
/// 优先级:
/// ① `metadata.user_id` 里的 **session_id(UUID)** —— Claude Code 原生会话 ID,跨轮恒定、
///    与消息内容无关,且在**请求 body 内**(不受 caio router 第一跳丢客户端 header 影响)。
///    这是最稳的键:同一会话的每一轮都拿到同一个 key → 永远钉同号 → 缓存最大化。
/// ② 回退:首条用户消息文本哈希([`affinity_from_body`])。无 metadata 的客户端(裸 API 调用)
///    仍能在「同会话首条文本稳定」下粘同号;弱点是不同会话若首条文本恰好相同会撞同号(可接受,
///    只影响负载分布,不影响正确性)。
///
/// 两路都拿不到 → `None`(调用方退化为无亲和的 LRU 选号)。
pub(crate) fn affinity_key_from_body(body: &serde_json::Value) -> Option<String> {
    if let Some(sid) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
        .and_then(extract_session_id)
    {
        return Some(format!("dario-sid-{sid}"));
    }
    affinity_from_body(body)
}

/// 从 Claude Code 的 `metadata.user_id` 提取会话 UUID。
///
/// CC 的 user_id 可能是 JSON(`{"session_id":"<uuid>", ...}`)或形如
/// `...session_<uuid>...` 的字符串。两种都覆盖;非合法 UUID 返回 `None`。
/// (自包含实现,不依赖 gw-kiro;与其 `converter::session::extract_session_id` 同口径。)
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(sid) {
                return Some(sid.to_string());
            }
        }
    }
    // 回退:查找 "session_" 之后的 36 字符 UUID
    if let Some(pos) = user_id.find("session_") {
        let rest = &user_id[pos + "session_".len()..];
        if rest.len() >= 36 {
            let candidate = &rest[..36];
            if is_valid_uuid(candidate) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// 粗校验 UUID:36 字符且含 4 个连字符(与 gw-kiro 一致,够区分会话键即可)。
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 基于**首条用户消息文本**的稳定哈希键(亲和回退路径)。
/// 无用户消息或文本为空 → `None`。`DefaultHasher` 在单进程内确定性,足够 per-worker 会话钉号。
pub(crate) fn affinity_from_body(body: &serde_json::Value) -> Option<String> {
    let msgs = body.get("messages")?.as_array()?;
    let first_user_text = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| match m.get("content") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|b| b.get("text").and_then(|t| t.as_str()))
                .map(str::to_string),
            _ => None,
        })?;
    if first_user_text.trim().is_empty() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    first_user_text.hash(&mut h);
    Some(format!("dario-{:016x}", h.finish()))
}

// ── Task 3.2: chat status classifier ─────────────────────────────────────────

/// Map an HTTP error status code (+ response body text) to an `UpstreamErrorKind`.
///
/// Key change vs the original inline `match`: a bare **403** is now `BadRequest`
/// rather than `TokenInvalid`.  A 403 that does *not* mention "suspend" typically
/// means model-not-available, wrong region, or permission denied — errors that are
/// the same on every account, so permanently banning the token is wrong.
/// Only a `suspend`-bearing 403 indicates an account-level block.
pub(crate) fn classify_chat_status(code: u16, body: &str) -> UpstreamErrorKind {
    match code {
        400 => UpstreamErrorKind::BadRequest,
        401 => UpstreamErrorKind::TokenInvalid,
        402 => UpstreamErrorKind::QuotaExhausted, // Anthropic monthly quota (aligns gw-kiro)
        403 if body.to_lowercase().contains("suspend") => UpstreamErrorKind::TemporarilyBlocked,
        403 => UpstreamErrorKind::BadRequest, // model/region/permission; not a dead token
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    }
}

// ── Task 3.2: Forward to dario sidecar ───────────────────────────────────────

/// Forward a chat request to the local dario sidecar and stream back
/// `StreamItem` events.
///
/// # Key invariants
/// - **Force `stream: true`**: Anthropic returns a single JSON blob for
///   `stream: false`; `drain_sse_frames` cannot split it → zero events, zero
///   usage.  caio's `collect_response` folds the SSE into a non-streaming
///   response for clients that requested `stream: false` (spec §6.3).
/// - **Emit Usage even on error**: a mid-stream network error must still yield
///   the usage observed so far so per-key billing is not truncated.
pub(crate) async fn chat_via_sidecar(
    cfg: &DarioConfig,
    client: &reqwest::Client,
    ratelimit: RateLimitStore,
    req: ChatRequest,
    ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    // Per-account upstream egress proxy (anti-ban: dario chat must exit from
    // the account's home IP). Forwarded to the sidecar as x-dario-upstream-proxy,
    // which the fork applies per-request (Bun fetch `proxy`). Only http(s) —
    // dario cannot do socks; reject loudly so a misconfig never silently exits
    // the wrong IP (refresh_auth uses the same proxy, see lib.rs::egress_client_for).
    // Normalize via the shared helper so chat & refresh agree byte-for-byte on
    // the proxy URL (same validation + canonical form → same egress, same cache
    // key). Invalid/socks → BadRequest (won't switch accounts), never silently
    // exit the wrong IP.
    let upstream_proxy: Option<String> = match ctx
        .account
        .extra_str("proxy")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(crate::normalize_dario_proxy(raw).map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::BadRequest,
                format!("dario account extra.proxy 非法: {e}"),
            )
        })?),
        None => None,
    };

    // Resolve per-account credentials.
    let access_token = ctx
        .account
        .extra_str("access_token")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "dario account missing access_token",
            )
        })?
        .to_string();
    let device_id = ctx
        .account
        .extra_str("device_id")
        .unwrap_or_default()
        .to_string();
    let account_uuid = ctx
        .account
        .extra_str("account_uuid")
        .unwrap_or_default()
        .to_string();
    let session_id = ctx.session_id.clone();
    let api_key = cfg.api_key.clone();

    // 归一转发给 sidecar 的 body(强制流式 + 屏蔽 fast 档)。见 prepare_forwarded_body。
    let body = prepare_forwarded_body(req.body);

    let url = format!("{}/v1/messages", cfg.sidecar_url.trim_end_matches('/'));
    let mut rb = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-api-key", &api_key)                   // dario ingress auth (bare key)
        .header("x-dario-upstream-token", &access_token); // Phase 1 patch consumes this
    // Fix4: only send x-session-id when non-empty — sending an empty string
    // would make dario treat all no-session requests as belonging to the same
    // session, defeating the purpose and possibly corrupting session state.
    if !session_id.is_empty() {
        rb = rb.header("x-session-id", &session_id); // dario reads x-session-id (proxy.ts:1619)
    }
    if !device_id.is_empty() {
        rb = rb.header("x-dario-device-id", &device_id);
    }
    if !account_uuid.is_empty() {
        rb = rb.header("x-dario-account-uuid", &account_uuid);
    }
    if let Some(p) = &upstream_proxy {
        rb = rb.header("x-dario-upstream-proxy", p); // dario fork applies per-request (proxy.ts)
    }

    let resp = rb
        .json(&body)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("dario sidecar connect failed: {e}")))?;

    let status = resp.status();

    // 抓取经 sidecar 透传回来的 Anthropic 5h/7d 利用率头,缓存供 account_quota 只读返回。
    // 成功与 429 响应都带这些头(429 尤其有价值:正好命中限额),故在成功/错误分流**之前**捕获。
    // 无相关头 → 保留旧快照(不拿空响应抹掉已知用量)。**就地 merge**(只覆盖本次带到的窗口,
    // 保留另一窗口的上次已知值)。poison 用 into_inner 恢复(与 account_quota 读路径一致,#3)。
    if let Some(fresh) = parse_dario_ratelimit(resp.headers()) {
        let mut m = ratelimit.lock().unwrap_or_else(|p| p.into_inner());
        m.entry(ctx.account.account_id.clone()).or_default().merge_present(&fresh);
    }

    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        // Fix3: use classify_chat_status — bare 403 is BadRequest (not TokenInvalid).
        let kind = classify_chat_status(code, &text);
        return Err(UpstreamError::new(
            kind,
            format!(
                "dario/anthropic {code}: {}",
                text.chars().take(500).collect::<String>()
            ),
        )
        .with_status(code));
    }

    // Stream response bytes, parse SSE frames on the fly.
    // Fix1: use a raw byte buffer so multi-byte UTF-8 characters (CJK / emoji)
    // that straddle chunk boundaries are never corrupted by lossy decoding.
    let mut byte_stream = resp.bytes_stream();
    let stream = async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut acc = UsageAcc::default();
        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    let (frames, rest) = drain_sse_frames_bytes(&buf);
                    buf = rest;
                    for (event, data_str) in frames {
                        // Skip empty data and OpenAI-style [DONE] sentinel silently.
                        if data_str.is_empty() || data_str == "[DONE]" {
                            continue;
                        }
                        // Fix1: skip (warn) on JSON parse failure instead of emitting
                        // a Null data frame, which would be forwarded to the client as
                        // a malformed SSE event.
                        match serde_json::from_str::<serde_json::Value>(&data_str) {
                            Ok(data) => {
                                acc.observe(&event, &data);
                                yield Ok(StreamItem::Sse(SseEvent::new(event, data)));
                            }
                            Err(_) => {
                                tracing::warn!(
                                    event = %event,
                                    data = %data_str.chars().take(200).collect::<String>(),
                                    "dario SSE frame JSON parse failed, skipping frame"
                                );
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    // Even on a mid-stream network error, emit the usage
                    // observed so far so per-key billing is not truncated.
                    yield Ok(StreamItem::Usage(std::mem::take(&mut acc).into_usage()));
                    yield Err(UpstreamError::network(format!("dario stream interrupted: {e}")));
                    return;
                }
                None => break,
            }
        }
        // Normal end: emit final usage.
        yield Ok(StreamItem::Usage(acc.into_usage()));
    };
    Ok(Box::pin(stream))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_start_and_delta() {
        let start = serde_json::json!({
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 100,
                    "cache_read_input_tokens": 40,
                    "cache_creation_input_tokens": 10
                }
            }
        });
        let delta = serde_json::json!({
            "type": "message_delta",
            "usage": { "output_tokens": 25 }
        });
        let mut acc = UsageAcc::default();
        acc.observe("message_start", &start);
        acc.observe("message_delta", &delta);
        let u = acc.into_usage();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_creation_tokens),
            (100, 25, 40, 10)
        );
        assert_eq!(u.real_cache_read_tokens, 0);
        assert_eq!(u.metering_credit, 0.0);
    }

    #[test]
    fn split_frames_and_keep_partial() {
        let (f, rest) = drain_sse_frames(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: par",
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, "message_start");
        assert_eq!(rest, "event: par");
    }

    // ── Fix1: byte-buffer SSE splitter tests ─────────────────────────────────

    #[test]
    fn bytes_split_basic_frame() {
        let (f, rest) = drain_sse_frames_bytes(
            b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: par",
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, "message_start");
        assert_eq!(rest, b"event: par");
    }

    /// Fix1 core: CJK / emoji split mid-character across two chunks must not
    /// produce U+FFFD replacement characters in the decoded data.
    #[test]
    fn bytes_buffer_preserves_multibyte_unicode_across_chunk_boundary() {
        // "你好世界" = [E4 BD A0] [E5 A5 BD] [E4 B8 96] [E7 95 8C] — 12 bytes
        let json_data = r#"{"text":"你好世界"}"#;
        let frame = format!("event: test\ndata: {}\n\n", json_data);
        let frame_bytes = frame.as_bytes();

        // Find the byte offset of "你" and split *one byte into* the first CJK char.
        let text_start = frame.find("你好世界").expect("CJK text must exist in frame");
        let split_point = text_start + 1; // one byte of "你" (0xE4) in chunk1

        let chunk1 = &frame_bytes[..split_point];
        let chunk2 = &frame_bytes[split_point..];

        // Simulate the streaming loop: accumulate bytes, then split frames.
        let mut buf = Vec::new();
        buf.extend_from_slice(chunk1);
        let (frames, leftover) = drain_sse_frames_bytes(&buf);
        assert!(frames.is_empty(), "no complete frame from partial chunk");
        buf = leftover;

        buf.extend_from_slice(chunk2);
        let (frames, leftover) = drain_sse_frames_bytes(&buf);
        assert_eq!(frames.len(), 1, "exactly one frame after both chunks");
        assert!(leftover.is_empty(), "no leftover after complete frame");

        let (event, data) = &frames[0];
        assert_eq!(event, "test");
        assert!(
            !data.contains('\u{FFFD}'),
            "replacement character found — unicode was corrupted: {}",
            data
        );
        let parsed: serde_json::Value = serde_json::from_str(data).expect("must be valid JSON");
        assert_eq!(parsed["text"], "你好世界");
    }

    #[test]
    fn bytes_single_chunk_multiple_frames() {
        // Two complete frames in one chunk — both should be returned.
        let input = b"event: a\ndata: {\"n\":1}\n\nevent: b\ndata: {\"n\":2}\n\n";
        let (frames, leftover) = drain_sse_frames_bytes(input);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, "a");
        assert_eq!(frames[1].0, "b");
        assert!(leftover.is_empty());
    }

    // ── Fix3: classify_chat_status tests ─────────────────────────────────────

    #[test]
    fn classify_403_suspend_is_temporarily_blocked() {
        assert_eq!(
            classify_chat_status(403, "account suspended by upstream"),
            UpstreamErrorKind::TemporarilyBlocked
        );
    }

    #[test]
    fn classify_403_non_suspend_is_bad_request() {
        // 403 without "suspend" → BadRequest (model/region/permission, not a dead token).
        assert_eq!(
            classify_chat_status(403, "Forbidden: model not available in your region"),
            UpstreamErrorKind::BadRequest
        );
        assert_eq!(
            classify_chat_status(403, "permission denied"),
            UpstreamErrorKind::BadRequest
        );
    }

    #[test]
    fn classify_other_status_codes() {
        assert_eq!(classify_chat_status(400, "bad input"), UpstreamErrorKind::BadRequest);
        assert_eq!(classify_chat_status(401, "unauthorized"), UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_chat_status(402, "quota"), UpstreamErrorKind::QuotaExhausted);
        assert_eq!(classify_chat_status(429, "rate limited"), UpstreamErrorKind::RateLimited);
        assert_eq!(classify_chat_status(503, "server error"), UpstreamErrorKind::ServerError);
    }

    #[test]
    fn affinity_hashes_first_user_text() {
        let b = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}]
        });
        let k = affinity_from_body(&b);
        assert!(k.is_some());
        // Same first-user text → same key (process-stable)
        assert_eq!(k, affinity_from_body(&b));
    }

    #[test]
    fn affinity_none_without_messages() {
        assert_eq!(affinity_from_body(&serde_json::json!({})), None);
    }

    #[test]
    fn affinity_key_prefers_session_id_from_metadata() {
        let sid = "11111111-2222-4333-8444-555555555555";
        let b = serde_json::json!({
            "metadata": {"user_id": format!("user_abc_account__session_{sid}")},
            "messages": [{"role": "user", "content": "hello"}]
        });
        // session_id 路径优先,且与首条文本无关
        assert_eq!(affinity_key_from_body(&b), Some(format!("dario-sid-{sid}")));
    }

    #[test]
    fn affinity_key_session_id_stable_across_turns_regardless_of_text() {
        let sid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let uid = serde_json::json!({"session_id": sid}).to_string();
        let turn1 = serde_json::json!({
            "metadata": {"user_id": uid},
            "messages": [{"role": "user", "content": "first question"}]
        });
        let turn2 = serde_json::json!({
            "metadata": {"user_id": uid},
            "messages": [
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "totally different follow-up"}
            ]
        });
        // 同会话不同轮、首条文本即便变化,session_id 键恒定 → 钉同号
        let k = affinity_key_from_body(&turn1);
        assert_eq!(k, Some(format!("dario-sid-{sid}")));
        assert_eq!(k, affinity_key_from_body(&turn2));
    }

    #[test]
    fn affinity_key_falls_back_to_text_hash_without_metadata() {
        let b = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}]
        });
        // 无 metadata → 回退首条文本哈希(与 affinity_from_body 同值)
        assert_eq!(affinity_key_from_body(&b), affinity_from_body(&b));
        assert!(affinity_key_from_body(&b).unwrap().starts_with("dario-"));
    }

    #[test]
    fn affinity_key_ignores_invalid_session_id() {
        // user_id 里没有合法 UUID → 退回文本哈希,不 panic
        let b = serde_json::json!({
            "metadata": {"user_id": "user_no_session_here"},
            "messages": [{"role": "user", "content": "q"}]
        });
        assert_eq!(affinity_key_from_body(&b), affinity_from_body(&b));
    }

    #[test]
    fn extract_session_id_handles_json_and_string_forms() {
        let sid = "12345678-1234-4234-8234-123456789abc";
        assert_eq!(
            extract_session_id(&serde_json::json!({"session_id": sid}).to_string()),
            Some(sid.to_string())
        );
        assert_eq!(
            extract_session_id(&format!("anything_session_{sid}_suffix")),
            Some(sid.to_string())
        );
        assert_eq!(extract_session_id("no uuid at all"), None);
        assert_eq!(extract_session_id("session_not-a-valid-uuid"), None);
    }

    #[test]
    fn forwarded_body_forces_stream_and_blocks_fast_keeps_thinking() {
        // 客户端开了 /fast(service_tier:auto)+ 拉满思考强度 + 结构化输出。
        let body = prepare_forwarded_body(serde_json::json!({
            "model": "claude-opus-4-8",
            "messages": [],
            "stream": false,
            "service_tier": "auto",                       // fast 档
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "output_config": {"effort": "max", "format": {"type": "json_schema"}}
        }));
        // 强制流式。
        assert_eq!(body["stream"], serde_json::json!(true));
        // fast 被降级为普通档(覆盖客户端的 auto)。
        assert_eq!(body["service_tier"], serde_json::json!("standard_only"), "必须屏蔽 fast 档");
        // 思考强度 / thinking / 结构化输出 一律原样保留(只动 service_tier)。
        assert_eq!(body["output_config"]["effort"], serde_json::json!("max"), "思考强度不得被动");
        assert_eq!(body["thinking"]["type"], serde_json::json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], serde_json::json!(4096));
        assert_eq!(body["output_config"]["format"]["type"], serde_json::json!("json_schema"));
    }

    #[test]
    fn forwarded_body_sets_standard_tier_even_when_client_omits_it() {
        // 客户端没传 service_tier 时也强制 standard_only(防默认 auto 走优先级)。
        let body = prepare_forwarded_body(serde_json::json!({
            "model": "claude-opus-4-8", "messages": []
        }));
        assert_eq!(body["service_tier"], serde_json::json!("standard_only"));
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    // ── 5h/7d 配额捕获 ────────────────────────────────────────────────────────

    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn parse_ratelimit_reads_5h_7d_reset_status_and_converts_to_percent() {
        let mut h = HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-5h-utilization", HeaderValue::from_static("0.37"));
        h.insert("anthropic-ratelimit-unified-7d-utilization", HeaderValue::from_static("0.12"));
        h.insert("anthropic-ratelimit-unified-reset", HeaderValue::from_static("1750000000"));
        h.insert("anthropic-ratelimit-unified-status", HeaderValue::from_static("allowed"));
        let s = parse_dario_ratelimit(&h).expect("应解析出快照");
        assert_eq!(s.util5h, Some(0.37));
        assert_eq!(s.util7d, Some(0.12));
        assert_eq!(s.reset, Some(1_750_000_000));
        assert_eq!(s.status, "allowed");
        // 分数 → 百分比窗口
        let q = s.to_quota().expect("两窗口齐全应有 quota");
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[0].label, "5h");
        assert!((q.windows[0].percent_used - 37.0).abs() < 1e-6);
        assert_eq!(q.windows[0].reset_at, Some(1_750_000_000));
        assert_eq!(q.windows[1].label, "7d");
        assert!((q.windows[1].percent_used - 12.0).abs() < 1e-6);
        assert_eq!(q.currency.as_deref(), Some("allowed"));
        // 积分字段保持 0(dario 无积分概念)
        assert_eq!(q.limit, 0.0);
    }

    #[test]
    fn parse_ratelimit_none_when_no_utilization_headers() {
        // 完全无头 → None。
        assert!(parse_dario_ratelimit(&HeaderMap::new()).is_none());
        // 只有 status、无 5h/7d → 仍 None(避免拿无用量的响应抹掉旧快照)。
        let mut h = HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-status", HeaderValue::from_static("allowed"));
        assert!(parse_dario_ratelimit(&h).is_none());
    }

    #[test]
    fn parse_ratelimit_partial_5h_only_emits_only_5h_window() {
        let mut h = HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-5h-utilization", HeaderValue::from_static("1.05"));
        let s = parse_dario_ratelimit(&h).expect("有 5h 即应解析");
        assert_eq!(s.util5h, Some(1.05));
        assert_eq!(s.util7d, None); // 未知,不是 0
        assert_eq!(s.reset, None);
        let q = s.to_quota().expect("有 5h 即应有 quota");
        // 只渲染已知窗口:不伪造 7d 0%(对抗审查 #2)。
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].label, "5h");
        // 超额(>100%)如实体现,不 clamp。
        assert!((q.windows[0].percent_used - 105.0).abs() < 1e-6);
        assert!(q.currency.is_none()); // status 缺失 → label None。
    }

    #[test]
    fn merge_present_keeps_last_known_other_window() {
        // 先有 5h+7d,再来一份 5h-only:7d 应保留上次已知值,而非被抹成未知/0。
        let mut acc = DarioRateLimit {
            util5h: Some(0.30),
            util7d: Some(0.10),
            reset: Some(100),
            status: "allowed".into(),
        };
        let fresh_5h_only = DarioRateLimit {
            util5h: Some(0.55),
            util7d: None,
            reset: Some(200),
            status: String::new(),
        };
        acc.merge_present(&fresh_5h_only);
        assert_eq!(acc.util5h, Some(0.55), "5h 应被更新");
        assert_eq!(acc.util7d, Some(0.10), "7d 应保留上次已知值");
        assert_eq!(acc.reset, Some(200), "reset 带到则更新");
        assert_eq!(acc.status, "allowed", "status 缺失则保留旧值");
        let q = acc.to_quota().expect("merge 后两窗口齐全");
        assert_eq!(q.windows.len(), 2);
    }
}
