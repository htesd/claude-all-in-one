//! 真实 Kiro 端到端冒烟(手动跑,顺序、低并发):
//! `cargo run -p gw-kiro --example e2e_pro`
//!
//! 读取 `data/test-cred-pro.json`(gitignore,**不含任何硬编码密钥**),依次:
//! 1. refresh_auth(social)→ 拿 access_token + profileArn,并把轮换后的 refresh_token 回写文件;
//! 2. 一次纯文本 chat,验证文本流式 SSE 时序;
//! 3. 一次 tool_use chat,验证模型工具调用能转成 tool_use 块抵达客户端(①B 真实验证)。
//!
//! 注意:会消耗真实额度,顺序执行不打并发(账号防封)。无凭证文件时直接退出。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::provider::{CallCtx, ChatRequest, Provider, StreamItem};
use gw_kiro::KiroProvider;
use serde_json::json;

const CRED_PATH: &str = "data/test-cred-pro.json";

#[tokio::main]
async fn main() {
    let raw = match std::fs::read_to_string(CRED_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("缺少凭证 {CRED_PATH}: {e};跳过 e2e。");
            return;
        }
    };
    let cred: serde_json::Value = serde_json::from_str(&raw).expect("凭证 JSON 解析失败");
    let refresh = cred["refreshToken"].as_str().unwrap_or("").to_string();
    let access = cred["accessToken"].as_str().unwrap_or("").to_string();
    assert!(!refresh.is_empty(), "凭证缺 refreshToken");

    let mut extra = BTreeMap::new();
    extra.insert("refresh_token".into(), json!(refresh));
    extra.insert("access_token".into(), json!(access));
    extra.insert("region".into(), json!("us-east-1"));
    let account = Account {
        account_id: "kiro-pro-test".into(),
        provider: "kiro".into(),
        max_concurrency: 1,
        disabled: false,
        extra,
    };

    let client = reqwest::Client::builder()
        .build()
        .expect("build reqwest client");
    let provider = KiroProvider::new(client);

    // ---- 1. refresh ----
    println!("[1] refresh_auth(social)...");
    let account = match provider.refresh_auth(&account).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("    refresh 失败: {e}");
            return;
        }
    };
    let has_arn = account.extra_str("profile_arn").is_some();
    let access_len = account.extra_str("access_token").map(|s| s.len()).unwrap_or(0);
    println!("    ok: profile_arn={has_arn} access_token_len={access_len}");

    // 回写轮换后的 refresh_token / access_token(rolling,旧的可能失效)。
    if let (Some(rt), Some(at)) = (account.extra_str("refresh_token"), account.extra_str("access_token"))
    {
        let updated = json!({
            "accessToken": at,
            "refreshToken": rt,
            "clientId": cred["clientId"].as_str().unwrap_or(""),
            "clientSecret": cred["clientSecret"].as_str().unwrap_or(""),
        });
        if let Err(e) = std::fs::write(CRED_PATH, serde_json::to_string_pretty(&updated).unwrap()) {
            eprintln!("    警告:回写凭证失败: {e}");
        } else {
            println!("    已回写轮换后的凭证");
        }
    }

    let acct = Arc::new(account);

    // ---- 2. 纯文本 ----
    println!("[2] text chat...");
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role": "user", "content": "Reply with exactly: E2E-TEXT-OK"}]
    });
    run_and_report(&provider, ChatRequest::from_anthropic_body(body), &acct, "1", "text").await;

    // ---- 3. tool_use(①B 真实验证)----
    println!("[3] tool_use chat...");
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 256,
        "stream": true,
        "tools": [{
            "name": "get_weather",
            "description": "Get the current weather for a given city",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string", "description": "City name"}},
                "required": ["city"]
            }
        }],
        "messages": [{"role": "user",
            "content": "Use the get_weather tool to check the weather in San Francisco. You must call the tool."}]
    });
    run_and_report(&provider, ChatRequest::from_anthropic_body(body), &acct, "2", "tool").await;

    // ---- 4. 非流式折叠(框架:provider 产流 → gw-core 折叠成单个 Messages JSON)----
    println!("[4] non-stream fold...");
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": false,
        "messages": [{"role": "user", "content": "Reply with exactly: E2E-FOLD-OK"}]
    });
    run_nonstream_fold(&provider, ChatRequest::from_anthropic_body(body), &acct, "3").await;
}

