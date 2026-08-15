//! 服务端 web search 执行器(修复 ccmax/kiro「用不了 web search」)。
//!
//! # 背景(根因)
//! Anthropic 的 `web_search_20250305` 是**服务端工具**:客户端声明它,期待 Anthropic
//! 服务端跑搜索、回 `web_search_tool_result`,模型据此直接出答案。但我们的上游都**不替它
//! 执行**这一步:
//! - dario 为伪装 Claude Code,把它改写成 CC 的**客户端函数型** `WebSearch`,Anthropic 只
//!   回一个客户端 `tool_use` 就停;客户端发的是 server 工具、没有执行器 → 死局/空。
//! - Kiro/CodeWhisperer 后端同理不跑 Anthropic 服务端搜索。
//!
//! 即:**搜索这一步在整条链路上没有任何人执行**。本模块让反代**自己执行**搜索(DuckDuckGo,
//! 无需 API key),把结果注回对话续跑,最后组装成标准的 `server_tool_use` +
//! `web_search_tool_result` 响应——对客户端透明,与原生服务端 web search 同形。
//!
//! # 隔离原则
//! 只在客户端请求**确实声明了服务端 web 工具**(`type` 以 `web_search` 开头)时激活;真 CC
//! 的函数型 `WebSearch`(客户端自执行)与所有其它流量**完全不经过本模块**,行为字节一致。
//!
//! # 已知限制(对抗审查 deferred,均非回归且失败=优雅降级而非崩)
//! - **回环内 re-call 不复用 worker 的 403 刷新/换号**:token 在轮次间过期 → 优雅降级
//!   (用已得内容收尾,保 usage),不放大成 502。轮间隔仅数秒,过期概率低。
//! - **Kiro 多轮缓存失真**:追加 tool_result user 消息会改变 Kiro 由「前 2 条 user」派生的
//!   conversationId → 第 2 轮缓存 miss(仅成本,功能正常)。dario 不受影响(按首条 user 文本锚定)。
//! - **流式为缓冲编排**:stream 客户端要等全部搜索+轮次跑完才见首字节(原生服务端搜索也有延迟)。

use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use serde_json::{json, Value};

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::{CallCtx, ChatRequest, ChatStream, ChatUsage, Provider, SseEvent, StreamItem};

/// DuckDuckGo lite 端点(无 key、返回真实直链,从数据中心 IP 实测可用)。
const DDG_LITE_URL: &str = "https://lite.duckduckgo.com/lite/";
/// 浏览器 UA(lite 端点对默认 reqwest UA 会降级/拒绝)。
const SEARCH_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// 每次搜索返回给模型的结果条数。
const RESULT_COUNT: usize = 6;
/// 客户端没给 `max_uses` 时的默认上限。
const DEFAULT_MAX_USES: u32 = 5;
/// 单次 web search 请求**总轮数**硬上限(防上游不收敛把回环跑飞)。须 > 任何 max_uses + 收尾轮。
const HARD_ROUND_CAP: u32 = 12;
/// `max_uses` 钳制上限(留出收尾轮空间,远小于 HARD_ROUND_CAP)。
const MAX_USES_CAP: u32 = 8;
/// DDG 单次请求超时(压低以控制缓冲编排的尾延迟)。
const SEARCH_TIMEOUT_SECS: u64 = 8;
/// `collect` 抽干事件数硬上限(OOM 护栏;超出视为异常上游,受控失败)。
const MAX_EVENTS: usize = 500_000;

/// 客户端声明的服务端 web search 工具规格。
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchSpec {
    /// 工具名(模型回传 `tool_use.name` 通常与之一致,如 `web_search`)。
    pub name: String,
    /// 最多执行几次搜索。
    pub max_uses: u32,
}

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 一次搜索的结果,区分「真零结果」与「检索失败(captcha/限流/网络)」。
enum SearchOutcome {
    Results(Vec<SearchResult>),
    /// 检索失败:喂给模型「暂时不可用」而非「没搜到」,避免误导出事实性空答。
    Unavailable,
}

