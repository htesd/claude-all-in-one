//! GrokBot 0.39 驱动(`pool=bot` 账号的推理面;sand 身份 + chat 通道,故名 sandchat)。
//!
//! ## 这是什么
//!
//! cursor BOT 池的新推理面。旧面 `aiserver.v1.InferenceService/Stream` 于
//! 2026-09-09 被服务端锁死(任何 cursor JWT 一律 401 `ERROR_NOT_LOGGED_IN`);
//! 官方 grokbot 桌面端 0.39.0 起推理改走 `aiserver.v1.GrokBotService`,架构是
//! 「unary 发消息 + 流式监听 transcript」:
//!
//! - **agent 确保**:每账号惰性确保一个 temporal harness 的聊天 agent
//!   (`ListGrokBotAgents` 挑 `harness=="temporal"` 的,没有就
//!   `CreateGrokBotTemporalAgent` 建一个 `caio-relay`),agent_id 缓存进账号
//!   extra `sand_agent_id`(经 provider 的 extra 增量通道落库)。
//! - **会话绑定**:客户端会话指纹(与 inference/clidrv 同一 material 口径)→
//!   `caio-<指纹>` 的 session_id;进程内 [`SandSessions`] 记
//!   (session_id, 历史指纹链, last_updated_seq)。首包发**全量渲染文本**
//!   (system + TextEmu 工具契约 + fold_history 折叠),续包只发**增量**
//!   (前缀命中判定沿用 clidrv 的思路:已发链是本轮历史的严格前缀才续)。
//! - **发**:unary `SendGrokBotUserMessage`(application/proto 裸 body),
//!   `message_id` 是幂等键(uuid hex),transcript 里以 `clientNonce` 回显。
//! - **收**:`WatchGrokBotTranscripts`(application/connect+proto + 5 字节信封,
//!   复用 [`crate::wire::frame`])。先等我们消息的回声,再收 assistant 的
//!   `message` entry:同 seq 覆写 = 流式更新(对全文做增量 diff 发 text_delta),
//!   `isStreaming:false` = 定稿。全文本过 inference 那套 [`Folder`] +
//!   `TextToolFilter`,`<tool_call>` 文本块折叠成 Anthropic tool_use。
//!
//! 协议逐字段实锤见 `docs/grokbot-0.39-protocol-re.md`;字段号另经 0.39.0 asar
//! 内 proto.cjs 的 protobuf-es 描述符逐条核对(2026-09-09,本文件常量表)。
//!
//! ## 与旧 inference 面的关键差异
//!
//! - **没有模型选择**:send/create/agent 均无 model 字段,服务端只有一种模型
//!   (实测自述 Grok)。BOT 渠道从此是纯 grok 渠道;Anthropic 请求里的模型名
//!   只用于 SSE 回显(客户端按原名对账,与 inference 的 message_start 同口径)。
//! - **没有 token 用量**:transcript 不带 usage 字段。按文本量估算入账
//!   (见 [`Folder::set_estimated_usage`],注释注明估算口径);真实池消耗由
//!   后台 `GetSandUsageStatus`(`usage.rs`)周期核对。
//! - **工具只有文本契约一条路**:协议层没有 tools 通道,带 tools 的请求一律
//!   TextEmu —— 与 inference 的 TextEmu 门面同一份契约文本、同一个折叠器。
//!
//! ## 出口与安全(与 inference 同纪律)
//!
//! 出口 client 由调用方注入、经 [`crate::inference::inference_client`] 解析:
//! 账号配代理 → 专用 H1 client(fail-closed);没配 → worker egress client。
//! 刷新(worker 层)与发包同出口的铁律不变。401 统一分类 TokenInvalid,由
//! worker「同号强制刷新一次并重试」覆盖(gw-app worker/mod.rs 既有口径),
//! 本驱动内不做自发刷新重试(与 inference 面一致)。
//!
//! ## 已知缺口(v1 显式记录,非回归)
//!
//! - `cursor_too_old` 直接报错;TODO(二期):`ListGrokBotTranscriptEntries`
//!   权威重拉后从断点续收(请求/响应字段表已核:A7/U7)。
//! - Temporal 附件暂未接入，图片仍为占位、PDF 抽文本；box 降级路径已
//!   通过 uploadAttachment 上传内联图片/PDF，见 box_attachments 模块。
//! - 同账号多会话共用一个 temporal agent 并全程串行(run gate 排队);
//!   并发排队策略与 agent 池化是二期议题(协议文档「待解」)。
//! - `GetGrokBotSendStatus` 的送达轮询/重发未接(v1 靠回声等待 + 超时兜底)。
//! - grok 的思考是否出现在 transcript 未实锤;若有,当前按正文文本下发。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, StreamItem};
use serde_json::Value as Json;

use crate::protobuf::{Reader, Value as PVal, Writer};
use crate::wire;

mod box_gateway;
mod box_attachments;

/// GrokBotService 的 RPC 根(官方 `SAND_BACKEND_URL` 缺省值,0.39.0 实锤)。
const BASE: &str = "https://api2.cursor.sh/aiserver.v1.GrokBotService";

/// 官方客户端头表(0.39.0 main-app.cjs `w7` 函数逐项核对):
/// `x-cursor-client-type: sand`、`x-cursor-client-version: <app 版本>`、
/// `x-sand-box-namespace: prod`、`x-ghost-mode`、`x-request-id`。
const CLIENT_TYPE: &str = "sand";
const CLIENT_VERSION: &str = "0.39.0";
const BOX_NAMESPACE: &str = "prod";

/// 我们自建的 agent 名(挑 agent 时优先认领它,避免认到用户手工建的)。
const AGENT_NAME: &str = "caio-relay";

/// GrokBotClientSurface:DESKTOP(proto 枚举值 1,描述符 `JW`)。
const SOURCE_DESKTOP: u64 = 1;
/// GrokBotAgentHarnessKind:TEMPORAL(枚举值 2;1=BOX 是 computer-use 通道,不用)。
const HARNESS_TEMPORAL: u64 = 2;
/// GrokBotUserMessageDelivery:REFUSED(枚举值 4;1/2/3 = 接受/接受/幂等重复)。
const DELIVERY_REFUSED: u64 = 4;

/// watch 请求的 `inline_body_max_bytes`(body 超阈值会被 body_omitted 截断,
/// 10MB 对文本 transcript 绰绰有余)。
const INLINE_BODY_MAX_BYTES: u64 = 10_000_000;

/// unary RPC 整请求超时(官方客户端 sendPrompt 也用 60s 级 deadline)。
const UNARY_TIMEOUT: Duration = Duration::from_secs(60);
/// watch 等响应头的上限(建流通常秒级)。
const WATCH_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
/// 帧间停滞兜底(规格:120s;服务端有心跳,真停滞才会触发)。
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 单次运行总时长兜底(规格:10min)。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
/// 定稿后无新行的收尾窗口(规格:3s)。
const FINAL_QUIET: Duration = Duration::from_secs(3);
/// 会话表条目 TTL(对齐 clidrv 的 SESSION_TTL;过期条目续不上就全量重铺)。
const SESSION_TTL: Duration = Duration::from_secs(2 * 3600);

/// temporal 门控记忆的 TTL:到点重探一次(上游可能对该号开放)。
const TEMPORAL_GATED_TTL: Duration = Duration::from_secs(3600);
/// 中断 RPC 的尽力而为超时(客户端已断开,别为它等太久)。
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);

// ── 门面开关(默认开)─────────────────────────────────────────────────────────
//
// pool=bot 的唯一活路:旧 inference 端点已死,开关关掉时 pool=bot 账号无面可走
// (lib.rs 路由处直接报错,不落 inference)。
// 开关链:env `CURSOR_SANDCHAT`(默认开,`0`/`false` 关)→ yaml 基线 →
// 热配置 `cursor_sandchat`(见 gw-core config.rs,与 CURSOR_TEXT_TOOLS 同款)。
static SANDCHAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// sandchat 门面当前是否开(进程级,worker 启动/30s 轮询设置时热应用)。
pub fn sandchat_enabled() -> bool {
    SANDCHAT.load(std::sync::atomic::Ordering::Relaxed)
}

/// 热设置 sandchat 门面开关。
pub fn set_sandchat_enabled(v: bool) {
    SANDCHAT.store(v, std::sync::atomic::Ordering::Relaxed);
}

// ── 账号 extra 增量通道 ─────────────────────────────────────────────────────
//
// 与 clidrv 的 TokenUpdates 同一条 `Provider::poll_token_updates` 通道取走,
// worker 周期任务 merge 落库。目前只装 `sand_agent_id`(agent 确保的产物)。

/// account_id → 待落库的 extra 增量键值。
pub(crate) type ExtraUpdates = Arc<Mutex<HashMap<String, BTreeMap<String, Json>>>>;

/// 记录一条账号 extra 增量(同号同键只留最新)。
fn report_extra(updates: &ExtraUpdates, account_id: &str, key: &str, value: Json) {
    updates
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(account_id.to_string())
        .or_default()
        .insert(key.to_string(), value);
}

// ── 会话表(进程内)──────────────────────────────────────────────────────────

/// 一条已建立的 grokbot 会话绑定。
struct SandSession {
    /// 服务端会话 id(`caio-<指纹>`;重开带短后缀,见 [`SandSessions::lookup`])。
    session_id: String,
    /// 上一次**成功**请求的历史指纹链(增量判定用;失败不登记 —— 服务端可能
    /// 没落轮,乐观登记会让下一次只发增量而服务端缺轮,与 ConvRegistry 同教训)。
    fps: Vec<u64>,
    /// watch 见过的最大 updated_seq(续收游标)。
    last_updated_seq: u64,
    touched: Instant,
}

/// 会话查找结果。
enum SandLookup {
    /// 无记录:全新会话。
    New,
    /// 有记录但历史对不上(分叉 / 同长重试 / 增量尾部无 user 轮):
    /// 另开新会话(session_id 带随机后缀,避开服务端旧 transcript)。
    Restart,
    /// 前缀命中:续会话,发增量。
    Resume {
        session_id: String,
        prefix_len: usize,
        last_updated_seq: u64,
    },
}

/// sandchat 的进程内状态:会话绑定表 + agent 确保缓存 + 两道闸门。
///
/// 会话历史在**服务端**(transcript 属于某账号的某 agent),所以键带 account_id
/// (换号 = 失忆,天然 New —— 与 ConvRegistry「会话属于某一个账号」同口径)。
#[derive(Default)]
pub(crate) struct SandSessions {
    /// box 默认会话不支持 temporal session_id，按客户端会话隔离 agent。
    box_state: box_gateway::State,
    /// (account_id, 会话指纹) → 绑定。
    map: Mutex<HashMap<(String, String), SandSession>>,
    /// account_id → 已确保的 agent_id(进程内真值;extra 落库有轮询延迟)。
    agents: Mutex<HashMap<String, String>>,
    /// agent 确保的 single-flight(按号;并发请求不重复 List/Create)。
    agent_gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 同账号串行闸门:一个 temporal agent 是单对话,多会话并发只能排队
    /// (v1 接受的简化,见模块文档「已知缺口」)。
    run_gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// account_id → 记录时刻:服务端对该号关闭了 temporal harness
    /// (CreateGrokBotTemporalAgent → failed_precondition「harness is not available」)。
    /// 门控是账号级服务端状态,期间内请求直接干净失败(ModelNotAvailable,
    /// 不伤账号健康),不再重复 List/Create 空转;TTL 到期自动重探。
    temporal_gated: Mutex<HashMap<String, Instant>>,
}

