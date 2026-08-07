//! gw-cursor 端到端验证:只读本机 Cursor 的真实身份值(“成为”一个真 IDE 实例),
//! 构造一个 Anthropic 请求走 CursorProvider::chat,打印合成的 SSE。
//!
//! 真身份值来源(逆向 3.14.27 确认):
//! - accessToken:state.vscdb `cursorAuth/accessToken`
//! - machineId / macMachineId:globalStorage/storage.json `telemetry.machineId`(64-hex)
//!   / `telemetry.macMachineId`(64-hex)—— checksum 用的正是 telemetryService.machineId
//! - refreshToken:state.vscdb `cursorAuth/refreshToken`(与 accessToken 同一个 JWT)
//! - config_version:provider 现调 GetServerConfig 取会话级新鲜值
//!
//! 用法:
//!   cargo run -p gw-cursor --example e2e -- "你的问题"
//!
//! 仅本机、只读凭据,消耗极少量 Cursor 额度(一次短对话)。不打印 token 明文。

use std::collections::BTreeMap;

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::provider::{CallCtx, ChatRequest, Provider, StreamItem};
use gw_cursor::{run::RunShape, CursorConfig, CursorProvider, RunTuning};

fn read_kv(db: &str, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM ItemTable WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// 从 globalStorage/storage.json 读一个顶层字符串字段(如 telemetry.machineId)。
fn read_storage_json(home: &str, field: &str) -> Option<String> {
    let path = format!("{home}/.config/Cursor/User/globalStorage/storage.json");
    let data = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    v.get(field)?.as_str().map(|s| s.to_string())
}

/// 从 state.vscdb 的 `cursorai/serverConfig` JSON 里取 `configVersion`。
fn read_config_version(db: &str) -> Option<String> {
    let raw = read_kv(db, "cursorai/serverConfig")?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("configVersion")?.as_str().map(|s| s.to_string())
}

fn mask(s: &str) -> String {
    if s.len() <= 8 {
        return format!("{}…", &s[..s.len().min(2)]);
    }
    format!("{}…{}(len={})", &s[..6], &s[s.len() - 4..], s.len())
}

#[tokio::main]
async fn main() {
    // 没有 subscriber 时 provider 内部的 tracing::debug! 会被静默丢弃。
    // 用 RUST_LOG=gw_cursor=debug 打开逐帧诊断。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gw_cursor=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Reply with exactly: hello from cursor".to_string());

    let home = std::env::var("HOME").expect("HOME");
    let db = format!("{home}/.config/Cursor/User/globalStorage/state.vscdb");

    let token = read_kv(&db, "cursorAuth/accessToken").expect("读不到 accessToken(Cursor 未登录?)");

    // 默认“成为真 IDE”:读真 telemetry.machineId(64-hex)+ telemetry.macMachineId,
    // 并**不**从磁盘取 config_version —— provider 会在 chat 前现调 GetServerConfig 取会话级
    // 新鲜值(磁盘 cursorai/serverConfig 里的值会随服务端轮换而过期)。
    // CURSOR_MID_MODE=service 退回旧的 serviceMachineId 派生路径(仅对照实验)。
    // CURSOR_CONFIG_VERSION 可显式指定(调试用),否则留空让 provider 现取。
    let ide_mode = std::env::var("CURSOR_MID_MODE").as_deref() != Ok("service");
    let explicit_cfg = std::env::var("CURSOR_CONFIG_VERSION").ok().filter(|s| !s.is_empty());
    // 磁盘缓存值仅用于观测比对(不再喂给 provider)。
    let _disk_cfg = read_config_version(&db);
    let (machine_id, mac_machine_id) = if ide_mode {
        (
            read_storage_json(&home, "telemetry.machineId").unwrap_or_default(),
            read_storage_json(&home, "telemetry.macMachineId").unwrap_or_default(),
        )
    } else {
        (read_kv(&db, "storage.serviceMachineId").unwrap_or_default(), String::new())
    };
    eprintln!(
        "token len={} mode={} machineId={} macMachineId={} configVersion={}",
        token.len(),
        if ide_mode { "ide(真身份)" } else { "service(对照)" },
        if machine_id.is_empty() { "(派生)".into() } else { mask(&machine_id) },
        if mac_machine_id.is_empty() { "(无)".into() } else { mask(&mac_machine_id) },
        match &explicit_cfg { Some(c) => format!("显式 {}", mask(c)), None => "(provider 现取 GetServerConfig)".into() },
    );

    let mut extra = BTreeMap::new();
    extra.insert("access_token".to_string(), serde_json::json!(token));
    if let Some(rt) = read_kv(&db, "cursorAuth/refreshToken") {
        extra.insert("refresh_token".to_string(), serde_json::json!(rt));
    }
    if !machine_id.is_empty() {
        extra.insert("machine_id".to_string(), serde_json::json!(machine_id));
    }
    if !mac_machine_id.is_empty() {
        extra.insert("mac_machine_id".to_string(), serde_json::json!(mac_machine_id));
    }
    if let Some(cfg) = explicit_cfg {
        extra.insert("config_version".to_string(), serde_json::json!(cfg));
    }
    let account = Account {
        account_id: "local-cursor".to_string(),
        provider: "cursor".to_string(),
        max_concurrency: 2,
        disabled: false,
        extra,
    };

    // 默认用 grok-4.5:PROTOCOL §4 实测本号 ✅。claude/gpt 系在本号是计费耗尽
    // (ERROR_RATE_LIMITED_CHANGEABLE),拿它们测会把「协议对不对」和「有没有额度」混在一起。
    let model = std::env::var("CURSOR_E2E_MODEL").unwrap_or_else(|_| "grok-4.5".to_string());
    eprintln!("model={model}");
    // 多个参数 = **同进程内连续多轮**(一个参数一次请求),共用同一个 provider,
    // 因而共用会话注册表 —— 这是唯一能测到 `Phase::Continuation` 的方式。
    // 两次 `cargo run` 是两个进程,注册表各自为空,永远只会是 Opening。
    let prompts: Vec<String> = {
        let a: Vec<String> = std::env::args().skip(1).collect();
        if a.is_empty() { vec![prompt.clone()] } else { a }
    };

    // 协议试错开关(默认 = 完整模拟真客户端)。用于对着真上游二分「哪些字段/帧必需」。
    //   CURSOR_NO_ENV=1        不发 1.1 环境块
    //   CURSOR_NO_BUDGET=1     不发 1.1.5 预算表
    //   CURSOR_NO_RICH=1       不发 1.2.1.1.8 ProseMirror
    //   CURSOR_NO_CATALOG=1    不发 1.14 模型清单
    //   CURSOR_NO_CTXBLK=1     不发 1.2.17 大上下文块(环境详情 + 系统提示)
    //   CURSOR_NO_FRAMES=1     只发帧0,不发两个 field 3 上下文帧
    //   CURSOR_HALF_CLOSE=1    发完就关流(不保持 BiDi 打开)
    let on = |k: &str| std::env::var(k).as_deref() != Ok("1");
    let tuning = RunTuning {
        shape: RunShape {
            env_block: on("CURSOR_NO_ENV"),
            budget_table: on("CURSOR_NO_BUDGET"),
            prosemirror: on("CURSOR_NO_RICH"),
            model_catalog: on("CURSOR_NO_CATALOG"),
            context_block: on("CURSOR_NO_CTXBLK"),
        },
        context_frames: on("CURSOR_NO_FRAMES"),
        keep_stream_open: on("CURSOR_HALF_CLOSE"),
    };
    eprintln!("tuning={tuning:?}");
    let provider = CursorProvider::new(CursorConfig::default()).with_tuning(tuning);
    // CURSOR_E2E_CONV 固定 conversation_id;不设则新会话。
    let conversation_id = std::env::var("CURSOR_E2E_CONV")
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    eprintln!("conversation_id={conversation_id}");

    // CURSOR_E2E_HISTORY=1:把所有参数当成**一个请求里的多轮历史**(user/assistant 交替),
    // 用来验「Anthropic 式的无状态重放上游认不认」。默认是连续多轮(每个参数一次请求)。
    let replay_history = std::env::var("CURSOR_E2E_HISTORY").as_deref() == Ok("1");
    let rounds: Vec<Vec<serde_json::Value>> = if replay_history {
        vec![prompts.iter().enumerate().map(|(i, t)| serde_json::json!({
            "role": if i % 2 == 0 { "user" } else { "assistant" }, "content": t
        })).collect()]
    } else {
        prompts.iter().map(|p| vec![serde_json::json!({"role":"user","content":p})]).collect()
    };

    for (i, msgs) in rounds.iter().enumerate() {
        eprintln!("\n════════ 第 {} 轮({} 条消息)════════", i + 1, msgs.len());
        let body = serde_json::json!({
            "model": model,
            "stream": true,
            "max_tokens": 256,
            "messages": msgs
        });
        let req = ChatRequest::from_anthropic_body(body);
        let ctx = CallCtx {
            account: std::sync::Arc::new(account.clone()),
            session_id: conversation_id.clone(),
            cache_key: "e2e".to_string(),
        };
        let mut stream = match provider.chat(req, &ctx).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("chat 失败: {e:?}");
                std::process::exit(1);
            }
        };
        let mut full = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::Sse(ev)) => {
                    if ev.event == "content_block_delta" {
                        if let Some(t) = ev.data["delta"]["text"].as_str() {
                            full.push_str(t);
                            print!("{t}");
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    } else if ev.event == "message_delta" {
                        eprintln!("\n[stop_reason={}]", ev.data["delta"]["stop_reason"]);
                    }
                }
                Ok(StreamItem::Usage(u)) => eprintln!("[usage output_tokens={}]", u.output_tokens),
                Err(e) => {
                    eprintln!("\n流错误: {e:?}");
                    std::process::exit(1);
                }
            }
        }
        eprintln!("---- 第 {} 轮回复 {} 字符 ----", i + 1, full.chars().count());
    }
}
