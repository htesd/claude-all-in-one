//! CLI 驱动:把 `cursor-agent` 官方 CLI 当上游(子进程模式)。
//!
//! 2026-08-16 决策:Cursor 的服务端持史是「内容寻址存储 + 哈希回显握手」,
//! 线协议合成续轮的代价比预期深(见 PROTOCOL-agent-run.md §20)。兜底方案 =
//! 不模拟协议,直接驱动真客户端 —— 协议保真 100%,多轮/持史/缓存命中全免费。
//!
//! ## 驱动方式(全部实测)
//!
//! - **每号一个 HOME**(`<base>/<account_id>/`):把号库里的 access/refresh token
//!   写成 `.config/cursor/auth.json` 即完成登录,CLI 自己刷新并回写。
//! - **每次请求 spawn 一次** `-p --output-format stream-json --stream-partial-output`;
//!   首轮新会话,从 `system.init` 事件拿 `session_id`;后续轮 `--resume <id>`。
//! - **安全闸**:`--mode ask`(read-only)且**绝不**加 `--force`。注意 ask 模式
//!   仍允许**只读 shell**(ls/cat 会真跑)—— 部署侧用降权 uid + 目录权限隔离
//!   敏感文件(见部署文档),模型能读到的只有每号工作区。
//! - 调用方 system 提示写进每号工作区的 `AGENTS.md`(CLI 原生 rules 位置)。
//! - **工具桥**(调用方声明了 tools 时):每号 HOME 写死 `.cursor/mcp.json` +
//!   `permissions.json`(`{"mcpAllowlist":["gwtools:*"]}`,格式挖自 CLI bundle 的
//!   `shouldBlockMcp`/`matchesMcpPattern`,必须带冒号)。桥是本网关二进制的
//!   `--mode cursor-mcp-bridge` 子进程;CLI 调工具 → 桥挂起 → 网关发 tool_use
//!   给客户 → 客户下次请求带 tool_result → 桥应答 → CLI 继续。
//! - **图片**:落到每号工作区 `assets/`,提示词带绝对路径(ask 模式只读工具
//!   能读图,实测左红右蓝识别正确);**PDF**:抽文本层内联进提示词。
//!
//! ## 事件流(stream-json,逐行 NDJSON)
//!
//! `system.init`(session_id)→ `thinking.delta`×N → `assistant` 增量×N
//! → `assistant` 全量回显(**无 timestamp_ms**,要去重)→ `result`(usage 是
//! **上游真实值**,含 cacheReadTokens)。
//!
//! ## 跨请求接力(工具回路的关键)
//!
//! 反代是一问一答,CLI 是长会话。桥接期间 CLI 进程**保持存活**:泵任务把
//! 「阶段输出」写进 [`OutQueue`],每个 Anthropic 响应是一条 drain 流;泵在
//! 桥调用处挂起,等下一轮请求把 tool_result 喂进来再继续泵。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, Notify};

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{ChatUsage, SseEvent, StreamItem};

/// 单轮 CLI 调用的硬上限。gw-app 还有 300s 空闲熔断;这里取 240s,
/// 保证先于我方上层超时干净地杀掉进程。
const CLI_TIMEOUT: Duration = Duration::from_secs(240);
/// 桥调用等调用方带回 tool_result 的上限。超时给桥回错误,让 CLI 干净收尾。
const PENDING_TTL: Duration = Duration::from_secs(280);
/// 会话条目 TTL:超时按新会话处理(与线协议形态同值)。
const SESSION_TTL: Duration = Duration::from_secs(2 * 3600);

/// 解析好的 CLI 二进制与数据根。
#[derive(Debug, Clone)]
pub struct CliDriverConfig {
    /// cursor-agent 可执行文件路径。
    pub bin: PathBuf,
    /// 每号 HOME 的根目录(`<base>/<account_id>/`)。
    pub base_dir: PathBuf,
    /// 本网关二进制(桥子进程用 `--mode cursor-mcp-bridge` 起它)。
    pub self_exe: PathBuf,
}