impl SandSessions {
    fn gate(
        gates: &Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
        account_id: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = gates.lock().unwrap_or_else(|p| p.into_inner());
        // 账号数受池大小约束,无界增长风险低;与 config_gates 同形态(不做淘汰)。
        m.entry(account_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn agent_gate(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Self::gate(&self.agent_gates, account_id)
    }

    fn run_gate(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Self::gate(&self.run_gates, account_id)
    }

    fn agent_of(&self, account_id: &str) -> Option<String> {
        self.agents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(account_id)
            .cloned()
    }

    /// 该号是否已被服务端判 temporal 门控(且在 TTL 内)。
    fn is_temporal_gated(&self, account_id: &str) -> bool {
        let mut m = self.temporal_gated.lock().unwrap_or_else(|p| p.into_inner());
        match m.get(account_id) {
            Some(t) if t.elapsed() < TEMPORAL_GATED_TTL => true,
            Some(_) => {
                m.remove(account_id);
                false
            }
            None => false,
        }
    }

    fn mark_temporal_gated(&self, account_id: &str) {
        self.temporal_gated
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account_id.to_string(), Instant::now());
    }

    fn set_agent(&self, account_id: &str, agent_id: &str) {
        self.agents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account_id.to_string(), agent_id.to_string());
    }

    /// 剔除缓存的 agent_id(Send 报 not_found = 服务端已删,需要重建)。
    fn evict_agent(&self, account_id: &str) {
        self.agents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(account_id);
    }

    /// 增量判定(沿用 clidrv 的思路):已发指纹链是本轮历史的**严格前缀**才续。
    ///
    /// 严格(不等长):同长相同 = 客户端原样重试上一轮,续发会把同一条用户消息
    /// 再投一次;降级 Restart 全量重开,幂等键不撞、旧 transcript 不受污染。
    fn lookup(&self, account_id: &str, conv_fp: &str, fps: &[u64]) -> SandLookup {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let Some(e) = map.get(&(account_id.to_string(), conv_fp.to_string())) else {
            return SandLookup::New;
        };
        if e.touched.elapsed() > SESSION_TTL {
            return SandLookup::New; // 过期按新会话处理(条目留待 insert 时机会清理)
        }
        let prefix_ok =
            e.fps.len() < fps.len() && e.fps.iter().zip(fps).all(|(a, b)| a == b);
        if prefix_ok {
            SandLookup::Resume {
                session_id: e.session_id.clone(),
                prefix_len: e.fps.len(),
                last_updated_seq: e.last_updated_seq,
            }
        } else {
            let first_diff = e
                .fps
                .iter()
                .zip(fps)
                .position(|(a, b)| a != b)
                .map(|p| p as i64)
                .unwrap_or(-1);
            tracing::info!(
                account = %account_id,
                stored = e.fps.len(),
                incoming = fps.len(),
                first_diff,
                "sandchat:历史前缀不匹配(分叉/重试/prefill),另开新会话全量重铺"
            );
            SandLookup::Restart
        }
    }

    /// 成功后登记(下一次的增量基准)。失败路径**不调**(见 [`SandSession::fps`])。
    fn insert(&self, account_id: &str, conv_fp: &str, session: SandSession) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        // 机会主义 GC:阈值摊销,防长进程按历史会话数慢漏(clidrv 桶同款)。
        if map.len() > 1024 {
            map.retain(|_, e| e.touched.elapsed() <= SESSION_TTL);
        }
        map.insert((account_id.to_string(), conv_fp.to_string()), session);
    }
}

// ── proto 编解码(字段号见模块文档;逐条核对自 0.39.0 proto.cjs 描述符)──────

/// `SendGrokBotUserMessage` 请求:
/// `{1 agent_id, 2 message_id, 3 text, 4 sent_at_ms, 13 source=DESKTOP, 15 session_id}`。
fn encode_send(agent_id: &str, message_id: &str, text: &str, session_id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(1, agent_id);
    w.string(2, message_id);
    w.string(3, text);
    let sent_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    w.uint(4, sent_at_ms);
    w.uint(13, SOURCE_DESKTOP);
    w.string(15, session_id);
    w.into_bytes()
}

/// `CreateGrokBotTemporalAgent` 请求(与 CreateGrokBotAgent 同类型):
/// `{2 name, 8 agent_id(客户端生成 uuid), 9 harness=TEMPORAL, 11 introduction_suppressed}`。
fn encode_create_agent(agent_id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(2, AGENT_NAME);
    w.string(8, agent_id);
    w.uint(9, HARNESS_TEMPORAL);
    w.bool(11, true); // introduction_suppressed:不要开场白(烧了也是噪声)
    w.into_bytes()
}

/// `WatchGrokBotTranscripts` 请求:
/// `{1 cursors repeated, 3 inline_body_max_bytes}`;
/// cursor = `{1 agent_id, 2 generation, 3 after_updated_seq, 4 session_id}`。
/// generation 不掌握,缺省 0(cleared 帧才带新 generation,v1 不追)。
fn encode_watch(agent_id: &str, session_id: &str, after_updated_seq: u64) -> Vec<u8> {
    let mut c = Writer::new();
    c.string(1, agent_id);
    c.uint(3, after_updated_seq);
    c.string(4, session_id);
    let mut w = Writer::new();
    w.message(1, &c);
    w.uint(3, INLINE_BODY_MAX_BYTES);
    w.into_bytes()
}

/// `InterruptGrokBotAgentRun` 请求:`{1 agent_id, 2 reason, 3 session_id opt}`。
fn encode_interrupt(agent_id: &str, session_id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(1, agent_id);
    w.string(2, "client_disconnected");
    w.string(3, session_id);
    w.into_bytes()
}

/// `ListGrokBotAgents` 响应里我们关心的三个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentInfo {
    agent_id: String,
    name: String,
    harness: String,
}

/// GrokBotAgent `{3 name, 12 agent_id, 13 harness}`(其余字段用不到)。
fn decode_agent(sub: &[u8]) -> AgentInfo {
    let mut a = AgentInfo {
        agent_id: String::new(),
        name: String::new(),
        harness: String::new(),
    };
    for (f, v) in Reader::new(sub) {
        if let PVal::Len(s) = v {
            let text = String::from_utf8_lossy(s).into_owned();
            match f {
                3 => a.name = text,
                12 => a.agent_id = text,
                13 => a.harness = text,
                _ => {}
            }
        }
    }
    a
}

/// `ListGrokBotAgentsResponse {1 agents repeated}`。
fn decode_list_agents(bytes: &[u8]) -> Vec<AgentInfo> {
    let mut out = Vec::new();
    for (f, v) in Reader::new(bytes) {
        if let (1, PVal::Len(sub)) = (f, v) {
            out.push(decode_agent(sub));
        }
    }
    out
}

/// `CreateGrokBotAgentResponse {1 agent}` → agent_id。
fn decode_create_agent_resp(bytes: &[u8]) -> Result<String, UpstreamError> {
    for (f, v) in Reader::new(bytes) {
        if let (1, PVal::Len(sub)) = (f, v) {
            let a = decode_agent(sub);
            if !a.agent_id.is_empty() {
                return Ok(a.agent_id);
            }
        }
    }
    Err(UpstreamError::new(
        UpstreamErrorKind::ServerError,
        "sandchat: CreateGrokBotTemporalAgent 响应缺 agent(协议漂移?)",
    ))
}

/// `SendGrokBotUserMessageResponse` 的处置结果。
#[derive(Debug)]
enum SendOutcome {
    /// 接受(ACCEPTED_BOX=1 / ACCEPTED_TEMPORAL=2 / DUPLICATE=3 幂等命中)。
    Accepted(u64),
    /// REFUSED=4:带 refusal{failure_code, message}。
    Refused { code: String, message: String },
}

/// `SendGrokBotUserMessageResponse {1 dispatched, 2 mode, 3 workflow_id, 4 delivery, 5 refusal}`。
fn decode_send_resp(bytes: &[u8]) -> SendOutcome {
    let mut delivery = 0u64;
    let mut refusal: Option<(String, String)> = None;
    for (f, v) in Reader::new(bytes) {
        match (f, v) {
            (4, PVal::Varint(n)) => delivery = n,
            (5, PVal::Len(sub)) => {
                let (mut code, mut msg) = (String::new(), String::new());
                for (ff, vv) in Reader::new(sub) {
                    if let PVal::Len(s) = vv {
                        match ff {
                            1 => code = String::from_utf8_lossy(s).into_owned(),
                            2 => msg = String::from_utf8_lossy(s).into_owned(),
                            _ => {}
                        }
                    }
                }
                refusal = Some((code, msg));
            }
            _ => {}
        }
    }
    if delivery == DELIVERY_REFUSED {
        let (code, message) = refusal.unwrap_or_default();
        SendOutcome::Refused { code, message }
    } else {
        SendOutcome::Accepted(delivery)
    }
}

// ── watch 帧解码 ─────────────────────────────────────────────────────────────

/// transcript 一行(GrokBotTranscriptEntry):
/// `{1 seq, 2 entry_kind, 3 body opt, 5 updated_seq, 7 body_omitted}`(4 blob_hash /
/// 6 entry_id 用不到)。
#[derive(Debug, Clone)]
struct WatchEntry {
    seq: u64,
    kind: String,
    body: Vec<u8>,
    updated_seq: u64,
    body_omitted: bool,
}

fn decode_entry(sub: &[u8]) -> WatchEntry {
    let mut e = WatchEntry {
        seq: 0,
        kind: String::new(),
        body: Vec::new(),
        updated_seq: 0,
        body_omitted: false,
    };
    for (f, v) in Reader::new(sub) {
        match (f, v) {
            (1, PVal::Varint(n)) => e.seq = n,
            (2, PVal::Len(s)) => e.kind = String::from_utf8_lossy(s).into_owned(),
            (3, PVal::Len(s)) => e.body = s.to_vec(),
            (5, PVal::Varint(n)) => e.updated_seq = n,
            (7, PVal::Varint(n)) => e.body_omitted = n != 0,
            _ => {}
        }
    }
    e
}

/// GrokBotTranscriptWatchFrame 的 oneof(字段号逐条核对):
/// 1 connected / 2 rows / 3 cleared / 4 cursor_too_old / 5 heartbeat /
/// 6 agent_state / 7 computer_actions / 8 agent_state_changed。
enum WatchFrame {
    Connected { stream_id: String },
    /// (session_id, entries)
    Rows { session_id: String, entries: Vec<WatchEntry> },
    Cleared,
    CursorTooOld,
    /// (agent_id, 忙碌 = is_running || is_composing_message, session_id)
    AgentState(Vec<(String, bool, String)>),
    Heartbeat,
    /// 认识的 oneof 但我们不消费的(computer_actions / agent_state_changed)。
    Ignored,
}