/// 抽干真实 Kiro 流 → 用 gw-core 折叠成单个非流式 Messages JSON,验证折叠对真实线缆生效。
async fn run_nonstream_fold(
    provider: &KiroProvider,
    req: ChatRequest,
    account: &Arc<Account>,
    sess: &str,
) {
    use gw_core::provider::SseEvent;
    let ctx = CallCtx {
        account: account.clone(),
        session_id: format!("e2e-sess-{sess}"),
        cache_key: format!("e2e-{sess}"),
    };
    let mut stream = match provider.chat(req, &ctx).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("    [fold] chat 启动失败: {e}");
            return;
        }
    };
    let mut events: Vec<SseEvent> = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => events.push(ev),
            Ok(StreamItem::Usage(_)) => {}
            Ok(StreamItem::UpstreamCut) => eprintln!("    [fold] upstream_cut(静默掐流信号)"),
            Err(e) => {
                eprintln!("    [fold] 流错误: {e}");
                return;
            }
        }
    }
    match gw_core::fold::fold_sse_to_message(&events) {
        Ok(msg) => {
            let text = msg["content"]
                .as_array()
                .and_then(|a| a.iter().find(|b| b["type"] == "text"))
                .and_then(|b| b["text"].as_str())
                .unwrap_or("");
            println!(
                "    [fold] role={} stop_reason={} text={:?} usage_out={}",
                msg["role"], msg["stop_reason"], text, msg["usage"]["output_tokens"]
            );
        }
        Err(e) => eprintln!("    [fold] 折叠失败: {e}"),
    }
}

async fn run_and_report(
    provider: &KiroProvider,
    req: ChatRequest,
    account: &Arc<Account>,
    sess: &str,
    label: &str,
) {
    let ctx = CallCtx {
        account: account.clone(),
        session_id: format!("e2e-sess-{sess}"),
        cache_key: format!("e2e-{sess}"),
    };
    let mut stream = match provider.chat(req, &ctx).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("    [{label}] chat 启动失败: {e}");
            return;
        }
    };
    let mut events: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut tool_json = String::new();
    let mut stop_reason = String::new();
    let mut err: Option<String> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => {
                events.push(ev.event.clone());
                match ev.event.as_str() {
                    "content_block_start" if ev.data["content_block"]["type"] == "tool_use" => {
                        tool_names.push(
                            ev.data["content_block"]["name"].as_str().unwrap_or("").to_string(),
                        );
                    }
                    "content_block_delta" => {
                        if let Some(t) = ev.data["delta"]["text"].as_str() {
                            text.push_str(t);
                        }
                        if let Some(pj) = ev.data["delta"]["partial_json"].as_str() {
                            tool_json.push_str(pj);
                        }
                    }
                    "message_delta" => {
                        if let Some(sr) = ev.data["delta"]["stop_reason"].as_str() {
                            stop_reason = sr.to_string();
                        }
                    }
                    _ => {}
                }
            }
            Ok(StreamItem::Usage(u)) => {
                println!(
                    "    [{label}] usage in={} out={} cache_read={}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens
                );
            }
            Ok(StreamItem::UpstreamCut) => eprintln!("    [{label}] upstream_cut(静默掐流信号)"),
            Err(e) => err = Some(e.to_string()),
        }
    }
    println!("    [{label}] events={events:?}");
    println!("    [{label}] stop_reason={stop_reason} text={text:?}");
    if !tool_names.is_empty() {
        println!("    [{label}] tool_use names={tool_names:?} input_json={tool_json}");
    }
    if let Some(e) = err {
        eprintln!("    [{label}] 流错误: {e}");
    }
}