impl CliDriverConfig {
    pub fn from_env() -> Self {
        let bin = std::env::var("CURSOR_AGENT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/opt/cursor-agent/cursor-agent"));
        let base_dir = std::env::var("CURSOR_CLI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/app/data/cursor-cli"));
        let self_exe = std::env::var("CURSOR_SELF_BIN")
            .map(PathBuf::from)
            .ok()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("claude-all-in-one"));
        Self {
            bin,
            base_dir,
            self_exe,
        }
    }
}

// ── 每号 HOME ────────────────────────────────────────────────────────────────

/// 备好一个账号的 HOME:auth.json(仅缺失时写 —— CLI 会自行刷新回写,
/// 用号库里的旧 token 覆盖它会打掉刷新成果)、MCP 权限白名单、工作区。
///
/// `system` 非空时同步到工作区 AGENTS.md(内容没变就不写,免得每轮刷 mtime)。
pub fn prepare_home(
    cfg: &CliDriverConfig,
    account_id: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    system: &str,
) -> Result<(PathBuf, PathBuf), UpstreamError> {
    let home = cfg.base_dir.join(account_id);
    let ws = home.join("ws");
    let io = |what: &str, e: std::io::Error| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("cursor-cli: {what}失败: {e}"),
        )
    };
    std::fs::create_dir_all(home.join(".config/cursor")).map_err(|e| io("创建 home", e))?;
    std::fs::create_dir_all(home.join(".cursor")).map_err(|e| io("创建 .cursor", e))?;
    std::fs::create_dir_all(ws.join("assets")).map_err(|e| io("创建工作区", e))?;

    let auth_file = home.join(".config/cursor/auth.json");
    if !auth_file.exists() {
        let rt = refresh_token.unwrap_or(access_token);
        let body = json!({"accessToken": access_token, "refreshToken": rt});
        let tmp = home.join(".config/cursor/.auth.json.tmp");
        std::fs::write(&tmp, body.to_string()).map_err(|e| io("写 auth.json", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &auth_file).map_err(|e| io("落 auth.json", e))?;
    }

    // MCP 工具白名单(格式实证:Mcp(server:tool),冒号必填,支持 glob)。
    let perms = home.join(".cursor/permissions.json");
    let perms_body = r#"{"mcpAllowlist":["gwtools:*"]}"#;
    if std::fs::read_to_string(&perms).unwrap_or_default() != perms_body {
        std::fs::write(&perms, perms_body).map_err(|e| io("写 permissions.json", e))?;
    }

    if !system.is_empty() {
        let agents = ws.join("AGENTS.md");
        if std::fs::read_to_string(&agents).unwrap_or_default() != system {
            std::fs::write(&agents, system).map_err(|e| io("写 AGENTS.md", e))?;
        }
    }

    // 降权隔离:CLI 子进程以 nobody(65534)运行(见 start_conv),HOME 得归它,
    // 否则它写不了 auth 刷新与本地 transcript。/app/data 等敏感目录对 nobody
    // 不可读(部署要求 data/ 700、control.db 600,见 CHANGELOG/部署文档)。
    // 非 root 环境(本机开发)chown 会失败,忽略即可。
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("chown")
            .arg("-R")
            .arg("65534:65534")
            .arg(&home)
            .status();
    }
    Ok((home, ws))
}

/// 对外模型名 → CLI 模型名(`--model` 参数)。
pub fn cli_model_name(cursor_model: &str) -> String {
    match cursor_model {
        "default" => "auto".into(),
        "grok-4.6" => "cursor-grok-4.6-high-fast".into(),
        "grok-4.5" => "cursor-grok-4.5-high-fast".into(),
        "composer-2.5" => "composer-2.5-fast".into(),
        "claude-opus-5" => "claude-opus-5-thinking-high-fast".into(),
        "claude-sonnet-5" => "claude-sonnet-5-thinking-high".into(),
        "claude-fable-5" => "claude-fable-5-thinking-high".into(),
        "kimi-k3" => "kimi-k3-high".into(),
        other => other.to_string(),
    }
}