fn decode_watch_frame(payload: &[u8]) -> Result<WatchFrame, UpstreamError> {
    let mut out = WatchFrame::Ignored;
    let mut seen = false;
    for (f, v) in Reader::new(payload) {
        let PVal::Len(sub) = v else { continue };
        seen = true;
        match f {
            1 => {
                // connected {1 stream_id, 2 server_time_ms, 3 absolute_lifetime_ms}
                let mut stream_id = String::new();
                for (ff, vv) in Reader::new(sub) {
                    if let (1, PVal::Len(s)) = (ff, vv) {
                        stream_id = String::from_utf8_lossy(s).into_owned();
                    }
                }
                out = WatchFrame::Connected { stream_id };
            }
            2 => {
                // rows {1 agent_id, 2 generation, 3 entries, 4 deletes, 5 replay, 6 session_id}
                let mut session_id = String::new();
                let mut entries = Vec::new();
                for (ff, vv) in Reader::new(sub) {
                    match (ff, vv) {
                        (3, PVal::Len(e)) => entries.push(decode_entry(e)),
                        (6, PVal::Len(s)) => {
                            session_id = String::from_utf8_lossy(s).into_owned()
                        }
                        _ => {}
                    }
                }
                out = WatchFrame::Rows { session_id, entries };
            }
            3 => out = WatchFrame::Cleared,
            4 => out = WatchFrame::CursorTooOld,
            5 => out = WatchFrame::Heartbeat,
            6 => {
                // agent_state {1 live repeated GrokBotAgentLiveState, 2 snapshot}
                // live: {1 agent_id, 2 is_running, 3 is_composing_message, 11 session_id}
                let mut live = Vec::new();
                for (ff, vv) in Reader::new(sub) {
                    if let (1, PVal::Len(ls)) = (ff, vv) {
                        let (mut id, mut running, mut composing, mut sid) =
                            (String::new(), false, false, String::new());
                        for (fff, vvv) in Reader::new(ls) {
                            match (fff, vvv) {
                                (1, PVal::Len(s)) => {
                                    id = String::from_utf8_lossy(s).into_owned()
                                }
                                (2, PVal::Varint(n)) => running = n != 0,
                                (3, PVal::Varint(n)) => composing = n != 0,
                                (11, PVal::Len(s)) => {
                                    sid = String::from_utf8_lossy(s).into_owned()
                                }
                                _ => {}
                            }
                        }
                        live.push((id, running || composing, sid));
                    }
                }
                out = WatchFrame::AgentState(live);
            }
            _ => out = WatchFrame::Ignored,
        }
    }
    // 一个 oneof 字段都没见到 = 空帧/协议漂移;不是错(当心跳处理),但留痕。
    if !seen {
        tracing::warn!("sandchat: watch 空帧(协议漂移?)");
        out = WatchFrame::Ignored;
    }
    Ok(out)
}

// ── 文本增量 diff ────────────────────────────────────────────────────────────

/// 流式覆写的增量:content 按 append 增长,前缀一致 → 余下即增量。
/// 非前缀覆写(改写)时 Anthropic SSE 无法回收已发文本,按最长公共前缀后的
/// 余量下发并留痕(可能与前文重复,但总比拼错位置强)。
fn text_delta(prev: &str, new: &str) -> String {
    if let Some(stripped) = new.strip_prefix(prev) {
        return stripped.to_string();
    }
    let mut i = 0usize;
    for (a, b) in prev.chars().zip(new.chars()) {
        if a != b {
            break;
        }
        i += a.len_utf8();
    }
    tracing::warn!(
        prev_len = prev.len(),
        new_len = new.len(),
        common = i,
        "sandchat: 同 seq 覆写不是前缀增长(改写?),按最长公共前缀补发"
    );
    new[i..].to_string()
}

// ── transcript body(JSON)的判定 ─────────────────────────────────────────────

/// 回声判定:我们发出去的消息进了 transcript。
///
/// 官方 renderer 按 `kind=="message" && role=="user" && clientNonce==nonce` 认
/// 自己的消息(0.39.0 index 包 `Cgt`/`dG`);逆向文档另记录 `send-message`
/// 回执形态(clientNonce/requestId 在顶层或 `message` 子对象)。两种都收。
/// 返回 Some(requestId)(可能没有)。
fn nonce_of(v: &Json) -> Option<&str> {
    v.get("clientNonce").and_then(Json::as_str)
}

fn request_id_of(v: &Json) -> Option<String> {
    v.get("requestId")
        .and_then(Json::as_str)
        .map(str::to_string)
}

fn match_echo(body: &Json, message_id: &str) -> Option<Option<String>> {
    let kind = body.get("kind").and_then(Json::as_str)?;
    match kind {
        "message"
            if body.get("role").and_then(Json::as_str) == Some("user")
                && nonce_of(body) == Some(message_id) =>
        {
            Some(request_id_of(body))
        }
        "send-message"
            if nonce_of(body) == Some(message_id)
                || body.get("message").and_then(nonce_of) == Some(message_id) =>
        {
            Some(request_id_of(body).or_else(|| body.get("message").and_then(request_id_of)))
        }
        _ => None,
    }
}

/// assistant 的 `message` body → (content, is_streaming)。
/// `role:"user"` 是用户消息回显(回声门已处理);renderer 里 `fromUser` 标记
/// 也算 assistant 侧(0.39.0 `gg` 函数),一并认。
fn as_assistant_message(body: &Json) -> Option<(&str, bool)> {
    if body.get("kind").and_then(Json::as_str)? != "message" {
        return None;
    }
    if body.get("role").and_then(Json::as_str) == Some("user") {
        return None;
    }
    let content = body.get("content").and_then(Json::as_str)?;
    let streaming = body
        .get("isStreaming")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    Some((content, streaming))
}

// ── 一次运行的归集器(纯状态机,无网络,可测)─────────────────────────────────

/// 回声门 → 逐 seq 覆写 diff → 定稿。文本一律经 [`Folder`]
///(text_tools 开着时 `<tool_call>` 块在 Folder 内被 TextToolFilter 切出)。
struct Collector {
    folder: crate::inference::Folder,
    /// 本轮消息的 message_id(= transcript 里的 clientNonce)。
    message_id: String,
    /// 我们的 user 消息是否已进 transcript。回声之前的行一律不收
    ///(可能是旧轮重放/别的运行残留)。
    echo_seen: bool,
    /// 回声里的 requestId(观测用;条目相关性按位置,不按它过滤 ——
    /// 单 agent 单会话串行,回声之后的 assistant message 就是本轮的)。
    #[allow(dead_code)]
    request_id: Option<String>,
    /// seq → 已收到的 assistant 消息全文(同 seq 覆写 = 流式更新)。
    contents: HashMap<u64, String>,
    /// 正在跟随的 assistant 消息 seq(多段回复时切换到最新一条)。
    current_seq: Option<u64>,
    /// current_seq 已收到 `isStreaming:false`(定稿)。
    finalized: bool,
    /// agent_state 帧里本 agent 的忙碌观测(None = 还没收到过)。
    agent_busy: Option<bool>,
    /// 见过的最大 updated_seq(重连游标 / 会话表回写)。
    last_updated_seq: u64,
}

impl Collector {
    fn new(folder: crate::inference::Folder, message_id: String, after_updated_seq: u64) -> Self {
        Self {
            folder,
            message_id,
            echo_seen: false,
            request_id: None,
            contents: HashMap::new(),
            current_seq: None,
            finalized: false,
            agent_busy: None,
            last_updated_seq: after_updated_seq,
        }
    }

    /// 喂一行 entry。返回「本轮定稿且 agent 已知不在跑」(= 可以提前收尾)。
    fn feed_entry(&mut self, e: &WatchEntry) {
        self.last_updated_seq = self.last_updated_seq.max(e.updated_seq);
        if e.body_omitted {
            // inline_body_max_bytes=10MB 不该触发;TODO(二期):blob_hash 通道拉取。
            tracing::warn!(seq = e.seq, "sandchat: entry body 被省略(body_omitted),跳过");
            return;
        }
        if e.kind != "message" && e.kind != "send-message" {
            // tool-call / notice / event / user-attachment / spend-initiation:
            // agent 自己的内部行,与客户端工具无关(v1 不消费)。
            return;
        }
        let body: Json = match serde_json::from_slice(&e.body) {
            Ok(j) => j,
            Err(err) => {
                tracing::debug!(seq = e.seq, kind = %e.kind, "sandchat: entry body 不是 JSON,跳过: {err}");
                return;
            }
        };
        if !self.echo_seen {
            if let Some(rid) = match_echo(&body, &self.message_id) {
                self.echo_seen = true;
                self.request_id = rid;
                tracing::debug!(seq = e.seq, "sandchat: 收到本轮消息回声");
            }
            return;
        }
        let Some((content, streaming)) = as_assistant_message(&body) else {
            return;
        };
        if self.current_seq != Some(e.seq) {
            // 新一条 assistant 消息(多段回复):接着往后写;定稿后另起一条 = 还没完。
            self.current_seq = Some(e.seq);
            self.contents.insert(e.seq, String::new());
            self.finalized = false;
        }
        let prev = self.contents.get(&e.seq).cloned().unwrap_or_default();
        let delta = text_delta(&prev, content);
        if !delta.is_empty() {
            self.folder.on_text(&delta, false);
        }
        self.contents.insert(e.seq, content.to_string());
        if !streaming {
            self.finalized = true;
        }
    }

    /// 喂一次 agent_state 观测(只关心本 agent)。
    fn feed_agent_state(&mut self, agent_id: &str, live: &[(String, bool, String)]) {
        if let Some((_, busy, _)) = live.iter().find(|(id, _, _)| id == agent_id) {
            self.agent_busy = Some(*busy);
        }
    }

    /// 收尾条件①:定稿且 agent_state 显示本 agent 不在跑。
    fn done_by_state(&self) -> bool {
        self.finalized && self.agent_busy == Some(false)
    }
}

// ── 渲染(TextEmu 契约形态;指纹与发送文本共用同一份)────────────────────────

/// render 产物:全量/增量共用同一份 turns —— 这是指纹一致性的根
///(两处各渲各的,前缀判定迟早漂移)。
struct Rendered {
    /// 逐轮文本(契约形态),原顺序。
    turns: Vec<crate::run::Turn>,
    /// 逐轮指纹(`chat::turn_fp`,与 clidrv 同函数)。
    fps: Vec<u64>,
    /// system + 工具契约的合并段(可能为空)。
    head: String,
    /// 尾部提醒(带 tools 且末轮 user 时非空;消融实验定位,见
    /// inference.rs `textemu_tail_reminder` 注释)。
    tail: String,
}

