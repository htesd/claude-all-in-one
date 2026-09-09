//! BOT 实弹验收：stdin 读取 Account JSON，不刷新、不打印凭据。
//! 固定五轮：文本、多轮记忆、会话隔离、工具调用、工具结果续轮。
use futures::StreamExt;
use gw_core::{
    account::Account,
    provider::{CallCtx, ChatRequest, Provider, StreamItem},
};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};

async fn ask(
    provider: &gw_cursor::CursorProvider,
    ctx: &CallCtx,
    messages: &[Value],
    tools: Value,
) -> anyhow::Result<Value> {
    let mut body =
        json!({"model":"grok_bot_auto","stream":true,"max_tokens":512,"messages":messages});
    if !tools.is_null() {
        body["tools"] = tools;
    }
    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mut stream = provider
            .chat(ChatRequest::from_anthropic_body(body), ctx)
            .await?;
        let mut events = Vec::new();
        let mut usage = false;
        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::Sse(event) => events.push(event),
                StreamItem::Usage(_) => usage = true,
                StreamItem::UpstreamCut => {}
            }
        }
        anyhow::ensure!(usage, "缺少最终用量");
        anyhow::ensure!(
            events.iter().any(|e| e.event == "message_stop"),
            "缺少 message_stop"
        );
        gw_core::fold::fold_sse_to_message(&events).map_err(|_| anyhow::anyhow!("SSE 折叠失败"))
    })
    .await??;
    println!(
        "{}",
        json!({"elapsed_ms":start.elapsed().as_millis(),"response":result})
    );
    Ok(result)
}
fn text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let mut account: Account = serde_json::from_str(&raw)?;
    account.extra.insert("pool".into(), json!("bot"));
    let account = Arc::new(account);
    let provider = gw_cursor::CursorProvider::new(Default::default());
    let ctx = CallCtx {
        account: account.clone(),
        session_id: uuid::Uuid::new_v4().to_string(),
        cache_key: String::new(),
    };
    let marker = format!("CAIO_MEMORY_{}", uuid::Uuid::new_v4().simple());
    let mut messages = vec![
        json!({"role":"user","content":format!("Remember the passphrase {marker}. Reply exactly STORED. Do not use tools.")}),
    ];
    let a = ask(&provider, &ctx, &messages, Value::Null).await?;
    anyhow::ensure!(text(&a).contains("STORED"), "首轮正文不匹配");
    messages.push(json!({"role":"assistant","content":a["content"]}));
    messages.push(
        json!({"role":"user","content":"What is the passphrase? Reply with only the passphrase."}),
    );
    let a = ask(&provider, &ctx, &messages, Value::Null).await?;
    anyhow::ensure!(text(&a).contains(&marker), "多轮记忆丢失");
    let isolated = CallCtx {
        account: account.clone(),
        session_id: uuid::Uuid::new_v4().to_string(),
        cache_key: String::new(),
    };
    let a=ask(&provider,&isolated,&[json!({"role":"user","content":"If a passphrase was previously given in this conversation, repeat it. Otherwise reply exactly CLEAN. Do not use tools."})],Value::Null).await?;
    anyhow::ensure!(
        text(&a).contains("CLEAN") && !text(&a).contains(&marker),
        "会话隔离失败"
    );
    let tools = json!([{"name":"lookup_probe","description":"Return the current probe code. You must call this to get the code.","input_schema":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}}]);
    let toolctx = CallCtx {
        account,
        session_id: uuid::Uuid::new_v4().to_string(),
        cache_key: String::new(),
    };
    let mut messages = vec![
        json!({"role":"user","content":"Call lookup_probe with key=check. Do not guess the result. Return the code after receiving the tool result."}),
    ];
    let a = ask(&provider, &toolctx, &messages, tools.clone()).await?;
    let call = a["content"]
        .as_array()
        .and_then(|b| {
            b.iter()
                .find(|b| b["type"] == "tool_use" && b["name"] == "lookup_probe")
        })
        .ok_or_else(|| anyhow::anyhow!("未返回工具调用"))?;
    anyhow::ensure!(a["stop_reason"] == "tool_use", "工具 stop_reason 不匹配");
    let tool_marker = format!("TOOL_RESULT_{}", uuid::Uuid::new_v4().simple());
    messages.push(json!({"role":"assistant","content":a["content"]}));
    messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call["id"],"content":tool_marker}]}));
    let a = ask(&provider, &toolctx, &messages, tools).await?;
    anyhow::ensure!(text(&a).contains(&tool_marker), "工具结果续轮失败");
    println!("SANDCHAT_E2E_PASS 5/5");
    Ok(())
}
