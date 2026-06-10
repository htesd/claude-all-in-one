//! 非流式折叠 —— 把一串 Anthropic SSE 事件折叠成单个 Messages 响应 JSON。
//!
//! ## 为什么在 gw-core(契约层,写一次)
//!
//! Provider trait 约定:**provider 一律产流**(只写流式一遍),流式 vs 非流式是
//! gw-app 的关注点。客户端 `stream:false` 时,gw-app 把 provider 吐的 Anthropic SSE
//! 事件折叠回单个 Messages 响应。这段折叠**与 provider 无关**(只依赖标准 Anthropic
//! 线缆协议),因此放 gw-core 写一次,所有 provider(kiro / claude-subprocess / dario)
//! 共享——这正是"写一个 provider 就能插进来"框架红利的一部分。
//!
//! ## 折叠规则(标准 Anthropic Messages 流)
//!
//! - `message_start` → 取 `message` 骨架(id/type/role/model/usage,content 置空);
//! - `content_block_start` → 按 index 起一个块(text/tool_use/thinking 骨架);
//! - `content_block_delta` → text_delta 追加 text;input_json_delta 累积 tool 入参 JSON;
//!   thinking_delta 追加 thinking;signature_delta 追加 signature;
//! - `content_block_stop` → tool_use 块把累积的 partial_json 解析成 `input` 对象;
//! - `message_delta` → 合并 stop_reason/stop_sequence + 累加 usage(output_tokens 等);
//! - `message_stop` → 收尾,content 按 index 顺序装配进 message。
//!
//! 遇到 `error` 事件返回 `Err(那条 error 的 data)`,调用方据此回非流式错误响应。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::provider::SseEvent;