// ── NDJSON 事件解析 ─────────────────────────────────────────────────────────

#[derive(Default)]
struct NdjsonState {
    seen_text: String,
    session_id: Option<String>,
    result: Option<Value>,
}

enum Ev {
    Delta(String),
    Thinking(String),
    Done,
    Nothing,
}

fn handle_line(state: &mut NdjsonState, line: &str) -> Ev {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Ev::Nothing;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                state.session_id = Some(sid.to_string());
            }
            Ev::Nothing
        }
        Some("thinking") => {
            if v.get("subtype").and_then(|s| s.as_str()) == Some("delta") {
                if let Some(t) = v.get("text").and_then(|s| s.as_str()) {
                    return Ev::Thinking(t.to_string());
                }
            }
            Ev::Nothing
        }
        Some("assistant") => {
            let text: String = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|bs| {
                    bs.iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                b.get("text").and_then(|t| t.as_str())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if text.is_empty() {
                return Ev::Nothing;
            }
            // 事件三分(实测字段差异):
            // - 增量:有 timestamp_ms、无 model_call_id;
            // - 每条 model call 完结快照:有 model_call_id(内容已被增量覆盖,跳过防复读);
            // - 最终全量回显:两者皆无,只补差量。
            if v.get("model_call_id").is_some() {
                return Ev::Nothing;
            }
            if v.get("timestamp_ms").is_some() {
                state.seen_text.push_str(&text);
                Ev::Delta(text)
            } else if text.starts_with(&state.seen_text) {
                // 无时间戳 = 累积快照(每则 assistant 消息完结时一份):只补差量。
                let rest = text[state.seen_text.len()..].to_string();
                state.seen_text = text;
                if rest.is_empty() {
                    Ev::Nothing
                } else {
                    Ev::Delta(rest)
                }
            } else {
                // 快照与已发对不上(工具回路里几乎必现:最终回显只含末段文本)。
                // 丢掉是正确动作(防复读),不构成异常,debug 级即可。
                tracing::debug!("cursor-cli:全量回显与已发增量对不上,忽略该事件");
                Ev::Nothing
            }
        }
        Some("result") => {
            state.result = Some(v);
            Ev::Done
        }
        _ => Ev::Nothing,
    }
}

/// 当前进程是否 root(读 /proc,不引 libc)。
#[cfg(unix)]
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v == "0"))
        })
        .unwrap_or(false)
}

// ── 会话与输出队列 ──────────────────────────────────────────────────────────

enum OutItem {
    Item(Result<StreamItem, UpstreamError>),
    /// 阶段结束:drain 流收到它就收尾(本次 Anthropic 响应完结)。
    End,
}

/// 泵 → 响应流的单向队列。空时 drain 等在 notify 上。
struct OutQueue {
    q: Mutex<VecDeque<OutItem>>,
    notify: Notify,
}

impl OutQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            q: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        })
    }
    fn push(&self, item: OutItem) {
        self.q.lock().unwrap_or_else(|p| p.into_inner()).push_back(item);
        self.notify.notify_one();
    }
    async fn pop(&self) -> OutItem {
        loop {
            if let Some(it) = self.q.lock().unwrap_or_else(|p| p.into_inner()).pop_front() {
                return it;
            }
            self.notify.notified().await;
        }
    }
}

/// 待调用方应答的桥调用。
struct PendingSlot {
    /// 给调用方的 tool_use.id(排查日志用)。
    #[allow(dead_code)]
    tool_use_id: String,
    /// 把结果还给泵任务的通道。
    responder: oneshot::Sender<Result<String, String>>,
}

