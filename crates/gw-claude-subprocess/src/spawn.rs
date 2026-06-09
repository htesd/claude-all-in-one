use std::process::Stdio;

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::StreamItem;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use crate::ndjson::parse_ndjson_line;

#[derive(Debug, Clone)]
pub struct ClaudeSubprocessCommand {
    pub home_dir: String,
    pub prompt: String,
    pub model: Option<String>,
}

pub fn spawn_chat_stream(command: ClaudeSubprocessCommand) -> Result<ReceiverStream<Result<StreamItem, UpstreamError>>, UpstreamError> {
    let mut child = Command::new("claude");
    child
        .arg("-p")
        .arg(&command.prompt)
        .arg("--bare")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--setting-sources")
        .arg("")
        .arg("--max-turns")
        .arg("8")
        .env("HOME", &command.home_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);

    if let Some(model) = command.model.as_deref().filter(|s| !s.is_empty()) {
        child.arg("--model").arg(model);
    }

    let mut child = child
        .spawn()
        .map_err(|err| UpstreamError::network(format!("failed to spawn claude subprocess: {err}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::Other, "claude subprocess stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::Other, "claude subprocess stderr unavailable"))?;

    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let stderr_task = tokio::spawn(read_stderr(stderr));

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match parse_ndjson_line(&line) {
                    Ok(Some(item)) => {
                        if tx.send(Ok(item)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        debug!(target: "gw_claude_subprocess", "ignored NDJSON line");
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        let _ = child.start_kill();
                        return;
                    }
                },
                Ok(None) => break,
                Err(err) => {
                    let _ = tx
                        .send(Err(UpstreamError::network(format!(
                            "failed reading claude stdout: {err}"
                        ))))
                        .await;
                    let _ = child.start_kill();
                    return;
                }
            }
        }

        let stderr_output = match stderr_task.await {
            Ok(output) => output,
            Err(err) => {
                warn!(target: "gw_claude_subprocess", "stderr join failed: {err}");
                String::new()
            }
        };

        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let stderr_output = trim_stderr(&stderr_output);
                let message = if stderr_output.is_empty() {
                    format!("claude subprocess exited with status {status}")
                } else {
                    format!("claude subprocess exited with status {status}: {stderr_output}")
                };
                let _ = tx.send(Err(UpstreamError::network(message))).await;
            }
            Err(err) => {
                let _ = tx
                    .send(Err(UpstreamError::network(format!(
                        "failed waiting claude subprocess: {err}"
                    ))))
                    .await;
            }
        }
    });

    Ok(ReceiverStream::new(rx))
}

async fn read_stderr(stderr: tokio::process::ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut chunks = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            chunks.push(line);
        }
    }
    chunks.join(" | ")
}

fn trim_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.len() <= 400 {
        trimmed.to_string()
    } else {
        // 在 ≤400 的 char 边界截断(避免切在 UTF-8 多字节中间 panic,stderr 可能含中文)。
        let end = trimmed
            .char_indices()
            .take_while(|(i, _)| *i <= 400)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &trimmed[..end])
    }
}
