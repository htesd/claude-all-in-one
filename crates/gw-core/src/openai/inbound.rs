//! 两种 OpenAI 入站格式**共用**的转换零件。
//!
//! ChatCompletions 与 Responses 的顶层形状差别很大(`messages` vs `input`),
//! 但工具定义、工具选择、内容分片、采样参数这几块几乎一致 —— 都放这里,
//! 免得两个 `*_req.rs` 各写一份然后慢慢漂开。
//!
//! 产物一律是 **Anthropic Messages** 的片段(块 / 工具 / 顶层字段),
//! 见 [`crate::provider`] 的「内部 IR = Anthropic Messages」。

use serde_json::{json, Map, Value};

/// 客户端没给 `max_tokens` / `max_output_tokens` 时注入的默认值。
///
/// Anthropic Messages 的 `max_tokens` 是**必填**,而 OpenAI 两种格式里它都是可选的
/// (不给 = 用模型上限)。所以缺席时必须我方补一个,否则转出来的 IR 不是合法请求。
///
/// 取 64k 而不是某个小值:这是「客户端没表达意见」的情形,补一个小上限等于**替客户
/// 截断回答**,而截断在流式里表现为「话说一半就停」,是最难被认出是网关干的那种故障。
pub const DEFAULT_MAX_TOKENS: u64 = 64_000;

/// 入站转换失败。`param` 指向出问题的字段,直接进 OpenAI 错误体的 `param`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    pub message: String,
    pub param: Option<String>,
}

impl ConvertError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            param: None,
        }
    }

    pub fn at(param: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            param: Some(param.into()),
        }
    }
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.param {
            Some(p) => write!(f, "{} (字段: {p})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

/// 顶层 `system` 的累积器:OpenAI 把 system/developer 当成消息数组里的一条,
/// Anthropic 把它放顶层。多条按出现顺序拼接(用空行分隔)。
///
/// 为什么**提升**而不是当普通消息保留:`docs/_refmatrix_sf_converter.md` 把这条列为
/// P1 借鉴项 —— 留在 messages 里会让上游把它当对话轮次,系统指令的权重完全不同;
/// 而且 Anthropic 只认第一条 system 之前的位置,夹在中间的 system 消息是非法的。
#[derive(Debug, Default)]
pub struct SystemAccum(Vec<String>);

impl SystemAccum {
    pub fn push(&mut self, text: impl Into<String>) {
        let t: String = text.into();
        if !t.trim().is_empty() {
            self.0.push(t);
        }
    }

    /// 拼成顶层 `system` 值;全空则 `None`(不要塞一个空 system,上游会拒)。
    pub fn finish(self) -> Option<Value> {
        if self.0.is_empty() {
            return None;
        }
        Some(Value::String(self.0.join("\n\n")))
    }
}

/// OpenAI 的 `content`(字符串或分片数组)→ Anthropic 内容块数组。
///
/// 认得的分片:`text` / `input_text` / `output_text`(都当文本)、
/// `image_url`(ChatCompletions 形状)、`input_image`(Responses 形状)。
/// 认不出的分片**跳过而不报错**:OpenAI 一直在加新分片类型,为一个我方不认识的
/// 附件把整条请求打回去,比丢掉那个附件更糟(客户端会以为是我方坏了)。
pub fn content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({"type": "text", "text": s})]
            }
        }
        Some(Value::Array(parts)) => parts.iter().filter_map(part_to_block).collect(),
        // 其余类型不是合法 content;交给下游 `validate_message_contents` 点名,这里不猜。
        Some(_) => Vec::new(),
    }
}

fn part_to_block(part: &Value) -> Option<Value> {
    let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" | "input_text" | "output_text" => {
            let t = part.get("text").and_then(Value::as_str)?;
            if t.is_empty() {
                return None;
            }
            Some(json!({"type": "text", "text": t}))
        }
        // ChatCompletions: {"type":"image_url","image_url":{"url":"..."}}
        "image_url" => {
            let url = part
                .get("image_url")
                .and_then(|u| u.get("url"))
                .or_else(|| part.get("image_url"))
                .and_then(Value::as_str)?;
            image_block(url)
        }
        // Responses: {"type":"input_image","image_url":"..."}
        "input_image" => {
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .or_else(|| part.get("image_url").and_then(|u| u.get("url")).and_then(Value::as_str))?;
            image_block(url)
        }
        _ => None,
    }
}