/// assistant 消息 → 契约文本:text 原样,tool_use 渲回模型当初写出的
/// `<tool_call>` 文本形态(与 inference TextEmu 门面逐字节同式);thinking 丢弃
///(transcript 没有思考通道,回传原文只会让 grok 鹦鹉学舌)。
fn render_assistant(content: Option<&Json>) -> String {
    let mut out = String::new();
    match content {
        Some(Json::String(s)) => out.push_str(s),
        Some(Json::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Json::as_str) {
                    Some("text") => crate::inference::push_text(
                        &mut out,
                        b.get("text").and_then(Json::as_str).unwrap_or(""),
                    ),
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Json::as_str).unwrap_or("");
                        let input = b
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".into());
                        crate::inference::push_text(
                            &mut out,
                            &format!(
                                "<tool_call>{{\"name\":{},\"arguments\":{}}}</tool_call>",
                                serde_json::to_string(name).unwrap_or_default(),
                                input
                            ),
                        );
                    }
                    _ => {} // thinking/redacted_thinking/未知块:丢
                }
            }
        }
        _ => {}
    }
    out
}

/// user 消息 → 契约文本:text 原样;tool_result 渲成 `<tool_result name=…>`
///(复用 inference 的 `tool_results_as_text`,名字从 id→name 映射回填);
/// 图片没有上传通道(v1),留占位;PDF 抽文本层内联(与 inference 同形,
/// 同一份 12MB/24MB 媒体预算)。
fn render_user(
    content: Option<&Json>,
    names: &HashMap<String, String>,
    doc_n: &mut usize,
    doc_budget: &mut usize,
) -> String {
    let mut out = String::new();
    let Some(content) = content else { return out };
    match content {
        Json::String(s) => out.push_str(s),
        Json::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Json::as_str) {
                    Some("text") => crate::inference::push_text(
                        &mut out,
                        b.get("text").and_then(Json::as_str).unwrap_or(""),
                    ),
                    Some("tool_result") => {
                        let text = crate::inference::tool_results_as_text(
                            std::slice::from_ref(b),
                            names,
                            doc_n,
                        );
                        crate::inference::push_text(&mut out, &text);
                    }
                    Some("image") => crate::inference::push_text(
                        &mut out,
                        "[image omitted: grokbot channel has no attachment upload (v1)]",
                    ),
                    Some("document") => {
                        // 预算预检(b64 长度估原始大小,别先解出 200MB):
                        // 与 inference 门控同一份常量。
                        let b64_len = b
                            .get("source")
                            .and_then(|s| s.get("data"))
                            .and_then(Json::as_str)
                            .map(|d| d.len() / 4 * 3)
                            .unwrap_or(usize::MAX);
                        if b64_len > crate::inference::MAX_ONE_IMAGE
                            || *doc_budget + b64_len > crate::inference::MAX_ALL_IMAGES
                        {
                            crate::inference::push_text(
                                &mut out,
                                "[document omitted: 超媒体预算]",
                            );
                            continue;
                        }
                        *doc_budget += b64_len;
                        match crate::inference::document_inject_text(b, doc_n) {
                            Some(t) => crate::inference::push_text(&mut out, &t),
                            None => crate::inference::push_text(
                                &mut out,
                                "[document omitted: base64 解码失败]",
                            ),
                        }
                    }
                    // CC 工具搜索的延迟工具引用:让模型看到名字,别静默丢。
                    Some("tool_reference") => {
                        let tn = b.get("tool_name").and_then(Json::as_str).unwrap_or("?");
                        crate::inference::push_text(&mut out, &format!("[tool_reference: {tn}]"));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// Anthropic 请求体 → 契约形态的逐轮渲染 + 指纹 + 头尾段。
fn render(body: &Json) -> Result<Rendered, UpstreamError> {
    let messages = body
        .get("messages")
        .and_then(Json::as_array)
        .ok_or_else(|| UpstreamError::bad_request("sandchat: 请求缺 messages"))?;
    if messages.is_empty() {
        return Err(UpstreamError::bad_request("sandchat: 请求 messages 为空"));
    }

    // system + TextEmu 工具契约(与 inference build_request 同 merge 形态)。
    let sys = crate::chat::extract_system(body);
    let tools = body.get("tools").and_then(Json::as_array);
    let has_tools = tools.is_some_and(|t| !t.is_empty());
    let tool_block = tools
        .filter(|t| !t.is_empty())
        .map(|t| crate::inference::tools_text_block(t));
    let head = match (sys.is_empty(), tool_block) {
        (false, Some(b)) => format!("{sys}\n\n{b}"),
        (false, None) => sys,
        (true, Some(b)) => b,
        (true, None) => String::new(),
    };

    // 先扫一遍 assistant 的 tool_use,建 id→name 映射(tool_result 回填名字用,
    // 与 inference build_request 同一步骤)。
    let mut names: HashMap<String, String> = HashMap::new();
    for m in messages {
        if m.get("role").and_then(Json::as_str) != Some("assistant") {
            continue;
        }
        if let Some(blocks) = m.get("content").and_then(Json::as_array) {
            for b in blocks {
                if b.get("type").and_then(Json::as_str) == Some("tool_use") {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(Json::as_str),
                        b.get("name").and_then(Json::as_str),
                    ) {
                        names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    let mut doc_n = 0usize;
    let mut doc_budget = 0usize;
    let mut turns = Vec::with_capacity(messages.len());
    for m in messages {
        let role = m.get("role").and_then(Json::as_str).unwrap_or("user");
        let is_user = role != "assistant";
        let text = if is_user {
            render_user(m.get("content"), &names, &mut doc_n, &mut doc_budget)
        } else {
            render_assistant(m.get("content"))
        };
        // 空轮留占位(与 chat::to_turns 同形态):整条丢掉会让指纹链错位。
        let text = if text.is_empty() {
            "(unsupported content omitted)".to_string()
        } else {
            text
        };
        turns.push(crate::run::Turn { text, is_user });
    }
    let fps = turns
        .iter()
        .map(|t| crate::chat::turn_fp(t.is_user, &t.text))
        .collect();

    // 尾部提醒只在末轮 user 时钉(prefill 形态下钉在 assistant 后面语义就反了,
    // 与 inference 的门控一致)。
    let last_is_user = messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Json::as_str)
        == Some("user");
    let tail = if has_tools && last_is_user {
        crate::inference::textemu_tail_reminder(tools.expect("has_tools 时 tools 恒 Some"))
    } else {
        String::new()
    };

    Ok(Rendered {
        turns,
        fps,
        head,
        tail,
    })
}

/// 全量渲染(新会话首包):头段 + fold_history 折叠 + 尾部提醒。
fn render_full(r: &Rendered) -> String {
    // fold_history 把多轮折成一条 user 消息(<conversation_history> 包裹,
    // 与 wire/clidrv 同一份折叠格式);turns 恒非空(render 已挡空 messages)。
    let folded = crate::chat::fold_history(&r.turns, None);
    let mut s = String::new();
    if !r.head.is_empty() {
        s.push_str(&r.head);
        s.push_str("\n\n");
    }
    s.push_str(&folded[0].text);
    if !r.tail.is_empty() {
        s.push_str("\n\n");
        s.push_str(&r.tail);
    }
    s
}

/// 增量渲染(续包):已发前缀之后的尾部。
///
/// **丢弃开头连续的 assistant 轮**:那是模型自己写进 transcript 的回复,
/// 重发只会重复。其后必须全是 user 轮;中间再夹 assistant(交错/编辑)就
/// 超出增量语义,回 None 让调用方全量重开。
fn render_incremental(r: &Rendered, prefix_len: usize) -> Option<String> {
    let suffix = &r.turns[prefix_len.min(r.turns.len())..];
    let first_user = suffix.iter().position(|t| t.is_user)?;
    let tail_turns = &suffix[first_user..];
    if tail_turns.iter().any(|t| !t.is_user) {
        return None;
    }
    let mut s = tail_turns
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if s.is_empty() {
        return None;
    }
    // 尾部提醒每包都钉(模型最后读到的位置;与 inference 每请求都带同口径)。
    if !r.tail.is_empty() {
        s.push_str("\n\n");
        s.push_str(&r.tail);
    }
    Some(s)
}

// ── HTTP 层 ──────────────────────────────────────────────────────────────────

/// 每条 GrokBot RPC 的头表(0.39.0 客户端 `w7` 同款,逐项核对)。
///
/// ⚠️ checksum 只带 machineId:sand 客户端的 `Xp(machineId)` 只吃一个参数,
/// **没有** IDE glass 面的 `/macMachineId` 后缀 —— 别照抄 inference 面的头。
fn with_sand_headers(
    rb: reqwest::RequestBuilder,
    account: &Account,
    token: &str,
) -> reqwest::RequestBuilder {
    let machine_id = crate::CursorProvider::machine_id_of(account, token);
    rb.header("authorization", format!("Bearer {token}"))
        .header("x-cursor-checksum", wire::checksum(&machine_id, None))
        .header("x-cursor-client-type", CLIENT_TYPE)
        .header("x-cursor-client-version", CLIENT_VERSION)
        .header("x-sand-box-namespace", BOX_NAMESPACE)
        // 官方 w7 恒发;缺省 true(不训练,最保守),账号级 extra.ghost_mode
        // 可覆盖(与 inference 面同约定)。
        .header(
            "x-ghost-mode",
            account
                .extra
                .get("ghost_mode")
                .and_then(Json::as_str)
                .unwrap_or("true"),
        )
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
}

/// unary RPC(裸 proto body)。非 200 走 inference 同款结构化分类
///(401 → TokenInvalid → worker 同号刷新重试)。
async fn unary(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    method: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, UpstreamError> {
    let submission = method == "SendGrokBotUserMessage";
    let request_error = |e: UpstreamError| {
        if submission { e.with_delivery(gw_core::error::RequestDelivery::Unknown) } else { e }
    };
    let url = format!("{BASE}/{method}");
    let rb = client
        .post(&url)
        .header("content-type", "application/proto")
        .body(body);
    let resp = tokio::time::timeout(UNARY_TIMEOUT, with_sand_headers(rb, account, token).send())
        .await
        .map_err(|_| {
            request_error(UpstreamError::network(format!(
                "sandchat {method} 等响应超时({}s)",
                UNARY_TIMEOUT.as_secs()
            )))
        })?
        .map_err(|e| request_error(UpstreamError::network(format!("sandchat {method} 请求发送失败: {e}"))))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| request_error(UpstreamError::network(format!("sandchat {method} 读响应体失败: {e}"))))?;
    if status != 200 {
        let e = crate::inference::classify_http_error_for(
            "sandchat",
            status,
            &String::from_utf8_lossy(&bytes),
        );
        return Err(if submission && matches!(status, 400 | 401 | 403 | 404 | 429) {
            e.with_delivery(gw_core::error::RequestDelivery::NotSubmitted)
        } else { request_error(e) });
    }
    Ok(bytes.to_vec())
}

/// 开 watch 流(connect+proto + 信封;裸 proto 会被 415,实测见协议文档)。
async fn open_watch(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    agent_id: &str,
    session_id: &str,
    after_updated_seq: u64,
) -> Result<reqwest::Response, UpstreamError> {
    let rb = client
        .post(format!("{BASE}/WatchGrokBotTranscripts"))
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .header("te", "trailers")
        .body(wire::frame(&encode_watch(
            agent_id,
            session_id,
            after_updated_seq,
        )));
    let resp = tokio::time::timeout(
        WATCH_HEADER_TIMEOUT,
        with_sand_headers(rb, account, token).send(),
    )
    .await
    .map_err(|_| {
        UpstreamError::network(format!(
            "sandchat watch 等响应头超时({}s)",
            WATCH_HEADER_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| UpstreamError::network(format!("sandchat watch 请求发送失败: {e}")))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.bytes().await.unwrap_or_default();
        return Err(crate::inference::classify_http_error_for(
            "sandchat",
            status,
            &String::from_utf8_lossy(&body),
        ));
    }
    Ok(resp)
}

/// 发一条用户消息;REFUSED 上抛带 refusal.message 的错误。
async fn send_message(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    agent_id: &str,
    message_id: &str,
    text: &str,
    session_id: &str,
) -> Result<(), UpstreamError> {
    let bytes = unary(
        client,
        account,
        token,
        "SendGrokBotUserMessage",
        encode_send(agent_id, message_id, text, session_id),
    )
    .await?;
    match decode_send_resp(&bytes) {
        // DUPLICATE = 同 message_id 重发命中幂等键,也算成功(消息已落)。
        SendOutcome::Accepted(d) => {
            tracing::debug!(delivery = d, "sandchat: 消息已被接受");
            Ok(())
        }
        SendOutcome::Refused { code, message } => Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!("sandchat 消息被 harness 拒绝(delivery=REFUSED[{code}]): {message}"),
        )),
    }
}

/// 客户端断开 / 中途放弃时喊停 agent(best-effort;空烧额度比失败更贵)。
async fn interrupt_run(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    agent_id: &str,
    session_id: &str,
) {
    let fut = unary(
        client,
        account,
        token,
        "InterruptGrokBotAgentRun",
        encode_interrupt(agent_id, session_id),
    );
    match tokio::time::timeout(INTERRUPT_TIMEOUT, fut).await {
        Ok(Ok(_)) => tracing::info!(agent_id, session_id, "sandchat: 已中断 agent 运行"),
        other => tracing::warn!(
            agent_id,
            session_id,
            "sandchat: 中断 agent 运行失败(尽力而为,忽略): {other:?}"
        ),
    }
}

// ── agent 确保 ───────────────────────────────────────────────────────────────

/// temporal 门控的对外错误:ModelNotAvailable —— 不伤账号健康、客户端拿中性
/// 「模型不可用」文案(原始 failed_precondition 报文只进日志,指纹纪律),
/// 换号尝试受调度上限约束(其它 bot 号同样门控时快速耗尽,回 503 系)。
fn temporal_gated_err() -> UpstreamError {
    UpstreamError::new(
        UpstreamErrorKind::ModelNotAvailable,
        "sandchat: 该号 temporal harness 未开放(上游门控),渠道暂不可服务",
    )
}

/// 确保本账号有一个 temporal 聊天 agent:进程内缓存 → 账号 extra `sand_agent_id`
/// → List 挑 temporal(优先我们的 caio-relay)→ 都没有就 Create。
///
/// `exclude`:Send 报 not_found 后的重建路径 —— 跳过两级缓存并排除已死的 id。
async fn ensure_agent(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    sessions: &SandSessions,
    updates: &ExtraUpdates,
    exclude: Option<&str>,
) -> Result<String, UpstreamError> {
    // 门控记忆先行:已知该号 temporal 未开放就不再 List/Create 空转(每个
    // 请求省两次 RPC,也避免在上游反复撞「harness is not available」)。
    if sessions.is_temporal_gated(&account.account_id) {
        return Err(temporal_gated_err());
    }
    if exclude.is_none() {
        // 进程内真值先行(extra 落库有轮询延迟,进程内 map 是本 worker 的权威)。
        if let Some(id) = sessions.agent_of(&account.account_id) {
            return Ok(id);
        }
        if let Some(id) = account
            .extra
            .get("sand_agent_id")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let id = id.to_string();
            sessions.set_agent(&account.account_id, &id);
            return Ok(id);
        }
    }
    let gate = sessions.agent_gate(&account.account_id);
    let _g = gate.lock().await;
    // 拿到闸门后重查:排在前面那位可能已经建好/认领好了。
    if exclude.is_none() {
        if let Some(id) = sessions.agent_of(&account.account_id) {
            return Ok(id);
        }
    }
    let bytes = unary(client, account, token, "ListGrokBotAgents", Vec::new()).await?;
    let agents = decode_list_agents(&bytes);
    let pick = agents
        .iter()
        .find(|a| {
            a.harness == "temporal" && a.name == AGENT_NAME && Some(a.agent_id.as_str()) != exclude
        })
        .or_else(|| {
            agents
                .iter()
                .find(|a| a.harness == "temporal" && Some(a.agent_id.as_str()) != exclude)
        });
    let id = match pick {
        Some(a) => {
            tracing::info!(
                account = %account.account_id,
                agent_id = %a.agent_id,
                name = %a.name,
                "sandchat: 认领现有 temporal agent"
            );
            a.agent_id.clone()
        }
        None => {
            let new_id = uuid::Uuid::new_v4().to_string();
            let resp = match unary(
                client,
                account,
                token,
                "CreateGrokBotTemporalAgent",
                encode_create_agent(&new_id),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    // 服务端门控(2026-09-09 实测:Ultra 与新号一律
                    // failed_precondition「The Temporal Grok Bot harness is not
                    // available」)—— 是账号级状态而非瞬时故障,记下来快速失败,
                    // 别让它走普通 BadRequest 逐请求空转。
                    if e.kind == UpstreamErrorKind::BadRequest
                        && e.message.contains("harness is not available")
                    {
                        sessions.mark_temporal_gated(&account.account_id);
                        tracing::warn!(
                            account = %account.account_id,
                            "sandchat: 该号 temporal harness 未开放(服务端门控),记入缓存快速失败"
                        );
                        return Err(temporal_gated_err());
                    }
                    return Err(e);
                }
            };
            let got = decode_create_agent_resp(&resp)?;
            if got != new_id {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    format!(
                        "sandchat: 建 agent 响应的 agent_id({got})与请求({new_id})不符(协议漂移?)"
                    ),
                ));
            }
            tracing::info!(account = %account.account_id, agent_id = %new_id,
                "sandchat: 已创建 temporal agent(caio-relay)");
            new_id
        }
    };
    sessions.set_agent(&account.account_id, &id);
    // 写回 extra(worker 周期任务取走 merge 落库;丢一次也只是下次重 List,不致命)。
    report_extra(
        updates,
        &account.account_id,
        "sand_agent_id",
        Json::String(id.clone()),
    );
    Ok(id)
}

// ── 入口 ────────────────────────────────────────────────────────────────────

/// pool=bot 的 chat 入口:确保 agent → 会话绑定(全量/增量)→ 发消息 →
/// watch 收 transcript → Anthropic SSE。
///
/// 错误纪律与 inference 面一致:返回 Err 之前没有产出任何 SSE(重放安全),
/// 401 分类 TokenInvalid 交 worker 刷新重试,驱动级错误交调度层换号
///(pool=bot 不落 cli/wire —— 绝不跨池,见 lib.rs 路由注释)。
pub(crate) async fn chat_stream(
    egress: &reqwest::Client,
    account: &Account,
    token: &str,
    req: ChatRequest,
    ctx: &CallCtx,
    sessions: &Arc<SandSessions>,
    updates: &ExtraUpdates,
) -> Result<ChatStream, UpstreamError> {
    // 与 inference 同纪律:账号代理 → H1 专用 client(fail-closed),无代理 → egress。
    let client = crate::inference::inference_client(account, egress)?;

    // 会话指纹:与 inference/clidrv 同一 material 口径(router 下发 > cache_key
    // > 内容派生),保证「同会话」在三张面之间是同一个概念。
    let material = if !ctx.session_id.is_empty() {
        ctx.session_id.clone()
    } else if !ctx.cache_key.is_empty() {
        ctx.cache_key.clone()
    } else {
        crate::chat::affinity_key_from_body(&req.body)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    };
    let conv_fp = crate::chat::conversation_uuid(&material);

    // 先选协议面；box 先校验附件预算再渲染，不能提前抽取随后会被丢弃的 PDF。
    let agent_id = match ensure_agent(&client, account, token, sessions, updates, None).await {
        Ok(id) => id,
        Err(_) if sessions.is_temporal_gated(&account.account_id) => {
            return box_gateway::chat_stream(client, account, token, req, conv_fp, sessions).await;
        }
        Err(e) => return Err(e),
    };
    let rendered = render_blocking(req.body.clone()).await?;

    // 会话绑定:前缀命中 → 增量续包;否则全量重铺(New 用定基 session_id,
    // Restart 加随机后缀避开服务端旧 transcript)。
    let (session_id, after_seq, text) = match sessions.lookup(&account.account_id, &conv_fp, &rendered.fps) {
        SandLookup::Resume {
            session_id,
            prefix_len,
            last_updated_seq,
        } => match render_incremental(&rendered, prefix_len) {
            Some(t) => (session_id, last_updated_seq, t),
            None => {
                tracing::info!(account = %account.account_id,
                    "sandchat: 增量尾部无有效 user 内容(prefill/交错),全量重开");
                (
                    fresh_session_id(&conv_fp, true),
                    0,
                    render_full(&rendered),
                )
            }
        },
        SandLookup::New => (
            fresh_session_id(&conv_fp, false),
            0,
            render_full(&rendered),
        ),
        SandLookup::Restart => (
            fresh_session_id(&conv_fp, true),
            0,
            render_full(&rendered),
        ),
    };

    // 同账号单 agent:全程串行。guard 移进流任务,流结束才放 —— 排队是 v1
    // 接受的形态(模块文档「已知缺口」)。
    let run_guard = sessions.run_gate(&account.account_id).lock_owned().await;

    // **先开 watch 再发消息**:watch 建立失败(401 等)时消息还没落,重试安全;
    // 反过来 send 已落、watch 挂掉,调度层会把同一条用户消息重发一遍。
    // 缓存 agent 已被服务端删掉(not_found)时:剔除重建、重开 watch、同
    // message_id 重发(幂等键保证不重投)。
    let message_id = uuid::Uuid::new_v4().simple().to_string();
    let mut agent_id = agent_id;
    let mut tried_recreate = false;
    let resp = loop {
        let resp = open_watch(&client, account, token, &agent_id, &session_id, after_seq).await?;
        match send_message(&client, account, token, &agent_id, &message_id, &text, &session_id)
            .await
        {
            Ok(()) => break resp,
            Err(e) if e.kind == UpstreamErrorKind::ModelNotAvailable && !tried_recreate => {
                tried_recreate = true;
                tracing::warn!(account = %account.account_id, agent_id = %agent_id,
                    "sandchat: Send 报 not_found,缓存 agent 可能已被删除,重建后重发一次");
                sessions.evict_agent(&account.account_id);
                let dead_id = agent_id.clone();
                agent_id =
                    ensure_agent(&client, account, token, sessions, updates, Some(&dead_id))
                        .await?;
                // 旧 watch 的 cursor 绑的是死 agent,丢掉(resp 随循环体重建)。
                drop(resp);
            }
            Err(e) => return Err(e),
        }
    };

    let declared_tools: HashSet<String> = req
        .body
        .get("tools")
        .and_then(Json::as_array)
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t.get("name").and_then(Json::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // 本面工具只有文本契约一条路:带 tools 就开 TextEmu 折叠(模型按契约吐
    // `<tool_call>` 文本块);不带 tools 原样直通(模型没有契约,幻觉块按文本
    // 原样下发,与 inference 的 None 门面同口径)。thinking 没有通道,
    // show_thinking 恒 false(从不喂 on_thinking)。
    let folder = crate::inference::Folder::new(
        &req.model,
        declared_tools.clone(),
        false,
        text.len() as u64,
        !declared_tools.is_empty(),
    );
    let input_bytes = text.len() as u64;
    let collector = Collector::new(folder, message_id, after_seq);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);
    let task_state = TaskCtx {
        client: client.clone(),
        account: account.clone(),
        token: token.to_string(),
        agent_id,
        session_id: session_id.clone(),
        conv_fp,
        fps: rendered.fps.clone(),
        sessions: sessions.clone(),
        input_bytes,
    };
    tokio::spawn(watch_loop(resp, collector, tx.clone(), task_state, run_guard));

    Ok(submitted_stream(rx))
}

/// 发消息已经得到接纳确认，后续 watch/补读错误不得触发整轮重发。
fn submitted_stream(rx: tokio::sync::mpsc::Receiver<Result<StreamItem, UpstreamError>>) -> ChatStream {
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(|item| {
        item.map_err(|e| e.with_delivery(gw_core::error::RequestDelivery::Submitted))
    }))
}

/// `render` 里可能含 PDF 文本抽取(同步 CPU 活),与 inference 同一套
/// 「spawn_blocking + 进程级并发槽」纪律。
async fn render_blocking(body: Json) -> Result<Rendered, UpstreamError> {
    let permit = if crate::inference::has_document_block(&body) {
        Some(
            crate::inference::PDF_EXTRACT_SLOTS
                .acquire()
                .await
                .map_err(|_| UpstreamError::new(UpstreamErrorKind::Other, "PDF 抽取槽已关闭"))?,
        )
    } else {
        None
    };
    let result = tokio::task::spawn_blocking(move || render(&body))
        .await
        .map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::Other,
                format!("sandchat: 渲染任务异常退出: {e}"),
            )
        })?;
    drop(permit);
    result
}

