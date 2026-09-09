//! InferenceService.Stream 直连驱动(`driver=inference`)。
//!
//! ## 这是什么
//!
//! cursor 通道的第三条路径:绕过 AgentService.Run(进程级门控,见
//! `docs-cursor-protocol-re-2026-08-23.md` §四)与 clidrv(常驻 CLI 子进程),
//! 直连 `api2.cursor.sh` 的 `aiserver.v1.InferenceService/Stream` —— Grok Bot 官方
//! 客户端(host-main.cjs 0.18.0 实锤)使用的推理面:
//!
//! - HTTP/1.1 + connect 流式协议(5 字节信封 + protobuf),无进程校验;
//! - 服务端前缀缓存按账号隔离、跨会话命中、流内回真实 `cache_read_tokens`;
//! - 全部用量记 auto/included 池。
//!
//! 协议细节(字段号/帧格式/缓存规律)全部实测于 2026-08-26,见上述文档 §七。
//!
//! ## 模型名:这面发裸名是「跟官方一致」,不是重犯 2026-08-17 的旧错
//!
//! 那次裸名被收走发生在 AgentService 面(官方在那面发 `cursor-grok-4.6-high`);
//! InferenceService 面官方 host 发的就是裸名 + `requested_model.parameters`
//!(后缀由服务端合成,实测 `grok-4.6` → `cursor-grok-4.6-high`)。前提:parameters
//! 也照官方发(`model_params`)。
//!
//! ## 出口与安全(codex 复审 blocker#1)
//!
//! 出口 client 一律由调用方注入(worker 按实例出口配置构建):账号配了代理就建
//! 专用 H1 client(fail-closed),没配就用 worker 的 egress client —— 后者可能
//! ALPN 上 h2,是指纹级差异(官方 host 钉死 H1),但**出口 IP 一致**才是账号
//! 安全的关键属性,h2 妥协记录在案,后续如被风控怀疑再拆 H1 专用 egress。
//!
//! ## 已知缺口(显式记录,非回归)
//!
//! - `tool_choice` 全通道都不支持(含 clidrv/wire),proto 里也没找到对应字段。
//! - `stop_sequences` 上游**接受**(2026-09-03 实测 200 正常收尾),但是否严格
//!   在边界截断未经语义级验证;已流出的 delta 无法回收,无法补截断。
//! - prefill(assistant 结尾)2026-09-03 实测上游 200 接受,门控已放开。
//! - URL 图片不支持(主动出网下载是 SSRF 面,与 cli/wire 同口径拒绝,回退 cli/wire)。
//! - PDF/document:文字型走 `pdf.rs` 文本抽取注入(与 cli/wire 同形态);扫描件/
//!   图片型抽不到文本层时注入「无法读取」说明,不假装支持。
//! - 剥签名重试只覆盖建流阶段的 400;流起来了再报签名错误无法重试
//!  (实测签名校验在请求入口,见 kiro 同形经验)。

use futures::StreamExt;
use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, ChatUsage, SseEvent, StreamItem};
use serde_json::{json, Value as Json};

use base64::Engine as _;

use crate::protobuf::{Reader, Value as PVal, Writer};
use crate::wire;

/// 推理面端点(官方 host 同款,`getConfiguredBackendUrl` 默认 api2.cursor.sh)。
const API_URL: &str = "https://api2.cursor.sh/aiserver.v1.InferenceService/Stream";

/// 官方 host 的 client-type(`SAND_CLIENT_TYPE`)。
const CLIENT_TYPE: &str = "sand";
/// 对齐官方包版本(0.18.0 DMG,sha256 钉死)。
const CLIENT_VERSION: &str = "0.18.0";
/// 官方 prod 命名空间(`x-sand-box-namespace`)。
const BOX_NAMESPACE: &str = "prod";

/// 响应头等待上限(首帧通常秒级;长思考也是秒回 thinking 帧)。
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// 帧间 idle 上限(上游停滞 backstop;worker 层另有停滞监控)。
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 与其它 Cursor 驱动一致的媒体预算。Inference 路径直接把 base64 放进 protobuf，
/// 如果不在构建请求前卡住，单个大附件会同时占用 JSON、protobuf 与 HTTP body 多份内存。
/// `pub(crate)`:sandchat 的文档渲染用同一份预算(grokbot 面没有附件上传通道,
/// 超预算的块渲成占位文本而不是上线)。
pub(crate) const MAX_ONE_IMAGE: usize = 12 * 1024 * 1024;
pub(crate) const MAX_ALL_IMAGES: usize = 24 * 1024 * 1024;

// ── field9 门面(grok 专用实验,默认关)──────────────────────────────────
//
// 2026-09-07 本地 Ultra 号实弹:grok/claude 在本面只要 AgentTool 带 parameters
// 字段(**空 Struct 也算,字段存在即触发**)就被平台侧拒(grok 422 / claude
// 400);但 tools 数组整体留空、工具名投进 accepted_unadvertised_tool_names
// (field 9)、schema 以文本进 system,grok 会正常发结构化 tool_call
// (tool_name / 增量 args / is_complete 完整)。这是官方 dynamicToolProfile
// 延迟工具机制的同一条通道,只是我们直接把真工具名放进 field9(实测服务端
// 不校验名字是不是官方元工具)。
//
// claude 系**不适用**:任何 tools 数组条目都 400,field9 它也不认(模型回复
// 「工具没暴露给我」)—— claude 带工具继续走 wire 面。
//
// 门面开启时 grok 系带工具请求留在本面(烧 Bot 周池),不再绕行 wire(烧
// api 月池)。开关链:env `CURSOR_FIELD9_TOOLS` → yaml 基线 → 热配置
// `cursor_field9_tools`(见 gw-core config)。
static FIELD9_TOOLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// field9 门面当前是否开(进程级,worker 启动/30s 轮询设置时热应用)。
pub fn field9_tools_enabled() -> bool {
    FIELD9_TOOLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// 热设置 field9 门面开关。
pub fn set_field9_tools(v: bool) {
    FIELD9_TOOLS.store(v, std::sync::atomic::Ordering::Relaxed);
}

// ── 文本模拟工具门面(claude 专用实验,默认关)────────────────────────────
//
// claude 系在本面的处境(2026-09-07 本地 Ultra 号 + grokbot 团队号双号实弹):
// tools 数组出现**任何条目**就 400(空 parameters、无 parameters、官方元工具
// 名都试过了);field9 它不认(模型明确回复「工具没暴露给我」);max_mode 两态、
// fable-5-1 同样 400。官方 grokbot 的 computer-use 子代理走 agent.v1 另一条面,
// 本面没有 claude 工具的官方形态可抄。
//
// 剩下的唯一通路:文本模拟。schema 与调用契约以文本进 system,模型把工具调用
// 写成 `<tool_call>{"name":...,"arguments":{...}}</tool_call>` 文本块(Hermes/Qwen 标准形态),我们在折叠层
// 解析转 Anthropic tool_use;历史里 assistant 的 tool_use 渲染回 `<tool_call>`
// 文本、tool_result 渲染成 `<tool_result>` 文本进 user 消息(两轮回环已实弹
// 验证:opus-5 正确读 result 作答)。
//
// 开关链:env `CURSOR_TEXT_TOOLS` → yaml 基线 → 热配置 `cursor_text_tools`。
static TEXT_TOOLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 文本模拟工具门面当前是否开。
pub fn text_tools_enabled() -> bool {
    TEXT_TOOLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// 热设置文本模拟工具门面开关。
pub fn set_text_tools(v: bool) {
    TEXT_TOOLS.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// 工具门面形态。`None` = 官方原生(tools 数组原样进 tools=2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolFacade {
    None,
    /// grok 系:tools 留空,名字进 field9,schema 文本进 system。
    Field9,
    /// claude 系:全文本模拟(见上)。
    TextEmu,
}

/// 本请求的门面判定:模型族 × 开关 × 是否带工具。composer 永远原生。
pub(crate) fn tool_facade(model: &str, body: &Json) -> ToolFacade {
    let has_tools = body
        .get("tools")
        .and_then(Json::as_array)
        .is_some_and(|t| !t.is_empty());
    if !has_tools {
        return ToolFacade::None;
    }
    let m = model.to_ascii_lowercase();
    if m.starts_with("grok-") && field9_tools_enabled() {
        return ToolFacade::Field9;
    }
    if m.starts_with("claude-") && text_tools_enabled() {
        return ToolFacade::TextEmu;
    }
    ToolFacade::None
}

/// field9 门面注入 system 的工具清单文本(每轮全量:本面无状态,重试/换号后省略
/// 即失效;键序靠 serde_json 的 BTreeMap 天然排序保字节稳定)。
fn tools_prompt_block(tools: &[Json]) -> String {
    // 措辞加固(2026-09-07 生产实测):长上下文(180k+)里 grok 会回滑到训练里
    // 的 Cursor 原生工具名(run_terminal_cmd)或把 shell 命令塞进 name 字段
    // ("ls -la"),必须明令禁止这两类越界。
    let mut s = String::from(
        "Available tools (to use one, emit a structured tool call with the exact tool name \
         and a JSON arguments object; never describe the call in prose). \
         The name field accepts ONLY the exact tool names listed below, character for \
         character (they are all lowercase). NEVER invent tool names, NEVER use \
         run_terminal_cmd or similar, and NEVER put a shell command in the name field — \
         commands go in the arguments of the bash tool:\n",
    );
    for t in tools {
        let name = t.get("name").and_then(Json::as_str).unwrap_or("");
        let desc = t.get("description").and_then(Json::as_str).unwrap_or("");
        s.push_str("- ");
        s.push_str(name);
        if !desc.is_empty() {
            s.push_str(": ");
            s.push_str(desc);
        }
        if let Some(schema) = t.get("input_schema") {
            s.push_str("\n  args schema: ");
            s.push_str(&schema.to_string());
        }
        s.push('\n');
    }
    s
}

/// 文本模拟门面的 system 契约(实弹验证过的措辞,别随手改:grok/claude 都按
/// 它输出可解析的 `<tool_call>` 块)。
/// `pub(crate)`:sandchat(grokbot 0.39 面)的工具只有这一条文本契约,原样复用。
pub(crate) fn tools_text_block(tools: &[Json]) -> String {
    let mut s = String::from(
        "You have access to the following tools. To call a tool, output a block in exactly \
         this format:\n\n<tool_call>{\"name\":\"TOOL_NAME\",\"arguments\":{...}}</tool_call>\n\n\
         Rules:\n- arguments must be a valid JSON object matching the tool's schema.\n\
         - STRICT JSON: every key needs an explicit value — write boolean flags as {\"flag\":true}, \
         never a bare {\"flag\"}.\n\
         - Do not wrap the block in markdown fences. Do not add commentary inside the block.\n\
         - After emitting a tool call, stop and wait for the tool result.\n\
         - The tools listed below are the COMPLETE set. There is NO discovery or meta tool: \
         never call GetMcpTools, GetDynamicTools, CallDynamicTool or any other unlisted name — \
         such calls are silently dropped and you will never receive a result.\n\
         - Tool results arrive as <tool_result name=\"TOOL_NAME\">...</tool_result> in a user message. \
         Never emit a <tool_result> block yourself — only the user side provides them.\n\n\
         Tools:\n",
    );
    for t in tools {
        let name = t.get("name").and_then(Json::as_str).unwrap_or("");
        let desc = t.get("description").and_then(Json::as_str).unwrap_or("");
        s.push_str("- ");
        s.push_str(name);
        if !desc.is_empty() {
            s.push_str(": ");
            s.push_str(desc);
        }
        if let Some(schema) = t.get("input_schema") {
            s.push_str("\n  args schema: ");
            s.push_str(&schema.to_string());
        }
        s.push('\n');
    }
    s
}

/// TextEmu 的会话尾部提醒(2026-09-07 消融实验定位):宿主客户端(Claude Code)
/// 的 system 人格会压过追加在 system 末尾的工具契约 —— claude 把工具调用写成
/// CC 对话记录格式的 markdown("**Bash** ```json ...```"),永远形不成
/// <tool_call>;甚至自己编 `_result` 谎称执行成功。同报文消融:换极简 system →
/// 正常结构化调用;CC system + 工具砍到 2 个 → 依旧散文。把精简提醒钉在会话
/// 尾巴(模型最后读到的位置)后,同报文立即恢复结构化回调。
/// `pub(crate)`:sandchat 同契约复用(全量/增量包都钉在尾巴上)。
pub(crate) fn textemu_tail_reminder(tools: &[Json]) -> String {
    let mut s = String::from(
        "<system-reminder>IMPORTANT: Tool calling in this environment is TEXT-BASED. \
         To call a tool, your reply MUST contain exactly one \
         <tool_call>{\"name\":\"<exact tool name>\",\"arguments\":{...}}</tool_call> block \
         and NOTHING after it. arguments must be strict JSON — boolean flags need explicit \
         values (\"-n\":true), never bare keys. NEVER write tool calls as markdown (like \"**Bash** ```json ...```\"), \
         NEVER narrate them in prose, and NEVER fabricate a <tool_result> or _result yourself — \
         tool results are provided by the user in the next turn. \
         There is NO tool-list or discovery tool (no GetMcpTools / GetDynamicTools / CallDynamicTool) — \
         the names below are the complete set, call them directly. Available tools: ",
    );
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Json::as_str))
        .collect();
    s.push_str(&names.join(", "));
    s.push_str(".</system-reminder>");
    s
}

// InferenceMessageRole
const ROLE_USER: u64 = 1;const ROLE_ASSISTANT: u64 = 2;
const ROLE_TOOL: u64 = 3;
const ROLE_SYSTEM: u64 = 4;

// ── 按账号 client(代理时 H1 专用,fail-closed)──────────────────────────────
//
// `pub(crate)`:sandchat(grokbot 0.39 面)走同一纪律 —— 刷新与发包同出口。
pub(crate) fn inference_client(
    account: &Account,
    egress: &reqwest::Client,
) -> Result<reqwest::Client, UpstreamError> {
    let proxy = account
        .extra
        .get("proxy")
        .and_then(Json::as_str)
        .unwrap_or("");
    if proxy.is_empty() {
        // 无账号代理:用 worker 注入的 egress client(出口身份一致是硬要求)。
        return Ok(egress.clone());
    }
    static CLIENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, reqwest::Client>>,
    > = std::sync::OnceLock::new();
    let cache = CLIENTS.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let key = format!("{}|{proxy}", account.account_id);
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = guard.get(&key) {
        return Ok(c.clone());
    }
    let p = reqwest::Proxy::all(proxy).map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("inference: 账号代理无效(fail-closed,拒绝直连): {e}"),
        )
    })?;
    let client = reqwest::Client::builder()
        .http1_only()
        .proxy(p)
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::Other,
                format!("inference: client 构造失败(fail-closed): {e}"),
            )
        })?;
    guard.insert(key, client.clone());
    Ok(client)
}

/// driver=inference 的形态门控:2026-09-03 起不再要求尾轮 user
///(prefill 经本地 Ultra 实测上游 200 接受)。仍挡:空 messages、URL 媒体
///(主动出网下载是 SSRF 面,与 cli/wire 同口径)、非 PDF 文档、非法 base64、
/// 超预算附件。
pub(crate) fn inference_eligible(body: &Json) -> bool {
    let Some(messages) = body.get("messages").and_then(Json::as_array) else {
        return false;
    };
    if messages.is_empty() {
        return false;
    }
    let mut media_bytes = 0usize;
    for m in messages {
        // fail-closed 到消息级(codex 二轮 M4):非对象消息、非 string/数组的
        // content 会在编码器里被静默丢掉,门控不能放行。
        if !m.is_object() {
            return false;
        }
        match m.get("content") {
            // 无 content(纯 role 占位)与字符串 content:编码器都能处理
            None | Some(Json::Null) | Some(Json::String(_)) => continue,
            Some(Json::Array(arr)) => {
                for b in arr {
                    if !b.is_object() || !eligible_media_block(b, &mut media_bytes) {
                        return false;
                    }
                }
            }
            // 数字/对象/布尔 content:编码器会丢,fail-closed
            _ => return false,
        }
    }
    true
}

/// base64 媒体源的公共校验:尺寸预算(累计计入 `media_bytes`)+ 解码可行性。
/// gate 与编码器共用同一套判断,两处不会分叉。
fn check_base64_source(source: Option<&Json>, media_bytes: &mut usize) -> bool {
    if source.and_then(|s| s.get("type")).and_then(Json::as_str) != Some("base64") {
        return false;
    }
    let Some(data) = source.and_then(|s| s.get("data")).and_then(Json::as_str) else {
        return false;
    };
    // 先按 base64 长度估原始大小,别先解出 200MB 再判超限。
    if data.len() / 4 * 3 > MAX_ONE_IMAGE {
        return false;
    }
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return false;
    };
    if raw.len() > MAX_ONE_IMAGE {
        return false;
    }
    let Some(total) = media_bytes.checked_add(raw.len()) else {
        return false;
    };
    if total > MAX_ALL_IMAGES {
        return false;
    }
    *media_bytes = total;
    true
}

/// base64 PDF 文档块的校验:media_type + 尺寸/解码预算。
fn check_document_block(block: &Json, media_bytes: &mut usize) -> bool {
    let source = block.get("source");
    let mime = source
        .and_then(|s| s.get("media_type"))
        .and_then(Json::as_str)
        .unwrap_or("application/pdf");
    mime == "application/pdf" && check_base64_source(source, media_bytes)
}

/// 检查顶层媒体与 tool_result 内嵌媒体。**fail-closed,与编码器严格同集合**
///(codex 复审 2026-09-03 major#2):编码器只处理 text/image/document/tool_use/
/// thinking/redacted_thinking 与一层的 tool_result;门控对此外的一切(嵌套
/// tool_result、未知块类型)一律拒,让请求落回 cli/wire(to_turns 会渲染成文本),
/// 而不是通过门控后在编码器里静默丢失。
fn eligible_media_block(block: &Json, media_bytes: &mut usize) -> bool {
    match block.get("type").and_then(Json::as_str) {
        Some("document") => check_document_block(block, media_bytes),
        Some("image") => check_base64_source(block.get("source"), media_bytes),
        Some("tool_result") => match block.get("content") {
            Some(Json::Array(content)) => content.iter().all(|nested| {
                match nested.get("type").and_then(Json::as_str) {
                    Some("text") => true,
                    Some("image") => check_base64_source(nested.get("source"), media_bytes),
                    Some("document") => check_document_block(nested, media_bytes),
                    // CC 工具搜索的延迟工具引用块:纯引用标记,编码器渲染成占位文本。
                    Some("tool_reference") => true,
                    // 嵌套 tool_result / 未知类型:编码器处理不了,回退
                    _ => false,
                }
            }),
            // 字符串/缺省 content 不涉及媒体;对象/数字等会被编码器丢,fail-closed
            None | Some(Json::Null) | Some(Json::String(_)) => true,
            _ => false,
        },
        // 编码器认识的非媒体块;tool_reference 是 CC 工具搜索的延迟引用,渲染成占位文本
        Some("text" | "tool_use" | "thinking" | "redacted_thinking" | "tool_reference") => true,
        // 未知块类型:fail-closed
        _ => false,
    }
}

/// `pub(crate)`:sandchat 的渲染器复用(同一个拼接口径,别各写各的)。
pub(crate) fn push_text(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}
// ── google.protobuf.Struct / Value 编码 ─────────────────────────────────────
//
// 工具入参(parameters / args)与 tool_result 的 result 都是任意 JSON,官方线格式
// 用 well-known types 承载:Struct{fields=1 repeated {key=1, value=2}},
// Value oneof: null=1 / number=2(double) / string=3 / bool=4 / struct=5 / list=6
// (ListValue{values=1 repeated Value})。

fn value_bytes(v: &Json) -> Vec<u8> {
    let mut w = Writer::new();
    match v {
        Json::Null => w.uint(1, 0),
        Json::Bool(b) => w.uint(4, *b as u64),
        Json::Number(n) => w.double(2, n.as_f64().unwrap_or(0.0)),
        Json::String(s) => w.string(3, s),
        Json::Array(arr) => {
            let mut lv = Writer::new();
            for item in arr {
                lv.bytes(1, &value_bytes(item));
            }
            w.message(6, &lv); // Value.list_value
        }
        Json::Object(map) => {
            w.message(5, &struct_writer(map)); // Value.struct_value
        }
    }
    w.into_bytes()
}