/// 图片 URL → Anthropic `image` 块。data URI 拆成 base64 源,http(s) 用 url 源。
pub fn image_block(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        // data:<media_type>;base64,<data>
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.split(';').next().unwrap_or("").trim();
        if media_type.is_empty() || !meta.contains("base64") {
            return None;
        }
        return Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({"type": "image", "source": {"type": "url", "url": url}}));
    }
    None
}

/// [`convert_tools`] 的产物:转好的工具 + **被丢掉的**工具类型。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSet {
    pub tools: Vec<Value>,
    /// 被丢掉的托管工具类型名(如 `web_search`)。
    ///
    /// 单独交出来是为了让它**可被观测**:客户端声明了我方没有的能力、我方照常回答,
    /// 这种降级在响应里看不见(Responses 侧的 `tools:[]` 回显能透出一点,
    /// ChatCompletions 侧完全无痕)。调用方据此打一条日志,把「静默降级」变成
    /// 一条能 grep 到的记录(对抗评审 Skeptic#7)。
    pub dropped: Vec<String>,
}

/// OpenAI 工具定义数组 → Anthropic `tools`。
///
/// 同时认两种形状:Responses 的**平铺** `{"type":"function","name":...,"parameters":...}`
/// 与 ChatCompletions 的**嵌套** `{"type":"function","function":{...}}`。
/// 非 function 类型(`web_search` / `code_interpreter` 等托管工具)**跳过** ——
/// 那是 OpenAI 服务端自己执行的东西,cursor 上游没有对应物。为一个我方不认识的工具
/// 把整条请求打回去比丢掉它更糟,但丢掉这件事必须留痕(见 [`ToolSet::dropped`])。
pub fn convert_tools(tools: Option<&Value>) -> ToolSet {
    let Some(arr) = tools.and_then(Value::as_array) else {
        return ToolSet::default();
    };
    let mut out = ToolSet::default();
    for t in arr {
        match convert_one_tool(t) {
            Some(tool) => out.tools.push(tool),
            None => {
                let kind = t
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("<无 type>")
                    .to_string();
                out.dropped.push(kind);
            }
        }
    }
    out
}

fn convert_one_tool(t: &Value) -> Option<Value> {
    let kind = t.get("type").and_then(Value::as_str);
    // 嵌套形状优先:ChatCompletions 的 name 在 function 里面。
    let spec = match t.get("function") {
        Some(f) if f.is_object() => f,
        _ => t,
    };
    // 有 name 就当函数工具;`type` 缺席(老客户端)也放行。
    if matches!(kind, Some(k) if k != "function") && t.get("function").is_none() {
        return None;
    }
    let name = spec.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    let mut tool = Map::new();
    tool.insert("name".into(), json!(name));
    if let Some(d) = spec.get("description").and_then(Value::as_str) {
        tool.insert("description".into(), json!(d));
    }
    // Anthropic 的 input_schema 必须是对象;缺席/非对象一律给一个空 object schema。
    let schema = spec
        .get("parameters")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    tool.insert("input_schema".into(), schema);
    Some(Value::Object(tool))
}

/// OpenAI `tool_choice` + `parallel_tool_calls` → Anthropic `tool_choice`。
///
/// `required` → Anthropic 的 `any`(「必须用某个工具」,名字不同语义相同)。
/// `parallel_tool_calls:false` 在 Anthropic 侧是 `tool_choice` 的兄弟键
/// `disable_parallel_tool_use`,所以两个字段必须一起算 —— 这也是为什么它们共用一个函数。
///
/// 返回 `None` = 不写 `tool_choice`(让上游用默认)。
pub fn convert_tool_choice(choice: Option<&Value>, parallel: Option<bool>) -> Option<Value> {
    let mut obj = match choice {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            // 认不出的字符串 = 不表态,交默认。
            _ => return parallel_only(parallel),
        },
        Some(Value::Object(o)) => {
            // {"type":"function","function":{"name":"x"}} 或 {"type":"function","name":"x"}
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| o.get("name"))
                .and_then(Value::as_str);
            match name {
                Some(n) => json!({"type": "tool", "name": n}),
                None => return parallel_only(parallel),
            }
        }
        _ => return parallel_only(parallel),
    };
    if parallel == Some(false) {
        if let Some(m) = obj.as_object_mut() {
            m.insert("disable_parallel_tool_use".into(), json!(true));
        }
    }
    Some(obj)
}

