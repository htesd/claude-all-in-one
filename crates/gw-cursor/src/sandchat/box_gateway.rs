//! Temporal 未开放时走官方云端 box 网关。不同客户端会话各有独立 agent，
//! 不向 box 传不受支持的 session_id，也不借用用户桌面端已有的 agent。

use super::*;
use gw_core::error::RequestDelivery;
use serde_json::json;

const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_SESSIONS: usize = 64;

#[derive(Clone)]
struct Session {
    agent_id: String,
    fps: Vec<u64>,
    head: String,
    touched: Instant,
}

impl Session {
    fn resume(&self, rendered: &Rendered) -> Option<(String, String)> {
        if self.touched.elapsed() >= SESSION_TTL
            || self.head != rendered.head
            || self.fps.len() >= rendered.fps.len()
            || !self.fps.iter().zip(&rendered.fps).all(|(a, b)| a == b)
        {
            return None;
        }
        render_incremental(rendered, self.fps.len()).map(|text| (self.agent_id.clone(), text))
    }
}

#[derive(Default)]
pub(super) struct State {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

struct Gateway {
    url: reqwest::Url,
    headers: reqwest::header::HeaderMap,
}

fn protocol_error(message: &str) -> UpstreamError {
    UpstreamError::new(
        UpstreamErrorKind::ServerError,
        format!("sandchat box: {message}"),
    )
}

fn gateway_url(value: &str) -> Result<reqwest::Url, UpstreamError> {
    let url = reqwest::Url::parse(value).map_err(|_| protocol_error("网关地址无效"))?;
    if url.scheme() != "https"
        || !url.host_str().is_some_and(|h| h.ends_with(".cursorvm.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(protocol_error("拒绝非官方 HTTPS box 网关地址"));
    }
    Ok(url)
}

impl Gateway {
    async fn connect(
        client: &reqwest::Client,
        account: &Account,
        token: &str,
    ) -> Result<Self, UpstreamError> {
        let data = unary(client, account, token, "EnsureSandBox", vec![]).await?;
        let mut fields = HashMap::new();
        let mut reader = Reader::new(&data);
        for (no, value) in reader.by_ref() {
            if let PVal::Len(b) = value {
                if matches!(no, 4 | 10 | 11) {
                    fields.insert(
                        no,
                        String::from_utf8(b.to_vec())
                            .map_err(|_| protocol_error("网关凭据编码无效"))?,
                    );
                }
            }
        }
        if !reader.is_done() {
            return Err(protocol_error("沙箱响应不完整"));
        }
        let url = gateway_url(fields.get(&10).map(String::as_str).unwrap_or(""))?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            (
                "authorization",
                format!(
                    "Bearer {}",
                    fields
                        .get(&11)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| protocol_error("缺少网关 token"))?
                ),
            ),
            (
                "x-anyrun-network-token",
                fields
                    .get(&4)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| protocol_error("缺少网络 token"))?
                    .clone(),
            ),
        ] {
            let mut value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| protocol_error("网关凭据头无效"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::HeaderName::from_static(name), value);
        }
        headers.insert(
            "x-sand-slim-avatars",
            reqwest::header::HeaderValue::from_static("1"),
        );
        Ok(Self { url, headers })
    }

    async fn command(
        &self,
        client: &reqwest::Client,
        method: &str,
        body: Json,
    ) -> Result<Json, UpstreamError> {
        let url = self
            .url
            .join(&format!("api/{method}"))
            .map_err(|_| protocol_error("网关方法地址无效"))?;
        let response = client
            .post(url)
            .headers(self.headers.clone())
            .json(&body)
            .send();
        // 只有 sendPrompt 会提交本轮推理；提交后的响应丢失不能经 readiness 触发换号。
        let submission = method == "sendPrompt";
        let uncertain = |e: UpstreamError| {
            if submission { e.with_delivery(RequestDelivery::Unknown) } else { e }
        };
        let response = tokio::time::timeout(UNARY_TIMEOUT, response)
            .await
            .map_err(|_| uncertain(protocol_error("网关命令超时")))?
            .map_err(|e| {
                let error = protocol_error("网关命令连接失败");
                if submission && e.is_connect() {
                    error.with_delivery(RequestDelivery::NotSubmitted)
                } else { uncertain(error) }
            })?;
        check_status(response.status().as_u16()).map_err(|e| {
            if submission && matches!(response.status().as_u16(), 400 | 401 | 403 | 429) {
                e.with_delivery(RequestDelivery::NotSubmitted)
            } else { uncertain(e) }
        })?;
        let data = tokio::time::timeout(UNARY_TIMEOUT, read_bounded(response))
            .await
            .map_err(|_| uncertain(protocol_error("网关响应读取超时")))?
            .map_err(uncertain)?;
        serde_json::from_slice(&data).map_err(|_| uncertain(protocol_error("网关响应不是 JSON")))
    }

    async fn events(&self, client: &reqwest::Client) -> Result<reqwest::Response, UpstreamError> {
        let response = client
            .get(
                self.url
                    .join("events")
                    .map_err(|_| protocol_error("事件地址无效"))?,
            )
            .headers(self.headers.clone())
            .header("accept", "text/event-stream")
            .send();
        let response = tokio::time::timeout(UNARY_TIMEOUT, response)
            .await
            .map_err(|_| protocol_error("网关事件建流超时"))?
            .map_err(|_| protocol_error("网关事件连接失败"))?;
        check_status(response.status().as_u16())?;
        Ok(response)
    }

    async fn create_agent(&self, client: &reqwest::Client) -> Result<String, UpstreamError> {
        let name = format!("caio-relay-{}", uuid::Uuid::new_v4().simple());
        let result = self
            .command(
                client,
                "createAgent",
                json!({
                    "name":name, "description":"CAIO 独立客户端会话",
                    "creationRoute":{"kind":"box"}, "isIntroductionSuppressed":true,
                    "isKickstartRequested":false, "supportsTemporalHarness":true,
                    "clientNonce":uuid::Uuid::new_v4().to_string(),
                }),
            )
            .await
            .and_then(|created| {
                created["agent"]["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| protocol_error("创建响应缺少 agent id"))
            });
        if result.is_err() {
            // 创建可能已成功但响应丢失；只按本次随机全名找回并删除，绝不重试创建。
            let cleanup = async {
                let agents = self.command(client, "listAgents", json!({})).await?;
                let ids = agents
                    .as_array()
                    .ok_or_else(|| protocol_error("agent 列表格式无效"))?
                    .iter()
                    .filter(|a| a["name"].as_str() == Some(name.as_str()))
                    .filter_map(|a| a["id"].as_str().map(str::to_owned))
                    .collect();
                retire(self, client, ids).await;
                Ok::<_, UpstreamError>(())
            };
            if !matches!(
                tokio::time::timeout(UNARY_TIMEOUT, cleanup).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("sandchat box: 创建结果不确定且 agent 核对清理失败");
            }
        }
        result
    }
}

