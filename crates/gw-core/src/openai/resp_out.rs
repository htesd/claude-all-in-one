//! Anthropic 事件流 → OpenAI **Responses** 线缆。
//!
//! Responses 的流不是「一串同构 chunk」,而是**带生命周期的条目流**:
//! 每个输出项(message / reasoning / function_call)都要成对地
//! `added … delta … done`,漏一半客户端就永远等不到那个条目收口。
//! 所以状态机要为每个打开中的 Anthropic 块记住它的 `output_index` 与累积文本。
//!
//! Anthropic 块 → Responses 输出项的对应:
//!
//! | Anthropic | Responses |
//! |---|---|
//! | `text` | `message`(内含一个 `output_text` part) |
//! | `thinking` | `reasoning`(内含一个 `summary_text`) |
//! | `tool_use` | `function_call` |

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::chat_out::now_unix;
use super::usage::UsageAccum;
use super::WireFrame;
use crate::provider::SseEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
}

#[derive(Debug, Clone)]
struct OpenItem {
    output_index: usize,
    item_id: String,
    kind: ItemKind,
    /// 累积的文本 / 思考 / 参数 JSON —— `*.done` 事件与最终快照都要它的全量。
    buf: String,
    call_id: String,
    name: String,
}

/// `Response` 对象里要回显请求的那几个字段,**从 IR 反推**。
///
/// 不带它就只能硬编码 `instructions:null` / `tools:[]` / `tool_choice:"auto"` /
/// `parallel_tool_calls:true` —— 客户端明明发了 `instructions` 和一组工具,拿回来的
/// 快照却说没有,那是网关在撒谎(对抗评审 Architect#4)。
///
/// 数据源是**转换后的 Anthropic body**(worker 手上就有),不必把原始 OpenAI 请求一路
/// 带下来。代价是 IR 有损,以下三处**回显不精确**,别把这个快照当请求的权威副本
/// (对抗评审 Minimalist#6):
///
/// - `instructions`:顶层 `instructions` 与被提升的 `developer` 消息在 IR 里已经合并成
///   一个 `system`,分不开了 —— 两者都发过时回显的是拼接结果。
/// - `tools[].strict`:IR 不带这个标志,一律回 `false`(即使客户端要的是 `true`)。
/// - `metadata`:IR 不带,恒回 `{}`。
///
/// 要做到逐字精确就得把原始请求体一路带到出站,那是为一个「审计用」字段增加一条贯穿
/// 全链路的数据通道 —— 不值。这里选择「大致对 + 把不准的地方写明」。
#[derive(Debug, Clone, Default)]
pub struct RequestEcho {
    instructions: Value,
    tools: Value,
    tool_choice: Value,
    parallel_tool_calls: bool,
}

impl RequestEcho {
    /// 从 Anthropic Messages body 反向映射出 OpenAI 侧的回显值。
    pub fn from_anthropic(body: &Value) -> Self {
        let instructions = match body.get("system") {
            Some(Value::String(s)) if !s.is_empty() => Value::String(s.clone()),
            // 数组形态的 system(块列表):把文本拼起来,拼不出就当没有。
            Some(Value::Array(a)) => {
                let joined: String = a
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if joined.is_empty() {
                    Value::Null
                } else {
                    Value::String(joined)
                }
            }
            _ => Value::Null,
        };
        let tools = body
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .map(|t| {
                            json!({
                                "type": "function",
                                "name": t.get("name").cloned().unwrap_or(Value::Null),
                                "description": t.get("description").cloned().unwrap_or(Value::Null),
                                "parameters": t.get("input_schema").cloned()
                                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                                "strict": false,
                            })
                        })
                        .collect(),
                )
            })
            .unwrap_or_else(|| json!([]));
        let tc = body.get("tool_choice");
        let tool_choice = match tc.and_then(|c| c.get("type")).and_then(Value::as_str) {
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => json!({
                "type": "function",
                "name": tc.and_then(|c| c.get("name")).cloned().unwrap_or(Value::Null),
            }),
            _ => json!("auto"),
        };
        // Anthropic 用「禁用并行」表达,OpenAI 用「允许并行」表达 —— 取反。
        let parallel_tool_calls = !tc
            .and_then(|c| c.get("disable_parallel_tool_use"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            instructions,
            tools,
            tool_choice,
            parallel_tool_calls,
        }
    }
}

/// Responses 流式状态机。
pub struct ResponsesStreamOut {
    id: String,
    created: i64,
    model: String,
    seq: u64,
    usage: UsageAccum,
    open: BTreeMap<usize, OpenItem>,
    /// 已收口的输出项,**按 `output_index` 索引**而不是按收口顺序 push。
    ///
    /// 两个并行块若 1 先于 0 收口,按 push 顺序的数组会变成 `[项1, 项0]` ——
    /// 而事件流里它们的 `output_index` 是 0、1。客户端实时重建出的结果与
    /// `response.completed` 快照互相矛盾(对抗评审 Skeptic#5)。用 index 当键,
    /// 快照永远按 index 升序,与事件流一致。
    done_items: BTreeMap<usize, Value>,
    next_output_index: usize,
    stop_reason: Option<String>,
    started: bool,
    finished: bool,
    echo: RequestEcho,
}

