//! Anthropic Messages → Cursor `agent.v1.AgentService/Run`,并把 ConnectRPC 响应流
//! 桥接成 Anthropic SSE。
//!
//! 端点、头表、请求体结构全部依据 `PROTOCOL-agent-run.md`(2026-08-06 对本机 3.14.27
//! 抓真包逐字节解码)。请求体的 protobuf 构造在 [`crate::run`]。
//!
//! ## 这个文件曾经错在哪
//!
//! 旧版打的是 `aiserver.v1.ChatService/StreamUnifiedChatWithTools`,并在注释里断言
//! 「存在一道只作用于推理路径的客户端完整性门」。**那道门不存在**:真 IDE 的推理
//! 根本不调那个服务,服务端对还在打退役端点的请求回「请升级」,字面为真。
//! 那段注释连同 `CURSOR_METHOD` 环境变量开关、`wrap` 分支、`slow_pool` 参数
//! 一并删除 —— 它们都是那个错误判断的产物。

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{ChatRequest, ChatStream, ChatUsage, SseEvent, StreamItem};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::run::{self, Model, RunShape, Turn};
use crate::wire;

/// 上游连续多久「只有心跳、零进展」就判定本轮废了。
///
/// 心跳是 10 秒一个,取 90 秒 = 容忍 9 个心跳。首 token 前的正常等待实测在 2 秒内,
/// 长思考的模型可能更久,但那期间会持续来 `1.4` 思考帧(算进展),不会触发。
/// 这个值宁可偏大:误判的代价是把一次本来会成功的慢请求变成失败。
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 从发出请求到收到**响应头**的上限。
///
/// 实测正常情况是 300–800ms(见日志 `响应头已到`)。取 60s 是给代理链路留足余量,
/// 同时保证「上游收了连接但永不回话」的情形有个尽头 —— 它不在
/// [`STALL_TIMEOUT`] 的管辖范围内(那个 watchdog 在流循环里,进不去)。
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 一次 Run 调用需要的、与账号绑定的全部参数。
///
/// 打成一个结构体而不是长参数列表:这些值**必须整组随号冻结**(PROTOCOL §7),
/// 散成 8 个参数很容易在调用点漏传或串号。
#[derive(Debug, Clone)]
pub struct RunCtx {
    pub host: String,
    pub token: String,
    pub machine_id: String,
    pub mac_machine_id: Option<String>,
    pub config_version: String,
    pub timezone: String,
    pub conversation_id: String,
    /// 本次请求处在会话的哪个阶段。由 [`crate::ConvRegistry`] 判定:
    /// 服务端已有这个会话(且还是同一个号)→ [`Phase::Continuation`],否则降级回
    /// [`Phase::Opening`] 把历史整个重铺。
    pub phase: run::Phase,
    /// 帧0 发哪些分节。默认全发(完整模拟客户端);出问题时二分用。
    pub shape: RunShape,
    /// 是否补发 `field 3` 上下文帧(真客户端初始发 3 帧:帧0 + 两个 field 3)。
    pub context_frames: bool,
    /// 发完初始帧后是否**保持请求流不关闭**。
    ///
    /// 真客户端是 BiDi:发完 3 帧继续发 keepalive/增量,不 half-close。
    /// `false` 则一次性发完 body 就关流(HTTP 语义上是 half-close)。
    /// 两种行为服务端可能区别对待,做成开关以便实测。
    pub keep_stream_open: bool,
}

/// `message_start` 事件。抽出来是因为三条路径都要发它(思考先到 / 正文先到 /
/// 纯工具调用),而漏发会让后面的块没有依附。
fn message_start(msg_id: &str, model: &str) -> SseEvent {
    SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": msg_id, "type": "message", "role": "assistant",
                "model": model, "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    )
}

/// 从 Anthropic content(string 或 block 数组)抽出纯文本。
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                // 工具块渲染成文本。**这就是工具回路的闭合方式**:
                // 我们把历史折成一条消息(见 fold_history),所以调用方回传的
                // tool_use / tool_result 也只能以文字形态进上下文。上游那条
                // 「请求侧 field 2 帧」的中途回传通道我们不用 —— 那要求把流一直
                // 挂着等调用方执行完,而反代是一问一答的。
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let rendered = match kind {
                    "text" => b.get("text").and_then(|t| t.as_str()).map(str::to_string),
                    "tool_use" => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        Some(format!("[调用工具 {name},参数 {input}]"))
                    }
                    "tool_result" => {
                        // content 可能是字符串,也可能是块数组(递归取文本)。
                        let c = b.get("content").cloned().unwrap_or(Value::Null);
                        let body = match &c {
                            Value::String(s) => s.clone(),
                            other => extract_text(other),
                        };
                        let err = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        Some(format!(
                            "[工具{}返回]\n{body}",
                            if err { "出错" } else { "" }
                        ))
                    }
                    // image / document 不进文本 —— 它们走 `to_media` 内联成真附件
                    // (图片 → 1.2.1.1.3,文档 → 1.2.1.2.20),渲染成文字反而会丢内容。
                    _ => None,
                };
                if let Some(t) = rendered {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn extract_system(body: &Value) -> String {
    body.get("system").map(extract_text).unwrap_or_default()
}

/// 拦住模型去调 Cursor 的**内建**工具。
///
/// ## 为什么需要这道护栏
///
/// Cursor 的内建工具(终端、读写文件、网页搜索)是**服务端自带**的:哪怕我们一个工具都
/// 不声明,模型照样会调。而它们的结果要由客户端在真实机器上执行后回传 —— 反代做不到,
/// 也**不该做**(那等于跑模型选定的任意 shell 命令)。
///
/// 不拦的实际后果(2026-08-07 实测):问一句「帮我查今天的新闻」,模型先输出一句
/// 「先确认今天日期,再查最新新闻」,然后去调 `date '+%Y-%m-%d'`,我方只能收口 ——
/// 用户看到的是一句没头没尾的计划,像卡死了。
///
/// 系统提示能改行为是实证过的(往 `1.2.1.2.25` 塞「只准回答 PINEAPPLE」,模型照做)。
fn builtin_tool_guard(has_tools: bool) -> &'static str {
    if has_tools {
        "\n\n[运行环境约束]你运行在一个没有本地执行能力的环境里。\
         只能使用本次请求中显式声明的工具(名字以 `gwtools-` 开头)。\
         **不要**调用任何内建工具:终端/命令行、读写文件、网页搜索、代码库检索。\
         需要它们才能完成时,直接说明你需要什么,不要尝试调用。"
    } else {
        "\n\n[运行环境约束]你运行在一个没有任何工具的环境里。\
         **不要**尝试调用终端/命令行、读写文件、网页搜索或代码库检索 —— 调用不会有结果。\
         直接基于已有信息回答;信息不足时说明缺什么,并让用户提供。\
         (特别地:你无法得知当前日期,也无法访问网络。)"
    }
}

/// Anthropic 请求体 → Run 的会话轮次列表。
///
/// ⚠️ **system 不在这里。** 它的家是 `1.2.17.9.25`(抓包实物证实),由
/// [`extract_system`] 单独取出后交给 [`run::build_frame0`]。早先的实现把 system
/// 折进第一条用户消息 —— 那会让模型把系统指令当成用户说的话。
pub fn to_turns(body: &Value) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = extract_text(m.get("content").unwrap_or(&Value::Null));
            if text.is_empty() {
                // 走到这里 = 这一轮只有非文本块。整条丢掉会让 user/assistant 交替错位,
                // 所以留一个占位。**区分两种情况**:媒体块是真的发出去了(见 to_media),
                // 说"不支持"会误导模型去问用户重发;其它未知块才是真不支持。
                let has_media = m
                    .get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|bs| {
                        bs.iter().any(|b| {
                            matches!(
                                b.get("type").and_then(|t| t.as_str()),
                                Some("image") | Some("document")
                            )
                        })
                    });
                turns.push(Turn {
                    text: if has_media { "(见附件)" } else { "(unsupported content omitted)" }
                        .to_string(),
                    is_user: role != "assistant",
                });
                continue;
            }
            turns.push(Turn {
                text,
                is_user: role != "assistant",
            });
        }
    }
    turns
}

/// 单个附件的原始字节上限。
///
/// 无上限时的失效链(两位审查员各自独立指出):50MB 的 PDF → base64 解码 50MB →
/// `from_utf8_lossy` 最坏膨胀到 ~150MB 的 `String` → 进 protobuf frame0 → gzip 缓冲,
/// 原始字节 / lossy 串 / protobuf 副本 / gzip 输出**同时存活**,峰值是附件的数倍。
/// 而这些工作全在 `chat_stream` 首个 await 之前同步做完,会占住 tokio 线程,
/// 让同 worker 上**别的账号**的请求一起饥饿。gw-kiro 早有出站体积硬顶,这里补齐。
const MAX_ONE_ATTACHMENT: usize = 12 * 1024 * 1024;

