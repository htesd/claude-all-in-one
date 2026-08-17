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
    /// 账号 id:模拟缓存的会话键要按账号隔离(服务端会话是 per-account 的,
    /// 换号 = 冷启动),见 [`crate::cache_sim`]。
    pub account_id: String,
    /// 本次请求处在会话的哪个阶段。由 [`crate::ConvRegistry`] 判定:
    /// 服务端已有这个会话(且还是同一个号)→ [`Phase::Continuation`],否则降级回
    /// [`Phase::Opening`] 把历史整个重铺。
    pub phase: run::Phase,
    /// 请求形态(IDE / CLI)。见 [`crate::cli::Profile`]。
    pub profile: crate::cli::Profile,
    /// 帧0 发哪些分节。默认全发(完整模拟客户端);出问题时二分用。
    pub shape: RunShape,
    /// 是否补发 `field 3` 上下文帧(真客户端初始发 3 帧:帧0 + 两个 field 3)。
    pub context_frames: bool,
    /// 发完初始帧后是否**保持请求流不关闭**。
    ///
    /// 真客户端是 BiDi:发完 3 帧继续发 keepalive/增量,不 half-close。
    /// `false` 则一次性发完 body 就关流(HTTP 语义上是 half-close)。
    /// 两种行为服务端可能区别对待,做成开关以便实测。
    ///
    /// ⚠️ 关掉它的代价不止是形态差异:服务端 write/read 调用(带图流程)的
    /// 回执要从请求侧送回去,流关了就没法回 —— 带图请求会 90s 死等。
    pub keep_stream_open: bool,
    /// 服务端 write_args 推来的资产内存库(带图流程的回图来源)。见 [`crate::AssetStore`]。
    pub assets: std::sync::Arc<crate::AssetStore>,
    /// 「上一轮被内建工具截断」的待发纠偏标记。见 [`crate::TruncationNotices`]。
    pub notices: std::sync::Arc<crate::TruncationNotices>,
}

/// `message_start` 事件。抽出来是因为三条路径都要发它(思考先到 / 正文先到 /
/// 纯工具调用),而漏发会让后面的块没有依附。
fn message_start(msg_id: &str, model: &str) -> SseEvent {
    message_start_pub(msg_id, model)
}

/// `pub(crate)`:CLI 驱动(clidrv)也发同一形态。
pub(crate) fn message_start_pub(msg_id: &str, model: &str) -> SseEvent {
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

/// 收尾帧(`message_delta`)的 usage JSON。
///
/// **线上口径 = Anthropic 规范**(与 gw-kiro `usage.rs::build_usage_json` 一致):
/// `input_tokens` 只算**未命中缓存的新增部分**,缓存读取单列。上游 `1.14` 给的
/// input 是含缓存命中的总上下文,直接透传的话,按 Anthropic 语义解读的下游
/// (Claude Code 的用量显示、new-api 的客户计费)会把缓存部分重复计费。
/// cursor 的 `1.14` 只有 {输入,输出,缓存命中} 三个数,没有缓存创建计数,
/// 所以不出 `cache_creation_input_tokens`(kiro 同理没有)。
///
/// ⚠️ 这只是**给客户端的线上口径**;`StreamItem::Usage` 的入库口径不变
/// (仍是总上下文,计价方自己相减,见 `gw_core::pricing` 模块文档)。
fn delta_usage_json(usage: &ChatUsage, output_tokens: u64) -> Value {
    delta_usage_json_impl(usage, output_tokens)
}

/// `pub(crate)`:CLI 驱动(clidrv)收尾复用同一口径。output 取 usage 里的值。
pub(crate) fn delta_usage_json_pub(usage: &ChatUsage) -> Value {
    delta_usage_json_impl(usage, usage.output_tokens)
}

fn delta_usage_json_impl(usage: &ChatUsage, output_tokens: u64) -> Value {
    let uncached_input = usage.input_tokens.saturating_sub(usage.cache_read_tokens);
    let mut m = serde_json::Map::new();
    m.insert("input_tokens".into(), json!(uncached_input));
    m.insert("output_tokens".into(), json!(output_tokens));
    if usage.cache_read_tokens > 0 {
        m.insert(
            "cache_read_input_tokens".into(),
            json!(usage.cache_read_tokens),
        );
    }
    Value::Object(m)
}

/// 文本的粗略 token 估算(计费兜底口径)。
///
/// ASCII ≈ 4 字符/token;非 ASCII 按 1 字符/token 的保守值 —— 中文实测多在
/// 1~1.5 字符/token,emoji/组合字符甚至超过 1 token/字符。这是计费代码,
/// 估不准就宁多勿少(低估 = 运营方贴钱)。
pub(crate) fn est_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + other
}

/// 上游没给 `1.14` 用量时的回退估算(典型场景:**外部 tool_use 收口**)。
///
/// 反代是一问一答:认出外部工具就 `break` 并把 `tool_use` 交给调用方,不会在同
/// 一条 BiDi 流里等服务端再吐用量(协议文档也写了 tool_use 轮通常不带 `1.14`)。
/// 旧回退只估 `output = chars/4`,于是 new-api / 面板看到 **输入=0、缓存=0**,
/// 只有输出 —— 生产上 fable 近两小时 39/40 条成功请求都是这个形态。
///
/// 本函数**只估 input/output,不估缓存**:cache_read 一律由 [`crate::cache_sim`]
/// 在收尾处统一给(按会话指纹的真实前缀命中模拟,与 kiro 同口径)。早先这里用
/// 「input − 本轮新增」的一次性启发式估缓存,但生产上 `fold_history` 把多轮折成
/// 单条 Turn,`turns.len() > 1` 恒不成立,那段代码实际是死的;而且新会话首轮
/// 粘贴长历史会被它误判成命中。模拟器两条都不沾。
fn estimate_usage_fallback(
    system: &str,
    turns: &[run::Turn],
    tools: &[run::ToolDef],
    output_chars: usize,
) -> ChatUsage {
    let input_tokens = est_text_tokens(system)
        + turns
            .iter()
            .map(|t| est_text_tokens(&t.text))
            .sum::<u64>()
        + tools
            .iter()
            .map(|t| {
                est_text_tokens(&t.name) + est_text_tokens(&t.description) + est_text_tokens(&t.schema)
            })
            .sum::<u64>();
    let output_tokens = (output_chars as u64).div_ceil(4);

    ChatUsage {
        input_tokens,
        output_tokens,
        // 缓存归模拟器管,这里恒 0(见函数级注释)。
        cache_read_tokens: 0,
        ..Default::default()
    }
}

/// 工具调用的「出字量」估算:工具名 + 整个参数对象的 JSON 序列化(token 粗估)。
///
/// tool_use 轮的产出大头就是参数 JSON(纯工具调用轮正文可以是零)——只数正文
/// delta 会得到 output≈0,把工具参数漏出账单。整对象一次序列化,引号/逗号/
///  braces 等结构字符也算进估算(短 key 多时占比不小)。
fn tool_call_tokens(tc: &run::ToolCall) -> u64 {
    let args: serde_json::Map<String, Value> = tc.args.iter().cloned().collect();
    let args_tokens = serde_json::to_string(&Value::Object(args))
        .map(|s| est_text_tokens(&s))
        .unwrap_or(0);
    est_text_tokens(&tc.name) + args_tokens
}

/// 把上游 `1.14` 三元组收成 [`ChatUsage`]。
///
/// 上游自报的缓存命中是**真实**命中,`cache_read` 与 `real_cache_read` 同值 ——
/// 漏填 `real_cache_read` 会让面板「真实缓存」列永远是 0。注意 cursor 现在是
/// 双轨口径(与 kiro 同构):收尾处若上游 cached=0,`cache_read` 会被
/// [`crate::cache_sim`] 的模拟值顶替(客户计费列),而 `real_cache_read`
/// 永远只认这里填的上游自报值(对账列)。
fn usage_from_upstream(input: u64, output: u64, cached: u64) -> ChatUsage {
    ChatUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        real_cache_read_tokens: cached,
        ..Default::default()
    }
}

/// 客户端是否要求透传思考块(以及把思考帧算作流进展)。
///
/// - `enabled` / `adaptive`(Claude Code):要透传。
/// - `disabled` / 缺省 / 未知:不透传;未下发的思考也不得刷新 stall 计时。
pub(crate) fn client_wants_thinking(thinking: Option<&Value>) -> bool {
    matches!(
        thinking
            .and_then(|t| t.get("type"))
            .and_then(|t| t.as_str()),
        Some("enabled" | "adaptive")
    )
}

/// 从 Anthropic content(string 或 block 数组)抽出纯文本。
fn extract_text(content: &Value) -> String {
    extract_text_in_msg(content, None)
}

/// [`extract_text`] 的占位感知版。`media` = `(消息下标, to_media 的占位表)`:
/// 只有 [`to_turns_with_media`] 带着它 —— tool_result **内嵌**的 image/document
/// 在文本流里留下 `[图片见附件 attach-N]` 占位,编号与 [`to_media`] 收集顺序
/// 严格一致(占位表与附件出自同一次扫描,不是两边各数各的)。
/// system / 亲和键等其它调用点不带表,行为与旧版逐字相同。
fn extract_text_in_msg(content: &Value, media: Option<(usize, &MediaPlaceholders)>) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for (bi, b) in blocks.iter().enumerate() {
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
                        // content 可能是字符串,也可能是块数组。块数组逐块渲染:
                        // text 原样,**内嵌 image/document 留占位文本**指向 to_media
                        // 收进的真附件。曾经这里把媒体块丢给 `_ => None`,注释声称
                        // 「它们走 to_media 内联」,而 to_media 只扫顶层块 —— 两边
                        // 互相推诿,工具返回的图片就静默消失了(2026-08-13 生产事故)。
                        let c = b.get("content").cloned().unwrap_or(Value::Null);
                        let body = match &c {
                            Value::String(s) => s.clone(),
                            Value::Array(nested) => {
                                let mut nb = String::new();
                                for (ni, n) in nested.iter().enumerate() {
                                    let nk =
                                        n.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    let piece = match nk {
                                        "text" => n
                                            .get("text")
                                            .and_then(|t| t.as_str())
                                            .map(str::to_string),
                                        // 占位表里没有 = 该块没被收(非 base64 / 超限 /
                                        // 调用点不带表),沿旧行为不渲染,编号不空洞。
                                        "image" | "document" => media.and_then(
                                            |(mi, map)| map.get(&(mi, bi, ni)).cloned(),
                                        ),
                                        _ => None,
                                    };
                                    if let Some(p) = piece {
                                        if !nb.is_empty() {
                                            nb.push('\n');
                                        }
                                        nb.push_str(&p);
                                    }
                                }
                                nb
                            }
                            other => extract_text(other),
                        };
                        let err = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        Some(format!(
                            "[工具{}返回]\n{body}",
                            if err { "出错" } else { "" }
                        ))
                    }
                    // 顶层 image / document 不进文本 —— 它们走 `to_media` 内联成真附件
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

/// 取顶层 `system` 文本,并**剥掉每请求都变的滚动指纹行**。
///
/// ## 为什么这一行剥不掉就什么都不对
///
/// Claude Code 在 system 顶部拼一行
/// `x-anthropic-billing-header: …; cch=<5位16进制>;`,`cch` 是**每请求都变**的 body 哈希
/// (详见 [`gw_core::normalize::strip_rolling_fingerprints`])。而本函数的产物同时喂给
/// **三个**地方:
///
/// 1. [`affinity_key_from_body`] —— 会话亲和键 / 上游 `conversation_id` 的来源;
/// 2. `1.2.1.2.25`(`DET_SYSTEM_PROMPT`)—— 发给上游的系统提示,prefix cache 的前缀;
/// 3. [`crate::cache_sim`] 的指纹 —— 命中率统计。
///
/// 不剥的连锁后果(三条都表现为「莫名其妙就是不命中」):
///
/// - 会话键每请求都变 → 调度层的账号钉扎失效、来回换号;
/// - `conversation_id` 每轮都变 → [`crate::ConvRegistry::phase_for`] 永远拿不到
///   `Continuation`,每轮都按首轮形态发(环境块/预算表按 `Opening` 走);
/// - 缓存指纹每轮都变 → 报出来的命中率恒为 0。
///
/// ⚠️ **它治不了「模型绕圈重复」**。那个症状的成因是 [`fold_history`] 把历史折成一条
/// 假对话记录,而折叠是**无条件**执行的 —— `ctx.phase` 只传给 `build_frame0` 决定环境块,
/// 从不决定要不要折(见 `chat_stream` 里 `fold_history` 的调用点)。也就是说即便
/// `Continuation` 命中,上游收到的仍是那条长文本。要治绕圈得单独改折叠这条路。
/// (第一版注释在这里把两件事连成了因果,是错的;对抗评审 Skeptic#2 指出。)
///
/// gw-kiro 早有这道处理(`converter::normalize`),cursor 这边一直没有 —— 同一份实现
/// 现已上收到 gw-core,两边共用,不会再各自漂移。
pub(crate) fn extract_system(body: &Value) -> String {
    let raw = body.get("system").map(extract_text).unwrap_or_default();
    gw_core::normalize::strip_rolling_fingerprints(&raw)
}

/// `SessionStart` hook 注入的稳定前缀标记(跨轮不变,值得提升进 system)。
const SESSION_START_PREFIX: &str = "SessionStart hook additional context:";
/// Claude Code / Agent SDK 的身份行:出现即说明这条 system 消息是稳定前缀。
const CC_IDENTITY_LINE: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const SDK_IDENTITY_LINE: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// 用户中途插话被中转框成 system 消息时的固定开头。
const INTERRUPTED_USER_PREFIX: &str = "The user sent a new message while you were working:";

/// 稳定 system 前缀:跨轮不变,提升进顶层 `system` 反而利于前缀缓存。
fn is_stable_system_prefix(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with(SESSION_START_PREFIX)
        || text
            .lines()
            .map(str::trim_start)
            .any(|l| l == CC_IDENTITY_LINE || l == SDK_IDENTITY_LINE)
}

/// **每轮都变、对模型零信息增量**的注入,丢弃。
///
/// 这里的判据必须精确到「整条消息就是这个东西」:一旦放宽成子串匹配,用户正文里
/// 提到 `<total_tokens>` 就会让半条消息凭空消失。
fn is_dynamic_system_noise(text: &str) -> bool {
    let t = text.trim();
    // ① 剩余预算计数器:`<total_tokens>15000000 tokens left</total_tokens>`。
    //    **它就是本次事故的元凶**(见 route_system_role_messages 的文档)。
    (t.starts_with("<total_tokens>") && t.ends_with("</total_tokens>"))
        // ② 「最近没用 task 工具」的周期性催促。
        || t.starts_with("The task tools haven't been used recently.")
}

/// interrupted-user 特例:实为用户插话,被中转框成了 system。取出正文当 user。
fn interrupted_user_payload(text: &str) -> Option<String> {
    let body = text.trim_start().strip_prefix(INTERRUPTED_USER_PREFIX)?;
    let payload = body
        .split_once("\n\nIMPORTANT:")
        .map_or(body, |(p, _)| p)
        .trim();
    (!payload.is_empty()).then(|| payload.to_string())
}