impl ResponsesStreamOut {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            created: now_unix(),
            model: model.into(),
            seq: 0,
            usage: UsageAccum::default(),
            open: BTreeMap::new(),
            done_items: BTreeMap::new(),
            next_output_index: 0,
            stop_reason: None,
            started: false,
            finished: false,
            echo: RequestEcho::default(),
        }
    }

    /// 挂上请求回显(来自转换后的 Anthropic body)。不挂则回显为「什么都没要」。
    pub fn with_echo(mut self, echo: RequestEcho) -> Self {
        self.echo = echo;
        self
    }

    pub fn push(&mut self, ev: &SseEvent) -> Vec<WireFrame> {
        // 终态之后一帧都不许再出,理由同 [`super::chat_out::ChatStreamOut::push`]。
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
                self.start()
            }
            "content_block_start" => self.on_block_start(&ev.data),
            "content_block_delta" => self.on_delta(&ev.data),
            "content_block_stop" => match index_of(&ev.data) {
                Some(i) => self.close_item(i),
                None => Vec::new(),
            },
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
            _ => Vec::new(),
        }
    }

    /// `response.created` + `response.in_progress`。幂等(只发一次)。
    fn start(&mut self) -> Vec<WireFrame> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let snap = self.snapshot("in_progress");
        vec![
            self.emit("response.created", json!({ "response": snap })),
            self.emit("response.in_progress", json!({ "response": self.snapshot("in_progress") })),
        ]
    }

    fn on_block_start(&mut self, data: &Value) -> Vec<WireFrame> {
        let Some(idx) = index_of(data) else {
            return Vec::new();
        };
        let cb = data.get("content_block");
        let ty = cb.and_then(|b| b.get("type")).and_then(Value::as_str);
        let kind = match ty {
            Some("text") => ItemKind::Message,
            Some("thinking") => ItemKind::Reasoning,
            Some("tool_use") => ItemKind::FunctionCall,
            // Responses 侧没有对应物的块:不登记 → 它的增量也一并丢弃,
            // 且不占用 output_index(否则最终 output 数组会出现空洞)。
            _ => return Vec::new(),
        };

        // 同一个块下标重复 start(上游协议违例):**忽略**,不重新登记。
        // 覆盖登记会再占一个 output_index,留下一个永远空着的条目,后续增量全写到
        // 新条目上(对抗评审 Skeptic#8)。
        if self.open.contains_key(&idx) {
            return Vec::new();
        }

        // 上游没发 message_start 的极端情形下也要先把 response 开出来,
        // 否则客户端会收到一个没有 `response.created` 的孤儿条目流。
        let mut out = self.start();

        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = match kind {
            ItemKind::Message => format!("msg_{}", uuid::Uuid::new_v4().simple()),
            ItemKind::Reasoning => format!("rs_{}", uuid::Uuid::new_v4().simple()),
            ItemKind::FunctionCall => format!("fc_{}", uuid::Uuid::new_v4().simple()),
        };
        let call_id = cb
            .and_then(|b| b.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = cb
            .and_then(|b| b.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let item = OpenItem {
            output_index,
            item_id: item_id.clone(),
            kind,
            buf: String::new(),
            call_id,
            name,
        };
        out.push(self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item_skeleton(&item)}),
        ));
        match kind {
            ItemKind::Message => out.push(self.emit(
                "response.content_part.added",
                json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            )),
            ItemKind::Reasoning => out.push(self.emit(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": item_id, "output_index": output_index, "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""},
                }),
            )),
            // function_call 没有 part 层,参数直接走 arguments.delta。
            ItemKind::FunctionCall => {}
        }
        self.open.insert(idx, item);
        out
    }

    fn on_delta(&mut self, data: &Value) -> Vec<WireFrame> {
        let Some(idx) = index_of(data) else {
            return Vec::new();
        };
        let Some(delta) = data.get("delta") else {
            return Vec::new();
        };
        let kind_str = delta.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(item) = self.open.get(&idx) else {
            return Vec::new();
        };
        let (kind, item_id, output_index) = (item.kind, item.item_id.clone(), item.output_index);

        let (event, key, text) = match (kind, kind_str) {
            (ItemKind::Message, "text_delta") => (
                "response.output_text.delta",
                "delta",
                delta.get("text").and_then(Value::as_str).unwrap_or(""),
            ),
            (ItemKind::Reasoning, "thinking_delta") => (
                "response.reasoning_summary_text.delta",
                "delta",
                delta.get("thinking").and_then(Value::as_str).unwrap_or(""),
            ),
            (ItemKind::FunctionCall, "input_json_delta") => (
                "response.function_call_arguments.delta",
                "delta",
                delta.get("partial_json").and_then(Value::as_str).unwrap_or(""),
            ),
            // signature_delta 等:Responses 侧无处安放且不可回放,丢弃。
            _ => return Vec::new(),
        };
        if let Some(item) = self.open.get_mut(&idx) {
            item.buf.push_str(text);
        }

        let mut payload = Map::new();
        payload.insert("item_id".into(), json!(item_id));
        payload.insert("output_index".into(), json!(output_index));
        match kind {
            ItemKind::Message => {
                payload.insert("content_index".into(), json!(0));
            }
            ItemKind::Reasoning => {
                payload.insert("summary_index".into(), json!(0));
            }
            ItemKind::FunctionCall => {}
        }
        payload.insert(key.into(), json!(text));
        vec![self.emit(event, Value::Object(payload))]
    }

    /// 收口一个输出项:`*.done` 系列 + `output_item.done`,并把成品存进最终快照。
    fn close_item(&mut self, idx: usize) -> Vec<WireFrame> {
        let Some(item) = self.open.remove(&idx) else {
            return Vec::new();
        };
        let (id, oi) = (item.item_id.clone(), item.output_index);
        let mut out = Vec::new();
        match item.kind {
            ItemKind::Message => {
                out.push(self.emit(
                    "response.output_text.done",
                    json!({"item_id": id, "output_index": oi, "content_index": 0,
                           "text": item.buf}),
                ));
                out.push(self.emit(
                    "response.content_part.done",
                    json!({"item_id": id, "output_index": oi, "content_index": 0,
                           "part": {"type":"output_text","text": item.buf,"annotations":[]}}),
                ));
            }
            ItemKind::Reasoning => {
                out.push(self.emit(
                    "response.reasoning_summary_text.done",
                    json!({"item_id": id, "output_index": oi, "summary_index": 0,
                           "text": item.buf}),
                ));
                out.push(self.emit(
                    "response.reasoning_summary_part.done",
                    json!({"item_id": id, "output_index": oi, "summary_index": 0,
                           "part": {"type":"summary_text","text": item.buf}}),
                ));
            }
            ItemKind::FunctionCall => {
                out.push(self.emit(
                    "response.function_call_arguments.done",
                    json!({"item_id": id, "output_index": oi, "arguments": item.buf}),
                ));
            }
        }
        let finished = item_final(&item);
        out.push(self.emit(
            "response.output_item.done",
            json!({"output_index": oi, "item": finished.clone()}),
        ));
        self.done_items.insert(oi, finished);
        out
    }

    /// 传输层 EOF(底层流走到尽头)时调用。
    ///
    /// ⚠️ **EOF 不等于协议完成**。没见过 `message_stop` 就 EOF = 上游半截断流,
    /// 必须发 `response.failed` 而不是 `response.completed` —— 理由见
    /// [`super::error::truncated_stream_payload`]。幂等。
    pub fn finish(&mut self) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        self.fail(&super::error::truncated_stream_payload())
    }

    /// 已收到 `message_stop` 的正常收尾:补齐所有还开着的条目,再发 `response.completed`。
    ///
    /// 条目必须收口:客户端会卡在一个永远等不到 `done` 的条目上。
    fn complete(&mut self) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        // 上游只发了 `message_stop`(没 message_start、没内容块)时也要先开场:
        // 一个没有 `response.created` 的孤儿 `response.completed`,客户端的状态机认不了。
        let mut out = self.start();
        let status = self.terminal_status();
        // ⚠️ **只有干净完成才给还开着的条目补 `done`。**
        //
        // `incomplete`(撞 max_tokens)意味着内容是**半截的**:此时给一个还没收口的
        // `function_call` 发 `arguments.done` + `output_item.done`,等于告诉客户端
        // 「这个工具调用已完整,去执行吧」,而参数可能只有 `{"path":"/etc` 那么长
        // (对抗评审 Minimalist#5)。半截条目就该留着不收口,由终止事件告诉客户端出事了。
        if status == "completed" {
            let still_open: Vec<usize> = self.open.keys().copied().collect();
            out.extend(still_open.into_iter().flat_map(|i| self.close_item(i)));
        }
        self.finished = true;
        // 终止事件名必须跟状态走:`incomplete` 状态配 `response.completed` 事件,
        // 按**事件名**分派的客户端(绝大多数 SDK)会把它当成功。
        let event = if status == "completed" {
            "response.completed"
        } else {
            "response.incomplete"
        };
        out.push(self.emit(event, json!({"response": self.snapshot(status)})));
        out
    }

    /// 终态失败 → `response.failed`。`data` 应已被调用方中性化。
    ///
    /// **不给开着的条目补 `done`**:那等于宣称它完成了。`response.failed` 是终态,
    /// 客户端据此中止,半开条目由它自己丢弃。
    pub fn fail(&mut self, anthropic_error: &Value) -> Vec<WireFrame> {
        if self.finished {
            return Vec::new();
        }
        // 上游连 message_start 都没发就出错时,先把 response 开出来 ——
        // 客户端的状态机认不了一个没有 `response.created` 的 `response.failed`。
        let mut out = self.start();
        self.finished = true;
        let err = super::error::from_anthropic_error(anthropic_error, 502);
        let mut snap = self.snapshot("failed");
        if let Some(m) = snap.as_object_mut() {
            m.insert(
                "error".into(),
                json!({
                    "code": err.pointer("/error/type").cloned().unwrap_or(Value::Null),
                    "message": err.pointer("/error/message").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        out.push(self.emit("response.failed", json!({ "response": snap })));
        out
    }

    /// 上游静默时的保活。
    ///
    /// 发 `response.in_progress` 而不是 SSE 注释:注释在标准解析器里被跳过、
    /// 不会变成任何下游事件,客户端照样判定空闲 —— 与 worker 的 `keepalive_frame`
    /// 是同一条实测结论。`in_progress` 是协议内合法事件,语义正好是「还在跑」。
    ///
    /// 返回 `Vec` 而不是单帧,是因为**首个上游事件迟到超过保活间隔**时,
    /// `response.created` 还没发出去 —— 那就得先补它,否则客户端收到的第一帧是
    /// `in_progress`,状态机直接倒序。`start()` 幂等,已开过就只出保活那一帧。
    pub fn keepalive(&mut self) -> Vec<WireFrame> {
        let mut out = self.start();
        let ka = self.emit("response.in_progress", json!({"response": self.snapshot("in_progress")}));
        out.push(ka);
        out
    }

    /// 是否已经发过终止事件。调用方据此**停发保活** —— `response.completed` 之后
    /// 再冒出 `response.in_progress`,严格客户端会当成协议违例。
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn terminal_status(&self) -> &'static str {
        match self.stop_reason.as_deref() {
            Some("max_tokens") => "incomplete",
            _ => "completed",
        }
    }

    /// 当前的 response 快照。`output` 只含**已收口**的条目 ——
    /// 半截条目进快照会让客户端把未完成的参数当成最终值。
    fn snapshot(&self, status: &str) -> Value {
        let incomplete = if status == "incomplete" {
            json!({"reason": "max_output_tokens"})
        } else {
            Value::Null
        };
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("object".into(), json!("response"));
        m.insert("created_at".into(), json!(self.created));
        m.insert("status".into(), json!(status));
        m.insert("model".into(), json!(self.model));
        m.insert(
            "output".into(),
            Value::Array(self.done_items.values().cloned().collect()),
        );
        m.insert("error".into(), Value::Null);
        m.insert("incomplete_details".into(), incomplete);
        // 回显客户端**实际发来的**请求参数,不是固定值(见 RequestEcho)。
        m.insert("instructions".into(), self.echo.instructions.clone());
        m.insert("metadata".into(), json!({}));
        m.insert(
            "parallel_tool_calls".into(),
            json!(self.echo.parallel_tool_calls),
        );
        m.insert("previous_response_id".into(), Value::Null);
        m.insert("tool_choice".into(), self.echo.tool_choice.clone());
        m.insert("tools".into(), self.echo.tools.clone());
        // 用量在终态才有意义;跑到一半时给 null 而不是 0,避免客户把它当真实值。
        m.insert(
            "usage".into(),
            if self.usage.is_empty() {
                Value::Null
            } else {
                self.usage.responses_json()
            },
        );
        Value::Object(m)
    }

    fn emit(&mut self, event: &str, mut data: Value) -> WireFrame {
        self.seq += 1;
        if let Some(m) = data.as_object_mut() {
            m.insert("type".into(), json!(event));
            m.insert("sequence_number".into(), json!(self.seq));
        }
        WireFrame::named(event, &data)
    }
}

/// `output_item.added` 用的骨架(内容为空、状态 in_progress)。
fn item_skeleton(item: &OpenItem) -> Value {
    match item.kind {
        ItemKind::Message => json!({
            "id": item.item_id, "type": "message", "status": "in_progress",
            "role": "assistant", "content": [],
        }),
        ItemKind::Reasoning => json!({
            "id": item.item_id, "type": "reasoning", "summary": [],
        }),
        ItemKind::FunctionCall => json!({
            "id": item.item_id, "type": "function_call", "status": "in_progress",
            "call_id": item.call_id, "name": item.name, "arguments": "",
        }),
    }
}

/// 收口后的完整条目(也是最终 `response.output` 的元素)。
fn item_final(item: &OpenItem) -> Value {
    match item.kind {
        ItemKind::Message => json!({
            "id": item.item_id, "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": item.buf, "annotations": []}],
        }),
        ItemKind::Reasoning => json!({
            "id": item.item_id, "type": "reasoning",
            "summary": [{"type": "summary_text", "text": item.buf}],
        }),
        ItemKind::FunctionCall => json!({
            "id": item.item_id, "type": "function_call", "status": "completed",
            "call_id": item.call_id, "name": item.name,
            // 参数原样透传字符串(可能不是合法 JSON —— 那是上游的问题,
            // 我方擅自「修好」它反而会掩盖故障)。
            "arguments": item.buf,
        }),
    }
}