fn struct_writer(map: &serde_json::Map<String, Json>) -> Writer {
    let mut s = Writer::new();
    // 键序说明(2026-09-04):官方客户端经 JS 对象保序,Struct 字段按客户端原始
    // JSON 顺序上线;serde_json 默认 BTreeMap 重排成字母序。曾怀疑这是 grok/claude
    // 带 tools 422/400 的根因,但把字节做到与官方库完全一致(仅 uuid 不同)后上游
    // 照拒 —— 键序**不是**根因(真凶见 tools_skip_inference 注释)。保留 type 首位
    // 只是让字节更接近官方惯用序,无害;完整保序需 serde_json preserve_order
    //(波及全 workspace,kiro 字节对齐面未审计,暂缓)。
    let mut emit = |k: &str, v: &Json| {
        let mut entry = Writer::new();
        entry.string(1, k);
        entry.bytes(2, &value_bytes(v));
        s.message(1, &entry);
    };
    if let Some(v) = map.get("type") {
        emit("type", v);
    }
    for (k, v) in map {
        if k != "type" {
            emit(k, v);
        }
    }
    s
}

/// 带工具声明(tools 非空)的非 composer 请求绕过 inference 直连(2026-09-04 实弹定论):
/// Cursor 后端把 AgentTool 翻译给模型供应商时,**只要 AgentTool 带 parameters
/// (任意 schema 内容、任意键序——与官方客户端库逐字节一致的报文照样拒)**,
/// grok 系 providerStatusCode 422 / claude 系 400;不带 parameters 的空壳工具正常,
/// composer 全系带 tools 正常。判官样:官方 proto 库(grokbot 重构源码内
/// generated/aiserver/v1/inference_pb.ts)序列化的同构请求,在 4 个
/// x-cursor-client-version(0.18.0 / 2026.08.11-e8db854 / 1.7.44 / 2.0.0)、
/// maxMode 两态、builtInModel、acceptedUnadvertisedToolNames 全组合下均 422。
/// 结论:平台侧行为(疑 2026-09-03 xAI 故障期开始的回归或有意收紧),字节层面无解。
/// 这类请求回落 clidrv(AgentService 面,工具链成熟);上游若恢复,把本函数
/// 改热配置或直接删掉即可。
///
/// 2026-09-07 例外:grok 系在 field9 门面开启时**不绕行**(见文件头「field9 门面」
/// 一节)—— tools 数组留空、名字进 field9,平台侧不拒,模型照常结构化回调。
pub(crate) fn tools_skip_inference(model: &str, body: &Json) -> bool {
    let has_tools = body
        .get("tools")
        .and_then(Json::as_array)
        .is_some_and(|t| !t.is_empty());
    has_tools
        && !model.to_ascii_lowercase().starts_with("composer")
        && tool_facade(model, body) == ToolFacade::None
}

// ── 请求构建 ────────────────────────────────────────────────────────────────

/// 模型 → (max_mode, parameters)。对齐官方 cli-config 实测形态
///(grok 系 effort=high/fast=false;composer 系 fast=false;claude 系裸 max_mode)。
/// ⚠️ 若上游再单方面改参数面,优先把这张表改热配置,别再走"改代码重部署"的老路。
fn model_params(model: &str, thinking_enabled: bool) -> (bool, Vec<(&'static str, &'static str)>) {
    let m = model.to_ascii_lowercase();
    if m.starts_with("grok-") {
        // 客户端明确关掉思考 → 不带 effort(与官方「无 schema 即不发」同形)。
        if thinking_enabled {
            (true, vec![("effort", "high"), ("fast", "false")])
        } else {
            (true, vec![("fast", "false")])
        }
    } else if m.starts_with("composer") {
        (false, vec![("fast", "false")])
    } else {
        (true, vec![])
    }
}

/// document 块 → 注入文本。与 cli/wire 同形态(chat.rs:1877):抽到文本层就内联,
/// 抽不到(扫描件/图片型)明确告知模型无法读取 —— 否则它会反复尝试调工具读文件,
/// 而反代答不了内建终端工具。返回 None = base64 解不出(门控已挡,这里是兜底)。
/// `pub(crate)`:sandchat 的文档注入同形复用。
pub(crate) fn document_inject_text(b: &Json, doc_n: &mut usize) -> Option<String> {
    let data = b
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(Json::as_str)?;
    let raw = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let path = format!("/tmp/gw-cursor/doc-{}.pdf", *doc_n);
    *doc_n += 1;
    Some(match crate::pdf::extract_text(&raw) {
        Some(txt) => format!("<document path=\"{path}\">\n{txt}\n</document>\n\n"),
        None => format!(
            "<document path=\"{path}\" note=\"无法抽取文本层(可能是扫描件或图片型 PDF);\
             请直接告知用户无法读取,不要尝试调用工具读文件\"/>\n\n"
        ),
    })
}

/// Anthropic user 块的文本/图片/文档 → ContentParts。
fn user_parts(blocks: &[Json], doc_n: &mut usize, w: &mut Writer) {
    let mut parts = Writer::new();
    for b in blocks {
        let mut part = Writer::new();
        match b.get("type").and_then(Json::as_str) {
            Some("text") => {
                let mut tp = Writer::new();
                tp.string(1, b.get("text").and_then(Json::as_str).unwrap_or(""));
                part.message(1, &tp);
            }
            Some("image") => {
                let mut ip = Writer::new();
                let src = b.get("source");
                ip.string(
                    1,
                    src.and_then(|s| s.get("data"))
                        .and_then(Json::as_str)
                        .unwrap_or(""),
                );
                ip.string(
                    2,
                    src.and_then(|s| s.get("media_type"))
                        .and_then(Json::as_str)
                        .unwrap_or("image/png"),
                );
                part.message(2, &ip);
            }
            Some("document") => {
                // 文档抽文本层后以 text part 内联(与 cli/wire 同形态)
                if let Some(doc_text) = document_inject_text(b, doc_n) {
                    let mut tp = Writer::new();
                    tp.string(1, &doc_text);
                    part.message(1, &tp);
                } else {
                    continue;
                }
            }
            // CC 工具搜索的延迟工具引用(顶层块):一行占位文本,别静默丢。
            Some("tool_reference") => {
                let tn = b.get("tool_name").and_then(Json::as_str).unwrap_or("?");
                let mut tp = Writer::new();
                tp.string(1, &format!("[tool_reference: {tn}]"));
                part.message(1, &tp);
            }
            _ => continue,
        }
        parts.message(1, &part);
    }
    w.message(3, &parts); // ContentParts{parts=1}
}