/// 把有序的 Anthropic SSE 事件折叠成单个非流式 Messages 响应 JSON。
///
/// `Ok(message)` = 完整 assistant 消息对象;`Err(error_data)` = 流中出现 `error` 事件
/// (把该错误负载原样上交,调用方回非流式错误响应)。
pub fn fold_sse_to_message(events: &[SseEvent]) -> Result<Value, Value> {
    let mut message: Option<Value> = None;
    // index → 块骨架(来自 content_block_start)。
    let mut blocks: BTreeMap<usize, Value> = BTreeMap::new();
    // 各类增量按 index 累积进独立 String 缓冲(push_str 摊还 O(1),整体 O(L);
    // 不在 JSON 块里反复重拷整串——审查 #3 的 O(n²) 修正)。
    let mut text_buf: BTreeMap<usize, String> = BTreeMap::new();
    let mut thinking_buf: BTreeMap<usize, String> = BTreeMap::new();
    let mut sig_buf: BTreeMap<usize, String> = BTreeMap::new();
    let mut tool_buf: BTreeMap<usize, String> = BTreeMap::new();
    // delta 指向一个从未 content_block_start 的 index = 上游协议违例。框架应失败要响,
    // 而非静默丢内容回个看似成功的 200(审查 #2)。
    let mut malformed = false;

    for ev in events {
        match ev.event.as_str() {
            "error" => return Err(ev.data.clone()),
            "message_start" => {
                if let Some(m) = ev.data.get("message") {
                    message = Some(m.clone()); // content 在末尾按块装配覆盖。
                }
            }
            "content_block_start" => {
                let Some(idx) = ev.data.get("index").and_then(Value::as_u64) else {
                    continue;
                };
                let block = ev
                    .data
                    .get("content_block")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                blocks.insert(idx as usize, block);
            }
            "content_block_delta" => {
                let Some(idx) = ev.data.get("index").and_then(Value::as_u64) else {
                    continue;
                };
                let idx = idx as usize;
                let Some(delta) = ev.data.get("delta") else {
                    continue;
                };
                if !blocks.contains_key(&idx) {
                    malformed = true; // 孤儿 delta:无对应已起始块。
                    continue;
                }
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            text_buf.entry(idx).or_default().push_str(t);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(pj) = delta.get("partial_json").and_then(Value::as_str) {
                            tool_buf.entry(idx).or_default().push_str(pj);
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                            thinking_buf.entry(idx).or_default().push_str(t);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(s) = delta.get("signature").and_then(Value::as_str) {
                            sig_buf.entry(idx).or_default().push_str(s);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let Some(msg) = message.as_mut().and_then(Value::as_object_mut) else {
                    continue;
                };
                if let Some(delta) = ev.data.get("delta").and_then(Value::as_object) {
                    if let Some(sr) = delta.get("stop_reason") {
                        msg.insert("stop_reason".into(), sr.clone());
                    }
                    if let Some(ss) = delta.get("stop_sequence") {
                        msg.insert("stop_sequence".into(), ss.clone());
                    }
                }
                // usage:累加 message_delta 里的(权威 output_tokens / cache 等)。
                if let Some(u) = ev.data.get("usage").and_then(Value::as_object) {
                    if let Some(usage) = msg
                        .entry("usage")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                    {
                        for (k, v) in u {
                            usage.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            _ => {} // content_block_stop / message_stop / ping:无需累积(末尾统一装配)。
        }
    }

    if malformed {
        return Err(json!({"type":"error","error":{"type":"api_error",
            "message":"上游流出现未起始内容块的 delta(协议违例),无法折叠非流式响应"}}));
    }
    let mut message = message.ok_or_else(
        || json!({"type":"error","error":{"type":"api_error","message":"上游流缺少 message_start,无法折叠非流式响应"}}),
    )?;

    // 装配:把累积缓冲写回各块字段(无 delta 的字段保持 content_block_start 的骨架值)。
    let content: Vec<Value> = blocks
        .into_iter()
        .map(|(idx, mut block)| {
            if let Some(obj) = block.as_object_mut() {
                if let Some(t) = text_buf.remove(&idx) {
                    obj.insert("text".into(), Value::String(t));
                }
                if let Some(t) = thinking_buf.remove(&idx) {
                    obj.insert("thinking".into(), Value::String(t));
                }
                if let Some(s) = sig_buf.remove(&idx) {
                    obj.insert("signature".into(), Value::String(s));
                }
                if let Some(buf) = tool_buf.remove(&idx) {
                    // 空缓冲 → 保持骨架 input:{};非空但解析失败 → 同样保持(宽容,审查可接受)。
                    if !buf.is_empty() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&buf) {
                            obj.insert("input".into(), parsed);
                        }
                    }
                }
            }
            block
        })
        .collect();
    if let Some(obj) = message.as_object_mut() {
        obj.insert("content".into(), Value::Array(content));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: Value) -> SseEvent {
        SseEvent::new(event, data)
    }

    #[test]
    fn folds_simple_text_message() {
        let events = vec![
            ev(
                "message_start",
                json!({"type":"message_start","message":{
                    "id":"msg_1","type":"message","role":"assistant","model":"claude-x",
                    "content":[],"stop_reason":null,"stop_sequence":null,
                    "usage":{"input_tokens":10,"output_tokens":0}
                }}),
            ),
            ev(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"text","text":""}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"Hello "}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"world"}}),
            ),
            ev("content_block_stop", json!({"index":0})),
            ev(
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
            ),
            ev("message_stop", json!({"type":"message_stop"})),
        ];
        let msg = fold_sse_to_message(&events).unwrap();
        assert_eq!(msg["id"], "msg_1");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "Hello world");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["input_tokens"], 10);
        assert_eq!(msg["usage"]["output_tokens"], 5);
    }

    #[test]
    fn folds_tool_use_with_accumulated_json() {
        let events = vec![
            ev(
                "message_start",
                json!({"message":{"id":"m","type":"message","role":"assistant","model":"x","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
            ),
            ev(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"toolu_9","name":"get_weather","input":{}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"\"SF\"}"}}),
            ),
            ev("content_block_stop", json!({"index":0})),
            ev(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
            ),
            ev("message_stop", json!({})),
        ];
        let msg = fold_sse_to_message(&events).unwrap();
        assert_eq!(msg["content"][0]["type"], "tool_use");
        assert_eq!(msg["content"][0]["id"], "toolu_9");
        assert_eq!(msg["content"][0]["name"], "get_weather");
        assert_eq!(msg["content"][0]["input"]["city"], "SF");
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn folds_thinking_then_text_in_index_order() {
        let events = vec![
            ev(
                "message_start",
                json!({"message":{"id":"m","type":"message","role":"assistant","model":"x","content":[],"usage":{"input_tokens":1}}}),
            ),
            ev(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"signature_delta","signature":"sig123"}}),
            ),
            ev("content_block_stop", json!({"index":0})),
            ev(
                "content_block_start",
                json!({"index":1,"content_block":{"type":"text","text":""}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":1,"delta":{"type":"text_delta","text":"answer"}}),
            ),
            ev("content_block_stop", json!({"index":1})),
            ev("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
            ev("message_stop", json!({})),
        ];
        let msg = fold_sse_to_message(&events).unwrap();
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][0]["thinking"], "reason");
        assert_eq!(msg["content"][0]["signature"], "sig123");
        assert_eq!(msg["content"][1]["type"], "text");
        assert_eq!(msg["content"][1]["text"], "answer");
    }

    #[test]
    fn error_event_returns_err() {
        let events = vec![
            ev(
                "message_start",
                json!({"message":{"id":"m","content":[]}}),
            ),
            ev(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"boom"}}),
            ),
        ];
        let err = fold_sse_to_message(&events).unwrap_err();
        assert_eq!(err["error"]["type"], "overloaded_error");
    }

    #[test]
    fn missing_message_start_is_err() {
        let events = vec![ev("content_block_stop", json!({"index":0}))];
        let err = fold_sse_to_message(&events).unwrap_err();
        assert_eq!(err["type"], "error");
    }

    #[test]
    fn orphan_delta_without_block_start_is_err() {
        // 协议违例:delta 指向一个从未 content_block_start 的 index → 折叠应报错(框架失败要响)。
        let events = vec![
            ev(
                "message_start",
                json!({"message":{"id":"m","type":"message","role":"assistant","model":"x","content":[],"usage":{"input_tokens":1}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"orphan"}}),
            ),
            ev("message_stop", json!({})),
        ];
        let err = fold_sse_to_message(&events).unwrap_err();
        assert_eq!(err["type"], "error");
    }

    #[test]
    fn orphan_tool_json_without_block_start_is_err() {
        let events = vec![
            ev(
                "message_start",
                json!({"message":{"id":"m","type":"message","role":"assistant","model":"x","content":[],"usage":{"input_tokens":1}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            ),
        ];
        assert!(fold_sse_to_message(&events).is_err());
    }
}