/// 一条存活中的 CLI 会话(可能正挂在桥调用上)。
pub struct CliConv {
    pub account_id: String,
    /// CLI 的 session_id(pump 拿到 init 事件后填)。
    cli_session_id: Mutex<Option<String>>,
    /// 调用方历史逐轮指纹(分叉检测,同线协议形态)。
    pub fps: Mutex<Vec<u64>>,
    out: Arc<OutQueue>,
    pending: Mutex<Option<PendingSlot>>,
    at: Mutex<Instant>,
}

impl CliConv {
    pub fn session_id(&self) -> Option<String> {
        self.cli_session_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }
    /// 取走待应答槽(消费语义)。
    fn take_pending(&self) -> Option<PendingSlot> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
    fn touch(&self) {
        *self.at.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
    fn fresh(&self) -> bool {
        self.at.lock().unwrap_or_else(|p| p.into_inner()).elapsed() < SESSION_TTL
    }
    /// 占位:子进程所有权在 pump 里(kill_on_drop 兜底),这里不做显式 kill。
    fn kill_procs(&self) {}
}

/// 会话表:我方 conversation_id → CLI 会话。
#[derive(Default)]
pub struct CliConversations {
    inner: Mutex<HashMap<String, Arc<CliConv>>>,
}

/// 本轮的会话开法。
pub enum CliLookup {
    Fresh,
    Resume(String),
}

impl CliConversations {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CliConv>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, conv: &str) -> Option<Arc<CliConv>> {
        self.map().get(conv).cloned()
    }

    pub fn insert(&self, conv: &str, entry: Arc<CliConv>) {
        let mut map = self.map();
        map.insert(conv.to_string(), entry);
        map.retain(|_, e| e.fresh());
    }

    #[allow(dead_code)] // 预留:后台清理任务用
    pub fn remove(&self, conv: &str) {
        if let Some(e) = self.map().remove(conv) {
            e.kill_procs();
        }
    }

    /// 判定本轮怎么开会话(指纹前缀校验,语义同线协议形态)。
    pub fn lookup(&self, conv: &str, account_id: &str, history_fps: &[u64]) -> CliLookup {
        let map = self.map();
        match map.get(conv) {
            Some(e) if e.account_id == account_id && e.fresh() => {
                let fps = e.fps.lock().unwrap_or_else(|p| p.into_inner());
                let prefix_ok = fps.len() <= history_fps.len()
                    && fps.iter().zip(history_fps).all(|(a, b)| a == b);
                if prefix_ok {
                    let sid = e.session_id();
                    drop(fps);
                    match sid {
                        Some(sid) => CliLookup::Resume(sid),
                        None => CliLookup::Fresh,
                    }
                } else {
                    let first_diff = fps
                        .iter()
                        .zip(history_fps)
                        .position(|(a, b)| a != b)
                        .map(|p| p as i64)
                        .unwrap_or(-1);
                    tracing::info!(
                        conversation_id = conv,
                        stored = fps.len(),
                        incoming = history_fps.len(),
                        first_diff,
                        "cursor-cli:调用方历史分叉,新会话重铺"
                    );
                    drop(fps);
                    CliLookup::Fresh
                }
            }
            _ => CliLookup::Fresh,
        }
    }
}

// ── SSE 辅助 ────────────────────────────────────────────────────────────────

struct SsePhase {
    msg_id: String,
    started: bool,
    next_idx: u32,
    open: Option<(u32, &'static str)>,
    model: String,
}

impl SsePhase {
    fn new(model: &str) -> Self {
        Self {
            msg_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            started: false,
            next_idx: 0,
            open: None,
            model: model.to_string(),
        }
    }

    fn ensure_started(&mut self, out: &OutQueue) {
        if !self.started {
            out.push(OutItem::Item(Ok(StreamItem::Sse(
                crate::chat::message_start_pub(&self.msg_id, &self.model),
            ))));
            self.started = true;
        }
    }