/// session_id 形态:`caio-<会话指纹>`;重开(分叉/重试/prefill 降级)加随机
/// 后缀,避开服务端旧 transcript。
fn fresh_session_id(conv_fp: &str, suffix: bool) -> String {
    if suffix {
        format!("caio-{conv_fp}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
    } else {
        format!("caio-{conv_fp}")
    }
}

/// 流任务的共享上下文(重连/中断/会话登记都要用)。
struct TaskCtx {
    client: reqwest::Client,
    account: Account,
    token: String,
    agent_id: String,
    session_id: String,
    conv_fp: String,
    fps: Vec<u64>,
    sessions: Arc<SandSessions>,
    input_bytes: u64,
}

/// watch 读帧主循环(在独立任务里跑;run_guard 随任务结束释放)。
///
/// 结束条件(规格):定稿 + (agent_state 显示本 agent 不在跑) 或 定稿后 3s
/// 无新行;兜底 idle 120s(帧间停滞)/ 总长 10min。watch 在 lifetime 到期或
/// 网络抖动 EOF 时**未定稿就重连**(游标带最新 last_updated_seq;同 seq 覆写
/// 语义让重放幂等,不会重复下发)。
async fn watch_loop(
    first_resp: reqwest::Response,
    mut c: Collector,
    tx: tokio::sync::mpsc::Sender<Result<StreamItem, UpstreamError>>,
    st: TaskCtx,
    _run_guard: tokio::sync::OwnedMutexGuard<()>,
) {
    let total = tokio::time::sleep(TOTAL_TIMEOUT);
    tokio::pin!(total);
    // 定稿后的静默窗口(任何新行都会重置;见「结束条件」)。
    let mut final_quiet: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    // None = 需要(重)建流(EOF/END 未定稿时清空它,循环顶部统一重连,
    // 游标带最新 last_updated_seq;同 seq 覆写语义让重放幂等)。
    let mut resp_slot: Option<reqwest::Response> = Some(first_resp);

    'conn: loop {
        let resp = match resp_slot.take() {
            Some(r) => r,
            None => match open_watch(
                &st.client,
                &st.account,
                &st.token,
                &st.agent_id,
                &st.session_id,
                c.last_updated_seq,
            )
            .await
            {
                Ok(r) => {
                    tracing::info!(agent_id = %st.agent_id, "sandchat: watch 已重连续收");
                    r
                }
                Err(e) => {
                    c.folder.fail(e);
                    break 'conn;
                }
            },
        };
        let mut dec = wire::FrameDecoder::new();
        let mut stream = resp.bytes_stream();
        loop {
            let next = tokio::select! {
                // 客户端断开(rx 关闭)立即停止并喊停 agent(空烧额度比失败更贵);
                // 否则 lease 已释放、agent 还在跑,实际并发会突破账号上限。
                _ = tx.closed() => {
                    interrupt_run(&st.client, &st.account, &st.token, &st.agent_id, &st.session_id).await;
                    return;
                }
                _ = &mut total => {
                    tracing::warn!(agent_id = %st.agent_id, "sandchat: 单轮总长超 {}s 兜底", TOTAL_TIMEOUT.as_secs());
                    interrupt_run(&st.client, &st.account, &st.token, &st.agent_id, &st.session_id).await;
                    break 'conn;
                }
                _ = async { final_quiet.as_mut().expect("guard 保证 Some").await },
                    if final_quiet.is_some() => {
                    // 定稿后 3s 无新行:收尾。
                    break 'conn;
                }
                r = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => r,
            };
            let chunk = match next {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => {
                    c.folder
                        .fail(UpstreamError::network(format!("sandchat 读上游流失败: {e}")));
                    break 'conn;
                }
                Ok(None) => {
                    // EOF(无 END):定稿了就收尾(流已尽,回复已定);
                    // 未定稿 → 清槽重连(resp_slot 已是 None,回 'conn 顶部)。
                    if c.finalized {
                        break 'conn;
                    }
                    tracing::info!(agent_id = %st.agent_id, "sandchat: watch EOF 未定稿,重连续收");
                    continue 'conn;
                }
                Err(_) => {
                    // 帧间停滞兜底(服务端有心跳,真停滞才会到这)。
                    tracing::warn!(agent_id = %st.agent_id,
                        "sandchat: watch 停滞超 {}s,放弃本轮", IDLE_TIMEOUT.as_secs());
                    interrupt_run(&st.client, &st.account, &st.token, &st.agent_id, &st.session_id)
                        .await;
                    if !c.folder.saw_content() {
                        c.folder.fail(UpstreamError::network(format!(
                            "sandchat 上游停滞超过 {}s",
                            IDLE_TIMEOUT.as_secs()
                        )));
                    }
                    break 'conn;
                }
            };
            dec.feed(&chunk);
            loop {
                match dec.try_next_frame() {
                    Ok(Some((flag, payload))) if flag & 0x02 != 0 => {
                        // END trailer(JSON):有错 → 失败终态;无错 → 寿命到期,
                        // 未定稿就清槽重连,定稿了就收尾。
                        match trailer_error(&payload) {
                            Some(e) => c.folder.fail(e),
                            None if !c.finalized => continue 'conn,
                            None => {}
                        }
                        break 'conn;
                    }
                    Ok(Some((flag, payload))) => {
                        let data = match wire::frame_payload(flag, &payload) {
                            Ok(d) => d,
                            Err(e) => {
                                c.folder.fail(UpstreamError::new(
                                    UpstreamErrorKind::ServerError,
                                    format!("sandchat 帧解压失败: {e}"),
                                ));
                                break 'conn;
                            }
                        };
                        match decode_watch_frame(&data) {
                            Ok(frame) => {
                                feed_frame(&mut c, &st, frame);
                                for item in c.folder.take_pending() {
                                    if tx.send(item).await.is_err() {
                                        // 客户端断开:喊停后走人。
                                        interrupt_run(
                                            &st.client,
                                            &st.account,
                                            &st.token,
                                            &st.agent_id,
                                            &st.session_id,
                                        )
                                        .await;
                                        return;
                                    }
                                }
                                // 静默窗口:定稿状态下任何新行都重置 3s。
                                final_quiet = if c.finalized {
                                    Some(Box::pin(tokio::time::sleep(FINAL_QUIET)))
                                } else {
                                    None
                                };
                                if c.done_by_state() {
                                    break 'conn;
                                }
                                if c.folder.is_failed() {
                                    break 'conn;
                                }
                            }
                            Err(e) => {
                                c.folder.fail(e);
                                break 'conn;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        c.folder.fail(UpstreamError::new(
                            UpstreamErrorKind::ServerError,
                            format!("sandchat 帧解码失败: {e}"),
                        ));
                        break 'conn;
                    }
                }
            }
            // 内层帧循环正常结束(缓冲不足一帧)→ 回 select 等下一包。
        }
    }

    // 统一出口:失败终态只发 Err;正常路径估算用量入账后 finish(exactly-once)。
    if c.folder.is_failed() {
        for item in c.folder.take_pending() {
            let _ = tx.send(item).await;
        }
        return;
    }
    // transcript 无 token 计数:按文本量估算(发送文本字节/4 + 产出字符/4,
    // 见 Folder::set_estimated_usage 注释)。**这是估算口径**,偏差已知。
    c.folder.set_estimated_usage(st.input_bytes);
    c.folder.finish();
    for item in c.folder.take_pending() {
        let _ = tx.send(item).await;
    }
    if c.echo_seen {
        // 成功才登记(失败不登记的理由见 SandSession::fps)。
        st.sessions.insert(
            &st.account.account_id,
            &st.conv_fp,
            SandSession {
                session_id: st.session_id,
                fps: st.fps,
                last_updated_seq: c.last_updated_seq,
                touched: Instant::now(),
            },
        );
    }
}

