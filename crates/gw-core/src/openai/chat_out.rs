//! Anthropic 事件流 → OpenAI **ChatCompletions** 线缆。
//!
//! 两条出口:
//! - 流式:[`ChatStreamOut`] 状态机,吃 [`SseEvent`] 吐 [`WireFrame`](一进可能零出或多出)。
//! - 非流式:[`fold_completion`] 吃已折叠的 Anthropic Messages 吐 `chat.completion`。
//!
//! 两条必须给出**一致**的内容与用量 —— 客户端切换 `stream` 只该改变分帧,不该改变答案。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::usage::UsageAccum;
use super::WireFrame;
use crate::provider::SseEvent;

/// ChatCompletions 流的终止哨兵。少了它,绝大多数 OpenAI 客户端会一直等到自己超时。
pub const DONE: &str = "[DONE]";

/// Anthropic `stop_reason` → OpenAI `finish_reason`。
pub fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        Some("refusal") => "content_filter",
        // end_turn / stop_sequence / pause_turn / 缺席 都归 stop。
        _ => "stop",
    }
}

/// 当前开着的 Anthropic 内容块在 OpenAI 侧对应什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text,
    Thinking,
    /// `tool_calls[]` 里的下标。
    ///
    /// ⚠️ **不能复用 Anthropic 的块下标**:那个下标是 text/thinking/tool_use
    /// **共用**的,一段思考 + 一次工具调用会得到 `index:1`,而 OpenAI 要求
    /// tool_calls 的 index 从 0 连续 —— 客户端按它拼参数,错位就是参数串台。
    Tool(u32),
}

/// ChatCompletions 流式状态机。
pub struct ChatStreamOut {
    id: String,
    created: i64,
    model: String,
    include_usage: bool,
    usage: UsageAccum,
    blocks: BTreeMap<usize, Block>,
    next_tool_index: u32,
    role_sent: bool,
    stop_reason: Option<String>,
    /// 已经发过终止序列(末帧 + 用量帧 + `[DONE]`),不得再发第二次。
    finished: bool,
}

