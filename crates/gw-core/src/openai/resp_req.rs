//! OpenAI **Responses** 请求 → Anthropic Messages 请求。
//!
//! 与 ChatCompletions 的差别都在顶层:`instructions` 顶掉 system 消息、
//! `input` 顶掉 `messages`、工具调用与工具返回是 `input` 里**平级的条目**
//! 而不是挂在消息上的字段。零件仍复用 [`super::inbound`]。

use serde_json::{json, Map, Value};

use super::inbound::{
    content_to_blocks, convert_tool_choice, convert_tools, copy_sampling, effort_to_thinking,
    max_tokens_of, reconcile_tool_choice, session_metadata, tool_choice_forbids_tools,
    tool_output_to_content,
    ConvertError, SystemAccum,
};
use super::{Converted, Wire};

/// `POST /v1/responses` 的请求体 → Anthropic Messages body。
pub fn convert_request(src: &Value) -> Result<Converted, ConvertError> {
    let obj = src
        .as_object()
        .ok_or_else(|| ConvertError::new("请求体必须是 JSON 对象"))?;

    // 有状态会话我方不支持,且**不能装作支持**:静默忽略它等于把「续上一轮」变成
    // 「重新开一轮」,客户端拿到的是一个上下文凭空丢失的回答,查都没法查。
    if obj
        .get("previous_response_id")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
    {
        return Err(ConvertError::at(
            "previous_response_id",
            "本网关是无状态反代,不保存历史响应;请把完整对话放进 input 后重试",
        ));
    }

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| ConvertError::at("model", "缺少 model"))?;

    let mut system = SystemAccum::default();
    if let Some(s) = obj.get("instructions").and_then(Value::as_str) {
        system.push(s);
    }

    let mut messages: Vec<Value> = Vec::new();
    match obj.get("input") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => push_blocks(
            &mut messages,
            "user",
            content_to_blocks(Some(&Value::String(s.clone()))),
        ),
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                convert_item(i, item, &mut system, &mut messages)?;
            }
        }
        Some(_) => return Err(ConvertError::at("input", "input 必须是字符串或条目数组")),
    }

    if messages.is_empty() {
        return Err(ConvertError::at(
            "input",
            "input 里没有任何可转换的内容(只有 instructions 也不行,上游需要至少一条对话消息)",
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
        .get("reasoning")
        .and_then(|r| r.get("effort"))
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

    Ok(Converted {
        body: Value::Object(out),
        dropped_tools: tool_set.dropped,
        wire: Wire::OpenAiResponses,
    })
}

fn convert_item(
    i: usize,
    item: &Value,
    system: &mut SystemAccum,
    messages: &mut Vec<Value>,
) -> Result<(), ConvertError> {
    // `type` 可缺席:裸 `{role, content}` 是合法简写。
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(if item.get("role").is_some() { "message" } else { "" });

    match kind {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    for b in content_to_blocks(item.get("content")) {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            system.push(t);
                        }
                    }
                }
                "user" | "assistant" => {
                    push_blocks(messages, role, content_to_blocks(item.get("content")))
                }
                other => {
                    return Err(ConvertError::at(
                        format!("input[{i}].role"),
                        format!("不认识的 role: {other:?}"),
                    ))
                }
            }
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                ConvertError::at(format!("input[{i}].name"), "function_call 缺少 name")
            })?;
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or(name);
            let raw = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let input = match serde_json::from_str::<Value>(raw) {
                Ok(v) if v.is_object() => v,
                _ if raw.trim().is_empty() => json!({}),
                // 同 chat_req:宁可参数不精确,也不能丢掉调用把后面的输出变成孤儿块。
                _ => json!({ "_raw": raw }),
            };
            push_blocks(
                messages,
                "assistant",
                vec![json!({"type":"tool_use","id":id,"name":name,"input":input})],
            );
        }
        "function_call_output" => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ConvertError::at(
                        format!("input[{i}].call_id"),
                        "function_call_output 缺少 call_id,无法与工具调用配对",
                    )
                })?;
            push_blocks(
                messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": tool_output_to_content(item.get("output")),
                })],
            );
        }
        // 客户端回传的上一轮推理条目:**收下即丢**。
        //
        // cursor 通道拿不到可回放的加密 CoT(见记忆 caio-thinking-blob-extraction),
        // 硬把 summary 当 thinking 块塞回去会得到一个**没有签名**的思考块 ——
        // Anthropic 家族上游对无签名 thinking 是直接拒收的,比丢掉它糟得多。
        "reasoning" => {}
        // 引用服务端存量条目:与 previous_response_id 同理,我方无状态,认不了。
        "item_reference" => {
            return Err(ConvertError::at(
                format!("input[{i}]"),
                "本网关是无状态反代,不支持 item_reference;请把条目内容直接放进 input",
            ))
        }
        other => {
            return Err(ConvertError::at(
                format!("input[{i}].type"),
                format!("不认识的 input 条目类型: {other:?}"),
            ))
        }
    }
    Ok(())
}