/// 喂一个解码后的 watch 帧。
fn feed_frame(c: &mut Collector, st: &TaskCtx, frame: WatchFrame) {
    match frame {
        WatchFrame::Connected { stream_id } => {
            tracing::debug!(stream_id, "sandchat: watch 已连接");
        }
        WatchFrame::Rows {
            session_id,
            entries,
        } => {
            // 防御:游标按 session 过滤了,服务端仍可能夹带(快照帧);
            // 带 session_id 且不是我们的就跳。
            if !session_id.is_empty() && session_id != st.session_id {
                return;
            }
            for e in &entries {
                c.feed_entry(e);
            }
        }
        WatchFrame::AgentState(live) => c.feed_agent_state(&st.agent_id, &live),
        WatchFrame::Cleared => {
            // transcript 被清空(新 generation):本轮语义已崩,报错不猜。
            c.folder.fail(UpstreamError::new(
                UpstreamErrorKind::ServerError,
                "sandchat: transcript 被清空(cleared 帧),本轮中止",
            ));
        }
        WatchFrame::CursorTooOld => {
            // TODO(二期):ListGrokBotTranscriptEntries 权威重拉后从断点续收
            // (请求/响应字段表已核:A7/U7)。v1 直接报错,别静默丢行。
            c.folder.fail(UpstreamError::new(
                UpstreamErrorKind::ServerError,
                "sandchat: watch 游标过旧(cursor_too_old),v1 不支持权威重拉,请重试",
            ));
        }
        WatchFrame::Heartbeat | WatchFrame::Ignored => {}
    }
}