/// 探测请求体里有没有**服务端** web search 工具(`type` 以 `web_search` 开头)。
///
/// 返回 `None` = 普通流量,调用方走原路径(零行为改变)。`web_fetch` 等其它服务端工具
/// 不在本模块处理范围,留作 `None`(模型若调用,tool_use 原样透传给客户端)。
pub fn detect_web_search(body: &Value) -> Option<WebSearchSpec> {
    let tools = body.get("tools")?.as_array()?;
    for t in tools {
        let ty = t.get("type").and_then(Value::as_str).unwrap_or("");
        if ty.starts_with("web_search") {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("web_search")
                .to_string();
            let max_uses = t
                .get("max_uses")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_USES as u64)
                .clamp(1, MAX_USES_CAP as u64) as u32;
            return Some(WebSearchSpec { name, max_uses });
        }
    }
    None
}

/// 一个块是否是 web search 的工具调用(容忍上游把名字呈现为 `web_search` 或 CC 的 `WebSearch`)。
fn is_web_search_call(block: &Value, spec: &WebSearchSpec) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return false;
    }
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    name == spec.name
        || name.eq_ignore_ascii_case("websearch")
        || name.eq_ignore_ascii_case("web_search")
}

// ── DuckDuckGo 后端 ──────────────────────────────────────────────────────────

fn link_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?s)<a\b([^>]*?result-link[^>]*?)>(.*?)</a>").unwrap())
}
fn href_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"href=['"]?([^'" >]+)"#).unwrap())
}
fn snippet_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?s)<td[^>]*?result-snippet[^>]*?>(.*?)</td>").unwrap())
}
fn tag_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<[^>]+>").unwrap())
}

/// 极简 HTML 实体反转义(覆盖 DDG lite 实际出现的实体;未知实体原样保留)。
fn html_unescape(s: &str) -> String {
    let mut out = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&#47;", "/")
        .replace("&nbsp;", " ");
    if out.contains("&#") {
        let num_re = {
            static RE: OnceLock<regex::Regex> = OnceLock::new();
            RE.get_or_init(|| regex::Regex::new(r"&#(\d{1,5});").unwrap())
        };
        out = num_re
            .replace_all(&out, |c: &regex::Captures| {
                c[1]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
    }
    out
}

fn strip_tags(s: &str) -> String {
    html_unescape(tag_re().replace_all(s, "").trim())
}

/// 解析 DDG lite 响应 HTML → 结果列表(纯函数,可测)。
///
/// 按**文档字节偏移**把每个 `result-link` 与紧跟其后、下一个 link 之前的 `result-snippet`
/// 关联(广告条仍占 snippet 单元格,纯位置 zip 会串位,故用偏移区间匹配)。过滤广告
/// (`y.js` 跳转 / `/l/` 重定向 / 「more info」/ 非 http)。
pub fn parse_ddg_lite(html: &str) -> Vec<SearchResult> {
    let links: Vec<(usize, &str, &str)> = link_re()
        .captures_iter(html)
        .map(|c| {
            let m = c.get(0).unwrap();
            (
                m.start(),
                c.get(1).map(|x| x.as_str()).unwrap_or(""),
                c.get(2).map(|x| x.as_str()).unwrap_or(""),
            )
        })
        .collect();
    let snips: Vec<(usize, String)> = snippet_re()
        .captures_iter(html)
        .map(|c| {
            let m = c.get(0).unwrap();
            (m.start(), strip_tags(c.get(1).map(|x| x.as_str()).unwrap_or("")))
        })
        .collect();

    let mut out = Vec::new();
    for (i, (off, attrs, inner)) in links.iter().enumerate() {
        let Some(href) = href_re()
            .captures(attrs)
            .and_then(|c| c.get(1))
            .map(|m| html_unescape(m.as_str()))
        else {
            continue;
        };
        let title = strip_tags(inner);
        if !href.starts_with("http")
            || href.contains("y.js")
            || href.contains("duckduckgo.com/l")
            || title.is_empty()
            || title.eq_ignore_ascii_case("more info")
        {
            continue;
        }
        let next_off = links.get(i + 1).map(|l| l.0).unwrap_or(usize::MAX);
        let snippet = snips
            .iter()
            .find(|(so, _)| *so > *off && *so < next_off)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url: href,
            snippet,
        });
    }
    out
}

