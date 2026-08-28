//! MCP 桥子进程(`--mode cursor-mcp-bridge`)。
//!
//! 角色:cursor-agent CLI 的 MCP server(stdio,NDJSON 行协议),把「调用方声明的
//! Anthropic 工具」暴露成 `gwtools` server 下的 MCP 工具。CLI 调工具时,桥把调用
//! 经 unix socket 转给网关进程(**挂起等待**),网关把它变成 Anthropic `tool_use`
//! 发给远端客户;客户的下一个请求带回 `tool_result`,网关写回 socket,桥应答 CLI,
//! CLI 继续生成。这就是「反代一问一答 ↔ CLI 长会话」之间的活桥。
//!
//! ## 协议
//!
//! - stdio 侧:MCP(2024-11-05),换行分隔的 JSON-RPC。`initialize` / `tools/list`
//!   (内容来自 `--tools` 文件)/ `tools/call`(转 socket)。
//! - socket 侧(JSON 行,unix domain):
//!   - 桥 → 网关:`{"call": {"id": N, "name": "...", "args": {...}}}`
//!   - 网关 → 桥:`{"result": "..."} | {"error": "..."}`(按 id 顺序,同时只挂一个)
//!
//! 安全红线:桥只转发,绝不自己执行任何东西;工具的实际执行在远端客户机器上。

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// 桥入口(gw-app `main` 调用)。`tools` = MCP tools 数组的 JSON 文件。
pub async fn run(sock: PathBuf, tools: PathBuf) -> anyhow::Result<()> {
    let tools_json: Value = serde_json::from_str(&std::fs::read_to_string(&tools)?)?;
    let stream = UnixStream::connect(&sock).await?;
    let (sock_r, mut sock_w) = stream.into_split();
    let mut sock_lines = BufReader::new(sock_r).lines();
    // 串行闸:同一时刻只允许一个未决 tools/call(网关侧按序应答)。
    let call_lock = Mutex::new(());

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut call_id = 0u64;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let resp = match method {
            "initialize" => Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "gwtools", "version": "1.0"},
            })),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({"tools": tools_json})),
            "tools/call" => {
                let _guard = call_lock.lock().await;
                call_id += 1;
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let call = json!({"call": {"id": call_id, "name": name, "args": args}});
                sock_w.write_all(call.to_string().as_bytes()).await?;
                sock_w.write_all(b"\n").await?;
                sock_w.flush().await?;
                // 挂起等网关应答(远端客户执行工具要多久都行,由网关侧管超时)。
                let answer = sock_lines.next_line().await;
                match answer {
                    Ok(Some(ans)) => {
                        let v: Value =
                            serde_json::from_str(&ans).unwrap_or(json!({"error": "bad reply"}));
                        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                            Some(json!({
                                "content": [{"type": "text", "text": format!("工具调用失败: {err}")}],
                                "isError": true,
                            }))
                        } else {
                            let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
                            Some(json!({"content": [{"type": "text", "text": text}]}))
                        }
                    }
                    _ => Some(json!({
                        "content": [{"type": "text", "text": "工具调用失败: 网关连接中断"}],
                        "isError": true,
                    })),
                }
            }
            _ => id.as_ref().map(|_| json!({})),
        };
        if let (Some(rid), Some(result)) = (id, resp) {
            let out = json!({"jsonrpc": "2.0", "id": rid, "result": result});
            stdout.write_all(out.to_string().as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
