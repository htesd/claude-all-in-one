use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, ChatUsage};
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

// ── Task 3.2 stub (replaced below after 3.1 commit) ──────────────────────────

pub(crate) async fn chat_via_sidecar(
    _cfg: &DarioConfig,
    _client: &reqwest::Client,
    _req: ChatRequest,
    _ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    Err(UpstreamError::new(UpstreamErrorKind::Other, "not implemented"))
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
}
