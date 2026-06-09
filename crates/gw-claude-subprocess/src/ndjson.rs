use gw_core::error::UpstreamError;
use gw_core::provider::{ChatUsage, SseEvent, StreamItem};

/// 解析一行 claude `stream-json` NDJSON,转换为内部 `StreamItem`。
///
/// P0 只做最小转换:
/// - `stream_event`：直接把内层 Anthropic SSE event 原样透传
/// - `result`：提取 usage,产出结构化 `Usage`
/// - `system` / `assistant`：当前忽略
pub fn parse_ndjson_line(line: &str) -> Result<Option<StreamItem>, UpstreamError> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|err| UpstreamError::bad_request(format!("invalid NDJSON line: {err}")))?;

    let kind = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| UpstreamError::bad_request("NDJSON line missing top-level type"))?;

    match kind {
        "stream_event" => parse_stream_event(&value),
        "result" => Ok(parse_result_usage(&value).map(StreamItem::Usage)),
        "system" | "assistant" => Ok(None),
        other => Err(UpstreamError::bad_request(format!(
            "unsupported NDJSON type: {other}"
        ))),
    }
}

fn parse_stream_event(value: &serde_json::Value) -> Result<Option<StreamItem>, UpstreamError> {
    let event = value
        .get("event")
        .and_then(|v| v.as_object())
        .ok_or_else(|| UpstreamError::bad_request("stream_event missing event object"))?;

    let event_type = event
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| UpstreamError::bad_request("stream_event.event missing type"))?;

    Ok(Some(StreamItem::Sse(SseEvent::new(
        event_type,
        serde_json::Value::Object(event.clone()),
    ))))
}

pub fn parse_result_usage(value: &serde_json::Value) -> Option<ChatUsage> {
    let usage = value.get("usage")?;
    Some(ChatUsage {
        input_tokens: usage_from_paths(usage, &[&["input_tokens"], &["input", "tokens"]]),
        output_tokens: usage_from_paths(usage, &[&["output_tokens"], &["output", "tokens"]]),
        cache_read_tokens: usage_from_paths(
            usage,
            &[&["cache_read_input_tokens"], &["cache_read", "input_tokens"]],
        ),
        cache_creation_tokens: usage_from_paths(
            usage,
            &[&["cache_creation_input_tokens"], &["cache_creation", "input_tokens"]],
        ),
    })
}

fn usage_from_paths(value: &serde_json::Value, paths: &[&[&str]]) -> u64 {
    paths.iter()
        .find_map(|path| lookup_u64(value, path))
        .unwrap_or_default()
}

fn lookup_u64(value: &serde_json::Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_event_into_sse() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        let item = parse_ndjson_line(line).unwrap().unwrap();
        match item {
            StreamItem::Sse(event) => {
                assert_eq!(event.event, "content_block_delta");
                assert_eq!(event.data["delta"]["text"], "Hello");
            }
            _ => panic!("expected sse event"),
        }
    }

    #[test]
    fn parses_result_into_usage() {
        let line = r#"{"type":"result","usage":{"input_tokens":12,"output_tokens":34,"cache_read_input_tokens":56,"cache_creation_input_tokens":78}}"#;
        let item = parse_ndjson_line(line).unwrap().unwrap();
        match item {
            StreamItem::Usage(usage) => {
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 34);
                assert_eq!(usage.cache_read_tokens, 56);
                assert_eq!(usage.cache_creation_tokens, 78);
            }
            _ => panic!("expected usage item"),
        }
    }

    #[test]
    fn ignores_system_lines() {
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert!(parse_ndjson_line(line).unwrap().is_none());
    }

    #[test]
    fn errors_when_home_type_missing() {
        let line = r#"{"event":{"type":"message_start"}}"#;
        let err = parse_ndjson_line(line).unwrap_err();
        assert_eq!(err.kind.to_string(), "bad_request");
    }
}