impl ChatStreamOut {
    /// `model` 是客户端请求的对外模型名,`message_start` 若带了模型名会覆盖它。
    pub fn new(model: impl Into<String>, include_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: now_unix(),
            model: model.into(),
            include_usage,
            usage: UsageAccum::default(),
            blocks: BTreeMap::new(),
            next_tool_index: 0,
            role_sent: false,
            stop_reason: None,
            finished: false,
        }
    }

    /// 吃一条 Anthropic SSE 事件,吐 0..N 条 OpenAI 帧。
    pub fn push(&mut self, ev: &SseEvent) -> Vec<WireFrame> {
        // **终态之后一帧都不许再出。** 只让 `finish/complete/fail` 互相幂等是不够的:
        // 上游在 `error` 之后又冒出 `content_block_delta`(或 `message_stop` 之后又
        // 开一个新块)时,内容帧会跟在 `[DONE]` 后面 —— 严格客户端判协议损坏。
        if self.finished {
            return Vec::new();
        }
        match ev.event.as_str() {
            "message_start" => {
                if let Some(m) = ev.data.pointer("/message/model").and_then(Value::as_str) {
                    self.model = m.to_string();
                }
                if let Some(u) = ev.data.pointer("/message/usage") {
                    self.usage.merge(u);
                }
                // OpenAI 约定:首帧带 role,后续帧只带增量。
                self.role_sent = true;
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            "content_block_start" => self.on_block_start(&ev.data),
            "content_block_delta" => self.on_delta(&ev.data),
            "content_block_stop" => {
                if let Some(i) = index_of(&ev.data) {
                    self.blocks.remove(&i);
                }
                Vec::new()
            }
            "message_delta" => {
                if let Some(s) = ev.data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(s.to_string());
                }
                if let Some(u) = ev.data.get("usage") {
                    self.usage.merge(u);
                }
                Vec::new()
            }
            "message_stop" => self.complete(),
            "error" => self.fail(&ev.data),
            // ping 及其他:OpenAI 侧没有对应物,丢掉。保活由 `keepalive()` 单独负责。
            _ => Vec::new(),
        }
    }

    fn on_block_start(&mut self, data: &Value) -> Vec<WireFrame> {
        let Some(idx) = index_of(data) else {
            return Vec::new();
        };
        // 同一个块下标重复 start(上游协议违例):**忽略**。重新登记会再占一个
        // tool_calls 下标,客户端那边留下一个空的 index、参数全写到下一个上
        // (对抗评审 Skeptic#8)。
        if self.blocks.contains_key(&idx) {
            return Vec::new();
        }
        let cb = data.get("content_block");
        match cb.and_then(|b| b.get("type")).and_then(Value::as_str) {
            Some("text") => {
                self.blocks.insert(idx, Block::Text);
                Vec::new()
            }
            Some("thinking") => {
                self.blocks.insert(idx, Block::Thinking);
                Vec::new()
            }
            Some("tool_use") => {
                let tool_index = self.next_tool_index;
                self.next_tool_index += 1;
                self.blocks.insert(idx, Block::Tool(tool_index));
                let id = cb.and_then(|b| b.get("id")).and_then(Value::as_str).unwrap_or("");
                let name = cb.and_then(|b| b.get("name")).and_then(Value::as_str).unwrap_or("");
                vec![self.chunk(
                    json!({"tool_calls": [{
                        "index": tool_index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]}),
                    None,
                )]
            }
            // server_tool_use / web_search_tool_result 等:OpenAI 侧没有对应物。
            // 不登记 = 后续该块的增量也一并丢弃,不会串到别的块上。
            _ => Vec::new(),
        }
    }

    fn on_delta(&mut self, data: &Value) -> Vec<WireFrame> {
        let Some(idx) = index_of(data) else {
            return Vec::new();
        };
        let Some(&block) = self.blocks.get(&idx) else {
            return Vec::new();
        };
        let Some(delta) = data.get("delta") else {
            return Vec::new();
        };
        let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
        match (block, kind) {
            (Block::Text, "text_delta") => {
                let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                // 空串也照发:上游发的零增量是它自己的节奏信号,原样传递即可
                // (我方的保活帧走 [`Self::keepalive`],根本到不了这里)。
                vec![self.chunk(json!({ "content": t }), None)]
            }
            (Block::Thinking, "thinking_delta") => {
                let t = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                // `reasoning_content` 是 DeepSeek 系带起来的事实标准,NewAPI 认它。
                // OpenAI 官方 ChatCompletions 没有思考字段,不发就等于把思考整段吞掉。
                vec![self.chunk(json!({ "reasoning_content": t }), None)]
            }
            (Block::Tool(ti), "input_json_delta") => {
                let p = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                vec![self.chunk(
                    json!({"tool_calls": [{
                        "index": ti,
                        "function": {"arguments": p},
                    }]}),
                    None,
                )]
            }
            // signature_delta:思考签名,OpenAI 侧无处安放且不可回放,丢弃。
            _ => Vec::new(),
        }
    }

    /// 传输层 EOF(底层流走到尽头)时调用。
    ///
    /// ⚠️ **EOF 不等于协议完成**。没见过 `message_stop` 就 EOF = 上游半截断流,
    /// 这里必须报错而不是补一个 `finish_reason:"stop"` —— 理由见
    /// [`super::error::truncated_stream_payload`]。幂等。
    pub fn finish(&mut self) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        self.fail(&super::error::truncated_stream_payload())
    }

    /// 已收到 `message_stop` 的正常收尾:末帧(带 finish_reason)→ 用量帧(客户要了才发)
    /// → `[DONE]`。
    ///
    /// **顺序不能反**:用量帧必须在 `[DONE]` 之前,否则 NewAPI 读到终止哨兵就收工,
    /// 这次请求在它那边记 0 用量。
    fn complete(&mut self) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let reason = finish_reason(self.stop_reason.as_deref());
        let mut out = vec![self.chunk(json!({}), Some(reason))];
        if self.include_usage {
            let mut c = self.envelope();
            c.insert("choices".into(), json!([]));
            c.insert("usage".into(), self.usage.chat_json());
            out.push(WireFrame::data(Value::Object(c).to_string()));
        }
        out.push(WireFrame::data(DONE));
        out
    }

    /// 流中出现错误:发一条 `{"error":{...}}` 再 `[DONE]`。
    ///
    /// `data` 应当**已被调用方中性化**(与 worker 的 `sanitize_upstream_error_payload`
    /// 同一道闸门),本函数只换形状。
    pub fn fail(&mut self, anthropic_error: &Value) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let body = super::error::from_anthropic_error(anthropic_error, 502);
        vec![WireFrame::data(body.to_string()), WireFrame::data(DONE)]
    }

    /// 上游静默时的保活帧:一条**内容为空**的增量。
    ///
    /// 不用 SSE 注释(`: ping`):它在标准解析器里被跳过,不会变成任何下游事件,
    /// 客户端照样判定空闲 —— 这正是 worker 那条 `keepalive_frame` 注释里
    /// 实测过的坑,换协议不换结论。
    pub fn keepalive(&self) -> WireFrame {
        let mut c = self.envelope();
        c.insert(
            "choices".into(),
            json!([{"index": 0, "delta": {}, "finish_reason": Value::Null}]),
        );
        WireFrame::data(Value::Object(c).to_string())
    }

    /// 是否已经发过终止序列。调用方据此**停发保活** —— `[DONE]` 之后再冒出一条空 chunk,
    /// 严格客户端会当成协议违例。
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> WireFrame {
        let mut c = self.envelope();
        c.insert(
            "choices".into(),
            json!([{
                "index": 0,
                "delta": delta,
                "logprobs": Value::Null,
                "finish_reason": finish.map(Value::from).unwrap_or(Value::Null),
            }]),
        );
        WireFrame::data(Value::Object(c).to_string())
    }

    fn envelope(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("object".into(), json!("chat.completion.chunk"));
        m.insert("created".into(), json!(self.created));
        m.insert("model".into(), json!(self.model));
        m
    }
}

