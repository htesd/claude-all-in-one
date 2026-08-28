//! UA 版本探针(手动跑,单次最小请求):
//! `cargo run -p gw-kiro --example probe_version`
//!
//! 读取 `data/test-cred-probe.json`(gitignore,不含硬编码密钥):
//! ```json
//! {"refreshToken":"...","accessToken":"...","proxy":"http://...","kiro_version":"1.0.337","model":"claude-opus-5"}
//! ```
//! 用账号专属代理出口发一次极小 chat,验证指定 kiro_version 的 UA 形态被上游接受,
//! 并报告响应里是否带 reasoningContent(thinking 块)。refresh 发生时会回写轮换后的凭证。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::provider::{CallCtx, ChatRequest, Provider, StreamItem};
use gw_kiro::KiroProvider;
use serde_json::json;

const CRED_PATH: &str = "data/test-cred-probe.json";

#[tokio::main]
async fn main() {
    let raw = match std::fs::read_to_string(CRED_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("缺少凭证 {CRED_PATH}: {e};跳过探针。");
            return;
        }
    };
    let cred: serde_json::Value = serde_json::from_str(&raw).expect("凭证 JSON 解析失败");
    let refresh = cred["refreshToken"].as_str().unwrap_or("").to_string();
    let access = cred["accessToken"].as_str().unwrap_or("").to_string();
    let proxy = cred["proxy"].as_str().unwrap_or("").to_string();
    let version = cred["kiro_version"].as_str().unwrap_or("").to_string();
    let model = cred["model"].as_str().unwrap_or("claude-opus-5").to_string();
    assert!(!refresh.is_empty(), "凭证缺 refreshToken");

    let mut extra = BTreeMap::new();
    extra.insert("refresh_token".into(), json!(refresh));
    extra.insert("access_token".into(), json!(access));
    extra.insert("region".into(), json!("us-east-1"));
    if !proxy.is_empty() {
        extra.insert("proxy".into(), json!(proxy));
    }
    if !version.is_empty() {
        extra.insert("kiro_version".into(), json!(version.clone()));
    }
    let account = Account {
        account_id: "kiro-version-probe".into(),
        provider: "kiro".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra,
    };

    let client = reqwest::Client::builder().build().expect("build reqwest client");
    let provider = KiroProvider::new(client);

    println!("[probe] kiro_version={version} model={model}");
    let account = match provider.refresh_auth(&account).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("    refresh 失败: {e}");
            return;
        }
    };
    println!(
        "    refresh ok: profile_arn={} access_len={}",
        account.extra_str("profile_arn").is_some(),
        account.extra_str("access_token").map(|s| s.len()).unwrap_or(0)
    );

    // 回写轮换后的凭证(refresh_token 可能已滚动)。
    if let (Some(rt), Some(at)) = (
        account.extra_str("refresh_token"),
        account.extra_str("access_token"),
    ) {
        let mut updated = cred.clone();
        updated["refreshToken"] = json!(rt);
        updated["accessToken"] = json!(at);
        if let Err(e) = std::fs::write(CRED_PATH, serde_json::to_string_pretty(&updated).unwrap()) {
            eprintln!("    警告:回写凭证失败: {e}");
        } else {
            println!("    已回写轮换后的凭证");
        }
    }

    let acct = Arc::new(account);
    let body = json!({
        "model": model,
        "max_tokens": 1024,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 512},
        "messages": [{"role": "user", "content": "Reply with exactly: PROBE-OK"}]
    });
    let ctx = CallCtx {
        account: acct.clone(),
        session_id: "probe-version-1".into(),
        cache_key: "probe-version-1".into(),
    };
    let mut stream = match provider
        .chat(ChatRequest::from_anthropic_body(body), &ctx)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("    chat 启动失败: {e}");
            return;
        }
    };
    let mut saw_thinking = false;
    let mut saw_signature = false;
    let mut text = String::new();
    let mut stop_reason = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => match ev.event.as_str() {
                "content_block_start" => {
                    let t = ev.data["content_block"]["type"].as_str().unwrap_or("");
                    println!("    block_start: {t}");
                    if t == "thinking" {
                        saw_thinking = true;
                        if ev.data["content_block"]["signature"].is_string() {
                            saw_signature = true;
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(t) = ev.data["delta"]["text"].as_str() {
                        text.push_str(t);
                    }
                    if ev.data["delta"]["signature"].is_string() {
                        saw_signature = true;
                    }
                }
                "message_delta" => {
                    if let Some(sr) = ev.data["delta"]["stop_reason"].as_str() {
                        stop_reason = sr.to_string();
                    }
                }
                _ => {}
            },
            Ok(StreamItem::Usage(u)) => {
                println!(
                    "    usage in={} out={} cache_read={}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens
                );
            }
            Ok(StreamItem::UpstreamCut) => eprintln!("    upstream_cut(静默掐流信号)"),
            Err(e) => {
                eprintln!("    流错误: {e}");
                return;
            }
        }
    }
    println!(
        "    RESULT stop_reason={stop_reason} thinking={saw_thinking} signature={saw_signature} text={text:?}"
    );
}