/// 把 `messages[]` 里**代理链中段注入的 `role:"system"` 消息**分流掉,原地改写 `body`。
///
/// ## 这道处理不做的后果(2026-08-17 生产事故,用户报障原话「grok 收到空消息」)
///
/// 真实流量里,客户端(经中转)会在**每条用户消息之后**再追一条
/// `{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"}`。
/// 而 [`to_turns`] 判 `is_user` 用的是 `role != "assistant"` —— 于是这条计数器成了
/// **最后一轮**。两条路径同时被带偏:
///
/// 1. **CLI 驱动的 prompt 只发最后一轮**([`crate::clidrv::start_conv`]),发出去的
///    整条 prompt 就是那行计数器。grok 的 thinking 原文:
///    "The user's message contains only a token count indicator." —— 用户看到的是
///    「你这条消息是空的」,而他明明打了一大段。
/// 2. [`last_tool_results`] 要求末条是 `role=="user"`,尾巴是 system 就返回 `None`
///    → 挂起的桥调用**永远接不上** → 模型反复说「MCP 读取被中断了」,
///    并伴随 90s stall 与 `incomplete_stream`。
///
/// 线协议那条路侥幸没露:[`fold_history`] 无条件全量重铺,尾巴上多一行计数器无伤。
/// 所以症状是「切了 CLI 驱动才开始空」,很容易误判成 CLI 驱动本身坏了。
///
/// ## 四级分流
///
/// | 类别 | 处置 | 理由 |
/// |---|---|---|
/// | 稳定前缀(hook / 身份行) | 提升进顶层 `system` | 跨轮不变,进 system 才吃得到前缀缓存 |
/// | 动态噪声(预算计数器等) | 丢弃 | 每轮都变、零信息增量,留着只会毒化尾轮与指纹 |
/// | interrupted-user | 转 `role:"user"` 原位保留 | 那本来就是用户说的话 |
/// | 其余未知 | 裹 `<system_context>` 转 user 原位保留 | 不认识就不敢丢:保内容、保位置语义 |
///
/// 空 system 消息直接丢。**无 system-role 消息时一个字节都不改**(绝大多数流量的快路径)。
///
/// gw-kiro 早有同一道处理(`converter::normalize::route_system_role_messages`,注释里
/// 写着「代理链中段注入」),cursor 通道一直没有。**没有把实现上收到 gw-core 共用**是
/// 刻意的:kiro 那份跑在 `Vec<Message>` 强类型上,共用要动 kiro 的转换管线,而 kiro 是
/// 生产主力面;这份跑在裸 `Value` 上,且多一条 kiro 不需要的规则(动态噪声里的预算
/// 计数器)。两份的分类口径若要合并,应作为一次独立改动、单独回归 kiro。
pub(crate) fn route_system_role_messages(body: &mut Value) {
    let has_system_role = body
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|msgs| {
            msgs.iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        });
    if !has_system_role {
        return; // 快路径:原样,零拷贝。
    }
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut promoted: Vec<String> = Vec::new();
    for m in msgs.drain(..) {
        if m.get("role").and_then(|r| r.as_str()) != Some("system") {
            out.push(m);
            continue;
        }
        let text = gw_core::normalize::strip_rolling_fingerprints(&extract_text(
            m.get("content").unwrap_or(&Value::Null),
        ));
        if text.trim().is_empty() {
            continue;
        }
        if is_stable_system_prefix(&text) {
            promoted.push(text);
        } else if let Some(payload) = interrupted_user_payload(&text) {
            out.push(json!({"role": "user", "content": payload}));
        } else if is_dynamic_system_noise(&text) {
            // 丢弃
        } else {
            out.push(json!({
                "role": "user",
                "content": format!("<system_context>\n{text}\n</system_context>"),
            }));
        }
    }
    *msgs = out;

    if !promoted.is_empty() {
        // 提升的文本接在既有 system 之后。**这里把 system 拍平成字符串是安全的**:
        // 本 crate 只有 `extract_system` 读它,而那本来就是拍平取文本
        // (grep `get("system")` 全仓仅一处)。
        let mut sys = body.get("system").map(extract_text).unwrap_or_default();
        for p in promoted {
            if !sys.is_empty() {
                sys.push_str("\n\n");
            }
            sys.push_str(&p);
        }
        body["system"] = Value::String(sys);
    }
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
///
/// ## 2026-08-13 第三版:为什么必须列名 + 给替代表
///
/// 生产实测(worker-cursor 12 小时):外部工具成功 **3119** 次,内建工具收口
/// **302** 次(8.8%),其中 **296 次发生在已出字之后** —— 客户端收到的是一段
/// 半截回答 + 正常的 `end_turn`,既没有错误也没有工具调用。用户的报障原话是
/// 「工具调用总失败」,看到的就是这个。落盘的 preview 里,模型要调的是
/// `echo …; grep -nE …`(shell)和「按符号名 + 目录搜代码」——**全都是调用方
/// 已经声明过的能力**(Claude Code 的 `Bash` / `Grep`),模型只是伸手抓了
/// Cursor 服务端那个同义的内建工具。
///
/// 第二版文案(「声明的工具都是真的,但不要调内建的终端/文件读写/网页搜索/
/// 代码库检索」)在这种情况下是**自相矛盾**的:调用方声明的工具就叫 `Bash`、
/// `Read`、`Edit`,能力与被点名禁掉的那几项逐字重合。模型先按第一句认下工具、
/// 写好计划,到了真要动手那一步又撞上第二句的禁令,于是去抓了名字不在禁令里
/// 的那个(内建)。
///
/// 第三版换掉「禁什么」的写法,改成三条正向信息:
/// 1. **把可用工具的全名逐个列出来** —— 闭集,比任何否定句都硬;
/// 2. **给能力→工具的替代表**(要跑命令用哪个、要读文件用哪个),按调用方
///    实际声明的名字生成 —— 模型伸手时有个明确去处,而不只是被拦住;
/// 3. 策略句(「没有的能力怎么办」)交给 [`crate::tool_guard_policy`] 从热配置取,
///    可以在后台改、当场生效,不用重新部署就能 A/B 文案。
///
/// ## 刻意**没有**写进去的东西(gpt-5.6-sol 对抗评审,2026-08-13)
///
/// - **不点名「Cursor 内建的终端 / 文件读写 / 代码库检索」**。这正是第二版翻车的那句:
///   点名的能力与调用方的 `Bash`/`Read`/`Edit` 逐字重合,会把合法工具一起吓退。
/// - **不写「否则你这一轮回答会被截断」**。后果威胁可能让模型对合法工具也变保守,
///   或者转头在正文里跟用户解释网关/Cursor 环境。它作为**热配置里的实验变体**存在,
///   不作第一版默认值 —— 「文案自相矛盾」和「Cursor 服务端 agent prompt 对内建工具
///   有更强偏置」这两个解释目前同样成立,要靠线上分桶才能分开,所以第一版只描述动作规则。
///
/// ## 结构:哪部分是代码、哪部分是配置
///
/// ```text
/// [代码] 前缀说明 + 工具闭集(按本次请求的 tools 生成)
/// [代码] 能力替代表(按本次请求的 tools 生成)
/// [热配] 策略句(默认见 crate::DEFAULT_TOOL_GUARD_POLICY)
/// ```
///
/// 动态部分的**结构**不会变,变的只是那几句自然语言 —— 所以配置里不放
/// `{tools}` / `{redirects}` 占位符:模板一旦拼错,整个闭集会被静默丢掉,
/// 而闭集恰恰是这道护栏最硬的那一半。
fn builtin_tool_guard(tools: &[run::ToolDef]) -> String {
    builtin_tool_guard_with(tools, &crate::tool_guard_policy())
}

/// [`builtin_tool_guard`] 的纯函数版:策略句由调用方给,不读进程全局。
///
/// 拆出来是为了可测:护栏文案的断言(列名、替代表、删掉哪几句)与
/// 「热配置能不能改」是两件事。读全局的话每个文案测试都要抢同一把锁,
/// 而它们**本来不关心**全局里此刻是哪一版 —— 那种耦合的表现是
/// 「单跑全绿、全量跑随机红」(2026-08-13 真踩过一次)。
fn builtin_tool_guard_with(tools: &[run::ToolDef], policy: &str) -> String {
    if tools.is_empty() {
        // 无工具的分支**不受热配置影响**:302 次内建收口全部发生在有工具的请求上,
        // 这一版文案没有需要 A/B 的东西,多一个旋钮只多一处能配错的地方。
        return "\n\n[运行环境约束]你运行在一个没有任何工具的环境里。\
                **不要**尝试调用终端/命令行、读写文件、网页搜索或代码库检索 —— 调用不会有结果。\
                直接基于已有信息回答;信息不足时说明缺什么,并让用户提供。\
                (特别地:你无法得知当前日期,也无法访问网络。)"
            .to_string();
    }

    let mut s = String::from("\n\n[运行环境]你的工具在协议里以 `");
    s.push_str(run::TOOL_NS);
    s.push_str("-` 前缀注册(声明为 `read` 的,调用名就是 `");
    s.push_str(run::TOOL_NS);
    s.push_str("-read`)。");

    // 工具闭集。按**字符预算**决定列不列,而不是按工具个数卡死 ——
    // 「第 25 个工具让闭集突然消失」是说不通的:挂了 MCP server 的复杂场景恰恰
    // 更需要闭集,而工具的完整 schema 早就比这份名单大一个数量级了。
    let names: Vec<String> = tools
        .iter()
        .map(|t| format!("`{}-{}`", run::TOOL_NS, t.name))
        .collect();
    let listed = names.join("、");
    if listed.chars().count() <= GUARD_LIST_BUDGET_CHARS {
        s.push_str("本次可用的**全部**工具就是这 ");
        s.push_str(&tools.len().to_string());
        s.push_str(" 个:");
        s.push_str(&listed);
        s.push_str("。清单之外**不存在**任何其他工具。");
    } else {
        s.push_str("本次声明的工具就是你的全部工具,清单之外**不存在**任何其他工具。");
    }

    // 能力替代表:模型真正会去抓内建工具的那几个能力,逐个指向调用方声明的同义工具。
    // 只为**确实声明了**对应工具的能力出行 —— 指向一个不存在的工具比不指更糟。
    let redirects = capability_redirects(tools);
    if !redirects.is_empty() {
        s.push_str("\n需要某项能力时用清单里的对应工具:");
        for (i, (cap, tool)) in redirects.iter().enumerate() {
            if i > 0 {
                s.push_str(";");
            }
            s.push_str(cap);
            s.push_str("调用 `");
            s.push_str(run::TOOL_NS);
            s.push('-');
            s.push_str(tool);
            s.push('`');
        }
        s.push_str("。");
    }

    // 策略句(热配置;快照由 [`builtin_tool_guard`] 取好传进来,这里不碰锁)。
    if !policy.trim().is_empty() {
        s.push('\n');
        s.push_str(policy.trim());
    }
    s
}

/// 工具闭集的字符预算(超了就不逐个列名)。
///
/// 1200 个字符 ≈ 300~400 token,大约能装 60 个 `gwtools-Xxx`。到这个量级时
/// 工具声明本身(每个都带 description + JSON Schema)已经是几十 KB,再重复一遍
/// 名单的边际收益压不住成本;而这个量级也基本只出现在挂了大 MCP server 的场景。
const GUARD_LIST_BUDGET_CHARS: usize = 1200;

/// 「模型想要的能力」→「调用方声明的同义工具裸名」。
///
/// 表里的能力就是生产日志里模型真的去抓内建工具的那几项。各家客户端命名互不相同
/// (Claude Code `Bash`/`Read`/`Grep`、opencode `bash`/`read`/`grep`、
/// Cursor 风格 `run_terminal_cmd`/`read_file`),所以要按别名表认。
///
/// ## 匹配规则刻意保守(gpt-5.6-sol 评审:宁可不出替代行,也不要指错工具)
///
/// 三档,取最紧的那个;同档取先声明的:
/// 0. 裸名(小写)与别名**全等**;
/// 1. 去掉所有非字母数字后与别名全等(`Read_File` → `readfile`);
/// 2. 以别名**开头**,且别名长度 ≥ 4(`read_file` 命中 `read`)。
///
/// **没有**纯子串档。子串会让 `Thread` 命中 `read`、`fetch_user` 命中网页工具 ——
/// 那是把「读文件请用 gwtools-Thread」这种胡话写进系统提示。代价是
/// `NotebookEdit`、`TodoWrite` 这类中缀命名认不出来,于是该能力干脆不出替代行,
/// 这是刻意选的那一侧。
///
/// 全等档同时解决另一个坑:`TodoWrite` 与 `Write` 并存时,「写/改文件」必须指向
/// `Write`(全等,0 档)而不是先声明的 `TodoWrite`。
fn capability_redirects(tools: &[run::ToolDef]) -> Vec<(&'static str, String)> {
    // (能力说法, 已知客户端的别名表)
    const CAPS: &[(&str, &[&str])] = &[
        (
            "跑命令/终端",
            &[
                "bash", "sh", "zsh", "shell", "terminal", "run_terminal_cmd", "runterminalcmd",
                "run_command", "runcommand", "execute_command", "executecommand", "exec",
            ],
        ),
        (
            "读文件",
            &["read", "read_file", "readfile", "view_file", "viewfile", "open_file", "openfile", "cat"],
        ),
        (
            "写/改文件",
            &[
                "write", "write_file", "writefile", "edit", "edit_file", "editfile", "multiedit",
                "str_replace_editor", "strreplaceeditor", "apply_patch", "applypatch", "patch",
            ],
        ),
        (
            "搜代码/找文件",
            &[
                "grep", "glob", "rg", "ripgrep", "search", "codebase_search", "codebasesearch",
                "grep_search", "grepsearch", "file_search", "filesearch", "find",
            ],
        ),
        (
            "查网页",
            &[
                "websearch", "web_search", "webfetch", "web_fetch", "fetch", "browse", "browser",
            ],
        ),
    ];
    let mut out = Vec::new();
    for (cap, aliases) in CAPS {
        let best = tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let lower = t.name.to_ascii_lowercase();
                let squashed: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
                aliases
                    .iter()
                    .filter_map(|a| {
                        if lower == *a {
                            Some(0u8)
                        } else if squashed == *a {
                            Some(1)
                        } else if a.len() >= 4 && lower.starts_with(a) {
                            Some(2)
                        } else {
                            None
                        }
                    })
                    .min()
                    .map(|tightness| (tightness, i, &t.name))
            })
            .min();
        if let Some((_, _, name)) = best {
            out.push((*cap, name.clone()));
        }
    }
    out
}

/// 内建工具调用帧 → 它想要的**能力说法**(与 [`capability_redirects`] 的 key 同一套词)。
///
/// 身份是字段号(见 [`run::builtin_tool_ident`]),只有抓包实证过的两个认得出:
/// `.1` 终端命令、`.4` 读文件。其余返回 `None` —— 那意味着「知道模型调了内建工具、
/// 但不知道调的是哪个」,此时纠偏只能给通用话术,不能指名工具。
///
/// ⚠️ 不要凭字段号相邻就往上加映射。猜错的后果是下一轮纠偏对着模型说
/// 「你上次调了内建终端」而它其实调的是代码检索 —— 一句自信的错话比一句模糊的
/// 实话更容易把模型带偏。枚举要靠 `CURSOR_DUMP_TOOL_FRAMES` 抓实物定死。
fn builtin_capability(payload: &[u8]) -> Option<&'static str> {
    match run::builtin_tool_ident(payload)? {
        1 => Some("跑命令/终端"),
        4 => Some("读文件"),
        _ => None,
    }
}

/// 内建调用 → 调用方声明工具的兼容转换表(2026-08-13)。
///
/// 收口的替代:模型抓了无前缀的内建工具(`Bash` 而不是 `gwtools-Bash`)时,
/// 把这次调用翻译成调用方**声明过的**同义工具的标准 `tool_use` 下发 ——
/// 客户端照常执行并在下一轮回传 tool_result,「半截回答 + 静默截断」变成
/// 无感成功。生产 12h 内建收口 302 次,终端/读文件是绝对大头。
///
/// 表在请求开头按本次声明的 `tools` 生成一次:
/// - 工具名复用 [`capability_redirects`] 的保守匹配(全等 > 去符号全等 > 前缀);
/// - **参数键名从该工具的 `input_schema.properties` 里认**(Claude Code 是
///   `command`/`file_path`,opencode 是 `command`/`filePath`),候选键都不在
///   schema 里就放弃翻译 —— 猜键名的后果是客户端执行一个参数张冠李戴的调用,
///   比收口糟得多。翻译失败一律落回收口 + 纠偏,风险单向。
#[derive(Debug, Clone, Default)]
struct BuiltinXlate {
    /// 终端命令 → `(声明工具裸名, 参数键名)`,如 `("Bash", "command")`。
    terminal: Option<(String, String)>,
    /// 读文件 → 同上,如 `("Read", "file_path")`。
    read_file: Option<(String, String)>,
}

impl BuiltinXlate {
    fn from_tools(tools: &[run::ToolDef]) -> Self {
        let redirects = capability_redirects(tools);
        let find = |cap: &str, keys: &[&str]| -> Option<(String, String)> {
            let tool = redirects
                .iter()
                .find(|(c, _)| *c == cap)
                .map(|(_, t)| t.clone())?;
            let def = tools.iter().find(|t| t.name == tool)?;
            let schema: Value = serde_json::from_str(&def.schema).ok()?;
            let props = schema.get("properties")?.as_object()?;
            let key = keys.iter().find(|k| props.contains_key(**k))?;
            Some((tool, (*key).to_string()))
        };
        BuiltinXlate {
            terminal: find("跑命令/终端", &["command", "cmd", "script"]),
            read_file: find(
                "读文件",
                &["file_path", "filePath", "path", "target_file", "absolute_path"],
            ),
        }
    }
}

/// 把一次已解出参数的内建调用翻译成调用方工具的 [`run::ToolCall`]。
/// 转换表里没有对应工具 → `None`(调用方落回收口 + 纠偏)。
fn translate_builtin(bc: run::BuiltinCall, x: &BuiltinXlate) -> Option<run::ToolCall> {
    let (id, name, key, val) = match bc {
        run::BuiltinCall::Terminal { id, command } => {
            let (name, key) = x.terminal.clone()?;
            (id, name, key, command)
        }
        run::BuiltinCall::ReadFile { id, path } => {
            let (name, key) = x.read_file.clone()?;
            (id, name, key, path)
        }
    };
    // 空 id 兜底与 parse_tool_call 同款:我方从不把 call id 发回上游
    // (每轮都是全新的 Opening 请求),合成是安全的。
    let id = if id.trim().is_empty() {
        format!("call_{}", uuid::Uuid::new_v4().simple())
    } else {
        id
    };
    Some(run::ToolCall {
        id,
        name,
        args: vec![(key, Value::String(val))],
    })
}

