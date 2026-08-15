//! OpenAI **ChatCompletions** 请求 → Anthropic Messages 请求。
//!
//! 这是 NewAPI 之类中转真正在说的协议。转换只做形状搬运,不做任何策略判断
//! (模型名归一、能力过滤都在下游 provider 里做,见 `gw-cursor` 的 `resolve_cursor_model`)。

use serde_json::{json, Map, Value};

use super::inbound::{
    content_to_blocks, convert_tool_choice, convert_tools, copy_sampling, effort_to_thinking,
    max_tokens_of, reconcile_tool_choice, session_metadata, tool_choice_forbids_tools,
    tool_output_to_content,
    ConvertError, SystemAccum,
};
use super::{Converted, Wire};

/// `POST /v1/chat/completions` 的请求体 → Anthropic Messages body。
pub fn convert_request(src: &Value) -> Result<Converted, ConvertError> {
    let obj = src
        .as_object()
        .ok_or_else(|| ConvertError::new("请求体必须是 JSON 对象"))?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| ConvertError::at("model", "缺少 model"))?;

    let src_msgs = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ConvertError::at("messages", "缺少 messages 数组"))?;

    let mut system = SystemAccum::default();
    let mut messages: Vec<Value> = Vec::with_capacity(src_msgs.len());

    for (i, m) in src_msgs.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                for b in content_to_blocks(m.get("content")) {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        system.push(t);
                    }
                }
            }
            "user" => push_blocks(&mut messages, "user", content_to_blocks(m.get("content"))),
            "assistant" => {
                let mut blocks = content_to_blocks(m.get("content"));
                blocks.extend(tool_calls_to_blocks(m.get("tool_calls")));
                // 旧版(已弃用但仍合法)的单个 `function_call`。NewAPI 的老客户端还在发。
                // 不认它的后果不是「少一个字段」:调用块被丢掉,后面那条
                // `role:"function"` 的返回值就成了孤儿 tool_result,上游拒整条请求
                // (对抗评审 Minimalist#7)。
                if let Some(fc) = m.get("function_call") {
                    let one = Value::Array(vec![fc.clone()]);
                    blocks.extend(tool_calls_to_blocks(Some(&one)));
                }
                push_blocks(&mut messages, "assistant", blocks);
            }
            // 工具返回值在 Anthropic 侧是**下一条 user 消息里的 tool_result 块**。
            // 连续多条 tool 消息会被 `push_blocks` 合并进同一条 user 消息 —— 这正是
            // 并行工具调用的正确形态(一轮里 N 个 tool_result 必须同处一条消息)。
            "tool" | "function" => {
                let id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .or_else(|| m.get("id").and_then(Value::as_str))
                    // 旧版 `role:"function"` 用 `name` 配对(那时还没有 call id)。
                    // 与上面 assistant 的 legacy `function_call` 对齐:那边也拿 name 当 id。
                    .or_else(|| m.get("name").and_then(Value::as_str))
                    .unwrap_or_default();
                if id.is_empty() {
                    return Err(ConvertError::at(
                        format!("messages[{i}].tool_call_id"),
                        "role=tool 的消息缺少 tool_call_id(旧版 function 消息则缺 name),\
                         无法与工具调用配对",
                    ));
                }
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": tool_output_to_content(m.get("content")),
                });
                push_blocks(&mut messages, "user", vec![block]);
            }
            other => {
                return Err(ConvertError::at(
                    format!("messages[{i}].role"),
                    format!("不认识的 role: {other:?}"),
                ))
            }
        }
    }

    if messages.is_empty() {
        return Err(ConvertError::at(
            "messages",
            "messages 里没有任何可转换的内容(只有 system 也不行,上游需要至少一条对话消息)",
        ));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("max_tokens".into(), json!(max_tokens_of(obj)));
    out.insert("messages".into(), Value::Array(messages));
    if let Some(s) = system.finish() {
        out.insert("system".into(), s);
    }
    let tool_set = convert_tools(obj.get("tools"));
    // `tool_choice:"none"` 要**真的执行**:撤掉 tools 才管用,光写 tool_choice 是摆设
    // (当前上游只读 tools)。见 `tool_choice_forbids_tools`。
    let forbid = tool_choice_forbids_tools(obj.get("tool_choice"));
    let has_tools = !tool_set.tools.is_empty() && !forbid;
    if has_tools {
        out.insert("tools".into(), Value::Array(tool_set.tools));
    }
    let parallel = obj.get("parallel_tool_calls").and_then(Value::as_bool);
    // tools 与 tool_choice 必须一起判(见 reconcile_tool_choice):独立转换会造出
    // 「要求必须用工具却没有 tools」这种上游必拒的组合。
    let choice = convert_tool_choice(obj.get("tool_choice"), parallel);
    if let Some(tc) = reconcile_tool_choice(has_tools, choice, "tool_choice")? {
        out.insert("tool_choice".into(), tc);
    }
    if let Some(t) = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .and_then(effort_to_thinking)
    {
        out.insert("thinking".into(), t);
    }
    copy_sampling(obj, &mut out);
    // 终端用户标识 → `metadata.user_id`。cursor 的会话派生**优先**读它,填上就绕开了
    // 「纯内容哈希」那条会把不同用户合并的脆弱路径(见 `session_metadata`)。
    if let Some(md) = session_metadata(obj) {
        out.insert("metadata".into(), md);
    }
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    out.insert("stream".into(), json!(stream));

    // `stream_options.include_usage`:决定流末尾要不要补那条 `choices:[]` 的用量帧。
    // 只有客户端明确要了才发 —— 严格按 choices[0] 解析的客户端会被空 choices 噎住。
    let include_usage = obj
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(Converted {
        body: Value::Object(out),
        dropped_tools: tool_set.dropped,
        wire: Wire::OpenAiChat { include_usage },
    })
}