/// 已折叠的 Anthropic Messages → `chat.completion`(非流式)。
///
/// `msg` 就是 [`crate::fold::fold_sse_to_message`] 的产物;`fallback_model` 在
/// 上游没回模型名时兜底。
pub fn fold_completion(msg: &Value, fallback_model: &str) -> Value {
    let mut usage = UsageAccum::default();
    if let Some(u) = msg.get("usage") {
        usage.merge(u);
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => {
                reasoning.push_str(b.get("thinking").and_then(Value::as_str).unwrap_or(""))
            }
            Some("tool_use") => {
                let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "index": tool_calls.len(),
                    "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                    "type": "function",
                    "function": {
                        "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                        // OpenAI 的 arguments 是**字符串化的 JSON**,不是对象。
                        "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                    },
                }));
            }
            _ => {}
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    // content 恒存在:纯工具轮也要给 null 而不是省略键(定长结构体客户端会解析失败)。
    message.insert(
        "content".into(),
        if text.is_empty() { Value::Null } else { json!(text) },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let stop = msg.get("stop_reason").and_then(Value::as_str);
    json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": now_unix(),
        "model": msg.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "logprobs": Value::Null,
            "finish_reason": finish_reason(stop),
        }],
        "usage": usage.chat_json(),
    })
}

fn index_of(data: &Value) -> Option<usize> {
    data.get("index").and_then(Value::as_u64).map(|i| i as usize)
}

pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: Value) -> SseEvent {
        SseEvent::new(event, data)
    }

    /// 把帧的 data 解析成 JSON(`[DONE]` 保留原样标记)。
    fn payloads(frames: &[WireFrame]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| {
                assert!(f.event.is_none(), "ChatCompletions 不该写 event 行");
                serde_json::from_str(&f.data).unwrap_or(Value::String(f.data.clone()))
            })
            .collect()
    }

    fn text_stream() -> Vec<SseEvent> {
        vec![
            ev(
                "message_start",
                json!({"message": {"model": "grok-4.5", "usage": {"input_tokens": 10}}}),
            ),
            ev(
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "he"}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "llo"}}),
            ),
            ev("content_block_stop", json!({"index": 0})),
            ev(
                "message_delta",
                json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            ),
            ev("message_stop", json!({})),
        ]
    }

    fn run(events: &[SseEvent], include_usage: bool) -> Vec<Value> {
        let mut s = ChatStreamOut::new("m", include_usage);
        let frames: Vec<WireFrame> = events.iter().flat_map(|e| s.push(e)).collect();
        payloads(&frames)
    }

    #[test]
    fn 纯文本流的完整帧序列() {
        let out = run(&text_stream(), false);
        assert_eq!(out.len(), 5, "role 帧 + 2 个增量 + 末帧 + [DONE]");
        assert_eq!(out[0]["choices"][0]["delta"], json!({"role":"assistant","content":""}));
        assert_eq!(out[0]["object"], "chat.completion.chunk");
        assert_eq!(out[0]["model"], "grok-4.5", "message_start 的模型名该覆盖请求名");
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "he");
        assert_eq!(out[2]["choices"][0]["delta"]["content"], "llo");
        assert_eq!(out[3]["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[3]["choices"][0]["delta"], json!({}));
        // 所有 chunk 共用同一个 id([DONE] 不是 JSON,跳过)。
        let id = out[0]["id"].as_str().unwrap();
        assert!(id.starts_with("chatcmpl-"));
        assert!(out[..4].iter().all(|c| c["id"] == json!(id)));
    }

    #[test]
    fn 没有终止哨兵就等于让客户端干等到超时() {
        let out = run(&text_stream(), false);
        assert_eq!(out.last().unwrap(), &json!("[DONE]").clone(), "末尾必须是 [DONE]");
    }

    #[test]
    fn 用量帧在_done_之前_且只在客户要了时才发() {
        let with = run(&text_stream(), true);
        let n = with.len();
        assert_eq!(with[n - 1], json!("[DONE]"));
        // 顺序反了 NewAPI 就记不到用量 —— 这条单独锁。
        assert_eq!(with[n - 2]["choices"], json!([]));
        assert_eq!(with[n - 2]["usage"]["prompt_tokens"], json!(10));
        assert_eq!(with[n - 2]["usage"]["completion_tokens"], json!(3));

        let without = run(&text_stream(), false);
        assert!(without.iter().all(|c| c.get("usage").is_none()));
    }

    #[test]
    fn 工具调用的_index_从零连续_不复用_anthropic_块下标() {
        // 思考块占了 index 0,两次工具调用在 Anthropic 侧是 index 1、2。
        let events = vec![
            ev("message_start", json!({"message": {}})),
            ev("content_block_start", json!({"index":0,"content_block":{"type":"thinking"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("content_block_start",
               json!({"index":1,"content_block":{"type":"tool_use","id":"t1","name":"f"}})),
            ev("content_block_delta",
               json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}})),
            ev("content_block_delta",
               json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}})),
            ev("content_block_stop", json!({"index":1})),
            ev("content_block_start",
               json!({"index":2,"content_block":{"type":"tool_use","id":"t2","name":"g"}})),
            ev("content_block_stop", json!({"index":2})),
            ev("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events, false);
        let tool_frames: Vec<&Value> = out
            .iter()
            .filter(|c| c.pointer("/choices/0/delta/tool_calls").is_some())
            .collect();
        assert_eq!(tool_frames[0].pointer("/choices/0/delta/tool_calls/0/index"), Some(&json!(0)));
        assert_eq!(tool_frames[0].pointer("/choices/0/delta/tool_calls/0/id"), Some(&json!("t1")));
        assert_eq!(
            tool_frames[0].pointer("/choices/0/delta/tool_calls/0/function/name"),
            Some(&json!("f"))
        );
        assert_eq!(
            tool_frames[1].pointer("/choices/0/delta/tool_calls/0/function/arguments"),
            Some(&json!("{\"a\":"))
        );
        // 第二次调用必须是 index 1(Anthropic 侧它是 2)。
        let last_tool = tool_frames.last().unwrap();
        assert_eq!(last_tool.pointer("/choices/0/delta/tool_calls/0/index"), Some(&json!(1)));
        assert_eq!(last_tool.pointer("/choices/0/delta/tool_calls/0/id"), Some(&json!("t2")));
        // 工具轮的 finish_reason 必须是 tool_calls,否则客户端不会去执行工具。
        let fin = out.iter().rev().find(|c| c.pointer("/choices/0/finish_reason").is_some_and(|v| !v.is_null()));
        assert_eq!(fin.unwrap()["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn 思考走_reasoning_content_而不是被吞掉() {
        let events = vec![
            ev("message_start", json!({"message": {}})),
            ev("content_block_start", json!({"index":0,"content_block":{"type":"thinking"}})),
            ev("content_block_delta",
               json!({"index":0,"delta":{"type":"thinking_delta","thinking":"想"}})),
            // 签名无处安放,必须丢掉且不产生任何帧。
            ev("content_block_delta",
               json!({"index":0,"delta":{"type":"signature_delta","signature":"sig"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events, false);
        let deltas: Vec<&Value> = out.iter().filter_map(|c| c.pointer("/choices/0/delta")).collect();
        assert!(deltas.iter().any(|d| d.get("reasoning_content") == Some(&json!("想"))));
        assert!(!out.iter().any(|c| c.to_string().contains("sig")));
    }

    #[test]
    fn 块结束后的迟到增量不会串到别的块() {
        let mut s = ChatStreamOut::new("m", false);
        s.push(&ev("message_start", json!({"message": {}})));
        s.push(&ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})));
        s.push(&ev("content_block_stop", json!({"index":0})));
        let late = s.push(&ev(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"迟到"}}),
        ));
        assert!(late.is_empty(), "没登记的块下标不该产出任何帧");
    }

    #[test]
    fn 未登记的块类型连同它的增量一起丢弃() {
        let mut s = ChatStreamOut::new("m", false);
        s.push(&ev("message_start", json!({"message": {}})));
        let a = s.push(&ev(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"server_tool_use","id":"x","name":"web"}}),
        ));
        let b = s.push(&ev(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        ));
        assert!(a.is_empty() && b.is_empty());
        // 而且它不能占掉一个 tool_calls 下标。
        let c = s.push(&ev(
            "content_block_start",
            json!({"index":1,"content_block":{"type":"tool_use","id":"t","name":"f"}}),
        ));
        let v = payloads(&c);
        assert_eq!(v[0].pointer("/choices/0/delta/tool_calls/0/index"), Some(&json!(0)));
    }

    #[test]
    fn 收尾幂等_message_stop_之后再_finish_不重复发() {
        let mut s = ChatStreamOut::new("m", true);
        for e in text_stream() {
            s.push(&e);
        }
        assert!(s.finish().is_empty(), "重复收尾会发出第二个 [DONE]");
    }

    /// **传输 EOF ≠ 协议完成。** 上游半截断流(没发 `message_stop`)时补一个
    /// `finish_reason:"stop"` 会把可检测的截断变成静默的假成功 —— 客户端会把半截答案
    /// 当完整结果用。必须报错。
    #[test]
    fn 半截断流报错_而不是补一个成功的_finish_reason() {
        let mut s = ChatStreamOut::new("m", true);
        for e in text_stream().into_iter().take(4) {
            s.push(&e);
        }
        let tail = payloads(&s.finish());
        assert_eq!(tail.len(), 2, "错误帧 + [DONE]");
        assert!(
            tail[0].get("error").is_some(),
            "必须是错误帧,不能是 finish_reason:stop 的末帧:{:?}",
            tail[0]
        );
        assert_eq!(tail[1], json!("[DONE]"));
        // 反向:正常收到 message_stop 的流才给 finish_reason。
        let ok = run(&text_stream(), false);
        assert_eq!(ok[ok.len() - 2]["choices"][0]["finish_reason"], "stop");
    }

    /// 终态之后上游又冒出内容(协议违例)时,不能让内容帧跟在 `[DONE]` 后面。
    #[test]
    fn 终态之后的事件一律不再产出帧() {
        // error 之后又来 delta。
        let mut s = ChatStreamOut::new("m", false);
        s.push(&ev("message_start", json!({"message":{}})));
        s.push(&ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})));
        s.push(&ev("error", json!({"type":"error","error":{"type":"api_error","message":"x"}})));
        assert!(s
            .push(&ev("content_block_delta",
                      json!({"index":0,"delta":{"type":"text_delta","text":"迟到"}})))
            .is_empty());
        assert!(s.push(&ev("message_stop", json!({}))).is_empty());

        // message_stop 之后又开新块。
        let mut s2 = ChatStreamOut::new("m", false);
        for e in text_stream() {
            s2.push(&e);
        }
        assert!(s2
            .push(&ev("content_block_start", json!({"index":9,"content_block":{"type":"text"}})))
            .is_empty());
    }

    /// 同一块下标重复 start:忽略,不能再占一个 tool_calls 下标。
    #[test]
    fn 重复的块起始不会占掉第二个工具下标() {
        let mut s = ChatStreamOut::new("m", false);
        s.push(&ev("message_start", json!({"message":{}})));
        let first = payloads(&s.push(&ev(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"tool_use","id":"t1","name":"f"}}),
        )));
        assert_eq!(first[0].pointer("/choices/0/delta/tool_calls/0/index"), Some(&json!(0)));
        // 重复 start 同一个下标 → 零帧。
        assert!(s
            .push(&ev("content_block_start",
                      json!({"index":0,"content_block":{"type":"tool_use","id":"t1","name":"f"}})))
            .is_empty());
        // 参数仍写到 index 0,不会跑到一个凭空多出来的 index 1 上。
        let d = payloads(&s.push(&ev(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        )));
        assert_eq!(d[0].pointer("/choices/0/delta/tool_calls/0/index"), Some(&json!(0)));
    }

    #[test]
    fn is_finished_在终止后为真_供调用方停发保活() {
        let mut s = ChatStreamOut::new("m", false);
        assert!(!s.is_finished());
        for e in text_stream() {
            s.push(&e);
        }
        assert!(s.is_finished(), "[DONE] 之后再发保活,严格客户端会当协议违例");
    }

    #[test]
    fn 错误事件转成_openai_错误体后再_done() {
        let mut s = ChatStreamOut::new("m", false);
        s.push(&ev("message_start", json!({"message": {}})));
        let out = payloads(&s.push(&ev(
            "error",
            json!({"type":"error","error":{"type":"rate_limit_error","message":"慢点"}}),
        )));
        assert_eq!(out[0]["error"]["type"], "rate_limit_error");
        assert_eq!(out[0]["error"]["message"], "慢点");
        assert_eq!(out[1], json!("[DONE]"));
        // 错误已经收过尾,后面的 message_stop 不该再吐东西。
        assert!(s.push(&ev("message_stop", json!({}))).is_empty());
    }

    #[test]
    fn 保活帧是空_delta_而不是_sse_注释() {
        let s = ChatStreamOut::new("m", false);
        let f = s.keepalive();
        assert!(f.event.is_none());
        let v: Value = serde_json::from_str(&f.data).unwrap();
        assert_eq!(v["choices"][0]["delta"], json!({}));
        assert!(v["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn 非流式折叠与流式给出同样的文本和用量() {
        let msg = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "grok-4.5",
            "content": [{"type":"text","text":"hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 3},
        });
        let c = fold_completion(&msg, "m");
        assert_eq!(c["object"], "chat.completion");
        assert_eq!(c["model"], "grok-4.5");
        assert_eq!(c["choices"][0]["message"]["content"], "hello");
        assert_eq!(c["choices"][0]["finish_reason"], "stop");
        assert_eq!(c["usage"]["prompt_tokens"], json!(10));
        assert_eq!(c["usage"]["completion_tokens"], json!(3));
    }

    #[test]
    fn 非流式的工具调用_arguments_是字符串不是对象() {
        let msg = json!({
            "role": "assistant", "content": [
                {"type":"thinking","thinking":"想"},
                {"type":"tool_use","id":"t1","name":"f","input":{"a":1}}],
            "stop_reason": "tool_use",
        });
        let c = fold_completion(&msg, "m");
        let m = &c["choices"][0]["message"];
        assert_eq!(m["content"], Value::Null, "纯工具轮 content 必须是 null,不能省略键");
        assert_eq!(m["reasoning_content"], "想");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(m["tool_calls"][0]["index"], json!(0));
        assert_eq!(c["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn stop_reason_映射表() {
        assert_eq!(finish_reason(Some("end_turn")), "stop");
        assert_eq!(finish_reason(Some("stop_sequence")), "stop");
        assert_eq!(finish_reason(Some("max_tokens")), "length");
        assert_eq!(finish_reason(Some("tool_use")), "tool_calls");
        assert_eq!(finish_reason(Some("refusal")), "content_filter");
        assert_eq!(finish_reason(None), "stop");
    }
}
