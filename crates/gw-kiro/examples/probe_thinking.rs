//! 多轮 thinking 实发探针(手动跑):
//! `cargo run -p gw-kiro --example probe_thinking`
//!
//! 读取 `data/test-cred-probe.json`(gitignore):
//! {"refreshToken":"...","accessToken":"...","proxy":"http://...","kiro_version":"1.0.337","model":"claude-opus-5"}
//!
//! 三个实验(同一账号、同一代理出口):
//! A. 两轮记忆:第 1 轮让模型只在 thinking 里做计划、正文只说 NOTED;
//!    第 2 轮带 thinking+签名回传,问计划细节 —— 答得对 = 结构化透传真的恢复了记忆。
//! B. effort A/B:同一推理题,小 budget vs 大 budget,对比 thinking 长度 —— 验证推理强度生效。
//! 全程统计 THINKING_SIGNATURE_INVALID / 400 等错误。会消耗真实额度,低并发顺序执行。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::provider::{CallCtx, ChatRequest, Provider, StreamItem};
use gw_kiro::KiroProvider;
use serde_json::{json, Value};

const CRED_PATH: &str = "data/test-cred-probe.json";

struct TurnResult {
    thinking: String,
    signature: String,
    text: String,
    stop_reason: String,
    error: Option<String>,
}

async fn run_turn(
    provider: &KiroProvider,
    acct: &Arc<Account>,
    sess: &str,
    body: Value,
) -> TurnResult {
    let ctx = CallCtx {
        account: acct.clone(),
        session_id: sess.into(),
        cache_key: sess.into(),
    };
    let mut stream = match provider
        .chat(ChatRequest::from_anthropic_body(body), &ctx)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return TurnResult {
                thinking: String::new(),
                signature: String::new(),
                text: String::new(),
                stop_reason: String::new(),
                error: Some(format!("chat 启动失败: {e}")),
            }
        }
    };
    let mut r = TurnResult {
        thinking: String::new(),
        signature: String::new(),
        text: String::new(),
        stop_reason: String::new(),
        error: None,
    };
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => {
                if ev.event == "content_block_delta" {
                    match ev.data["delta"]["type"].as_str().unwrap_or("") {
                        "thinking_delta" => r
                            .thinking
                            .push_str(ev.data["delta"]["thinking"].as_str().unwrap_or("")),
                        "signature_delta" => r
                            .signature
                            .push_str(ev.data["delta"]["signature"].as_str().unwrap_or("")),
                        "text_delta" => {
                            r.text.push_str(ev.data["delta"]["text"].as_str().unwrap_or(""))
                        }
                        _ => {}
                    }
                }
                if ev.event == "message_delta" {
                    if let Some(sr) = ev.data["delta"]["stop_reason"].as_str() {
                        r.stop_reason = sr.to_string();
                    }
                }
            }
            Ok(StreamItem::Usage(u)) => {
                println!("    usage in={} out={} cache_read={}", u.input_tokens, u.output_tokens, u.cache_read_tokens);
            }
            Ok(StreamItem::UpstreamCut) => eprintln!("    upstream_cut"),
            Err(e) => {
                r.error = Some(e.to_string());
                break;
            }
        }
    }
    r
}