/// 同 [`super::chat_req`] 的合并规则:相邻同角色并入一条,空块列表跳过。
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
    fn 字符串_input_当成一条_user() {
        let out = conv(json!({"model": "m", "input": "hi"}));
        assert_eq!(
            out["messages"],
            json!([{"role":"user","content":[{"type":"text","text":"hi"}]}])
        );
        assert_eq!(out["stream"], json!(false));
    }

    #[test]
    fn instructions_成为顶层_system() {
        let out = conv(json!({"model":"m","instructions":"be brief","input":"hi"}));
        assert_eq!(out["system"], json!("be brief"));
    }

    #[test]
    fn input_里的_system_消息与_instructions_按序拼接() {
        let out = conv(json!({
            "model":"m","instructions":"A",
            "input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"B"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
            ]
        }));
        assert_eq!(out["system"], json!("A\n\nB"));
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn 裸_role_content_条目也认() {
        let out = conv(json!({"model":"m","input":[{"role":"user","content":"hi"}]}));
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn 工具往返_function_call_与_output() {
        let out = conv(json!({
            "model":"m",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
                {"type":"function_call","call_id":"call_1","name":"f","arguments":"{\"a\":1}"},
                {"type":"function_call_output","call_id":"call_1","output":"done"},
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[1],
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"call_1","name":"f","input":{"a":1}}]})
        );
        assert_eq!(
            msgs[2],
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_1",
                 "content":[{"type":"text","text":"done"}]}]})
        );
    }

    #[test]
    fn reasoning_条目被丢弃且不影响消息序列() {
        let out = conv(json!({
            "model":"m",
            "input":[
                {"type":"message","role":"user","content":"q"},
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"想了想"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"a"}]},
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"][0]["text"], "a");
    }

    #[test]
    fn 有状态字段一律明确拒绝_不静默忽略() {
        let e = convert_request(&json!({
            "model":"m","input":"hi","previous_response_id":"resp_abc"
        }))
        .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("previous_response_id"));

        let e = convert_request(&json!({
            "model":"m","input":[{"type":"item_reference","id":"x"}]
        }))
        .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("input[0]"));

        // null / 空串 = 没设,不该拦。
        assert!(convert_request(&json!({
            "model":"m","input":"hi","previous_response_id":null
        }))
        .is_ok());
    }

    #[test]
    fn reasoning_effort_与工具一起转() {
        let out = conv(json!({
            "model":"m","input":"q",
            "reasoning":{"effort":"low","summary":"auto"},
            "tools":[{"type":"function","name":"f","parameters":{"type":"object"}}],
            "tool_choice":"auto",
            "max_output_tokens": 1234,
        }));
        assert_eq!(out["thinking"], json!({"type":"enabled","budget_tokens":4096}));
        assert_eq!(out["tools"], json!([{"name":"f","input_schema":{"type":"object"}}]));
        assert_eq!(out["tool_choice"], json!({"type":"auto"}));
        assert_eq!(out["max_tokens"], json!(1234));
    }

    #[test]
    fn 空_input_与坏条目类型都点名报错() {
        let e = convert_request(&json!({"model":"m","input":[]})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("input"));

        let e = convert_request(&json!({"model":"m","input":[{"type":"web_search_call"}]}))
            .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("input[0].type"));

        let e =
            convert_request(&json!({"model":"m","input":[{"type":"function_call_output","output":"x"}]}))
                .unwrap_err();
        assert_eq!(e.param.as_deref(), Some("input[0].call_id"));
    }

    #[test]
    fn wire_是_responses() {
        let c = convert_request(&json!({"model":"m","input":"hi","stream":true})).unwrap();
        assert_eq!(c.wire, Wire::OpenAiResponses);
        assert_eq!(c.body["stream"], json!(true));
    }
}