/// 执行一次 DuckDuckGo 搜索(经系统 `curl`)。
///
/// 区分三态:有结果 / 真零结果(200 但无条目)/ 检索不可用(curl 失败、非 2xx、captcha/anomaly 页)。
/// **经 curl 而非 reqwest**:DDG lite 反爬按 **TLS ClientHello 指纹**拦截 reqwest/rustls(实测
/// rustls→anomaly 拦截页;curl/python 的 OpenSSL 指纹→正常结果)。curl 调用与 provider/egress 的
/// reqwest TLS 栈**完全隔离**,绝不影响 Kiro/dario 发包指纹(零封号风险)。浏览器头三件套
/// (Accept/Accept-Language/Referer)必带,否则即便 curl 也会被判机器人。query 经独立 arg
/// 传入(非 shell 解释),无注入。**绝不** panic、绝不把检索失败伪装成「没搜到」。
async fn ddg_search(query: &str) -> SearchOutcome {
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .arg("-m")
        .arg(SEARCH_TIMEOUT_SECS.to_string())
        .arg("-A")
        .arg(SEARCH_UA)
        .arg("-H")
        .arg("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .arg("-H")
        .arg("Accept-Language: en-US,en;q=0.9")
        .arg("-H")
        .arg("Referer: https://lite.duckduckgo.com/")
        .arg("--data-urlencode")
        .arg(format!("q={query}"))
        .arg(DDG_LITE_URL)
        .output()
        .await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!("DDG curl 非零退出 query={query:?} code={:?}", o.status.code());
            return SearchOutcome::Unavailable;
        }
        Err(e) => {
            tracing::warn!("DDG curl 启动失败 query={query:?}: {e}");
            return SearchOutcome::Unavailable;
        }
    };
    let html = String::from_utf8_lossy(&output.stdout);
    // captcha / 拦截页:lite 正常页一定含 result-link 结构;无 result-link 且含 anomaly → 不可用。
    if !html.contains("result-link") && html.contains("anomaly") {
        tracing::warn!("DDG 疑似拦截页 query={query:?}");
        return SearchOutcome::Unavailable;
    }
    let mut results = parse_ddg_lite(&html);
    results.truncate(RESULT_COUNT);
    SearchOutcome::Results(results)
}

// ── Anthropic 响应块组装(纯函数) ────────────────────────────────────────────

fn server_tool_use_block(id: &str, name: &str, query: &str) -> Value {
    json!({"type":"server_tool_use","id":id,"name":name,"input":{"query":query}})
}

fn web_search_tool_result_block(id: &str, outcome: &SearchOutcome) -> Value {
    match outcome {
        SearchOutcome::Unavailable => json!({
            "type":"web_search_tool_result","tool_use_id":id,
            "content":{"type":"web_search_tool_result_error","error_code":"unavailable"}
        }),
        SearchOutcome::Results(results) => {
            let items: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "type":"web_search_result",
                        "url": r.url, "title": r.title,
                        "page_age": null, "encrypted_content": ""
                    })
                })
                .collect();
            json!({"type":"web_search_tool_result","tool_use_id":id,"content":items})
        }
    }
}

/// 喂回模型的 tool_result 文本(模型据此出答案;含 snippet 供模型理解)。
fn results_to_text(query: &str, outcome: &SearchOutcome) -> String {
    match outcome {
        SearchOutcome::Unavailable => format!(
            "Web search for \"{query}\" is temporarily unavailable (the search backend returned an error). Answer from your own knowledge and tell the user the live search could not be run."
        ),
        SearchOutcome::Results(results) if results.is_empty() => {
            format!("No web search results found for \"{query}\".")
        }
        SearchOutcome::Results(results) => {
            let mut s = format!("Web search results for \"{query}\":\n\n");
            for (i, r) in results.iter().enumerate() {
                s.push_str(&format!("{}. {}\n{}\n", i + 1, r.title, r.url));
                if !r.snippet.is_empty() {
                    s.push_str(&r.snippet);
                    s.push('\n');
                }
                s.push('\n');
            }
            s
        }
    }
}