/// 客户端只说了 `parallel_tool_calls:false`、没说 `tool_choice` 时,仍要把这个意图带上。
fn parallel_only(parallel: Option<bool>) -> Option<Value> {
    (parallel == Some(false))
        .then(|| json!({"type": "auto", "disable_parallel_tool_use": true}))
}

/// `tools` 与 `tool_choice` 的**组合**校验。
///
/// 两者独立转换会造出上游必拒的 IR:客户端只声明了托管工具(`web_search` /
/// `code_interpreter`)时 [`convert_tools`] 会把它们全删掉,而 `tool_choice:"required"`
/// 照样转成了 `{"type":"any"}` —— 一个「必须调用工具」却**没有 tools** 的请求,
/// Anthropic 直接拒整条(对抗评审 Architect#5)。
///
/// - 有 tools:原样放行。
/// - 无 tools + 明确要求用工具(`any` / `tool`)→ **400**。客户的意图实现不了,
///   静默降级成普通回答会让他以为工具跑过了,那是最难查的一类"成功"。
/// - 无 tools + `auto` / `none`:是 no-op,丢掉(留着上游会拒)。
pub fn reconcile_tool_choice(
    has_tools: bool,
    choice: Option<Value>,
    param: &str,
) -> Result<Option<Value>, ConvertError> {
    let Some(c) = choice else {
        return Ok(None);
    };
    if has_tools {
        return Ok(Some(c));
    }
    match c.get("type").and_then(Value::as_str) {
        Some("any") | Some("tool") => Err(ConvertError::at(
            param,
            "请求要求必须调用工具,但声明的工具本网关都不支持(托管工具如 web_search / \
             code_interpreter 由 OpenAI 服务端执行,本通道没有对应能力)。请改用函数工具,\
             或把 tool_choice 改成 auto。",
        )),
        _ => Ok(None),
    }
}

