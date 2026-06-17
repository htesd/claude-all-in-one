use futures::StreamExt;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, ChatUsage, SseEvent, StreamItem};
use crate::DarioConfig;

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

// ── Task 3.1: Affinity key ───────────────────────────────────────────────────

/// Derive a stable session-affinity key from the first user message text.
/// Kept for future use; currently not called (affinity_key returns None in MVP).
#[allow(dead_code)]
/// Returns `None` if there is no user message or the text is blank.
///
/// Uses `DefaultHasher` which is deterministic within a single process run —
/// sufficient for per-worker in-process session pinning.
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
    req: ChatRequest,
    ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    // Fix5: warn if this account carries a per-account proxy that cannot be
    // honoured in MVP (all dario chat goes through a single sidecar_url).
    if ctx
        .account
        .extra_str("proxy")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        tracing::warn!(
            account = %ctx.account.account_id,
            "dario 账号设了 extra.proxy,但 MVP 所有 dario chat 走单一 sidecar_url——\
             该 per-account 代理被忽略,请确保 dario 组单出口且 sidecar upstream-proxy 一致"
        );
    }

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

    // Force upstream streaming so dario returns SSE regardless of what the
    // downstream client requested.  Move `req.body` out (req's other fields
    // are not used past this point), saving one allocation.
    let mut body = req.body;
    if let serde_json::Value::Object(ref mut m) = body {
        m.insert("stream".into(), serde_json::Value::Bool(true));
    }

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

    let resp = rb
        .json(&body)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("dario sidecar connect failed: {e}")))?;

    let status = resp.status();
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
    fn forces_stream_true_in_forwarded_body() {
        let req = gw_core::provider::ChatRequest::from_anthropic_body(
            serde_json::json!({"model":"claude-opus-4-8","messages":[]}),
        );
        assert!(!req.stream);
        let mut body = req.body.clone();
        if let serde_json::Value::Object(m) = &mut body {
            m.insert("stream".into(), serde_json::Value::Bool(true));
        }
        assert_eq!(body["stream"], serde_json::json!(true));
    }
}