fn usage_json(u: &ChatUsage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_read_input_tokens": u.cache_read_tokens,
        "cache_creation_input_tokens": u.cache_creation_tokens,
    })
}

/// 把组装好的完整 message 合成 Anthropic SSE 事件序列(逐块 start/delta/stop)。
///
/// **每个 data 负载都带顶层 `type`**(`content_block_start`/`_delta`/`_stop`/`message_delta`),
/// 与上游真实流字节同形——Anthropic SDK 风格的流解析器按 `data.type` 派发,缺它会炸
/// (对抗审查 H1)。同时能被 [`gw_core::fold::fold_sse_to_message`] 无损折回。
pub fn synth_sse(message: &Value, usage: &ChatUsage) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let id = message.get("id").and_then(Value::as_str).unwrap_or("msg_ws");
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let stop_reason = message
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");

    events.push(SseEvent::new(
        "message_start",
        json!({"type":"message_start","message":{
            "id": id, "type":"message", "role":"assistant", "model": model,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": usage.input_tokens, "output_tokens": 0}
        }}),
    ));

    let cb_start = |idx: usize, block: Value| {
        SseEvent::new(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,"content_block":block}),
        )
    };
    let cb_delta = |idx: usize, delta: Value| {
        SseEvent::new(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,"delta":delta}),
        )
    };
    let cb_stop = |idx: usize| {
        SseEvent::new(
            "content_block_stop",
            json!({"type":"content_block_stop","index":idx}),
        )
    };

    let empty = Vec::new();
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for (idx, block) in content.iter().enumerate() {
        let bt = block.get("type").and_then(Value::as_str).unwrap_or("");
        match bt {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                events.push(cb_start(idx, json!({"type":"text","text":""})));
                events.push(cb_delta(idx, json!({"type":"text_delta","text":text})));
                events.push(cb_stop(idx));
            }
            "thinking" => {
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                let sig = block.get("signature").and_then(Value::as_str).unwrap_or("");
                events.push(cb_start(
                    idx,
                    json!({"type":"thinking","thinking":"","signature":""}),
                ));
                events.push(cb_delta(idx, json!({"type":"thinking_delta","thinking":thinking})));
                if !sig.is_empty() {
                    events.push(cb_delta(idx, json!({"type":"signature_delta","signature":sig})));
                }
                events.push(cb_stop(idx));
            }
            "server_tool_use" | "tool_use" => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let mut skeleton = block.clone();
                if let Some(obj) = skeleton.as_object_mut() {
                    obj.insert("input".into(), json!({}));
                }
                events.push(cb_start(idx, skeleton));
                events.push(cb_delta(
                    idx,
                    json!({"type":"input_json_delta","partial_json": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into())}),
                ));
                events.push(cb_stop(idx));
            }
            // web_search_tool_result 及其它块:整块经 content_block_start 透传(fold 原样保留)。
            _ => {
                events.push(cb_start(idx, block.clone()));
                events.push(cb_stop(idx));
            }
        }
    }

    events.push(SseEvent::new(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":usage.output_tokens}}),
    ));
    events.push(SseEvent::new("message_stop", json!({"type":"message_stop"})));
    events
}

// ── 编排回环 ─────────────────────────────────────────────────────────────────

/// 从一个 provider 流抽干出 SSE 事件 + 终结 usage。超出事件硬上限 → 受控失败(不静默截断)。
async fn collect(mut stream: ChatStream) -> Result<(Vec<SseEvent>, ChatUsage), UpstreamError> {
    let mut events = Vec::new();
    let mut usage = ChatUsage::default();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => {
                if events.len() >= MAX_EVENTS {
                    return Err(UpstreamError::new(
                        UpstreamErrorKind::ServerError,
                        "web search 回环:上游事件数超硬上限",
                    ));
                }
                events.push(ev);
            }
            Ok(StreamItem::Usage(u)) => usage = u,
            // 掐流信号(仅 Kiro 发):websearch 回环只关心事件与用量,忽略。
            Ok(StreamItem::UpstreamCut) => {}
            Err(e) => return Err(e),
        }
    }
    Ok((events, usage))
}