#[tokio::main]
async fn main() {
    let raw = std::fs::read_to_string(CRED_PATH).expect("缺凭证文件");
    let cred: Value = serde_json::from_str(&raw).expect("凭证 JSON 解析失败");
    let model = cred["model"].as_str().unwrap_or("claude-opus-5").to_string();

    let mut extra = BTreeMap::new();
    extra.insert("refresh_token".into(), json!(cred["refreshToken"].as_str().unwrap()));
    extra.insert("access_token".into(), json!(cred["accessToken"].as_str().unwrap_or("")));
    extra.insert("region".into(), json!("us-east-1"));
    for k in ["proxy", "kiro_version"] {
        if let Some(v) = cred[k].as_str().filter(|s| !s.is_empty()) {
            extra.insert(k.into(), json!(v));
        }
    }
    let account = Account {
        account_id: "kiro-thinking-probe".into(),
        provider: "kiro".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra,
    };
    let provider = KiroProvider::new(reqwest::Client::builder().build().unwrap());
    let account = match provider.refresh_auth(&account).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("refresh 失败: {e}");
            return;
        }
    };
    // 回写凭证(refresh_token 若滚动必须保留)。
    if let (Some(rt), Some(at)) = (
        account.extra_str("refresh_token"),
        account.extra_str("access_token"),
    ) {
        let mut updated = cred.clone();
        updated["refreshToken"] = json!(rt);
        updated["accessToken"] = json!(at);
        let _ = std::fs::write(CRED_PATH, serde_json::to_string_pretty(&updated).unwrap());
    }
    let acct = Arc::new(account);
    println!("model={model}\n");

    // ── A1: 推理过程只存在于 thinking(逻辑谜题,正文只报盒号)──
    let puzzle_a = "There are 3 boxes. Box 1 says: \"The gem is in box 2.\" Box 2 says: \
                    \"The gem is not here.\" Box 3 says: \"The gem is in box 1.\" \
                    Exactly one of the three statements is true, and the gem is in exactly one box. \
                    Which box contains the gem? Reply with only the box number (1, 2, or 3).";
    println!("[A1] 逻辑谜题,推理只应发生在 thinking 里,正文只报盒号");
    let a1 = run_turn(
        &provider,
        &acct,
        "probe-th-a1",
        json!({
            "model": model,
            "max_tokens": 4096,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [{"role": "user", "content": puzzle_a}]
        }),
    )
    .await;
    println!(
        "    A1: stop={} thinking_len={} sig_len={} text={:?} err={:?}",
        a1.stop_reason,
        a1.thinking.len(),
        a1.signature.len(),
        a1.text,
        a1.error
    );
    if a1.error.is_some() || a1.signature.is_empty() {
        eprintln!("A1 失败或无签名,终止");
        return;
    }
    println!("    A1 thinking 前 200 字符: {:?}", &a1.thinking[..a1.thinking.len().min(200)]);

    // ── A2: 带 thinking+签名回传,问计划细节 ──
    println!("\n[A2] 带 thinking+签名回传,问第 2 步是什么");
    let a2 = run_turn(
        &provider,
        &acct,
        "probe-th-a2",
        json!({
            "model": model,
            "max_tokens": 4096,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [
                {"role": "user", "content": puzzle_a},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": a1.thinking, "signature": a1.signature},
                    {"type": "text", "text": a1.text}
                ]},
                {"role": "user", "content": "Which box did you eliminate FIRST in your reasoning just now, and what was the key contradiction that eliminated it? Answer briefly based on your own earlier analysis."}
            ]
        }),
    )
    .await;
    println!(
        "    A2: stop={} thinking_len={} sig_len={} err={:?}",
        a2.stop_reason,
        a2.thinking.len(),
        a2.signature.len(),
        a2.error
    );
    println!("    A2 正文: {:?}", a2.text);

    // ── B: effort A/B(Opus 走 adaptive,强度旋钮是 output_config.effort,不是 budget)──
    let puzzle = "How many integers from 1 to 10000 (inclusive) are divisible by none of 3, 5, or 7? \
                  Reason carefully with inclusion-exclusion, then give the final number.";
    for (label, effort) in [("B-low", "low"), ("B-xhigh", "xhigh")] {
        println!("\n[{label}] effort={effort}");
        let r = run_turn(
            &provider,
            &acct,
            &format!("probe-th-{label}"),
            json!({
                "model": model,
                "max_tokens": 16000,
                "stream": true,
                "output_config": {"effort": effort},
                "messages": [{"role": "user", "content": puzzle}]
            }),
        )
        .await;
        println!(
            "    {label}: stop={} thinking_len={} sig_len={} text_len={} err={:?}",
            r.stop_reason,
            r.thinking.len(),
            r.signature.len(),
            r.text.len(),
            r.error
        );
    }
    println!("\n完成。判读:A2 正文若正确复述自己此前的排除过程 = 记忆恢复;B-xhigh thinking_len 应明显 > B-low。");
}