/// 已折叠的 Anthropic Messages → Responses 的 `Response` 对象(非流式)。
///
/// `echo` 来自**请求**的 Anthropic body(不是这里的响应 `msg`),与流式侧同源。
pub fn fold_response(msg: &Value, fallback_model: &str, echo: &RequestEcho) -> Value {
    let mut usage = UsageAccum::default();
    if let Some(u) = msg.get("usage") {
        usage.merge(u);
    }
    let mut output: Vec<Value> = Vec::new();
    for b in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => output.push(json!({
                "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                "type": "message", "status": "completed", "role": "assistant",
                "content": [{"type":"output_text",
                             "text": b.get("text").and_then(Value::as_str).unwrap_or(""),
                             "annotations": []}],
            })),
            Some("thinking") => output.push(json!({
                "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
                "type": "reasoning",
                "summary": [{"type":"summary_text",
                             "text": b.get("thinking").and_then(Value::as_str).unwrap_or("")}],
            })),
            Some("tool_use") => {
                let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                output.push(json!({
                    "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function_call", "status": "completed",
                    "call_id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                    "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                }));
            }
            _ => {}
        }
    }
    let incomplete = msg.get("stop_reason").and_then(Value::as_str) == Some("max_tokens");
    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": now_unix(),
        "status": if incomplete { "incomplete" } else { "completed" },
        "model": msg.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "output": output,
        "error": Value::Null,
        "incomplete_details": if incomplete { json!({"reason":"max_output_tokens"}) } else { Value::Null },
        "instructions": echo.instructions.clone(),
        "metadata": {},
        "parallel_tool_calls": echo.parallel_tool_calls,
        "previous_response_id": Value::Null,
        "tool_choice": echo.tool_choice.clone(),
        "tools": echo.tools.clone(),
        "usage": usage.responses_json(),
    })
}