fn add_usage(mut a: ChatUsage, b: ChatUsage) -> ChatUsage {
    a.input_tokens += b.input_tokens;
    a.output_tokens += b.output_tokens;
    a.cache_read_tokens += b.cache_read_tokens;
    a.cache_creation_tokens += b.cache_creation_tokens;
    a.real_cache_read_tokens += b.real_cache_read_tokens;
    a.metering_credit += b.metering_credit;
    a
}

/// 收尾轮:剔除 web search 工具(按 type,镜像探测语义)并中和会指向它的 `tool_choice`,
/// 强制模型出文本答案、不再搜。
fn strip_web_search_tool(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|t| {
            !t.get("type")
                .and_then(Value::as_str)
                .map(|ty| ty.starts_with("web_search"))
                .unwrap_or(false)
        });
    }
    // tool_choice 若强制某工具(可能正是被剔除的 web_search)→ 改为 auto,避免引用已删工具 400。
    if let Some(obj) = body.as_object_mut() {
        if obj.contains_key("tool_choice") {
            obj.insert("tool_choice".into(), json!({"type":"auto"}));
        }
    }
}

/// 执行 web search 回环。消费首轮(由调用方发起,复用 worker 的 403 刷新)→ 发现 web_search
/// 调用就真搜+注回+续跑,直到模型出文本答案 / 达 max_uses / 触硬上限;最后组装成标准
/// `server_tool_use`+`web_search_tool_result`+text 响应,合成 SSE 返回(连同累计 usage)。
///
/// 失败语义:**仅首轮**失败上抛 `Err`(交 worker 上报+落库);后续轮失败/折叠失败 → **优雅降级**
/// (用已得内容收尾,保住已计费 usage,不放大成 502,符合 v60 不放大错误契约)。
/// 每发起一次**后续轮**上游调用前的准入回调(定频用):返回 `false` = 已达有效 RPM
/// 上限(含暖机),**不得再发** —— 回环按「后续轮失败」同款优雅降级收尾(已得内容
/// 照常返回、保 usage,不放大成 502,符合 v60 不放大错误契约)。
///
/// 为什么用回调而不是把 scheduler 传进来:websearch 不该依赖调度层。而漏记/漏拦会让
/// 一轮 web search 的 N 次续轮调用全部不计入 RPM —— 单个请求就能把号推过阈值。
pub type OnUpstreamCall<'a> = &'a (dyn Fn() -> bool + Send + Sync);