/// tool_result blocks → ToolResultContent。`names` 是 tool_use_id → 工具名映射
///(Anthropic 的 tool_result 不带名字,上游 ToolResultPart.tool_name 需要)。
///
/// 形态说明:官方 host 的 converters 把 AI-SDK 的 `role:"tool"` 消息映成
/// tool_content;Anthropic 的 tool_result 块住在 user 消息里,我们把它拆成独立的
/// TOOL 角色消息(role=3,InferenceMessageRole.TOOL 的官方枚举值),与官方同形。
/// TextEmu 门面的 tool_result 文本渲染(契约措辞见 `tools_text_block`,
/// 两者必须逐字一致,模型是靠契约文本认这个形态的)。
/// TextEmu 门面的 tool_result → `<tool_result name=…>` 文本(契约形态)。
/// `pub(crate)`:sandchat 历史渲染同形复用。
pub(crate) fn tool_results_as_text(
    blocks: &[Json],
    names: &std::collections::HashMap<String, String>,
    doc_n: &mut usize,
) -> String {
    let mut out = String::new();
    for b in blocks {
        let id = b.get("tool_use_id").and_then(Json::as_str).unwrap_or("");
        let name = names.get(id).map(String::as_str).unwrap_or("");
        let mut texts = String::new();
        match b.get("content") {
            Some(Json::String(s)) => texts.push_str(s),
            Some(Json::Array(arr)) => {
                for c in arr {
                    match c.get("type").and_then(Json::as_str) {
                        Some("text") => push_text(
                            &mut texts,
                            c.get("text").and_then(Json::as_str).unwrap_or(""),
                        ),
                        // 图片结果在文本门面里没法回传(契约没定义),明说比静默丢强。
                        Some("image") => push_text(&mut texts, "[image omitted: text-tool facade]"),
                        Some("document") => {
                            if let Some(doc_text) = document_inject_text(c, doc_n) {
                                push_text(&mut texts, &doc_text);
                            }
                        }
                        // CC 工具搜索的延迟工具引用:让模型看到名字,别静默丢。
                        Some("tool_reference") => {
                            let tn = c.get("tool_name").and_then(Json::as_str).unwrap_or("?");
                            push_text(&mut texts, &format!("[tool_reference: {tn}]"));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let err = b.get("is_error").and_then(Json::as_bool).unwrap_or(false);
        if err {
            out.push_str(&format!(
                "<tool_result name=\"{name}\" error=\"true\">{texts}</tool_result>"
            ));
        } else {
            out.push_str(&format!("<tool_result name=\"{name}\">{texts}</tool_result>"));
        }
    }
    out
}

fn tool_result_content(
    blocks: &[Json],
    names: &std::collections::HashMap<String, String>,
    doc_n: &mut usize,
    w: &mut Writer,
) {
    let mut content = Writer::new();
    for b in blocks {
        let mut part = Writer::new();
        let id = b.get("tool_use_id").and_then(Json::as_str).unwrap_or("");
        part.string(1, id);
        part.string(2, names.get(id).map(String::as_str).unwrap_or(""));
        let mut texts = String::new();
        let mut image_parts: Vec<Writer> = Vec::new();
        match b.get("content") {
            Some(Json::String(s)) => texts.push_str(s),
            Some(Json::Array(arr)) => {
                for c in arr {
                    match c.get("type").and_then(Json::as_str) {
                        Some("text") => push_text(
                            &mut texts,
                            c.get("text").and_then(Json::as_str).unwrap_or(""),
                        ),
                        Some("image") => {
                            let mut cp = Writer::new();
                            let mut ip = Writer::new();
                            let src = c.get("source");
                            ip.string(
                                1,
                                src.and_then(|s| s.get("data"))
                                    .and_then(Json::as_str)
                                    .unwrap_or(""),
                            );
                            ip.string(
                                2,
                                src.and_then(|s| s.get("media_type"))
                                    .and_then(Json::as_str)
                                    .unwrap_or("image/png"),
                            );
                            cp.message(2, &ip); // ContentPart.image
                            image_parts.push(cp);
                        }
                        Some("document") => {
                            // 内嵌文档:抽文本层并进结果文本(与 cli/wire 同形态)
                            if let Some(doc_text) = document_inject_text(c, doc_n) {
                                push_text(&mut texts, &doc_text);
                            }
                        }
                        // CC 工具搜索的延迟工具引用:结构化结果里没有它的字段,
                        // 渲染成一行文本并进结果(让模型看到名字,别静默丢)。
                        Some("tool_reference") => {
                            let tn = c.get("tool_name").and_then(Json::as_str).unwrap_or("?");
                            push_text(&mut texts, &format!("[tool_reference: {tn}]"));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        // 文本结果进 result(Value 字符串,官方同款);空结果给空串,字段必须在。
        part.bytes(3, &value_bytes(&Json::String(texts)));
        if b.get("is_error").and_then(Json::as_bool) == Some(true) {
            part.uint(4, 1);
        }
        for img in image_parts {
            part.message(5, &img); // experimental_content
        }
        content.message(1, &part);
    }
    w.message(6, &content);
}

/// 构建 InferenceStreamRequest 的 protobuf 字节。
///
/// `strip_reasoning`:换号重试时置 true —— 缓存与签名都按账号隔离(实测跨账号
/// 不命中),历史里的签名来自别的号,带着只会多一次 400 往返(kiro 同形教训)。
///
/// 结构(字段号全部来自官方 proto 定义):
/// messages=1 repeated CoreMessage, tools=2 repeated AgentTool, model_config=4,
/// invocation_id=6, requested_model=7, conversation_id=8。
pub fn build_request(
    body: &Json,
    model: &str,
    conversation_id: &str,
    strip_reasoning: bool,
) -> Result<Vec<u8>, UpstreamError> {
    let mut out = Writer::new();
    let mut tool_names: std::collections::HashMap<String, String> = Default::default();
    // 文档附件的编号器:/tmp/gw-cursor/doc-N.pdf 路径在请求内唯一(与 cli/wire 同约定)。
    let mut doc_n = 0usize;
    // 工具门面(见文件头):Field9 = grok 系 tools 留空走 field 9;TextEmu =
    // claude 系全文本模拟。历史里的 assistant tool_use / tool_result 在 Field9 下
    // 保持结构化回传(gpt-6 评审:先不文本化,实测出问题再降级);TextEmu 下必须
    // 渲染成文本块(上游没见过这些工具的结构化声明,结构化历史会让模型困惑)。
    let facade = tool_facade(model, body);

    // system → 首条 SYSTEM 消息(官方 converters.ts:175 同款:role=SYSTEM + text)。
    let sys_text = body.get("system").map(|sys| match sys {
        Json::String(s) => s.clone(),
        Json::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Json::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    });
    let facade_block = match facade {
        ToolFacade::Field9 => body
            .get("tools")
            .and_then(Json::as_array)
            .map(|t| tools_prompt_block(t)),
        ToolFacade::TextEmu => body
            .get("tools")
            .and_then(Json::as_array)
            .map(|t| tools_text_block(t)),
        ToolFacade::None => None,
    };
    let sys_merged = match (sys_text, facade_block) {
        (Some(t), Some(b)) if !t.is_empty() => Some(format!("{t}\n\n{b}")),
        (Some(t), None) if !t.is_empty() => Some(t),
        (None, Some(b)) => Some(b),
        (_, Some(b)) => Some(b),
        _ => None,
    };
    if let Some(text) = sys_merged {
        let mut m = Writer::new();
        m.uint(1, ROLE_SYSTEM);
        m.string(2, &text);
        out.message(1, &m);
    }

    let messages = body
        .get("messages")
        .and_then(Json::as_array)
        .ok_or_else(|| UpstreamError::bad_request("inference: 请求缺 messages"))?;

    // 先扫一遍 assistant 的 tool_use,建 id→name 映射(tool_result 要用)。
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
                        tool_names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    for m in messages {
        let role = m.get("role").and_then(Json::as_str).unwrap_or("user");
        let content = m.get("content");
        match role {
            "assistant" => {
                let mut w = Writer::new();
                w.uint(1, ROLE_ASSISTANT);
                match content {
                    Some(Json::String(s)) => {
                        w.string(2, s);
                    }
                    Some(Json::Array(blocks)) => {
                        let mut texts = String::new();
                        for b in blocks {
                            match b.get("type").and_then(Json::as_str) {
                                Some("text") => push_text(
                                    &mut texts,
                                    b.get("text").and_then(Json::as_str).unwrap_or(""),
                                ),
                                Some("thinking") if !strip_reasoning => {
                                    let mut rp = Writer::new();
                                    // is_redacted=1 false 可省(proto3 默认)
                                    rp.string(
                                        2,
                                        b.get("thinking").and_then(Json::as_str).unwrap_or(""),
                                    );
                                    if let Some(sig) = b.get("signature").and_then(Json::as_str) {
                                        rp.string(3, sig);
                                    }
                                    // model_name=5:Anthropic 侧无此数据,不发。
                                    w.message(7, &rp);
                                }
                                Some("redacted_thinking") if !strip_reasoning => {
                                    let mut rp = Writer::new();
                                    rp.uint(1, 1);
                                    rp.string(
                                        4,
                                        b.get("data").and_then(Json::as_str).unwrap_or(""),
                                    );
                                    w.message(7, &rp);
                                }
                                Some("tool_use") => {
                                    if facade == ToolFacade::TextEmu {
                                        // 文本门面:历史 tool_use 渲染回模型当初写出的
                                        // 文本形态(上游从没见过结构化声明,结构化历史
                                        // 只会让模型困惑)。
                                        let name =
                                            b.get("name").and_then(Json::as_str).unwrap_or("");
                                        let input = b
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_else(|| "{}".into());
                                        push_text(
                                            &mut texts,
                                            &format!(
                                                "<tool_call>{{\"name\":{},\"arguments\":{}}}</tool_call>",
                                                serde_json::to_string(name).unwrap_or_default(),
                                                input
                                            ),
                                        );
                                        continue;
                                    }
                                    let mut tc = Writer::new();
                                    tc.string(1, b.get("id").and_then(Json::as_str).unwrap_or(""));
                                    tc.string(
                                        2,
                                        b.get("name").and_then(Json::as_str).unwrap_or(""),
                                    );
                                    if let Some(input) = b.get("input") {
                                        if input.is_object() {
                                            tc.message(
                                                3,
                                                &struct_writer(input.as_object().unwrap()),
                                            );
                                        } else {
                                            tc.string(4, &input.to_string());
                                        }
                                    }
                                    w.message(4, &tc);
                                }
                                _ => {}
                            }
                        }
                        if !texts.is_empty() {
                            w.string(2, &texts);
                        }
                    }
                    _ => {}
                }
                out.message(1, &w);
            }
            _ => {
                // user(以及残留的其他角色):文本/图片进 USER 消息,tool_result
                // 拆成独立的 TOOL 消息(官方形态:role=TOOL + tool_content)。
                let blocks: Vec<&Json> = match content {
                    Some(Json::Array(arr)) => arr.iter().collect(),
                    _ => Vec::new(),
                };
                let tool_results: Vec<Json> = blocks
                    .iter()
                    .copied()
                    .filter(|b| b.get("type").and_then(Json::as_str) == Some("tool_result"))
                    .cloned()
                    .collect();
                let rest: Vec<Json> = blocks
                    .iter()
                    .copied()
                    .filter(|b| b.get("type").and_then(Json::as_str) != Some("tool_result"))
                    .cloned()
                    .collect();

                match content {
                    Some(Json::String(s)) => {
                        let mut w = Writer::new();
                        w.uint(1, ROLE_USER);
                        w.string(2, s);
                        out.message(1, &w);
                    }
                    _ => {
                        if !rest.is_empty() {
                            let mut w = Writer::new();
                            w.uint(1, ROLE_USER);
                            let only_text = rest
                                .iter()
                                .all(|b| b.get("type").and_then(Json::as_str) == Some("text"));
                            if only_text {
                                let joined = rest
                                    .iter()
                                    .filter_map(|b| b.get("text").and_then(Json::as_str))
                                    .filter(|text| !text.is_empty())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                w.string(2, &joined);
                            } else {
                                user_parts(&rest, &mut doc_n, &mut w);
                            }
                            out.message(1, &w);
                        }
                        if !tool_results.is_empty() {
                            if facade == ToolFacade::TextEmu {
                                // 文本门面:tool_result 渲染成 user 文本(契约见
                                // tools_text_block),与模型看到的协议一致。
                                let mut w = Writer::new();
                                w.uint(1, ROLE_USER);
                                w.string(2, &tool_results_as_text(&tool_results, &tool_names, &mut doc_n));
                                out.message(1, &w);
                            } else {
                                let mut w = Writer::new();
                                w.uint(1, ROLE_TOOL);
                                tool_result_content(&tool_results, &tool_names, &mut doc_n, &mut w);
                                out.message(1, &w);
                            }
                        }
                    }
                }
            }
        }
    }

    // TextEmu 尾部契约提醒(见 textemu_tail_reminder 的消融实验注释):追加一条
    // 独立 USER 消息钉在会话尾巴 —— 连续 USER 消息本面已有先例(上面 TextEmu 的
    // tool_result 就是拆成第二条 USER 发的)。assistant 结尾(prefill)不追加。
    // 位置在消息流末尾,不动历史前缀,缓存命中不受影响。
    if facade == ToolFacade::TextEmu {
        let last_is_user = messages
            .last()
            .and_then(|m| m.get("role"))
            .and_then(Json::as_str)
            == Some("user");
        if last_is_user {
            if let Some(tools) = body.get("tools").and_then(Json::as_array) {
                let mut w = Writer::new();
                w.uint(1, ROLE_USER);
                w.string(2, &textemu_tail_reminder(tools));
                out.message(1, &w);
            }
        }
    }

    // tools → AgentTool{name=1, description=2, parameters=3:Struct}
    // Field9 门面:tools=2 整体留空(parameters 字段存在即被平台侧拒),
    // 工具名投进 accepted_unadvertised_tool_names(field 9),schema 已在 system。
    // TextEmu 门面:连 field 9 都不发(claude 不认,发了只会让它困惑),契约全在 system。
    if facade != ToolFacade::TextEmu {
        if let Some(tools) = body.get("tools").and_then(Json::as_array) {
            for t in tools {
                if facade == ToolFacade::Field9 {
                    out.string(9, t.get("name").and_then(Json::as_str).unwrap_or(""));
                    continue;
                }
                let mut w = Writer::new();
                w.string(1, t.get("name").and_then(Json::as_str).unwrap_or(""));
                w.string(2, t.get("description").and_then(Json::as_str).unwrap_or(""));
                if let Some(schema) = t.get("input_schema").and_then(Json::as_object) {
                    w.message(3, &struct_writer(schema));
                }
                out.message(2, &w);
            }
        }
    }

    // model_config
    {
        let mut mc = Writer::new();
        if let Some(mt) = body.get("max_tokens").and_then(Json::as_i64) {
            mc.uint(1, mt.max(0) as u64);
        }
        // temperature/top_p 是 float32(T:2=FLOAT),不是 double —— 编错线型上游
        // 解析脱同步,报 "parse binary: illegal tag"(2026-08-26 灰度实测)。
        if let Some(t) = body.get("temperature").and_then(Json::as_f64) {
            mc.float(2, t as f32);
        }
        if let Some(t) = body.get("top_p").and_then(Json::as_f64) {
            mc.float(3, t as f32);
        }
        if let Some(ss) = body.get("stop_sequences").and_then(Json::as_array) {
            for s in ss.iter().filter_map(Json::as_str) {
                mc.string(4, s);
            }
        }
        out.message(4, &mc);
    }

    // invocation_id(每次请求新 uuid)
    out.string(6, &uuid::Uuid::new_v4().to_string());

    // requested_model
    {
        let thinking_enabled = !matches!(
            body.get("thinking")
                .and_then(|t| t.get("type"))
                .and_then(Json::as_str),
            Some("disabled")
        );
        let (max_mode, params) = model_params(model, thinking_enabled);
        let mut rm = Writer::new();
        rm.string(1, model);
        rm.uint(2, max_mode as u64);
        for (id, value) in params {
            let mut p = Writer::new();
            p.string(1, id);
            p.string(2, value);
            rm.message(3, &p);
        }
        out.message(7, &rm);
    }

    out.string(8, conversation_id);
    Ok(out.into_bytes())
}

/// 请求历史里有没有带签名的 thinking(决定 400 时是否值得剥了重试)。
fn history_has_signature(body: &Json) -> bool {
    body.get("messages")
        .and_then(Json::as_array)
        .map(|ms| {
            ms.iter().any(|m| {
                m.get("content")
                    .and_then(Json::as_array)
                    .map(|bs| {
                        bs.iter().any(|b| {
                            b.get("type").and_then(Json::as_str) == Some("thinking")
                                && b.get("signature").and_then(Json::as_str).is_some()
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ── 响应流折叠 ──────────────────────────────────────────────────────────────

/// 缓冲中的工具调用(同一 id 的分片按**到达顺序**拼接;is_complete 到齐才整块吐)。
///
/// ⚠️ args 语义(2026-09-07 读官方 agent-host 675.js 实锤):**is_complete 帧的
/// args 字段就是全量参数**(官方直接 `JSON.parse(complete.args)`,中间帧只当
/// UI 流式 delta 转发,从不拼接)。中间帧与完成帧可能重复携带同一份全量 args
/// (grok 实测:两个帧的 args 逐字节相同)—— 无脑拼接会得到 `{...}{...}` 非法
/// JSON(生产第一发 field9 门面请求就这么挂的)。所以完成帧有 args 就用它;
/// 完成帧空(理论上的纯 delta 流)才退拼接缓冲。
struct PendingTool {
    id: String,
    name: String,
    /// 全部分片的拼接(兜底/观测用,见上)。
    args: String,
    /// 最近一帧携带的 args(完成帧优先用它 = 官方语义)。
    args_last: String,
}

/// 兜底宽容解析器:专门收拾严格解析 + 两级修复都救不回的块 —— 字符串值里混着
/// **裸引号**(2026-09-08 生产实弹:write_file/Bash 的参数里塞 python 代码,
/// `"depthTestEnable":` 这种内容引号让严格解析直接崩)。
///
/// 与修复链的根本区别:不在文本层面修补,而是按 `{"name":..,"arguments":{..}}`
/// 的目标形态做前向解析。字符串值的终止引号判定:**仅当**该引号之后(跳过空白)
/// 紧跟 `}`(对象收尾)或 `,"<标识符>":`(下一个键)才算终止;否则视为内容里的
/// 裸引号,原样收进值。键名一律严格(键里出现裸引号 = 不是我们认识的形态,放弃)。
/// 值内容做宽容反转义:合法转义(\\n/\\t/\\"/\\\\/\\uXXXX 等)照译,非法转义
/// (\d 之类)原样保留反斜杠 —— 那正是模型想写进命令里的正则。
///
/// 只在完全走完形态(根对象恰好闭合到 EOF)时返回 Some;任何一步对不上 → None,
/// 调用方继续走诚实降级,绝不伪造一个结构可疑的调用。
fn parse_tool_call_lenient(raw: &str) -> Option<Json> {
    let cs: Vec<char> = raw.chars().collect();
    let mut p = LenientParser { cs: &cs, i: 0 };
    p.ws();
    let v = p.object()?;
    p.ws();
    if p.i != cs.len() {
        return None; // 尾巴上有东西:形态不符
    }
    Some(v)
}

struct LenientParser<'a> {
    cs: &'a [char],
    i: usize,
}

impl LenientParser<'_> {
    fn ws(&mut self) {
        while self.i < self.cs.len() && self.cs[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.cs.get(self.i).copied()
    }

    fn eat(&mut self, c: char) -> Option<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Some(())
        } else {
            None
        }
    }

    /// 严格字符串(键名用):不支持任何宽容;返回反转义后的串。
    fn strict_string(&mut self) -> Option<String> {
        self.eat('"')?;
        let start = self.i;
        while self.i < self.cs.len() {
            match self.cs[self.i] {
                '"' => {
                    let raw: String = self.cs[start..self.i].iter().collect();
                    self.i += 1;
                    return Some(lenient_unescape(&raw));
                }
                '\\' => self.i += 2, // 跳过转义对,内容留给反转义统一处理
                // 键/结构串里的裸控制字符:不容忍(键不该有这种东西)。
                c if (c as u32) < 0x20 => return None,
                _ => self.i += 1,
            }
        }
        None // 未闭合
    }

    /// 宽容字符串值:终止引号判定见 [`parse_tool_call_lenient`]。返回(内容, 是否闭合)。
    fn lenient_string_value(&mut self) -> Option<String> {
        self.eat('"')?;
        let start = self.i;
        let mut out = String::new();
        while self.i < self.cs.len() {
            let c = self.cs[self.i];
            if c == '\\' && self.i + 1 < self.cs.len() {
                // 转义对先两个都收下,合法与否留给反转义阶段判。
                out.push('\\');
                out.push(self.cs[self.i + 1]);
                self.i += 2;
                continue;
            }
            if c == '"' {
                // 终止符还是内容引号?向后看:跳过空白后紧跟 `}` / `,"<键>":` 才终止。
                let mut j = self.i + 1;
                while j < self.cs.len() && self.cs[j].is_whitespace() {
                    j += 1;
                }
                let terminates = match self.cs.get(j) {
                    Some('}') => true,
                    Some(',') => {
                        // 试读 `,"<ident>":` —— 读得出才终止。
                        let mut k = j + 1;
                        while k < self.cs.len() && self.cs[k].is_whitespace() {
                            k += 1;
                        }
                        if self.cs.get(k) == Some(&'"') {
                            k += 1;
                            let ks = k;
                            while k < self.cs.len()
                                && (self.cs[k].is_ascii_alphanumeric()
                                    || matches!(self.cs[k], '_' | '-' | '.'))
                            {
                                k += 1;
                            }
                            let key_ok = k > ks && k - ks <= 64;
                            let mut m = k;
                            while m < self.cs.len() && self.cs[m].is_whitespace() {
                                m += 1;
                            }
                            key_ok
                                && self.cs.get(m) == Some(&'"')
                                && {
                                    m += 1;
                                    while m < self.cs.len() && self.cs[m].is_whitespace() {
                                        m += 1;
                                    }
                                    self.cs.get(m) == Some(&':')
                                }
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if terminates {
                    self.i += 1; // 吃掉终止引号,调用方从 `,`/`}` 继续
                    return Some(lenient_unescape(&out));
                }
                // 内容引号:原样收进值,继续。
                out.push('"');
                self.i += 1;
                continue;
            }
            // 其余一切(含裸换行/制表符)原样收。
            out.push(c);
            self.i += 1;
        }
        None // 一直没等到终止引号:截断块,放弃
    }

    /// 标量(数字/布尔/null):读到 , } ] 为止,严格解析。容器不在这里:
    /// 容器必须递归走宽容 object/array(它们的字符串值里可能有裸引号,
    /// 严格容器读取在生产语料上必炸 —— 2239933 的 arguments 就是)。
    fn scalar(&mut self) -> Option<Json> {
        let start = self.i;
        while self.i < self.cs.len() && !matches!(self.cs[self.i], ',' | '}' | ']') {
            self.i += 1;
        }
        let text: String = self.cs[start..self.i].iter().collect::<String>().trim().into();
        if text.is_empty() {
            return None;
        }
        serde_json::from_str(&text).ok()
    }

    /// 数组:元素同 [`Self::value`] 分派(宽容)。
    fn array(&mut self) -> Option<Json> {
        self.eat('[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(']') {
            self.i += 1;
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.peek()? {
                ',' => {
                    self.i += 1;
                }
                ']' => {
                    self.i += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    /// 值分派:字符串走宽容,容器递归宽容,标量严格。
    fn value(&mut self) -> Option<Json> {
        self.ws();
        match self.peek()? {
            '"' => self.lenient_string_value().map(Json::String),
            '{' => self.object(),
            '[' => self.array(),
            _ => self.scalar(),
        }
    }

    /// 对象:键严格、值宽容。`,`/`}` 收尾后由调用方继续。
    fn object(&mut self) -> Option<Json> {
        self.eat('{')?;
        let mut map = serde_json::Map::new();
        self.ws();
        if self.peek() == Some('}') {
            self.i += 1;
            return Some(Json::Object(map));
        }
        loop {
            self.ws();
            let key = self.strict_string()?;
            self.ws();
            self.eat(':')?;
            let v = self.value()?;
            map.insert(key, v);
            self.ws();
            match self.peek()? {
                ',' => {
                    self.i += 1;
                }
                '}' => {
                    self.i += 1;
                    return Some(Json::Object(map));
                }
                _ => return None,
            }
        }
    }
}

/// 宽容反转义:合法 JSON 转义照译;非法转义(如 `\d`)原样保留两个字符 ——
/// 模型写正则时想要的就是字面反斜杠。
fn lenient_unescape(raw: &str) -> String {
    let cs: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(cs.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && i + 1 < cs.len() {
            let n = cs[i + 1];
            match n {
                'n' => {
                    out.push('\n');
                    i += 2;
                }
                't' => {
                    out.push('\t');
                    i += 2;
                }
                'r' => {
                    out.push('\r');
                    i += 2;
                }
                'b' => {
                    out.push('\u{8}');
                    i += 2;
                }
                'f' => {
                    out.push('\u{c}');
                    i += 2;
                }
                '"' | '\\' | '/' => {
                    out.push(n);
                    i += 2;
                }
                'u' => {
                    let hex: String = cs.iter().skip(i + 2).take(4).collect();
                    let parsed_cp = if hex.len() == 4 && hex.chars().all(|h| h.is_ascii_hexdigit())
                    {
                        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                    } else {
                        None
                    };
                    match parsed_cp {
                        Some(ch) => {
                            out.push(ch);
                            i += 6;
                        }
                        None => {
                            out.push('\\');
                            i += 1;
                        }
                    }
                }
                // 非法转义:反斜杠原样保留,下一字符走正常流程。
                _ => {
                    out.push('\\');
                    i += 1;
                }
            }
        } else {
            out.push(cs[i]);
            i += 1;
        }
    }
    out
}

/// TextEmu 契约 JSON 的字符串内容消毒:模型写长命令(heredoc / 正则)时常见的
/// 两类字符串内畸形,实弹均见于 2026-09-08 Windows CC 用户的生产报文:
/// 1. **裸控制字符**:heredoc 里的真换行/制表符直接写进字符串(严格 JSON 禁止),
///    转义成 \\n/\\t/\\r/\u00XX;
/// 2. **非法转义**:正则的 `\d`/`\s` 等单反斜杠序列(JSON 只认 \" \\ \/ \b \f \n
///    \r \t \uXXXX),把反斜杠加倍成 `\\d`。
/// 字符串外一个字符不动;已合法的转义序列(含 \\u 四位十六进制)原样保留。
fn repair_json_string_escapes(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len() + 8);
    let mut in_str = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !in_str {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // 字符串内
        match c {
            '"' => {
                in_str = false;
                out.push(c);
                i += 1;
            }
            '\\' => {
                let next = chars.get(i + 1).copied();
                let valid = match next {
                    Some('"') | Some('\\') | Some('/') | Some('b') | Some('f') | Some('n')
                    | Some('r') | Some('t') => Some(2),
                    Some('u') => {
                        let hex_ok = (2..=5).all(|k| {
                            chars.get(i + k).is_some_and(|h| h.is_ascii_hexdigit())
                        });
                        if hex_ok {
                            Some(6)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                match valid {
                    Some(n) => {
                        // 合法转义:整段原样过。
                        for k in 0..n {
                            out.push(chars[i + k]);
                        }
                        i += n;
                    }
                    None => {
                        // 非法转义:反斜杠加倍,后随字符保持原样(孤立 \ 也一样补)。
                        out.push_str("\\\\");
                        if let Some(n2) = next {
                            out.push(n2);
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
            }
            '\n' => {
                out.push_str("\\n");
                i += 1;
            }
            '\t' => {
                out.push_str("\\t");
                i += 1;
            }
            '\r' => {
                out.push_str("\\r");
                i += 1;
            }
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// TextEmu 契约 JSON 的窄修复:模型(实弹里是 claude 系)偶尔把布尔旗标写成
/// 裸键 —— `{"-n","glob":"*.rs"}`(合法 JSON 要求 `{"-n":true,...}`)。
/// 扫描级修复:**对象内 key 位置**读完一个字符串后,下一个非空白字符是 `,`/`}`
/// 而不是 `:` → 补 `:true`。只动这一种畸形;其它(截断/缺括号等)原样返回,
/// 解析仍失败则走诚实降级。字符串/转义/嵌套容器状态全程跟踪,不会误伤值位置
/// 的字符串(如 `"name":"grep"` 的 `"grep"`)。
fn repair_json_bare_flags(raw: &str) -> String {
    /// 容器上下文:数组,或对象的三个相位(等 key / 等 value / value 已完)。
    #[derive(Clone, Copy, PartialEq)]
    enum Ctx {
        Arr,
        Key,
        Val,
        AfterVal,
    }
    let mut out = String::with_capacity(raw.len() + 8);
    let mut stack: Vec<Ctx> = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    // 对象里刚读完一个候选 key 字符串,等 ':' 的证据(否则就是裸键)。
    let mut bare_key = false;
    for c in raw.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => {
                    in_str = false;
                    match stack.last() {
                        Some(Ctx::Key) => bare_key = true,
                        Some(Ctx::Val) => {
                            *stack.last_mut().unwrap() = Ctx::AfterVal;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }
        if bare_key {
            match c {
                // key 与 ':' 之间允许空白,先原样留住。
                c if c.is_whitespace() => {
                    out.push(c);
                    continue;
                }
                ':' => {
                    bare_key = false;
                    *stack.last_mut().unwrap() = Ctx::Val;
                    out.push(c);
                    continue;
                }
                ',' => {
                    // 裸键实锤:补 :true,然后按「下一个 key」继续。
                    bare_key = false;
                    out.push_str(":true");
                    *stack.last_mut().unwrap() = Ctx::Key;
                    out.push(c);
                    continue;
                }
                '}' => {
                    bare_key = false;
                    out.push_str(":true");
                    stack.pop();
                    if let Some(Ctx::Val) = stack.last() {
                        *stack.last_mut().unwrap() = Ctx::AfterVal;
                    }
                    out.push(c);
                    continue;
                }
                // 其它字符:不是合法的 key 后继,放弃这一处的修复尝试。
                _ => bare_key = false,
            }
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' => {
                stack.push(Ctx::Key);
                out.push(c);
            }
            '[' => {
                stack.push(Ctx::Arr);
                out.push(c);
            }
            '}' | ']' => {
                stack.pop();
                // 容器作为对象 value 收尾:父级相位推进到 AfterVal。
                if let Some(Ctx::Val) = stack.last() {
                    *stack.last_mut().unwrap() = Ctx::AfterVal;
                }
                out.push(c);
            }
            ',' => {
                // value(标量/字符串/容器)后的逗号 → 等下一个 key。
                if matches!(stack.last(), Some(Ctx::AfterVal) | Some(Ctx::Val)) {
                    *stack.last_mut().unwrap() = Ctx::Key;
                }
                out.push(c);
            }
            ':' => {
                if let Some(Ctx::Key) = stack.last() {
                    *stack.last_mut().unwrap() = Ctx::Val;
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// TextEmu 契约 JSON 的窄修复(第三级):模型写深层嵌套参数时偶尔丢掉**最外层
/// 收尾括号** —— 2026-09-09 生产实弹两条(fable-5-1 的 todowrite、opus-5 的
/// skill_manage):`{"name":"X","arguments":{…[…]}` 数组和 arguments 都合上了,
/// 根对象的 `}` 没写就输出 </tool_call>,严格解析必然失败。
/// 规则:全程跟踪字符串/转义;扫完仍停在字符串**里面**的(疑似截断,补括号等于
/// 虚构参数)一律原样返回不修;否则把未闭合容器按栈序补 `}`/`]`。只补括号,
/// 不虚构任何内容;补完仍不合法就交给后续的宽容解析/诚实降级。
fn repair_json_unclosed(raw: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_str {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if in_str || stack.is_empty() {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len() + stack.len());
    out.push_str(raw);
    for c in stack.iter().rev() {
        out.push(*c);
    }
    out
}

/// TextEmu 门面的流式文本过滤器:在文本 delta 流里识别 `<tool_call>…</tool_call>`
/// 块。标签可能横跨两个 delta,所以滞回 `OPEN.len()-1` 个字符确认不是标签前缀
/// 才往下流放(流式体验只损失这 10 个字符的延迟)。
///
/// 语义对齐实弹验证过的契约(`tools_text_block`):模型写完一个调用块就停,
/// 块内是 `{"name":...,"arguments":{...}}` 单个 JSON object(Hermes 标准键;
/// 解析侧兼容旧契约的 `args`)。
struct TextToolFilter {
    /// 疑似标签前缀的滞回缓冲(非块内状态)。
    holdback: String,
    /// 已进入块内:Call 累积待解析;Drop 累积待丢弃(幻觉 `<tool_result>` 块)。
    in_block: Option<(bool, String)>, // bool: true=Call false=Drop
}

/// 过滤器产出:纯文本往下游走;调用块内容(标签之间的 JSON)去解析。
enum FilterOut {
    Text(String),
    Call(String),
}

impl TextToolFilter {
    const OPEN: &'static str = "<tool_call>";
    const CLOSE: &'static str = "</tool_call>";
    /// 幻觉结果块:开标签可带属性(`<tool_result name="x">`),按前缀+`>` 识别。
    /// 契约禁止模型自写 tool_result,但 opus-5 实弹偶尔会抢答一个假结果 ——
    /// 那是我们的协议标记,模型没有正当理由产出它,留着只会污染用户可见文本。
    const OPEN_RES_PREFIX: &'static str = "<tool_result";
    const CLOSE_RES: &'static str = "</tool_result>";

    fn new() -> Self {
        Self {
            holdback: String::new(),
            in_block: None,
        }
    }

    fn feed(&mut self, text: &str, out: &mut Vec<FilterOut>) {
        let mut cur = text.to_string();
        loop {
            if let Some((is_call, buf)) = self.in_block.as_mut() {
                buf.push_str(&cur);
                let close = if *is_call { Self::CLOSE } else { Self::CLOSE_RES };
                if let Some(pos) = buf.find(close) {
                    let inner = buf[..pos].to_string();
                    let after = buf[pos + close.len()..].to_string();
                    let is_call = *is_call;
                    self.in_block = None;
                    if is_call {
                        out.push(FilterOut::Call(inner));
                    } else {
                        tracing::warn!("inference: TextEmu 丢弃模型幻觉的 tool_result 块");
                    }
                    cur = after;
                    continue;
                }
                return;
            }
            // 非块内:holdback + 新文本里找两个开标签的最早者。
            let mut hay = std::mem::take(&mut self.holdback);
            hay.push_str(&cur);
            let call_at = hay.find(Self::OPEN);
            let res_at = hay.find(Self::OPEN_RES_PREFIX);
            let hit = match (call_at, res_at) {
                (Some(c), Some(r)) => Some((c.min(r), c <= r)),
                (Some(c), None) => Some((c, true)),
                (None, Some(r)) => Some((r, false)),
                (None, None) => None,
            };
            if let Some((pos, is_call)) = hit {
                if !is_call {
                    let after_prefix = pos + Self::OPEN_RES_PREFIX.len();
                    if after_prefix >= hay.len() {
                        // 前缀都可能没攒全,等下一包。
                        self.holdback = hay;
                        return;
                    }
                    let c = hay.as_bytes()[after_prefix];
                    if c != b'>' && !c.is_ascii_whitespace() {
                        // 不是我们的标签(如 <tool_results>):前缀原样放行,
                        // 从下一个字符继续扫,别进 Drop 把后文全攒住。
                        out.push(FilterOut::Text(hay[..after_prefix].to_string()));
                        cur = hay[after_prefix..].to_string();
                        continue;
                    }
                    // 幻觉结果块:开标签要到 `>` 才算完(属性段),没等到就先攒着。
                    let Some(gt) = hay[after_prefix..].find('>') else {
                        self.holdback = hay;
                        return;
                    };
                    if pos > 0 {
                        out.push(FilterOut::Text(hay[..pos].to_string()));
                    }
                    self.in_block = Some((false, String::new()));
                    cur = hay[after_prefix + gt + 1..].to_string();
                    continue;
                }
                if pos > 0 {
                    out.push(FilterOut::Text(hay[..pos].to_string()));
                }
                self.in_block = Some((true, String::new()));
                cur = hay[pos + Self::OPEN.len()..].to_string();
                continue;
            }
            // 没找到开标签:末尾保留最长前缀-1 个字符防半标签,其余放行。
            let hold = Self::OPEN_RES_PREFIX.len() - 1;
            if hay.len() > hold {
                // 不在 UTF-8 边界上直接切会 panic,退回 char 边界。
                let split = hay.floor_char_boundary(hay.len() - hold);
                out.push(FilterOut::Text(hay[..split].to_string()));
                self.holdback = hay[split..].to_string();
            } else {
                self.holdback = hay;
            }
            return;
        }
    }

    /// 流收尾:滞回缓冲按文本放掉;未闭合的调用块/幻觉块都原文吐回
    ///(诚实降级:可能根本不是我们的标签,静默吞掉比原文展示更糟)。
    fn finish(&mut self, out: &mut Vec<FilterOut>) {
        if !self.holdback.is_empty() {
            out.push(FilterOut::Text(std::mem::take(&mut self.holdback)));
        }
        if let Some((is_call, buf)) = self.in_block.take() {
            let open = if is_call {
                Self::OPEN.to_string()
            } else {
                format!("{}>", Self::OPEN_RES_PREFIX)
            };
            out.push(FilterOut::Text(format!("{open}{buf}")));
        }
    }
}

/// 流折叠状态机:InferenceStreamResponse 帧 → Anthropic SSE 事件序列。
///
/// 工具调用**不流式吐**:按 tool_call_id 归并缓冲,`is_complete` 到达时整块吐
///(content_block_start → 一帧全量 input_json_delta → content_block_stop),与
/// Run 路径(chat.rs:3175「input 一次性给全」)同形态,两条路径产出可比。
/// Anthropic 侧同一时刻只有一个打开块;并行调用靠缓冲天然串行化。
///
/// `pub(crate)`:sandchat(grokbot 0.39 面)的 transcript 折叠直接复用这台
/// 状态机 —— 它只经 [`Folder::on_text`] 喂文本增量,TextEmu 过滤器
/// (`text_tools=true`)在内部把 `<tool_call>` 块切成 tool_use,与本面同形态。
pub(crate) struct Folder {
    msg_id: String,
    model: String,
    started: bool,
    /// 当前打开的块:(index, kind),kind ∈ text / thinking。工具块开闭都在
    /// flush 瞬间完成,不会停留在 open 里。
    open: Option<(u64, &'static str)>,
    next_idx: u64,
    /// 是否产出过工具块(stop_reason 口径:有工具调用就是 tool_use,否则 agent
    /// 循环不会去执行工具 —— chat.rs:3207 同款口径)。
    saw_tool_use: bool,
    /// 是否见过文本块:之后迟到的 thinking 违反 Anthropic 顺序约束,丢弃留痕。
    saw_text: bool,
    /// 客户端没关思考(thinking.type != "disabled")才往下发 thinking 块。
    show_thinking: bool,
    pending_tools: std::collections::VecDeque<PendingTool>,
    declared_tools: std::collections::HashSet<String>,
    usage: ChatUsage,
    /// 是否见过权威 usage 帧(usage/extended_usage);没有就在收尾时自估,
    /// 不能让「内容成功但全零入账」(wire 路径有同口径兜底)。
    saw_usage: bool,
    /// 产出文本/思考/工具参数的累计字符(自估 output 的基准)。
    out_chars: u64,
    /// 请求体量(自估 input 的兜底基准)。
    req_bytes: u64,
    saw_content: bool,
    /// response_info.messages 的字节量(协议漂移探测:流式为空而回放非空 = 我们
    /// 漏解了帧,必须报错而不是静默空回复)。
    replay_bytes: usize,
    hit_max_tokens: bool,
    /// 见过终结信号(response_info / OUTPUT_TOKEN_LIMIT / END trailer)。
    finished: bool,
    /// finale(message_delta+message_stop+Usage)已发 —— exactly-once。
    finale_sent: bool,
    /// 已失败(发过 Err):失败终态后绝不再产正常收尾。
    failed: bool,
    pending: Vec<Result<StreamItem, UpstreamError>>,
    /// TextEmu 门面:文本流里识别 `<tool_call>` 块(见 [`TextToolFilter`])。
    text_tools: bool,
    filter: TextToolFilter,
}

impl Folder {
    pub(crate) fn new(
        model: &str,
        declared_tools: std::collections::HashSet<String>,
        show_thinking: bool,
        req_bytes: u64,
        text_tools: bool,
    ) -> Self {
        Self {
            msg_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            open: None,
            next_idx: 0,
            saw_tool_use: false,
            saw_text: false,
            show_thinking,
            pending_tools: Default::default(),
            declared_tools,
            usage: ChatUsage::default(),
            saw_usage: false,
            out_chars: 0,
            req_bytes,
            saw_content: false,
            replay_bytes: 0,
            hit_max_tokens: false,
            finished: false,
            finale_sent: false,
            failed: false,
            pending: Vec::new(),
            text_tools,
            filter: TextToolFilter::new(),
        }
    }

    fn push(&mut self, item: StreamItem) {
        self.pending.push(Ok(item));
    }

    /// 失败终态:发一次 Err,此后任何收尾都不再产正常事件。
    pub(crate) fn fail(&mut self, e: UpstreamError) {
        self.failed = true;
        self.pending.push(Err(e));
    }

    /// 是否已进失败终态(sandchat 的流循环按它提前收线)。
    pub(crate) fn is_failed(&self) -> bool {
        self.failed
    }

    /// 是否产出过内容(sandchat 的停滞兜底据此决定「部分内容收尾」还是「报错」)。
    pub(crate) fn saw_content(&self) -> bool {
        self.saw_content
    }

    /// sandchat 专用:grokbot 0.39 的 transcript **没有 token 用量字段**,
    /// 按文本量估算 —— input = 本轮发送文本字节/4(全量渲染或增量),
    /// output = 产出字符/4。**这是估算口径**(request_logs 里的数会偏离真实
    /// 消耗),真实 Bot 周池消耗由后台 `GetSandUsageStatus` 周期核对。
    /// 在 [`Folder::finish`] 前调用;调了之后 finish 不再走自估兜底。
    /// 注意:TextEmu 过滤器滞回缓冲里的尾巴(≤一个标签前缀长,finish 才放出)
    /// 不在 out_chars 里,output 估算会少算那几字节 —— 估算口径本就粗糙,忽略。
    pub(crate) fn set_estimated_usage(&mut self, input_bytes: u64) {
        self.usage = ChatUsage {
            input_tokens: input_bytes / 4,
            output_tokens: (self.out_chars / 4).max(1),
            ..Default::default()
        };
        self.saw_usage = true;
    }

    fn sse(&mut self, event: &'static str, data: Json) {
        self.push(StreamItem::Sse(SseEvent::new(event, data)));
    }

    fn ensure_started(&mut self) {
        if !self.started {
            // message_start 的 model 必须是**客户端请求的原名**(客户端按它对账);
            // response_info 是终结帧,回填永远赶不上,别想从它那拿。
            self.push(StreamItem::Sse(crate::chat::message_start_pub(
                &self.msg_id,
                &self.model,
            )));
            self.started = true;
        }
    }

    fn close_block(&mut self) {
        if let Some((idx, _)) = self.open.take() {
            self.sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            );
        }
    }

    fn open_block(&mut self, kind: &'static str, block: Json) {
        self.ensure_started();
        if self.open.map(|o| o.1) == Some(kind) {
            return; // 同类块已在开
        }
        self.close_block();
        let idx = self.next_idx;
        self.next_idx += 1;
        self.sse(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,"content_block":block}),
        );
        self.open = Some((idx, kind));
    }

    fn delta(&mut self, d: Json) {
        if let Some((idx, _)) = self.open {
            self.sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,"delta":d}),
            );
        }
    }

    fn on_text_direct(&mut self, text: &str, is_final: bool) {
        if !text.is_empty() {
            self.saw_content = true;
            self.saw_text = true;
            self.out_chars += text.len() as u64;
            self.open_block("text", json!({"type":"text","text":""}));
            self.delta(json!({"type":"text_delta","text":text}));
        }
        if is_final {
            self.close_block();
        }
    }

    /// 喂一段正文文本增量(text_tools 开着时过 TextToolFilter 切 `<tool_call>`)。
    /// `pub(crate)`:sandchat 的 transcript diff 直接喂这里。
    pub(crate) fn on_text(&mut self, text: &str, is_final: bool) {
        if !self.text_tools {
            self.on_text_direct(text, is_final);
            return;
        }
        // TextEmu 门面:文本流过标签过滤器,`<tool_call>` 块转 Anthropic tool_use。
        let mut outs = Vec::new();
        self.filter.feed(text, &mut outs);
        if is_final {
            self.filter.finish(&mut outs);
        }
        for o in outs {
            match o {
                FilterOut::Text(t) => self.on_text_direct(&t, false),
                FilterOut::Call(raw) => self.publish_text_tool_call(raw),
            }
        }
        // 幂等:没开着块就是 no-op;publish_tool 自己开过闭过不影响这里。
        if is_final {
            self.close_block();
        }
    }

    /// TextEmu:模型写出的 `<tool_call>` 块内容(两个标签之间的 JSON)→ tool_use。
    /// 解析失败走三级窄修复:字符串消毒 + 裸布尔旗标 → 未闭合括号补齐 →
    /// 宽容前向解析;仍失败 / 未声明 / args 非 object:原文按文本吐回(诚实
    /// 降级,不伪造调用),负反馈按**真实失败原因**分三类措辞 —— 2026-09-09
    /// 生产实态:模型丢最外层 `}` 导致解析失败,反馈却咬死"工具没声明",
    /// 模型被误导后对着声明清单里明明存在的名字反复重发同一个畸形块。
    fn publish_text_tool_call(&mut self, raw: String) {
        let parsed = serde_json::from_str::<Json>(&raw)
            .ok()
            .or_else(|| {
                // 两级窄修复:字符串内容消毒(真换行/非法转义)+ 对象裸布尔旗标。
                let repaired = repair_json_bare_flags(&repair_json_string_escapes(&raw));
                serde_json::from_str::<Json>(&repaired)
                    .ok()
                    .inspect(|_| {
                        tracing::warn!("inference: TextEmu 契约 JSON 修复生效(转义/裸旗标)")
                    })
            })
            .or_else(|| {
                // 第三级:未闭合容器补齐(模型丢最外层收尾括号,生产实弹)。
                // 字符串里没走完(疑似截断)时 repair_json_unclosed 原样返回,不补。
                let repaired = repair_json_bare_flags(&repair_json_string_escapes(&raw));
                let balanced = repair_json_unclosed(&repaired);
                (balanced != repaired)
                    .then(|| serde_json::from_str::<Json>(&balanced).ok())
                    .flatten()
                    .inspect(|_| {
                        tracing::warn!("inference: TextEmu 契约 JSON 补齐未闭合括号生效")
                    })
            })
            .or_else(|| {
                // 最后兜底:宽容前向解析(字符串值里的裸引号,生产实弹形态)。
                parse_tool_call_lenient(&raw).inspect(|_| {
                    tracing::warn!("inference: TextEmu 契约 JSON 经宽容解析救回(裸引号)")
                })
            });
        /// 降级原因:决定负反馈措辞,模型要据此修自己的下一个块。
        enum Why {
            /// 三级修复后仍不是合法 JSON(截断/畸形)。
            Malformed,
            /// 解析成功但缺 name,或 arguments/args 不是对象。
            BadShape,
            /// 解析成功但工具名不在声明清单。
            Undeclared(String),
        }
        let mut why = Why::Malformed;
        let good = parsed.as_ref().and_then(|v| {
            let Some(name) = v.get("name").and_then(Json::as_str) else {
                why = Why::BadShape;
                return None;
            };
            // Hermes 标准是 `arguments`;旧契约(今天早些时候灰度)用 `args`,兼容收。
            let Some(args) = v
                .get("arguments")
                .or_else(|| v.get("args"))
                .filter(|a| a.is_object())
            else {
                why = Why::BadShape;
                return None;
            };
            match self.resolve_declared_name(name) {
                Some(n) => Some((n, args.to_string())),
                None => {
                    why = Why::Undeclared(name.to_string());
                    None
                }
            }
        });
        match good {
            Some((name, args)) => {
                self.saw_content = true;
                // id 是我们合成的(cursor 风格 call-<uuid>-0);客户端回 tool_result
                // 时按 id→name 映射回名,见 build_request 的 tool_names。
                let id = format!("call-{}-0", uuid::Uuid::new_v4());
                self.publish_tool(id, name, args);
            }
            None => {
                tracing::warn!("inference: TextEmu 调用块解析失败/未声明,降级为文本");
                self.on_text_direct(
                    &format!("{}{}{}", TextToolFilter::OPEN, raw, TextToolFilter::CLOSE),
                    false,
                );
                // 给模型一条**下一轮能在历史里读到**的负反馈:静默降级会让它以为
                // "调用发出去了但没回",然后开始空转"先取工具清单"(2026-09-08 生产
                // 诊断实态——模型对未声明名发调用后自我脑补了整套发现协议)。
                let feedback = match &why {
                    Why::Malformed => "\n[gateway: tool call NOT executed — the JSON inside \
                        <tool_call> was malformed or unbalanced. Re-issue the SAME tool call \
                        with strict, fully-closed JSON (every opened brace must be closed, \
                        including the outermost one).]\n"
                        .to_string(),
                    Why::BadShape => "\n[gateway: tool call NOT executed — the block must \
                        contain a \"name\" and an \"arguments\" JSON object. Re-issue with \
                        that exact shape.]\n"
                        .to_string(),
                    Why::Undeclared(n) => format!(
                        "\n[gateway: tool call NOT executed — \"{n}\" is not a declared tool. \
                         No result will arrive; re-issue with one of the declared tool names.]\n"
                    ),
                };
                self.on_text_direct(&feedback, false);
            }
        }
    }

    fn on_thinking(&mut self, text: &str, signature: Option<&str>, is_final: bool) {
        // 客户端明确关了思考 → 不上 thinking 块(但内容照样计 output)。
        if !self.show_thinking {
            self.out_chars += text.len() as u64;
            self.saw_content |= !text.is_empty();
            return;
        }
        // Anthropic 顺序约束:thinking 只能在正文之前。正文开始后迟到的 thinking
        // 丢弃留痕(wire 路径同约束)。
        if self.saw_text {
            tracing::warn!("inference: 正文后收到迟到 thinking,已丢弃");
            self.out_chars += text.len() as u64;
            return;
        }
        if !text.is_empty() {
            self.saw_content = true;
            self.out_chars += text.len() as u64;
            self.open_block("thinking", json!({"type":"thinking","thinking":""}));
            self.delta(json!({"type":"thinking_delta","thinking":text}));
        }
        if is_final {
            // 签名在关块前以 signature_delta 下发(Anthropic 规范)。
            if let (Some(sig), Some((idx, "thinking"))) = (signature, self.open) {
                self.sse(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":idx,
                           "delta":{"type":"signature_delta","signature":sig}}),
                );
            }
            self.close_block();
        }
    }

    fn on_tool_call_part(&mut self, id: &str, name: &str, args_delta: &str, is_complete: bool) {
        self.out_chars += args_delta.len() as u64;
        if id.is_empty() {
            // 没 id 无法归并分片,当文本吐掉,别让客户端等一个永不闭合的块。
            tracing::warn!(name, "inference: 工具调用帧缺 tool_call_id,降级为文本");
            self.on_text(name, false);
            return;
        }
        if let Some(t) = self.pending_tools.iter_mut().find(|t| t.id == id) {
            t.args.push_str(args_delta);
            if !args_delta.is_empty() {
                t.args_last = args_delta.to_string();
            }
        } else {
            self.pending_tools.push_back(PendingTool {
                id: id.to_string(),
                name: name.to_string(),
                args: args_delta.to_string(),
                args_last: args_delta.to_string(),
            });
        }
        if is_complete {
            self.flush_tool(id, false);
        }
    }

    /// 整块吐出一个工具调用(content_block_start → 全量 input_json_delta → stop)。
    ///
    /// `forced`:finish 时没等到 is_complete 的强制 flush。args 必须是合法 JSON
    /// object 才发布 —— 伪造 `{}` 等于让客户端执行与模型原意不同的默认动作;
    /// 声明清单解析:精确命中 > 大小写折叠唯一命中(实测 grok 会把 `glob`
    /// 写成 `Glob`)。返回声明清单里的**规范名**;都不沾 → None(模型越界)。
    fn resolve_declared_name(&self, name: &str) -> Option<String> {
        if self.declared_tools.contains(name) {
            return Some(name.to_string());
        }
        let folded = name.to_ascii_lowercase();
        let ci: Vec<&String> = self
            .declared_tools
            .iter()
            .filter(|d| d.to_ascii_lowercase() == folded)
            .collect();
        if ci.len() == 1 {
            tracing::warn!(from = %name, to = %ci[0], "inference: 工具名大小写笔误,按声明名纠正");
            return Some(ci[0].clone());
        }
        None
    }

    /// 拼不出合法参数就进失败终态(内容已流出,Err 比假数据诚实)。
    fn flush_tool(&mut self, id: &str, forced: bool) {
        let Some(pos) = self.pending_tools.iter().position(|t| t.id == id) else {
            return;
        };
        let mut tool = self.pending_tools.remove(pos).expect("position 刚查到");
        self.saw_content = true;
        // 未声明的工具:模型越界。**无条件**校验 —— 客户端没声明 tools 时任何
        // 工具调用都是异常,放行的后果是客户端收到不认识的 tool_use 直接卡死。
        // 唯一豁免是大小写笔误(resolve_declared_name 内处理)。
        match self.resolve_declared_name(&tool.name) {
            Some(n) => tool.name = n,
            None => {
                tracing::warn!(tool = %tool.name, "inference: 模型调用了未声明的工具,降级为文本块");
                self.on_text(
                    &format!("[未声明的工具调用 {}({})]", tool.name, tool.args),
                    false,
                );
                return;
            }
        }
        let valid_obj =
            |s: &str| serde_json::from_str::<Json>(s).map(|v| v.is_object()).unwrap_or(false);
        // 官方语义:完成帧的 args 就是全量(优先 args_last);完成帧没带才退拼接缓冲
        //(纯 delta 流)。两者都拼不出合法 object 才进失败终态。
        let args = if !tool.args_last.is_empty() && valid_obj(&tool.args_last) {
            tool.args_last
        } else if tool.args.is_empty() {
            "{}".to_string() // 无参工具的合法形态(真空调用)
        } else if valid_obj(&tool.args) {
            tool.args
        } else {
            self.fail(UpstreamError::new(
                UpstreamErrorKind::ServerError,
                format!(
                    "inference: 工具 {} 的参数{}不是合法 JSON object,拒绝伪造下发",
                    tool.name,
                    if forced {
                        "(未完成强制 flush) "
                    } else {
                        " "
                    }
                ),
            ));
            return;
        };
        self.publish_tool(tool.id, tool.name, args);
    }

    /// 整块发布一个工具调用(content_block_start → 全量 input_json_delta → stop)。
    /// 结构化帧(flush_tool)与文本门面(publish_text_tool_call)共用。
    fn publish_tool(&mut self, id: String, name: String, args: String) {
        self.ensure_started();
        self.close_block();
        let idx = self.next_idx;
        self.next_idx += 1;
        self.sse(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        );
        self.sse(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,
                   "delta":{"type":"input_json_delta","partial_json":args}}),
        );
        self.sse(
            "content_block_stop",
            json!({"type":"content_block_stop","index":idx}),
        );
        self.saw_tool_use = true;
    }

    /// 收尾前 flush 所有没等到 is_complete 的工具(END/EOF 先到的情形)。
    fn flush_all_tools(&mut self) {
        let ids: Vec<String> = self.pending_tools.iter().map(|t| t.id.clone()).collect();
        if !ids.is_empty() {
            tracing::warn!(
                n = ids.len(),
                "inference: 流结束时仍有未完成工具调用,强制 flush"
            );
        }
        for id in ids {
            if self.failed {
                break;
            }
            self.flush_tool(&id, true);
        }
    }

    /// 喂一帧 InferenceStreamResponse。
    fn feed_frame(&mut self, payload: &[u8]) -> Result<(), UpstreamError> {
        let mut reader = Reader::new(payload);
        while let Some((field, value)) = reader.next() {
            let PVal::Len(sub) = value else { continue };
            match field {
                1 => {
                    // text_part{text=1, is_final=2}
                    let (mut text, mut fin) = (String::new(), false);
                    for (f, v) in Reader::new(sub) {
                        match (f, v) {
                            (1, PVal::Len(s)) => text = String::from_utf8_lossy(s).into_owned(),
                            (2, PVal::Varint(n)) => fin = n != 0,
                            _ => {}
                        }
                    }
                    self.on_text(&text, fin);
                }
                9 => {
                    // thinking_part{text=1, signature=2, is_final=3}
                    let (mut text, mut sig, mut fin) = (String::new(), None, false);
                    for (f, v) in Reader::new(sub) {
                        match (f, v) {
                            (1, PVal::Len(s)) => text = String::from_utf8_lossy(s).into_owned(),
                            (2, PVal::Len(s)) => {
                                sig = Some(String::from_utf8_lossy(s).into_owned())
                            }
                            (3, PVal::Varint(n)) => fin = n != 0,
                            _ => {}
                        }
                    }
                    self.on_thinking(&text, sig.as_deref(), fin);
                }
                2 => {
                    // tool_call_part{id=1, name=2, args=3(增量), is_complete=4, tool_index=5}
                    let (mut id, mut name, mut args, mut done) =
                        (String::new(), String::new(), String::new(), false);
                    for (f, v) in Reader::new(sub) {
                        match (f, v) {
                            (1, PVal::Len(s)) => {
                                // 上游 id 是两段换行拼接(call-…\nfc_…):只留第一段
                                //(与 wire 面 client_tool_id 同口径;客户端校验/日志
                                // 都见不得换行)。回传走我们自己渲染的历史,不需要原值。
                                id = crate::chat::client_tool_id(&String::from_utf8_lossy(s));
                            }
                            (2, PVal::Len(s)) => name = String::from_utf8_lossy(s).into_owned(),
                            (3, PVal::Len(s)) => args = String::from_utf8_lossy(s).into_owned(),
                            (4, PVal::Varint(n)) => done = n != 0,
                            _ => {}
                        }
                    }
                    self.on_tool_call_part(&id, &name, &args, done);
                }
                3 => {
                    // usage{prompt=1, completion=2, total=3} —— 兜底口径,
                    // extended_usage 到了会被覆盖。
                    let (mut p, mut c) = (0u64, 0u64);
                    for (f, v) in Reader::new(sub) {
                        if let (n, PVal::Varint(x)) = (f, v) {
                            match n {
                                1 => p = x,
                                2 => c = x,
                                _ => {}
                            }
                        }
                    }
                    if !self.saw_usage {
                        self.usage.input_tokens = p;
                        self.usage.output_tokens = c;
                    }
                    self.saw_usage = true;
                }
                5 => {
                    // extended_usage{input=1, output=2, cache_read=3, cache_write=4}
                    //
                    // ## 计费口径(2026-08-26 运营拍板)
                    //
                    // 上游回的是**真实**缓存计量,直接透传,不做任何模拟/夹取 ——
                    // 这条面能拿到真值,没有模拟的必要。后果显式知情:命中率 99.6%+
                    // 时客户侧 uncached input ≈ 0.4%,input 计费随之趋零;我们的
                    // 成本侧是 auto 池扣额而非按 token,成立。
                    //
                    // `input_tokens` 必须是**总上下文**(含缓存命中):worker 入库
                    // `reported_tokens = input + output`(worker/mod.rs:3789 口径);
                    // SSE 侧的减法(input - cache_read)由 `delta_usage_json_pub` 做。
                    let (mut i, mut o, mut cr, mut cw) = (0u64, 0u64, 0u64, 0u64);
                    for (f, v) in Reader::new(sub) {
                        if let (n, PVal::Varint(x)) = (f, v) {
                            match n {
                                1 => i = x,
                                2 => o = x,
                                3 => cr = x,
                                4 => cw = x,
                                _ => {}
                            }
                        }
                    }
                    self.usage = ChatUsage {
                        input_tokens: i,
                        output_tokens: o,
                        cache_read_tokens: cr,
                        cache_creation_tokens: cw,
                        real_cache_read_tokens: cr,
                        metering_credit: 0.0,
                    };
                    self.saw_usage = true;
                }
                4 => {
                    // response_info{id=1, model=2, created_at=3, messages=4}:
                    // 终结信号(不立刻收尾 —— END trailer 可能还在后面,收尾在
                    // 桥接循环出口统一做,exactly-once 由 finale_sent 保证)。
                    // messages 回放的体量记下来:流式零产出而回放非空 = 我方解析
                    // 漏了帧(压缩/新 oneof case),finish 时报错不静默。
                    for (f, v) in Reader::new(sub) {
                        if let (4, PVal::Len(s)) = (f, v) {
                            self.replay_bytes += s.len();
                        }
                    }
                    self.finished = true;
                }
                8 => {
                    // error{message=1, code=2, is_input_token_limit=3,
                    //       is_output_token_limit=4, error_type=5}
                    let (mut message, mut code, mut etype) = (String::new(), String::new(), 0u64);
                    let (mut in_limit, mut out_limit) = (false, false);
                    for (f, v) in Reader::new(sub) {
                        match (f, v) {
                            (1, PVal::Len(s)) => message = String::from_utf8_lossy(s).into_owned(),
                            (2, PVal::Len(s)) => code = String::from_utf8_lossy(s).into_owned(),
                            (3, PVal::Varint(n)) => in_limit = n != 0,
                            (4, PVal::Varint(n)) => out_limit = n != 0,
                            (5, PVal::Varint(n)) => etype = n,
                            _ => {}
                        }
                    }
                    // 输出撞上限是**正常终止**不是错误:Anthropic 语义 = stop_reason
                    // max_tokens,内容照发。当 Err 抛会把已产出的内容变成失败。
                    if etype == 3 || out_limit {
                        self.hit_max_tokens = true;
                        self.finished = true;
                        return Ok(());
                    }
                    // INPUT_TOKEN_LIMIT:请求太大,客户端问题,不罚号。
                    if etype == 2 || in_limit {
                        return Err(UpstreamError::new(
                            UpstreamErrorKind::BadRequest,
                            format!("inference 输入超上限: {message}"),
                        ));
                    }
                    return Err(map_stream_error(etype, &code, &message));
                }
                _ => {} // invocation_id=7 / provider_metadata=6 / image_descriptions=10:忽略
            }
            if self.failed {
                return Ok(()); // flush_tool 已进失败终态,停在本帧
            }
        }
        // Reader 遇畸形静默停(请求侧是好性质,响应侧意味着「帧坏了 = 丢字段无人
        // 知晓」)。没消费完整个 buffer 必须留痕 —— 协议漂移的第一现场不该是
        // 「回复偶尔丢字」。
        if !reader.is_done() {
            tracing::warn!(
                len = payload.len(),
                "inference: 响应帧未完整解析(协议漂移?)"
            );
        }
        Ok(())
    }

    /// 收尾(message_delta + message_stop + Usage)。exactly-once;失败终态后不调。
    pub(crate) fn finish(&mut self) {
        if self.finale_sent || self.failed {
            return;
        }
        self.finale_sent = true;
        // TextEmu:流结束时过滤器里可能还压着滞回文本/半截调用块,先放出来
        //(要排在 saw_content 检查之前,这部分文本也算内容产出)。
        if self.text_tools {
            let mut outs = Vec::new();
            self.filter.finish(&mut outs);
            for o in outs {
                // filter.finish 只产 Text(未闭合块也按原文降级),不会有 Call。
                if let FilterOut::Text(t) = o {
                    self.on_text_direct(&t, false);
                }
            }
        }
        self.flush_all_tools();
        if self.failed {
            return; // flush 途中进了失败终态
        }
        if !self.saw_content {
            if self.replay_bytes > 0 {
                // 回放有货而流式为空 = 解析器落后于上游。可告警的错误,不是空回复。
                self.pending.push(Err(UpstreamError::new(
                    UpstreamErrorKind::Other,
                    format!(
                        "inference: 流式零产出但 response_info 回放 {} 字节(协议漂移,请升级解析器)",
                        self.replay_bytes
                    ),
                )));
                return;
            }
            self.pending.push(Err(UpstreamError::new(
                UpstreamErrorKind::EmptyResponse,
                "inference: 上游空响应(零内容产出)",
            )));
            return;
        }
        self.ensure_started();
        self.close_block();
        if !self.saw_usage {
            // 没等到权威 usage:自估兜底(内容成功不能全零入账)。
            // input ≈ 请求字节/4(总上下文量级),output = 产出字符估算
            //(混合语种取 2 字符/token 的保守中间值)。
            self.usage = ChatUsage {
                input_tokens: self.req_bytes / 4,
                output_tokens: (self.out_chars / 2).max(1),
                ..Default::default()
            };
            tracing::warn!("inference: 流内未见 usage 帧,按估算入账");
        }
        let stop = if self.hit_max_tokens {
            "max_tokens"
        } else if self.saw_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };
        self.sse(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":stop,"stop_sequence":null},
                   "usage":crate::chat::delta_usage_json_pub(&self.usage)}),
        );
        self.sse("message_stop", json!({"type":"message_stop"}));
        self.push(StreamItem::Usage(self.usage.clone()));
    }

    /// connect END trailer(flag&2)。payload 可能是 gzip(flag&1),先还原。
    /// 有 error → Err;无 → 正常结束。非空但解不出 JSON = 协议漂移,报错。
    fn on_trailer(&mut self, flag: u8, payload: &[u8]) -> Result<(), UpstreamError> {
        self.finished = true;
        let payload = wire::frame_payload(flag, payload).map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::ServerError,
                format!("inference END 帧解压失败: {e}"),
            )
        })?;
        if payload.is_empty() {
            return Ok(());
        }
        let json: Json = match serde_json::from_slice(&payload) {
            Ok(j) => j,
            Err(e) => {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    format!("inference END trailer 不是合法 JSON(协议漂移?): {e}"),
                ));
            }
        };
        if let Some(err) = json.get("error") {
            let code = err.get("code").and_then(Json::as_str).unwrap_or("");
            let message = err
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("上游错误");
            // ERROR_BAD_MODEL_NAME 埋在 details debug 里,message 也能见到 "model"。
            if message.contains("Model Not Found") || message.contains("ERROR_BAD_MODEL_NAME") {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::ModelNotAvailable,
                    format!("inference 模型不可用: {message}"),
                ));
            }
            // 模型供应商级故障(xAI 宕机等)绝不能当 RateLimited 冷却账号 ——
            // 2026-09-03 生产事故:grok 上游 422/RESOURCE_EXHAUSTED 被误判成限流,
            // 账号被批量冷却,composer 健康流量被「选号失败」株连,单模型故障
            // 放大成全通道故障。它的真实语义 = 模型级 Overloaded(不罚号、不换号、
            // 同号退避重试)。判定:debug.error = ERROR_PROVIDER_ERROR,或
            // ERROR_RESOURCE_EXHAUSTED 且详情指向 provider(标题/providerStatusCode)。
            let debug = err
                .get("details")
                .and_then(Json::as_array)
                .and_then(|arr| arr.first())
                .and_then(|d| d.get("debug"));
            let debug_error = debug
                .and_then(|d| d.get("error"))
                .and_then(Json::as_str)
                .unwrap_or("");
            let provider_marked = debug
                .and_then(|d| d.get("details"))
                .map(|d| {
                    d.get("title").and_then(Json::as_str).unwrap_or("").contains("provider")
                        || d.get("detail").and_then(Json::as_str).unwrap_or("").contains("provider")
                        || d.pointer("/additionalInfo/providerStatusCode").is_some()
                })
                .unwrap_or(false);
            if debug_error == "ERROR_PROVIDER_ERROR"
                || (debug_error == "ERROR_RESOURCE_EXHAUSTED" && provider_marked)
            {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::Overloaded,
                    format!("inference 模型供应商不可用[{debug_error}]: {message}"),
                ));
            }
            return Err(map_connect_error(code, message));
        }
        Ok(())
    }

    pub(crate) fn take_pending(&mut self) -> Vec<Result<StreamItem, UpstreamError>> {
        std::mem::take(&mut self.pending)
    }
}

/// 流内 error 帧的 error_type → kind。
/// 枚举:UNKNOWN=1 INPUT_TOKEN_LIMIT=2 OUTPUT_TOKEN_LIMIT=3 RATE_LIMIT=4
/// AUTHENTICATION=5 PERMISSION=6 OVERLOADED=7(2/3 已在 feed_frame 拦截)。
fn map_stream_error(error_type: u64, code: &str, message: &str) -> UpstreamError {
    let kind = match error_type {
        4 => UpstreamErrorKind::RateLimited,
        5 => UpstreamErrorKind::TokenInvalid,
        // PERMISSION 没依据判 QuotaExhausted —— 后者在调度器里是**永久禁号**
        // (scheduler.rs:3264),「档位不含此模型」绝不允许走到那。权限类一律
        // ModelNotAvailable(不罚号+换号),池爆的真实错误形态待实测后单列。
        6 => UpstreamErrorKind::ModelNotAvailable,
        7 => UpstreamErrorKind::Overloaded,
        _ => {
            if code.contains("BAD_MODEL") {
                UpstreamErrorKind::ModelNotAvailable
            } else if message.contains("high load") || message.contains("overloaded") {
                UpstreamErrorKind::Overloaded
            } else {
                UpstreamErrorKind::ServerError
            }
        }
    };
    UpstreamError::new(kind, format!("inference 上游错误[{code}]: {message}"))
}

/// connect 层错误(END trailer JSON / 非 200 结构化 body)的 code → kind。
fn map_connect_error(code: &str, message: &str) -> UpstreamError {
    map_connect_error_for("inference", code, message)
}

/// [`map_connect_error`] 的可冠名版:`face` 只是日志/消息前缀(sandchat 复用)。
pub(crate) fn map_connect_error_for(face: &str, code: &str, message: &str) -> UpstreamError {
    let kind = match code {
        "unauthenticated" => UpstreamErrorKind::TokenInvalid,
        "resource_exhausted" => UpstreamErrorKind::RateLimited,
        // 同上:permission_denied 不罚号。
        "permission_denied" => UpstreamErrorKind::ModelNotAvailable,
        "not_found" => UpstreamErrorKind::ModelNotAvailable,
        "invalid_argument" | "failed_precondition" | "out_of_range" => {
            UpstreamErrorKind::BadRequest
        }
        "unavailable" => UpstreamErrorKind::Overloaded,
        _ => UpstreamErrorKind::ServerError,
    };
    UpstreamError::new(kind, format!("{face} 上游错误[{code}]: {message}"))
}

/// 非 200 的 HTTP 错误分类(inference 专用,不套用 Run 路径的分类器 ——
/// 那套把结构化 permission_denied 映成 TokenInvalid,对本面太重)。
///
/// 保留的核心教训(chat.rs:2162):**401 才是号的问题**;403 无结构化错误体时
/// 是出口 IP 被拦(坏的是 IP 不是号),判 Other 不动账号健康。
fn classify_http_error(status: u16, body: &str) -> UpstreamError {
    classify_http_error_for("inference", status, body)
}

/// [`classify_http_error`] 的可冠名版:`face` 只是日志/消息前缀(sandchat 复用,
/// 同一套分类语义,含 401=TokenInvalid → worker 同号刷新重试的约定)。
pub(crate) fn classify_http_error_for(face: &str, status: u16, body: &str) -> UpstreamError {
    if let Ok(j) = serde_json::from_str::<Json>(body) {
        if let Some(code) = j.get("code").and_then(Json::as_str) {
            let message = j
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("上游错误");
            return map_connect_error_for(face, code, message).with_status(status);
        }
    }
    let kind = match status {
        401 => UpstreamErrorKind::TokenInvalid,
        403 => {
            tracing::warn!(
                body_head = %body.chars().take(120).collect::<String>(),
                "{face} 403 且无结构化错误体 —— 疑似出口 IP 被拦(不是号的问题),不动账号健康"
            );
            UpstreamErrorKind::Other
        }
        429 => UpstreamErrorKind::RateLimited,
        400 | 464 => UpstreamErrorKind::BadRequest,
        404 => UpstreamErrorKind::ModelNotAvailable,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    UpstreamError::new(
        kind,
        format!(
            "{face} HTTP {status}: {}",
            body.chars().take(300).collect::<String>()
        ),
    )
    .with_status(status)
}

// ── 入口 ────────────────────────────────────────────────────────────────────

/// driver=inference 的 chat 入口:构建 protobuf 请求 → connect 流式 → ChatStream。
///
/// `egress`:worker 按实例出口配置构建的 client(无账号代理时用它,出口身份一致)。
///
/// 换号重试由调度层做;本函数内部的唯一一次自发重试:BadRequest 且历史带签名
/// → 剥 reasoning_parts 重发(kiro THINKING_SIGNATURE_INVALID 同形预案;缓存与
/// 签名都按账号隔离,换号后历史签名必然是别人的,带着只会多一次 400 往返)。
pub(crate) async fn chat_stream(
    egress: &reqwest::Client,
    account: &Account,
    token: &str,
    req: ChatRequest,
    ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    let client = inference_client(account, egress)?;

    // 模型名归一(复用别名表;未知模型不上游 —— 别拿裸名去撞 not_found,
    // 那会逐号污染 model_unavailable)。SSE 侧仍回显客户端请求的原名。
    let Some(upstream_model) = crate::models::resolve_cursor_model(&req.model) else {
        return Err(UpstreamError::bad_request_visible(format!(
            "inference: 未知模型名 {:?}",
            req.model
        )));
    };

    // conversation_id:必填字段。沿用 wire 路径同源派生(会话稳定 —— 随机 id 会
    // 造出「每轮一个新会话」的异常分布,本身是可区分特征)。
    let material = if !ctx.session_id.is_empty() {
        ctx.session_id.clone()
    } else if !ctx.cache_key.is_empty() {
        ctx.cache_key.clone()
    } else {
        crate::chat::affinity_key_from_body(&req.body)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    };
    let conversation_id = crate::chat::conversation_uuid(&material);

    let body_bytes =
        build_request_blocking(req.body.clone(), upstream_model.clone(), conversation_id.clone(), false).await?;
    match chat_once(&client, account, token, &req, body_bytes).await {
        Ok(stream) => Ok(stream),
        Err(e) if e.kind == UpstreamErrorKind::BadRequest && history_has_signature(&req.body) => {
            tracing::warn!(
                account = %account.account_id,
                "inference: BadRequest 且历史带签名,剥 reasoning_parts 重试一次: {e}"
            );
            let stripped =
                build_request_blocking(req.body.clone(), upstream_model, conversation_id, true).await?;
            chat_once(&client, account, token, &req, stripped).await
        }
        Err(e) => Err(e),
    }
}

/// `build_request` 里可能含 PDF 文本抽取(同步 CPU 活,单文档最多解 64MB),
/// 挪出 Tokio worker 线程,别让一个大 PDF 占住事件循环(codex 复审 2026-09-03 major#3)。
///
/// 带 document 的请求先拿进程级并发槽:单抽峰值 ~80MB(64MB 总量预算 + 单流
/// 16MB 过头),4 槽把并发 PDF 的解压内存压到 ~320MB 上限(codex 二轮 M5)。
async fn build_request_blocking(
    body: Json,
    model: String,
    conversation_id: String,
    strip_reasoning: bool,
) -> Result<Vec<u8>, UpstreamError> {
    // 纯形状检查(不解 base64),只在有文档时才排队拿槽
    let permit = if has_document_block(&body) {
        Some(
            PDF_EXTRACT_SLOTS
                .acquire()
                .await
                .map_err(|_| UpstreamError::new(UpstreamErrorKind::Other, "PDF 抽取槽已关闭"))?,
        )
    } else {
        None
    };
    let result = tokio::task::spawn_blocking(move || {
        build_request(&body, &model, &conversation_id, strip_reasoning)
    })
    .await
    .map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("inference: 请求构建任务异常退出: {e}"),
        )
    })?;
    drop(permit);
    result
}

/// PDF 文本抽取的进程级并发槽(codex 二轮 M5)。
/// `pub(crate)`:sandchat 的渲染走同一道闸(同一进程共享同一份内存预算)。
pub(crate) static PDF_EXTRACT_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(4);

/// 请求里有没有 document 块(顶层或 tool_result 内嵌)。只在拿抽取并发槽前用。
/// `pub(crate)`:sandchat 同用。
pub(crate) fn has_document_block(body: &Json) -> bool {
    let Some(ms) = body.get("messages").and_then(Json::as_array) else {
        return false;
    };
    ms.iter().any(|m| {
        let Some(arr) = m.get("content").and_then(Json::as_array) else {
            return false;
        };
        arr.iter().any(|b| {
            b.get("type").and_then(Json::as_str) == Some("document")
                || b
                    .get("content")
                    .and_then(Json::as_array)
                    .map(|c| {
                        c.iter().any(|n| {
                            n.get("type").and_then(Json::as_str) == Some("document")
                        })
                    })
                    .unwrap_or(false)
        })
    })
}

async fn chat_once(
    client: &reqwest::Client,
    account: &Account,
    token: &str,
    req: &ChatRequest,
    body_bytes: Vec<u8>,
) -> Result<ChatStream, UpstreamError> {
    let machine_id = crate::CursorProvider::machine_id_of(account, token);
    let mac_machine_id = crate::CursorProvider::mac_machine_id_of(account, token);
    let req_len = body_bytes.len() as u64;

    let resp = tokio::time::timeout(
        HEADER_TIMEOUT,
        client
            .post(API_URL)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip")
            .header("authorization", format!("Bearer {token}"))
            .header(
                "x-cursor-checksum",
                wire::checksum(&machine_id, Some(&mac_machine_id)),
            )
            .header("x-cursor-client-type", CLIENT_TYPE)
            .header("x-cursor-client-version", CLIENT_VERSION)
            .header("x-sand-box-namespace", BOX_NAMESPACE)
            // 默认 true = 不训练(最保守;官方由隐私模式驱动)。留账号级开关
            // extra.ghost_mode,一旦怀疑它参与风控可整池切换而不必改代码。
            .header(
                "x-ghost-mode",
                account
                    .extra
                    .get("ghost_mode")
                    .and_then(Json::as_str)
                    .unwrap_or("true"),
            )
            .header("x-request-id", uuid::Uuid::new_v4().to_string())
            .header("te", "trailers")
            .body(wire::frame(&body_bytes))
            .send(),
    )
    .await
    .map_err(|_| {
        UpstreamError::network(format!(
            "inference 等响应头超时({}s)",
            HEADER_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| UpstreamError::network(format!("inference 请求发送失败: {e}")))?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.bytes().await.unwrap_or_default();
        return Err(classify_http_error(status, &String::from_utf8_lossy(&body)));
    }

    let declared_tools: std::collections::HashSet<String> = req
        .body
        .get("tools")
        .and_then(Json::as_array)
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t.get("name").and_then(Json::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let show_thinking = !matches!(
        req.body
            .get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(Json::as_str),
        Some("disabled")
    );

    // 门面判定与 build_request 用同一个模型口径:客户端名先归一成上游名,
    // 别名(如 grok4.6)才不会漏判族前缀。
    let facade = crate::models::resolve_cursor_model(&req.model)
        .map(|m| tool_facade(&m, &req.body))
        .unwrap_or(ToolFacade::None);
    let mut folder = Folder::new(
        &req.model,
        declared_tools,
        show_thinking,
        req_len,
        facade == ToolFacade::TextEmu,
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);
    tokio::spawn(async move {
        let mut dec = wire::FrameDecoder::new();
        let mut stream = resp.bytes_stream();
        let mut saw_upstream_frame = false;
        'outer: loop {
            // 客户端断开(rx 关闭)立即停止:否则 lease 已释放、上游请求还活着,
            // 实际并发会突破账号上限。
            let next = tokio::select! {
                _ = tx.closed() => break 'outer,
                r = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => r,
            };
            let chunk = match next {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => {
                    // 字节层错误走 Network,失败终态,绝不再产正常收尾。
                    folder.fail(UpstreamError::network(format!(
                        "inference 读取上游流失败: {e}"
                    )));
                    break 'outer;
                }
                Ok(None) => break 'outer, // EOF
                Err(_) => {
                    folder.fail(UpstreamError::network(format!(
                        "inference 上游停滞超过 {}s",
                        IDLE_TIMEOUT.as_secs()
                    )));
                    break 'outer;
                }
            };
            dec.feed(&chunk);
            loop {
                match dec.try_next_frame() {
                    Ok(Some((flag, payload))) => {
                        if flag & 0x02 != 0 {
                            // END trailer(JSON,可能 gzip):正常或错误的终态
                            match folder.on_trailer(flag, &payload) {
                                Ok(()) => {}
                                Err(e) => folder.fail(e),
                            }
                            break 'outer; // trailer 是最后一帧
                        }
                        saw_upstream_frame = true;
                        let data = match wire::frame_payload(flag, &payload) {
                            Ok(d) => d,
                            Err(e) => {
                                folder.fail(UpstreamError::new(
                                    UpstreamErrorKind::ServerError,
                                    format!("inference 帧解压失败: {e}"),
                                ));
                                break 'outer;
                            }
                        };
                        if let Err(e) = folder.feed_frame(&data) {
                            folder.fail(e);
                            break 'outer;
                        }
                        if folder.failed {
                            break 'outer;
                        }
                        for item in folder.take_pending() {
                            if tx.send(item).await.is_err() {
                                return; // 客户端断开
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        folder.fail(UpstreamError::new(
                            UpstreamErrorKind::ServerError,
                            format!("inference 帧解码失败: {e}"),
                        ));
                        break 'outer;
                    }
                }
            }
        }
        // 统一出口:失败终态只发 Err;正常路径 finish() exactly-once。
        if folder.failed {
            for item in folder.take_pending() {
                let _ = tx.send(item).await;
            }
            return;
        }
        if !folder.finished {
            // EOF 无 END/response_info = 上游静默中止。
            //
            // ⚠️ 第一版**只打点不发 UpstreamCut**(2026-08-26 claude 复审 P1#9):
            // UpstreamCut 的软冷却参数是 kiro 实测标定的(KIRO_DRAIN_*),cursor
            // 侧没有等价证据;照搬会让偶发掐流把健康号拉黑 25 分钟。先观察真实
            // 频率,确认与封号相关再接,届时阈值按 family 拆开。
            // decoder 里残留半帧 = 截断实锤,单独 warn。
            if dec.pending_bytes() > 0 {
                tracing::warn!(
                    leftover = dec.pending_bytes(),
                    "inference: EOF 时残留半帧(流被截断)"
                );
            }
            if saw_upstream_frame {
                tracing::warn!(
                    model = %folder.model,
                    "inference: EOF 无 END 帧(上游静默中止),按现有内容收尾"
                );
            }
        }
        folder.finish();
        for item in folder.take_pending() {
            let _ = tx.send(item).await;
        }
    });

    Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── 测试用解码小工具 ──
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
    fn value_json各型编码() {
        let b = value_bytes(&json!("hi"));
        assert_eq!(str_of(&b, 3).as_deref(), Some("hi"));
        let b = value_bytes(&json!(42.5));
        match fields(&b).into_iter().next() {
            Some((2, PVal::Fixed64(x))) => assert_eq!(f64::from_le_bytes(x), 42.5),
            other => panic!("number 编码错: {other:?}"),
        }
        let b = value_bytes(&json!(true));
        assert_eq!(var_of(&fields(&b), 4), Some(1));
        let b = value_bytes(&Json::Null);
        assert_eq!(var_of(&fields(&b), 1), Some(0));
        let b = value_bytes(&json!(["a", "b"]));
        let lv = len_of(&fields(&b), 6).expect("list_value");
        let mut vals = Vec::new();
        for (f, v) in fields(lv) {
            assert_eq!(f, 1);
            if let PVal::Len(inner) = v {
                vals.push(str_of(inner, 3).unwrap());
            }
        }
        assert_eq!(vals, vec!["a", "b"]);
        let b = value_bytes(&json!({"k": 1.0, "s": "x"}));
        let st = len_of(&fields(&b), 5).expect("struct_value");
        let mut got = std::collections::HashMap::new();
        for (f, v) in fields(st) {
            assert_eq!(f, 1);
            if let PVal::Len(entry) = v {
                let efs = fields(entry);
                let k = len_of(&efs, 1)
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .unwrap();
                let vfs = fields(len_of(&efs, 2).unwrap());
                if let Some(sv) = len_of(&vfs, 3) {
                    got.insert(k, String::from_utf8_lossy(sv).into_owned());
                } else if let Some((2, PVal::Fixed64(x))) = vfs.into_iter().next() {
                    got.insert(k, f64::from_le_bytes(x).to_string());
                }
            }
        }
        assert_eq!(got.get("s").map(String::as_str), Some("x"));
        assert_eq!(got.get("k").map(String::as_str), Some("1"));
    }

    #[test]
    fn build_request_全块型() {
        let body = json!({
            "system": [{"text": "sys prompt"}],
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "SIG"},
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ],
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}}]
        });
        let bytes = build_request(&body, "grok-4.6", "conv-1", false).unwrap();
        let fs = fields(&bytes);

        let msgs: Vec<&[u8]> = fs
            .iter()
            .filter_map(|(f, v)| match (f, v) {
                (1, PVal::Len(s)) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 4, "system+user+assistant+tool 四条");

        assert_eq!(var_of(&fields(msgs[0]), 1), Some(ROLE_SYSTEM));
        assert_eq!(str_of(msgs[0], 2).as_deref(), Some("sys prompt"));
        assert_eq!(var_of(&fields(msgs[1]), 1), Some(ROLE_USER));
        assert_eq!(str_of(msgs[1], 2).as_deref(), Some("hello"));

        let afs = fields(msgs[2]);
        assert_eq!(var_of(&afs, 1), Some(ROLE_ASSISTANT));
        assert_eq!(str_of(msgs[2], 2).as_deref(), Some("answer"));
        let rp = len_of(&afs, 7).expect("reasoning_part");
        assert_eq!(str_of(rp, 2).as_deref(), Some("hmm"));
        assert_eq!(str_of(rp, 3).as_deref(), Some("SIG"), "签名必须原样透传");
        let tc = len_of(&afs, 4).expect("tool_call");
        assert_eq!(str_of(tc, 1).as_deref(), Some("t1"));
        assert_eq!(str_of(tc, 2).as_deref(), Some("Bash"));
        let args = len_of(&fields(tc), 3).expect("args struct");
        let entry = len_of(&fields(args), 1).unwrap();
        let efs = fields(entry);
        assert_eq!(
            len_of(&efs, 1)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .as_deref(),
            Some("cmd")
        );

        let tfs = fields(msgs[3]);
        assert_eq!(var_of(&tfs, 1), Some(ROLE_TOOL));
        let trc = len_of(&tfs, 6).expect("tool_content");
        let part = len_of(&fields(trc), 1).expect("tool_result_part");
        assert_eq!(str_of(part, 1).as_deref(), Some("t1"));
        assert_eq!(
            str_of(part, 2).as_deref(),
            Some("Bash"),
            "tool_name 应从 id 映射回填"
        );
        let result_val = len_of(&fields(part), 3).expect("result value");
        assert_eq!(str_of(result_val, 3).as_deref(), Some("ok"));

        let tool = fs
            .iter()
            .find_map(|(f, v)| match (f, v) {
                (2, PVal::Len(s)) => Some(*s),
                _ => None,
            })
            .expect("tools");
        assert_eq!(str_of(tool, 1).as_deref(), Some("Bash"));
        let mc = len_of(&fs, 4).expect("model_config");
        assert_eq!(var_of(&fields(mc), 1), Some(1024));
        assert!(str_of(&bytes, 6).is_some());
        assert_eq!(str_of(&bytes, 8).as_deref(), Some("conv-1"));
        let rm = len_of(&fs, 7).expect("requested_model");
        assert_eq!(str_of(rm, 1).as_deref(), Some("grok-4.6"));
        assert_eq!(var_of(&fields(rm), 2), Some(1), "grok max_mode=true");
        let p1 = len_of(&fields(rm), 3).expect("grok 参数");
        assert_eq!(str_of(p1, 1).as_deref(), Some("effort"));
        assert_eq!(str_of(p1, 2).as_deref(), Some("high"));
    }

    #[test]
    fn 多个文本块用换行分隔() {
        let body = json!({"messages":[
            {"role":"user","content":[
                {"type":"text","text":"user-a"},
                {"type":"text","text":"user-b"}
            ]},
            {"role":"assistant","content":[
                {"type":"text","text":"assistant-a"},
                {"type":"text","text":"assistant-b"}
            ]},
            {"role":"user","content":"next"}
        ]});
        let bytes = build_request(&body, "grok-4.6", "c", false).unwrap();
        let messages: Vec<&[u8]> = fields(&bytes)
            .into_iter()
            .filter_map(|(field, value)| match (field, value) {
                (1, PVal::Len(message)) => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(str_of(messages[0], 2).as_deref(), Some("user-a\nuser-b"));
        assert_eq!(
            str_of(messages[1], 2).as_deref(),
            Some("assistant-a\nassistant-b")
        );
    }

    #[test]
    fn build_request_剥签名重试() {
        let body = json!({"messages": [{"role":"assistant","content":[
            {"type":"thinking","thinking":"hmm","signature":"SIG"},
            {"type":"redacted_thinking","data":"XYZ"},
            {"type":"text","text":"answer"}]}]});
        let stripped = build_request(&body, "grok-4.6", "c", true).unwrap();
        let m = len_of(&fields(&stripped), 1).expect("message");
        assert!(
            len_of(&fields(m), 7).is_none(),
            "strip 后不得有 reasoning_parts"
        );
        assert_eq!(str_of(m, 2).as_deref(), Some("answer"), "正文保留");
        assert!(history_has_signature(&body));
        let nosig =
            json!({"messages":[{"role":"assistant","content":[{"type":"text","text":"x"}]}]});
        assert!(!history_has_signature(&nosig));
    }

    #[test]
    fn build_request_关闭思考时不带effort() {
        let body = json!({"thinking": {"type": "disabled"}, "messages": []});
        let bytes = build_request(&body, "grok-4.6", "c", false).unwrap();
        let fs = fields(&bytes);
        let rm = len_of(&fs, 7).unwrap();
        let p1 = len_of(&fields(rm), 3).expect("参数");
        assert_eq!(
            str_of(p1, 1).as_deref(),
            Some("fast"),
            "disabled 时只留 fast,不带 effort"
        );
        // 默认(adaptive)→ effort=high
        let body2 = json!({"thinking": {"type": "adaptive"}, "messages": []});
        let bytes2 = build_request(&body2, "grok-4.6", "c", false).unwrap();
        let rm2 = len_of(&fields(&bytes2), 7).unwrap();
        let p2 = len_of(&fields(rm2), 3).unwrap();
        assert_eq!(str_of(p2, 1).as_deref(), Some("effort"));
    }

    // ── 流折叠 ──

    fn resp_frame(case: u32, sub: &Writer) -> Vec<u8> {
        let mut w = Writer::new();
        w.message(case, sub);
        w.into_bytes()
    }
    fn text_part(text: &str, fin: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(1, text);
        if fin {
            w.uint(2, 1);
        }
        resp_frame(1, &w)
    }
    fn thinking_part(text: &str, sig: Option<&str>, fin: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(1, text);
        if let Some(s) = sig {
            w.string(2, s);
        }
        if fin {
            w.uint(3, 1);
        }
        resp_frame(9, &w)
    }
    fn tool_call_part(id: &str, name: &str, args: &str, done: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(1, id);
        w.string(2, name);
        w.string(3, args);
        if done {
            w.uint(4, 1);
        }
        resp_frame(2, &w)
    }
    fn extended_usage(i: u64, o: u64, cr: u64, cw: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.uint(1, i);
        w.uint(2, o);
        w.uint(3, cr);
        w.uint(4, cw);
        resp_frame(5, &w)
    }

    fn declared(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn folder(model: &str, tools: &[&str]) -> Folder {
        Folder::new(model, declared(tools), true, 4000, false)
    }

    fn sse_jsons(f: &mut Folder) -> Vec<String> {
        f.take_pending()
            .into_iter()
            .map(|r| match r.unwrap() {
                StreamItem::Sse(ev) => ev.data.to_string(),
                StreamItem::Usage(u) => format!(
                    "USAGE in={} out={} cr={} cw={} real={}",
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_tokens,
                    u.cache_creation_tokens,
                    u.real_cache_read_tokens
                ),
                StreamItem::UpstreamCut => "CUT".into(),
            })
            .collect()
    }

    #[test]
    fn 流折叠_思考签名文本工具全路径() {
        let mut f = folder("grok-4.6", &["Bash"]);
        f.feed_frame(&thinking_part("思考中", None, false)).unwrap();
        f.feed_frame(&thinking_part("", Some("SIG1"), true))
            .unwrap();
        f.feed_frame(&text_part("答", false)).unwrap();
        f.feed_frame(&text_part("案", true)).unwrap();
        f.feed_frame(&tool_call_part("t1", "Bash", "{\"cmd\":", false))
            .unwrap();
        f.feed_frame(&tool_call_part("t1", "Bash", "\"ls\"}", true))
            .unwrap();
        f.feed_frame(&extended_usage(1000, 50, 900, 80)).unwrap();
        f.feed_frame(&resp_frame(4, &Writer::new())).unwrap(); // response_info
        f.finish();
        let evs = sse_jsons(&mut f);
        let joined = evs.join("\n");
        assert!(joined.contains("message_start"), "缺 message_start");
        assert!(joined.contains(r#""thinking""#), "缺 thinking 块");
        assert!(
            joined.contains("signature_delta"),
            "签名要以 signature_delta 下发"
        );
        assert!(joined.contains("答"), "缺文本");
        assert!(
            joined.contains("input_json_delta") && joined.contains("cmd") && joined.contains("ls"),
            "工具参数应按到达顺序拼全: {joined}"
        );
        assert!(joined.contains(r#"tool_use"#), "stop_reason 应为 tool_use");
        let usage_line = evs.iter().find(|e| e.starts_with("USAGE")).unwrap();
        assert!(
            usage_line.contains("in=1000"),
            "input_tokens 必须是总上下文: {usage_line}"
        );
        assert!(usage_line.contains("out=50"), "{usage_line}");
        assert!(
            usage_line.contains("real=900"),
            "真值完整落库: {usage_line}"
        );
        let stop_pos = evs.iter().position(|e| e.contains("message_stop")).unwrap();
        let usage_pos = evs.iter().position(|e| e.starts_with("USAGE")).unwrap();
        assert!(stop_pos < usage_pos);
    }

    #[test]
    fn 工具调用_isComplete未到_参数合法则flush() {
        let mut f = folder("grok-4.6", &["Bash"]);
        f.feed_frame(&tool_call_part("t1", "Bash", "{\"cmd\":\"ls\"}", false))
            .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains("content_block_stop"),
            "客户端不能等不到关块: {joined}"
        );
        assert!(joined.contains("tool_use"), "{joined}");
    }

    /// 模型把 `glob` 写成 `Glob`(生产实测):按声明清单规范名纠正,工具照常下发;
    /// 大小写折叠后有多个候选(歧义)才不纠,防错配。
    #[test]
    fn 工具名大小写笔误_按声明名纠正() {
        let mut f = folder("grok-4.6", &["glob", "write"]);
        f.feed_frame(&tool_call_part("t1", "Glob", "{\"pattern\":\"*.rs\"}", true))
            .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(joined.contains(r#""type":"tool_use""#), "{joined}");
        assert!(joined.contains(r#""name":"glob""#), "规范名下发: {joined}");
        // 真·未声明(折叠后也不沾)仍降级为文本。
        let mut f2 = folder("grok-4.6", &["glob"]);
        f2.feed_frame(&tool_call_part("t2", "GlobX", "{}", true)).unwrap();
        f2.finish();
        let j2 = sse_jsons(&mut f2).join("\n");
        assert!(!j2.contains(r#""type":"tool_use""#), "{j2}");
        assert!(j2.contains("未声明的工具调用"), "{j2}");
    }
    /// grok 实测帧型(2026-09-07):中间帧与完成帧**重复携带同一份全量 args**。
    /// 无脑拼接会得到 `{...}{...}` 非法 JSON;完成帧的 args 就是全量(官方
    /// agent-host 直接 JSON.parse(complete.args)),必须优先用它。
    #[test]
    fn 工具调用_完成帧重复全量args_不拼接() {
        let mut f = folder("grok-4.6", &["bash"]);
        let full = r#"{"command":"ls -la","description":"List files"}"#;
        f.feed_frame(&tool_call_part("t1", "bash", "", false)).unwrap();
        f.feed_frame(&tool_call_part("t1", "bash", full, false)).unwrap();
        f.feed_frame(&tool_call_part("t1", "bash", full, true)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(joined.contains("tool_use"), "{joined}");
        // partial_json 必须是单个合法 JSON object:恰好出现一次 key。
        assert_eq!(joined.matches(r#"\"command\""#).count(), 1, "{joined}");
        assert!(!joined.contains("不是合法 JSON"), "{joined}");
    }

    #[test]
    fn 工具参数非法JSON_报错不伪造() {
        let mut f = folder("grok-4.6", &["Bash"]);
        f.feed_frame(&tool_call_part("t1", "Bash", "{broken", true))
            .unwrap();
        f.finish();
        let items = f.take_pending();
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Err(e) if e.kind == UpstreamErrorKind::ServerError)),
            "非法参数必须报错,不能伪造空对象"
        );
        let oks: Vec<String> = items
            .into_iter()
            .filter_map(|i| i.ok())
            .filter_map(|i| match i {
                StreamItem::Sse(ev) => Some(ev.data.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            !oks.join("\n").contains(r#""type":"tool_use""#),
            "不得吐伪造的工具块"
        );
    }

    #[test]
    fn 未声明工具_无条件降级为文本() {
        // 客户端声明了别的工具
        let mut f = folder("grok-4.6", &["Bash"]);
        f.feed_frame(&tool_call_part("t1", "read_file", "{}", true))
            .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            !joined.contains(r#""type":"tool_use""#),
            "未声明工具不得吐 tool_use 块"
        );
        assert!(
            joined.contains("未声明的工具调用"),
            "应降级为文本: {joined}"
        );
        // 客户端压根没声明 tools:任何工具调用也是异常
        let mut f2 = folder("grok-4.6", &[]);
        f2.feed_frame(&tool_call_part("t1", "Bash", "{}", true))
            .unwrap();
        f2.finish();
        let j2 = sse_jsons(&mut f2).join("\n");
        assert!(
            !j2.contains(r#""type":"tool_use""#),
            "空声明下工具调用不得放行"
        );
    }

    #[test]
    fn 输出撞上限_按max_tokens正常收尾() {
        let mut f = folder("grok-4.6", &[]);
        f.feed_frame(&text_part("半句话", false)).unwrap();
        let mut w = Writer::new();
        w.string(1, "output limit");
        w.uint(5, 3);
        f.feed_frame(&resp_frame(8, &w)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains(r#"max_tokens"#),
            "stop_reason 必须是 max_tokens: {joined}"
        );
        assert!(
            joined.contains("message_stop"),
            "内容照发、正常收尾: {joined}"
        );
    }

    #[test]
    fn 输入撞上限_BadRequest不罚号() {
        let mut f = folder("grok-4.6", &[]);
        let mut w = Writer::new();
        w.string(1, "too long");
        w.uint(3, 1); // is_input_token_limit 布尔字段
        let err = f.feed_frame(&resp_frame(8, &w)).unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::BadRequest);
    }

    #[test]
    fn finish_exactly_once() {
        let mut f = folder("grok-4.6", &[]);
        f.feed_frame(&text_part("x", true)).unwrap();
        f.feed_frame(&extended_usage(10, 5, 0, 0)).unwrap();
        f.finish();
        let n1 = sse_jsons(&mut f).len();
        f.finish(); // 第二次必须是空操作
        assert!(f.take_pending().is_empty());
        let stops = sse_jsons_count(&mut f);
        let _ = stops;
        assert!(n1 >= 4, "start+delta+stop+stop+usage 至少这些: {n1}");
    }
    fn sse_jsons_count(_f: &mut Folder) -> usize {
        0
    }

    #[test]
    fn 零产出_finish报EmptyResponse() {
        let mut f = folder("grok-4.6", &[]);
        f.finish();
        let items = f.take_pending();
        assert!(matches!(
            items.as_slice(),
            [Err(e)] if e.kind == UpstreamErrorKind::EmptyResponse
        ));
    }

    #[test]
    fn 零产出但回放非空_报协议漂移() {
        let mut f = folder("grok-4.6", &[]);
        let mut ri = Writer::new();
        let mut m = Writer::new();
        m.string(3, "回放内容");
        ri.message(4, &m);
        f.feed_frame(&resp_frame(4, &ri)).unwrap();
        f.finish();
        let items = f.take_pending();
        assert!(
            matches!(
                items.as_slice(),
                [Err(e)] if e.kind == UpstreamErrorKind::Other
            ),
            "回放非空而流式为空 = 解析器落后,必须报错"
        );
    }

    #[test]
    fn 无usage帧_按估算入账不全零() {
        let mut f = folder("grok-4.6", &[]);
        f.feed_frame(&text_part("一些中文输出内容", true)).unwrap();
        f.finish();
        let evs = sse_jsons(&mut f);
        let usage_line = evs.iter().find(|e| e.starts_with("USAGE")).unwrap();
        assert!(
            !usage_line.contains("in=0"),
            "input 不能零入账: {usage_line}"
        );
        assert!(
            !usage_line.contains("out=0"),
            "output 不能零入账: {usage_line}"
        );
    }

    #[test]
    fn 关闭思考_思考帧不下发但计费() {
        let mut f = Folder::new("grok-4.6", declared(&[]), false, 4000, false);
        f.feed_frame(&thinking_part("秘密思考", Some("S"), true))
            .unwrap();
        f.feed_frame(&text_part("答案", true)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            !joined.contains("thinking"),
            "disabled 时不下发 thinking 块: {joined}"
        );
        assert!(joined.contains("答案"));
    }

    #[test]
    fn 正文后迟到的thinking丢弃() {
        let mut f = folder("grok-4.6", &[]);
        f.feed_frame(&text_part("正文", false)).unwrap();
        f.feed_frame(&thinking_part("迟到", None, true)).unwrap();
        f.feed_frame(&text_part("继续", true)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            !joined.contains("迟到"),
            "正文后的 thinking 必须丢弃: {joined}"
        );
        assert!(joined.contains("继续"));
    }

    #[test]
    fn 流内error帧_限流映射() {
        let mut f = folder("grok-4.6", &[]);
        let mut w = Writer::new();
        w.string(1, "slow down");
        w.string(2, "RATE");
        w.uint(5, 4);
        let err = f.feed_frame(&resp_frame(8, &w)).unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::RateLimited);
    }

    #[test]
    fn permission不罚号() {
        let e = map_stream_error(6, "PERM", "model not in your tier");
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
        // quota 字样也不升级(QuotaExhausted 会永久禁号,等实测到池爆形态再单列)
        let e = map_stream_error(6, "PERM", "quota exceeded for this period");
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
    }

    #[test]
    fn trailer错误映射() {
        let mut f = folder("grok-4.6", &[]);
        let e = f
            .on_trailer(
                0,
                br#"{"error":{"code":"unauthenticated","message":"bad token"}}"#,
            )
            .unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
        let mut f2 = folder("grok-4.6", &[]);
        f2.on_trailer(0, b"{}").unwrap();
        assert!(f2.finished);
        // 非空但解不出 JSON = 协议漂移报错
        let mut f3 = folder("grok-4.6", &[]);
        let e = f3.on_trailer(0, b"\x01\x02garbage").unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
    }

    #[test]
    fn 模型不存在_trailer映射ModelNotAvailable() {
        let mut f = folder("grok-4.6", &[]);
        let e = f
            .on_trailer(
                0,
                br#"{"error":{"code":"not_found","message":"AI Model Not Found"}}"#,
            )
            .unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
    }

    #[test]
    fn 供应商故障_映射Overloaded而非RateLimited() {
        // 2026-09-03 生产原文:xAI(grok)宕机时 END trailer 长这样。
        // 误判成 RateLimited 会冷却账号、株连健康模型流量(选号失败),必须 Overloaded。
        let provider_422 = br#"{"error":{"code":"resource_exhausted","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails","debug":{"error":"ERROR_PROVIDER_ERROR","details":{"title":"Provider Error","detail":"We're having trouble connecting to the model provider.","isRetryable":false,"additionalInfo":{"providerStatusCode":"422"}},"isExpected":true},"value":"x"}]},"metadata":{"x-cursor-inference-request-error-type":["PROVIDER_ERROR"]}}"#;
        let mut f = folder("grok-4.6", &[]);
        let e = f.on_trailer(0, provider_422).unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::Overloaded, "PROVIDER_ERROR 必须 Overloaded: {e}");

        let provider_re = br#"{"error":{"code":"resource_exhausted","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails","debug":{"error":"ERROR_RESOURCE_EXHAUSTED","details":{"title":"Unable to reach the model provider","detail":"We're having trouble connecting to the model provider."},"isExpected":true},"value":"x"}]},"metadata":{"x-cursor-inference-request-error-type":["RESOURCE_EXHAUSTED"]}}"#;
        let mut f2 = folder("grok-4.6", &[]);
        let e2 = f2.on_trailer(0, provider_re).unwrap_err();
        assert_eq!(e2.kind, UpstreamErrorKind::Overloaded, "provider 字样的 RESOURCE_EXHAUSTED 必须 Overloaded: {e2}");

        // 对照:没有 provider 痕迹的 resource_exhausted 仍然是 RateLimited(真限流)
        let genuine = br#"{"error":{"code":"resource_exhausted","message":"rate limit"}}"#;
        let mut f3 = folder("grok-4.6", &[]);
        let e3 = f3.on_trailer(0, genuine).unwrap_err();
        assert_eq!(e3.kind, UpstreamErrorKind::RateLimited, "真限流不受影响: {e3}");
    }

    #[test]
    fn http分类_401罚号403不罚() {
        let e = classify_http_error(401, "invalid");
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
        let e = classify_http_error(403, "<html>cloudflare</html>");
        assert_eq!(
            e.kind,
            UpstreamErrorKind::Other,
            "403 无结构化体 = IP 被拦,不罚号"
        );
        let e = classify_http_error(429, "{}");
        assert_eq!(e.kind, UpstreamErrorKind::RateLimited);
        // 结构化 permission_denied 不罚号(与 Run 路径分类器的关键差异)
        let e = classify_http_error(403, r#"{"code":"permission_denied","message":"tier"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
    }

    #[test]
    fn 计费_真实缓存值原样透传() {
        // 运营拍板(2026-08-26):真值不夹不模拟,cache_read = 上游原值。
        let mut f = folder("grok-4.6", &[]);
        f.feed_frame(&text_part("x", true)).unwrap();
        f.feed_frame(&extended_usage(100000, 10, 99600, 0)).unwrap();
        f.finish();
        let items = sse_jsons(&mut f);
        let usage_line = items
            .iter()
            .find(|e| e.starts_with("USAGE"))
            .unwrap()
            .clone();
        assert!(usage_line.contains("in=100000"), "input 总量: {usage_line}");
        assert!(
            usage_line.contains("cr=99600"),
            "cache_read 原样透传: {usage_line}"
        );
        assert!(usage_line.contains("real=99600"), "{usage_line}");
    }

    #[test]
    fn model_config的温度是float32() {
        // 钉住 2026-08-26 灰度事故:temperature/top_p 必须是 fixed32(wt=5),
        // 编 double(wt=1) 上游解析脱同步报 "parse binary: illegal tag"。
        let body = json!({"max_tokens": 64, "temperature": 0.7, "top_p": 0.9, "messages": []});
        let bytes = build_request(&body, "grok-4.6", "c", false).unwrap();
        let fs = fields(&bytes);
        let mc = len_of(&fs, 4).expect("model_config");
        let mcf = fields(mc);
        let temp = mcf.iter().find(|(f, _)| *f == 2).expect("temperature");
        match temp.1 {
            PVal::Fixed32(b) => assert!((f32::from_le_bytes(b) - 0.7).abs() < 1e-6),
            _ => panic!("temperature 必须是 fixed32(fixed64 = 事故回归)"),
        }
        let topp = mcf.iter().find(|(f, _)| *f == 3).expect("top_p");
        assert!(matches!(topp.1, PVal::Fixed32(_)), "top_p 必须是 fixed32");
    }

    #[test]
    fn tools绕行门控() {
        use serde_json::json;
        // 本用例断言「开关全关」的行为,必须持门面锁(别的用例会翻全局开关)。
        let _guard = Field9Guard::lock();
        set_field9_tools(false);
        set_text_tools(false);
        let tool = json!({"name":"get_weather","description":"d","input_schema":{"type":"object"}});
        let with_tools = json!({"messages":[{"role":"user","content":"hi"}],"tools":[tool]});
        let no_tools = json!({"messages":[{"role":"user","content":"hi"}]});
        let empty_tools = json!({"messages":[{"role":"user","content":"hi"}],"tools":[]});
        // 带 tools:grok/claude 绕行(平台侧必拒,见 tools_skip_inference 注释)
        assert!(tools_skip_inference("grok-4.6", &with_tools));
        assert!(tools_skip_inference("claude-sonnet-5", &with_tools));
        // composer 带 tools 正常,不绕行
        assert!(!tools_skip_inference("composer-2.5", &with_tools));
        // 无 tools / 空 tools:不绕行
        assert!(!tools_skip_inference("grok-4.6", &no_tools));
        assert!(!tools_skip_inference("grok-4.6", &empty_tools));
    }

    /// field9 门面(2026-09-07 实弹):开关开 + grok 系 + 带 tools → 不绕行,
    /// tools=2 留空、工具名进 field 9、schema 文本进 system;claude 系不受门面影响。
    #[test]
    fn field9门面_报文形态() {
        use serde_json::json;
        let tool = json!({"name":"get_weather","description":"Get weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}});
        let body = json!({
            "system": "You are helpful.",
            "messages":[{"role":"user","content":"weather in Tokyo?"}],
            "tools":[tool],
        });
        let _guard = Field9Guard::lock();
        set_field9_tools(true);
        // 门控:grok 不绕行,claude 照旧绕行,composer 不受影响
        assert!(!tools_skip_inference("grok-4.6", &body));
        assert!(tools_skip_inference("claude-opus-5", &body));
        let bytes = build_request(&body, "grok-4.6", "c", false).unwrap();
        let fs = fields(&bytes);
        // tools=2 不存在;field 9 = 工具名
        assert!(len_of(&fs, 2).is_none(), "门面下不该有 tools 数组");
        let f9: Vec<&[u8]> = fs
            .iter()
            .filter(|(f, _)| *f == 9)
            .filter_map(|(_, v)| match v {
                PVal::Len(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(f9.len(), 1, "field 9 应恰好一条: {f9:?}");
        assert_eq!(f9[0], b"get_weather");
        // system 消息带工具清单文本(schema canonical JSON 在内)
        let sys = len_of(&fs, 1).expect("system 消息");
        let sysf = fields(sys);
        let text = sysf
            .iter()
            .find(|(f, _)| *f == 2)
            .and_then(|(_, v)| match v {
                PVal::Len(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .expect("system 文本");
        assert!(text.contains("You are helpful."), "原 system 保留: {text}");
        assert!(text.contains("get_weather"), "工具名进清单: {text}");
        assert!(
            text.contains(r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#)
                || text.contains(r#""city":{"type":"string"}"#),
            "schema 进清单: {text}"
        );
    }

    /// 开关关(默认)时 grok 带 tools 照旧绕行 —— 回滚闸。
    #[test]
    fn field9门面_默认关() {
        use serde_json::json;
        let tool = json!({"name":"t","description":"d"});
        let body = json!({"messages":[{"role":"user","content":"hi"}],"tools":[tool]});
        let _guard = Field9Guard::lock();
        set_field9_tools(false);
        assert!(tools_skip_inference("grok-4.6", &body));
    }

    /// 测试结束把全局开关拨回关,避免串扰;门面用例全程持互斥锁串行
    ///(cargo test 默认并发作线程,两个用例同时翻全局开关会互踩)。
    struct Field9Guard(std::sync::MutexGuard<'static, ()>);
    impl Field9Guard {
        fn lock() -> Self {
            static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
            Self(M.lock().unwrap_or_else(|p| p.into_inner()))
        }
    }
    impl Drop for Field9Guard {
        fn drop(&mut self) {
            set_field9_tools(false);
            set_text_tools(false);
        }
    }

    /// TextEmu 门面报文形态:tools=2 与 field 9 都不发,契约+schema 进 system,
    /// 历史 tool_use/tool_result 渲染成文本块。
    #[test]
    fn textemu门面_报文形态() {
        use serde_json::json;
        let _guard = Field9Guard::lock();
        set_text_tools(true);
        let tool = json!({"name":"get_weather","description":"Get weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}});
        let body = json!({
            "system": "You are helpful.",
            "messages":[
                {"role":"user","content":"weather?"},
                {"role":"assistant","content":[
                    {"type":"text","text":"Let me check."},
                    {"type":"tool_use","id":"call-x-0","name":"get_weather","input":{"city":"Tokyo"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call-x-0","content":"sunny"}
                ]}
            ],
            "tools":[tool],
        });
        // 门控:claude 开 text_tools 不绕行;grokk 没开 field9 仍绕行
        assert!(!tools_skip_inference("claude-opus-5", &body));
        assert!(tools_skip_inference("grok-4.6", &body));
        let bytes = build_request(&body, "claude-opus-5", "c", false).unwrap();
        let fs = fields(&bytes);
        assert!(len_of(&fs, 2).is_none(), "TextEmu 不该有 tools 数组");
        assert!(
            !fs.iter().any(|(f, _)| *f == 9),
            "TextEmu 不该发 field 9"
        );
        // system 含契约与 schema
        let sys = len_of(&fs, 1).expect("system 消息");
        let text = fields(sys)
            .iter()
            .find(|(f, _)| *f == 2)
            .and_then(|(_, v)| match v {
                PVal::Len(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .expect("system 文本");
        assert!(text.contains("You are helpful."), "{text}");
        assert!(text.contains("<tool_call>"), "契约进 system: {text}");
        assert!(text.contains("get_weather"), "{text}");
        // 契约必须明令否决发现/元工具:模型对未声明名发调用会被静默降级,
        // 然后陷入"先取清单"空转(2026-09-08 生产诊断)。
        assert!(
            text.contains("NO discovery or meta tool") && text.contains("GetDynamicTools"),
            "system 契约否决发现工具: {text}"
        );
        // 消息序列:system / user / assistant(文本化 tool_use)/ user(文本化 result)
        // / user(尾部契约提醒,2026-09-07 消融实验后新增)
        let msgs: Vec<&[u8]> = fs
            .iter()
            .filter(|(f, _)| *f == 1)
            .filter_map(|(_, v)| match v {
                PVal::Len(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 5, "5 条消息: {msgs:?}");
        let msg_text = |m: &[u8]| {
            fields(m)
                .iter()
                .find(|(f, _)| *f == 2)
                .and_then(|(_, v)| match v {
                    PVal::Len(b) => Some(String::from_utf8_lossy(b).into_owned()),
                    _ => None,
                })
        };
        let asst = msg_text(msgs[2]).expect("assistant 文本");
        assert!(
            asst.contains(r#"<tool_call>{"name":"get_weather","arguments":{"city":"Tokyo"}}</tool_call>"#),
            "历史 tool_use 文本化: {asst}"
        );
        assert!(asst.contains("Let me check."), "原正文保留: {asst}");
        let res = msg_text(msgs[3]).expect("result 文本");
        assert!(
            res.contains(r#"<tool_result name="get_weather">sunny</tool_result>"#),
            "tool_result 文本化: {res}"
        );
        let tail = msg_text(msgs[4]).expect("尾部提醒文本");
        assert!(
            tail.contains("<system-reminder>") && tail.contains("<tool_call>"),
            "尾部提醒契约: {tail}"
        );
        assert!(tail.contains("get_weather"), "尾部提醒带工具名清单: {tail}");
        // 提醒里绝不能出现真实工具名之外的幻觉诱饵(措辞别随手改)
        assert!(tail.contains("NEVER"), "{tail}");
        // 尾部同样否决发现工具(模型最后读到的位置)
        assert!(tail.contains("no GetMcpTools"), "尾部否决发现工具: {tail}");
    }

    /// 尾部提醒只钉在 user 结尾的会话;assistant 结尾(prefill)不追加,
    /// 免得破坏 prefill 语义。
    #[test]
    fn textemu尾部提醒_prefill结尾不追加() {
        use serde_json::json;
        let _guard = Field9Guard::lock();
        set_text_tools(true);
        let tool = json!({"name":"bash","description":"run","input_schema":{"type":"object"}});
        let body = json!({
            "messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[{"type":"text","text":"prefill"}]}
            ],
            "tools":[tool],
        });
        let bytes = build_request(&body, "claude-opus-5", "c", false).unwrap();
        let fs = fields(&bytes);
        let msgs: Vec<&[u8]> = fs
            .iter()
            .filter(|(f, _)| *f == 1)
            .filter_map(|(_, v)| match v {
                PVal::Len(b) => Some(*b),
                _ => None,
            })
            .collect();
        let last_text = fields(msgs[msgs.len() - 1])
            .iter()
            .find(|(f, _)| *f == 2)
            .and_then(|(_, v)| match v {
                PVal::Len(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            !last_text.contains("<system-reminder>"),
            "prefill 结尾不该追加提醒: {last_text}"
        );
    }

    /// TextEmu 折叠:调用块横跨多个 delta 也能完整切出;前文后文各自成块。
    #[test]
    fn textemu_流折叠_调用块跨delta() {
        let mut f = Folder::new(
            "claude-opus-5",
            declared(&["get_weather"]),
            true,
            4000,
            true,
        );
        let deltas = [
            "Checking ",
            "the weather <tool_",
            "call>{\"name\":\"get_weather\",\"args\":{\"ci",
            "ty\":\"Tokyo\"}}</tool_call> Done.",
        ];
        for d in deltas {
            f.feed_frame(&text_part(d, false)).unwrap();
        }
        f.finish();
        let evs = sse_jsons(&mut f);
        // 文本 delta 逐段拼接必须无损还原(标签两侧的空格都在)。
        let text: String = evs
            .iter()
            .filter(|e| e.contains(r#""type":"text_delta""#))
            .filter_map(|e| serde_json::from_str::<Json>(e).ok())
            .filter_map(|e| {
                e.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Json::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "Checking the weather  Done.", "文本还原: {text:?}");
        let joined = evs.join("\n");
        assert!(joined.contains(r#""type":"tool_use""#), "{joined}");
        assert!(joined.contains("Tokyo"), "{joined}");
        // stop_reason 必须是 tool_use,客户端 agent 循环才会去执行。
        assert!(joined.contains(r#"tool_use"#), "{joined}");
    }

    /// TextEmu 诚实降级:块内不是合法 JSON / 未声明工具 → 原文按文本吐,
    /// 不伪造 tool_use;流尾半截标签也按原文放。
    #[test]
    fn textemu_坏块与半截标签降级() {
        let mut f = Folder::new("claude-opus-5", declared(&["get_weather"]), true, 4000, true);
        f.feed_frame(&text_part("A<tool_call>{broken}</tool_call>B", false))
            .unwrap();
        f.feed_frame(&text_part("C<tool_call>{\"name\":\"undeclared\",\"arguments\":{}}</tool_call>D", false))
            .unwrap();
        f.feed_frame(&text_part("E<tool_call>{\"name\":\"get_weather\"", false))
            .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(!joined.contains(r#""type":"tool_use""#), "不该有工具块: {joined}");
        assert!(joined.contains("{broken}"), "坏 JSON 原文: {joined}");
        assert!(joined.contains("undeclared"), "未声明原文: {joined}");
        assert!(
            joined.contains(r#"<tool_call>{\"name\":\"get_weather\""#),
            "半截标签原文: {joined}"
        );
        // 降级必须附网关负反馈(模型下一轮能在历史里读到),否则它会以为
        // "调用发出去了没回"而陷入"先取清单"空转(2026-09-08 生产诊断)。
        // 反馈措辞按真实原因分类:未声明 ≠ 畸形,别让模型被误诊带偏(2026-09-09
        // 生产实态:模型丢最外层 } 被判成"未声明",对着清单里存在的名字空转)。
        assert!(
            joined.contains(r#"tool call NOT executed — \"undeclared\" is not a declared tool"#),
            "未声明调用应带名字反馈: {joined}"
        );
        assert!(
            joined.contains("malformed or unbalanced"),
            "坏块应带畸形反馈: {joined}"
        );
    }

    /// 2026-09-09 生产实弹:模型写深层嵌套参数丢掉**最外层**收尾 `}`
    /// (`{"name":"todowrite","arguments":{"todos":[…]}` 就闭标签)。窄补齐后
    /// 应正常产出 tool_use 而不是降级。
    #[test]
    fn textemu_丢最外层收尾括号被补齐() {
        let mut f = Folder::new("claude-fable-5-1", declared(&["todowrite"]), true, 4000, true);
        f.feed_frame(
            &text_part(
                "<tool_call>{\"name\":\"todowrite\",\"arguments\":{\"todos\":[\
                 {\"content\":\"甲\",\"status\":\"pending\"},\
                 {\"content\":\"乙\",\"status\":\"done\"}\
                 ]}</tool_call>",
                false,
            ),
        )
        .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains(r#""type":"tool_use""#),
            "补齐后应产出 tool_use 而非降级文本: {joined}"
        );
        assert!(joined.contains("todowrite"), "{joined}");
        assert!(joined.contains("乙"), "参数内容不丢: {joined}");
        assert!(
            !joined.contains("NOT executed"),
            "不该出现降级负反馈: {joined}"
        );
    }

    /// 截断疑云不修:扫完仍停在字符串**里面**的块(像被掐断),补括号等于虚构
    /// 参数 —— 原样降级,绝不能伪造调用。
    #[test]
    fn textemu_字符串内截断不虚构() {
        let mut f = Folder::new("claude-opus-5", declared(&["bash"]), true, 4000, true);
        f.feed_frame(
            &text_part(
                "<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"rm -rf /tmp/x</tool_call>",
                false,
            ),
        )
        .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            !joined.contains(r#""type":"tool_use""#),
            "截断块绝不能产出 tool_use: {joined}"
        );
        assert!(
            joined.contains("malformed or unbalanced"),
            "截断块应带畸形反馈: {joined}"
        );
    }

    #[test]
    fn repair_json_unclosed_只补括号且不碰字符串() {
        // 生产原形:数组/对象都合了,根 } 丢了。
        let raw = r#"{"name":"todowrite","arguments":{"todos":[{"content":"a"}]}"#;
        let fixed = repair_json_unclosed(raw);
        assert_eq!(fixed, format!("{raw}}}"));
        let v: Json = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["name"], "todowrite");
        // 合法 JSON 原样不动。
        let ok = r#"{"name":"bash","arguments":{"command":"ls"}}"#;
        assert_eq!(repair_json_unclosed(ok), ok);
        // 字符串内截断:原样返回(不虚构)。
        let trunc = r#"{"name":"bash","arguments":{"command":"rm -rf"#;
        assert_eq!(repair_json_unclosed(trunc), trunc);
        // 字符串里的括号不计数:{"text":"}]} 合不上是假的"} —— 已合法,不动。
        let tricky = r#"{"text":"}]}"}"#;
        assert_eq!(repair_json_unclosed(tricky), tricky);
    }

    /// 模型幻觉的 `<tool_result>` 块(契约禁止,opus-5 实弹会抢答)→ 整段丢弃,
    /// 两侧文本保留;`<tool_results>` 这类相似拼写不是我们的标签,原样放行。
    #[test]
    fn textemu_幻觉tool_result块被丢弃() {
        let mut f = Folder::new("claude-opus-5", declared(&["get_weather"]), true, 4000, true);
        f.feed_frame(&text_part("前<tool_", false)).unwrap();
        f.feed_frame(&text_part("result name=\"bash\">2026 Monday</tool_result>中", false))
            .unwrap();
        f.feed_frame(&text_part("<tool_results> 是别的东西 </tool_results>后", false))
            .unwrap();
        f.finish();
        let evs = sse_jsons(&mut f);
        let text: String = evs
            .iter()
            .filter(|e| e.contains(r#""type":"text_delta""#))
            .filter_map(|e| serde_json::from_str::<Json>(e).ok())
            .filter_map(|e| {
                e.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Json::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "前中<tool_results> 是别的东西 </tool_results>后", "{text:?}");
    }

    #[test]
    fn textemu_裸布尔旗标被修复() {
        // 2026-09-08 生产实弹畸形:{"name":"grep","arguments":{"-n","glob":...}} —
        // 布尔旗标写成裸键,严格 JSON 不合法。窄修复后应正常产出 tool_use。
        let mut f = Folder::new("claude-opus-5", declared(&["grep"]), true, 4000, true);
        f.feed_frame(
            &text_part(
                "<tool_call>{\"name\":\"grep\",\"arguments\":{\"-n\",\"glob\":\"*.{h,cpp}\",\"pattern\":\"class T\"}}</tool_call>",
                false,
            ),
        )
        .unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains(r#""type":"tool_use""#),
            "修复后应产出 tool_use 而非降级文本: {joined}"
        );
        assert!(joined.contains(r#""-n\":true"#), "裸键补 true: {joined}");
        assert!(joined.contains("class T"), "其余参数原样保留: {joined}");
    }

    #[test]
    fn repair_json_bare_flags_只修对象key位置() {
        // 合法 JSON 原样不动(含值位置的字符串)。
        let ok = r#"{"name":"grep","arguments":{"pattern":"class T","-n":true}}"#;
        assert_eq!(repair_json_bare_flags(ok), ok);
        // 对象末尾的裸键(} 前)。
        assert_eq!(
            repair_json_bare_flags(r#"{"a":{"x":1,"flag"}}"#),
            r#"{"a":{"x":1,"flag":true}}"#
        );
        // 嵌套对象里的裸键,后面还跟着正常键值对。
        assert_eq!(
            repair_json_bare_flags(r#"{"name":"g","arguments":{"-n","glob":"*.rs"}}"#),
            r#"{"name":"g","arguments":{"-n":true,"glob":"*.rs"}}"#
        );
        // 字符串值内含逗号/引号转义/括号:不受影响。
        let tricky = r#"{"a":"x,\"}, y"}"#;
        assert_eq!(repair_json_bare_flags(tricky), tricky);
        // 数组里的字符串不是 key,不修。
        let arr = r#"{"a":["x","y"]}"#;
        assert_eq!(repair_json_bare_flags(arr), arr);
        // 截断 JSON 依旧不合法(修不了的东西不伪造),调用方走诚实降级。
        let cut = r#"{"name":"grep","arguments":{"pattern":"class T"#;
        assert!(serde_json::from_str::<Json>(&repair_json_bare_flags(cut)).is_err());
    }

    #[test]
    fn textemu_字符串内字面换行被修复() {
        // 2026-09-08 生产实弹(Windows CC 用户):heredoc 里的真换行写进 JSON
        // 字符串,严格解析失败 → 整条调用降级成用户可见原文,CC 干等。
        let mut f = Folder::new("claude-opus-5", declared(&["Bash"]), true, 4000, true);
        let payload = "<tool_call>{\"name\":\"Bash\",\"arguments\":{\"command\":\"cd /e/proj && cat > /tmp/x.py <<'EOF'\nimport sys\nprint('hi')\nEOF\npython3 /tmp/x.py\"}}</tool_call>";
        f.feed_frame(&text_part(payload, false)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains(r#""type":"tool_use""#),
            "字面换行修复后应产出 tool_use: {joined}"
        );
        assert!(joined.contains("import sys"), "heredoc 内容应保留: {joined}");
        assert!(
            !joined.contains("<tool_call>"),
            "修复成功后原文标签不得泄漏: {joined}"
        );
    }

    #[test]
    fn repair_json_string_escapes_只动字符串内() {
        // 字符串内的真换行/制表符被转义
        assert_eq!(
            repair_json_string_escapes("{\"a\":\"x\ny\tz\"}"),
            "{\"a\":\"x\\ny\\tz\"}"
        );
        // 字符串外的换行(排版)原样保留
        assert_eq!(
            repair_json_string_escapes("{\"a\":1,\n\"b\":2}"),
            "{\"a\":1,\n\"b\":2}"
        );
        // 已合法的转义不重复转义(含 \n 与 \uXXXX)
        assert_eq!(
            repair_json_string_escapes("{\"a\":\"x\\ny\\u0041z\"}"),
            "{\"a\":\"x\\ny\\u0041z\"}"
        );
        // 非法转义(正则 \d \s):反斜杠加倍 —— 2026-09-08 生产实弹形态
        assert_eq!(
            repair_json_string_escapes("{\"a\":\"(\\d)|(\\s)\"}"),
            "{\"a\":\"(\\\\d)|(\\\\s)\"}"
        );
        // 已合法的双反斜杠与非法单反斜杠混排(同一条生产报文里的真实情况)
        assert_eq!(
            repair_json_string_escapes("{\"a\":\"\\\\s*(\\d)\"}"),
            "{\"a\":\"\\\\s*(\\\\d)\"}"
        );
        // 与裸键修复链式兼容:两类畸形一次修好
        let both = "{\"name\":\"g\",\"arguments\":{\"-n\",\"cmd\":\"a\nb (\\d)\"}}";
        let fixed = repair_json_bare_flags(&repair_json_string_escapes(both));
        let v = serde_json::from_str::<Json>(&fixed).unwrap();
        assert_eq!(v["arguments"]["-n"], true);
        assert_eq!(v["arguments"]["cmd"], "a\nb (\\d)");
    }

    #[test]
    fn textemu_裸引号经宽容解析救回() {
        // 2026-09-08 生产实弹(Windows CC 用户):write_file/Bash 的参数里塞 python
        // 代码,内容引号("depthTestEnable": 这种)让严格解析与两级修复全部失败。
        let mut f = Folder::new("claude-opus-5", declared(&["Bash"]), true, 4000, true);
        let payload = "<tool_call>{\"name\":\"Bash\",\"arguments\":{\"command\":\"python3 -c '\nres = {\n    \"depthTestEnable\": bool(ds.depthTestEnable),\n}\nprint(res)'\n\",\"max_lines\":120}}</tool_call>";
        f.feed_frame(&text_part(payload, false)).unwrap();
        f.finish();
        let joined = sse_jsons(&mut f).join("\n");
        assert!(
            joined.contains(r#""type":"tool_use""#),
            "裸引号块应被宽容解析救回: {joined}"
        );
        assert!(joined.contains("depthTestEnable"), "内容应完整: {joined}");
        assert!(!joined.contains("<tool_call>"), "原文标签不得泄漏: {joined}");
    }

    #[test]
    fn 宽容解析器_边界形态() {
        // 正常块也过宽容解析(幂等)
        let ok = r#"{"name":"Bash","arguments":{"command":"ls","max_lines":10}}"#;
        let v = parse_tool_call_lenient(ok).unwrap();
        assert_eq!(v["arguments"]["max_lines"], 10);
        // 多键 + 尾带标量 + 内容含裸引号与真换行(生产 2239933 同形态)
        let tricky = "{\"name\":\"bash\",\"arguments\":{\"command\":\"cat > /tmp/x.py <<'EOF'\nd = {\"a\": 1}\nEOF\npython3 /tmp/x.py\",\"timeout\":600}}";
        let v = parse_tool_call_lenient(tricky).unwrap();
        assert_eq!(v["arguments"]["timeout"], 600);
        assert!(v["arguments"]["command"].as_str().unwrap().contains("\"a\": 1"));
        // 数组参数(宽容路径递归)
        let arr = r#"{"name":"f","arguments":{"ids":[1,2,3],"q":"x"}}"#;
        let v = parse_tool_call_lenient(arr).unwrap();
        assert_eq!(v["arguments"]["ids"], serde_json::json!([1, 2, 3]));
        // 尾巴带垃圾 → 拒(形态不符,宁可降级)
        assert!(parse_tool_call_lenient(r#"{"name":"f","arguments":{}} trailing"#).is_none());
        // 半截块(无终止) → 拒
        assert!(parse_tool_call_lenient(r#"{"name":"f","arguments":{"a":"bc"#).is_none());
    }

    #[test]
    fn eligible门控() {
        use base64::Engine as _;
        use serde_json::json;
        // 正常:尾轮 user
        let ok = json!({"messages":[{"role":"user","content":"hi"}]});
        assert!(inference_eligible(&ok));
        // 空 messages → 不接
        assert!(!inference_eligible(&json!({"messages":[]})));
        // prefill:尾轮 assistant → 2026-09-03 实测上游 200 接受,接
        let prefill = json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"续"}]});
        assert!(inference_eligible(&prefill));
        // base64 PDF document → 接(构建期抽文本层注入)
        let doc = json!({"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","data":"eA==","media_type":"application/pdf"}}]}]});
        assert!(inference_eligible(&doc));
        // 非 PDF 文档 / 非 base64 文档源 → 不接
        let txt_doc = json!({"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","data":"eA==","media_type":"text/plain"}}]}]});
        assert!(!inference_eligible(&txt_doc));
        let url_doc = json!({"messages":[{"role":"user","content":[{"type":"document","source":{"type":"url","url":"http://x","media_type":"application/pdf"}}]}]});
        assert!(!inference_eligible(&url_doc));
        // base64 图片 → 接
        let img = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"eA==","media_type":"image/png"}}]}]});
        assert!(inference_eligible(&img));
        let invalid = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"不是base64","media_type":"image/png"}}]}]});
        assert!(!inference_eligible(&invalid));
        let missing = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png"}}]}]});
        assert!(!inference_eligible(&missing));
        let oversized =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_ONE_IMAGE + 1]);
        let huge = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":oversized,"media_type":"image/png"}}]}]});
        assert!(!inference_eligible(&huge));
        // tool_result 内嵌 document(合法 base64 PDF)→ 接
        let nested_doc = json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"document","source":{"type":"base64","data":"eA==","media_type":"application/pdf"}}]}]}]});
        assert!(inference_eligible(&nested_doc));
        let nested_invalid = json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"image","source":{"type":"base64","data":"bad!","media_type":"image/png"}}]}]}]});
        assert!(!inference_eligible(&nested_invalid));
        // URL 图片 → 不接
        let url = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"http://x"}}]}]});
        assert!(!inference_eligible(&url));
        // 未知块类型 → fail-closed(编码器会静默丢,落回 cli/wire 更诚实)
        let unknown = json!({"messages":[{"role":"user","content":[{"type":"future_block","x":1}]}]});
        assert!(!inference_eligible(&unknown));
        // CC 工具搜索的 tool_reference(嵌套在 tool_result 里,生产实弹 2241573)→ 接
        let tool_ref = json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"x"},{"type":"tool_reference","tool_name":"TaskOutput"}]}]}]});
        assert!(inference_eligible(&tool_ref));
        // 顶层 tool_reference → 接
        let top_ref = json!({"messages":[{"role":"user","content":[{"type":"tool_reference","tool_name":"TaskOutput"}]}]});
        assert!(inference_eligible(&top_ref));
        // 嵌套 tool_result(两层)→ 编码器只处理一层,fail-closed
        let nested_tr = json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"tool_result","tool_use_id":"t2","content":"x"}]}]}]});
        assert!(!inference_eligible(&nested_tr));
        // thinking/tool_use 等编码器认识的块 → 接
        let thinking = json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"..."},{"type":"text","text":"ok"}]},{"role":"user","content":"go"}]});
        assert!(inference_eligible(&thinking));
        // 消息级 fail-closed(codex 二轮 M4):非对象消息 / 非标量 content /
        // tool_result 的对象 content —— 编码器会静默丢,门控必须拒
        assert!(!inference_eligible(&json!({"messages":[null]})));
        assert!(!inference_eligible(&json!({"messages":[{"role":"user","content":42}]})));
        assert!(!inference_eligible(&json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":{"x":1}}]}]})));
        // 字符串/缺省 content 的 tool_result 不受影响
        assert!(inference_eligible(&json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]})));
    }

    #[test]
    fn document块注入文本层() {
        use base64::Engine as _;
        use serde_json::json;
        // 假 PDF(抽不到文本层)→ 注入「无法读取」说明而不是静默丢弃
        let fake_pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 fake");
        let body = json!({"max_tokens":64,"messages":[{"role":"user","content":[
            {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":fake_pdf}},
            {"type":"text","text":"看下这个文档"}
        ]}]});
        assert!(inference_eligible(&body));
        let bytes = build_request(&body, "grok-4.6", "c", false).unwrap();
        let fs = fields(&bytes);
        let msg = len_of(&fs, 1).expect("user 消息");
        let mf = fields(msg);
        let parts = len_of(&mf, 3).expect("ContentParts");
        let pf = fields(parts);
        let first_part = len_of(&pf, 1).expect("第一个 part(文档)");
        let ppf = fields(first_part);
        let text_part = len_of(&ppf, 1).expect("文档 part 是 text");
        let tf = fields(text_part);
        let text = match &tf[0].1 {
            PVal::Len(s) => String::from_utf8_lossy(s).into_owned(),
            _ => panic!("text part 字段1应为字符串"),
        };
        assert!(text.contains("/tmp/gw-cursor/doc-0.pdf"), "{text}");
        assert!(text.contains("无法抽取文本层"), "{text}");
    }
}