// 网关使用短期独立凭据，401/403 不能当作账号 OAuth 失效去刷新、禁号。
fn check_status(status: u16) -> Result<(), UpstreamError> {
    if status == 200 {
        return Ok(());
    }
    let kind = match status {
        401 | 403 => UpstreamErrorKind::ModelNotAvailable,
        429 => UpstreamErrorKind::RateLimited,
        _ => UpstreamErrorKind::ServerError,
    };
    Err(UpstreamError::new(
        kind,
        format!("sandchat box 网关 HTTP {status}"),
    ))
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, UpstreamError> {
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| protocol_error("响应读流失败"))?;
        if data.len() + chunk.len() > MAX_BODY {
            return Err(protocol_error("响应超出 16 MiB"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[derive(Default)]
struct Events {
    pending: Vec<u8>,
}

impl Events {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Json>, UpstreamError> {
        if self.pending.len() + bytes.len() > MAX_BODY {
            return Err(protocol_error("SSE 缓冲超限"));
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((end, len)) = [
            self.pending
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, 2)),
            self.pending
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| (i, 4)),
        ]
        .into_iter()
        .flatten()
        .min()
        {
            let event: Vec<u8> = self.pending.drain(..end + len).collect();
            let text = std::str::from_utf8(&event).map_err(|_| protocol_error("SSE UTF-8 无效"))?;
            let data = text
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                events.push(
                    serde_json::from_str(&data).map_err(|_| protocol_error("SSE JSON 无效"))?,
                );
            }
        }
        Ok(events)
    }
}

struct Reply {
    agent_id: String,
    nonce: String,
    request_id: Option<String>,
    emitted: HashMap<String, String>,
    settled: bool,
    folder: crate::inference::Folder,
}

impl Reply {
    fn entry(&mut self, entry: &Json) {
        if entry["kind"] == "message"
            && entry["role"] == "user"
            && entry["clientNonce"] == self.nonce
        {
            if let Some(id) = entry["requestId"].as_str() {
                self.request_id = Some(id.to_string());
            }
            return;
        }
        // 用 requestId 严格关联，旧轮重放和用户回声不能当成模型回答。
        if self
            .request_id
            .as_deref()
            .is_none_or(|id| entry["requestId"].as_str() != Some(id))
        {
            return;
        }
        if entry["kind"] != "send-message" || entry["message"]["type"] != "text" {
            return;
        }
        let (Some(id), Some(content)) =
            (entry["id"].as_str(), entry["message"]["content"].as_str())
        else {
            return;
        };
        let prev = self.emitted.get(id).map(String::as_str).unwrap_or("");
        if let Some(delta) = content.strip_prefix(prev) {
            if !delta.is_empty() {
                self.folder.on_text(delta, false);
            }
            self.emitted.insert(id.to_string(), content.to_string());
        } else if !prev.starts_with(content) {
            self.folder
                .fail(protocol_error("已输出的消息被非前缀改写，停止以免重复输出"));
        }
    }

    fn agent(&mut self, agent: &Json) {
        if agent["id"] != self.agent_id {
            return;
        }
        let settlement = &agent["lastTurnSettlement"];
        if settlement["clientNonce"] == self.nonce {
            if settlement["outcome"] == "success" {
                self.settled = true;
            } else if settlement["outcome"].is_string() {
                self.folder.fail(protocol_error("云端运行未成功结算"));
            }
        }
    }

    fn event(&mut self, event: &Json) {
        let payload = &event["payload"];
        match event["channel"].as_str() {
            Some("transcript")
                if payload["agentId"] == self.agent_id
                    || payload["activeAgentId"] == self.agent_id =>
            {
                if let Some(entries) = payload["entries"].as_array() {
                    for entry in entries {
                        self.entry(entry);
                    }
                }
                if payload["entry"].is_object() {
                    self.entry(&payload["entry"]);
                }
            }
            Some("agent-upserted") => self.agent(&payload["agent"]),
            _ => {}
        }
    }

    fn complete(&self) -> bool {
        self.settled && !self.emitted.is_empty()
    }
}

async fn read_tail(
    gateway: &Gateway,
    client: &reqwest::Client,
    reply: &mut Reply,
) -> Result<(), UpstreamError> {
    let tail = gateway
        .command(
            client,
            "getAgentTranscriptTail",
            json!({"id":reply.agent_id,"limit":1000}),
        )
        .await?;
    let entries = tail["entries"]
        .as_array()
        .ok_or_else(|| protocol_error("transcript 缺少 entries"))?;
    for entry in entries {
        reply.entry(entry);
    }
    Ok(())
}

async fn catch_up(
    gateway: &Gateway,
    client: &reqwest::Client,
    reply: &mut Reply,
) -> Result<(), UpstreamError> {
    // 先观察结算再拉 tail；不能把结算前的快照当成最终正文。
    let agents = gateway.command(client, "listAgents", json!({})).await?;
    for agent in agents
        .as_array()
        .ok_or_else(|| protocol_error("agent 列表格式无效"))?
    {
        reply.agent(agent);
    }
    read_tail(gateway, client, reply).await
}

async fn retire(gateway: &Gateway, client: &reqwest::Client, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let result = tokio::time::timeout(
        INTERRUPT_TIMEOUT,
        gateway.command(client, "deleteAgents", json!({"ids":ids})),
    )
    .await;
    if !matches!(result, Ok(Ok(_))) {
        tracing::warn!("sandchat box: 本进程创建的过期 agent 清理失败");
    }
}

// 云端原生 Bash/文件工具与客户端同名工具不是同一执行环境。必须显式交代
// 此次任务是转发外部客户端协议，避免上游在 box 里寻找客户端本机路径。
fn client_tool_prompt(text: &str, has_tools: bool) -> String {
    if !has_tools {
        return text.to_string();
    }
    let contract = "You are handling a request from a REMOTE CLIENT through a tool-call relay. \
        The client has its own computer and filesystem, completely separate from this cloud box. \
        All tools declared inside the client request (including Bash/bash, Read/read and Edit/edit) \
        execute ON THE CLIENT, not in your cloud environment. Never invoke your native shell, \
        filesystem, search or computer tools to perform a client tool operation; the client's paths \
        do not exist in your box. Instead, send the client a message whose literal text is \
        <tool_call>{\"name\":\"EXACT_DECLARED_NAME\",\"arguments\":{...}}</tool_call>. \
        Use your normal send-message mechanism to send that literal text, then end your turn. \
        Do not merely describe the operation or claim it ran. The relay executes it locally and \
        supplies the real tool_result in a later message. Only explicitly uploaded attachments \
        belong to the cloud environment and may be inspected there.";
    format!("{contract}\n\n<remote_client_request>\n{text}\n</remote_client_request>\n\n{contract}")
}

pub(super) async fn chat_stream(
    client: reqwest::Client,
    account: &Account,
    token: &str,
    req: ChatRequest,
    conv_fp: String,
    sessions: &Arc<SandSessions>,
) -> Result<ChatStream, UpstreamError> {
    let body = req.body.clone();
    let (body, attachments) = tokio::task::spawn_blocking(move || box_attachments::prepare(body))
        .await
        .map_err(|_| protocol_error("附件准备任务失败"))??;
    let rendered = render_blocking(body).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let account = account.clone();
    let token = token.to_string();
    let sessions = sessions.clone();
    // 先建立取消信号再发生云端副作用；调用方丢弃 stream 不会直接中止准备
    // future，已提交的 create/send 会有机会拿到响应并执行清理。
    tokio::spawn(async move {
        let mut ready = Some(ready_tx);
        if let Err(e) = run(
            client,
            &account,
            &token,
            req,
            conv_fp,
            rendered,
            attachments,
            &sessions,
            tx.clone(),
            &mut ready,
        )
        .await
        {
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(e));
            } else {
                let _ = tx.send(Err(e)).await;
            }
        }
    });
    // 建流/上传失败仍是 Provider::chat 的 Err；提交确认丢失另带 Unknown，禁止重放。
    // 不能过早返回 Ok(stream)，否则这些错误绕过换号循环，直接变成客户端 502。
    ready_rx
        .await
        .map_err(|_| protocol_error("准备任务提前结束").with_delivery(RequestDelivery::Unknown))??;
    Ok(submitted_stream(rx))
}