/// watch 的 END trailer(JSON)。Some(e) = 有错;None = 干净结束(寿命到期)。
/// 本面无 gzip(官方没开 connect 压缩),payload 直接按 JSON 解。
fn trailer_error(payload: &[u8]) -> Option<UpstreamError> {
    if payload.is_empty() {
        return None;
    }
    let json: Json = match serde_json::from_slice(payload) {
        Ok(j) => j,
        Err(e) => {
            return Some(UpstreamError::new(
                UpstreamErrorKind::ServerError,
                format!("sandchat END trailer 不是合法 JSON(协议漂移?): {e}"),
            ))
        }
    };
    let err = json.get("error")?;
    let code = err.get("code").and_then(Json::as_str).unwrap_or("");
    let message = err
        .get("message")
        .and_then(Json::as_str)
        .unwrap_or("上游错误");
    Some(crate::inference::map_connect_error_for("sandchat", code, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 提交后续收鉴权失败保留类别并禁止重放() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(Err(UpstreamError::new(UpstreamErrorKind::TokenInvalid,
            "watch 重连被拒"))).await.unwrap();
        drop(tx);
        let mut stream = submitted_stream(rx);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::TokenInvalid);
        assert_eq!(error.delivery, gw_core::error::RequestDelivery::Submitted);
        assert!(stream.next().await.is_none());
    }
    use serde_json::json;

    // ── 测试用解码小工具(与 inference.rs 测试同款)──
    fn fields(buf: &[u8]) -> Vec<(u32, PVal<'_>)> {
        Reader::new(buf).collect()
    }
    fn len_of<'a>(fs: &[(u32, PVal<'a>)], no: u32) -> Option<&'a [u8]> {
        fs.iter().find_map(|(f, v)| match (f, v) {
            (n, PVal::Len(s)) if *n == no => Some(*s),
            _ => None,
        })
    }
    fn var_of(fs: &[(u32, PVal)], no: u32) -> Option<u64> {
        fs.iter().find_map(|(f, v)| match (f, v) {
            (n, PVal::Varint(x)) if *n == no => Some(*x),
            _ => None,
        })
    }
    fn str_of(buf: &[u8], no: u32) -> Option<String> {
        len_of(&fields(buf), no).map(|s| String::from_utf8_lossy(s).into_owned())
    }

    #[test]
    fn send_request_roundtrip() {
        let b = encode_send("agent-1", "deadbeef", "你好", "caio-fp");
        let fs = fields(&b);
        assert_eq!(
            len_of(&fs, 1).map(|s| String::from_utf8_lossy(s).into_owned()),
            Some("agent-1".into())
        );
        assert_eq!(str_of(&b, 2).as_deref(), Some("deadbeef"));
        assert_eq!(str_of(&b, 3).as_deref(), Some("你好"));
        assert!(var_of(&fs, 4).is_some(), "sent_at_ms 必填");
        assert_eq!(var_of(&fs, 13), Some(SOURCE_DESKTOP));
        assert_eq!(str_of(&b, 15).as_deref(), Some("caio-fp"));
    }

    #[test]
    fn create_agent_request_roundtrip() {
        let b = encode_create_agent("uuid-9");
        let fs = fields(&b);
        assert_eq!(str_of(&b, 2).as_deref(), Some(AGENT_NAME));
        assert_eq!(str_of(&b, 8).as_deref(), Some("uuid-9"));
        assert_eq!(var_of(&fs, 9), Some(HARNESS_TEMPORAL));
        assert_eq!(var_of(&fs, 11), Some(1), "introduction_suppressed=true");
    }

    #[test]
    fn watch_request_roundtrip() {
        let b = encode_watch("agent-1", "caio-fp", 42);
        let fs = fields(&b);
        let cursor = len_of(&fs, 1).expect("cursors[0]");
        assert_eq!(str_of(cursor, 1).as_deref(), Some("agent-1"));
        assert_eq!(var_of(&fields(cursor), 3), Some(42));
        assert_eq!(str_of(cursor, 4).as_deref(), Some("caio-fp"));
        assert_eq!(var_of(&fs, 3), Some(INLINE_BODY_MAX_BYTES));
    }

    #[test]
    fn interrupt_request_roundtrip() {
        let b = encode_interrupt("agent-1", "caio-fp");
        assert_eq!(str_of(&b, 1).as_deref(), Some("agent-1"));
        assert_eq!(str_of(&b, 2).as_deref(), Some("client_disconnected"));
        assert_eq!(str_of(&b, 3).as_deref(), Some("caio-fp"));
    }

    /// 手工构造 ListGrokBotAgentsResponse:temporal(caio-relay) + box + temporal(别的名)。
    #[test]
    fn list_agents_decode_and_pick() {
        let mut a1 = Writer::new();
        a1.string(3, "caio-relay");
        a1.string(12, "id-relay");
        a1.string(13, "temporal");
        let mut a2 = Writer::new();
        a2.string(3, "boxy");
        a2.string(12, "id-box");
        a2.string(13, "box");
        let mut a3 = Writer::new();
        a3.string(3, "mine");
        a3.string(12, "id-temporal-2");
        a3.string(13, "temporal");
        let mut w = Writer::new();
        w.message(1, &a3); // 乱序:先给别人的 temporal
        w.message(1, &a2);
        w.message(1, &a1);
        let agents = decode_list_agents(&w.into_bytes());
        assert_eq!(agents.len(), 3);
        // ensure_agent 的挑选口径:优先我们的 caio-relay。
        let pick = agents
            .iter()
            .find(|a| a.harness == "temporal" && a.name == AGENT_NAME)
            .or_else(|| agents.iter().find(|a| a.harness == "temporal"));
        assert_eq!(pick.map(|a| a.agent_id.as_str()), Some("id-relay"));
    }

    #[test]
    fn send_resp_decode() {
        // ACCEPTED_TEMPORAL=2
        let mut w = Writer::new();
        w.uint(1, 1);
        w.uint(4, 2);
        assert!(matches!(
            decode_send_resp(&w.into_bytes()),
            SendOutcome::Accepted(2)
        ));
        // REFUSED=4 + refusal{failure_code, message}
        let mut r = Writer::new();
        r.string(1, "busy");
        r.string(2, "agent 忙");
        let mut w = Writer::new();
        w.uint(4, 4);
        w.message(5, &r);
        match decode_send_resp(&w.into_bytes()) {
            SendOutcome::Refused { code, message } => {
                assert_eq!(code, "busy");
                assert_eq!(message, "agent 忙");
            }
            other => panic!("应为 Refused: {other:?}"),
        }
    }

    /// 造一个 rows watch 帧:frame{2: rows{3: entries…, 6: session_id}}。
    fn rows_frame(session_id: &str, entries: &[(u64, &str, &str, u64)]) -> Vec<u8> {
        let mut rows = Writer::new();
        for (seq, kind, body, us) in entries {
            let mut e = Writer::new();
            e.uint(1, *seq);
            e.string(2, kind);
            e.bytes(3, body.as_bytes());
            e.uint(5, *us);
            rows.message(3, &e);
        }
        rows.string(6, session_id);
        let mut f = Writer::new();
        f.message(2, &rows);
        f.into_bytes()
    }

    fn agent_state_frame(agent_id: &str, running: bool) -> Vec<u8> {
        let mut live = Writer::new();
        live.string(1, agent_id);
        live.uint(2, running as u64);
        let mut astate = Writer::new();
        astate.message(1, &live);
        astate.uint(2, 1); // snapshot
        let mut f = Writer::new();
        f.message(6, &astate);
        f.into_bytes()
    }

    fn new_collector(model: &str, tools: &[&str], message_id: &str) -> Collector {
        let declared: HashSet<String> = tools.iter().map(|s| s.to_string()).collect();
        let folder = crate::inference::Folder::new(model, declared, false, 100, true);
        Collector::new(folder, message_id.to_string(), 0)
    }

    /// 把 collector 的产物 drain 成 SSE (event, data) 列表 + 末尾的 Usage。
    fn drain(c: &mut Collector) -> Vec<String> {
        c.folder
            .take_pending()
            .into_iter()
            .map(|it| match it {
                Ok(StreamItem::Sse(e)) => format!("{}:{}", e.event, e.data),
                Ok(StreamItem::Usage(u)) => format!(
                    "usage:in={}/out={}",
                    u.input_tokens, u.output_tokens
                ),
                Ok(StreamItem::UpstreamCut) => "cut".into(),
                Err(e) => format!("err:{e}"),
            })
            .collect()
    }

    #[test]
    fn 折叠_回声_流式覆写_定稿_工具块() {
        let mut c = new_collector("grok-4.6", &["Bash"], "m1");
        let sid = "caio-fp";
        // ① 回声(send-message 形态,带 requestId)
        let f = rows_frame(
            sid,
            &[(
                1,
                "send-message",
                r#"{"kind":"send-message","clientNonce":"m1","requestId":"r1"}"#,
                1,
            )],
        );
        match decode_watch_frame(&f).unwrap() {
            WatchFrame::Rows { session_id, entries } => {
                assert_eq!(session_id, sid);
                for e in &entries {
                    c.feed_entry(e);
                }
            }
            _ => panic!("应为 rows 帧"),
        }
        assert!(c.echo_seen);
        assert!(drain(&mut c).is_empty(), "回声本身不产文本");

        // ② 同 seq 覆写 3 次(流式)+ 定稿;内容含 <tool_call> 块
        let frames = [
            r#"{"kind":"message","role":"assistant","content":"好的,","isStreaming":true}"#,
            r#"{"kind":"message","role":"assistant","content":"好的,先查。<tool_call>{\"name\":\"Bash\",\"arguments\":{\"cmd\":\"ls\"}}</tool_call>","isStreaming":true}"#,
            r#"{"kind":"message","role":"assistant","content":"好的,先查。<tool_call>{\"name\":\"Bash\",\"arguments\":{\"cmd\":\"ls\"}}</tool_call>完毕","isStreaming":false}"#,
        ];
        for (i, body) in frames.iter().enumerate() {
            let f = rows_frame(sid, &[(2, "message", body, 2 + i as u64)]);
            match decode_watch_frame(&f).unwrap() {
                WatchFrame::Rows { entries, .. } => {
                    for e in &entries {
                        c.feed_entry(e);
                    }
                }
                _ => panic!(),
            }
        }
        assert!(c.finalized, "定稿帧(isStreaming:false)应置 finalized");

        let ev = drain(&mut c);
        // 文本 delta:「好的,」「先查。」;工具块整块吐出。注意 TextToolFilter 的
        // 滞回缓冲:末尾不足一个标签前缀的尾巴(「完毕」)压在过滤器里,finish 才放。
        let text_deltas: Vec<&String> = ev
            .iter()
            .filter(|e| e.starts_with("content_block_delta:"))
            .collect();
        let joined = text_deltas
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("好的,"), "首个 delta 丢失: {joined}");
        assert!(joined.contains("先查。"), "流式 diff delta 丢失: {joined}");
        // 工具块被 TextToolFilter 切出,折叠成 tool_use
        let tool = ev
            .iter()
            .find(|e| e.contains("\"tool_use\"") && e.contains("\"Bash\""));
        assert!(tool.is_some(), "工具块未折叠成 tool_use: {ev:?}");
        let args = ev.iter().find(|e| e.contains("input_json_delta"));
        assert!(
            args.is_some_and(|e| e.contains("cmd") && e.contains("ls")),
            "工具参数未下发: {ev:?}"
        );

        // ③ agent_state 显示不在跑 → done_by_state
        let f = agent_state_frame("agent-1", false);
        match decode_watch_frame(&f).unwrap() {
            WatchFrame::AgentState(live) => c.feed_agent_state("agent-1", &live),
            _ => panic!("应为 agent_state 帧"),
        }
        assert!(c.done_by_state());

        // ④ 收尾:估算用量 + finish,滞回尾巴放出、stop_reason=tool_use
        c.folder.set_estimated_usage(400);
        c.folder.finish();
        let ev = drain(&mut c);
        assert!(
            ev.iter().any(|e| e.contains("完毕")),
            "finish 应放出滞回尾巴: {ev:?}"
        );
        let delta = ev.iter().find(|e| e.starts_with("message_delta:"));
        assert!(
            delta.is_some_and(|e| e.contains("\"tool_use\"")),
            "stop_reason 应为 tool_use: {ev:?}"
        );
        assert!(
            ev.iter().any(|e| e == "usage:in=100/out=4"),
            "估算用量(input 400/4=100;output 在 finish 前估算,过滤器滞回的\
             尾巴(「完毕」6 字节)不计入,故 (18)/4=4): {ev:?}"
        );
    }

    #[test]
    fn 回声也认_message_role_user_形态() {
        // 官方 renderer 的认领形态:kind=message + role=user + clientNonce。
        let mut c = new_collector("grok-4.6", &[], "m2");
        let f = rows_frame(
            "caio-x",
            &[(
                1,
                "message",
                r#"{"kind":"message","role":"user","content":"你好","clientNonce":"m2","isStreaming":false}"#,
                1,
            )],
        );
        match decode_watch_frame(&f).unwrap() {
            WatchFrame::Rows { entries, .. } => {
                for e in &entries {
                    c.feed_entry(e);
                }
            }
            _ => panic!(),
        }
        assert!(c.echo_seen, "message+role:user+clientNonce 应被认作回声");
        // 回声之后 role=user 的行(别人的回声/旧轮)不收
        let f = rows_frame(
            "caio-x",
            &[(
                2,
                "message",
                r#"{"kind":"message","role":"user","content":"别人的","isStreaming":false}"#,
                2,
            )],
        );
        match decode_watch_frame(&f).unwrap() {
            WatchFrame::Rows { entries, .. } => {
                for e in &entries {
                    c.feed_entry(e);
                }
            }
            _ => panic!(),
        }
        assert!(drain(&mut c).is_empty(), "role=user 的行不该产出文本");
    }

    #[test]
    fn 回声之前的行一律不收() {
        let mut c = new_collector("grok-4.6", &[], "m3");
        // 旧轮 assistant 消息(重放)在回声之前到达
        let f = rows_frame(
            "caio-x",
            &[(
                7,
                "message",
                r#"{"kind":"message","role":"assistant","content":"旧轮内容","isStreaming":false}"#,
                7,
            )],
        );
        match decode_watch_frame(&f).unwrap() {
            WatchFrame::Rows { entries, .. } => {
                for e in &entries {
                    c.feed_entry(e);
                }
            }
            _ => panic!(),
        }
        assert!(!c.echo_seen);
        assert!(drain(&mut c).is_empty(), "回声前的行必须被门挡住");
        assert_eq!(c.last_updated_seq, 7, "游标照样推进(重连不断档)");
    }

    #[test]
    fn text_delta_前缀增长与改写() {
        assert_eq!(text_delta("abc", "abcdef"), "def");
        assert_eq!(text_delta("", "abc"), "abc");
        assert_eq!(text_delta("abc", "abc"), "");
        // 改写:按最长公共前缀补发
        assert_eq!(text_delta("abc", "abxy"), "xy");
    }

    #[test]
    fn watch_frame_解码各形态() {
        // connected
        let mut conn = Writer::new();
        conn.string(1, "stream-9");
        let mut f = Writer::new();
        f.message(1, &conn);
        match decode_watch_frame(&f.into_bytes()).unwrap() {
            WatchFrame::Connected { stream_id } => assert_eq!(stream_id, "stream-9"),
            _ => panic!(),
        }
        // cursor_too_old
        let mut cto = Writer::new();
        cto.string(1, "agent-1");
        let mut f = Writer::new();
        f.message(4, &cto);
        assert!(matches!(
            decode_watch_frame(&f.into_bytes()).unwrap(),
            WatchFrame::CursorTooOld
        ));
        // cleared
        let mut cl = Writer::new();
        cl.string(1, "agent-1");
        let mut f = Writer::new();
        f.message(3, &cl);
        assert!(matches!(
            decode_watch_frame(&f.into_bytes()).unwrap(),
            WatchFrame::Cleared
        ));
        // heartbeat(只有 varint 字段的帧 → Ignored)
        let mut hb = Writer::new();
        hb.uint(1, 12345);
        let mut f = Writer::new();
        f.message(5, &hb);
        assert!(matches!(
            decode_watch_frame(&f.into_bytes()).unwrap(),
            WatchFrame::Heartbeat
        ));
    }

    #[test]
    fn 渲染_全量含契约与折叠历史() {
        let body = json!({
            "system": [{"text": "sys"}],
            "messages": [
                {"role": "user", "content": "记住 4712"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "好的"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]},
                {"role": "user", "content": "那个数字是?"}
            ],
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}}]
        });
        let r = render(&body).unwrap();
        assert_eq!(r.turns.len(), 4);
        // assistant 历史渲回 <tool_call> 契约形态
        assert!(
            r.turns[1]
                .text
                .contains(r#"<tool_call>{"name":"Bash","arguments":{"cmd":"ls"}}</tool_call>"#),
            "{}",
            r.turns[1].text
        );
        // tool_result 渲成 <tool_result name=…>(名字从 id 映射回填)
        assert!(
            r.turns[2]
                .text
                .contains("<tool_result name=\"Bash\">ok</tool_result>"),
            "{}",
            r.turns[2].text
        );
        let full = render_full(&r);
        assert!(full.contains("sys"), "system 段缺失");
        assert!(full.contains("<tool_call>{\"name\":\"TOOL_NAME\""), "工具契约缺失");
        assert!(full.contains("<conversation_history>"), "历史折叠缺失");
        assert!(full.contains("那个数字是?"), "本轮问题缺失");
        assert!(full.contains("<system-reminder>"), "尾部提醒缺失(末轮 user + 带 tools)");
    }

    #[test]
    fn 渲染_增量只发尾部_user_轮() {
        let mk = |extra: Vec<serde_json::Value>| {
            let mut msgs = vec![
                json!({"role": "user", "content": "第一轮"}),
                json!({"role": "assistant", "content": "第一轮答"}),
            ];
            msgs.extend(extra);
            json!({"messages": msgs})
        };
        let r1 = render(&mk(vec![])).unwrap();
        // 第二轮:历史 = 旧两轮 + 新 user
        let r2 = render(&mk(vec![json!({"role": "user", "content": "第二轮"})])).unwrap();
        assert!(
            r1.fps.len() < r2.fps.len() && r1.fps.iter().zip(&r2.fps).all(|(a, b)| a == b),
            "指纹链必须是严格前缀"
        );
        let inc = render_incremental(&r2, r1.fps.len()).unwrap();
        assert_eq!(inc, "第二轮", "增量只发新 user 轮(assistant 轮丢弃): {inc}");
        // 工具回路中间轮:尾部只有 tool_result 的 user 轮
        let r3 = render(&mk(vec![
            json!({"role": "user", "content": "第二轮"}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t9", "name": "Bash", "input": {"cmd": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t9", "content": "out"}
            ]}),
        ]))
        .unwrap();
        // 工具回路中间轮:第二轮成功后登记的是 r2 的指纹链(3 轮),
        // 尾部 = [assistant(tool_use,模型自己写进 transcript 的,丢弃),
        //         user(tool_result)] → 只剩契约形态的工具结果轮。
        let inc = render_incremental(&r3, r2.fps.len()).unwrap();
        assert!(
            inc.contains("<tool_result name=\"Bash\">out</tool_result>"),
            "工具结果轮按契约渲染: {inc}"
        );
        // prefill(末尾 assistant):尾部 [user, assistant] 交错 → 超出增量语义,
        // None(调用方全量重开,fold_history 会保留 prefill)。
        let r4 = render(&mk(vec![
            json!({"role": "user", "content": "第二轮"}),
            json!({"role": "assistant", "content": "预填"}),
        ]))
        .unwrap();
        assert_eq!(render_incremental(&r4, r1.fps.len()), None);
        // 前缀取满:尾部为空 → None
        let r5 = render(&mk(vec![json!({"role": "user", "content": "第二轮"})])).unwrap();
        assert_eq!(render_incremental(&r5, r5.fps.len()), None);
    }

    #[test]
    fn 会话表_增量判定() {
        let t = SandSessions::default();
        let fps_a = vec![1u64, 2];
        let fps_b = vec![1u64, 2, 3];
        // 无记录 → New
        assert!(matches!(t.lookup("a", "c", &fps_b), SandLookup::New));
        t.insert(
            "a",
            "c",
            SandSession {
                session_id: "caio-c".into(),
                fps: fps_a.clone(),
                last_updated_seq: 9,
                touched: Instant::now(),
            },
        );
        // 严格前缀 → Resume
        match t.lookup("a", "c", &fps_b) {
            SandLookup::Resume {
                session_id,
                prefix_len,
                last_updated_seq,
            } => {
                assert_eq!(session_id, "caio-c");
                assert_eq!(prefix_len, 2);
                assert_eq!(last_updated_seq, 9);
            }
            _ => panic!("应为 Resume"),
        }
        // 同长相同(客户端重试)→ Restart
        assert!(matches!(t.lookup("a", "c", &fps_a), SandLookup::Restart));
        // 分叉 → Restart
        assert!(matches!(
            t.lookup("a", "c", &[1, 99, 3]),
            SandLookup::Restart
        ));
        // 换号 → New(键带 account_id)
        assert!(matches!(t.lookup("b", "c", &fps_b), SandLookup::New));
    }

    #[test]
    fn 开关默认开() {
        assert!(sandchat_enabled());
        set_sandchat_enabled(false);
        assert!(!sandchat_enabled());
        set_sandchat_enabled(true); // 复位,别影响别的测试
    }
}