fn index_of(data: &Value) -> Option<usize> {
    data.get("index").and_then(Value::as_u64).map(|i| i as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: Value) -> SseEvent {
        SseEvent::new(event, data)
    }

    /// (事件名, 负载) 序列。
    fn run(events: &[SseEvent]) -> Vec<(String, Value)> {
        let mut s = ResponsesStreamOut::new("m");
        events
            .iter()
            .flat_map(|e| s.push(e))
            .map(|f| {
                (
                    f.event.expect("Responses 每帧都必须带 event 名"),
                    serde_json::from_str(&f.data).unwrap(),
                )
            })
            .collect()
    }

    fn names(out: &[(String, Value)]) -> Vec<&str> {
        out.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn text_stream() -> Vec<SseEvent> {
        vec![
            ev("message_start", json!({"message": {"model":"grok-4.5","usage":{"input_tokens":10}}})),
            ev("content_block_start", json!({"index":0,"content_block":{"type":"text","text":""}})),
            ev("content_block_delta", json!({"index":0,"delta":{"type":"text_delta","text":"he"}})),
            ev("content_block_delta", json!({"index":0,"delta":{"type":"text_delta","text":"llo"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("message_delta", json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}})),
            ev("message_stop", json!({})),
        ]
    }

    #[test]
    fn 纯文本流的完整事件序列() {
        let out = run(&text_stream());
        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // done 事件带的是**全量**文本,不是最后一段增量。
        let done = &out.iter().find(|(n, _)| n == "response.output_text.done").unwrap().1;
        assert_eq!(done["text"], "hello");
        let completed = &out.last().unwrap().1;
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["output"][0]["content"][0]["text"], "hello");
        assert_eq!(completed["response"]["usage"]["input_tokens"], json!(10));
        assert_eq!(completed["response"]["usage"]["output_tokens"], json!(3));
        assert_eq!(completed["response"]["model"], "grok-4.5");
    }

    #[test]
    fn 每帧都带自增的_sequence_number_与_type() {
        let out = run(&text_stream());
        for (i, (name, v)) in out.iter().enumerate() {
            assert_eq!(v["sequence_number"], json!(i as u64 + 1));
            assert_eq!(v["type"], json!(name.as_str()));
        }
    }

    #[test]
    fn 思考与工具各自成对收口() {
        let events = vec![
            ev("message_start", json!({"message":{}})),
            ev("content_block_start", json!({"index":0,"content_block":{"type":"thinking"}})),
            ev("content_block_delta",
               json!({"index":0,"delta":{"type":"thinking_delta","thinking":"想"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("content_block_start",
               json!({"index":1,"content_block":{"type":"tool_use","id":"call_1","name":"f"}})),
            ev("content_block_delta",
               json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}})),
            ev("content_block_stop", json!({"index":1})),
            ev("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events);
        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let final_out = &out.last().unwrap().1["response"]["output"];
        assert_eq!(final_out[0]["type"], "reasoning");
        assert_eq!(final_out[0]["summary"][0]["text"], "想");
        assert_eq!(final_out[1]["type"], "function_call");
        assert_eq!(final_out[1]["call_id"], "call_1");
        assert_eq!(final_out[1]["name"], "f");
        // arguments 必须是字符串,客户端要对它做 JSON.parse。
        assert_eq!(final_out[1]["arguments"], json!("{\"a\":1}"));
    }

    #[test]
    fn output_index_按登记顺序连续_跳过不认识的块() {
        let events = vec![
            ev("message_start", json!({"message":{}})),
            // 认不出的块:既不产出事件,也不能占掉一个 output_index。
            ev("content_block_start",
               json!({"index":0,"content_block":{"type":"server_tool_use","id":"s","name":"web"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("content_block_start", json!({"index":1,"content_block":{"type":"text"}})),
            ev("content_block_stop", json!({"index":1})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events);
        let added = &out.iter().find(|(n, _)| n == "response.output_item.added").unwrap().1;
        assert_eq!(added["output_index"], json!(0), "空洞会让客户端按下标取项时错位");
        assert_eq!(out.last().unwrap().1["response"]["output"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn 中途快照只含已收口的条目() {
        let mut s = ResponsesStreamOut::new("m");
        s.push(&ev("message_start", json!({"message":{}})));
        s.push(&ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})));
        s.push(&ev("content_block_delta",
                   json!({"index":0,"delta":{"type":"text_delta","text":"半截"}})));
        let frames = s.keepalive();
        assert_eq!(frames.len(), 1, "response 已开过,保活只该出一帧");
        let ka: Value = serde_json::from_str(&frames[0].data).unwrap();
        // 半截条目进快照 = 客户端把没写完的内容当成最终值。
        assert_eq!(ka["response"]["output"], json!([]));
        assert_eq!(ka["response"]["status"], "in_progress");
    }

    /// 首个上游事件迟到超过保活间隔时,保活**不能**抢在 `response.created` 前面 ——
    /// 那样客户端收到的第一帧是 `in_progress`,状态机直接倒序。
    #[test]
    fn 保活在_response_created_之前触发时先补开场() {
        let mut s = ResponsesStreamOut::new("m");
        let names: Vec<String> = s.keepalive().into_iter().map(|f| f.event.unwrap()).collect();
        assert_eq!(
            names,
            vec!["response.created", "response.in_progress", "response.in_progress"]
        );
        // 之后 message_start 不该再开第二遍。
        let again = s.push(&ev("message_start", json!({"message":{}})));
        assert!(again.is_empty(), "start 必须幂等");
    }

    /// 终止事件发过之后不得再发保活(调用方靠 `is_finished` 判断)。
    #[test]
    fn 终止之后停发保活() {
        let mut s = ResponsesStreamOut::new("m");
        for e in text_stream() {
            s.push(&e);
        }
        assert!(s.is_finished(), "调用方据此停发保活,否则 completed 之后还会冒出帧");
    }

    /// **传输 EOF ≠ 协议完成。** 上游半截断流(没发 `message_stop`)必须报失败,
    /// 补一个 `response.completed` 会把可检测的截断变成静默的假成功。
    #[test]
    fn 半截断流是_failed_而不是_completed() {
        let mut s = ResponsesStreamOut::new("m");
        for e in text_stream().into_iter().take(4) {
            s.push(&e);
        }
        let tail = s.finish();
        let names: Vec<&str> = tail.iter().map(|f| f.event.as_deref().unwrap()).collect();
        assert_eq!(names, vec!["response.failed"]);
        let v: Value = serde_json::from_str(&tail[0].data).unwrap();
        assert_eq!(v["response"]["status"], "failed");
        assert!(!v["response"]["error"].is_null());
    }

    /// 最终快照的 `output` 必须按 `output_index` 升序,**不是**按收口顺序 ——
    /// 否则客户端边收边重建出的结果与 `response.completed` 快照互相矛盾。
    #[test]
    fn 快照按_output_index_排序_而不是收口顺序() {
        // 块 0 先开、块 1 后开,但块 1 **先**收口。
        let events = vec![
            ev("message_start", json!({"message":{}})),
            ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})),
            ev("content_block_delta", json!({"index":0,"delta":{"type":"text_delta","text":"A"}})),
            ev("content_block_start",
               json!({"index":1,"content_block":{"type":"tool_use","id":"t","name":"f"}})),
            ev("content_block_stop", json!({"index":1})),
            ev("content_block_stop", json!({"index":0})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events);
        let output = &out.last().unwrap().1["response"]["output"];
        assert_eq!(output[0]["type"], "message", "output_index 0 的项必须排在前面");
        assert_eq!(output[0]["content"][0]["text"], "A");
        assert_eq!(output[1]["type"], "function_call");
    }

    /// 终态之后上游又冒出事件(协议违例)时不再产出任何帧。
    #[test]
    fn 终态之后的事件一律不再产出帧() {
        let mut s = ResponsesStreamOut::new("m");
        for e in text_stream() {
            s.push(&e);
        }
        assert!(s
            .push(&ev("content_block_start", json!({"index":9,"content_block":{"type":"text"}})))
            .is_empty());
        assert!(s.push(&ev("error", json!({"type":"error","error":{"message":"x"}}))).is_empty());
    }

    /// 只收到 `message_stop`(没有 message_start)也不能产出孤儿 `response.completed`。
    #[test]
    fn 孤儿_message_stop_也会先补开场() {
        let mut s = ResponsesStreamOut::new("m");
        let names: Vec<String> = s
            .push(&ev("message_stop", json!({})))
            .into_iter()
            .map(|f| f.event.unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["response.created", "response.in_progress", "response.completed"]
        );
    }

    /// 重复的块起始不能再占一个 output_index(否则留下永远空着的条目)。
    #[test]
    fn 重复的块起始被忽略() {
        let mut s = ResponsesStreamOut::new("m");
        s.push(&ev("message_start", json!({"message":{}})));
        s.push(&ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})));
        assert!(s
            .push(&ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})))
            .is_empty());
        s.push(&ev("content_block_delta", json!({"index":0,"delta":{"type":"text_delta","text":"A"}})));
        s.push(&ev("content_block_stop", json!({"index":0})));
        let last: Value = serde_json::from_str(
            &s.push(&ev("message_stop", json!({}))).last().unwrap().data,
        )
        .unwrap();
        let output = &last["response"]["output"];
        assert_eq!(output.as_array().unwrap().len(), 1, "不该多出一个空条目");
        assert_eq!(output[0]["content"][0]["text"], "A");
    }

    #[test]
    fn 收尾幂等() {
        let mut s = ResponsesStreamOut::new("m");
        for e in text_stream() {
            s.push(&e);
        }
        assert!(s.finish().is_empty());
    }

    #[test]
    fn max_tokens_是_incomplete_不是_completed() {
        let mut events = text_stream();
        events[5] = ev("message_delta", json!({"delta":{"stop_reason":"max_tokens"}}));
        let out = run(&events);
        let (name, v) = out.last().unwrap();
        // **事件名**也必须跟着状态走:绝大多数 SDK 按事件名分派,
        // `response.completed` 配 `status:"incomplete"` 会被当成功。
        assert_eq!(name, "response.incomplete");
        let r = &v["response"];
        assert_eq!(r["status"], "incomplete");
        assert_eq!(r["incomplete_details"]["reason"], "max_output_tokens");
    }

    /// 撞 `max_tokens` 时,还开着的 `function_call` **不能**被补 `done` ——
    /// 那等于告诉客户端「这个工具调用已完整,去执行吧」,而参数可能只有半截。
    #[test]
    fn 截断时不给半截工具调用补_done() {
        let events = vec![
            ev("message_start", json!({"message":{}})),
            ev("content_block_start",
               json!({"index":0,"content_block":{"type":"tool_use","id":"t","name":"rm"}})),
            ev("content_block_delta",
               json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/etc"}})),
            // 没有 content_block_stop —— 上游在块中途撞上了 max_tokens。
            ev("message_delta", json!({"delta":{"stop_reason":"max_tokens"}})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events);
        let names = names(&out);
        assert!(
            !names.contains(&"response.function_call_arguments.done"),
            "半截参数不得被标成完成:{names:?}"
        );
        assert!(!names.contains(&"response.output_item.done"));
        assert_eq!(*names.last().unwrap(), "response.incomplete");
        // 半截条目也不该进最终 output(它没收口)。
        assert_eq!(out.last().unwrap().1["response"]["output"], json!([]));
    }

    #[test]
    fn 错误发_response_failed_并带上错误码() {
        let mut s = ResponsesStreamOut::new("m");
        s.push(&ev("message_start", json!({"message":{}})));
        let out = s.push(&ev(
            "error",
            json!({"type":"error","error":{"type":"rate_limit_error","message":"慢点"}}),
        ));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event.as_deref(), Some("response.failed"));
        let v: Value = serde_json::from_str(&out[0].data).unwrap();
        assert_eq!(v["response"]["status"], "failed");
        assert_eq!(v["response"]["error"]["code"], "rate_limit_error");
        assert_eq!(v["response"]["error"]["message"], "慢点");
        // 失败已经是终态,后续 message_stop 不得再发 completed。
        assert!(s.push(&ev("message_stop", json!({}))).is_empty());
    }

    #[test]
    fn 上游没发_message_start_也不会漏掉_response_created() {
        let events = vec![
            ev("content_block_start", json!({"index":0,"content_block":{"type":"text"}})),
            ev("content_block_stop", json!({"index":0})),
            ev("message_stop", json!({})),
        ];
        let out = run(&events);
        assert_eq!(names(&out)[0], "response.created");
    }

    /// `Response` 对象必须回显客户端**实际发来的**参数。硬编码 `instructions:null` /
    /// `tools:[]` 就是网关在撒谎:客户端据这个快照做审计或重建下一轮时,拿到的是一份
    /// 与自己请求矛盾的状态。
    #[test]
    fn response_对象回显真实请求参数_而不是固定值() {
        // 这是**转换后的 Anthropic body**,worker 手上就有,信息一点没丢。
        let body = json!({
            "model": "m",
            "system": "be brief",
            "tools": [{"name":"f","description":"d","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"any","disable_parallel_tool_use": true},
        });
        let echo = RequestEcho::from_anthropic(&body);
        // 跑完整条流,取最后一帧(response.completed)的快照。
        let mut s = ResponsesStreamOut::new("m").with_echo(echo.clone());
        let mut last = Value::Null;
        for e in text_stream() {
            for f in s.push(&e) {
                last = serde_json::from_str(&f.data).unwrap();
            }
        }
        let r = &last["response"];
        assert_eq!(r["instructions"], "be brief");
        assert_eq!(r["tools"][0]["type"], "function");
        assert_eq!(r["tools"][0]["name"], "f");
        assert_eq!(r["tools"][0]["parameters"], json!({"type":"object"}));
        // Anthropic 的 any ↔ OpenAI 的 required;禁用并行 ↔ 允许并行取反。
        assert_eq!(r["tool_choice"], "required");
        assert_eq!(r["parallel_tool_calls"], json!(false));

        // 非流式走同一份 echo。
        let msg = json!({"role":"assistant","content":[{"type":"text","text":"hi"}],
                         "stop_reason":"end_turn"});
        let f = fold_response(&msg, "m", &echo);
        assert_eq!(f["instructions"], "be brief");
        assert_eq!(f["tool_choice"], "required");
        assert_eq!(f["parallel_tool_calls"], json!(false));
    }

    #[test]
    fn 请求什么都没带时回显的是_没要_而不是乱猜() {
        let echo = RequestEcho::from_anthropic(&json!({"model":"m","messages":[]}));
        let f = fold_response(&json!({"role":"assistant","content":[]}), "m", &echo);
        assert_eq!(f["instructions"], Value::Null);
        assert_eq!(f["tools"], json!([]));
        assert_eq!(f["tool_choice"], "auto");
        assert_eq!(f["parallel_tool_calls"], json!(true));
    }

    #[test]
    fn 点名工具与数组形态的_system_都能回显() {
        let echo = RequestEcho::from_anthropic(&json!({
            "system": [{"type":"text","text":"A"},{"type":"text","text":"B"}],
            "tools": [{"name":"g","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool","name":"g"},
        }));
        let f = fold_response(&json!({"role":"assistant","content":[]}), "m", &echo);
        assert_eq!(f["instructions"], "A\n\nB");
        assert_eq!(f["tool_choice"], json!({"type":"function","name":"g"}));
    }

    #[test]
    fn 非流式折叠与流式给出同样的输出与用量() {
        let msg = json!({
            "role":"assistant","model":"grok-4.5",
            "content":[{"type":"text","text":"hello"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":10,"output_tokens":3},
        });
        let r = fold_response(&msg, "m", &RequestEcho::default());
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["model"], "grok-4.5");
        assert_eq!(r["output"][0]["content"][0]["text"], "hello");
        assert_eq!(r["usage"]["input_tokens"], json!(10));
        assert_eq!(r["usage"]["output_tokens"], json!(3));
    }

    #[test]
    fn 非流式的工具与思考也进_output() {
        let msg = json!({
            "role":"assistant","content":[
                {"type":"thinking","thinking":"想"},
                {"type":"tool_use","id":"call_1","name":"f","input":{"a":1}}],
            "stop_reason":"tool_use",
        });
        let r = fold_response(&msg, "m", &RequestEcho::default());
        assert_eq!(r["output"][0]["type"], "reasoning");
        assert_eq!(r["output"][1]["type"], "function_call");
        assert_eq!(r["output"][1]["arguments"], json!("{\"a\":1}"));
        assert_eq!(r["status"], "completed");
    }
}