pub async fn run_loop(
    provider: Arc<dyn Provider>,
    ctx: &CallCtx,
    base_req: &ChatRequest,
    spec: WebSearchSpec,
    first_stream: ChatStream,
    on_upstream_call: OnUpstreamCall<'_>,
) -> Result<(Vec<SseEvent>, ChatUsage), UpstreamError> {
    let mut messages: Vec<Value> = base_req
        .body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut assembled: Vec<Value> = Vec::new();
    let mut total = ChatUsage::default();
    let mut searches: u32 = 0;
    let mut round: u32 = 0;
    let mut msg_id = String::new();
    let mut model = base_req.model.clone();
    let mut final_stop_reason = String::from("end_turn");
    let mut degraded = false; // 后续轮出错降级:收尾补一句说明。
    let mut cur_stream = first_stream;

    loop {
        round += 1;
        let (events, usage) = match collect(cur_stream).await {
            Ok(v) => v,
            Err(e) => {
                if round == 1 {
                    return Err(e); // 首轮失败:交 worker 走错误/重试路径。
                }
                degraded = true;
                break;
            }
        };
        total = add_usage(total, usage);
        let msg = match gw_core::fold::fold_sse_to_message(&events) {
            Ok(m) => m,
            Err(_) if round > 1 => {
                degraded = true;
                break;
            }
            Err(e) => {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    format!("web search 回环折叠失败: {e}"),
                ));
            }
        };
        if msg_id.is_empty() {
            if let Some(id) = msg.get("id").and_then(Value::as_str) {
                msg_id = id.to_string();
            }
            if let Some(m) = msg.get("model").and_then(Value::as_str) {
                model = m.to_string();
            }
        }
        let content: Vec<Value> = msg
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // 本轮所有 tool_use 调用。server-tool 模拟语义:客户端只声明了 web_search 这个**服务端**
        // 工具、不会执行任何 tool_use,故本轮每个调用都必须由我们代为消解,**绝不把无法执行的
        // tool_use 丢回客户端**(那正是本功能要消灭的死局)。
        let tool_calls: Vec<&Value> = content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .collect();

        // 终止:模型不再调工具(出了最终答案)或触硬上限。保留模型本轮真实 stop_reason。
        if tool_calls.is_empty() || round >= HARD_ROUND_CAP {
            if let Some(sr) = msg.get("stop_reason").and_then(Value::as_str) {
                final_stop_reason = sr.to_string();
            }
            assembled.extend(content);
            break;
        }

        // 检索前的 text/thinking 并入最终响应(保留模型的检索前叙述)。
        for b in &content {
            match b.get("type").and_then(Value::as_str) {
                Some("text") | Some("thinking") => assembled.push(b.clone()),
                _ => {}
            }
        }

        // 消解本轮**每个** tool_use(缺一条 tool_result 会致下一轮 400——对抗审查 H3):
        // web_search → 真搜并入标准 server_tool_use+web_search_tool_result 块;其它工具
        // (dario 注入的 CC 全家桶里 WebFetch 等,客户端无从执行)→ 回「不可用,据搜索结果作答」
        // 并标记强制收尾,保证客户端永远拿到完整答案。
        let mut tool_results: Vec<Value> = Vec::new();
        let mut force_answer = false;
        for tc in &tool_calls {
            let tc_id = tc.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if is_web_search_call(tc, &spec) && searches < spec.max_uses {
                searches += 1;
                let query = tc
                    .get("input")
                    .and_then(|i| i.get("query").or_else(|| i.get("search_term")))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let outcome = ddg_search(&query).await;
                let srv_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                assembled.push(server_tool_use_block(&srv_id, &spec.name, &query));
                assembled.push(web_search_tool_result_block(&srv_id, &outcome));
                tool_results.push(json!({
                    "type":"tool_result","tool_use_id": tc_id,
                    "content": results_to_text(&query, &outcome)
                }));
            } else {
                // 外部工具 / 超 max_uses 的 web_search → 回不可用,强制据已得结果作答。
                // 外部工具调用**不**进客户端可见输出(不属于 web search 契约)。
                force_answer = true;
                let tname = tc.get("name").and_then(Value::as_str).unwrap_or("that");
                tool_results.push(json!({
                    "type":"tool_result","tool_use_id": tc_id,
                    "content": format!("The `{tname}` tool is not available here. Answer the user directly using the web search results already provided; do not call any further tools.")
                }));
            }
        }

        // 历史接续:assistant(原样含 thinking+tool_use,dario 路径签名真且透传)+ user(全部 tool_result)。
        messages.push(json!({"role":"assistant","content": content}));
        messages.push(json!({"role":"user","content": tool_results}));

        let mut next_body = base_req.body.clone();
        next_body["messages"] = Value::Array(messages.clone());
        // 达 max_uses 或出现外部工具 → 剥 web 工具 + 中和 tool_choice(dario 见空 tools 数组即不注入
        // 工具,模型只能出文本),强制下一轮收尾作答。
        if searches >= spec.max_uses || force_answer {
            strip_web_search_tool(&mut next_body);
        }
        let next_req = ChatRequest::from_anthropic_body(next_body);
        // 定频准入:续轮同样是真实的上游调用(一轮 web search 可能有多次)。
        // 达有效 RPM 上限(含暖机)不再硬发 —— 按「后续轮失败」同款优雅降级收尾。
        if !on_upstream_call() {
            degraded = true;
            break;
        }
        match provider.chat(next_req, ctx).await {
            Ok(s) => cur_stream = s,
            Err(_) => {
                // 后续轮发起失败:优雅降级(已得搜索结果照常返回,保 usage)。
                degraded = true;
                break;
            }
        }
    }

    // 降级收尾:若没拿到模型的最终文本答案,补一句诚实说明(避免只回搜索块、无答案)。
    if degraded && !assembled.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("text")) {
        assembled.push(json!({"type":"text","text":"(已执行 web 搜索,但模型因上游临时错误未能生成最终回答,请重试。)"}));
    }

    let final_msg = json!({
        "id": if msg_id.is_empty() { "msg_ws".to_string() } else { msg_id },
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": assembled,
        "stop_reason": final_stop_reason,
        "stop_sequence": null,
        "usage": usage_json(&total),
    });
    let sse = synth_sse(&final_msg, &total);
    Ok((sse, total))
}