/// 一次请求内所有附件的原始字节合计上限。
///
/// 单个上限挡不住"20 张 10MB 图片"。而多轮会话更狠:`to_media` 收的是**有史以来
/// 全部**消息里的附件,每轮全量重新内联,请求体随会话长度线性膨胀。
const MAX_ALL_ATTACHMENTS: usize = 24 * 1024 * 1024;

/// 从 Anthropic 请求里收集图片与文档附件。
///
/// Anthropic 侧:
/// - 图片 `{type:"image", source:{type:"base64", media_type, data}}`
/// - 文档 `{type:"document", source:{type:"base64", media_type:"application/pdf", data}}`
///
/// 两者去处不同(见 [`run::Media`])。`source.type != "base64"`(如 `url`)一律跳过 ——
/// 替调用方去下载 URL 是我方主动出网,不该悄悄做。
fn to_media(body: &Value) -> (Vec<run::ImageAttachment>, Vec<run::DocAttachment>) {
    use base64::Engine as _;
    let mut images = Vec::new();
    let mut docs = Vec::new();
    // 已收下的附件原始字节总量,用来卡总闸(见 MAX_ONE_ATTACHMENT / MAX_ALL_ATTACHMENTS)。
    let mut budget: usize = 0;
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return (images, docs);
    };
    for m in msgs {
        let Some(blocks) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind != "image" && kind != "document" {
                continue;
            }
            let src = b.get("source");
            let stype = src.and_then(|s| s.get("type")).and_then(|t| t.as_str());
            if stype != Some("base64") {
                tracing::warn!(kind, ?stype, "cursor: 非 base64 的媒体源,跳过(不代为下载)");
                continue;
            }
            let mime = src
                .and_then(|s| s.get("media_type"))
                .and_then(|t| t.as_str())
                .unwrap_or(if kind == "image" { "image/png" } else { "application/pdf" })
                .to_string();
            let Some(data) = src.and_then(|s| s.get("data")).and_then(|d| d.as_str()) else {
                continue;
            };
            // 解码**之前**先按 base64 长度估原始大小,别先解出 200MB 再判超限。
            // base64 是 4:3,估值只用于挡明显超标的,精确判断在解码后。
            if data.len() / 4 * 3 > MAX_ONE_ATTACHMENT {
                tracing::warn!(kind, b64_len = data.len(), "cursor: 附件超单个上限,跳过");
                continue;
            }
            let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(data) else {
                tracing::warn!(kind, "cursor: base64 解码失败,跳过该附件");
                continue;
            };
            if raw.len() > MAX_ONE_ATTACHMENT || budget + raw.len() > MAX_ALL_ATTACHMENTS {
                tracing::warn!(
                    kind, bytes = raw.len(), budget,
                    "cursor: 附件超上限(单个或累计),跳过 —— 无上限时一个大附件能打爆整个 worker"
                );
                continue;
            }
            budget += raw.len();
            if kind == "image" {
                let (width, height) = run::image_dims(&raw);
                images.push(run::ImageAttachment { mime, bytes: raw, width, height });
            } else {
                // 文档字段在上游是 proto3 `string`,真客户端把 PDF 当 UTF-8 读、
                // 二进制部分有损替换成 U+FFFD。我方逐字节同构地照做。
                let n = docs.len();
                // 我方**自己抽文本层**。上游协议不原生支持 PDF:真客户端是让模型
                // 调终端工具跑 `pdftotext`,由客户端在真实磁盘上执行 —— 反代不能
                // 答应内建终端工具调用(那等于执行模型选定的任意 shell 命令)。
                let extracted = crate::pdf::extract_text(&raw);
                docs.push(run::DocAttachment {
                    path: format!("/tmp/gw-cursor/doc-{n}.pdf"),
                    // `.20` 登记表仍照发原文,与真客户端同形。
                    text: String::from_utf8_lossy(&raw).into_owned(),
                    extracted,
                });
            }
        }
    }
    (images, docs)
}

/// Anthropic 请求的 `tools` → Cursor 的工具声明。
///
/// Anthropic 侧是 `{name, description, input_schema}`;Cursor 侧要 5 个字段
/// (见 [`run::ToolDef`]),其中命名空间固定成 [`run::TOOL_NS`] —— 它同时是**回调时
/// 认领工具的依据**:模型调 `gwtools-<name>` 回来才是调用方的工具,
/// 否则是 Cursor 服务端自带的内建工具(我们执行不了)。
///
/// `input_schema` 缺失时给一个空 object schema:那一位在真包里 16/16 全都有值,
/// 留空很可能被拒,而"无参数工具"用空 schema 表达是标准做法。
fn to_tools(body: &Value) -> Vec<run::ToolDef> {
    let Some(arr) = body.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let schema = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(run::ToolDef {
                name,
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                schema: schema.to_string(),
            })
        })
        .collect()
}

/// 把多轮历史折成**一条**用户消息。
///
/// ## 为什么必须折
///
/// 实测:请求里只要有多于一条 `1.2.1`,上游就 200 接受、然后永远只发心跳
/// (与缺 `1.2.1.2` 时一模一样的静默挂起)。真客户端**从来只发一条** ——
/// 它的历史在服务端。所以「repeated `1.2.1` 携带历史」这条路不存在。
///
/// Anthropic 客户端每次都重传全量历史,我们只能把它渲染成一段文字塞进当前这轮。
/// 代价是上游看到的是「一条很长的用户消息」而不是结构化对话,但**模型确实读得到**,
/// 这比静默挂起好得多。
///
/// ## `task` 参数:折叠为什么会让工具回路发散
///
/// 2026-08-07 实测事故:grok 在 opencode 里对同一个文件连调 **9 次** `read`。
/// 原因不在工具解析(参数解得出来、结果也确实进了上下文),而在折叠的形状 ——
/// 工具回路的第二轮,Anthropic 请求的最后一条 user 消息**只有 tool_result**,
/// 于是折完之后本轮的用户消息是一段裸的 `[工具返回]…`,**一个问题都没有**,
/// 原始请求被埋进 `<conversation_history>` 里。模型看到「工具返回了但没人问我什么」,
/// 最合理的动作就是再调一次工具。每轮请求体只涨 1.2KB(多攒的那一对调用/返回),
/// 看起来像死循环。
///
/// 所以本轮消息只有工具返回时,`task` 传入**用户最近一次真实提问**,在工具返回后面
/// 补一句「据此继续,别重复调用」。系统提示能改行为是实证过的(见
/// [`builtin_tool_guard`]),这里用同一个杠杆。
fn fold_history(turns: &[Turn], task: Option<&str>) -> Vec<Turn> {
    if turns.len() <= 1 && task.is_none() {
        return turns.to_vec();
    }
    let last = turns.iter().rposition(|t| t.is_user).unwrap_or(turns.len() - 1);
    let mut buf = String::new();
    if last > 0 {
        buf.push_str("<conversation_history>\n");
        for t in &turns[..last] {
            buf.push_str(if t.is_user { "User: " } else { "Assistant: " });
            buf.push_str(&t.text);
            buf.push_str("\n\n");
        }
        buf.push_str("</conversation_history>\n\n");
    }
    buf.push_str(&turns[last].text);
    // 末尾若还有 assistant(prefill),原样附在后面 —— 丢掉会改变语义。
    for t in &turns[last + 1..] {
        buf.push_str("\n\n");
        buf.push_str(&t.text);
    }
    if let Some(task) = task {
        buf.push_str(
            "\n\n[继续]上面是你上一步请求的工具结果,已经拿到了。\
             请**据此**继续完成用户的请求,不要重复调用已经返回结果的工具。\
             还缺信息时才调别的工具;信息够了就直接给出最终回答。\n用户的请求是:",
        );
        buf.push_str(task);
    }
    vec![Turn { text: buf, is_user: true }]
}

/// 最后一条 user 消息是否**只装 tool_result**(工具回路的中间轮)。
///
/// 判据是块类型而不是折叠后的文本:嗅自己拼出来的 `[工具返回]` 前缀能work,
/// 但那是把渲染格式变成了协议,改一个字就静默失效。
fn last_user_is_tool_result_only(body: &Value) -> bool {
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let Some(last) = msgs
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) != Some("assistant"))
    else {
        return false;
    };
    let Some(blocks) = last.get("content").and_then(|c| c.as_array()) else {
        return false;
    };
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

