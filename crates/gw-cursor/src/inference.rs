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
const MAX_ONE_IMAGE: usize = 12 * 1024 * 1024;
const MAX_ALL_IMAGES: usize = 24 * 1024 * 1024;

// InferenceMessageRole
const ROLE_USER: u64 = 1;
const ROLE_ASSISTANT: u64 = 2;
const ROLE_TOOL: u64 = 3;
const ROLE_SYSTEM: u64 = 4;

// ── 按账号 client(代理时 H1 专用,fail-closed)──────────────────────────────

fn inference_client(
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
                    // 嵌套 tool_result / 未知类型:编码器处理不了,回退
                    _ => false,
                }
            }),
            // 字符串/缺省 content 不涉及媒体;对象/数字等会被编码器丢,fail-closed
            None | Some(Json::Null) | Some(Json::String(_)) => true,
            _ => false,
        },
        // 编码器认识的非媒体块
        Some("text" | "tool_use" | "thinking" | "redacted_thinking") => true,
        // 未知块类型:fail-closed
        _ => false,
    }
}

fn push_text(out: &mut String, text: &str) {
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
pub(crate) fn tools_skip_inference(model: &str, body: &Json) -> bool {
    let has_tools = body
        .get("tools")
        .and_then(Json::as_array)
        .is_some_and(|t| !t.is_empty());
    has_tools && !model.to_ascii_lowercase().starts_with("composer")
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
fn document_inject_text(b: &Json, doc_n: &mut usize) -> Option<String> {
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

    // system → 首条 SYSTEM 消息(官方 converters.ts:175 同款:role=SYSTEM + text)。
    if let Some(sys) = body.get("system") {
        let text = match sys {
            Json::String(s) => s.clone(),
            Json::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            let mut m = Writer::new();
            m.uint(1, ROLE_SYSTEM);
            m.string(2, &text);
            out.message(1, &m);
        }
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

    // tools → AgentTool{name=1, description=2, parameters=3:Struct}
    if let Some(tools) = body.get("tools").and_then(Json::as_array) {
        for t in tools {
            let mut w = Writer::new();
            w.string(1, t.get("name").and_then(Json::as_str).unwrap_or(""));
            w.string(2, t.get("description").and_then(Json::as_str).unwrap_or(""));
            if let Some(schema) = t.get("input_schema").and_then(Json::as_object) {
                w.message(3, &struct_writer(schema));
            }
            out.message(2, &w);
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
struct PendingTool {
    id: String,
    name: String,
    args: String,
}

/// 流折叠状态机:InferenceStreamResponse 帧 → Anthropic SSE 事件序列。
///
/// 工具调用**不流式吐**:按 tool_call_id 归并缓冲,`is_complete` 到达时整块吐
///(content_block_start → 一帧全量 input_json_delta → content_block_stop),与
/// Run 路径(chat.rs:3175「input 一次性给全」)同形态,两条路径产出可比。
/// Anthropic 侧同一时刻只有一个打开块;并行调用靠缓冲天然串行化。
struct Folder {
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
}

impl Folder {
    fn new(
        model: &str,
        declared_tools: std::collections::HashSet<String>,
        show_thinking: bool,
        req_bytes: u64,
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
        }
    }

    fn push(&mut self, item: StreamItem) {
        self.pending.push(Ok(item));
    }

    /// 失败终态:发一次 Err,此后任何收尾都不再产正常事件。
    fn fail(&mut self, e: UpstreamError) {
        self.failed = true;
        self.pending.push(Err(e));
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

    fn on_text(&mut self, text: &str, is_final: bool) {
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
        } else {
            self.pending_tools.push_back(PendingTool {
                id: id.to_string(),
                name: name.to_string(),
                args: args_delta.to_string(),
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
    /// 拼不出合法参数就进失败终态(内容已流出,Err 比假数据诚实)。
    fn flush_tool(&mut self, id: &str, forced: bool) {
        let Some(pos) = self.pending_tools.iter().position(|t| t.id == id) else {
            return;
        };
        let tool = self.pending_tools.remove(pos).expect("position 刚查到");
        self.saw_content = true;
        // 未声明的工具:模型越界。**无条件**校验 —— 客户端没声明 tools 时任何
        // 工具调用都是异常,放行的后果是客户端收到不认识的 tool_use 直接卡死。
        if !self.declared_tools.contains(&tool.name) {
            tracing::warn!(tool = %tool.name, "inference: 模型调用了未声明的工具,降级为文本块");
            self.on_text(
                &format!("[未声明的工具调用 {}({})]", tool.name, tool.args),
                false,
            );
            return;
        }
        let args_valid = !tool.args.is_empty()
            && serde_json::from_str::<Json>(&tool.args)
                .map(|v| v.is_object())
                .unwrap_or(false);
        let args = if tool.args.is_empty() {
            "{}".to_string() // 无参工具的合法形态(真空调用)
        } else if args_valid {
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
        self.ensure_started();
        self.close_block();
        let idx = self.next_idx;
        self.next_idx += 1;
        self.sse(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"tool_use","id":tool.id,"name":tool.name,"input":{}}}),
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
                            (1, PVal::Len(s)) => id = String::from_utf8_lossy(s).into_owned(),
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
    fn finish(&mut self) {
        if self.finale_sent || self.failed {
            return;
        }
        self.finale_sent = true;
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

    fn take_pending(&mut self) -> Vec<Result<StreamItem, UpstreamError>> {
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
    UpstreamError::new(kind, format!("inference 上游错误[{code}]: {message}"))
}

/// 非 200 的 HTTP 错误分类(inference 专用,不套用 Run 路径的分类器 ——
/// 那套把结构化 permission_denied 映成 TokenInvalid,对本面太重)。
///
/// 保留的核心教训(chat.rs:2162):**401 才是号的问题**;403 无结构化错误体时
/// 是出口 IP 被拦(坏的是 IP 不是号),判 Other 不动账号健康。
fn classify_http_error(status: u16, body: &str) -> UpstreamError {
    if let Ok(j) = serde_json::from_str::<Json>(body) {
        if let Some(code) = j.get("code").and_then(Json::as_str) {
            let message = j
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("上游错误");
            return map_connect_error(code, message).with_status(status);
        }
    }
    let kind = match status {
        401 => UpstreamErrorKind::TokenInvalid,
        403 => {
            tracing::warn!(
                body_head = %body.chars().take(120).collect::<String>(),
                "inference 403 且无结构化错误体 —— 疑似出口 IP 被拦(不是号的问题),不动账号健康"
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
            "inference HTTP {status}: {}",
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
static PDF_EXTRACT_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// 请求里有没有 document 块(顶层或 tool_result 内嵌)。只在拿抽取并发槽前用。
fn has_document_block(body: &Json) -> bool {
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

    let mut folder = Folder::new(&req.model, declared_tools, show_thinking, req_len);
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
        Folder::new(model, declared(tools), true, 4000)
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
        let mut f = Folder::new("grok-4.6", declared(&[]), false, 4000);
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
    #[test]
    fn tools绕行门控() {
        use serde_json::json;
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
