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

/// Split `buf` into complete SSE frames (separated by `\n\n`) and return
/// `(frames, leftover)`.  Each frame is `(event_name, data_string)`.
/// Partial frames at the tail are returned as the leftover `String` and must
/// be prepended to the next chunk.
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
        .header("x-api-key", &api_key)              // dario ingress auth (bare key, highest priority)
        .header("x-dario-upstream-token", &access_token) // Phase 1 patch consumes this
        .header("x-session-id", &session_id);        // dario reads x-session-id (proxy.ts:1619)
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
        let kind = match code {
            400 => UpstreamErrorKind::BadRequest,
            401 => UpstreamErrorKind::TokenInvalid,
            402 => UpstreamErrorKind::QuotaExhausted, // Anthropic monthly quota exhausted (aligns gw-kiro error_map)
            403 if text.to_lowercase().contains("suspend") => {
                UpstreamErrorKind::TemporarilyBlocked
            }
            403 => UpstreamErrorKind::TokenInvalid,
            429 => UpstreamErrorKind::RateLimited,
            500..=599 => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        };
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
    let mut byte_stream = resp.bytes_stream();
    let stream = async_stream::stream! {
        let mut buf = String::new();
        let mut acc = UsageAcc::default();
        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    let (frames, rest) = drain_sse_frames(&buf);
                    buf = rest;
                    for (event, data_str) in frames {
                        let data: serde_json::Value =
                            serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
                        acc.observe(&event, &data);
                        yield Ok(StreamItem::Sse(SseEvent::new(event, data)));
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