/// 把回环产出的事件 + usage 包成一个合成 `ChatStream`,交给现有 `finish_response` 收尾
/// (复用流式/非流式分发、日志、usage 上报全部机器)。
pub fn synth_stream(events: Vec<SseEvent>, usage: ChatUsage) -> ChatStream {
    let items: Vec<Result<StreamItem, UpstreamError>> = events
        .into_iter()
        .map(|e| Ok(StreamItem::Sse(e)))
        .chain(std::iter::once(Ok(StreamItem::Usage(usage))))
        .collect();
    Box::pin(futures::stream::iter(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HTML: &str = r##"
<table>
  <tr><td><a rel="nofollow" href="https://duckduckgo.com/y.js?ad_x=1" class='result-link'>Buy Messi Jersey - eBay</a></td></tr>
  <tr><td class='result-snippet'>Get Argentina Jersey on eBay. Fast shipping.</td></tr>
  <tr><td><a rel="nofollow" href="https://duckduckgo.com/y.js?ad_x=1" class="result-link">more info</a></td></tr>
  <tr><td valign="top">1.&nbsp;</td><td><a rel="nofollow" href="https://www.fifa.com/messi-2026" class='result-link'>Lionel Messi headlines Argentina squad - FIFA</a></td></tr>
  <tr><td>&nbsp;</td><td class='result-snippet'>Scaloni unveiled the 26-player squad for the World Cup.</td></tr>
  <tr><td valign="top">2.&nbsp;</td><td><a rel="nofollow" href="https://www.usatoday.com/messi" class='result-link'>Will Messi play in 2026? - USA Today</a></td></tr>
  <tr><td>&nbsp;</td><td class='result-snippet'>Messi is undecided about the 2026 World Cup.</td></tr>
</table>
"##;

    #[test]
    fn parse_filters_ads_and_aligns_snippets() {
        let r = parse_ddg_lite(SAMPLE_HTML);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://www.fifa.com/messi-2026");
        assert_eq!(r[0].title, "Lionel Messi headlines Argentina squad - FIFA");
        assert_eq!(r[0].snippet, "Scaloni unveiled the 26-player squad for the World Cup.");
        assert_eq!(r[1].url, "https://www.usatoday.com/messi");
        assert!(r[1].snippet.contains("undecided"));
    }

    #[test]
    fn parse_empty_html_is_empty() {
        assert!(parse_ddg_lite("<html>nothing here</html>").is_empty());
    }

    #[test]
    fn html_unescape_common_entities() {
        assert_eq!(html_unescape("a &amp; b &#x27;c&#x27; &#39;d&#39;"), "a & b 'c' 'd'");
        assert_eq!(html_unescape("Here&#x27;s"), "Here's");
    }

    #[test]
    fn detect_finds_server_web_search() {
        let body = json!({"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":8}]});
        let spec = detect_web_search(&body).expect("应探测到");
        assert_eq!(spec.name, "web_search");
        assert_eq!(spec.max_uses, 8);
    }

    #[test]
    fn detect_ignores_function_websearch() {
        let body = json!({"tools":[{"name":"WebSearch","description":"...","input_schema":{}}]});
        assert!(detect_web_search(&body).is_none());
    }

    #[test]
    fn detect_clamps_max_uses() {
        let body = json!({"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":999}]});
        assert_eq!(detect_web_search(&body).unwrap().max_uses, MAX_USES_CAP);
        let body0 = json!({"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":0}]});
        assert_eq!(detect_web_search(&body0).unwrap().max_uses, 1);
    }

    #[test]
    fn detect_missing_max_uses_defaults() {
        let body = json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]});
        assert_eq!(detect_web_search(&body).unwrap().max_uses, DEFAULT_MAX_USES);
    }

    #[test]
    fn is_web_search_call_tolerates_cc_name() {
        let spec = WebSearchSpec { name: "web_search".into(), max_uses: 5 };
        assert!(is_web_search_call(&json!({"type":"tool_use","name":"web_search"}), &spec));
        assert!(is_web_search_call(&json!({"type":"tool_use","name":"WebSearch"}), &spec));
        assert!(!is_web_search_call(&json!({"type":"tool_use","name":"Bash"}), &spec));
        assert!(!is_web_search_call(&json!({"type":"text","text":"hi"}), &spec));
    }

    #[test]
    fn strip_removes_web_search_by_type_and_neutralizes_tool_choice() {
        let mut body = json!({
            "tools":[{"type":"web_search_20250305","name":"web_search"},{"name":"Bash","input_schema":{}}],
            "tool_choice":{"type":"tool","name":"web_search"}
        });
        strip_web_search_tool(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(body["tool_choice"]["type"], "auto", "指向已删工具的 tool_choice 须中和");
    }

    #[test]
    fn strip_keeps_unrelated_tool_named_web_search() {
        // 普通函数工具恰好叫 web_search 但无 web_search type → 不该被剥(镜像探测=按 type)。
        let mut body = json!({"tools":[{"name":"web_search","input_schema":{}}]});
        strip_web_search_tool(&mut body);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    }

    // synth_sse → fold 往返:每类块不丢 + 每个事件 data 带顶层 type。
    #[test]
    fn synth_sse_folds_back_losslessly_and_has_type() {
        let outcome = SearchOutcome::Results(vec![SearchResult {
            title: "FIFA".into(),
            url: "https://fifa.com".into(),
            snippet: "squad".into(),
        }]);
        let msg = json!({
            "id":"msg_x","type":"message","role":"assistant","model":"claude-opus-4-8",
            "content":[
                server_tool_use_block("srvtoolu_1","web_search","messi 2026"),
                web_search_tool_result_block("srvtoolu_1", &outcome),
                {"type":"text","text":"Messi leads the squad."}
            ],
            "stop_reason":"end_turn","stop_sequence":null
        });
        let usage = ChatUsage { input_tokens: 10, output_tokens: 20, ..Default::default() };
        let events = synth_sse(&msg, &usage);

        // 每个事件 data 都有顶层 type(对抗审查 H1)。
        for e in &events {
            assert!(e.data.get("type").and_then(Value::as_str).is_some(), "事件 {} data 缺 type", e.event);
        }

        let folded = gw_core::fold::fold_sse_to_message(&events).expect("应折叠成功");
        let content = folded["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "server_tool_use");
        assert_eq!(content[0]["input"]["query"], "messi 2026");
        assert_eq!(content[1]["type"], "web_search_tool_result");
        assert_eq!(content[1]["content"][0]["url"], "https://fifa.com");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "Messi leads the squad.");
        assert_eq!(folded["stop_reason"], "end_turn");
    }

    #[test]
    fn results_to_text_distinguishes_empty_vs_unavailable() {
        assert!(results_to_text("foo", &SearchOutcome::Results(vec![])).contains("No web search results"));
        assert!(results_to_text("foo", &SearchOutcome::Unavailable).contains("temporarily unavailable"));
        let r = SearchOutcome::Results(vec![SearchResult{title:"T".into(),url:"https://u".into(),snippet:"S".into()}]);
        let t = results_to_text("foo", &r);
        assert!(t.contains("1. T") && t.contains("https://u") && t.contains('S'));
    }

    #[test]
    fn unavailable_result_block_is_error_shaped() {
        let b = web_search_tool_result_block("srvtoolu_1", &SearchOutcome::Unavailable);
        assert_eq!(b["content"]["type"], "web_search_tool_result_error");
        assert_eq!(b["content"]["error_code"], "unavailable");
    }
}