/// 上一轮被内建工具截断时,注入本轮用户消息的纠偏话术。
///
/// ## 为什么需要它:模型**收不到**失败信号
///
/// 模型调内建工具 → 我们当场关流。它不知道自己失败了,本轮就结束了。所以
/// 「失败了就换个工具再试」这句话写进系统提示是空转 —— 没有任何触发时机。
/// 生产实测同一个会话会**反复**撞同一面墙(12h 内 302 次收口)。
///
/// 协议内的正解是在同一条 BiDi 流里回一个「工具不可用」的失败结果,让模型在**本轮内**
/// 自己纠偏。但那要求先定死 `1.2.2.<N>` 的身份枚举与请求侧回执消息形状 ——
/// 猜字段号的后果是回执被上游忽略、退化成 90s 心跳死等,比现在的瞬间收口更差。
/// 所以在抓包定死之前,先走这条零协议风险的路:**下一轮**告诉它上次失败了。
///
/// 救不了当前那一轮,但能止住重复 —— 这是用户明确要的那半件事
/// (「还是需要告诉模型你工具调用失败了」)。
///
/// 杠杆与 [`fold_history`] 的 `[继续]` 同源(已在工具回路发散那次实证过)。
fn truncation_notice(cap: Option<&str>, tools: &[run::ToolDef]) -> String {
    let mut s = String::from(
        "\n\n[上一轮中断]你上一轮调用了本环境**不提供**的 Cursor 内建工具,\
         那次调用没有任何结果,你的回答也在那里被截断了(用户只看到半句话)。",
    );
    // 认得出能力、且调用方确实声明了同义工具 → 指名换哪个。这是最有用的一句。
    let redirect = cap.and_then(|c| {
        capability_redirects(tools)
            .into_iter()
            .find(|(k, _)| *k == c)
            .map(|(_, tool)| tool)
    });
    match redirect {
        Some(tool) => {
            s.push_str("请改用 `");
            s.push_str(run::TOOL_NS);
            s.push('-');
            s.push_str(&tool);
            s.push_str("` 重做刚才那一步,然后继续完成用户的请求。");
        }
        None => {
            s.push_str(
                "请只用本次声明的工具重做刚才那一步(调用名带 `gwtools-` 前缀);\
                 声明的工具里没有能干这件事的,就直接告诉用户你需要什么。",
            );
        }
    }
    s
}

/// 调用方历史的逐轮指纹(CLI 形态的分叉检测,见 `ConvRegistry::cli_lookup`)。
///
/// 指纹取 `to_turns` 的逐条消息(折叠之前),与 cache_sim 同一稳定语义层:
/// 折叠形态里闭合标签横在中间,跨轮不再是字节前缀。
pub fn history_fps(body: &Value) -> Vec<u64> {
    to_turns(body)
        .iter()
        .map(|t| {
            let mut h = Sha256::new();
            h.update([if t.is_user { b'u' } else { b'a' }]);
            h.update(t.text.as_bytes());
            let d = h.finalize();
            u64::from_be_bytes(d[..8].try_into().unwrap())
        })
        .collect()
}

/// CLI 驱动(子进程模式)能接的请求形态:末轮是 user 即可(含 tool_result
/// 接续轮;工具声明/图片/PDF 都由 CLI 侧原生处理)。assistant 结尾(prefill)
/// 不支持,回线协议形态。
pub(crate) fn cli_eligible(body: &Value) -> bool {
    to_turns(body).last().is_some_and(|t| t.is_user)
}

/// 取「调用方本轮新加的内容」:最后一条 assistant 之后的所有 user 轮,按序拼接。
///
/// CLI 驱动只把这一段当 prompt 发出去(历史在 CLI 会话里,不重铺),所以这个"一段"
/// 取错就等于把用户的话吞了。**为什么不是 `turns.last()`**:中转会在用户消息之后
/// 再追注入消息(见 [`route_system_role_messages`]),`last()` 拿到的是注入的那条。
/// 分流器已经把已知的注入形态处理掉了,但它的兜底分支是「不认识就裹 `<system_context>`
/// 转 user 原位保留」—— 那种未知注入照样会落在尾巴上。取整段而不是取末条,
/// 这一类注入就只是让 prompt 多一段说明,而不是把用户的话整条替换掉。
///
/// 正常形态(末轮就是一条用户消息)下与 `last()` **逐字节相同**。
pub(crate) fn latest_user_input(turns: &[Turn]) -> String {
    let start = turns.iter().rposition(|t| !t.is_user).map_or(0, |i| i + 1);
    let mut out = String::new();
    for t in &turns[start..] {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&t.text);
    }
    out
}

/// 取「整条都是 tool_result」的最后一条 user 消息:(tool_use_id, 文本) 列表。
///
/// 只认这个严格形态(Anthropic 客户端工具回路的真实形状):最后一条消息是 user、
/// 内容块**全部**是 tool_result 且都带 tool_use_id。不满足就返回 None(走重铺)。
/// id 必须返回:CLI 驱动的桥接续按它键控消费挂起槽,错配注入是静默语义损坏。
pub(crate) fn last_tool_results(body: &Value) -> Option<Vec<(String, String)>> {
    let msgs = body.get("messages")?.as_array()?;
    let last = msgs.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    let blocks = last.get("content")?.as_array()?;
    if blocks.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            return None;
        }
        let id = b.get("tool_use_id")?.as_str()?.to_string();
        out.push((id, extract_text(b.get("content")?)));
    }
    Some(out)
}

/// 消息里是否含 tool_use / tool_result 块。
///
/// CLI 形态 Phase 1 不接工具回路(工具调用历史文本化与服务端持史冲突,
/// 正解是 live-stream 桥接,见 PROTOCOL §20 的规划):带这些块的请求回 IDE 形态。
fn body_has_tool_blocks(body: &Value) -> bool {
    body.get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|msgs| {
            msgs.iter().any(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|bs| {
                        bs.iter().any(|b| {
                            matches!(
                                b.get("type").and_then(|t| t.as_str()),
                                Some("tool_use") | Some("tool_result")
                            )
                        })
                    })
            })
        })
}

/// Anthropic 请求体 → Run 的会话轮次列表。
///
/// ⚠️ **system 不在这里。** 它的家是 `1.2.17.9.25`(抓包实物证实),由
/// [`extract_system`] 单独取出后交给 [`run::build_frame0`]。早先的实现把 system
/// 折进第一条用户消息 —— 那会让模型把系统指令当成用户说的话。
pub fn to_turns(body: &Value) -> Vec<Turn> {
    to_turns_with_media(body, None)
}