/// `reasoning_effort` / `reasoning.effort` → Anthropic `thinking`。
///
/// 档位 → budget_tokens 的分档是**我方定的**(与 `gw-cursor` 的
/// `apply_thinking_pref` 反向对齐:那边再把 budget 折回 cursor 的 effort)。
/// 走一趟 Anthropic IR 会有一次量化损耗,但保住了「只有一种 IR」这条主干。
pub fn effort_to_thinking(effort: &str) -> Option<Value> {
    let budget = match effort.trim().to_ascii_lowercase().as_str() {
        // OpenAI 的 "minimal"/"none" 是明确的「别想」。
        "none" | "minimal" => return Some(json!({"type": "disabled"})),
        "low" => 4_096,
        "medium" => 16_384,
        "high" => 32_768,
        _ => return None,
    };
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

/// 采样参数直通(两种入站共用):同名同义的原样搬,不同名的按下表改名。
///
/// 只搬 Anthropic **认识**的键。多塞一个上游不认的键会让整条请求被
/// `invalid_argument` 拒掉 —— 与 `gw-cursor` 那条「只改目录里本来就有的键」同一个纪律。
pub fn copy_sampling(src: &Map<String, Value>, dst: &mut Map<String, Value>) {
    for k in ["temperature", "top_p", "top_k"] {
        if let Some(v) = src.get(k).filter(|v| v.is_number()) {
            dst.insert(k.to_string(), v.clone());
        }
    }
    // stop / stop_sequences → Anthropic 的 stop_sequences(只收字符串数组或单串)。
    let stop = src.get("stop").or_else(|| src.get("stop_sequences"));
    match stop {
        Some(Value::String(s)) => {
            dst.insert("stop_sequences".into(), json!([s]));
        }
        Some(Value::Array(a)) if a.iter().all(Value::is_string) && !a.is_empty() => {
            dst.insert("stop_sequences".into(), Value::Array(a.clone()));
        }
        _ => {}
    }
}

/// `tool_choice` 是不是「不许调用工具」。
///
/// 这条要**真的执行**,不能只写进 IR 就算完:cursor 的请求构造只读 `tools`,对
/// `tool_choice` 零读取(见 `gw-cursor/src/chat.rs` 只取 `body["tools"]`)。所以
/// 客户端说 `tool_choice:"none"`、我方却照样把 tools 发上去,模型完全可能调工具 ——
/// 一个能兑现的约束被写成了摆设(对抗评审 Minimalist#2)。
///
/// 兑现办法很直接:**把 tools 整个撤掉**。没有工具可用,模型自然调不了。
/// 这是唯一一个我方能在当前上游上真正执行的工具约束,所以单独拎出来。
pub fn tool_choice_forbids_tools(choice: Option<&Value>) -> bool {
    matches!(choice, Some(Value::String(s)) if s == "none")
        || matches!(
            choice.and_then(|c| c.get("type")).and_then(Value::as_str),
            Some("none")
        )
}

/// 把客户端给的**终端用户标识**转成 Anthropic 的 `metadata.user_id`。
///
/// ## 为什么这件事重要(不只是"透传个字段")
///
/// cursor 通道的会话身份默认是 `hash(system + 第一条 user)` —— **纯内容派生**。而 cursor
/// 是唯一有服务端会话续写的上游(`ConvRegistry::phase_for` → `Continuation`)。
/// 两个不同的终端用户只要 system 相同、开场那句话相同("hello" 就够了),就会拿到同一个
/// `conversation_id`,后来那位可能被当作前一位会话的续写发上去 = **跨用户串话**。
///
/// worker 侧已按**客户 API key** 分了一层命名空间,但那对多租户中转(NewAPI 这类)不够:
/// 中转对本网关只出示**一个** key,它背后所有终端用户共享它。
///
/// 所以要给中转一个能表达"这是哪个终端用户/哪个会话"的通道。OpenAI 两种格式都有现成字段
/// (`user` / `safety_identifier`),而 cursor 的 `affinity_key_from_body` **本来就优先**读
/// `metadata.user_id` 里的会话标识 —— 只要把它填上,内容哈希那条脆弱路径就整个绕开了。
///
/// 产物形态走 [`crate::routing::extract_session_from_metadata`] 的**形态 2**
/// (`session_<token>` 子串),字符集收窄到 `[A-Za-z0-9-]`:那个解析器只吃这些字符,
/// 不收窄的话带下划线/中文的 user 会被截断成另一个值(截断后仍稳定,但会把两个不同
/// 用户截成同一个 token —— 正是要防的那种合并)。
pub fn session_metadata(src: &Map<String, Value>) -> Option<Value> {
    // 优先级:显式会话 id > safety_identifier > user。前两个语义更"就是标识",
    // 而 `user` 在部分客户端里被塞成固定值(如 "user"),优先级放最后。
    let raw = ["session_id", "safety_identifier", "user"]
        .iter()
        .find_map(|k| src.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let token: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    // 全是非法字符 → 退化成全 `-`,那不是一个能区分用户的标识,不如不填。
    if token.chars().all(|c| c == '-') {
        return None;
    }
    Some(json!({ "user_id": format!("session_{token}") }))
}

/// 取 `max_tokens` 系列字段(按优先级),缺席回落 [`DEFAULT_MAX_TOKENS`]。
pub fn max_tokens_of(src: &Map<String, Value>) -> u64 {
    for k in ["max_output_tokens", "max_completion_tokens", "max_tokens"] {
        if let Some(n) = src.get(k).and_then(Value::as_u64).filter(|n| *n > 0) {
            return n;
        }
    }
    DEFAULT_MAX_TOKENS
}

/// 工具无输出时的占位文本。
///
/// **不能留空**:`[{"type":"text","text":""}]` 正是上游会拒的形态
/// (见 worker 的 `is_empty_message_content`:含空 text 块的数组按块校验直接失败)。
/// 而「工具跑完没有输出」是常态(写文件成功、命令无 stdout),不该让整条请求失败。
/// 占位文本是**我方合成的**,措辞上明说它是空的,不伪装成工具的真实返回。
const EMPTY_TOOL_OUTPUT: &str = "(工具无输出)";

/// 把 `tool_result` 的 `output` 归一成 Anthropic 认的形态。
///
/// OpenAI 的工具返回值可以是字符串、结构化对象,也可以是 Responses 的分片数组。
/// Anthropic 的 `tool_result.content` 只认 `text` / `image` 块 —— 所以数组要走
/// [`content_to_blocks`] 做**同一套**分片映射(`input_text` → `text` 等),
/// 不能只看「有 type 键」就原样透传:一个合法的 `input_text` 块塞过去,
/// 上游因为不认识这个块类型而拒掉整条请求(对抗评审 Skeptic#4)。
pub fn tool_output_to_content(output: Option<&Value>) -> Value {
    let blocks = match output {
        Some(Value::String(s)) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Some(v @ Value::Array(_)) => content_to_blocks(Some(v)),
        // 对象/数字/布尔:序列化成 JSON 文本(工具返回结构化数据是常见的)。
        Some(v) if !v.is_null() && !v.is_string() => {
            vec![json!({"type":"text","text": serde_json::to_string(v).unwrap_or_default()})]
        }
        _ => Vec::new(),
    };
    if blocks.is_empty() {
        return json!([{"type": "text", "text": EMPTY_TOOL_OUTPUT}]);
    }
    Value::Array(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字符串_content_转成单个文本块() {
        let blocks = content_to_blocks(Some(&json!("hello")));
        assert_eq!(blocks, vec![json!({"type": "text", "text": "hello"})]);
    }

    #[test]
    fn 空_content_产出零个块_而不是空文本块() {
        // 空文本块正是上游会拒的那种形态(见 worker 的 is_empty_message_content)。
        assert!(content_to_blocks(Some(&json!(""))).is_empty());
        assert!(content_to_blocks(Some(&json!([]))).is_empty());
        assert!(content_to_blocks(Some(&json!([{"type": "text", "text": ""}]))).is_empty());
        assert!(content_to_blocks(None).is_empty());
    }

    #[test]
    fn 两种图片分片形状都认() {
        let chat = json!([{"type":"image_url","image_url":{"url":"https://x/y.png"}}]);
        let resp = json!([{"type":"input_image","image_url":"https://x/y.png"}]);
        let want = vec![json!({"type":"image","source":{"type":"url","url":"https://x/y.png"}})];
        assert_eq!(content_to_blocks(Some(&chat)), want);
        assert_eq!(content_to_blocks(Some(&resp)), want);
    }

    #[test]
    fn data_uri_拆成_base64_源() {
        let b = image_block("data:image/png;base64,QUJD").unwrap();
        assert_eq!(
            b,
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}})
        );
        // 非 base64 的 data URI 与相对路径都不认(宁可丢附件,不要造一个上游必拒的块)。
        assert!(image_block("data:image/png,raw").is_none());
        assert!(image_block("/local/a.png").is_none());
    }

    #[test]
    fn 认不出的分片被跳过_而不是让整条请求失败() {
        let c = json!([
            {"type": "text", "text": "a"},
            {"type": "input_audio", "input_audio": {"data": "..."}},
            {"type": "text", "text": "b"},
        ]);
        assert_eq!(
            content_to_blocks(Some(&c)),
            vec![json!({"type":"text","text":"a"}), json!({"type":"text","text":"b"})]
        );
    }

    #[test]
    fn 工具定义_平铺与嵌套两种形状都认() {
        let flat = json!([{"type":"function","name":"get","description":"d",
                           "parameters":{"type":"object","properties":{"a":{"type":"string"}}}}]);
        let nested = json!([{"type":"function","function":{"name":"get","description":"d",
                             "parameters":{"type":"object","properties":{"a":{"type":"string"}}}}}]);
        let want = vec![json!({
            "name":"get","description":"d",
            "input_schema":{"type":"object","properties":{"a":{"type":"string"}}}
        })];
        assert_eq!(convert_tools(Some(&flat)).tools, want);
        assert_eq!(convert_tools(Some(&nested)).tools, want);
        assert!(convert_tools(Some(&flat)).dropped.is_empty());
    }

    #[test]
    fn 托管工具被跳过_缺_parameters_给空_schema() {
        let tools = json!([
            {"type": "web_search"},
            {"type": "function", "name": "noargs"},
        ]);
        let set = convert_tools(Some(&tools));
        assert_eq!(
            set.tools,
            vec![json!({"name":"noargs","input_schema":{"type":"object","properties":{}}})]
        );
        // 被丢掉的托管工具要留痕,否则静默降级永远查不到。
        assert_eq!(set.dropped, vec!["web_search".to_string()]);
    }

    #[test]
    fn tool_choice_三种字符串与点名形状() {
        assert_eq!(convert_tool_choice(Some(&json!("auto")), None), Some(json!({"type":"auto"})));
        // required → any,这是两边唯一名字不同、语义相同的一对。
        assert_eq!(convert_tool_choice(Some(&json!("required")), None), Some(json!({"type":"any"})));
        assert_eq!(convert_tool_choice(Some(&json!("none")), None), Some(json!({"type":"none"})));
        assert_eq!(
            convert_tool_choice(Some(&json!({"type":"function","function":{"name":"f"}})), None),
            Some(json!({"type":"tool","name":"f"}))
        );
        assert_eq!(
            convert_tool_choice(Some(&json!({"type":"function","name":"f"})), None),
            Some(json!({"type":"tool","name":"f"}))
        );
    }

    #[test]
    fn 串行工具意图在没有_tool_choice_时也不丢() {
        assert_eq!(
            convert_tool_choice(None, Some(false)),
            Some(json!({"type":"auto","disable_parallel_tool_use":true}))
        );
        assert_eq!(
            convert_tool_choice(Some(&json!("required")), Some(false)),
            Some(json!({"type":"any","disable_parallel_tool_use":true}))
        );
        // 没有任何表态就别写这个字段,让上游用自己的默认。
        assert_eq!(convert_tool_choice(None, None), None);
        assert_eq!(convert_tool_choice(None, Some(true)), None);
    }

    /// tools 与 tool_choice 必须一起判:独立转换会造出「要求必须用工具却没有 tools」
    /// 这种 Anthropic 必拒的组合(客户端只声明了托管工具时就会踩到)。
    #[test]
    fn 无_tools_时明确要求用工具是_400_而不是静默降级() {
        // 静默降级 = 客户以为工具跑过了,拿到一个没查过资料的回答 —— 最难查的那类"成功"。
        for c in [json!({"type":"any"}), json!({"type":"tool","name":"f"})] {
            let e = reconcile_tool_choice(false, Some(c.clone()), "tool_choice").unwrap_err();
            assert_eq!(e.param.as_deref(), Some("tool_choice"), "{c}");
        }
        // auto / none 在没有 tools 时是 no-op:丢掉(留着上游会拒),不报错。
        for c in [json!({"type":"auto"}), json!({"type":"none"})] {
            assert_eq!(reconcile_tool_choice(false, Some(c), "tool_choice").unwrap(), None);
        }
        // 有 tools 就原样放行。
        let c = json!({"type":"any"});
        assert_eq!(
            reconcile_tool_choice(true, Some(c.clone()), "tool_choice").unwrap(),
            Some(c)
        );
        // 压根没表态 → 不写这个字段。
        assert_eq!(reconcile_tool_choice(false, None, "tool_choice").unwrap(), None);
    }

    /// 多租户中转(NewAPI)对本网关只出示一个 API key,它背后所有终端用户共享它 ——
    /// 所以必须给它一条表达「这是哪个终端用户」的通道,否则 cursor 的会话身份只剩
    /// 内容哈希,不同用户同一句开场白就会被合并(还可能被服务端续写串到一起)。
    #[test]
    fn 终端用户标识被转成_cursor_认得的_metadata() {
        let md = |v: Value| session_metadata(v.as_object().unwrap());
        // 三个字段按优先级取。
        assert_eq!(
            md(json!({"user": "u-42"})),
            Some(json!({"user_id": "session_u-42"}))
        );
        assert_eq!(
            md(json!({"safety_identifier": "sid1", "user": "u-42"})),
            Some(json!({"user_id": "session_sid1"}))
        );
        assert_eq!(
            md(json!({"session_id": "s9", "safety_identifier": "sid1"})),
            Some(json!({"user_id": "session_s9"}))
        );
        // 产物必须能被 cursor 那边的解析器读回来(形态 2:`session_<token>`)。
        let out = md(json!({"user": "u-42"})).unwrap();
        let uid = out["user_id"].as_str().unwrap();
        assert_eq!(
            crate::routing::extract_session_from_metadata(uid).as_deref(),
            Some("session_u-42"),
            "填了却读不回来等于没填"
        );
        // 非法字符换成 `-`(解析器只吃 [A-Za-z0-9-],不换会被截断成另一个值)。
        let out = md(json!({"user": "张三_x"})).unwrap();
        let uid = out["user_id"].as_str().unwrap();
        assert!(crate::routing::extract_session_from_metadata(uid).is_some());
        // 两个不同用户不得塌成同一个 token。
        assert_ne!(md(json!({"user": "a_b"})), md(json!({"user": "a_c"})));
        // 没给 / 空 / 全非法 → 不填(填一个不能区分用户的值毫无意义)。
        assert_eq!(md(json!({})), None);
        assert_eq!(md(json!({"user": "   "})), None);
        assert_eq!(md(json!({"user": "___"})), None);
    }

    #[test]
    fn effort_分档与显式关闭() {
        assert_eq!(
            effort_to_thinking("high"),
            Some(json!({"type":"enabled","budget_tokens":32768}))
        );
        assert_eq!(effort_to_thinking("minimal"), Some(json!({"type":"disabled"})));
        assert_eq!(effort_to_thinking("weird"), None);
    }

    #[test]
    fn max_tokens_按优先级取_缺席给默认() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(max_tokens_of(&m(json!({"max_output_tokens": 7, "max_tokens": 9}))), 7);
        assert_eq!(max_tokens_of(&m(json!({"max_completion_tokens": 8, "max_tokens": 9}))), 8);
        assert_eq!(max_tokens_of(&m(json!({"max_tokens": 9}))), 9);
        assert_eq!(max_tokens_of(&m(json!({}))), DEFAULT_MAX_TOKENS);
        // 0 / null 当没说(OpenAI 侧 0 不是合法上限)。
        assert_eq!(max_tokens_of(&m(json!({"max_tokens": 0}))), DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn 采样参数只搬认识的键() {
        let src = json!({"temperature": 0.5, "top_p": 0.9, "frequency_penalty": 1.0,
                         "stop": ["x", "y"], "seed": 42});
        let mut dst = Map::new();
        copy_sampling(src.as_object().unwrap(), &mut dst);
        assert_eq!(dst.get("temperature"), Some(&json!(0.5)));
        assert_eq!(dst.get("top_p"), Some(&json!(0.9)));
        assert_eq!(dst.get("stop_sequences"), Some(&json!(["x", "y"])));
        // Anthropic 不认这两个 —— 塞过去会让整条请求被拒。
        assert!(!dst.contains_key("frequency_penalty"));
        assert!(!dst.contains_key("seed"));
    }

    #[test]
    fn 单串_stop_也归一成数组() {
        let src = json!({"stop": "END"});
        let mut dst = Map::new();
        copy_sampling(src.as_object().unwrap(), &mut dst);
        assert_eq!(dst.get("stop_sequences"), Some(&json!(["END"])));
    }

    #[test]
    fn system_累积器拼接并跳过空串() {
        let mut s = SystemAccum::default();
        s.push("a");
        s.push("   ");
        s.push("b");
        assert_eq!(s.finish(), Some(json!("a\n\nb")));
        assert_eq!(SystemAccum::default().finish(), None);
    }

    #[test]
    fn 结构化工具返回值被序列化成文本() {
        assert_eq!(
            tool_output_to_content(Some(&json!({"ok": true}))),
            json!([{"type":"text","text":"{\"ok\":true}"}])
        );
        assert_eq!(
            tool_output_to_content(Some(&json!("plain"))),
            json!([{"type":"text","text":"plain"}])
        );
    }

    /// 工具无输出是常态(写文件成功、命令无 stdout)。空 text 块正是上游必拒的形态,
    /// 所以必须给占位而不是原样留空 —— 否则整条请求被拒。
    #[test]
    fn 空工具输出给占位_不留空_text_块() {
        for v in [None, Some(&json!("")), Some(&json!(null)), Some(&json!([]))] {
            let out = tool_output_to_content(v);
            let text = out[0]["text"].as_str().unwrap();
            assert!(!text.trim().is_empty(), "{v:?} 产出了会被上游拒的空 text 块");
            assert_eq!(text, EMPTY_TOOL_OUTPUT);
        }
    }

    /// Responses 的结构化输出数组必须走同一套分片映射,不能只看「有 type 键」就透传:
    /// 一个合法的 `input_text` 块塞给 Anthropic,上游因为不认识块类型而拒整条请求。
    #[test]
    fn 分片数组的工具输出被映射成_anthropic_块() {
        let out = tool_output_to_content(Some(&json!([
            {"type": "input_text", "text": "结果"},
            {"type": "output_text", "text": "补充"},
        ])));
        assert_eq!(
            out,
            json!([{"type":"text","text":"结果"}, {"type":"text","text":"补充"}])
        );
        // 全是认不出的分片 → 落到占位,而不是产出一个空数组(空数组上游也拒)。
        let out = tool_output_to_content(Some(&json!([{"type":"input_audio","input_audio":{}}])));
        assert_eq!(out[0]["text"], EMPTY_TOOL_OUTPUT);
    }
}