    fn close_block(&mut self, out: &OutQueue) {
        if let Some((idx, _)) = self.open.take() {
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            )))));
        }
    }

    fn push_text(&mut self, out: &OutQueue, kind: &'static str, text: &str, is_thinking: bool) {
        self.ensure_started(out);
        if let Some((_, k)) = self.open {
            if k != kind {
                self.close_block(out);
            }
        }
        if self.open.is_none() {
            let block = if is_thinking {
                json!({"type":"thinking","thinking":""})
            } else {
                json!({"type":"text","text":""})
            };
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_start",
                json!({"type":"content_block_start","index":self.next_idx,"content_block":block}),
            )))));
            self.open = Some((self.next_idx, kind));
            self.next_idx += 1;
        }
        if let Some((idx, _)) = self.open {
            let delta = if is_thinking {
                json!({"type":"thinking_delta","thinking":text})
            } else {
                json!({"type":"text_delta","text":text})
            };
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,"delta":delta}),
            )))));
        }
    }

    /// 发 tool_use 块,收尾本阶段(message_delta stop_reason=tool_use + message_stop + End)。
    fn finish_tool_use(&mut self, out: &OutQueue, tool_use_id: &str, name: &str, args: &Value) {
        self.ensure_started(out);
        self.close_block(out);
        let idx = self.next_idx;
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"tool_use","id":tool_use_id,"name":name,"input":{}}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,
                   "delta":{"type":"input_json_delta","partial_json":args.to_string()}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_stop",
            json!({"type":"content_block_stop","index":idx}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":"tool_use","stop_sequence":null},
                   "usage":{"input_tokens":0,"output_tokens":0}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_stop",
            json!({"type":"message_stop"}),
        )))));
        out.push(OutItem::End);
    }

    /// 正常收尾(end_turn + 真实用量)。
    fn finish_done(&mut self, out: &OutQueue, usage: &ChatUsage) {
        self.ensure_started(out);
        self.close_block(out);
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":"end_turn","stop_sequence":null},
                   "usage":crate::chat::delta_usage_json_pub(usage)}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_stop",
            json!({"type":"message_stop"}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Usage(usage.clone()))));
        out.push(OutItem::End);
    }

    fn finish_error(&mut self, out: &OutQueue, err: UpstreamError) {
        if self.started {
            self.close_block(out);
        }
        out.push(OutItem::Item(Err(err)));
        out.push(OutItem::End);
    }
}

/// drain 一条响应流(到 End 为止)。
fn drain_stream(out: Arc<OutQueue>) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    futures::stream::unfold(out, |out| async move {
        match out.pop().await {
            OutItem::Item(it) => Some((it, out)),
            OutItem::End => None,
        }
    })
}

// ── 泵任务 ──────────────────────────────────────────────────────────────────

struct PumpArgs {
    conv: Arc<CliConv>,
    cli: tokio::process::Child,
    /// 桥 socket(CLI 拉起的桥进程回连;有工具时才有)。
    sock: Option<tokio::net::UnixStream>,
    want_thinking: bool,
    echo_model: String,
}

