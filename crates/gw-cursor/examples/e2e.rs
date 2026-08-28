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
    conn.query_row("SELECT value FROM ItemTable WHERE key=?1", [key], |row| {
        row.get::<_, String>(0)
    })
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

    // token 来源:CURSOR_E2E_TOKEN 环境变量 > IDE state.vscdb > CLI auth.json。
    let token = std::env::var("CURSOR_E2E_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| read_kv(&db, "cursorAuth/accessToken"))
        .or_else(|| {
            let raw = std::fs::read_to_string(format!("{home}/.config/cursor/auth.json")).ok()?;
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()?
                .get("accessToken")?
                .as_str()
                .map(String::from)
        })
        .expect("读不到 accessToken(Cursor 未登录?)");

    // 默认“成为真 IDE”:读真 telemetry.machineId(64-hex)+ telemetry.macMachineId,
    // 并**不**从磁盘取 config_version —— provider 会在 chat 前现调 GetServerConfig 取会话级
    // 新鲜值(磁盘 cursorai/serverConfig 里的值会随服务端轮换而过期)。
    // CURSOR_MID_MODE=service 退回旧的 serviceMachineId 派生路径(仅对照实验)。
    // CURSOR_CONFIG_VERSION 可显式指定(调试用),否则留空让 provider 现取。
    let ide_mode = std::env::var("CURSOR_MID_MODE").as_deref() != Ok("service");
    let explicit_cfg = std::env::var("CURSOR_CONFIG_VERSION")
        .ok()
        .filter(|s| !s.is_empty());
    // 磁盘缓存值仅用于观测比对(不再喂给 provider)。
    let _disk_cfg = read_config_version(&db);
    let (machine_id, mac_machine_id) = if ide_mode {
        (
            read_storage_json(&home, "telemetry.machineId").unwrap_or_default(),
            read_storage_json(&home, "telemetry.macMachineId").unwrap_or_default(),
        )
    } else {
        (
            read_kv(&db, "storage.serviceMachineId").unwrap_or_default(),
            String::new(),
        )
    };
    eprintln!(
        "token len={} mode={} machineId={} macMachineId={} configVersion={}",
        token.len(),
        if ide_mode {
            "ide(真身份)"
        } else {
            "service(对照)"
        },
        if machine_id.is_empty() {
            "(派生)".into()
        } else {
            mask(&machine_id)
        },
        if mac_machine_id.is_empty() {
            "(无)".into()
        } else {
            mask(&mac_machine_id)
        },
        match &explicit_cfg {
            Some(c) => format!("显式 {}", mask(c)),
            None => "(provider 现取 GetServerConfig)".into(),
        },
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
        extra.insert(
            "mac_machine_id".to_string(),
            serde_json::json!(mac_machine_id),
        );
    }
    if let Some(cfg) = explicit_cfg {
        extra.insert("config_version".to_string(), serde_json::json!(cfg));
    }
    let account = Account {
        account_id: "local-cursor".to_string(),
        provider: "cursor".to_string(),
        max_concurrency: 2,
        disabled: false,
        created_at: 0,
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
        if a.is_empty() {
            vec![prompt.clone()]
        } else {
            a
        }
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
        // CURSOR_PROFILE=cli 走 2026-08-16 抓包的 CLI 形态(服务端持史,见 cli.rs)。
        profile: gw_cursor::cli::Profile::from_env(),
        // CURSOR_NO_REPLAY=1 关掉续轮描述符回放。
        //
        // ⚠️ 这**不是**「另一种热续方案」:本地构造 `1.1` 那条支已按 08-23 实物删除
        // (客户端从不自己构造续轮 `1.1`),所以关掉后 lookup 恒 None → 每轮 Opening
        // 全量重铺。也就是说它比的是**「描述符增量」vs「CLI 形态全量重铺」**。
        // 「描述符路线比内联热续省不省」那个问题的对照臂是 **clidrv**(49–60%),
        // 不是本开关的任何一档。
        wire_descriptor_replay: on("CURSOR_NO_REPLAY"),
    };
    eprintln!("tuning={tuning:?}");
    let provider = CursorProvider::new(CursorConfig::default()).with_tuning(tuning);
    // CURSOR_E2E_CONV 固定 conversation_id;不设则新会话。
    let conversation_id =
        std::env::var("CURSOR_E2E_CONV").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    eprintln!("conversation_id={conversation_id}");

    // CURSOR_E2E_HISTORY=1:把所有参数当成**一个请求里的多轮历史**(user/assistant 交替),
    // 用来验「Anthropic 式的无状态重放上游认不认」。默认是连续多轮(每个参数一次请求)。
    let replay_history = std::env::var("CURSOR_E2E_HISTORY").as_deref() == Ok("1");
    let rounds: Vec<Vec<serde_json::Value>> = if replay_history {
        vec![prompts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                serde_json::json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" }, "content": t
                })
            })
            .collect()]
    } else {
        prompts
            .iter()
            .map(|p| vec![serde_json::json!({"role":"user","content":p})])
            .collect()
    };

    // 与真实 Anthropic 客户端一致:每轮**重放全量历史**(上一轮 user+assistant 进 messages)。
    // 不重放的话,CLI 形态的分叉检测会把每轮都当成新会话(这就是它的设计目的)。
    let mut history: Vec<serde_json::Value> = Vec::new();

    for (i, msgs) in rounds.iter().enumerate() {
        for m in msgs {
            history.push(m.clone());
        }
        // CURSOR_E2E_IMAGE=<路径>:首轮消息带一张图片(base64 块),验证附件落盘+读图。
        if i == 0 {
            if let Ok(img_path) = std::env::var("CURSOR_E2E_IMAGE") {
                let bytes = std::fs::read(&img_path).expect("读图片失败");
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                if let Some(last) = history.last_mut() {
                    let text = last["content"].as_str().unwrap_or("").to_string();
                    last["content"] = serde_json::json!([
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":b64}},
                        {"type":"text","text":text}
                    ]);
                }
            }
        }
        // CURSOR_E2E_TOOLS=1:带一个 get_weather 工具,并自动应答 tool_result
        // (哨兵串),验证 CLI 驱动的 MCP 桥全回路。
        let with_tools = std::env::var("CURSOR_E2E_TOOLS").as_deref() == Ok("1");
        let mut auto_follow = with_tools;
        loop {
            eprintln!(
                "\n════════ 第 {} 轮(累计 {} 条消息)════════",
                i + 1,
                history.len()
            );
            let mut body = serde_json::json!({
                "model": model,
                "stream": true,
                "max_tokens": 256,
                "messages": history
            });
            if with_tools {
                body["tools"] = serde_json::json!([{
                    "name": "get_weather",
                    "description": "查询指定城市的天气",
                    "input_schema": {"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
                }]);
            }
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
            let mut tool_uses: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
            let mut stop_reason = String::new();
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
                            if ev.data["delta"]["type"] == "input_json_delta" {
                                if let Some(p) = ev.data["delta"]["partial_json"].as_str() {
                                    if let Some(last) = tool_uses.last_mut() {
                                        last.2.push_str(p);
                                    }
                                }
                            }
                        } else if ev.event == "content_block_start"
                            && ev.data["content_block"]["type"] == "tool_use"
                        {
                            tool_uses.push((
                                ev.data["content_block"]["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                ev.data["content_block"]["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                String::new(),
                            ));
                        } else if ev.event == "message_delta" {
                            stop_reason = ev.data["delta"]["stop_reason"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            eprintln!("\n[stop_reason={stop_reason}]");
                        }
                    }
                    Ok(StreamItem::Usage(u)) => {
                        eprintln!("[usage output_tokens={}]", u.output_tokens)
                    }
                    Ok(StreamItem::UpstreamCut) => {}
                    Err(e) => {
                        eprintln!("\n流错误: {e:?}");
                        std::process::exit(1);
                    }
                }
            }
            eprintln!(
                "---- 第 {} 轮回复 {} 字符 ----",
                i + 1,
                full.chars().count()
            );

            if auto_follow && stop_reason == "tool_use" && !tool_uses.is_empty() {
                // 组装 assistant tool_use 块 + user tool_result 块,自动再续一轮。
                let content: Vec<serde_json::Value> = if full.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"type":"text","text":full})]
                };
                let mut assistant_content = content;
                let mut results = Vec::new();
                for (id, name, args) in &tool_uses {
                    assistant_content.push(serde_json::json!({
                        "type":"tool_use","id":id,"name":name,
                        "input": serde_json::from_str::<serde_json::Value>(args).unwrap_or(serde_json::json!({}))
                    }));
                    results.push(serde_json::json!({
                        "type":"tool_result","tool_use_id":id,
                        "content":"MCPSENTINEL-7749: 晴 26°C(网关桥接应答)"
                    }));
                }
                history.push(serde_json::json!({"role":"assistant","content":assistant_content}));
                history.push(serde_json::json!({"role":"user","content":results}));
                auto_follow = false; // 只自动续一轮
                continue;
            }
            history.push(serde_json::json!({"role":"assistant","content":full}));
            break;
        }
    }
}