/// 用户**最近一次真实提问**(跳过只装 tool_result 的中间轮)。
///
/// 取最近而不是最初:多轮对话里用户会改主题,拿第一条会把模型带回上个话题。
fn latest_real_user_request(body: &Value) -> Option<String> {
    let msgs = body.get("messages").and_then(|m| m.as_array())?;
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            continue;
        }
        let content = m.get("content")?;
        // 只装 tool_result 的轮次不是提问,跳过。
        if let Some(blocks) = content.as_array() {
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            {
                continue;
            }
        }
        let text = extract_text(content);
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// 排查用的落盘开关如果开着,吵一声。
///
/// `CURSOR_DUMP_REQ` / `CURSOR_DUMP_TOOL_FRAMES` 会把**含用户对话全文**(system prompt、
/// 客户代码)的帧明文写进磁盘,每请求一个文件、永不清理。生产机上有人为排查设了忘了摘,
/// 就是隐私与磁盘双输。provider 构造时报一次,让它在启动日志里显眼。
pub(crate) fn warn_if_dump_enabled() {
    for var in ["CURSOR_DUMP_REQ", "CURSOR_DUMP_TOOL_FRAMES"] {
        if let Ok(dir) = std::env::var(var) {
            tracing::warn!(
                %var, %dir,
                "⚠️ gw-cursor 落盘开关已开启:会把含用户对话全文的原始帧明文写进磁盘,\
                 且不清理。仅供排查,生产务必摘掉"
            );
        }
    }
}

/// 从请求体派生**会话亲和键**(`Provider::affinity_key` 用)。
///
/// ## 为什么必须有
///
/// 不覆盖 `affinity_key` 的代价不是"少一层优化",而是 `CallCtx.session_id` 与
/// `cache_key` 双双变成**空串**(worker 那边是 `affinity_key.unwrap_or_default()`),
/// 于是 `conversation_id` 恒为 `""`:上游 `1.5` 发零长度字符串(真客户端是 UUID)、
/// `x-blob-encryption-key`/`x-fs-client-key` 退化成每账号一个常量(真客户端每会话一把)、
/// [`crate::ConvRegistry`] 全部会话挤在同一个键上。前两条是稳定的可区分指纹,
/// 第三条在 `CURSOR_STATEFUL=1` 下是跨用户串话。
///
/// ## 为什么锚在**第一条**用户消息
///
/// 键必须在同一会话的多轮之间保持稳定,而 Anthropic 客户端每轮都重传全量历史 ——
/// 只有"第一条用户消息"这个锚点不随轮次变化。与 gw-kiro 的
/// `converter::affinity_key_from_body` 同一思路(那边也是首条锚定),
/// 但本 crate 不依赖 gw-kiro,故自带一份。
pub fn affinity_key_from_body(body: &Value) -> Option<String> {
    // 显式会话标识优先:客户端给了就用它,比内容哈希准。
    if let Some(uid) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
    {
        if let Some(sid) = gw_core::routing::extract_session_from_metadata(uid) {
            return Some(sid);
        }
    }
    let msgs = body.get("messages").and_then(|m| m.as_array())?;
    let first_user = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) != Some("assistant"))?;
    let anchor = extract_text(first_user.get("content")?);
    if anchor.trim().is_empty() {
        return None;
    }
    // system 一并入哈希:同一句"hi"在不同 agent(不同 system)下是不同会话。
    Some(gw_core::routing::short_hash(&[
        &extract_system(body),
        &anchor,
    ]))
}

/// 把任意亲和键材料变成一个**非空的 UUID** 形态 conversation_id。
///
/// 两件事一起办:
/// - 保证非空。空 `1.5` 是真客户端不会发的值。
/// - 保证是 UUID。worker 传下来的键可能带调度用的分组前缀(`"<组>\0<键>"`),
///   那是 caio 内部的命名空间,不该原样出现在上游报文里。UUIDv5 把它吃掉。
pub fn conversation_uuid(material: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, material.as_bytes()).to_string()
}