/// 泵:读 CLI stdout + 桥 socket,把事件翻译成 SSE 写进 OutQueue。
/// 桥调用处挂起(等下一轮网关请求喂结果),CLI 进程全程存活。
async fn pump(mut a: PumpArgs) {
    let out = a.conv.out.clone();
    let mut phase = SsePhase::new(&a.echo_model);
    let mut state = NdjsonState::default();
    let started = Instant::now();

    let stdout = a.cli.stdout.take().expect("stdout piped");
    let stderr = a.cli.stderr.take();
    let stderr_task = stderr.map(|s| {
        tokio::spawn(async move {
            let mut acc = String::new();
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                acc.push_str(&l);
                acc.push('\n');
                if acc.len() > 4096 {
                    acc.drain(..2048);
                }
            }
            acc
        })
    });

    let mut cli_lines = BufReader::new(stdout).lines();
    let (mut sock_lines, mut sock_w) = match a.sock {
        Some(s) => {
            let (r, w) = s.into_split();
            (Some(BufReader::new(r).lines()), Some(w))
        }
        None => (None, None),
    };

    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 结果:Ok(()) 正常完结;Err 进 finish_error。
    let outcome: Result<(), UpstreamError> = 'outer: loop {
        tokio::select! {
            line = cli_lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break 'outer Ok(()), // EOF:CLI 退出
                    Err(e) => break 'outer Err(UpstreamError::network(format!("读 cursor-cli 输出失败: {e}"))),
                };
                // 逆向排障:原始 NDJSON 落盘(仅设了 CURSOR_CLI_DUMP_NDJSON 时)。
                if let Ok(f) = std::env::var("CURSOR_CLI_DUMP_NDJSON") {
                    use std::io::Write as _;
                    if let Ok(mut fp) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
                        let _ = writeln!(fp, "{}", line);
                    }
                }
                match handle_line(&mut state, &line) {
                    Ev::Nothing => {}
                    Ev::Thinking(t) if a.want_thinking => phase.push_text(&out, "thinking", &t, true),
                    Ev::Thinking(_) => {}
                    Ev::Delta(t) => phase.push_text(&out, "text", &t, false),
                    Ev::Done => break 'outer Ok(()),
                }
                if let Some(sid) = state.session_id.clone() {
                    let mut slot = a.conv.cli_session_id.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.is_none() {
                        *slot = Some(sid);
                    }
                }
            }
            call = async {
                match &mut sock_lines {
                    Some(l) => l.next_line().await,
                    None => std::future::pending().await,
                }
            } => {
                let call = match call {
                    Ok(Some(c)) => c,
                    _ => break 'outer Err(UpstreamError::new(UpstreamErrorKind::Other, "cursor-cli 桥连接中断".to_string())),
                };
                let Ok(v) = serde_json::from_str::<Value>(&call) else { continue };
                let Some(call) = v.get("call") else { continue };
                let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = call.get("args").cloned().unwrap_or(json!({}));
                let tool_use_id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                tracing::info!(tool = %name, id = %tool_use_id, "cursor-cli:模型调用桥接工具,转成 tool_use 并挂起");

                let (tx_slot, rx_slot) = oneshot::channel::<Result<String, String>>();
                {
                    let mut p = a.conv.pending.lock().unwrap_or_else(|p| p.into_inner());
                    *p = Some(PendingSlot { tool_use_id: tool_use_id.clone(), responder: tx_slot });
                }
                phase.finish_tool_use(&out, &tool_use_id, &name, &args);
                // 新阶段(下一个 Anthropic 响应)重新开始计数。
                phase = SsePhase::new(&a.echo_model);

                // 挂起等调用方结果(带 TTL)。
                let res = tokio::time::timeout(PENDING_TTL, rx_slot).await;
                let reply = match res {
                    Ok(Ok(Ok(text))) => json!({"result": text}),
                    Ok(Ok(Err(err))) => json!({"error": err}),
                    Ok(Err(_)) => json!({"error": "网关注销了这次调用"}),
                    Err(_) => json!({"error": format!("等待调用方 tool_result 超时({}s)", PENDING_TTL.as_secs())}),
                };
                if let Some(w) = &mut sock_w {
                    let _ = w.write_all(reply.to_string().as_bytes()).await;
                    let _ = w.write_all(b"\n").await;
                    let _ = w.flush().await;
                }
            }
            _ = tick.tick() => {
                if started.elapsed() > CLI_TIMEOUT {
                    break 'outer Err(UpstreamError::new(
                        UpstreamErrorKind::Other,
                        format!("cursor-cli 单轮超过 {}s,杀进程", CLI_TIMEOUT.as_secs()),
                    ));
                }
            }
        }
    };

    // 收尾
    a.conv.kill_procs();
    if let Some(t) = stderr_task {
        let _ = t.await.map(|s| {
            if !s.trim().is_empty() {
                tracing::debug!(stderr = %s.chars().take(300).collect::<String>(), "cursor-cli stderr");
            }
        });
    }

    match outcome {
        Ok(()) => {
            let (ok, usage) = match &state.result {
                Some(r) if r.get("subtype").and_then(|s| s.as_str()) == Some("success") => {
                    let u = r.get("usage").cloned().unwrap_or_default();
                    (
                        true,
                        ChatUsage {
                            input_tokens: u.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            output_tokens: u.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            cache_read_tokens: u.get("cacheReadTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            ..Default::default()
                        },
                    )
                }
                other => {
                    tracing::warn!(result = ?other.as_ref().map(|r| r.to_string().chars().take(200).collect::<String>()),
                        "cursor-cli: 未见成功 result 事件");
                    (false, ChatUsage::default())
                }
            };
            if ok {
                phase.finish_done(&out, &usage);
            } else {
                phase.finish_error(&out, UpstreamError::new(
                    UpstreamErrorKind::Other,
                    "cursor-cli 未成功收尾(无 success result)".to_string(),
                ));
            }
        }
        Err(e) => phase.finish_error(&out, e),
    }
}