/// assistant 的 `tool_calls[]` → Anthropic `tool_use` 块。
///
/// `arguments` 是**字符串化的 JSON**(OpenAI 如此),Anthropic 的 `input` 要对象。
/// 解析失败时不丢掉这次调用,而是把原文塞进 `{"_raw": "..."}`:上下文里少一个工具调用
/// 会让后面的 `tool_result` 变成孤儿块,上游直接拒整条请求 —— 比参数不精确糟得多。
fn tool_calls_to_blocks(calls: Option<&Value>) -> Vec<Value> {
    let Some(arr) = calls.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            let f = c.get("function").unwrap_or(c);
            let name = f.get("name").and_then(Value::as_str)?;
            let id = c.get("id").and_then(Value::as_str).unwrap_or(name);
            let raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let input = match serde_json::from_str::<Value>(raw) {
                Ok(v) if v.is_object() => v,
                _ if raw.trim().is_empty() => json!({}),
                _ => json!({ "_raw": raw }),
            };
            Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
        })
        .collect()
}

/// 追加一条消息;**同角色相邻则并入上一条**。
///
/// Anthropic 要求 user/assistant 严格交替,而 OpenAI 侧连续同角色是常态
/// (并行工具的多条 `role:"tool"`、被拆开的多段 user)。空块列表直接跳过 ——
/// 空 content 的消息是上游必拒的形态(见 worker 的 `is_empty_message_content`)。
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(v: Value) -> Value {
        convert_request(&v).unwrap().body
    }

    #[test]
    fn 最小请求_补上_max_tokens_与_stream() {
        let out = conv(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(out["model"], "grok-4.5");
        assert_eq!(out["max_tokens"], json!(super::super::inbound::DEFAULT_MAX_TOKENS));
        assert_eq!(out["stream"], json!(false));
        assert_eq!(out["messages"], json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]));
        assert!(out.get("system").is_none());
    }

    #[test]
    fn system_与_developer_都提升到顶层并按序拼接() {
        let out = conv(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "S1"},
                {"role": "user", "content": "q"},
                {"role": "developer", "content": "S2"},
            ]
        }));
        assert_eq!(out["system"], json!("S1\n\nS2"));
        // 提升之后 messages 里不该再留下它们。
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn 相邻同角色消息被合并_满足交替要求() {
        let out = conv(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
                {"role": "assistant", "content": "c"},
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn 工具往返_一轮完整转换() {
        let out = conv(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "查天气"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "weather", "arguments": "{\"city\":\"SH\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "晴"},
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[1],
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"call_1","name":"weather","input":{"city":"SH"}}]})
        );
        assert_eq!(
            msgs[2],
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_1",
                 "content":[{"type":"text","text":"晴"}]}]})
        );
    }

    #[test]
    fn 并行工具的多条_tool_消息并进同一条_user() {
        let out = conv(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "tool_calls": [
                    {"id":"a","function":{"name":"f","arguments":"{}"}},
                    {"id":"b","function":{"name":"g","arguments":"{}"}}]},
                {"role": "tool", "tool_call_id": "a", "content": "1"},
                {"role": "tool", "tool_call_id": "b", "content": "2"},
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "两条 tool 必须合成一条 user,否则上游拒收");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn 参数不是合法_json_时保留原文而不是丢掉这次调用() {
        // 丢掉 tool_use 会让后面的 tool_result 成为孤儿块,上游拒整条请求。
        let out = conv(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "tool_calls": [
                    {"id":"x","function":{"name":"f","arguments":"{不是json"}}]},
            ]
        }));
        let tu = &out["messages"][1]["content"][0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(tu["input"], json!({"_raw": "{不是json"}));
    }

    #[test]
    fn 工具与工具选择一起转() {
        let out = conv(json!({
            "model": "m",
            "messages": [{"role":"user","content":"q"}],
            "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        }));
        assert_eq!(out["tools"], json!([{"name":"f","input_schema":{"type":"object"}}]));
        assert_eq!(out["tool_choice"], json!({"type":"any","disable_parallel_tool_use":true}));
    }

    /// `tool_choice:"none"` 必须**真的**生效。当前上游只读 `tools`,所以唯一能兑现
    /// 这条约束的办法是把 tools 整个撤掉 —— 光写 tool_choice 是摆设。
    #[test]
    fn tool_choice_none_会撤掉_tools_而不是只写个字段() {
        let out = conv(json!({
            "model":"m",
            "messages":[{"role":"user","content":"q"}],
            "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "tool_choice":"none",
        }));
        assert!(out.get("tools").is_none(), "不撤 tools 的话模型照样能调工具");
        // tools 撤掉后 tool_choice 也没意义(留着 Anthropic 会拒)。
        assert!(out.get("tool_choice").is_none());
        // 对照:auto 时 tools 必须在。
        let out = conv(json!({
            "model":"m",
            "messages":[{"role":"user","content":"q"}],
            "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "tool_choice":"auto",
        }));
        assert!(out.get("tools").is_some());
    }

    /// 旧版 `function_call` / `role:"function"` 的完整往返。丢掉调用块会让后面的
    /// 返回值变成孤儿 `tool_result`,上游拒整条请求。
    #[test]
    fn 旧版_function_call_往返不被打回() {
        let c = convert_request(&json!({
            "model":"m",
            "messages":[
                {"role":"user","content":"q"},
                {"role":"assistant","function_call":{"name":"f","arguments":"{\"a\":1}"}},
                {"role":"function","name":"f","content":"ok"},
            ]
        }))
        .unwrap();
        let msgs = c.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // 调用块在,且 id 用 name(旧版没有 call id)。
        assert_eq!(
            msgs[1],
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"f","name":"f","input":{"a":1}}]})
        );
        // 返回值按同一个 id 配对 —— 不是孤儿。
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "f");
    }

    #[test]
    fn 托管工具被丢掉时会记在_dropped_tools_里() {
        let c = convert_request(&json!({
            "model":"m",
            "messages":[{"role":"user","content":"q"}],
            "tools":[{"type":"web_search"},
                     {"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
        }))
        .unwrap();
        assert_eq!(c.dropped_tools, vec!["web_search".to_string()]);
    }

    #[test]
    fn reasoning_effort_映射成_thinking() {
        let out = conv(json!({
            "model": "m", "reasoning_effort": "high",
            "messages": [{"role":"user","content":"q"}]
        }));
        assert_eq!(out["thinking"], json!({"type":"enabled","budget_tokens":32768}));
    }

    #[test]
    fn include_usage_被带进_wire() {
        let c = convert_request(&json!({
            "model":"m","stream":true,"stream_options":{"include_usage":true},
            "messages":[{"role":"user","content":"q"}]
        }))
        .unwrap();
        assert_eq!(c.wire, Wire::OpenAiChat { include_usage: true });
        assert_eq!(c.body["stream"], json!(true));

        let c2 = convert_request(&json!({
            "model":"m","stream":true,"messages":[{"role":"user","content":"q"}]
        }))
        .unwrap();
        assert_eq!(c2.wire, Wire::OpenAiChat { include_usage: false });
    }

    #[test]
    fn 缺字段与坏_role_明确报错并点名字段() {
        let e = convert_request(&json!({"messages": []})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("model"));

        let e = convert_request(&json!({"model": "m"})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages"));

        let e = convert_request(&json!({"model":"m","messages":[{"role":"ghost","content":"x"}]}))
            .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages[0].role"));

        let e = convert_request(&json!({"model":"m","messages":[{"role":"tool","content":"x"}]}))
            .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages[0].tool_call_id"));
    }

    #[test]
    fn 只有_system_也要报错_而不是发一条空对话上去() {
        let e = convert_request(&json!({
            "model":"m","messages":[{"role":"system","content":"s"}]
        }))
        .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages"));
    }
}