/// [`to_turns`] 的占位感知版。`media` 是 [`to_media`] 同一次扫描产出的占位表:
/// 带表时,tool_result **内嵌**且被收进附件的图片/文档在文本流原位渲染成
/// `[图片见附件 attach-N]`,编号与附件顺序天然一致。不带表 = 旧行为。
/// 生产的 cache_sim 指纹取的是带占位的 raw_turns —— 占位文本由消息内容
/// 决定、跨轮稳定,逐消息指纹仍逐轮一致,模拟缓存不受影响。
fn to_turns_with_media(body: &Value, media: Option<&MediaPlaceholders>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for (mi, m) in msgs.iter().enumerate() {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = extract_text_in_msg(
                m.get("content").unwrap_or(&Value::Null),
                media.map(|p| (mi, p)),
            );
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

/// [`to_media`] 的占位表:`(消息下标, 顶层块下标, tool_result 内嵌块下标)` → 占位文本。
///
/// 只登记 tool_result **内嵌**且真的被收进附件的媒体块。文本渲染
/// ([`extract_text_in_msg`])按同一坐标在原位补占位 —— 占位表与附件出自
/// [`to_media`] 的同一次扫描,编号不可能与附件顺序漂移。
type MediaPlaceholders = std::collections::HashMap<(usize, usize, usize), String>;

/// 校验并收下**一个** image / document 块 —— 顶层块与 tool_result 内嵌块共用的
/// 单块逻辑。限额 / base64 校验 / 跳过口径必须两处一致,拆开写迟早不同步。
/// 收下返回 true;非 image/document、非 base64 源、超限、解码失败一律跳过返回 false。
fn collect_media_block(
    b: &Value,
    images: &mut Vec<run::ImageAttachment>,
    docs: &mut Vec<run::DocAttachment>,
    budget: &mut usize,
) -> bool {
    use base64::Engine as _;
    let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "image" && kind != "document" {
        return false;
    }
    let src = b.get("source");
    let stype = src.and_then(|s| s.get("type")).and_then(|t| t.as_str());
    if stype != Some("base64") {
        tracing::warn!(kind, ?stype, "cursor: 非 base64 的媒体源,跳过(不代为下载)");
        return false;
    }
    let mime = src
        .and_then(|s| s.get("media_type"))
        .and_then(|t| t.as_str())
        .unwrap_or(if kind == "image" { "image/png" } else { "application/pdf" })
        .to_string();
    let Some(data) = src.and_then(|s| s.get("data")).and_then(|d| d.as_str()) else {
        return false;
    };
    // 解码**之前**先按 base64 长度估原始大小,别先解出 200MB 再判超限。
    // base64 是 4:3,估值只用于挡明显超标的,精确判断在解码后。
    if data.len() / 4 * 3 > MAX_ONE_ATTACHMENT {
        tracing::warn!(kind, b64_len = data.len(), "cursor: 附件超单个上限,跳过");
        return false;
    }
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(data) else {
        tracing::warn!(kind, "cursor: base64 解码失败,跳过该附件");
        return false;
    };
    if raw.len() > MAX_ONE_ATTACHMENT || *budget + raw.len() > MAX_ALL_ATTACHMENTS {
        tracing::warn!(
            kind, bytes = raw.len(), budget = *budget,
            "cursor: 附件超上限(单个或累计),跳过 —— 无上限时一个大附件能打爆整个 worker"
        );
        return false;
    }
    *budget += raw.len();
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
    true
}

/// 从 Anthropic 请求里收集图片与文档附件。
///
/// Anthropic 侧:
/// - 图片 `{type:"image", source:{type:"base64", media_type, data}}`
/// - 文档 `{type:"document", source:{type:"base64", media_type:"application/pdf", data}}`
/// - **工具返回的图/文档**:同样的块,嵌在 `tool_result.content` 数组里。
///
/// 两者去处不同(见 [`run::Media`])。`source.type != "base64"`(如 `url`)一律跳过 ——
/// 替调用方去下载 URL 是我方主动出网,不该悄悄做。
///
/// 第三个返回值是 tool_result 内嵌媒体的占位表(见 [`MediaPlaceholders`])。
/// 曾经这里只扫顶层块,tool_result 内嵌的 image 直接 continue 掉;而
/// extract_text 的注释声称「媒体走 to_media 内联」—— 两边互相推诿,agent 用
/// Read 工具读的图片就静默消失、日志一条不留(2026-08-13 生产事故)。
pub(crate) fn to_media(body: &Value) -> (Vec<run::ImageAttachment>, Vec<run::DocAttachment>, MediaPlaceholders) {
    let mut images = Vec::new();
    let mut docs = Vec::new();
    let mut placeholders = MediaPlaceholders::new();
    // 已收下的附件原始字节总量,用来卡总闸(见 MAX_ONE_ATTACHMENT / MAX_ALL_ATTACHMENTS)。
    // 顶层块与 tool_result 内嵌块**共用同一个预算**,内嵌不是绕开限额的后门。
    let mut budget: usize = 0;
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return (images, docs, placeholders);
    };
    for (mi, m) in msgs.iter().enumerate() {
        let Some(blocks) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, b) in blocks.iter().enumerate() {
            let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind == "tool_result" {
                // content 为字符串时没有媒体;为块数组时逐块按顶层同款口径收。
                let Some(nested) = b.get("content").and_then(|c| c.as_array()) else {
                    continue;
                };
                for (ni, n) in nested.iter().enumerate() {
                    let is_image =
                        n.get("type").and_then(|t| t.as_str()) == Some("image");
                    if collect_media_block(n, &mut images, &mut docs, &mut budget) {
                        // 占位文本里的编号与 run.rs `ImageAttachment::encode(seq)`
                        // 合成的路径 `attach-{seq}-…` 同源(都是 images 向量下标)。
                        let ph = if is_image {
                            format!("[图片见附件 attach-{}]", images.len() - 1)
                        } else {
                            format!(
                                "[文档见 {}]",
                                docs.last().map(|d| d.path.as_str()).unwrap_or("")
                            )
                        };
                        placeholders.insert((mi, bi, ni), ph);
                    }
                }
            } else {
                collect_media_block(b, &mut images, &mut docs, &mut budget);
            }
        }
    }
    if !images.is_empty() || !docs.is_empty() {
        // info 级:生产排查「图有没有收进来」曾经只能靠猜(收不到时零日志)。
        tracing::info!(
            images = images.len(),
            docs = docs.len(),
            from_tool_result = placeholders.len(),
            "cursor: 已收集请求附件"
        );
    }
    (images, docs, placeholders)
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
pub(crate) fn to_tools(body: &Value) -> Vec<run::ToolDef> {
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

/// 增量历史模式的开关。**默认关,而且已被实测否决 —— 不要开。**
///
/// ## 2026-08-15 真号实测:开了就丢历史
///
/// 稳定 `metadata.user_id` 连发三轮(①记住 4712 ②聊件无关的事 ③问那个数字),
/// 同一构建、只翻这个开关:
///
/// | `CURSOR_DELTA_HISTORY` | 第三轮回答 |
/// |---|---|
/// | `1`(只发本轮) | 「这次对话里我没收到过你要我记住的数字」❌ |
/// | `0`(内联全量) | 「4712」✅ |
///
/// 结论:**服务端在我方这种请求形态下并不持有历史。** 模型的记忆百分之百来自
/// 我们内联的那份文本。
///
/// ## 那 `PROTOCOL-agent-run.md` §17.3 的「历史确实在服务端」是怎么来的
///
/// 那个实验有混淆变量:它断言「两个事实跨 4 轮全部答对 → 历史在服务端」,但当时
/// 请求里**同时还内联着全量历史**([`fold_history`] 无条件生效)。答对完全可以只是
/// 因为历史被贴给了模型。上面这次是第一次把内联那份拿掉的干净检验。
///
/// 于是 §17.3 的 98.7% 命中与「新算 token 降到两位数」要重新归因:收益来自
/// **内联文本被上游前缀缓存**,不是来自服务端持有历史。两个机制被混成了一个。
///
/// 真客户端能只发 2121B,靠的是 `1.2.17` + FileSyncService 上传的 4 个 blob 哈希;
/// §17.2 已实测我方走不了那条路(带哈希被拒,不带哈希则服务端静默等一个永不到来的 blob)。
///
/// 代码保留是为了把这次测量连同反例一起留在仓库里(测试锁住行为),
/// **不是留着等人打开**。要重开必须先在协议层拿到「服务端真的替我方存了历史」的直接证据。
fn delta_history_enabled() -> bool {
    gw_core::env_flag("CURSOR_DELTA_HISTORY")
}
/// **只发本轮新消息** —— 前提是服务端已持有历史,而 2026-08-15 实测**它并没有**。
/// 见 [`delta_history_enabled`] 的实测表:开这条路会丢历史。**别开。**
///
/// ## 它原本要解决什么(问题仍然存在,只是这个解法不成立)
///
/// [`fold_history`] 把整段对话渲染成**一条**用户消息
/// (`<conversation_history>User: … Assistant: …</conversation_history>` + 本轮)。
/// 上游看到的不是结构化多轮,而是「一个人在一条消息里自问自答了一整段,末尾又问了一句」。
/// 这个形状是 grok 反复重述/绕圈的怀疑对象。
///
/// 本函数试的方向是「声明续写就只发新东西」,与真客户端的**上传字节**行为一致
/// (§12:turn1 98858B → turn2 2121B)。实测否决:真客户端靠 blob 哈希接上前文,
/// 我方拿不到,声明了续写服务端也不会替我们记住。
///
/// 所以「一坨长文本」这个形状目前**没有替代品** —— 要改只能改渲染本身(分隔符、
/// 角色标记、把历史挪进 `1.2.1.2` 上下文块的独立条目),而不是不发。
///
/// ## 「本轮」的边界
///
/// 与 `fold_history` 用同一个锚:**最后一条 user 轮**及其之后的内容(尾随 assistant 是
/// prefill,丢掉会改变语义)。它之前的全部视为服务端已持有的历史,不再重传。
///
/// `task`(工具回路中间轮的用户原始请求复述)照旧附上 —— 那条杠杆治的是
/// 「本轮只有工具返回、一个问题都没有」,与历史在谁手里无关。
fn delta_history(turns: &[Turn], task: Option<&str>) -> Vec<Turn> {
    let Some(last) = turns.iter().rposition(|t| t.is_user) else {
        // 没有 user 轮:退回折叠(它对这种畸形输入有兜底),绝不返回空
        // —— `build_frame0` 对空 turns 是 assert,而且空请求上游只回心跳。
        return fold_history(turns, task);
    };
    let mut buf = String::new();
    buf.push_str(&turns[last].text);
    for t in &turns[last + 1..] {
        buf.push_str("\n\n");
        buf.push_str(&t.text);
    }
    if let Some(task) = task {
        buf.push_str(TOOL_LOOP_NUDGE);
        buf.push_str(task);
    }
    vec![Turn { text: buf, is_user: true }]
}

/// 工具回路中间轮补的那句提醒(折叠与增量两条路共用,避免两处措辞漂移)。
const TOOL_LOOP_NUDGE: &str = "\n\n[继续]上面是你上一步请求的工具结果,已经拿到了。\
     请**据此**继续完成用户的请求,不要重复调用已经返回结果的工具。\
     还缺信息时才调别的工具;信息够了就直接给出最终回答。\
     (下面这句原始请求只是提醒你目标,你多半已经回应过它 —— \
     不要再重复致意或确认,直接继续干活。)\n用户的请求是:";

/// 把多轮历史折成**一条**用户消息(服务端**没有**历史时的兜底形态)。
///
/// ## 为什么必须折
///
/// 实测:请求里只要有多于一条 `1.2.1`,上游就 200 接受、然后永远只发心跳
/// (与缺 `1.2.1.2` 时一模一样的静默挂起)。真客户端**从来只发一条** ——
/// 它的历史在服务端。所以「repeated `1.2.1` 携带历史」这条路不存在。
///
/// Anthropic 客户端每次都重传全量历史,我们只能把它渲染成一段文字塞进当前这轮。
/// 代价是上游看到的是「一条很长的用户消息」而不是结构化对话,但**模型确实读得到**,
/// 这比静默挂起好得多。要真正避开这个形状只能让服务端持有历史,见 [`delta_history`]。
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
pub(crate) fn fold_history(turns: &[Turn], task: Option<&str>) -> Vec<Turn> {
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
        buf.push_str(TOOL_LOOP_NUDGE);
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
    // ⚠️ `role=="system"` 必须跳过。这个函数由 worker 在 `Provider::chat` **之前**
    // 用原始 body 调用,那时 [`route_system_role_messages`] 还没跑过。中转注入的
    // system 消息若排在首位,锚点就会取到它 —— 而注入内容往往每轮都变
    // (预算计数器),亲和键跟着每轮都变,等于没有亲和。
    let first_user = msgs.iter().find(|m| {
        !matches!(
            m.get("role").and_then(|r| r.as_str()),
            Some("assistant") | Some("system")
        )
    })?;
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

/// CLI 形态的请求头(2026-08-16 抓包实物,cursor-agent 2026.08.11)。
///
/// 与 IDE 形态的全部差异:
/// - **没有** checksum / machineId / session-id / client-key / config-version /
///   fs-client-key / timezone / amzn-trace-id / cookie —— 也不需要 GetServerConfig 握手;
/// - `x-cursor-client-type: cli`,`x-cursor-client-version: cli-<版本>`;
/// - `x-ghost-mode: false`(IDE 是 true);
/// - 两条 traceparent **同值**且 flags 都是 `-01`(IDE 是一 `-00` 一 `-01`);
/// - `accept-encoding: gzip,br`(IDE 只有 gzip);
/// - `x-original-request-id` == `x-request-id` == 帧0 的 `1.25`(首个逻辑尝试;
///   真 CLI 重试时 x-request-id 换新、另两个不变,我方每个网关请求都是新尝试)。
fn apply_cli_headers(
    rb: reqwest::RequestBuilder,
    ctx: &RunCtx,
    request_id: &str,
) -> reqwest::RequestBuilder {
    let tp = wire::traceparent();
    rb.header("connect-protocol-version", "1")
        .header("connect-accept-encoding", "gzip,br")
        .header("connect-content-encoding", "gzip")
        .header("content-type", "application/connect+proto")
        .header("user-agent", wire::USER_AGENT)
        .header("authorization", format!("Bearer {}", ctx.token))
        .header(
            "x-blob-encryption-key",
            session_key(&ctx.token, &ctx.conversation_id, "blob"),
        )
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", crate::cli::CLI_CLIENT_VERSION)
        .header("x-ghost-mode", "false")
        .header("x-request-id", request_id)
        .header("x-original-request-id", request_id)
        .header("traceparent", &tp)
        .header("backend-traceparent", &tp)
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
    //
    // ⚠️ Claude Code 的 `adaptive` **必须算「要思考」**:只认 `enabled` 时上游仍开
    // thinking(目录默认 true),收侧却丢掉 `1.4` —— 思考帧还刷新 last_progress,
    // 90s 心跳 watchdog 进不去,客户端干等直到 gw-app 300s idle abort(无首字)。
    let thinking = req.body.get("thinking").cloned();
    let want_thinking = client_wants_thinking(thinking.as_ref());

    // 未知模型名直接 400,绝不静默归一成 `default`(交接规格第 8 条危险点):
    // 旧行为叠加含 default 的白名单时,任意拼错的模型名都会放行,客户端
    // **静默拿到另一个模型**。已知家族的有意降级(目录外 haiku→composer、
    // gemini→default)不受影响 —— None 只给「任何家族都不沾边」的真未知。
    let Some(cursor_model) = crate::models::resolve_cursor_model(&req.model) else {
        // bad_request_visible:客户端必须看到原文(知道自己拼错了什么才改得了)。
        // 文案我方完全掌控、不含上游响应体/账号标识,符合该构造器的准入条件。
        return Err(UpstreamError::bad_request_visible(format!(
            "cursor: 未知模型名 {:?} —— 不在模型目录且不属于任何已知家族(claude/gpt/grok/gemini/kimi/glm)。\
             用 GET /v1/models 查可用模型",
            req.model
        )));
    };
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
    // 附件收集在文本渲染**之前**:to_media 的同一次扫描产出 tool_result 内嵌
    // 媒体的占位表,to_turns_with_media 按它在文本原位补 `[图片见附件 attach-N]`,
    // 编号与附件收集顺序同源,不存在两边各数各的漂移。
    let (images, docs, media_placeholders) = to_media(&req.body);
    let raw_turns = to_turns_with_media(&req.body, Some(&media_placeholders));
    let tools = to_tools(&req.body);

    // ── CLI 形态(2026-08-16 抓包,见 `cli.rs` 模块文档)─────────────────────
    // 纯文本、无工具、无附件的请求走 CLI 极简形态:只发最后一条新消息,
    // 历史由服务端持有(实测跨进程记得,且上游真缓存命中回来)。
    // Phase 1 不接工具回路/附件:带这些的请求维持 IDE 形态(生产在跑的那条)。
    let cli_mode = ctx.profile.is_cli()
        && tools.is_empty()
        && images.is_empty()
        && docs.is_empty()
        && !body_has_tool_blocks(&req.body)
        && raw_turns.last().is_some_and(|t| t.is_user);

    // 服务端已持有本会话历史(Continuation)且增量模式打开 → 只发本轮新消息。
    // 否则照旧折叠成一条(见 `fold_history` / `delta_history`)。
    let turns = if cli_mode {
        if ctx.phase.is_continuation() {
            // 只发最后一条新消息(gate 已保证它是 user 轮)。
            vec![raw_turns.last().cloned().expect("cli gate 保证非空")]
        } else if raw_turns.len() == 1 {
            raw_turns.clone()
        } else {
            // 重铺(新会话/历史分叉):Phase 1 先折叠进首条消息,与线上同口径。
            // TODO(§20):CLI 形态的原生多条 1.2.1 重铺还没做消融实验,证实可行后换掉。
            fold_history(&raw_turns, task.as_deref())
        }
    } else if delta_history_enabled() && ctx.phase.is_continuation() {
        delta_history(&raw_turns, task.as_deref())
    } else {
        fold_history(&raw_turns, task.as_deref())
    };
    let system = {
        let mut sys = extract_system(&req.body);
        sys.push_str(&builtin_tool_guard(&tools));
        sys
    };

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
    // 上一轮被内建工具截断过 → 在本轮用户消息末尾补一句纠偏(取走即消费)。
    //
    // 放在**最后**:模型对轮末的指令最敏感,而这句话要盖过它上一轮的行为惯性。
    // 也放在 `[继续]` 之后 —— 那句说的是「工具结果已拿到」,这句说的是
    // 「上次那个工具根本不存在」,后者是更强的纠正。
    //
    // 不影响缓存指纹:`sim_fps` 取的是 `raw_turns`(折叠之前),见下方注释。
    let turns = match ctx.notices.take(&ctx.conversation_id) {
        Some(cap) => {
            tracing::info!(
                conversation_id = %ctx.conversation_id,
                cap = ?cap,
                "cursor:上一轮被内建工具截断,本轮注入纠偏"
            );
            let notice = truncation_notice(cap, &tools);
            let mut t = turns;
            match t.iter().rposition(|x| x.is_user) {
                Some(i) => t[i].text.push_str(&notice),
                // 理论上不会:fold_history 恒返回一条 user 轮。真出现就丢掉这句,
                // 绝不把纠偏话术塞进 assistant 轮(那会变成「模型自己说过的话」)。
                None => tracing::warn!("cursor:没有 user 轮可挂纠偏,本轮跳过"),
            }
            t
        }
        None => turns,
    };

    let media = run::Media { images: &images, docs: &docs };
    if turns.is_empty() {
        return Err(UpstreamError::bad_request("cursor: 请求里没有任何消息"));
    }

    // Prefix 缓存命中模拟(与 kiro 同一家口径,见 [`crate::cache_sim`] 模块注释):
    // 上游 `1.14` 几乎从不报缓存命中,客户侧计费要与 kiro 通道对齐只能靠模拟。
    // 键 = 账号 + 会话(服务端会话 per-account,换号即冷启动);`\x1f` 分隔不会撞:
    // account_id 过 validate_account_id(只允许字母数字与 -_.~),conversation_id
    // 是 UUID,两边都不可能有控制字符。指纹取**稳定语义层**(to_turns 的逐条消息,
    // fold_history 之前)——折叠形态里闭合标签横在中间,第二轮就不再是上一轮的
    // 字节前缀,而逐消息指纹跨轮逐字节稳定。两步走:这里 peek 拿计费值;
    // **成功收尾才 commit**(失败轮服务端没落下这一轮,指纹不能进模拟表 ——
    // 与 ConvRegistry 的 forget 语义一致),所以把 commit 包进 outcome 回调。
    let sim_key = format!("{}\x1f{}", ctx.account_id, ctx.conversation_id);
    let sim_fps =
        crate::cache_sim::fingerprints_from_context(&system, &tools, &raw_turns, est_text_tokens);
    let (sim, sim_gen) = crate::cache_sim::peek(&sim_key, &req.model, &sim_fps);
    let sim_cache_read = sim.cache_read_tokens as u64;
    let outcome = outcome.map(|h| {
        let key = sim_key.clone();
        let model = req.model.clone();
        let fps = sim_fps.clone();
        // 先 commit 再通知调用方:两方看到的是同一份终态。commit 带代际 CAS:
        // 同会话并发请求后完成的那个会被拒(宁可少记一轮,不乱序覆盖)。
        // 注意内建工具截断路径同样走 h(true):截的是**响应**,请求轮本身服务端
        // 已处理(模型已出字),ConvRegistry 在同路径 confirm 是既有语义,
        // 模拟缓存与它保持一致,不另开三态。
        std::sync::Arc::new(move |ok: bool| {
            if ok {
                crate::cache_sim::commit(&key, &model, fps.clone(), sim_gen);
            }
            h(ok);
        }) as OutcomeHook
    });

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let url = format!("https://{}/agent.v1.AgentService/Run", ctx.host);

    // CLI 形态与 IDE 形态分岔:帧序列与头表完全不同(见 `cli.rs` 模块文档)。
    let (frames, rb) = if cli_mode {
        // turn_id 同时是 `1.25` 与 x-request-id/x-original-request-id ——
        // 真 CLI 里这三个值同源(重试时 x-request-id 换新、1.25 与 original 不变;
        // 我方每个网关请求都是新的逻辑尝试,全部同值即可)。
        let turn_id = uuid::Uuid::new_v4().to_string();
        let opening = ctx.phase.is_opening();
        // 预算表的 conversation 节 = 历史总字符(本轮新消息不计)。
        let history_chars: usize = if opening {
            0
        } else {
            raw_turns[..raw_turns.len().saturating_sub(1)]
                .iter()
                .map(|t| t.text.chars().count())
                .sum()
        };
        let frame0 = crate::cli::build_frame0_cli(
            &turns[0].text,
            &model,
            &catalog,
            &ctx.conversation_id,
            &turn_id,
            &ctx.timezone,
            now_ms,
            opening,
            (system.chars().count(), history_chars),
            "file:///",
        );
        let context = crate::cli::build_context_frame_cli(
            &system,
            &ctx.token,
            &ctx.conversation_id,
            &ctx.timezone,
            "/",
        );
        if let Ok(dir) = std::env::var("CURSOR_DUMP_REQ") {
            let f = format!("{dir}/cli_frames_{}.bin", uuid::Uuid::new_v4().simple());
            let _ = std::fs::write(&f, [frame0.as_slice(), b"\n----\n", &context].concat());
            tracing::warn!(file = %f, "已落盘 CLI 帧0+上下文帧 payload");
        }
        tracing::debug!(
            phase = ?ctx.phase,
            frame0_bytes = frame0.len(),
            context_bytes = context.len(),
            "cursor Run 请求已构造(CLI 形态)"
        );
        let payloads = crate::cli::cli_request_frames(&frame0, &context);
        let mut frames = Vec::with_capacity(payloads.len());
        for (p, compress) in &payloads {
            // 逐帧按真包决定是否 gzip(首轮帧0 不压,两个大帧压,小帧裸发)。
            if *compress {
                frames.push(wire::frame_compressed(p).map_err(|e| {
                    UpstreamError::new(UpstreamErrorKind::Other, format!("gzip 请求帧失败: {e}"))
                })?);
            } else {
                frames.push(wire::frame(p));
            }
        }
        (frames, apply_cli_headers(client.post(&url), &ctx, &turn_id))
    } else {
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
        (frames, apply_headers(client.post(&url), &ctx))
    };

    // 保持请求流打开时,body 走一个 channel:初始帧先灌进去,发送端在响应读完前
    // 一直不 drop,于是 HTTP/2 的请求流不会 half-close —— 与真 BiDi 客户端一致。
    let (rb, body_keepalive) = if ctx.keep_stream_open {
        // 容量要装下初始帧全集(CLI 形态 10 帧),否则 try_send 会静默丢帧。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
        for f in frames {
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
    // tool_use 收口时常没有上游 `1.14`,用请求体预估一版兜底(见 estimate_usage_fallback)。
    // output_chars 在流里才知道,这里先按 0 估输入/缓存;收尾处会用真实 output 覆盖。
    let usage_fallback = estimate_usage_fallback(&system, &turns, &tools, 0);
    Ok(Box::pin(stream_to_anthropic(
        resp.bytes_stream(),
        model_name,
        want_thinking,
        body_keepalive,
        outcome,
        ctx.conversation_id.clone(),
        ctx.assets.clone(),
        ctx.notices.clone(),
        BuiltinXlate::from_tools(&tools),
        usage_fallback,
        sim_cache_read,
        cli_mode,
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
    // 服务端 write/read 调用要用:资产按会话存取(见 [`crate::AssetStore`])。
    conversation_id: String,
    assets: std::sync::Arc<crate::AssetStore>,
    // 内建工具收口时往这里记一笔,下一轮同会话请求据此注入纠偏
    // (见 [`truncation_notice`] 与 [`crate::TruncationNotices`])。
    notices: std::sync::Arc<crate::TruncationNotices>,
    // 内建调用 → 声明工具的兼容转换表(按本次请求的 tools 生成,见 [`BuiltinXlate`])。
    // 认得出的内建调用翻译成标准 tool_use 下发;认不出的落回收口 + 纠偏。
    xlate: BuiltinXlate,
    // 上游没给 `1.14` 时的用量兜底(见 [`estimate_usage_fallback`])。
    usage_fallback: ChatUsage,
    // 模拟的 cache_read(见 [`crate::cache_sim`]):上游 `1.14` 的 cached 为 0 时
    // 在收尾处顶替,封顶 input_tokens;上游报了真实命中时不用它。
    sim_cache_read: u64,
    // CLI 形态(见 `cli.rs`):响应没有 `1.14` 用量帧,收尾认 `is_turn_commit` 回显。
    cli_mode: bool,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);

    tokio::spawn(async move {
        use futures::StreamExt;
        // 全程持有发送端:除了「不 half-close」,服务端的 write/read 调用回执
        // 也经它从请求侧送回去(见下面的 exec 分支)。任务结束时随栈 drop,流才收。
        let body_keepalive = body_keepalive;
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        let mut decoder = wire::FrameDecoder::new();
        // ⚠️ message_start 必须等到**真的有内容**再发。一旦首字节写给客户端,这次请求
        // 就 committed 了,gw-app 再也不能换号重试。而 Run 的错误恰恰是在流**末尾**的
        // trailer 里报的 —— 先发 message_start 等于把所有可重试的错误变成不可重试。
        let mut started = false;
        let mut output_chars: usize = 0;
        // 正文里的非 ASCII 字数:收尾估 output token 时中文不能按 4 字符/token 算
        // (见 est_text_tokens)。
        let mut output_nonascii: usize = 0;
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
                // 未透传的思考**不算进展**:否则会一边丢 `1.4`、一边续命 last_progress,
                // 拖到 gw-app 的 300s 静默硬上限才报错(见 client_wants_thinking 注释)。
                if !text.is_empty()
                    || usage.is_some()
                    || (want_thinking && !fr.thinking.is_empty())
                {
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
                    output_nonascii += text.chars().filter(|c| !c.is_ascii()).count();
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

                // CLI 形态的会话登记通知(2026-08-16 抓包新帧,见 run::is_session_notice):
                // 顶层 field 2 的 request_context,真客户端不回任何帧 —— 忽略即可。
                // 早期实现把它误收成「内建工具调用」,首轮还没出字就被掐断。
                if run::is_session_notice(&payload) {
                    last_progress = std::time::Instant::now();
                    continue;
                }

                // CLI 形态**没有 `1.14` 用量帧**:收尾信号是「本轮已提交」回显
                // (见 run::is_turn_commit)。不认它,每轮都要等满看门狗才收口。
                if cli_mode && run::is_turn_commit(&payload) {
                    // 合帧防御:同帧若有用量先收下(目前 CLI 实测没有,留这条不亏)。
                    if let Some((input, output, cached)) = usage {
                        upstream_usage = Some(usage_from_upstream(input, output, cached));
                    }
                    saw_end = true;
                    break 'outer;
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
                    // 服务端内建 exec 调用(ExecServerMessage,字段号见 run.rs 常量区):
                    // **写盘** = 带图流程的资产落盘(服务端把附件字节推给客户端,
                    // 模型随后用**读文件**把同一路径的图看进去)。真 IDE 有真磁盘;
                    // 我们没有 —— 写:存进内存资产库并回执成功;读:从内存库回图。
                    // 只接「写 /assets/ 前缀」和「读我们存过的路径」两种 ——
                    // 代执行模型选定的任意文件/命令是红线,其余内建调用维持收口。
                    let mut exec_handled = false;
                    if let Some(w) = run::parse_exec_write(&payload) {
                        if w.path.starts_with("/assets/") {
                            last_progress = std::time::Instant::now();
                            match &body_keepalive {
                                Some(btx) => {
                                    if !assets.put(&conversation_id, &w.path, &w.bytes) {
                                        tracing::warn!(
                                            path = %w.path, bytes = w.bytes.len(),
                                            "cursor 资产超内存上限,未存(后续 read 会落空)"
                                        );
                                    }
                                    let ack = wire::frame(&run::encode_write_success(
                                        w.id, &w.exec_id, &w.path, w.bytes.len(),
                                    ));
                                    // 回执是协议关键帧:try_send 丢了会退化成 90s 死等,
                                    // 所以**等容量**而不是丢;只有真发出去才算已处理。
                                    // 通道对端是 hyper 在持续消费请求体,不会死锁。
                                    if btx.send(Ok(bytes::Bytes::from(ack))).await.is_err() {
                                        tracing::warn!(
                                            path = %w.path,
                                            "cursor 资产写回执发送失败(请求流已关),按未处理走收口"
                                        );
                                    } else {
                                        exec_handled = true;
                                        tracing::debug!(
                                            path = %w.path, bytes = w.bytes.len(),
                                            "cursor 资产写调用:已存内存并回执"
                                        );
                                    }
                                }
                                None => tracing::warn!(
                                    path = %w.path,
                                    "cursor 资产写调用无法回执(请求流未保持打开),按未处理走收口"
                                ),
                            }
                        }
                    } else if let Some(r) = run::parse_exec_read(&payload) {
                        if let Some(bytes) = assets.get(&conversation_id, &r.path) {
                            last_progress = std::time::Instant::now();
                            match &body_keepalive {
                                Some(btx) => {
                                    let reply = wire::frame(&run::encode_read_success_data(
                                        r.id, &r.exec_id, &r.path, &bytes,
                                    ));
                                    if btx.send(Ok(bytes::Bytes::from(reply))).await.is_err() {
                                        tracing::warn!(
                                            path = %r.path,
                                            "cursor 资产读回执发送失败(请求流已关),按未处理走收口"
                                        );
                                    } else {
                                        exec_handled = true;
                                        tracing::debug!(
                                            path = %r.path, bytes = bytes.len(),
                                            "cursor 资产读调用:已从内存回图"
                                        );
                                    }
                                }
                                None => tracing::warn!(
                                    path = %r.path,
                                    "cursor 资产读调用无法回执(请求流未保持打开),按未处理走收口"
                                ),
                            }
                        }
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
                            // 外部工具臂命中即 break(反代一问一答,不能挂流等工具结果)。
                            // 但同帧若已带 `1.14` 用量,绝不能丢 —— 合帧丢 usage 会让
                            // 面板/new-api 看到 input=0(见 PROTOCOL §残余假设 #2)。
                            if let Some((input, output, cached)) = usage {
                                upstream_usage = Some(usage_from_upstream(input, output, cached));
                            }
                            // 外部工具支是正常的 tool_use 收尾。
                            saw_end = true;
                            break 'outer;
                        }
                        // 已处理的 exec 调用(资产写/读):**不能 break** ——
                        // protobuf 层没有任何东西阻止上游把 exec 帧和 usage 并进
                        // 同一帧,在这里收口会把同帧的用量吞掉,陪上游死等心跳到
                        // watchdog。落出本分支,让下面的 usage/文本判断照常跑。
                        None if exec_handled => {}
                        // Cursor 服务端**内建**的工具(终端 / 读文件 …)。它们是闭集枚举、
                        // 不带名字 —— 但其中两种(.1 终端 / .4 读文件)的参数形状有抓包
                        // 实证,可走**兼容转换层**(2026-08-13):翻译成调用方声明工具的
                        // 标准 tool_use 下发,客户端照常执行,「半截回答 + 静默截断」变成
                        // 无感成功。翻译不了(身份/参数认不出、调用方没声明同义工具)才
                        // 落回收口 + 纠偏 —— 失败单向,绝不输出参数张冠李戴的调用。
                        // preview 留在日志里,是为了万一要扩内建工具映射时有据可查。
                        None => {
                            if let Some(tc) = run::parse_builtin_call(&payload)
                                .and_then(|bc| translate_builtin(bc, &xlate))
                            {
                                tracing::info!(
                                    tool = %tc.name,
                                    id = %client_tool_id(&tc.id),
                                    guard_rev = %crate::tool_guard_rev(),
                                    "cursor Run:内建工具调用已翻译成调用方工具的 tool_use(兼容转换层)"
                                );
                                tool_call = Some(tc);
                                // 同帧用量同样要收(与外部工具臂同一条合帧原则)。
                                if let Some((input, output, cached)) = usage {
                                    upstream_usage =
                                        Some(usage_from_upstream(input, output, cached));
                                }
                                // 与外部工具臂同款:正常的 tool_use 收尾,不是截断。
                                saw_end = true;
                                break 'outer;
                            }
                            let preview: String = String::from_utf8_lossy(&payload)
                                .chars()
                                .map(|c| if c.is_control() { '·' } else { c })
                                .take(300)
                                .collect();
                            tracing::warn!(
                                bytes = payload.len(), started, output_chars,
                                // 护栏文案的版本指纹。收口率是分子、按 guard_rev 分桶才
                                // 比得出「换了文案有没有用」——热改文案后没有它,这条日志
                                // 只能告诉你出了多少次,不能告诉你是哪一版出的。
                                guard_rev = %crate::tool_guard_rev(),
                                // 这一帧里模型要的能力(认得出的话),用来在下一轮纠偏时
                                // 指名换哪个工具;认不出就是 None,只记 preview。
                                cap = ?builtin_capability(&payload),
                                preview = %preview,
                                "cursor Run:模型调用了 Cursor 内建工具(我们无法执行)—— 收口"
                            );
                            // 记下来,下一轮同会话时告诉模型「上次那个工具不存在」。
                            //
                            // 两条分支(出字前 502 / 出字后截断)都记:两种情况下客户端
                            // 历史里都留着一次没有结果的尝试,模型两种情况下都会重复撞墙。
                            // 出字前那条尤其要记 —— 它连半句话都没有,客户端只看到 502,
                            // 用户几乎一定会原样重发。
                            notices.record(&conversation_id, builtin_capability(&payload));
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
                            // 同帧用量同样要收(与外部工具臂同一条合帧原则)。
                            if let Some((input, output, cached)) = usage {
                                upstream_usage = Some(usage_from_upstream(input, output, cached));
                            }
                            // 内建工具支下面会照常走收尾流程(而不是报错),所以算「见过收尾」。
                            saw_end = true;
                            break 'outer;
                        }
                    }
                }

                // 用量帧 = 本轮结束。BiDi 流永远不会自己关,不在这里收口的话
                // 每个请求都要挂到客户端超时 —— 「答完了却一直转圈」。
                if let Some((input, output, cached)) = usage {
                    upstream_usage = Some(usage_from_upstream(input, output, cached));
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

        // 上游自报优先,但「input=0 且 cache=0」的用量帧**不可信** —— 请求体显然
        // 非空,全零是上游字段漂移/置零,不是真实零输入,落回请求体粗估。
        // 没有用量帧时同样粗估(绝不能只估 output —— tool_use 轮会让 new-api
        // 显示「输入 0 / 缓存 0」)。
        let mut usage = match upstream_usage {
            Some(u) if u.input_tokens > 0 || u.cache_read_tokens > 0 => u,
            zeroed => {
                if zeroed.is_some() {
                    tracing::warn!(
                        tool = tool_call.as_ref().map(|t| t.name.as_str()),
                        "cursor Run:上游用量帧 input/cache 全零,按请求体粗估用量"
                    );
                }
                let mut u = usage_fallback;
                // output = 正文出字(ASCII/4 + 非 ASCII/1.5)+ 工具参数(纯 tool_use
                // 轮的产出大头是参数 JSON)。上游若给了 output(哪怕 input/cache
                // 置零),它比估算准,用它的。
                let text_tokens = ((output_chars - output_nonascii) as u64).div_ceil(4)
                    + output_nonascii as u64;
                u.output_tokens = text_tokens
                    + tool_call.as_ref().map(tool_call_tokens).unwrap_or(0);
                if let Some(up) = &zeroed {
                    if up.output_tokens > 0 {
                        u.output_tokens = up.output_tokens;
                    }
                }
                if u.input_tokens == 0 {
                    tracing::warn!(
                        output_chars,
                        tool = tool_call.as_ref().map(|t| t.name.as_str()),
                        "cursor Run:无上游用量且请求体估出 input=0 —— 计费会失真"
                    );
                } else {
                    tracing::debug!(
                        input = u.input_tokens,
                        output = u.output_tokens,
                        cache = u.cache_read_tokens,
                        tool = tool_call.as_ref().map(|t| t.name.as_str()),
                        "cursor Run:无上游 1.14,使用请求体粗估用量"
                    );
                }
                u
            }
        };
        // 缓存按**字段**单独决策(不能整对象二选一):上游 `1.14` 的 cached 报了
        // 真实命中(>0)就用上游;没报(=0)用模拟值顶替,封顶 input_tokens。
        // 上游的 0 是「没命中」还是「没统计」无法分辨,而生产实测它几乎恒 0
        // (见 cache_sim 模块注释),不顶替客户侧就永远全价。模拟不是事实断言:
        // real_cache_read_tokens 在此**不动**,仍只认上游自报(对账列)。
        if usage.cache_read_tokens == 0 && sim_cache_read > 0 {
            usage.cache_read_tokens = sim_cache_read.min(usage.input_tokens);
        }
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
        let delta_usage = delta_usage_json(&usage, output_tokens);
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

    /// **2026-08-17 生产事故的回归锁**:真实报文形态 —— 中转在每条用户消息之后
    /// 追一条 `role:"system"` 的预算计数器。分流前末轮是那条计数器,
    /// CLI 驱动就把它当 prompt 发出去了(用户看到「你这条消息是空的」)。
    #[test]
    fn 分流器_预算计数器不再顶掉用户末轮() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "# 项目 X 交接:第四阶段启动"}]},
            {"role": "system", "content": [{"type": "text", "text": "<total_tokens>15000000 tokens left</total_tokens>"}]},
        ]});
        // 分流前 + 旧的取法(`turns.last()`):末轮是注入的计数器,发出去的就是它。
        assert_eq!(
            to_turns(&body).last().unwrap().text,
            "<total_tokens>15000000 tokens left</total_tokens>",
            "这就是事故现场:用户打的那段被计数器整条顶掉"
        );
        // 两道修复各自独立够用:只上 `latest_user_input`(不分流)时用户的话也回来了。
        assert_eq!(
            latest_user_input(&to_turns(&body)),
            "# 项目 X 交接:第四阶段启动\n\n<total_tokens>15000000 tokens left</total_tokens>"
        );
        route_system_role_messages(&mut body);
        // 分流后:计数器被丢弃,prompt 是用户真正打的那段。
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            latest_user_input(&to_turns(&body)),
            "# 项目 X 交接:第四阶段启动"
        );
    }

    /// 同一条尾巴还会掐死工具回路:`last_tool_results` 要求末条是 user,
    /// 尾巴是 system 就返回 None → 挂起的桥调用永远接不上(模型报「MCP 读取被中断了」)。
    #[test]
    fn 分流器_工具回路的接续不再被尾巴掐死() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "读一下 STATUS.md"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_a", "content": "文件内容"}]},
            {"role": "system", "content": "<total_tokens>14990000 tokens left</total_tokens>"},
        ]});
        assert_eq!(last_tool_results(&body), None, "分流前:接不上");
        route_system_role_messages(&mut body);
        assert_eq!(
            last_tool_results(&body),
            Some(vec![("toolu_a".to_string(), "文件内容".to_string())]),
            "分流后:桥调用能接续"
        );
    }

    /// 四级分流各走各的路,且**位置语义保留**(未知注入原位转 user,不前后串位)。
    #[test]
    fn 分流器_四级分流() {
        let mut body = json!({"system": "原有 system", "messages": [
            {"role": "system", "content": "SessionStart hook additional context:\n项目约定 X"},
            {"role": "user", "content": "甲"},
            {"role": "system", "content": "The task tools haven't been used recently. 略"},
            {"role": "system", "content": "Available agent types for the Agent tool:\n- claude"},
            {"role": "system", "content": "   "},
            {"role": "system", "content": "The user sent a new message while you were working:\n先停一下\n\nIMPORTANT: 别提这条"},
            {"role": "assistant", "content": "乙"},
        ]});
        route_system_role_messages(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        // 稳定前缀提升 → 不在 messages 里;动态噪声与空消息丢弃。
        assert_eq!(msgs.len(), 4, "{msgs:#?}");
        assert_eq!(msgs[0], json!({"role": "user", "content": "甲"}));
        assert_eq!(
            msgs[1],
            json!({"role": "user",
                   "content": "<system_context>\nAvailable agent types for the Agent tool:\n- claude\n</system_context>"}),
            "不认识的注入:裹起来转 user,原位保留"
        );
        // interrupted-user 取正文、截掉 IMPORTANT 尾巴。
        assert_eq!(msgs[2], json!({"role": "user", "content": "先停一下"}));
        assert_eq!(msgs[3]["role"], "assistant");
        // 提升的稳定前缀接在既有 system 之后。
        assert_eq!(
            body["system"],
            json!("原有 system\n\nSessionStart hook additional context:\n项目约定 X")
        );
    }

    /// 快路径:没有 `role:"system"` 消息时**一个字节都不改** ——
    /// 绝大多数流量走这条,任何改写都是形状漂移。
    #[test]
    fn 分流器_无system角色时逐字节不变() {
        for b in [
            json!({"messages": [{"role": "user", "content": "hi"}]}),
            json!({"system": [{"type": "text", "text": "S"}],
                   "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]},
                                {"role": "assistant", "content": "yo"}]}),
            json!({}),
        ] {
            let mut got = b.clone();
            route_system_role_messages(&mut got);
            assert_eq!(got, b, "{b}");
        }
    }

    /// 动态噪声的判据必须精确到「整条就是这个」:带正文的消息不能被误丢。
    #[test]
    fn 分流器_动态噪声判据不放宽() {
        assert!(is_dynamic_system_noise("<total_tokens>123 tokens left</total_tokens>"));
        assert!(is_dynamic_system_noise(
            "\n  <total_tokens>123 tokens left</total_tokens>  \n"
        ));
        // 前后带正文 → 不是纯噪声,必须保内容。
        assert!(!is_dynamic_system_noise(
            "记住这条约定\n<total_tokens>123 tokens left</total_tokens>"
        ));
        assert!(!is_dynamic_system_noise(
            "<total_tokens>123 tokens left</total_tokens>\n顺便说下预算含义"
        ));
        let mut body = json!({"messages": [
            {"role": "system", "content": "记住这条约定\n<total_tokens>1 tokens left</total_tokens>"},
        ]});
        route_system_role_messages(&mut body);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1, "不能丢");
    }

    /// `latest_user_input`:正常形态(末轮一条用户消息)与 `last()` 逐字节相同;
    /// 末尾有多条 user 轮时取整段。
    #[test]
    fn 本轮输入取整段而非末条() {
        let one = vec![
            Turn { text: "甲".into(), is_user: true },
            Turn { text: "乙".into(), is_user: false },
            Turn { text: "丙".into(), is_user: true },
        ];
        assert_eq!(latest_user_input(&one), "丙", "正常形态 = last()");
        let two = vec![
            Turn { text: "乙".into(), is_user: false },
            Turn { text: "丙".into(), is_user: true },
            Turn { text: "<system_context>\n未知注入\n</system_context>".into(), is_user: true },
        ];
        assert_eq!(
            latest_user_input(&two),
            "丙\n\n<system_context>\n未知注入\n</system_context>",
            "未知注入落在尾巴上时,用户的话仍在 prompt 里"
        );
        // 全是 user(首轮)→ 整条都是本轮输入。
        let fresh = vec![Turn { text: "甲".into(), is_user: true }];
        assert_eq!(latest_user_input(&fresh), "甲");
        assert_eq!(latest_user_input(&[]), "");
    }

    /// 亲和键锚点必须跳过注入的 system 消息:worker 是在 `chat()` 之前拿**原始**
    /// body 调这个函数的,那时分流还没跑。锚点取到每轮都变的计数器 = 亲和归零。
    #[test]
    fn 亲和键锚点跳过注入的system消息() {
        let with_inject = json!({"messages": [
            {"role": "system", "content": "<total_tokens>15000000 tokens left</total_tokens>"},
            {"role": "user", "content": "锚点甲"},
        ]});
        let next_round = json!({"messages": [
            {"role": "system", "content": "<total_tokens>14000000 tokens left</total_tokens>"},
            {"role": "user", "content": "锚点甲"},
            {"role": "assistant", "content": "回"},
            {"role": "user", "content": "再问"},
        ]});
        let a = affinity_key_from_body(&with_inject);
        assert!(a.is_some());
        assert_eq!(a, affinity_key_from_body(&next_round), "同会话跨轮必须稳定");
        // 无注入时与旧行为一致。
        let plain = json!({"messages": [{"role": "user", "content": "锚点甲"}]});
        assert_eq!(a, affinity_key_from_body(&plain));
    }

    /// 线上 usage 口径 = Anthropic 规范:`input_tokens` 是**未命中缓存的新增部分**,
    /// 缓存读取单列;cursor 没有缓存创建计数,绝不出 `cache_creation_input_tokens`。
    /// 钉住这条是因为旧口径(总上下文透传)会让下游把缓存部分重复计费。
    #[test]
    fn 收尾_usage_输入是增量() {
        let u = ChatUsage {
            input_tokens: 98249,
            output_tokens: 516,
            cache_read_tokens: 98247,
            ..Default::default()
        };
        let v = delta_usage_json(&u, 516);
        assert_eq!(v["input_tokens"], 2, "总上下文 - 缓存命中 = 增量");
        assert_eq!(v["output_tokens"], 516);
        assert_eq!(v["cache_read_input_tokens"], 98247);
        assert!(v.get("cache_creation_input_tokens").is_none());

        // 缓存命中为 0:输入全量直出,且不出 cache_read 字段。
        let u2 = ChatUsage {
            input_tokens: 100,
            output_tokens: 5,
            ..Default::default()
        };
        let v2 = delta_usage_json(&u2, 5);
        assert_eq!(v2["input_tokens"], 100);
        assert!(v2.get("cache_read_input_tokens").is_none());

        // 上游给的缓存命中 > 总输入(字段漂移/口径变化)时夹到 0,不许下溢。
        let u3 = ChatUsage {
            input_tokens: 10,
            output_tokens: 1,
            cache_read_tokens: 999,
            ..Default::default()
        };
        assert_eq!(delta_usage_json(&u3, 1)["input_tokens"], 0);
    }

    /// 增量模式:只发本轮,历史交给服务端 —— 折叠形态那段假对话记录必须消失。
    #[test]
    fn 增量历史只发本轮_不重传历史() {
        let turns = vec![
            Turn { text: "第一个问题".into(), is_user: true },
            Turn { text: "第一个回答".into(), is_user: false },
            Turn { text: "第二个问题".into(), is_user: true },
        ];
        let out = delta_history(&turns, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "第二个问题");
        assert!(out[0].is_user);
        // 关键:历史与那对包装标签一个字都不能出现(它们正是「同一段对话喂两遍」的来源)。
        for leak in ["第一个问题", "第一个回答", "<conversation_history>"] {
            assert!(!out[0].text.contains(leak), "增量模式不该重传 {leak}");
        }
        // 对照:折叠模式仍然把历史整段带上(两条路都要保持各自的形状)。
        let folded = fold_history(&turns, None);
        assert!(folded[0].text.contains("<conversation_history>"));
        assert!(folded[0].text.contains("第一个回答"));
    }

    /// 尾随 assistant 是 prefill(让模型续写),丢掉会改变语义 —— 增量模式也要带上。
    #[test]
    fn 增量历史保留尾随的_prefill() {
        let turns = vec![
            Turn { text: "历史问题".into(), is_user: true },
            Turn { text: "历史回答".into(), is_user: false },
            Turn { text: "本轮问题".into(), is_user: true },
            Turn { text: "答案是".into(), is_user: false },
        ];
        let out = delta_history(&turns, None);
        assert_eq!(out[0].text, "本轮问题\n\n答案是");
        assert!(!out[0].text.contains("历史问题"));
    }

    /// 工具回路那句提醒两条路共用同一份措辞(治的是「本轮只有工具返回、没有问题」,
    /// 与历史在谁手里无关)。
    #[test]
    fn 增量历史照样补工具回路提醒() {
        let turns = vec![
            Turn { text: "[工具返回]42".into(), is_user: true },
        ];
        let d = delta_history(&turns, Some("帮我算一下"));
        let f = fold_history(&turns, Some("帮我算一下"));
        assert!(d[0].text.contains("不要重复调用已经返回结果的工具"));
        assert!(d[0].text.ends_with("帮我算一下"));
        // 单轮时两条路应当给出完全一样的东西(没有历史可省)。
        assert_eq!(d[0].text, f[0].text);
    }

    /// 畸形输入(没有任何 user 轮)不得产出空 turns:`build_frame0` 对空是 assert,
    /// 而空请求上游只回心跳。
    #[test]
    fn 没有_user_轮时退回折叠而不是返回空() {
        let turns = vec![Turn { text: "只有助手".into(), is_user: false }];
        let out = delta_history(&turns, None);
        assert!(!out.is_empty());
        assert!(out[0].text.contains("只有助手"));
    }

    /// **开关默认关**:正确性押在「服务端真存住了历史」这个未验证假设上,
    /// 所以代码先上、保持关闭,等真号实验验过再热开。
    #[test]
    fn 增量模式默认关闭() {
        assert!(
            !delta_history_enabled(),
            "默认必须是关的 —— 押错的代价是模型静默丢上下文"
        );
    }

    /// **会话键必须对 Claude Code 的滚动 billing 指纹免疫。**
    ///
    /// 不剥那行的后果是连锁的:键每请求都变 → 账号钉扎失效 + `conversation_id` 每轮都变
    /// → `phase_for` 永远给 `Opening`(环境块按首轮形态发)→ 顺带缓存指纹每轮都变、
    /// 命中率恒为 0。**注意它治不了「模型绕圈」** —— `fold_history` 是无条件执行的,
    /// 与 phase 无关(见 `extract_system` 的注释)。
    #[test]
    fn 会话键不受每请求变化的_cch_影响() {
        let body = |cch: &str| {
            serde_json::json!({
                "model": "grok-4.5",
                "system": format!(
                    "x-anthropic-billing-header: cc_version=2.1.63.a43; cc_entrypoint=cli; cch={cch};\n\
                     You are Claude Code, Anthropic's official CLI for Claude.\n"
                ),
                "messages": [{"role": "user", "content": "帮我看下这个函数"}],
            })
        };
        let k1 = affinity_key_from_body(&body("ea527")).expect("要能派生出键");
        let k2 = affinity_key_from_body(&body("b91f0")).expect("要能派生出键");
        assert_eq!(k1, k2, "同一会话两轮的键必须相同,否则钉扎/续写/缓存三件事一起废");

        // 顺带确认:发给上游的 system 里不能再留着那行随机数(它会毒化 prefix cache)。
        let sys = extract_system(&body("ea527"));
        assert!(!sys.contains("x-anthropic-billing-header"), "指纹行不该进上游报文");
        assert!(sys.contains("You are Claude Code"), "正文必须保留");

        // 反向:换了真正的会话内容(system 正文或开场白)仍必须换键。
        let other = serde_json::json!({
            "model": "grok-4.5",
            "system": "x-anthropic-billing-header: cch=ea527;\n你是另一个 agent\n",
            "messages": [{"role": "user", "content": "帮我看下这个函数"}],
        });
        assert_ne!(k1, affinity_key_from_body(&other).unwrap());
    }

    fn ctx() -> RunCtx {
        RunCtx {
            host: "agentn.api5.cursor.sh".into(),
            token: "tok".into(),
            machine_id: "m".repeat(64),
            mac_machine_id: Some("a".repeat(64)),
            config_version: "cv".into(),
            timezone: "Asia/Shanghai".into(),
            conversation_id: "conv-1".into(),
            account_id: "acc-1".into(),
            shape: RunShape::default(),
            context_frames: true,
            phase: run::Phase::Opening,
            profile: crate::cli::Profile::Ide,
            keep_stream_open: true,
            assets: std::sync::Arc::new(crate::AssetStore::default()),
            notices: std::sync::Arc::new(crate::TruncationNotices::default()),
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

    /// 2026-08-13 生产事故的回归锁:tool_result **内嵌**的 image 块必须被
    /// to_media 收进附件,且文本流原位出现编号一致的占位 —— 曾经 to_media 只扫
    /// 顶层块、extract_text 把媒体丢给 `_ => None`,两边互相推诿,工具读的图
    /// 静默消失且零日志。
    #[test]
    fn tool_result_nested_image_is_collected_with_placeholder() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"fake-png-bytes");
        let body = json!({"messages":[
            {"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}},
                {"type":"text","text":"看这张图"}
            ]},
            {"role":"assistant","content":[{"type":"tool_use","id":"c-1","name":"read","input":{}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"c-1","content":[
                    {"type":"text","text":"Image read successfully"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}}
                ]}
            ]}
        ]});
        let (images, docs, ph) = to_media(&body);
        assert_eq!(images.len(), 2, "顶层 1 张 + tool_result 内嵌 1 张");
        assert!(docs.is_empty());
        // 占位表只登记内嵌那张。编号 = images 向量下标(顶层那张是 0,内嵌是 1),
        // 与 run.rs `ImageAttachment::encode(seq)` 合成的 attach-{seq} 路径同源。
        assert_eq!(ph.len(), 1);
        assert_eq!(
            ph.get(&(2, 0, 1)).map(String::as_str),
            Some("[图片见附件 attach-1]"),
            "占位坐标 = (消息2, 顶层块0, 内嵌块1)"
        );
        // 文本流:工具返回的文字不丢,占位在原位出现。
        let turns = to_turns_with_media(&body, Some(&ph));
        assert!(turns[2].text.contains("Image read successfully"));
        assert!(turns[2].text.contains("[图片见附件 attach-1]"));
        // 不带表 = 旧行为(cache_sim 指纹等经 to_turns 的调用点):无占位,
        // 指纹口径不因本修复漂移。
        let old = to_turns(&body);
        assert!(!old[2].text.contains("附件"));
        assert!(old[2].text.contains("Image read successfully"));
        // 折叠后收进当前(最后一条)用户消息 —— run.rs 把图挂最后一条用户轮,
        // 历史折成一条,嵌套图天然随之挂上。
        let folded = fold_history(&turns, None);
        assert_eq!(folded.len(), 1);
        assert!(folded[0].text.contains("[图片见附件 attach-1]"));
    }

    /// 内嵌块与顶层块共用同一套校验/限额口径:url 源跳过(不代为下载)、
    /// 超单个上限跳过;被跳过的块**不占编号**,占位与附件顺序无空洞。
    #[test]
    fn tool_result_nested_non_base64_or_oversized_is_skipped() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"ok");
        // 超限走 base64 长度预检(len/4*3 > MAX_ONE_ATTACHMENT),不用真解码 12MB。
        let huge = "A".repeat(MAX_ONE_ATTACHMENT / 3 * 4 + 8);
        let body = json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"c-1","content":[
                    {"type":"image","source":{"type":"url","url":"https://x/img.png"}},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data": huge}},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}}
                ]}
            ]}
        ]});
        let (images, docs, ph) = to_media(&body);
        assert_eq!(images.len(), 1, "url 源与超限的都要跳过");
        assert!(docs.is_empty());
        assert_eq!(ph.len(), 1);
        // 唯一收下的是第 3 个内嵌块(ni=2),编号从 0 起、无空洞。
        assert_eq!(
            ph.get(&(0, 0, 2)).map(String::as_str),
            Some("[图片见附件 attach-0]")
        );
        // 被跳过的块在文本流里不留占位(占位表里没有它们)。
        let turns = to_turns_with_media(&body, Some(&ph));
        assert!(!turns[0].text.contains("attach-1"));
    }

    /// tool_result 内嵌 document:收进 docs 并留 `[文档见 <path>]` 占位;
    /// content 为纯字符串的 tool_result 没有媒体,一个附件都不收。
    #[test]
    fn tool_result_nested_document_and_string_content() {
        use base64::Engine as _;
        let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 fake");
        let body = json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"c-1","content":"plain string result"},
                {"type":"tool_result","tool_use_id":"c-2","content":[
                    {"type":"document","source":{"type":"base64","media_type":"application/pdf","data": pdf}}
                ]}
            ]}
        ]});
        let (images, docs, ph) = to_media(&body);
        assert!(images.is_empty());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "/tmp/gw-cursor/doc-0.pdf");
        assert_eq!(
            ph.get(&(0, 1, 0)).map(String::as_str),
            Some("[文档见 /tmp/gw-cursor/doc-0.pdf]")
        );
        let turns = to_turns_with_media(&body, Some(&ph));
        assert!(turns[0].text.contains("plain string result"));
        assert!(turns[0].text.contains("[文档见 /tmp/gw-cursor/doc-0.pdf]"));
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

    /// ⭐ exec 控制流集成测试(2026-08-10 带图 502 的根治路径):
    /// 收到资产写调用帧后 —— ① 回执**真的进了请求体通道**(不是只在日志里说说);
    /// ② 资产进了内存库;③ 同流随后的用量帧没被吞,收尾产出 Usage。
    /// 解析/编码单测替代不了这条:它钉的是 stream_to_anthropic 的控制流本身。
    #[tokio::test]
    async fn 资产写调用有回执且不吞用量() {
        // 真上游抓的资产写调用帧(1x1 PNG,73B,write_args)。
        let write_frame: &[u8] = include_bytes!("../tests/fixtures/asset_echo_real.bin");
        // 用量帧 {1:{14:{1:100,2:5,3:90}}} —— 手写字节,绕开我方 Writer(自己写自己解是白测)。
        let usage_payload = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a];
        // 正文帧 {1:{1:{1:"红"}}}:收尾要求出过字,否则按 EmptyResponse 提前返回(那是
        // 另一条故意的保护路径,不是本测试要钉的)。
        let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2];
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from(wire::frame(write_frame))),
            Ok(bytes::Bytes::from(wire::frame(&text_payload))),
            Ok(bytes::Bytes::from(wire::frame(&usage_payload))),
        ];
        let (btx, mut brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        let assets = std::sync::Arc::new(crate::AssetStore::default());
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            Some(btx),
            None,
            "conv-test".into(),
            assets.clone(),
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage::default(),
            0, // 本用例不关心模拟缓存,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;

        // ① 回执进了请求体通道,且是 AgentClientMessage{2: exec_client_message{3: …}}。
        let ack = brx
            .try_recv()
            .expect("资产写调用必须产生请求侧回执")
            .expect("回执帧必须是 Ok");
        let payload = wire::frame_payload(ack[0], &ack[5..]).expect("回执是合法 Connect 帧");
        let ecm = crate::protobuf::Reader::new(&payload)
            .find_map(|(f, v)| match (f, v) {
                (2, crate::protobuf::Value::Len(s)) => Some(s),
                _ => None,
            })
            .expect("回执顶层必须是 field 2(exec_client_message)");
        assert!(
            crate::protobuf::Reader::new(ecm).any(|(f, _)| f == 3),
            "回执必须带 field 3(write_result)"
        );

        // ② 资产进了内存库,路径与实物帧一致。
        let stored = assets
            .get("conv-test", "/assets/attach-0-6bd00159-1e01-4924-b8c2-12f28cc81e53.png")
            .expect("资产必须已存进内存库");
        assert_eq!(stored.len(), 73);
        assert!(stored.starts_with(b"\x89PNG"), "存的是原图字节");

        // ③ 用量没被吞:收尾产出 Usage(100/5/90),且 SSE 里 message_delta 带增量口径
        // (100-90=10 新增输入,缓存读取单列)。
        let usage = items.iter().find_map(|it| match it {
            Ok(StreamItem::Usage(u)) => Some(u),
            _ => None,
        });
        let usage = usage.expect("用量帧绝不能被 exec 处理吞掉");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 90);
        assert_eq!(usage.real_cache_read_tokens, 90, "cursor 真实缓存应同步");
        let delta = items.iter().find_map(|it| match it {
            Ok(StreamItem::Sse(e)) if e.event == "message_delta" => Some(e),
            _ => None,
        });
        let delta = delta.expect("正常收尾必须发 message_delta");
        let u = &delta.data["usage"];
        assert_eq!(u["input_tokens"], 10, "增量口径:100-90");
        assert_eq!(u["output_tokens"], 5);
        assert_eq!(u["cache_read_input_tokens"], 90);
    }

    /// 无上游 `1.14` 时(tool_use 收口常态)必须从请求体估出 input/output,
    /// 绝不能只留下 output —— 那会让 new-api 显示「0 / N」。
    /// cache_read 不在此估:统一由 cache_sim 在收尾处给(见 estimate_usage_fallback 注释)。
    #[test]
    fn 无上游用量时按请求体估出输入与产出() {
        let system = "You are a coding agent."; // 24 chars → 6 tok
        let turns = vec![
            run::Turn {
                text: "a".repeat(40), // 10 tok
                is_user: true,
            },
            run::Turn {
                text: "b".repeat(80), // 20 tok
                is_user: false,
            },
            run::Turn {
                text: "c".repeat(20), // 5 tok —— 本轮用户新增
                is_user: true,
            },
        ];
        let tools = [run::ToolDef {
            name: "Edit".into(),
            description: "edit a file".into(), // 11
            schema: "{\"type\":\"object\"}".into(), // 16
        }];
        // system 24 + conv 140 + tools (4+11+16=31) = 195 chars → 49 tok
        let u = estimate_usage_fallback(system, &turns, &tools, 12);
        assert!(u.input_tokens >= 40, "input={}", u.input_tokens);
        assert_eq!(u.output_tokens, 3, "12 chars → 3 tok");
        assert_eq!(
            u.cache_read_tokens, 0,
            "兜底估算不再给缓存 —— 缓存归 cache_sim 管(状态化前缀命中)"
        );
        assert_eq!(u.real_cache_read_tokens, 0, "估算路径绝不写对账列");

        // 单轮冷启动:同样只有 input,无缓存
        let cold = estimate_usage_fallback(
            "sys",
            &[run::Turn {
                text: "hello world!!".into(), // 13
                is_user: true,
            }],
            &[],
            0,
        );
        assert_eq!(cold.cache_read_tokens, 0);
        assert!(cold.input_tokens > 0);
    }

    /// 真实转换链路回归(审查 4.6):从 Anthropic body 走 to_turns → 指纹 →
    /// peek/commit,而不是手工构造 Turn。专门防「单测手造 3 个 Turn,生产
    /// fold_history 早已折成 1 个」的脱节:折叠形态里闭合标签横在中间,
    /// 按折叠后内容取指纹会让第二轮整段 miss —— 所以指纹必须取折叠前的
    /// 逐条消息,第二轮才能命中第一轮的逻辑前缀。
    #[test]
    fn 模拟缓存走真实转换链路第二轮命中() {
        let body1 = json!({
            "system": "You are a coding agent.",
            "messages": [{"role":"user","content":"帮我看看这个项目"}]
        });
        let body2 = json!({
            "system": "You are a coding agent.",
            "messages": [
                {"role":"user","content":"帮我看看这个项目"},
                {"role":"assistant","content":"好的,这是一个 Rust 工作区…"},
                {"role":"user","content":"先跑一下测试"}
            ]
        });
        // 与 chat_stream 同源的指纹构造(逐条消息 + tools + 带守卫的 system)。
        let fps_of = |body: &Value| {
            let mut sys = extract_system(body);
            let tools = to_tools(body);
            sys.push_str(&builtin_tool_guard(&tools));
            crate::cache_sim::fingerprints_from_context(&sys, &tools, &to_turns(body), est_text_tokens)
        };
        let store = crate::cache_sim::CacheSimStore::new();
        let t0 = std::time::Instant::now();
        let key = "acc-1\x1fconv-1";

        let fps1 = fps_of(&body1);
        let (r1, gen1) = store.peek_at(key, "claude-fable-5", &fps1, t0);
        assert_eq!(r1.cache_read_tokens, 0, "首轮冷启动必须 0 命中");
        assert!(store.commit_at(key, "claude-fable-5", fps1, t0, gen1));

        let fps2 = fps_of(&body2);
        let (r2, _) = store.peek_at(key, "claude-fable-5", &fps2, t0 + std::time::Duration::from_secs(30));
        // 第二轮应命中:system + 第一轮 user 消息(逻辑前缀)。
        let expect_min = r1.total_tokens; // 第一轮全部被第二轮包含为前缀
        assert!(
            r2.cache_read_tokens >= expect_min,
            "第二轮应命中第一轮全部内容: got {}, expect >= {}",
            r2.cache_read_tokens,
            expect_min
        );
        assert!(
            r2.cache_read_tokens < r2.total_tokens,
            "本轮新增(assistant 回复 + 新问题)不能算命中"
        );
    }

    /// 中文等非 ASCII 文本不能按 4 字符/token 估 —— 那会少计大半(计费口径,
    /// 保守取 1 字符/token)。
    #[test]
    fn 估算对中文不缩水() {
        let ascii = est_text_tokens(&"a".repeat(400));
        assert_eq!(ascii, 100, "ASCII 400 字符 ≈ 100 token");
        let cjk = est_text_tokens(&"汉".repeat(300));
        assert_eq!(cjk, 300, "非 ASCII 按保守 1 字符/token");
        assert!(cjk > 75, "按 /4 只有 75,直接少计 75%");
    }

    /// 纯 tool_use 轮(正文为零)的产出必须计入工具参数,否则 output=0 漏账。
    #[test]
    fn 工具参数计入产出估算() {
        let tc = run::ToolCall {
            id: "call_1".into(),
            name: "Edit".into(),
            args: vec![
                ("file_path".into(), json!("/tmp/a.rs")),
                ("content".into(), json!("汉".repeat(150))),
            ],
        };
        let t = tool_call_tokens(&tc);
        // 150 汉字 ≈ 100 token,加 key/路径/工具名,必须远大于 0
        assert!(t >= 100, "tool tokens={t}");
    }

    /// 上游用量帧 input/cache 全零 = 不可信(字段漂移/置零),必须落回请求体粗估,
    /// 但上游给的 output 仍采用(它比估算准)。cache_read 由模拟器顶替(封顶 input),
    /// 且模拟值绝不进 real_cache_read(对账列)。
    #[tokio::test]
    async fn 上游零用量帧回落请求体粗估() {
        let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
        // {1:{14:{1:0,2:7,3:0}}} —— input/cache 置零、output=7
        let usage_payload: &[u8] = &[
            0x0a, 0x08, 0x72, 0x06, 0x08, 0x00, 0x10, 0x07, 0x18, 0x00,
        ];
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from(wire::frame(&text_payload))),
            Ok(bytes::Bytes::from(wire::frame(usage_payload))),
        ];
        let assets = std::sync::Arc::new(crate::AssetStore::default());
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            None,
            None,
            "conv-test".into(),
            assets,
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage {
                input_tokens: 500,
                output_tokens: 1,
                ..Default::default()
            },
            400, // 模拟缓存命中,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;
        let usage = items
            .iter()
            .find_map(|it| match it {
                Ok(StreamItem::Usage(u)) => Some(u),
                _ => None,
            })
            .expect("必须产出 Usage");
        assert_eq!(usage.input_tokens, 500, "零帧不可信,用请求体粗估值");
        assert_eq!(usage.cache_read_tokens, 400, "cached=0 时由模拟值顶替");
        assert_eq!(
            usage.real_cache_read_tokens, 0,
            "模拟值不能进对账列(real 只认上游自报)"
        );
        assert_eq!(usage.output_tokens, 7, "上游给的 output 采用");
    }

    /// 模拟值必须封顶 input_tokens:会话内容突变(压缩/截断)后,上一轮的命中
    /// 可能超过本轮总输入,超帽会让增量 input 变 0、缓存部分被重复计费。
    #[tokio::test]
    async fn 模拟缓存封顶输入总量() {
        let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
        // {1:{14:{1:100,2:5,3:0}}} —— input=100, cached=0
        let usage_payload: &[u8] = &[
            0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x00,
        ];
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from(wire::frame(&text_payload))),
            Ok(bytes::Bytes::from(wire::frame(usage_payload))),
        ];
        let assets = std::sync::Arc::new(crate::AssetStore::default());
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            None,
            None,
            "conv-test".into(),
            assets,
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage::default(),
            99999, // 模拟命中远超上游 input,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;
        let usage = items
            .iter()
            .find_map(|it| match it {
                Ok(StreamItem::Usage(u)) => Some(u),
                _ => None,
            })
            .expect("必须产出 Usage");
        assert_eq!(usage.input_tokens, 100, "上游可信帧的 input 不动");
        assert_eq!(usage.cache_read_tokens, 100, "模拟值必须封顶 input_tokens");
        assert_eq!(usage.real_cache_read_tokens, 0);
    }

    /// 上游自报真实命中时,模拟值**不许**顶替(真实 > 模拟);且 real 同步写入。
    #[tokio::test]
    async fn 上游用量同步写入真实缓存字段() {
        let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
        // {1:{14:{1:1000,2:5,3:900}}}
        let usage_payload: &[u8] = &[
            0x0a, 0x0a, 0x72, 0x08, 0x08, 0xe8, 0x07, 0x10, 0x05, 0x18, 0x84, 0x07,
        ];
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from(wire::frame(&text_payload))),
            Ok(bytes::Bytes::from(wire::frame(usage_payload))),
        ];
        let assets = std::sync::Arc::new(crate::AssetStore::default());
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            None,
            None,
            "conv-test".into(),
            assets,
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage {
                input_tokens: 1,
                ..Default::default()
            }, // fallback 不应被用到
            5000, // 模拟值再大也不许盖过上游真实命中,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;
        let usage = items
            .iter()
            .find_map(|it| match it {
                Ok(StreamItem::Usage(u)) => Some(u),
                _ => None,
            })
            .expect("有用量帧时必须产出 Usage");
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 900);
        assert_eq!(
            usage.real_cache_read_tokens, 900,
            "上游用量必须同步写入 real_cache_read"
        );
    }

    /// Claude Code 发 `thinking.type=adaptive`;旧逻辑只认 `enabled`,导致:
    /// 上游开着思考帧刷新 last_progress,收侧却不透传 → 客户端 5 分钟无首字被
    /// gw-app `STREAM_IDLE_ABORT` 掐掉(生产 treenalepsch8375 / 马里奥像素游戏)。
    #[test]
    fn client_wants_thinking_认_adaptive_与_enabled() {
        assert!(client_wants_thinking(Some(&json!({"type": "enabled"}))));
        assert!(client_wants_thinking(Some(&json!({"type": "adaptive"}))));
        assert!(!client_wants_thinking(Some(&json!({"type": "disabled"}))));
        assert!(!client_wants_thinking(None));
        assert!(!client_wants_thinking(Some(&json!({"type": "weird"}))));
        assert!(!client_wants_thinking(Some(&json!({}))));
    }

    /// CLI 驱动桥接续按 tool_use_id 键控消费,提取必须带 id(2026-08-17 对抗审查
    /// 共识 S1-7:不带 id 的拼接文本无法校验错配/重放)。
    #[test]
    fn last_tool_results_带id且只认严格形态() {
        // 严格形态:末条 user、全部 tool_result、都带 tool_use_id。
        let body = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_a", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "结果甲"},
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": [{"type": "text", "text": "结果乙"}]},
            ]},
        ]});
        assert_eq!(
            last_tool_results(&body),
            Some(vec![
                ("toolu_a".to_string(), "结果甲".to_string()),
                ("toolu_b".to_string(), "结果乙".to_string()),
            ])
        );
        // 缺 tool_use_id → None(走重铺,不做无 id 的盲接续)。
        let no_id = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "content": "x"}]},
        ]});
        assert_eq!(last_tool_results(&no_id), None);
        // 混入非 tool_result 块 → None。
        let mixed = json!({"messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "x"},
                {"type": "text", "text": "附加话"},
            ]},
        ]});
        assert_eq!(last_tool_results(&mixed), None);
        // 末条不是 user → None。
        let tail_ai = json!({"messages": [
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        ]});
        assert_eq!(last_tool_results(&tail_ai), None);
    }

    /// 造一帧思考增量 `1.4.1`(与 run 单测同形,避免依赖私有 helper)。
    fn thinking_wire(text: &str) -> Vec<u8> {
        use crate::protobuf::Writer;
        let mut delta = Writer::new();
        delta.string(1, text);
        delta.uint(2, 1);
        let mut msg = Writer::new();
        msg.message(4, &delta);
        let mut outer = Writer::new();
        outer.message(1, &msg);
        outer.into_bytes()
    }

    #[tokio::test]
    async fn adaptive_同等_透传思考块避免无首字挂死() {
        // 只有思考 + 用量:若 want_thinking=false 会走 EmptyResponse(无字无工具);
        // 透传后必须先有 message_start,客户端才算有首字。
        let usage_payload = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a];
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from(wire::frame(&thinking_wire("规划马里奥关卡")))),
            Ok(bytes::Bytes::from(wire::frame(&usage_payload))),
        ];
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            true, // = client_wants_thinking(adaptive/enabled)
            None,
            None,
            "conv-think".into(),
            std::sync::Arc::new(crate::AssetStore::default()),
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage::default(),
            0,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;
        let events: Vec<_> = items
            .iter()
            .filter_map(|it| match it {
                Ok(StreamItem::Sse(e)) => Some(e.event.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            events.iter().any(|e| *e == "message_start"),
            "思考透传必须先发 message_start,否则 TTFB 永远空: {events:?}"
        );
        assert!(
            items.iter().any(|it| matches!(
                it,
                Ok(StreamItem::Sse(e))
                    if e.event == "content_block_delta"
                        && e.data.get("delta").and_then(|d| d.get("type")).and_then(|t| t.as_str())
                            == Some("thinking_delta")
            )),
            "必须出现 thinking_delta"
        );
    }

    // ── 内建工具兼容转换层(2026-08-13)──────────────────────────────────

    fn tool_with_schema(name: &str, schema: &str) -> run::ToolDef {
        run::ToolDef { name: name.into(), description: String::new(), schema: schema.into() }
    }

    /// 转换表的键名必须来自声明工具的 `input_schema.properties`,绝不猜:
    /// Claude Code 是 `command`/`file_path`,opencode 是 `command`/`filePath`,
    /// schema 里没有候选键就放弃翻译(落回收口)。
    #[test]
    fn 转换表按声明工具的_schema_认参数键名() {
        // Claude Code 形态。
        let tools = vec![
            tool_with_schema("Bash", r#"{"type":"object","properties":{"command":{"type":"string"}}}"#),
            tool_with_schema("Read", r#"{"type":"object","properties":{"file_path":{"type":"string"}}}"#),
        ];
        let x = BuiltinXlate::from_tools(&tools);
        assert_eq!(x.terminal, Some(("Bash".into(), "command".into())));
        assert_eq!(x.read_file, Some(("Read".into(), "file_path".into())));

        // opencode 形态(小写名 + filePath)。
        let tools = vec![
            tool_with_schema("bash", r#"{"properties":{"command":{}}}"#),
            tool_with_schema("read", r#"{"properties":{"filePath":{}}}"#),
        ];
        let x = BuiltinXlate::from_tools(&tools);
        assert_eq!(x.terminal, Some(("bash".into(), "command".into())));
        assert_eq!(x.read_file, Some(("read".into(), "filePath".into())));

        // schema 没有任何候选键 → 放弃翻译,绝不猜键名。
        let tools = vec![tool_with_schema("Bash", r#"{"properties":{"script_text":{}}}"#)];
        assert_eq!(BuiltinXlate::from_tools(&tools).terminal, None);

        // 一个工具都没声明 → 两项都空(转换层整体失活,行为回到收口)。
        let x = BuiltinXlate::from_tools(&[]);
        assert_eq!(x.terminal, None);
        assert_eq!(x.read_file, None);
    }

    #[test]
    fn 翻译产物与失败回退() {
        let x = BuiltinXlate {
            terminal: Some(("Bash".into(), "command".into())),
            read_file: None,
        };
        let tc = translate_builtin(
            run::BuiltinCall::Terminal { id: "call-1".into(), command: "ls".into() },
            &x,
        )
        .expect("表里有对应工具就必须翻译");
        assert_eq!(tc.name, "Bash");
        assert_eq!(tc.args, vec![("command".to_string(), json!("ls"))]);
        assert_eq!(tc.id, "call-1");

        // 空 id 合成(与 parse_tool_call 的兜底同款,我方从不把 call id 发回上游)。
        let tc = translate_builtin(
            run::BuiltinCall::Terminal { id: "  ".into(), command: "ls".into() },
            &x,
        )
        .unwrap();
        assert!(tc.id.starts_with("call_"), "空 id 必须合成: {}", tc.id);

        // 表里没有对应工具 → None(调用方落回收口 + 纠偏,失败单向)。
        assert_eq!(
            translate_builtin(run::BuiltinCall::ReadFile { id: "c".into(), path: "/a".into() }, &x),
            None
        );
    }

    /// 造一帧内建终端调用(`1.2.2.1.1.1` = 命令串,§13.2 抓包实证形状)。
    fn builtin_terminal_wire(command: &str) -> Vec<u8> {
        use crate::protobuf::Writer;
        let mut term = Writer::new();
        term.string(1, command);
        let mut one = Writer::new();
        one.message(1, &term);
        let mut detail = Writer::new();
        detail.message(1, &one); // 1.2.2.1 = 内建终端
        let mut ch = Writer::new();
        ch.string(1, "call-b-0");
        ch.message(2, &detail);
        let mut msg = Writer::new();
        msg.message(2, &ch);
        let mut outer = Writer::new();
        outer.message(1, &msg);
        outer.into_bytes()
    }

    /// 「空转中断」的直接回归锁:模型抓无前缀内建终端时,带转换表的流必须下发
    /// 标准 `tool_use`(客户端照常执行);不带表(调用方没声明同义工具)则维持
    /// 原收口行为(出字前 = EmptyResponse 错误)。
    #[tokio::test]
    async fn 内建终端调用被翻译成_tool_use_下发() {
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
            vec![Ok(bytes::Bytes::from(wire::frame(&builtin_terminal_wire("ls -la"))))];
        let xlate = BuiltinXlate {
            terminal: Some(("Bash".into(), "command".into())),
            read_file: None,
        };
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            None,
            None,
            "conv-xlate".into(),
            std::sync::Arc::new(crate::AssetStore::default()),
            std::sync::Arc::new(crate::TruncationNotices::default()),
            xlate,
            ChatUsage { input_tokens: 10, ..Default::default() },
            0,
            false, // cli_mode:测试默认 IDE 形态
        );
        use futures::StreamExt;
        let items: Vec<_> = out.collect().await;
        assert!(
            !items.iter().any(|it| it.is_err()),
            "翻译成功的内建调用不许再报错收口"
        );
        let start = items
            .iter()
            .find_map(|it| match it {
                Ok(StreamItem::Sse(e))
                    if e.event == "content_block_start"
                        && e.data["content_block"]["type"] == "tool_use" =>
                {
                    Some(e)
                }
                _ => None,
            })
            .expect("必须下发 tool_use 块");
        assert_eq!(start.data["content_block"]["name"], "Bash");
        assert_eq!(start.data["content_block"]["id"], "call-b-0");
        let delta = items
            .iter()
            .find_map(|it| match it {
                Ok(StreamItem::Sse(e))
                    if e.event == "content_block_delta"
                        && e.data["delta"]["type"] == "input_json_delta" =>
                {
                    Some(e)
                }
                _ => None,
            })
            .expect("参数必须经 input_json_delta 下发");
        let input: Value =
            serde_json::from_str(delta.data["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(input, json!({"command": "ls -la"}));

        // 反例:空转换表 → 维持收口(出字前收到内建调用 = EmptyResponse)。
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
            vec![Ok(bytes::Bytes::from(wire::frame(&builtin_terminal_wire("ls -la"))))];
        let out = stream_to_anthropic(
            futures::stream::iter(chunks),
            "claude-fable-5".into(),
            false,
            None,
            None,
            "conv-xlate-2".into(),
            std::sync::Arc::new(crate::AssetStore::default()),
            std::sync::Arc::new(crate::TruncationNotices::default()),
            BuiltinXlate::default(),
            ChatUsage::default(),
            0,
            false, // cli_mode:测试默认 IDE 形态
        );
        let items: Vec<_> = out.collect().await;
        assert!(
            items.iter().any(|it| it.is_err()),
            "没有转换表时必须维持原收口行为(纠偏机制兜底)"
        );
    }

    /// 文案断言用:带**内置默认**策略句的纯函数版,不碰进程全局。
    fn guard(tools: &[run::ToolDef]) -> String {
        builtin_tool_guard_with(tools, crate::DEFAULT_TOOL_GUARD_POLICY)
    }

    fn tdefs(names: &[&str]) -> Vec<run::ToolDef> {
        names
            .iter()
            .map(|n| run::ToolDef {
                name: (*n).to_string(),
                description: String::new(),
                schema: "{}".to_string(),
            })
            .collect()
    }

    #[test]
    fn 护栏逐个列出可用工具全名() {
        // 列名是闭集,比任何否定句都硬 —— 这是第三版护栏的主要杠杆。
        let g = guard(&tdefs(&["Read", "Bash"]));
        assert!(g.contains("`gwtools-Read`"), "{g}");
        assert!(g.contains("`gwtools-Bash`"), "{g}");
        assert!(g.contains("这 2 个"), "{g}");
        assert!(g.contains("清单之外**不存在**任何其他工具"), "{g}");
    }

    #[test]
    fn 护栏给能力替代表而不是只列禁令() {
        // 生产 302 次内建收口里模型要的就是 shell 与搜代码,必须有明确去处。
        let g = guard(&tdefs(&["Read", "Bash", "Grep", "Edit", "WebSearch"]));
        assert!(g.contains("跑命令/终端调用 `gwtools-Bash`"), "{g}");
        assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");
        assert!(g.contains("搜代码/找文件调用 `gwtools-Grep`"), "{g}");
        assert!(g.contains("查网页调用 `gwtools-WebSearch`"), "{g}");
    }

    /// gpt-5.6-sol 评审的三处删减,钉成断言 —— 这三句删掉是**刻意的**,
    /// 后果威胁与内建工具点名单要作为热配置里的实验变体存在,不进第一版默认。
    /// 没有这个测试,下一个人「顺手把话说狠一点」就会把第二版翻车的原因原样种回来。
    #[test]
    fn 默认护栏不含后果威胁也不点名内建工具() {
        let g = guard(&tdefs(&["Read", "Bash", "Edit"]));
        assert!(!g.contains("截断"), "后果威胁不进默认版: {g}");
        assert!(!g.contains("半句话"), "后果威胁不进默认版: {g}");
        assert!(!g.contains("Cursor"), "不点名 Cursor 内建工具: {g}");
        assert!(!g.contains("那类调用"), "指代不清的说法要删掉: {g}");
        // 保留的那半句:「不会返回任何结果」+「别解释命名规则」。
        assert!(g.contains("不会返回任何结果"), "{g}");
        assert!(g.contains("不要向用户解释这套工具命名规则"), "{g}");
    }

    #[test]
    fn 替代表按匹配紧密度取而不是声明顺序() {
        // TodoWrite 子串里带 write,排在 Write 前面时不能把「写文件」指向改待办的工具。
        let g = guard(&tdefs(&["TodoWrite", "Write"]));
        assert!(g.contains("写/改文件调用 `gwtools-Write`"), "{g}");
        assert!(!g.contains("`gwtools-TodoWrite`。"), "不该把写文件指向改待办的工具: {g}");
        // 只有 TodoWrite 时:纯子串档已被去掉,该能力干脆不出替代行(宁缺勿错)。
        let only_todo = guard(&tdefs(&["TodoWrite"]));
        assert!(!only_todo.contains("写/改文件"), "{only_todo}");
        // 同理 Thread 不该被当成读文件工具(子串 read 命中过的那个坑)。
        let thread = guard(&tdefs(&["Thread"]));
        assert!(!thread.contains("读文件"), "{thread}");
        // 而 read_file / run_terminal_cmd 这类真别名要认得出。
        let aliases = guard(&tdefs(&["read_file", "run_terminal_cmd", "codebase_search"]));
        assert!(aliases.contains("读文件调用 `gwtools-read_file`"), "{aliases}");
        assert!(aliases.contains("跑命令/终端调用 `gwtools-run_terminal_cmd`"), "{aliases}");
        assert!(aliases.contains("搜代码/找文件调用 `gwtools-codebase_search`"), "{aliases}");
    }

    #[test]
    fn 没有对应工具的能力不出替代行() {
        // 指向一个不存在的工具比不指更糟。
        let g = guard(&tdefs(&["Read"]));
        assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");
        assert!(!g.contains("跑命令/终端"), "{g}");
        assert!(!g.contains("查网页"), "{g}");
    }

    #[test]
    fn 一个可替代工具都没有时给兜底话术() {
        let g = guard(&tdefs(&["TaskCreate", "Skill"]));
        assert!(g.contains("`gwtools-TaskCreate`"), "{g}");
        assert!(g.contains("直接向用户说明你需要什么"), "{g}");
    }

    #[test]
    fn 工具过多时不列名只留通则() {
        // 名字长度不定,所以按**字符预算**而不是个数卡:堆到超预算为止。
        let many: Vec<String> = (0..400).map(|i| format!("tool_number_{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let g = guard(&tdefs(&refs));
        assert!(!g.contains("`gwtools-tool_number_0`"), "超预算不该逐个列名: {g}");
        assert!(g.contains("清单之外**不存在**任何其他工具"), "{g}");
        // 预算内则要列。
        let few: Vec<&str> = refs[..5].to_vec();
        assert!(guard(&tdefs(&few)).contains("`gwtools-tool_number_0`"));
    }

    #[test]
    fn 无工具时仍是那版无工具约束() {
        let g = guard(&[]);
        assert!(g.contains("没有任何工具的环境"), "{g}");
        assert!(g.contains("无法得知当前日期"), "{g}");
        assert!(!g.contains("gwtools-"), "{g}");
    }

    #[test]
    fn 策略句可热改且空值回内置默认() {
        let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = crate::set_tool_guard_policy("");
            }
        }
        let _r = Restore;

        crate::set_tool_guard_policy("测试策略句:只准回答 PINEAPPLE。").unwrap();
        let g = builtin_tool_guard(&tdefs(&["Read"]));
        assert!(g.contains("只准回答 PINEAPPLE"), "{g}");
        assert!(!g.contains("不会返回任何结果"), "热配版应整段顶掉默认策略句: {g}");
        // 闭集与替代表是**代码生成**的,不受配置影响 —— 这是不做占位符模板的理由。
        assert!(g.contains("`gwtools-Read`"), "{g}");
        assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");

        // 空 = 回内置默认。
        crate::set_tool_guard_policy("   ").unwrap();
        assert!(builtin_tool_guard(&tdefs(&["Read"])).contains("不会返回任何结果"));
    }

    #[test]
    fn 策略句过长被拒且保留上一份有效值() {
        let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = crate::set_tool_guard_policy("");
            }
        }
        let _r = Restore;

        crate::set_tool_guard_policy("良好策略句 ALPHA").unwrap();
        let too_long = "啊".repeat(3000);
        let err = crate::set_tool_guard_policy(&too_long).expect_err("过长必须被拒");
        assert!(err.contains("过长"), "{err}");
        // **不静默回默认**:护栏效果正在按 guard_rev 分桶比对,悄悄换一版会让数据作废。
        let g = builtin_tool_guard(&tdefs(&["Read"]));
        assert!(g.contains("ALPHA"), "校验失败要保留上一份有效值: {g}");
    }

    #[test]
    fn 策略句变化会改指纹() {
        let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = crate::set_tool_guard_policy("");
            }
        }
        let _r = Restore;

        crate::set_tool_guard_policy("").unwrap();
        let a = crate::tool_guard_rev();
        crate::set_tool_guard_policy("换一版文案").unwrap();
        let b = crate::tool_guard_rev();
        assert_ne!(a, b, "分桶字段必须随文案变");
        assert_eq!(a.len(), 8, "短指纹 4 字节 = 8 hex");
    }

    #[test]
    fn 截断后下一轮注入纠偏且只注入一次() {
        let n = crate::TruncationNotices::default();
        // 认得出能力 + 调用方声明了同义工具 → 指名换哪个。
        n.record("conv-a", Some("跑命令/终端"));
        let cap = n.take("conv-a").expect("标记应在");
        let notice = truncation_notice(cap, &tdefs(&["Bash", "Read"]));
        assert!(notice.contains("上一轮中断"), "{notice}");
        assert!(notice.contains("`gwtools-Bash`"), "{notice}");
        // 消费语义:取过就没了,不会在整个会话里反复出现。
        assert!(n.take("conv-a").is_none(), "标记应已被消费");
    }

    #[test]
    fn 认不出能力时纠偏给通用话术不瞎指工具() {
        // 「你上次调了内建终端」这种自信的错话比模糊的实话更容易把模型带偏。
        let notice = truncation_notice(None, &tdefs(&["Bash", "Read"]));
        assert!(notice.contains("上一轮中断"), "{notice}");
        assert!(!notice.contains("`gwtools-Bash`"), "认不出就别指名: {notice}");
        assert!(notice.contains("gwtools-"), "仍要说清只能用带前缀的那些: {notice}");

        // 认得出能力、但调用方没声明同义工具 → 同样退回通用话术。
        let notice = truncation_notice(Some("跑命令/终端"), &tdefs(&["TaskCreate"]));
        assert!(!notice.contains("`gwtools-TaskCreate`"), "{notice}");
    }

    #[test]
    fn 空会话id不记纠偏标记() {
        // conversation_id 兜底失败时是空串;拿它当 key 会让所有无 id 请求串成一个。
        let n = crate::TruncationNotices::default();
        n.record("", Some("读文件"));
        assert!(n.take("").is_none());
    }

    #[test]
    fn 内建身份字段号翻成能力说法() {
        // 只认抓包实证过的两个(§13.2:.1 终端、.4 读文件),其余一律 None。
        // 不认识的返回 None 是**刻意**的:枚举要靠抓实物定死,不靠字段号相邻猜。
        let frame = |ident: u32| {
            let mut inner = crate::protobuf::Writer::new();
            inner.string(1, "ls -la");
            let mut detail = crate::protobuf::Writer::new();
            detail.message(ident, &inner);
            let mut ch = crate::protobuf::Writer::new();
            ch.string(1, "call-1");
            ch.message(2, &detail);
            let mut msg = crate::protobuf::Writer::new();
            msg.message(2, &ch);
            let mut top = crate::protobuf::Writer::new();
            top.message(1, &msg);
            top.into_bytes()
        };
        assert_eq!(builtin_capability(&frame(1)), Some("跑命令/终端"));
        assert_eq!(builtin_capability(&frame(4)), Some("读文件"));
        assert_eq!(builtin_capability(&frame(7)), None, "没实证过的字段号不许猜");
        // 外部工具(.15)走 parse_tool_call,不该被当成内建。
        assert_eq!(builtin_capability(&frame(15)), None);
    }
}