// ── 对外入口 ────────────────────────────────────────────────────────────────

/// 开一条新的 CLI 会话(新 spawn;`lookup` 决定带不带 --resume),返回首阶段响应流。
#[allow(clippy::too_many_arguments)]
pub async fn start_conv(
    cfg: &CliDriverConfig,
    convs: &CliConversations,
    conv_key: &str,
    account_id: &str,
    home: &Path,
    ws: &Path,
    cli_model: &str,
    prompt: &str,
    resume_sid: Option<String>,
    tools: &[crate::run::ToolDef],
    want_thinking: bool,
    echo_model: &str,
) -> Result<gw_core::provider::ChatStream, UpstreamError> {
    // 同号 spawn 串行化:mcp.json 是每号一份的静态路径,桥 socket 每请求一条,
    // 等桥连上(或确认无工具)后再放行下一个,避免后一个请求改写 mcp.json 被
    // 前一个 CLI 读到。窗口 ~1s。用 tokio 锁:要跨 await 持有。
    static SPAWN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _gate = SPAWN_GATE.lock().await;

    let io = |what: &str, e: std::io::Error| {
        UpstreamError::new(UpstreamErrorKind::Other, format!("cursor-cli: {what}失败: {e}"))
    };

    // 工具桥:mcp.json + tools 文件 + unix socket 监听。**桥进程由 CLI 经
    // mcp.json 自己拉起**(stdio 是 MCP 的生命线),网关只监听回连 —— 
    // 第一版自己抢跑了一个桥,把唯一一次 accept 用掉,CLI 拉起的桥反而连不上。
    let mut listener = None;
    if !tools.is_empty() {
        let req_id = uuid::Uuid::new_v4().simple().to_string();
        let bridge_dir = home.join("bridge");
        std::fs::create_dir_all(&bridge_dir).map_err(|e| io("创建 bridge 目录", e))?;
        let sock_path = bridge_dir.join(format!("{req_id}.sock"));
        let tools_path = bridge_dir.join(format!("{req_id}.tools.json"));
        let mcp_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": serde_json::from_str::<Value>(&t.schema).unwrap_or(json!({"type":"object"})),
                })
            })
            .collect();
        std::fs::write(&tools_path, serde_json::to_string(&mcp_tools).unwrap())
            .map_err(|e| io("写 tools 文件", e))?;
        std::fs::write(
            home.join(".cursor/mcp.json"),
            json!({"mcpServers": {"gwtools": {
                "command": cfg.self_exe.to_string_lossy(),
                "args": ["--mode", "cursor-mcp-bridge",
                         "--sock", sock_path.to_string_lossy(),
                         "--tools", tools_path.to_string_lossy()],
            }}}).to_string(),
        )
        .map_err(|e| io("写 mcp.json", e))?;
        let l = tokio::net::UnixListener::bind(&sock_path).map_err(|e| io("绑 bridge socket", e))?;
        // 桥以 nobody 身份回连,socket 得放开写权限。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o666));
        }
        listener = Some(l);
    }

    let mut cmd = Command::new(&cfg.bin);
    cmd.arg("-p")
        .arg("--trust")
        .arg("--mode")
        .arg("ask") // 安全闸:read-only。绝不加 --force。
        .arg("--model")
        .arg(cli_model)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--stream-partial-output");
    if !tools.is_empty() {
        cmd.arg("--approve-mcps");
    }
    if let Some(sid) = &resume_sid {
        cmd.arg("--resume").arg(sid);
    }
    // prompt 走 stdin 不走 argv:折叠重铺的 prompt 能有几百 KB,
    // argv 上限(~2MB 但单参数 128KB)会直接 E2BIG(实测 "Argument list too long")。
    cmd.current_dir(ws)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 降权:nobody 运行。ask 模式的只读 shell 仍会真跑命令 —— 不能让模型
    // 用 `cat` 读走 /app/data 里的号库。nobody 对那些 700/600 目录无权读。
    // 仅在 root(容器)下启用;本机开发自动跳过(非 root setuid 会 EPERM)。
    #[cfg(unix)]
    if is_root() {
        use std::os::unix::process::CommandExt;
        cmd.uid(65534).gid(65534);
    }

    let mut cli = cmd.spawn().map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("cursor-cli: 启动 {} 失败: {e}", cfg.bin.display()),
        )
    })?;
    // 喂 prompt 进 stdin 并立刻关(EOF = 输入完毕)。
    if let Some(mut stdin) = cli.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let prompt_owned = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt_owned.as_bytes()).await;
            // drop stdin → EOF
        });
    }

    // CLI 已起:它会按 mcp.json 拉起桥,桥经 socket 回连。等这个回连(10s 上限,
    // CLI 冷启动要初始化 MCP,比裸进程慢)。
    let sock = match listener {
        Some(l) => match tokio::time::timeout(Duration::from_secs(10), l.accept()).await {
            Ok(Ok((s, _))) => Some(s),
            _ => return Err(UpstreamError::new(
                UpstreamErrorKind::Other,
                "cursor-cli 桥 10s 内未回连(MCP server 启动失败?)".to_string(),
            )),
        },
        None => None,
    };

    let conv = Arc::new(CliConv {
        account_id: account_id.to_string(),
        cli_session_id: Mutex::new(resume_sid),
        fps: Mutex::new(Vec::new()),
        out: OutQueue::new(),
        pending: Mutex::new(None),
        at: Mutex::new(Instant::now()),
    });
    convs.insert(conv_key, conv.clone());
    let out = conv.out.clone();
    // 子进程所有权交给 pump:pump 结束(Done/出错/超时)即 drop,kill_on_drop 收尾。
    tokio::spawn(pump(PumpArgs {
        conv,
        cli,
        sock,
        want_thinking,
        echo_model: echo_model.to_string(),
    }));
    Ok(Box::pin(drain_stream(out)))
}

/// 喂回桥调用结果(调用方带 tool_result 的下一轮请求),返回继续输出的响应流。
pub fn resume_conv(
    conv: Arc<CliConv>,
    result_text: String,
) -> Result<gw_core::provider::ChatStream, UpstreamError> {
    conv.touch();
    let slot = conv.take_pending().ok_or_else(|| {
        UpstreamError::bad_request("cursor-cli: 该会话没有等待结果的桥调用(可能已超时或重铺)")
    })?;
    slot.responder.send(Ok(result_text)).map_err(|_| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            "cursor-cli: 泵任务已退出,桥调用无法送达".to_string(),
        )
    })?;
    Ok(Box::pin(drain_stream(conv.out.clone())))
}