async fn run(
    client: reqwest::Client,
    account: &Account,
    token: &str,
    req: ChatRequest,
    conv_fp: String,
    rendered: Rendered,
    attachments: Vec<box_attachments::Attachment>,
    sessions: &Arc<SandSessions>,
    tx: tokio::sync::mpsc::Sender<Result<StreamItem, UpstreamError>>,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<(), UpstreamError>>>,
) -> Result<(), UpstreamError> {
    let guard = tokio::select! {
        _=tx.closed()=>return Ok(()),
        guard=sessions.run_gate(&account.account_id).lock_owned()=>guard,
    };
    // EnsureSandBox 的完整响应也必须有期限，且取消不能长期占住账号锁。
    let gateway = tokio::select! {
        _ = tx.closed() => return Ok(()),
        result = tokio::time::timeout(UNARY_TIMEOUT, Gateway::connect(&client, account, token)) =>
            result.map_err(|_| protocol_error("沙箱准备超时"))??,
    };
    if tx.is_closed() {
        return Ok(());
    }
    let key = (account.account_id.clone(), conv_fp);
    let expired = {
        let mut map = sessions
            .box_state
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut expired = Vec::new();
        map.retain(|(aid, _), s| {
            let keep = aid != &account.account_id || s.touched.elapsed() < SESSION_TTL;
            if !keep {
                expired.push(s.agent_id.clone());
            }
            keep
        });
        expired
    };
    retire(&gateway, &client, expired).await;
    let previous = sessions
        .box_state
        .sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .cloned();
    let resume = previous.as_ref().and_then(|s| s.resume(&rendered));
    let attachment_start = if resume.is_some() {
        previous.as_ref().unwrap().fps.len()
    } else {
        0
    };
    let (agent_id, text) = if let Some(resume) = resume {
        resume
    } else {
        let retired = {
            let mut map = sessions
                .box_state
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let mut retired = Vec::new();
            if let Some(old) = map.remove(&key) {
                retired.push(old.agent_id);
            }
            if map
                .keys()
                .filter(|(aid, _)| aid == &account.account_id)
                .count()
                >= MAX_SESSIONS
            {
                let oldest = map
                    .iter()
                    .filter(|((aid, _), _)| aid == &account.account_id)
                    .min_by_key(|(_, s)| s.touched)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    if let Some(old) = map.remove(&k) {
                        retired.push(old.agent_id);
                    }
                }
            }
            retired
        };
        retire(&gateway, &client, retired).await;
        let id = gateway.create_agent(&client).await?;
        (id, render_full(&rendered))
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let text = client_tool_prompt(
        &text,
        req.body["tools"].as_array().is_some_and(|t| !t.is_empty()),
    );
    // 先建事件流再提交，监听失败时没有产生上游消息。
    let prepared = async {
        if tx.is_closed() {
            return Err(protocol_error("下游在准备阶段断开"));
        }
        let mut paths = Vec::new();
        let mut names = Vec::new();
        for attachment in attachments.iter().filter(|a| a.message >= attachment_start) {
            if tx.is_closed() { return Err(protocol_error("下游在上传前断开")); }
            let uploaded = gateway.command(&client, "uploadAttachment", json!({
                "agentId":agent_id,"filename":attachment.name,"bytesBase64":attachment.data,
            })).await?;
            let path = uploaded["path"].as_str().filter(|p| !p.is_empty())
                .ok_or_else(|| protocol_error("附件上传响应缺少 path"))?;
            paths.push(path.to_string());
            names.push(attachment.name.clone());
        }
        let response = gateway.events(&client).await?;
        if tx.is_closed() {
            return Err(protocol_error("下游在提交前断开"));
        }
        let sent = gateway
            .command(
                &client,
                "sendPrompt",
                json!({"agentId":agent_id,"prompt":text,"clientNonce":nonce,"source":"desktop", "attachmentPaths":paths,"attachmentNames":names}),
            )
            .await?;
        if sent["accepted"] != true {
            let delivery = if sent["accepted"] == false {
                RequestDelivery::NotSubmitted
            } else { RequestDelivery::Unknown };
            return Err(protocol_error("网关未确认接受消息").with_delivery(delivery));
        }
        Ok(response)
    }
    .await;
    let response = match prepared {
        Ok(response) => response,
        Err(e) => {
            let _ = tokio::time::timeout(
                INTERRUPT_TIMEOUT,
                gateway.command(&client, "interruptAgentRun", json!({"id":agent_id})),
            )
            .await;
            sessions
                .box_state
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&key);
            retire(&gateway, &client, vec![agent_id]).await;
            return Err(e);
        }
    };
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(()));
    }
    let tools: HashSet<String> = req.body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    let folder = crate::inference::Folder::new(
        &req.model,
        tools.clone(),
        false,
        text.len() as u64,
        !tools.is_empty(),
    );
    let reply = Reply {
        agent_id: agent_id.clone(),
        nonce,
        request_id: None,
        emitted: HashMap::new(),
        settled: false,
        folder,
    };
    let state = sessions.clone();
    {
        let _guard = guard;
        let mut reply = reply;
        let mut response = response.bytes_stream();
        let mut decoder = Events::default();
        let mut recovery = tokio::time::interval(Duration::from_secs(30));
        recovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let run = async {
            loop {
                tokio::select! {
                    _ = tx.closed() => return Err(protocol_error("下游已断开")),
                    _ = recovery.tick() => tokio::select! {
                        _=tx.closed()=>return Err(protocol_error("下游已断开")),
                        result=catch_up(&gateway,&client,&mut reply)=>result?,
                    },
                    chunk = response.next() => match chunk {
                        Some(Ok(bytes)) => for event in decoder.feed(&bytes)? { reply.event(&event); },
                        Some(Err(_)) | None => {
                            // 先补读权威 tail；只重连续收，绝不重发 sendPrompt。
                            tokio::select! {
                                _=tx.closed()=>return Err(protocol_error("下游已断开")),
                                result=catch_up(&gateway,&client,&mut reply)=>result?,
                            }
                            if !reply.complete() {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                response = gateway.events(&client).await?.bytes_stream();
                                decoder = Events::default();
                            }
                        }
                    }
                }
                for item in reply.folder.take_pending() {
                    tx.send(item)
                        .await
                        .map_err(|_| protocol_error("下游已断开"))?;
                }
                if reply.folder.is_failed() {
                    return Err(protocol_error("回复归集失败"));
                }
                if reply.settled {
                    // SSE 与补读可交错；结算后再拉最终权威正文作为收尾屏障。
                    tokio::select! {
                        _=tx.closed()=>return Err(protocol_error("下游已断开")),
                        result=read_tail(&gateway,&client,&mut reply)=>result?,
                    }
                    if !reply.complete() {
                        return Err(protocol_error("云端成功结算但没有正文"));
                    }
                    if reply.folder.is_failed() {
                        return Err(protocol_error("最终正文归集失败"));
                    }
                    return Ok(());
                }
            }
        };
        let result = tokio::time::timeout(TOTAL_TIMEOUT, run).await;
        match result {
            Ok(Ok(())) => {
                reply.folder.set_estimated_usage(text.len() as u64);
                reply.folder.finish();
                if !reply.folder.is_failed() {
                    state
                        .box_state
                        .sessions
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(
                            key,
                            Session {
                                agent_id,
                                fps: rendered.fps,
                                head: rendered.head,
                                touched: Instant::now(),
                            },
                        );
                } else {
                    state
                        .box_state
                        .sessions
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&key);
                    retire(&gateway, &client, vec![agent_id]).await;
                }
            }
            other => {
                let _ = tokio::time::timeout(
                    INTERRUPT_TIMEOUT,
                    gateway.command(&client, "interruptAgentRun", json!({"id":agent_id})),
                )
                .await;
                state
                    .box_state
                    .sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
                retire(&gateway, &client, vec![agent_id]).await;
                if !reply.folder.is_failed() {
                    reply.folder.fail(match other {
                        Ok(Err(e)) => e,
                        _ => protocol_error("运行超过总时限"),
                    });
                }
            }
        }
        for item in reply.folder.take_pending() {
            if !matches!(
                tokio::time::timeout(INTERRUPT_TIMEOUT, tx.send(item)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply(tools: bool) -> Reply {
        let declared = if tools {
            HashSet::from(["lookup_probe".to_string()])
        } else {
            HashSet::new()
        };
        Reply {
            agent_id: "owned".into(),
            nonce: "this-turn".into(),
            request_id: None,
            emitted: HashMap::new(),
            settled: false,
            folder: crate::inference::Folder::new("grok_bot_auto", declared, false, 10, tools),
        }
    }
    fn echo() -> Json {
        json!({"kind":"message","id":"t1u","role":"user","clientNonce":"this-turn","requestId":"run-1"})
    }
    fn output(text: &str) -> Json {
        json!({"kind":"send-message","id":"t1s0","message":{"type":"text","content":text},"requestId":"run-1"})
    }
    fn settled(nonce: &str) -> Json {
        json!({"id":"owned","lastTurnSettlement":{"clientNonce":nonce,"outcome":"success"}})
    }

    #[test]
    fn 网关只允许官方地址且鉴权错误不误杀账号() {
        assert!(gateway_url("https://abc.us10.cursorvm.com").is_ok());
        for url in [
            "http://abc.cursorvm.com",
            "https://cursorvm.com.evil.test",
            "https://abc.cursorvm.com@evil.test",
            "https://127.0.0.1",
            "https://abc.cursorvm.com:8000",
            "https://abc.cursorvm.com/path",
            "https://abc.cursorvm.com?token=x",
        ] {
            assert!(gateway_url(url).is_err(), "{url}");
        }
        for status in [401, 403, 404, 502] {
            assert_ne!(
                check_status(status).unwrap_err().kind,
                UpstreamErrorKind::TokenInvalid
            );
        }
        for status in [401, 403] {
            assert!(check_status(status)
                .unwrap_err()
                .kind
                .spares_account_health());
        }
    }

    #[test]
    fn system或工具契约变化时必须全量重建会话() {
        let first = json!({"system":"original","messages":[{"role":"user","content":"first"}]});
        let initial = render(&first).unwrap();
        let session = Session {
            agent_id: "owned".into(),
            fps: initial.fps,
            head: initial.head,
            touched: Instant::now(),
        };
        let mut next = first.clone();
        next["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":"answer"}),
            json!({"role":"user","content":"next"}),
        ]);
        assert!(session.resume(&render(&next).unwrap()).is_some());
        next["system"] = json!("changed");
        assert!(session.resume(&render(&next).unwrap()).is_none());
        next["system"] = json!("original");
        next["tools"] = json!([{"name":"new_tool","input_schema":{"type":"object"}}]);
        assert!(session.resume(&render(&next).unwrap()).is_none());
    }

    #[test]
    fn sse分片中文混合换行和心跳() {
        let data = "data: {\"n\":1}\r\n\r\n: ping\n\ndata: {\"text\":\"中文\"}\n\n".as_bytes();
        let mut decoder = Events::default();
        let mut out = Vec::new();
        for byte in data {
            out.extend(decoder.feed(&[*byte]).unwrap());
        }
        assert_eq!(out, vec![json!({"n":1}), json!({"text":"中文"})]);
        let mut decoder = Events::default();
        assert_eq!(decoder.feed(data).unwrap(), out);
        assert!(decoder.feed(b"data: not-json\n\n").is_err());
    }

    #[test]
    fn 只接收本轮回复并去重重放() {
        let mut r = reply(false);
        r.entry(&output("不能提前接收"));
        assert!(!r.folder.saw_content());
        r.entry(&echo());
        let mut wrong = output("别轮内容");
        wrong["requestId"] = json!("other");
        r.entry(&wrong);
        assert!(!r.folder.saw_content());
        r.entry(&output("正确回答"));
        r.entry(&output("正确回答"));
        r.agent(&settled("other-turn"));
        assert!(!r.complete());
        r.agent(&settled("this-turn"));
        assert!(r.complete());
        r.folder.finish();
        let events: Vec<_> = r
            .folder
            .take_pending()
            .into_iter()
            .filter_map(|i| match i.unwrap() {
                StreamItem::Sse(e) => Some(e),
                _ => None,
            })
            .collect();
        let message = gw_core::fold::fold_sse_to_message(&events).unwrap();
        assert_eq!(message["content"][0]["text"], "正确回答");
    }

    #[test]
    fn 补读新快照后旧前缀重放不报错也不重复输出() {
        let mut r = reply(false);
        r.entry(&echo());
        r.entry(&output("Hello world"));
        r.entry(&output("Hello"));
        r.entry(&output("Hello world"));
        assert!(!r.folder.is_failed());
        assert_eq!(r.emitted["t1s0"], "Hello world");
        r.entry(&output("Goodbye"));
        assert!(r.folder.is_failed());
    }

    #[tokio::test]
    async fn 提交响应丢失标记未知而上传错误不阻止安全换号() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for response in [
            &b""[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"[..],
            &b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"[..],
        ] {
            for method in ["sendPrompt", "uploadAttachment"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut input = Vec::new();
                    loop {
                        let mut chunk = [0; 4096];
                        let n = socket.read(&mut chunk).await.unwrap();
                        assert!(n > 0);
                        input.extend_from_slice(&chunk[..n]);
                        if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&input[..end]).to_lowercase();
                            let size: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                            if input.len() >= end + 4 + size { break; }
                        }
                    }
                    // 已完整接收本次提交，随后丢失确认或返回不可判定的错误。
                    socket.write_all(response).await.unwrap();
                    socket.shutdown().await.unwrap();
                });
                let gateway = Gateway { url: format!("http://{address}/").parse().unwrap(), headers: Default::default() };
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                let error = gateway.command(&client, method, json!({"clientNonce":"one-turn"})).await.unwrap_err();
                assert_eq!(error.delivery.may_have_executed(), method == "sendPrompt");
                assert_eq!(error.kind, UpstreamErrorKind::ServerError);
                tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = Gateway { url: format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap(), headers: Default::default() };
        drop(listener);
        let error = gateway.command(&reqwest::Client::builder().no_proxy().build().unwrap(), "sendPrompt", json!({})).await.unwrap_err();
        assert_eq!(error.delivery, RequestDelivery::NotSubmitted);
    }

    #[tokio::test]
    async fn 创建响应丢失时只清理本次随机身份() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut name = String::new();
            for step in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                let (header_end, body_size) = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    input.extend_from_slice(&chunk[..n]);
                    if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]).to_lowercase();
                        let size: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if input.len() >= end + 4 + size {
                            break (end + 4, size);
                        }
                    }
                };
                let body: Json =
                    serde_json::from_slice(&input[header_end..header_end + body_size]).unwrap();
                let response = match step {
                    0 => {
                        name = body["name"].as_str().unwrap().to_string();
                        // 模拟远端创建完成，但连接在响应前断开。
                        continue;
                    }
                    1 => json!([{"id":"own","name":name},{"id":"user-agent","name":"user"}]),
                    _ => {
                        assert_eq!(body["ids"], json!(["own"]));
                        json!({})
                    }
                }
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).as_bytes()).await.unwrap();
            }
        });
        let gateway = Gateway {
            url: format!("http://{address}/").parse().unwrap(),
            headers: Default::default(),
        };
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(gateway.create_agent(&client).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn 补读在观察结算后获取最终正文() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut finished = false;
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = vec![0; 8192];
                let size = socket.read(&mut input).await.unwrap();
                let request = String::from_utf8_lossy(&input[..size]);
                let response = if request.starts_with("POST /api/listAgents ") {
                    finished = true;
                    json!([settled("this-turn")])
                } else {
                    json!({"entries":[echo(),output(if finished {"Hello world"} else {"Hello"})]})
                };
                let body = response.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            }
        });
        let gateway = Gateway {
            url: format!("http://{address}/").parse().unwrap(),
            headers: Default::default(),
        };
        let mut r = reply(false);
        catch_up(
            &gateway,
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &mut r,
        )
        .await
        .unwrap();
        assert!(r.complete());
        assert_eq!(r.emitted["t1s0"], "Hello world");
        server.await.unwrap();
    }

    #[test]
    fn 工具文本折叠为标准tool_use() {
        let mut r = reply(true);
        r.entry(&echo());
        r.entry(&output(
            "<tool_call>{\"name\":\"lookup_probe\",\"arguments\":{\"key\":\"check\"}}</tool_call>",
        ));
        r.agent(&settled("this-turn"));
        assert!(r.complete());
        r.folder.finish();
        let events: Vec<_> = r
            .folder
            .take_pending()
            .into_iter()
            .filter_map(|i| match i.unwrap() {
                StreamItem::Sse(e) => Some(e),
                _ => None,
            })
            .collect();
        let message = gw_core::fold::fold_sse_to_message(&events).unwrap();
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["name"], "lookup_probe");
        assert_eq!(message["content"][0]["input"]["key"], "check");
    }

    #[test]
    fn 其他agent事件不能串入当前流() {
        let mut r = reply(false);
        r.event(&json!({"channel":"transcript","payload":{"agentId":"other","entry":echo()}}));
        r.entry(&output("污染"));
        assert!(!r.folder.saw_content());
        r.event(&json!({"channel":"transcript","payload":{"agentId":"owned","entry":echo()}}));
        r.event(
            &json!({"channel":"transcript","payload":{"agentId":"owned","entry":output("正常")}}),
        );
        r.event(&json!({"channel":"agent-upserted","payload":{"agent":settled("this-turn")}}));
        assert!(r.complete());
    }
}