/// 每会话密钥:`sha256hex(token || conversation_id || 用途)`。
///
/// 真客户端用 `crypto.getRandomValues(32)`,每 conversation 一把。我们改成按会话
/// 派生而不是每请求现随机,理由:
/// - 服务端**无法区分**一个随机 64-hex 与一个派生 64-hex,所以指纹上等价;
/// - 但反代是无状态的,现随机会让同一会话的多次请求带**不同**的 key,
///   那反而与真客户端「一会话一把」的行为可区分。
///
/// ⚠️ 仅在无文件附件(L1)时成立。真要实现 L2 blob 加密时,这把 key 会被用来
/// 实际加密上传内容,那时必须换成全熵随机并跟随会话状态存起来。
fn session_key(token: &str, conversation_id: &str, purpose: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update(b"\x00");
    h.update(conversation_id.as_bytes());
    h.update(b"\x00");
    h.update(purpose.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 按 §2 铺满 Run 的请求头(30 条)。
///
/// 顺序与分组照 §2 的四类来源排,方便和文档逐行对照。
fn apply_headers(mut rb: reqwest::RequestBuilder, ctx: &RunCtx) -> reqwest::RequestBuilder {
    let request_id = uuid::Uuid::new_v4().to_string();

    // §2.1 静态常量
    rb = rb
        .header("connect-protocol-version", "1")
        .header("connect-accept-encoding", "gzip")
        .header("connect-content-encoding", "gzip")
        .header("content-type", "application/connect+proto")
        .header("user-agent", wire::USER_AGENT)
        .header("x-cursor-client-device-type", "desktop")
        .header("x-cursor-remote-type", "none")
        .header("x-cursor-retryinterceptor-enabled", "true")
        .header("x-cursor-streaming", "true")
        // ⚠️ 这两条旧代码发反了。真 IDE 是 ghost-mode=true / onboarding-completed=false。
        .header("x-ghost-mode", "true")
        .header("x-new-onboarding-completed", "false");

    // §2.2 客户端/平台标识
    // ⚠️ 真请求**不发** x-cursor-client-commit 与 x-cursor-client-os-version。
    rb = rb
        .header("x-cursor-client-type", wire::CLIENT_TYPE)
        .header("x-cursor-client-layout", wire::CLIENT_LAYOUT)
        .header("x-cursor-client-version", wire::CLIENT_VERSION)
        .header("x-cursor-client-os", wire::CLIENT_OS)
        .header("x-cursor-client-arch", wire::CLIENT_ARCH)
        .header("x-cursor-timezone", &ctx.timezone);

    // §2.3 由 token 派生
    rb = rb
        .header("authorization", format!("Bearer {}", ctx.token))
        .header("x-client-key", wire::client_key(&ctx.token))
        .header("x-session-id", wire::session_id(&ctx.token))
        .header(
            "x-cursor-checksum",
            wire::checksum(&ctx.machine_id, ctx.mac_machine_id.as_deref()),
        );

    // §2.4 握手下发
    rb = rb.header("x-cursor-config-version", &ctx.config_version);

    // §2.5 每会话
    rb = rb
        .header(
            "x-blob-encryption-key",
            session_key(&ctx.token, &ctx.conversation_id, "blob"),
        )
        .header(
            "x-fs-client-key",
            session_key(&ctx.token, &ctx.conversation_id, "fs"),
        );

    // §2.6 每请求 trace
    rb.header("x-request-id", &request_id)
        .header("x-original-request-id", &request_id)
        .header("x-amzn-trace-id", format!("Root={request_id}"))
        // ⚠️ 两条的 trace-flags 不同:抓包实物 traceparent 是 `-00`(未采样),
        // backend-traceparent 是 `-01`(已采样)。两条都写 -01 是可区分特征。
        .header("traceparent", wire::traceparent_unsampled())
        .header("backend-traceparent", wire::traceparent())
        // 真请求发一个**空** cookie 头。不发这个头本身就是差异。
        .header("cookie", "")
}

/// 发起一次 Cursor Run,返回 Anthropic SSE 事件流。
///
/// `client` 必须能协商 HTTP/2(Cursor 的流式端点强制 h2,降级到 h1 会被 ALB 回 464)。
/// 本轮终态回调:`true` = 正常收尾(服务端已持有这一轮),`false` = 失败。
///
/// 用回调而不是把 `ConvRegistry` 传进来:流循环不该知道会话注册表长什么样,
/// 它只知道「这轮成没成」。也让测试能直接观察终态判定。
pub type OutcomeHook = std::sync::Arc<dyn Fn(bool) + Send + Sync>;

pub async fn chat_stream(
    client: reqwest::Client,
    ctx: RunCtx,
    req: ChatRequest,
    outcome: Option<OutcomeHook>,
) -> Result<ChatStream, UpstreamError> {
    // 调用方是否请求了 thinking。两处都要用:
    // ① 决定发给上游的模型参数(见 apply_thinking_pref);
    // ② 决定收到 `1.4` 帧后是否转成 Anthropic thinking 块 —— **没请求就不发**,
    //    客户端要么报未知块类型、要么把推理当正文显示,两种都比不发坏。
    let thinking = req.body.get("thinking").cloned();
    let want_thinking = thinking
        .as_ref()
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        == Some("enabled");

    let cursor_model = crate::models::to_cursor_model(&req.model);
    // ⚠️ **替换必须留痕。** 我们按族把客户要的模型归一到 Cursor 侧的名字
    // (`claude-haiku-4-5` → `composer-2.5`、未知名 → `default`),而 SSE 回显的
    // `message_start.model` 是**客户请求的那个名字**(协议要求如此,客户端按它对账)。
    // 结果是「执行的模型」在任何对外面上都看不见 —— 客户点 haiku 实际跑 composer,
    // 而账单按 haiku 计价。至少让它在日志里可查。
    if cursor_model != req.model {
        tracing::info!(
            requested = %req.model, upstream = %cursor_model,
            "cursor 模型归一:实际执行的与客户请求的不是同一个(SSE 回显仍是客户请求的名字)"
        );
    }
    let mut model: Model = crate::models::model_by_name(&cursor_model);
    // 把客户端的 thinking 意图**真的发上去**。
    //
    // 早先 `want_thinking` 只是收侧过滤器:客户端开不开 thinking,我方发给上游的请求
    // 一模一样(模型参数恒为目录里那份 `thinking=true`)。两个后果:
    // - 客户明确不要推理时,上游照样按推理档跑 —— 白烧上游额度、也慢。
    // - 客户要推理时我方没有任何可拉的杠杆,「上游到底发不发 `1.4`」完全查不动。
    //
    // ⚠️ 只在**目录本来就声明了该参数**的模型上覆盖(claude 系)。给
    // `composer-2.5` / `gpt-5.6-*` 塞一个它们没有的参数是在猜,而猜错的代价是
    // 整个请求被拒(`invalid_argument`)。
    crate::models::apply_thinking_pref(&mut model, thinking.as_ref());
    let catalog = crate::models::catalog();
    // 工具回路的中间轮:本轮 user 消息只有 tool_result,折完就没有问题了 ——
    // 必须把用户的请求复述回本轮,否则模型会重复调同一个工具(见 fold_history)。
    let task = last_user_is_tool_result_only(&req.body)
        .then(|| latest_real_user_request(&req.body))
        .flatten();
    let turns = fold_history(&to_turns(&req.body), task.as_deref());
    let tools = to_tools(&req.body);
    let system = {
        let mut sys = extract_system(&req.body);
        sys.push_str(builtin_tool_guard(!tools.is_empty()));
        sys
    };

    let (images, docs) = to_media(&req.body);
    // 文档路径要**写进用户文本**。真客户端就是这么干的(实测:文本 =
    // "<路径> <问题>"),因为 `1.2.1.2.20` 只是按路径索引的内容登记表 ——
    // prompt 里不提路径,模型根本不知道有附件,只会说"我来找这个文件"。
    let turns = if docs.is_empty() {
        turns
    } else {
        let mut t = turns;
        if let Some(i) = t.iter().rposition(|x| x.is_user) {
            let mut pre = String::new();
            for d in &docs {
                match &d.extracted {
                    Some(txt) => {
                        // 抽到了就直接给模型看,别让它去调工具读文件 —— 那条路我们答不了。
                        pre.push_str(&format!(
                            "<document path=\"{}\">\n{}\n</document>\n\n",
                            d.path, txt
                        ));
                    }
                    None => {
                        // 抽不到(扫描件 / 非 Flate 压缩 / 图片型)。**明确告诉模型**,
                        // 否则它会反复尝试读文件,而我们每次都只能收口。
                        pre.push_str(&format!(
                            "<document path=\"{}\" note=\"无法抽取文本层(可能是扫描件或图片型 PDF);\
                             请直接告知用户无法读取,不要尝试调用工具读文件\"/>\n\n",
                            d.path
                        ));
                    }
                }
            }
            t[i].text = format!("{pre}{}", t[i].text);
        }
        t
    };
    let media = run::Media { images: &images, docs: &docs };
    if turns.is_empty() {
        return Err(UpstreamError::bad_request("cursor: 请求里没有任何消息"));
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let frame0 = run::build_frame0(
        &turns,
        &system,
        &tools,
        media,
        &model,
        &catalog,
        &ctx.conversation_id,
        &ctx.timezone,
        now_ms,
        ctx.shape,
        ctx.phase,
    );
    // 逆向期用:把帧0 的 payload(未 gzip)原样落盘,好与真客户端逐字节对比。
    if let Ok(dir) = std::env::var("CURSOR_DUMP_REQ") {
        let f = format!("{dir}/frame0_{}.bin", uuid::Uuid::new_v4().simple());
        let _ = std::fs::write(&f, &frame0);
        tracing::warn!(file = %f, bytes = frame0.len(), "已落盘帧0 payload");
    }
    tracing::debug!(
        phase = ?ctx.phase,
        turns = turns.len(),
        frame0_bytes = frame0.len(),
        "cursor Run 请求已构造"
    );

    // 帧0 走 gzip(6KB 级,压缩有意义);开场四帧是 2-4 字节的裸帧,
    // 抓包实物就是 flag=0x00 不压缩 —— 压几字节只会更大。
    let mut frames = vec![wire::frame_compressed(&frame0).map_err(|e| {
        UpstreamError::new(UpstreamErrorKind::Other, format!("gzip 请求帧失败: {e}"))
    })?];
    if ctx.context_frames {
        frames.extend(run::build_prelude_frames().iter().map(|p| wire::frame(p)));
    }

    let url = format!("https://{}/agent.v1.AgentService/Run", ctx.host);
    let rb = apply_headers(client.post(&url), &ctx);

    // 保持请求流打开时,body 走一个 channel:初始帧先灌进去,发送端在响应读完前
    // 一直不 drop,于是 HTTP/2 的请求流不会 half-close —— 与真 BiDi 客户端一致。
    let (rb, body_keepalive) = if ctx.keep_stream_open {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        for f in frames {
            // 容量 8 > 帧数,这里不会阻塞。
            let _ = tx.try_send(Ok(bytes::Bytes::from(f)));
        }
        let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
        (rb.body(body), Some(tx))
    } else {
        (rb.body(frames.concat()), None)
    };

    // ⚠️ **只给「等响应头」这一段设超时,不给整个请求设。**
    //
    // 账号配了 proxy 时用的是 `build_proxy_client`,那个 client **故意不设总超时** ——
    // Run 是流式的,总超时会把长回复掐断。代价是:TCP/TLS 都成功、h2 流也开了,
    // 但上游(或中间代理)永远不发响应头时,`send()` 会一直等下去。而 `STALL_TIMEOUT`
    // 的 watchdog 在流循环里,**拿不到响应根本进不去**。此时 scheduler 的并发租约被
    // 这个请求永久占住,几个这样的请求就能把号的并发槽耗光,表现是「号没坏但永远 busy」。
    //
    // 响应头到了就交给现有 watchdog 管,所以这里只需要一个握手期的上限。
    let resp = tokio::time::timeout(HEADER_TIMEOUT, rb.send())
        .await
        .map_err(|_| {
            UpstreamError::network(format!(
                "Cursor Run {}s 内没有返回响应头(连接已建立),放弃",
                HEADER_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| UpstreamError::network(format!("Cursor Run 请求失败: {e}")))?;

    // 降级到 HTTP/1.1 的表现是 ALB 回 464,错误体没有任何线索。先在这里点名,
    // 免得下次又去翻头表。
    if resp.version() != reqwest::Version::HTTP_2 {
        tracing::warn!(
            version = ?resp.version(),
            "Cursor Run 未走 HTTP/2(ALPN 协商失败或代理降级),上游大概率回 464"
        );
    }

    let status = resp.status();
    tracing::debug!(status = status.as_u16(), version = ?resp.version(), "cursor Run 响应头已到");
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_http_error(status.as_u16(), &text));
    }

    let model_name = req.model.clone();
    Ok(Box::pin(stream_to_anthropic(
        resp.bytes_stream(),
        model_name,
        want_thinking,
        body_keepalive,
        outcome,
    )))
}

/// 请求流的「不要关」把手。持有它 = 请求体的 channel 发送端还活着 =
/// HTTP/2 请求流不 half-close。读完响应后随任务一起 drop,流才收。
type BodyKeepalive = Option<tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>>;

/// HTTP 状态码 → UpstreamError(仅用于**非** 2xx;流内错误走 trailer 分类)。
fn classify_http_error(status: u16, body: &str) -> UpstreamError {
    // 非 2xx 时 Connect 也可能把结构化错误放在 body 里,先试着解出来。
    if let Some(t) = run::parse_trailer(body) {
        return trailer_to_error(&t).with_status(status);
    }
    let kind = match status {
        // ⚠️ 401 才是「这个 token 不行」。**403 不一定**:出口 IP 被 Cloudflare/ALB
        // 拦下时回的也是 403,但那时坏的是 **IP 不是号** —— 判 TokenInvalid 会让
        // gw-app 同号刷新(刷新走同出口,照样 403)然后把这个健康号禁掉。
        // Cursor 这种按 IP 信誉风控的服务,这个形态不罕见。
        //
        // 分辨法:真的凭据问题会给**结构化 Connect 错误体**
        // (`unauthenticated`/`permission_denied`,已在函数开头的 `parse_trailer` 里
        // 走掉了)。走到这里的 403 说明 body 不是 Connect 结构 —— 大概率是一张
        // HTML 错误页。判 `Other`(保守换号一次,不动账号健康)。
        401 => UpstreamErrorKind::TokenInvalid,
        403 => {
            tracing::warn!(
                body_head = %body.chars().take(120).collect::<String>(),
                "cursor 403 且无 Connect 结构化错误体 —— 疑似出口 IP 被拦(不是号的问题),不动账号健康"
            );
            UpstreamErrorKind::Other
        }
        429 => UpstreamErrorKind::RateLimited,
        400 | 464 => UpstreamErrorKind::BadRequest,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    UpstreamError::new(
        kind,
        format!(
            "Cursor 上游 {status}: {}",
            body.chars().take(300).collect::<String>()
        ),
    )
    .with_status(status)
}

/// end-stream trailer 里的错误 → UpstreamError。
///
/// 关键分类:`ERROR_RATE_LIMITED_CHANGEABLE` 是**该号在该模型上**的计费额度耗尽
/// (服务端还会给 `autoSwitchToModel`),不是账号健康问题 —— Cursor 自家模型
/// (composer/default)和 grok 同时仍然可用。所以映射到 `ModelNotAvailable`:
/// 不惩罚账号、换到有额度的号重试、调度层记住 `(账号,模型)` 不可用。
///
/// 若错判成 `RateLimited`,整个号会被冷却,连不受限的模型一起废掉。
fn trailer_to_error(e: &run::TrailerError) -> UpstreamError {
    let kind = if e.debug_error == "ERROR_RATE_LIMITED_CHANGEABLE"
        || !e.auto_switch_to_model.is_empty()
    {
        UpstreamErrorKind::ModelNotAvailable
    } else if e.debug_error.contains("BAD_MODEL_NAME") {
        UpstreamErrorKind::BadRequest
    } else if is_client_integrity_block(e) {
        // ⚠️ **这一条必须排在下面 `resource_exhausted` 之前。**
        //
        // 客户端完整性门(config_version 空/过期、客户端版本过旧)回的**也是**
        // `resource_exhausted`,文案是 "Update Required"(见 `config.rs` 模块文档)。
        // 落到下面那个 match 就是 `QuotaExhausted` —— 调度层对它的处置是
        // **持久禁用、不自愈**。于是一次控制面抖动会杀掉一个额度充足的健康号。
        //
        // 判成 `ServerError`:可换号重试(别的号有自己的 config_version),
        // 计一次可自愈的失败,**不进 quota 禁用**。
        UpstreamErrorKind::ServerError
    } else {
        match e.code.as_str() {
            "unauthenticated" | "permission_denied" => UpstreamErrorKind::TokenInvalid,
            "resource_exhausted" => UpstreamErrorKind::QuotaExhausted,
            "invalid_argument" => UpstreamErrorKind::BadRequest,
            "unavailable" => UpstreamErrorKind::Overloaded,
            "deadline_exceeded" => UpstreamErrorKind::Network,
            "internal" | "unknown" | "data_loss" => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        }
    };
    UpstreamError::new(kind, format!("Cursor Run: {}", e.summary()))
}

/// 上游的 call id 变成能给客户端的 `tool_use.id`。
///
/// Cursor 的 call id 是**两段用换行连起来**的:`call-<uuid>-N\nfc_<uuid>_N`
/// (见 `run::ToolCall::id`)。原样交出去有两个后果:客户端若校验 id 形态会拒收;
/// 日志里这一行会被劈成两半(实测 `tool=read id=call-… ⏎ fc_…_0 args=1`,
/// 排查时看起来像日志损坏)。
///
/// 只取第一段:`call-<uuid>-N` 本身就唯一。**上游侧不受影响** —— 我们从不把
/// call id 发回 Cursor(每轮都是全新的 Opening 请求,上一轮的 call id 被丢弃);
/// 将来真要走请求侧 `field 2` 工具通道回传结果,那里必须用 `ToolCall::id` 的**原值**,
/// 不能用这里这个。
fn client_tool_id(raw: &str) -> String {
    raw.split(['\n', '\r']).next().unwrap_or(raw).trim().to_string()
}

/// 这个错误是不是**客户端完整性门**(而不是真的额度问题)。
///
/// Cursor 对「config_version 空/过期」「客户端版本过旧」这类身份问题,回的是
/// `resource_exhausted` + "Update Required" 文案 —— 与真的额度耗尽**共用同一个 code**。
/// 分不开这两者的后果是不对称的:
/// - 把额度耗尽错判成完整性门 → 多试一次,浪费一点。
/// - 把完整性门错判成额度耗尽 → 账号被**持久禁用**,要人工 reset。
///
/// 所以判据宁松勿紧:文案里出现 update/upgrade/version 就当完整性门。
fn is_client_integrity_block(e: &run::TrailerError) -> bool {
    let hay = format!("{} {} {}", e.debug_error, e.title, e.detail).to_ascii_lowercase();
    ["update required", "please update", "upgrade", "client version", "out of date"]
        .iter()
        .any(|needle| hay.contains(needle))
}

/// 本轮失败:通知调用方别把会话记成「服务端已建立」。
fn fail(outcome: &Option<OutcomeHook>) {
    if let Some(h) = outcome {
        h(false);
    }
}

/// Cursor 帧流 → Anthropic SSE。
fn stream_to_anthropic(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    // 调用方是否请求了 thinking(见 `chat_stream` 里的取值处)。
    want_thinking: bool,
    body_keepalive: BodyKeepalive,
    outcome: Option<OutcomeHook>,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);

    tokio::spawn(async move {
        use futures::StreamExt;
        // 全程持有,任务结束(正常或提前 return)时随栈一起 drop,请求流那时才关。
        let _body_keepalive = body_keepalive;
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        let mut decoder = wire::FrameDecoder::new();
        // ⚠️ message_start 必须等到**真的有内容**再发。一旦首字节写给客户端,这次请求
        // 就 committed 了,gw-app 再也不能换号重试。而 Run 的错误恰恰是在流**末尾**的
        // trailer 里报的 —— 先发 message_start 等于把所有可重试的错误变成不可重试。
        let mut started = false;
        let mut output_chars: usize = 0;
        let mut trailer_err: Option<UpstreamError> = None;
        // 上游自报的用量(`1.14`)。有它就不用 chars/4 估 —— 那个估法在中文上偏差很大。
        let mut upstream_usage: Option<ChatUsage> = None;
        // 距上一次「有进展的帧」过了多久。**这是本 provider 唯一的失败兜底**:
        // 上游那个「200 + 每 10 秒心跳、永不生成」的状态**不会结束流、不发 trailer、
        // 不报任何错**,所以下面 `while let Some(...)` 会永远等下去,末尾那个
        // EmptyResponse 分支根本到不了。这正是本轮修的 bug 的形态 —— 而协议还会漂移,
        // 下次同样的形态一定会再出现一次。上游不给错误码,就得我们自己造一个。
        let mut last_progress = std::time::Instant::now();
        // 见过明确的收尾(用量帧或 end-stream trailer)。**没见过就不许报 end_turn** ——
        // 上游或中间代理用 HTTP/2 END_STREAM 干净截断时,底层流只是返回 `None`,
        // 于是一个「答了一半」的回复会被当成成功收尾:客户拿到残缺答案,
        // 账号健康度和请求日志却毫发无损,没有任何地方看得出来出过事。
        let mut saw_end = false;
        // 上游要求执行的工具(若有)。收尾时变成一个 `tool_use` 块 + `stop_reason: tool_use`。
        let mut tool_call: Option<run::ToolCall> = None;
        // 内建工具收口发生在「已经出过字」之后 = 本次回答被截断,但仍会按 end_turn 收尾。
        // 单独记一个标记,让收尾处能把它写进日志 —— 不然它和正常收尾在日志里没有区别。
        let mut builtin_truncated = false;
        // 正文开始后被丢掉的思考字数(见思考透传那一段的 else 分支)。
        let mut dropped_thinking: usize = 0;
        // 当前开着的内容块 `(索引, 种类)`,以及下一个可用索引。
        // Anthropic 的块索引必须**连续且不重复**,而 thinking / text / tool_use
        // 三种块都可能缺席,所以索引只能顺序分配,不能写死 0 和 1。
        let mut open_block: Option<(usize, &'static str)> = None;
        let mut next_idx: usize = 0;
        futures::pin_mut!(byte_stream);

        'outer: loop {
            let next = tokio::select! {
                biased;
                // 下游放弃了(客户端断连 / gw-app 换号重试)就立刻收工。
                // 不看这个的话:心跳状态下我们永远不会尝试 send,也就永远发现不了
                // receiver 已经没了,任务连同请求体 sender 一起长住,HTTP/2 流泄漏。
                _ = tx.closed() => {
                    tracing::debug!("cursor Run:下游已关闭,提前收工");
                    return;
                }
                chunk = byte_stream.next() => chunk,
                _ = tokio::time::sleep_until(
                    (last_progress + STALL_TIMEOUT).into()
                ) => {
                    tracing::warn!(
                        secs = STALL_TIMEOUT.as_secs(),
                        started,
                        "cursor Run:上游只发心跳、无任何进展,按空回复收口"
                    );
                    trailer_err = Some(UpstreamError::new(
                        UpstreamErrorKind::EmptyResponse,
                        format!(
                            "Cursor Run {}s 内只有心跳、没有任何文本或用量帧",
                            STALL_TIMEOUT.as_secs()
                        ),
                    ));
                    break 'outer;
                }
            };
            let Some(chunk) = next else { break 'outer };
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    fail(&outcome);
                    let _ = tx
                        .send(Err(UpstreamError::network(format!(
                            "读取 Cursor 流失败: {e}"
                        ))))
                        .await;
                    return;
                }
            };
            decoder.feed(&bytes);
            while let Some((flag, raw)) = decoder.next_frame() {
                // 先按 flag bit0 解压,再谈这一帧是什么。顺序反了的话,压缩过的
                // trailer(flag=0x03)会被当成明文 JSON 解析失败并被静默吞掉。
                let payload = match wire::frame_payload(flag, &raw) {
                    Ok(p) => p,
                    Err(e) => {
                        trailer_err = Some(UpstreamError::new(
                            UpstreamErrorKind::ServerError,
                            format!("Cursor 帧解压失败(flag={flag:#04x}): {e}"),
                        ));
                        break 'outer;
                    }
                };

                if flag & 0x02 != 0 {
                    let text = String::from_utf8_lossy(&payload);
                    if let Some(t) = run::parse_trailer(&text) {
                        trailer_err = Some(trailer_to_error(&t));
                    }
                    // 干净的 end-stream trailer 也是正常收尾 —— 与用量帧同级。
                    saw_end = true;
                    break 'outer;
                }

                let fr = run::parse_frame(&payload);
                // 上游帧种类很多(会话回显 field 4、计时 field 8、状态 1.8、思考 1.4…),
                // 只有 1.1.1 是正文。debug 级把每帧点名,排查「200 但没字」时不用再抓包。
                tracing::debug!(
                    flag,
                    bytes = payload.len(),
                    fields = ?run::top_fields(&payload),
                    inner = ?run::inner_fields(&payload),
                    status = ?run::status_code(&payload),
                    text_len = fr.text.chars().count(),
                    thinking_len = fr.thinking.chars().count(),
                    "cursor Run 响应帧"
                );

                // ⚠️ **用量必须最后判。** 「一帧只装一样东西」只是对抓包的观察,
                // protobuf 层没有任何东西阻止上游把最后一个正文增量和用量并进同一帧。
                // 先判用量就 break 的话,那一帧里的正文被静默丢掉,而客户还是收到
                // `end_turn` —— 表现为「回答少了结尾」且无任何错误。
                let usage = fr.usage;
                let text = fr.text;
                if !text.is_empty() || !fr.thinking.is_empty() || usage.is_some() {
                    last_progress = std::time::Instant::now();
                }

                // ── 思考透传 ────────────────────────────────────────────────
                //
                // **只在调用方请求了 thinking 时发。** 没请求却发 thinking 块,
                // 客户端要么报未知块类型、要么把推理当正文显示 —— 两种都比不发坏。
                // 不发时思考照旧丢掉(绝不能混进正文,见 run::RespFrame)。
                if want_thinking && !fr.thinking.is_empty() {
                    if !started {
                        if tx.send(Ok(StreamItem::Sse(message_start(&msg_id, &model)))).await.is_err() {
                            return;
                        }
                        started = true;
                    }
                    // thinking 块必须排在 text 之前(Anthropic 约定),而 text 一旦开过
                    // 就不能再回头开 thinking —— 那会让块顺序倒置。
                    if open_block.is_none() {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_start",
                                json!({"type":"content_block_start","index":next_idx,
                                       "content_block":{"type":"thinking","thinking":""}}),
                            ))))
                            .await;
                        open_block = Some((next_idx, "thinking"));
                    }
                    if let Some((idx, "thinking")) = open_block {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_delta",
                                json!({"type":"content_block_delta","index":idx,
                                       "delta":{"type":"thinking_delta","thinking":fr.thinking}}),
                            ))))
                            .await;
                    } else {
                        // 正文块已经开着 → 这段思考只能丢(Anthropic 不允许 thinking 排在
                        // text 之后)。**丢是对的,静默不对**:长回答里模型会二次规划,
                        // 那部分内容无声消失,排查「思考缺了一段」时没有任何痕迹。
                        dropped_thinking += fr.thinking.chars().count();
                        tracing::debug!(
                            chars = fr.thinking.chars().count(),
                            total = dropped_thinking,
                            "cursor Run:正文已开始,丢弃后续思考增量(块顺序不允许回头)"
                        );
                    }
                }

                if !text.is_empty() {
                    output_chars += text.chars().count();
                    if !started {
                        if tx.send(Ok(StreamItem::Sse(message_start(&msg_id, &model)))).await.is_err() {
                            return;
                        }
                        started = true;
                    }
                    // 思考块开着就先收掉 —— 一个索引上只能有一种块。
                    if let Some((idx, "thinking")) = open_block {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_stop",
                                json!({"type":"content_block_stop","index":idx}),
                            ))))
                            .await;
                        open_block = None;
                        next_idx += 1;
                    }
                    if open_block.is_none() {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_start",
                                json!({"type":"content_block_start","index":next_idx,
                                       "content_block":{"type":"text","text":""}}),
                            ))))
                            .await;
                        open_block = Some((next_idx, "text"));
                    }
                    if let Some((idx, _)) = open_block {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_delta",
                                json!({"type":"content_block_delta","index":idx,
                                       "delta":{"type":"text_delta","text":text}}),
                            ))))
                            .await;
                    }
                }

                // 工具调用:上游在等客户端执行工具并回帧。
                //
                // ⚠️ **这段必须排在正文/思考发射之后。** 与「用量必须最后判」是同一条
                // 原则:protobuf 层没有任何东西阻止上游把最后一个正文增量和工具调用
                // 并进同一帧。先判工具就 break 的话,那一帧的正文被静默丢掉,而客户
                // 收到的是一个看起来正常的 `tool_use` —— 回答少了结尾,无错误、无日志。
                // 早先 usage 帧防了这一手、工具帧没防,是同一条原则只执行了一半。
                if run::is_tool_call(&payload) {
                    // 逆向期用:把工具调用帧原样落盘,好离线解字段号。
                    // 设了 CURSOR_DUMP_TOOL_FRAMES=<目录> 才写,生产不留这条路径。
                    if let Ok(dir) = std::env::var("CURSOR_DUMP_TOOL_FRAMES") {
                        let f = format!("{dir}/toolframe_{}_{}.bin", msg_id, payload.len());
                        let _ = std::fs::write(&f, &payload);
                        tracing::warn!(file = %f, "已落盘工具调用帧");
                    }
                    match run::parse_tool_call(&payload) {
                        // 我们声明过的工具 → 映射成 Anthropic `tool_use`,交给调用方执行。
                        Some(tc) => {
                            tracing::info!(
                                // 用清洗后的 id:原值带换行,会把这行日志劈成两半。
                                tool = %tc.name, id = %client_tool_id(&tc.id), args = tc.args.len(),
                                "cursor Run:模型调用工具,转成 tool_use 交给调用方"
                            );
                            tool_call = Some(tc);
                        }
                        // Cursor 服务端**内建**的工具(终端 / 读文件 …)。它们是闭集枚举、
                        // 不带名字,调用方也没有对应的工具可执行,所以只能收口。
                        // preview 留在日志里,是为了万一要扩内建工具映射时有据可查。
                        None => {
                            let preview: String = String::from_utf8_lossy(&payload)
                                .chars()
                                .map(|c| if c.is_control() { '·' } else { c })
                                .take(300)
                                .collect();
                            tracing::warn!(
                                bytes = payload.len(), started, output_chars, preview = %preview,
                                "cursor Run:模型调用了 Cursor 内建工具(我们无法执行)—— 收口"
                            );
                            if !started {
                                trailer_err = Some(UpstreamError::new(
                                    UpstreamErrorKind::EmptyResponse,
                                    "Cursor 模型选择调用其内建工具,gw-cursor 无法代为执行",
                                ));
                            } else {
                                // 已经出过字了。首包既已 committed,报错也换不了号,
                                // 把已有的字交出去比丢掉整个回答好 —— 但这**确实是一次截断**,
                                // 而下面会照常发 `end_turn`。所以必须在这里留一条显式痕迹,
                                // 否则日志里它和一次正常收尾长得一模一样。
                                builtin_truncated = true;
                                tracing::warn!(
                                    output_chars,
                                    "cursor Run:内建工具收口发生在已出字之后 —— 本次回答是截断的,\
                                     但仍按 end_turn 收尾(首包已 committed,无法重试)"
                                );
                            }
                        }
                    }
                    // 内建工具那一支下面会照常走收尾流程(而不是报错),所以这里算「见过收尾」。
                    // 外部工具那一支本来就是正常的 tool_use 收尾。
                    saw_end = true;
                    break 'outer;
                }

                // 用量帧 = 本轮结束。BiDi 流永远不会自己关,不在这里收口的话
                // 每个请求都要挂到客户端超时 —— 「答完了却一直转圈」。
                if let Some((input, output, cached)) = usage {
                    upstream_usage = Some(ChatUsage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cached,
                        ..Default::default()
                    });
                    saw_end = true;
                    break 'outer;
                }
            }
        }

        if let Some(err) = trailer_err {
            fail(&outcome);
            let _ = tx.send(Err(err)).await;
            return;
        }

        // 上游 200 却一个字都没有。旧版会照常发完整收尾,gw-app 看到的是一次
        // 「成功的空回复」—— 客户拿到空白,而账号健康度毫发无损。明确报 EmptyResponse,
        // 让 empty-fallback 策略接管。
        // 有工具调用时,「一个字都没出」是**正常**的:模型可以直接决定调工具而不先说话。
        // 那时也必须补一个 message_start,否则后面的块没有依附。
        if !started && tool_call.is_some() {
            let _ = tx.send(Ok(StreamItem::Sse(message_start(&msg_id, &model)))).await;
        } else if !started {
            fail(&outcome);
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::EmptyResponse,
                    "Cursor Run 返回 200 但没有任何文本帧",
                )))
                .await;
            return;
        }

        // 出过字、但流是**断掉**的而不是收尾的 —— 报截断,不许伪装成 end_turn。
        // 首包已经发出去了,gw-app 换不了号,但至少让这次请求在日志里是红的。
        if !saw_end {
            fail(&outcome);
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    format!("Cursor Run 流在收到用量帧之前中断(已输出 {output_chars} 字)"),
                )))
                .await;
            return;
        }

        // 上游自报优先,没有才退回 chars/4 粗估。
        let usage = upstream_usage.unwrap_or(ChatUsage {
            output_tokens: (output_chars as u64).div_ceil(4),
            ..Default::default()
        });
        let output_tokens = usage.output_tokens;
        // 关掉还开着的那个块(可能是 thinking,可能是 text,也可能一个都没开)。
        if let Some((idx, _)) = open_block {
            let _ = tx
                .send(Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":idx}),
                ))))
                .await;
            // 这里只需要推进索引:块已经收掉,`open_block` 之后不再被读。
            next_idx += 1;
        }

        // 工具调用块。索引接在文本块之后 —— Anthropic 的块索引必须连续且不重复。
        //
        // `input` 一次性给全,所以 `input_json_delta` 只发一帧。分片发没有意义:
        // 我们是从 protobuf 一次解出来的,本来就没有增量。
        let stop_reason = if let Some(tc) = &tool_call {
            let idx = next_idx;
            // 参数值已经是解好的 JSON(数字仍是数字、对象仍是对象)。
            // 早先这里是 `Value::String(v.clone())` —— 把一切都变成字符串,
            // `{"limit": 200}` 会发成 `{"limit": "200"}`,schema 写 number 的工具直接拒。
            let input: serde_json::Map<String, Value> =
                tc.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let _ = tx
                .send(Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_start",
                    json!({"type":"content_block_start","index":idx,
                           "content_block":{"type":"tool_use","id":client_tool_id(&tc.id),
                                            "name":tc.name,"input":{}}}),
                ))))
                .await;
            let _ = tx
                .send(Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":idx,
                           "delta":{"type":"input_json_delta",
                                    "partial_json": Value::Object(input).to_string()}}),
                ))))
                .await;
            let _ = tx
                .send(Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":idx}),
                ))))
                .await;
            "tool_use"
        } else {
            "end_turn"
        };

        // ⚠️ usage 要**在 SSE 里也说全**。
        //
        // `message_start` 只能写 0:那时上游还没告诉我们输入量(`1.14` 是流末的帧)。
        // 但收尾这一帧是知道的,而早先这里只发 `output_tokens` —— 于是客户在 SSE 里
        // 看到输入恒为 0,却按非零输入被计费(计费走的是不转发客户端的
        // `StreamItem::Usage`)。**客户拿响应对不上账单**,而且 kiro/dario 的口径
        // 也没法横向比。现在两条路给的是同一组数。
        //
        // 口径与 caio 全局一致(见 `gw_core::pricing` 模块文档):`input_tokens` 是
        // **总上下文**、`cache_read_input_tokens` 是它的子集,计价方自己相减。
        let mut delta_usage = json!({"output_tokens": output_tokens});
        if let Some(m) = delta_usage.as_object_mut() {
            m.insert("input_tokens".into(), json!(usage.input_tokens));
            if usage.cache_read_tokens > 0 {
                m.insert(
                    "cache_read_input_tokens".into(),
                    json!(usage.cache_read_tokens),
                );
            }
        }
        if builtin_truncated || dropped_thinking > 0 {
            tracing::info!(
                builtin_truncated,
                dropped_thinking,
                output_chars,
                "cursor Run 收尾(带保留意见:见上面的 warn)"
            );
        }
        let _ = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                json!({"type":"message_delta",
                       "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                       "usage": delta_usage}),
            ))))
            .await;
        let _ = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_stop",
                json!({"type":"message_stop"}),
            ))))
            .await;
        // 走到这里 = 正常收尾。服务端已经持有这一轮,下次可以只发新消息。
        if let Some(h) = &outcome {
            h(true);
        }
        let _ = tx.send(Ok(StreamItem::Usage(usage))).await;
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RunCtx {
        RunCtx {
            host: "agentn.api5.cursor.sh".into(),
            token: "tok".into(),
            machine_id: "m".repeat(64),
            mac_machine_id: Some("a".repeat(64)),
            config_version: "cv".into(),
            timezone: "Asia/Shanghai".into(),
            conversation_id: "conv-1".into(),
            shape: RunShape::default(),
            context_frames: true,
            phase: run::Phase::Opening,
            keep_stream_open: true,
        }
    }

    /// 工具回路第 2 轮:折叠后上游看到的「当前用户消息」。
    ///
    /// 这个用例存在的理由是一次真实事故:grok 在 opencode 里对同一个文件连调
    /// 9 次 `read`。原因看这里打印出来的东西 —— 折叠把 tool_result 变成了
    /// **本轮用户消息的全部内容**,模型收到的是一段没有问题的工具返回。
    #[test]
    fn tool_loop_second_round_leaves_a_question_in_the_current_message() {
        let body = json!({
          "tools": [{"name":"read","input_schema":{"type":"object"}}],
          "messages": [
            {"role":"user","content":"读一下 data.txt 然后告诉我里面有什么"},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"call-abc-0\nfc_abc_0","name":"read",
                 "input":{"filePath":"/tmp/data.txt"}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call-abc-0\nfc_abc_0",
                 "content":[{"type":"text","text":"gamma\ndelta\n"}]}]}
          ]
        });
        assert!(last_user_is_tool_result_only(&body), "这一轮应被认成工具回路中间轮");
        let task = latest_real_user_request(&body);
        assert_eq!(task.as_deref(), Some("读一下 data.txt 然后告诉我里面有什么"));

        let turns = fold_history(&to_turns(&body), task.as_deref());
        assert_eq!(turns.len(), 1);
        // 原始问题必须还在**当前**消息里,不能只留在 <conversation_history> 里 ——
        // 否则本轮的用户消息只是一段裸的工具返回,模型会重新调一遍工具。
        let current = turns[0]
            .text
            .rsplit("</conversation_history>")
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            current.contains("告诉我里面有什么"),
            "本轮用户消息里没有任何问题,只有工具返回:\n{current}"
        );
        // 工具结果本身当然也要还在。
        assert!(current.contains("gamma"));
        // 而且要明确禁止重复调用 —— 这就是那 9 次 read 的直接病因。
        assert!(current.contains("不要重复调用"));
    }

    #[test]
    fn a_real_question_turn_is_not_treated_as_a_tool_loop() {
        // 普通提问轮不能被复述指令污染(否则每一轮都多一段废话)。
        let body = json!({"messages":[{"role":"user","content":"你好"}]});
        assert!(!last_user_is_tool_result_only(&body));
        let turns = fold_history(&to_turns(&body), None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "你好", "单轮请求不该被加工");
        assert!(!turns[0].text.contains("<conversation_history>"));
    }

    #[test]
    fn tool_loop_uses_the_latest_question_not_the_first() {
        // 用户改了主题:复述必须取**最近**那个问题,否则把模型带回上个话题。
        let body = json!({"messages":[
            {"role":"user","content":"先看看 a.txt"},
            {"role":"assistant","content":"好"},
            {"role":"user","content":"算了,改成统计 b.txt 的行数"},
            {"role":"assistant","content":[{"type":"tool_use","id":"c-1","name":"bash","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"c-1","content":"42"}]}
        ]});
        assert_eq!(
            latest_real_user_request(&body).as_deref(),
            Some("算了,改成统计 b.txt 的行数")
        );
    }

    #[test]
    fn client_tool_id_drops_the_second_segment() {
        // 上游 call id 是两段用换行连的;换行进 id 会让客户端校验失败、日志断行。
        assert_eq!(
            client_tool_id("call-8268dbb8-a85a-4ea1-8b99-452b916b55de-0\nfc_1fd1cd59_0"),
            "call-8268dbb8-a85a-4ea1-8b99-452b916b55de-0"
        );
        // 没有第二段时原样(去空白)
        assert_eq!(client_tool_id("call-x-0"), "call-x-0");
        assert!(!client_tool_id("a\r\nb").contains(['\r', '\n']));
    }

    #[test]
    fn extract_text_handles_string_and_blocks() {
        assert_eq!(extract_text(&json!("plain")), "plain");
        assert_eq!(
            extract_text(
                &json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}])
            ),
            "a\nb"
        );
    }

    #[test]
    fn system_stays_out_of_the_conversation_turns() {
        // system 的家是 1.2.17.9.25;混进用户消息会让模型把系统指令当用户说的话。
        let body = json!({
            "system": "be brief",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"yo"},
                {"role":"user","content":"more"}
            ]
        });
        let turns = to_turns(&body);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "hi", "system 不该出现在用户轮里");
        assert!(turns.iter().all(|t| !t.text.contains("be brief")));
        assert_eq!(turns[2].text, "more");
        assert!(!turns[1].is_user);
        // 单独取得出来
        assert_eq!(extract_system(&body), "be brief");
    }

    #[test]
    fn unsupported_content_keeps_turn_alignment() {
        // 只含 image 的一轮不能被整条丢掉,否则 user/assistant 交替会错位。
        let body = json!({"messages":[
            {"role":"user","content":[{"type":"image"}]},
            {"role":"assistant","content":"ok"}
        ]});
        let turns = to_turns(&body);
        assert_eq!(turns.len(), 2);
        assert!(turns[0].is_user);
        assert!(!turns[1].is_user);
    }

    #[test]
    fn session_keys_are_stable_per_conversation_and_differ_by_purpose() {
        let a = session_key("tok", "conv-1", "blob");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 同会话稳定
        assert_eq!(a, session_key("tok", "conv-1", "blob"));
        // 换用途、换会话、换号都要不同
        assert_ne!(a, session_key("tok", "conv-1", "fs"));
        assert_ne!(a, session_key("tok", "conv-2", "blob"));
        assert_ne!(a, session_key("tok2", "conv-1", "blob"));
    }

    #[test]
    fn headers_match_protocol_section_2() {
        let c = reqwest::Client::new();
        let req = apply_headers(c.post("https://example.invalid/x"), &ctx())
            .build()
            .unwrap();
        let h = req.headers();
        let get = |k: &str| h.get(k).map(|v| v.to_str().unwrap().to_string());

        // §2.1 静态 —— 含两条旧代码发反了的
        assert_eq!(get("x-ghost-mode").as_deref(), Some("true"));
        assert_eq!(get("x-new-onboarding-completed").as_deref(), Some("false"));
        assert_eq!(get("connect-content-encoding").as_deref(), Some("gzip"));
        assert_eq!(get("x-cursor-streaming").as_deref(), Some("true"));
        assert_eq!(get("x-cursor-remote-type").as_deref(), Some("none"));
        assert_eq!(
            get("x-cursor-retryinterceptor-enabled").as_deref(),
            Some("true")
        );

        // §2.2 —— client-type 是 glass 不是 ide;两条必须**不存在**
        assert_eq!(get("x-cursor-client-type").as_deref(), Some("glass"));
        assert_eq!(get("x-cursor-client-layout").as_deref(), Some("glass"));
        assert!(
            get("x-cursor-client-commit").is_none(),
            "真请求不发 client-commit"
        );
        assert!(
            get("x-cursor-client-os-version").is_none(),
            "真请求不发 client-os-version"
        );

        // §2.3 派生
        assert_eq!(get("authorization").as_deref(), Some("Bearer tok"));
        assert_eq!(get("x-client-key").unwrap().len(), 64);
        assert_eq!(get("x-session-id").unwrap().len(), 36);
        // checksum = b64url(6B) + 64 + "/" + 64 = 137
        assert_eq!(get("x-cursor-checksum").unwrap().len(), 137);

        // §2.5 每会话
        assert_eq!(get("x-blob-encryption-key").unwrap().len(), 64);
        assert_eq!(get("x-fs-client-key").unwrap().len(), 64);
        assert_ne!(get("x-blob-encryption-key"), get("x-fs-client-key"));

        // 真请求发空 cookie 头
        assert_eq!(get("cookie").as_deref(), Some(""));

        // §2.6 trace:original == request,amzn 带 Root=
        let rid = get("x-request-id").unwrap();
        assert_eq!(get("x-original-request-id").as_deref(), Some(rid.as_str()));
        assert_eq!(get("x-amzn-trace-id"), Some(format!("Root={rid}")));
        // 两组 traceparent 独立随机,且 **flags 不同**(抓包实物 -00 / -01)
        assert_ne!(get("traceparent"), get("backend-traceparent"));
        assert!(get("traceparent").unwrap().starts_with("00-"));
        assert!(get("traceparent").unwrap().ends_with("-00"), "traceparent 未采样");
        assert!(get("backend-traceparent").unwrap().ends_with("-01"), "backend 已采样");
    }

    #[test]
    fn rate_limit_downgrade_maps_to_model_not_available() {
        // 这是全文件最重要的一条分类:该号在该模型上没额度 ≠ 该号坏了。
        let t = run::TrailerError {
            code: "resource_exhausted".into(),
            debug_error: "ERROR_RATE_LIMITED_CHANGEABLE".into(),
            title: "API usage limit reached".into(),
            detail: "Switched to grok-4.5 after reaching API limit.".into(),
            auto_switch_to_model: "grok-4.5".into(),
        };
        let e = trailer_to_error(&t);
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
        // 不惩罚账号健康,才不会因为点了 claude 就把整个号冷却掉
        assert!(e.kind.spares_account_health());
    }

    #[test]
    fn trailer_codes_map_to_distinct_kinds() {
        let mk = |code: &str, dbg: &str| {
            trailer_to_error(&run::TrailerError {
                code: code.into(),
                debug_error: dbg.into(),
                ..Default::default()
            })
            .kind
        };
        assert_eq!(mk("unauthenticated", ""), UpstreamErrorKind::TokenInvalid);
        assert_eq!(mk("permission_denied", ""), UpstreamErrorKind::TokenInvalid);
        assert_eq!(mk("resource_exhausted", ""), UpstreamErrorKind::QuotaExhausted);
        assert_eq!(mk("invalid_argument", ""), UpstreamErrorKind::BadRequest);
        assert_eq!(mk("unavailable", ""), UpstreamErrorKind::Overloaded);
        assert_eq!(mk("internal", ""), UpstreamErrorKind::ServerError);
        assert_eq!(mk("weird_code", ""), UpstreamErrorKind::Other);
        // 模型名非法不换号(换号也一样错)
        assert_eq!(
            mk("invalid_argument", "ERROR_BAD_MODEL_NAME"),
            UpstreamErrorKind::BadRequest
        );
    }

    #[test]
    fn http_error_prefers_structured_trailer_over_status() {
        // 非 2xx 时 body 里若有 Connect 错误体,应按它分类而不是只看状态码。
        let body = r#"{"error":{"code":"resource_exhausted","details":[{"debug":
            {"error":"ERROR_RATE_LIMITED_CHANGEABLE","details":{"additionalInfo":
            {"autoSwitchToModel":"grok-4.5"}}}}]}}"#;
        let e = classify_http_error(429, body);
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
        // 没有结构化体时退回状态码分类
        assert_eq!(
            classify_http_error(429, "rate limited").kind,
            UpstreamErrorKind::RateLimited
        );
        assert_eq!(
            classify_http_error(401, "nope").kind,
            UpstreamErrorKind::TokenInvalid
        );
    }
}
