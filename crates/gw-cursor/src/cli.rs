//! CLI 形态(`cursor-agent` 2026.08.11)的请求构造。
//!
//! 2026-08-16 对本机 `cursor-agent`(bundled node + index.js,NODE_OPTIONS 预加载钩
//! http2)抓包逐字节解码,见 `PROTOCOL-agent-run.md` §20。与 IDE 形态(3.14.27)相比:
//!
//! - **头极简**:没有 checksum / machineId / session-id / client-key / config-version,
//!   不需要 GetServerConfig 握手;`x-cursor-client-type: cli`。
//! - **没有 `1.2.17` / blob / FileSync**:上下文(环境/身份/系统提示/MCP/skills)每轮
//!   用一个请求侧 field-`2.10` 大帧内联上传。
//! - **服务端持史实测成立**:跨进程 `--resume` 只发新消息纯文本(`{1:text, 4:kind}`,
//!   连消息 uuid 和 ProseMirror 都没有),模型记得首轮让它记的数字、记得上一轮
//!   工具调用的输出。这正是 IDE 形态一直没走通的那条路。
//! - 系统提示住在 `2.10.1.1.25`(与 IDE 的 `1.2.1.2.25` 字段号相同、位置不同)。
//!
//! 本模块只构造报文;选用哪个形态由 `chat.rs` 按 [`Profile`] 与请求内容决定。

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protobuf::Writer;
use crate::run::Model;

/// 请求形态。`CURSOR_PROFILE=cli` 切换;默认 `ide`(生产在跑形态,未经实测不换默认)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Ide,
    Cli,
}

impl Profile {
    pub fn from_env() -> Self {
        match std::env::var("CURSOR_PROFILE").as_deref() {
            Ok("cli") => Profile::Cli,
            _ => Profile::Ide,
        }
    }
    pub fn is_cli(self) -> bool {
        self == Profile::Cli
    }
}

/// `x-cursor-client-version` 的 CLI 真值(抓包实物)。
pub const CLI_CLIENT_VERSION: &str = "cli-2026.08.11-e8db854";

/// CLI 形态的完整请求头表(**生产与探针共用这一份**)。
///
/// 逐条来自 2026-08-16/23 抓包实物,与 IDE 形态的差异见模块头:没有 checksum /
/// machineId / session-id / client-key / config-version,也不需要 GetServerConfig 握手。
///
/// 自洽性约束(08-23 实证):`x-request-id` = `x-original-request-id` = 帧0 的 `1.25`;
/// 两条 traceparent **同值**。服务端会校验报文自洽性(stale uuid 回放会进心跳裸态),
/// 所以头与体必须同源 —— 调用方把 turn_id 同时喂给这里和 `build_frame0_cli`。
pub fn cli_headers(
    token: &str,
    conversation_id: &str,
    request_id: &str,
) -> Vec<(&'static str, String)> {
    let tp = crate::wire::traceparent();
    vec![
        ("connect-protocol-version", "1".into()),
        ("connect-accept-encoding", "gzip,br".into()),
        ("connect-content-encoding", "gzip".into()),
        ("content-type", "application/connect+proto".into()),
        ("user-agent", crate::wire::USER_AGENT.into()),
        ("authorization", format!("Bearer {token}")),
        (
            "x-blob-encryption-key",
            crate::chat::session_key(token, conversation_id, "blob"),
        ),
        ("x-cursor-client-type", "cli".into()),
        ("x-cursor-client-version", CLI_CLIENT_VERSION.into()),
        ("x-ghost-mode", "false".into()),
        ("x-request-id", request_id.into()),
        ("x-original-request-id", request_id.into()),
        ("traceparent", tp.clone()),
        ("backend-traceparent", tp),
    ]
}

// ── 帧0:RunRequest 主体 ─────────────────────────────────────────────────────
//
// 与 IDE 形态的差异(全部来自抓包实物,非推测):
// - 消息是 `{1: 文本, 2: uuid, 3: '', 4: 2}` —— uuid 与空附件容器都在,`.4` 是 **2**。
// - 多一个 `1.16` = conversation_id 的第二次出现。
//
// ## ⭐ 2026-08-23 实物普查更正了三条(旧记录来自 08-16,已作废)
//
// 三轮帧0(turn1 首轮 / turn2、turn4 续轮)解包后字段集**逐轮完全相同**:
// `{1, 2, 4, 5, 9, 12, 14×9, 16, 25}`。据此更正:
//
// - **不存在 `1.15`。** 全部 req 帧、全部嵌套深度扫描,`.15` 零命中。续轮同样发
//   `1.14`×9 带参目录(grok-4.6{effort,fast}、claude-opus-5{thinking,context,…})。
//   旧记录「续轮改发 1.15 纯名清单(204 项)」与实物矛盾,已删。
// - **`1.9` 没有「首轮带参 / 续轮裸名」的分岔。** 三轮都是 `{1: 'default'}`。
// - **首轮与续轮的唯一差异是 `1.1`**:首轮 `bin[0]` 空;续轮 = 服务端上一轮响应
//   顶层 `.3` 回声的描述符**逐字节回放**(turn2 实测 531B,字段
//   `.1×4 blob 引用 / .5 预算表 / .8 32B / .9 cwd / .10=2 / .22 'cli' / .26 ts / .27 tz`,
//   与 resp `.3` 逐字节相同)。所以「两种形态」其实是一种形态的两个 `1.1` 取值。
//   旧记录「续轮 1.1 带预算表」是对描述符 `.5` 的局部读法,不是客户端自己构造的。

const BODY_ENV: u32 = 1;
const BODY_CONVERSATION: u32 = 2;
const BODY_EMPTY: u32 = 4;
const BODY_CONVERSATION_ID: u32 = 5;
const BODY_MODEL: u32 = 9;
const BODY_FLAG12: u32 = 12;
const BODY_MODEL_CATALOG: u32 = 14; // 带参数清单(**两种轮次都发**,08-23 实物)
const BODY_CONVERSATION_ID2: u32 = 16;
const BODY_TURN_ID: u32 = 25;

const MSG_TEXT: u32 = 1;
const MSG_UUID: u32 = 2;
const MSG_ATTACH: u32 = 3;
const MSG_KIND: u32 = 4;
const MSG_KIND_USER: u64 = 2; // 08-23 实物:三轮都是 2

/// 一条 CLI 形态的用户消息:`{1: text, 2: uuid, 3: '', 4: 2}`(两种轮次同构)。
///
/// uuid 与空附件容器**都在** —— 第一版实现漏掉它们时,上游 200 接受、发完会话
/// 登记通知后只剩心跳,永不生成(与 IDE 形态缺 `1.2.1.2` 的静默挂起同形态)。
///
/// `.4` = **2**(08-23 三轮实物一致)。早先写 1 是错的。
fn cli_user_message(text: &str) -> Writer {
    let mut msg = Writer::new();
    msg.string(MSG_TEXT, text);
    msg.string(MSG_UUID, &uuid::Uuid::new_v4().to_string());
    msg.string(MSG_ATTACH, "");
    msg.uint(MSG_KIND, MSG_KIND_USER);
    msg
}

/// frame0 `1.14` 模型目录 —— **逐字照抄 2026-08-23 抓包实物的 9 条**(顺序即实物顺序)。
///
/// ## 为什么写死而不用 `models::catalog()`
///
/// 离线 diff(`examples/dump_frames`)对出来:我方原先发 33 条,实物 9 条,其中 26 条是
/// 这个账号**根本没有**的模型(kimi/glm/gpt-5.1~5.5/claude-4.x/gemini 各档),还有两条
/// 实物有而我方无(`grok-4.6`、`gemini-3.7-flash`),`grok-4.5` 的 `fast` 取值也相反。
/// `1.14` 是「这个号能用什么」的声明,拿一份不匹配的目录上去,服务端若据此校验就可能
/// 不建会话 —— 首轮探针的症状正是「只生成、不注册会话」。
///
/// ## ⚠️ 生产化前必须换成每账号动态目录
///
/// `1.14` 是**每账号**的模型清单:这 9 条是 lan 号的,别的号不一定相同(不同套餐/
/// 不同计费状态给的清单不同,见 `caio-kiro-account-model-entitlement` 那类教训)。
/// 写死只是探针阶段的权宜。生产化时改成按账号取:`accounts` 表已有
/// `availableModelsCache` 可用,拿它渲染本函数的输出。
/// **别把这份写死目录带上生产**——那会让所有号都声明 lan 的清单。
pub fn cli_catalog_lan() -> Vec<Model> {
    vec![
        Model::new("default"),
        Model::with_params("grok-4.6", &[("effort", "high"), ("fast", "true")]),
        Model::with_params(
            "claude-opus-5",
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "gpt-5.6-sol",
            &[
                ("context", "272k"),
                ("reasoning", "medium"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "claude-fable-5",
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
            ],
        ),
        Model::with_params("grok-4.5", &[("effort", "high"), ("fast", "true")]),
        Model::with_params("gemini-3.7-flash", &[("effort", "high")]),
        Model::with_params(
            "gpt-5.6-terra",
            &[
                ("context", "272k"),
                ("reasoning", "medium"),
                ("fast", "false"),
            ],
        ),
        Model::with_params(
            "claude-sonnet-5",
            &[
                ("thinking", "true"),
                ("context", "300k"),
                ("effort", "high"),
            ],
        ),
    ]
}

/// 本轮走首轮形态还是续轮形态。
///
/// 做成 enum 而不是 `opening: bool` + `descriptor: Option<_>`,是为了让
/// **「续轮但没有描述符」这个状态不可表示**。那个组合是一个静默失忆 bug:
/// `1.2` 只带本轮增量消息,而 `1.1` 里没有描述符 = 服务端手里没有历史、我方也没发,
/// 模型会看不到前几轮却照样答。影子表没料时唯一正确的动作是**降级回首轮形态**
/// (由调用方把历史折进 `text`),不是就地凑一个 `1.1`。
#[derive(Debug, Clone, Copy)]
pub enum CliTurn<'a> {
    /// 首轮 / 重铺:`1.1` = `bin[0]` 空,`text` 里带折叠后的全量历史。
    Opening,
    /// 续轮:`1.1` = 上一轮响应顶层 `.3` 描述符的**逐字节回放**,`text` 只带本轮增量。
    Continuation(&'a [u8]),
}

/// CLI 形态帧0。
///
/// ## 首轮与续轮的唯一差异是 `1.1`(2026-08-23 三轮实物普查)
///
/// 三轮帧0 解包后字段集完全相同:`{1, 2, 4, 5, 9, 12, 14×9, 16, 25}`。
/// 所以除了 `1.1` 取值,两种轮次**逐字段同构** —— 没有 `1.15`、`1.9` 不分岔、
/// `1.14` 两轮都发。旧实现按 `opening` 分岔发 `.15`/裸名 `.9` 是错的,已改。
pub fn build_frame0_cli(
    text: &str,
    model: &Model,
    catalog: &[Model],
    conversation_id: &str,
    turn_id: &str,
    phase: CliTurn<'_>,
) -> Vec<u8> {
    let mut body = Writer::new();

    // 1.1 环境块:首轮空;续轮 = 描述符逐字节回放。
    match phase {
        CliTurn::Opening => body.bytes(BODY_ENV, &[]),
        CliTurn::Continuation(desc) => {
            // ⚠️ **原样字节,永不解析重编码。**
            //
            // 描述符里的 `.1`×N 内容哈希与 `.8` 那个 32B 我方**造不出来** —— 那是服务端
            // 侧内容寻址存储的引用(turn2 实测 `.1`×4 + `.8`×1;turn4 涨到 6 + 2,
            // 多出的正好是上一轮产生的新 blob)。逐字回放是唯一正确的用法:重编码会
            // 改字段顺序,而 `.8` 疑似是对描述符自身的校验值,顺序一变就可能对不上。
            //
            // `Writer::bytes` 写的是 length-delimited,与 `message` 的线上形态逐字节
            // 一致,所以这里直接塞字节而不是先解成 Writer 再写回。
            //
            // 实测锚点(turn2,531B):`.1×4 / .5 预算表{.1=17683, .2=256000, .3=分节表}
            // / .8 32B / .9 'file:///tmp/cli-probe' / .10=2 / .22 'cli' / .26 ts /
            // .27 'Asia/Shanghai'`,与该会话上一轮响应顶层 `.3` 逐字节相同。
            body.bytes(BODY_ENV, desc)
        }
    }

    // 1.2 会话块:一条消息。首轮装折叠后的全量历史,续轮只装本轮增量。
    let mut msg_turn = Writer::new();
    msg_turn.message(1, &cli_user_message(text));
    let mut conv = Writer::new();
    conv.message(1, &msg_turn);
    body.message(BODY_CONVERSATION, &conv);

    body.bytes(BODY_EMPTY, &[]);
    body.string(BODY_CONVERSATION_ID, conversation_id);

    // 1.9 当前模型。**两种轮次同一形态**(08-23 三轮都是 `{1:'default'}`)。
    // 用 `Model::encode()`:无参模型编出来就是 `{1:name}`,与实物逐字节一致;
    // 带参模型(grok-4.6{effort,fast} 等)则带上参数 —— 与 `1.14` 目录里同一模型
    // 的编码方式保持一致。抓包只覆盖了 `default`,带参模型的 `1.9` 形态未实证。
    body.message(BODY_MODEL, &model.encode());
    body.uint(BODY_FLAG12, 0);

    // 1.14 带参模型目录,**两种轮次都发**(08-23 实物:三轮均 `.14×9`)。
    for m in catalog {
        if !m.menu_visible {
            continue;
        }
        body.message(BODY_MODEL_CATALOG, &m.encode());
    }

    body.string(BODY_CONVERSATION_ID2, conversation_id);
    body.string(BODY_TURN_ID, turn_id);

    let mut frame = Writer::new();
    frame.message(1, &body);
    frame.into_bytes()
}

// ── field-2.10 上下文帧 ─────────────────────────────────────────────────────
//
// 抓包结构:帧 payload = `{2: {10: {1: {1: <详情块>}}}}`。详情块字段:
//   .4  环境详情(os/cwd/shell/各种路径/时区/开关)
//   .6  身份('.' + uuid + JWT sub + f64 0 + 43 字符随机串)
//   .8/.9 notes 占位串
//   .14 MCP server 描述(我方声明调用方工具时用,Phase 3)
//   .25 系统提示全文
//   .26/.27 'enabled'   .28 hooks 名列表   .32/.33 = 1

const DET_ENV: u32 = 4;
#[allow(dead_code)] // 见 build_context_frame_cli 里不发 `.6` 的说明
const DET_IDENTITY: u32 = 6;
const DET_NOTES_A: u32 = 8;
const DET_NOTES_B: u32 = 9;
const DET_SYSTEM_PROMPT: u32 = 25;
const DET_HOOKS_A: u32 = 26;
const DET_HOOKS_B: u32 = 27;
const DET_HOOK_NAMES: u32 = 28;

/// notes 目录不存在时 CLI 发的占位串(原样照抄)。
const NOTES_PLACEHOLDER: &str =
    "(No notes directory yet - will be created when you write your first note)";

/// 从 access_token(JWT)解出 `sub`(不验签,只取 payload)。
///
/// CLI 的 `.6.5` 是 `auth0|user_…` 形态 —— 就是 JWT 的 sub。解不出来时
/// 用 token 派生一个同形态占位值,保证字段不缺席。
#[allow(dead_code)] // 同上:`.6` 暂不发,构造器留作回补候选
fn jwt_subject(token: &str) -> String {
    use base64::Engine;
    let Some(payload) = token.split('.').nth(1) else {
        return derived_identity(token);
    };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return derived_identity(token);
    };
    serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("sub")?.as_str().map(String::from))
        .unwrap_or_else(|| derived_identity(token))
}

#[allow(dead_code)]
fn derived_identity(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    format!("auth0|user_{}", hex_short(&d[..12]))
}

#[allow(dead_code)]
fn hex_short(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// 构造 field-2.10 上下文帧的 payload(未 gzip、未加 Connect 帧头)。
///
/// `cwd` 我们恒为 `/`(反代没有工作区);衍生的项目路径按 CLI 的命名规律拼。
pub fn build_context_frame_cli(
    system: &str,
    // `.6` 身份块已不发(见下),token 暂时用不上;保留形参免得回补时又要改所有调用点。
    _token: &str,
    conversation_id: &str,
    timezone: &str,
    cwd: &str,
) -> Vec<u8> {
    let mut env = Writer::new();
    env.string(
        1,
        &format!(
            "{} {}",
            crate::wire::CLIENT_OS,
            crate::wire::CLIENT_OS_VERSION
        ),
    );
    env.string(2, cwd);
    env.string(3, "bash");
    // CLI 的项目衍生路径:~/.cursor/projects/<cwd slug>/…。slug 规则 = 路径去斜杠。
    let slug = cwd.trim_matches('/').replace('/', "-");
    let proj = format!("/home/iiap/.cursor/projects/{slug}");
    env.string(7, &format!("{proj}/terminals"));
    env.string(8, &format!("{proj}/agent-notes/shared"));
    env.string(9, &format!("{proj}/agent-notes/{conversation_id}"));
    env.string(10, timezone);
    env.string(11, &proj);
    env.string(12, &format!("{proj}/agent-transcripts"));
    env.uint(14, 0);
    env.uint(16, 1);
    env.uint(19, 0);
    env.uint(20, 0);
    env.string(21, cwd);
    env.uint(22, 0);

    let mut hooks = Writer::new();
    hooks.string(1, "stop");
    hooks.string(1, "sessionStart");
    hooks.string(1, "sessionEnd");

    let mut det = Writer::new();
    det.message(DET_ENV, &env);
    // ⚠️ **不发 `.6` 身份块**(2026-08-23 离线逐字节 diff)。
    //
    // 官方 CLI 的 turn1 ENV 帧里**完全没有 `.6`** —— 我方原先发一个 112 B 的身份块
    // (uuid/JWT sub/随机串),那是唯一一处「我方发了而官方不发」的字段。多余的意外
    // 字段最容易把服务端切到另一条代码路径,而首轮探针的症状正是「只生成、不注册会话」
    // (无顶层 `.3`、无提交记录、无 `1.14` 用量帧)。
    //
    // 那条记录的来源与 `.15`、控制帧位置是同一批:08-16 的 IDE/glass 形态抓包,
    // 不是 CLI 形态。构造器(`jwt_subject` 等)保留不删 —— 若删掉 `.6` 之后症状不变,
    // 它是要试着补回去的第一批候选。
    det.string(DET_NOTES_A, NOTES_PLACEHOLDER);
    det.string(DET_NOTES_B, NOTES_PLACEHOLDER);
    if !system.is_empty() {
        det.string(DET_SYSTEM_PROMPT, system);
    }
    det.string(DET_HOOKS_A, "enabled");
    det.string(DET_HOOKS_B, "enabled");
    det.message(DET_HOOK_NAMES, &hooks);
    det.uint(32, 1);
    det.uint(33, 1);

    // 包裹层:{2: {10: {1: {1: det}}}}(逐层来自抓包实物)。
    let mut l1 = Writer::new();
    l1.message(1, &det);
    let mut l2 = Writer::new();
    l2.message(1, &l1);
    let mut l3 = Writer::new();
    l3.message(10, &l2);
    let mut frame = Writer::new();
    frame.message(2, &l3);
    frame.into_bytes()
}

// ── 小帧(上下文槽记账)────────────────────────────────────────────────────────
//
// IDE 形态已实测这些 2-8 字节小帧与出字无关(§11.1);CLI 形态照发,与真包同序。

/// `{7: ''}`
pub fn frame_field7_empty() -> Vec<u8> {
    let mut w = Writer::new();
    w.string(7, "");
    w.into_bytes()
}

/// `{5: {1: ''}}`
pub fn frame_field5_ack() -> Vec<u8> {
    let mut inner = Writer::new();
    inner.string(1, "");
    let mut w = Writer::new();
    w.message(5, &inner);
    w.into_bytes()
}

/// `{5: {3: ''}}`
///
/// ⚠️ **08-23 实物里不存在这一帧。** 保留是因为 08-16 的 H2 变体实验记录说
/// 「帧0 之后、2.10 之前缺了 `{7:}` + `{5:{3:''}}` 就只发心跳」。那次实验测的是
/// **我方合成请求**加上这两帧后开始出字,可能有共变量;而 08-23 抓的是官方客户端
/// 的真实写序,里面 `.5` 帧只有 `{5:{1:''}}` 一种。两者冲突时以实物为准
/// (见 [`cli_request_frames`]),这个构造器暂留不发,别删 —— 若探针出现心跳裸态,
/// 它是第一个要试着补回去的东西。
#[allow(dead_code)]
pub fn frame_field5_ack3() -> Vec<u8> {
    let mut inner = Writer::new();
    inner.string(3, "");
    let mut w = Writer::new();
    w.message(5, &inner);
    w.into_bytes()
}

/// `{3: {3: ''}}`(n=0)或 `{3: {1: n, 3: ''}}`
pub fn frame_field3_slot(n: u64) -> Vec<u8> {
    let mut inner = Writer::new();
    if n > 0 {
        inner.uint(1, n);
    }
    inner.string(3, "");
    let mut w = Writer::new();
    w.message(3, &inner);
    w.into_bytes()
}

/// 内容分节上传槽:`{3: {1: slot, 2: {1: 内容字节}}}`。
///
/// ## 格式与时序(2026-08-23 turn4 实物)
///
/// 服务端按 hash 逐条点名(field-4 `4.2`,一帧一个),客户端**按点名顺序**交出,
/// slot 序号从 0 递增。实测 7 个需求 → 7 个槽,每个槽内容的 sha256 正好等于被点名的
/// 哈希;交完之后服务端才发会话登记通知,客户端随后才发 ENV。
///
/// slot 0 **省略 `.1`**(proto3 默认值不上线),与 [`frame_field3_slot`] 同一约定 ——
/// 实物 req-001(slot 0)里确实没有 `.1`,req-002 起才有。
pub fn content_slot_frame(slot: u64, content: &[u8]) -> Vec<u8> {
    let mut holder = Writer::new();
    holder.bytes(1, content);
    let mut inner = Writer::new();
    if slot > 0 {
        inner.uint(1, slot);
    }
    inner.message(2, &holder);
    let mut w = Writer::new();
    w.message(3, &inner);
    w.into_bytes()
}

/// CLI 形态的完整请求帧序列,`(payload, 是否 gzip)`。
///
/// ## 帧序逐帧照抄 2026-08-23 官方客户端实物(turn1,13 帧)
///
/// ```text
/// req-000  帧0            727B   flag=0/1(压不压都行,turn1 未压、turn2/4 压)
/// req-001  ENV(2.10)    51558B   flag=1
/// req-002  {5:{1:''}}       4B
/// req-003  {3:{3:''}}       4B    ← slot 0
/// req-004  {3:{1:1,3:''}}   6B    ← slot 1
///  …                              ← slot 2..7
/// req-010  {3:{1:7,3:''}}   6B
/// req-011  {7:''}           2B    ← 只有一个,且在 slot 7 之后
/// req-012  {3:{1:8,3:''}}   6B    ← slot 8 在 {7:} 之后
/// ```
///
/// ## ⭐ 与旧实现的两处冲突(以实物为准)
///
/// 1. **小控制帧全部在 ENV 帧之后**,没有一个在前面。旧实现在帧0 与 ENV 之间插了
///    `{7:}` + `{5:{3:''}}`,凭空多出两帧;而缺 `{5:{3:''}}` 会挂起的说法来自 08-16
///    对**我方合成请求**做的 H2 实验,与官方真实写序矛盾(见 [`frame_field5_ack3`])。
/// 2. **slot 到 8,`{7:}` 夹在 7 和 8 之间。** 旧实现只到 7、且把 `{7:}` 放在 3 和 4
///    之间。这些帧 IDE 形态已实测与出字无关(§11.1),照发只为不可区分。
pub fn cli_request_frames(frame0: &[u8], context: &[u8]) -> Vec<(Vec<u8>, bool)> {
    let mut v = vec![
        (frame0.to_vec(), true),
        (context.to_vec(), true),
        (frame_field5_ack(), false),
    ];
    // slot 0..=7,然后 {7:},最后 slot 8 —— 顺序即实物。
    for n in 0..=7 {
        v.push((frame_field3_slot(n), false));
    }
    v.push((frame_field7_empty(), false));
    v.push((frame_field3_slot(8), false));
    v
}

// ── google.protobuf.Value(JSON schema 走 2.36 推工具定义时用,Phase 3)─────────
//
// 字段号对齐 google.protobuf.Value:null=1 / number=2(double) / string=3 /
// bool=4 / struct=5 / list=6。2026-08-16 CLI 抓包实证:string 值是 `{3: str}`、
// 对象是 `{5: repeated {1:key, 2:Value}}`、数组是 `{6: repeated {1: Value}}`。

/// 把任意 JSON 值按 google.protobuf.Value 编码进一个 message 字段。
pub fn struct_value_field(w: &mut Writer, field: u32, v: &Value) {
    let mut inner = Writer::new();
    write_struct_value(&mut inner, v);
    w.message(field, &inner);
}

fn write_struct_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Null => w.uint(1, 0),
        Value::Number(n) => w.double(2, n.as_f64().unwrap_or(0.0)),
        Value::String(s) => w.string(3, s),
        Value::Bool(b) => w.bool(4, *b),
        Value::Object(map) => {
            let mut st = Writer::new();
            for (k, val) in map {
                let mut entry = Writer::new();
                entry.string(1, k);
                struct_value_field(&mut entry, 2, val);
                st.message(1, &entry);
            }
            w.message(5, &st);
        }
        Value::Array(items) => {
            let mut list = Writer::new();
            for item in items {
                struct_value_field(&mut list, 1, item);
            }
            w.message(6, &list);
        }
    }
}

include!("cli_catalog.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{Reader, Value as PbValue};

    fn fields_of(bytes: &[u8]) -> Vec<u32> {
        Reader::new(bytes).map(|(f, _)| f).collect()
    }

    fn sub<'a>(bytes: &'a [u8], want: u32) -> Option<&'a [u8]> {
        for (f, v) in Reader::new(bytes) {
            if f == want {
                if let PbValue::Len(s) = v {
                    return Some(s);
                }
            }
        }
        None
    }

    #[test]
    fn opening_frame0_shape() {
        let model = Model::with_params("grok-4.6", &[("effort", "high"), ("fast", "true")]);
        let f0 = build_frame0_cli(
            "记住数字 4712",
            &model,
            &[],
            "conv-1",
            "turn-1",
            CliTurn::Opening,
        );
        let body = sub(&f0, 1).expect("帧 payload 应有 field 1");
        // 顶层字段序:1.1 空、1.2、1.4、1.5、1.9、1.12、1.16、1.25(无目录时)。
        assert_eq!(fields_of(body), vec![1, 2, 4, 5, 9, 12, 16, 25]);
        // 1.1 必须是在场的空块。
        assert_eq!(sub(body, 1), Some(&b""[..]));
        // 消息为 {1: text, 2: uuid, 3: '', 4: kind}(缺 uuid 会静默挂起,见函数注释)。
        let conv = sub(body, 2).unwrap();
        let turn = sub(conv, 1).unwrap();
        let msg = sub(turn, 1).unwrap();
        assert_eq!(fields_of(msg), vec![1, 2, 3, 4]);
    }

    /// 描述符回放:`1.1` 必须是**真实抓包描述符的逐字节副本**,不是重编码。
    #[test]
    fn 描述符回放_逐字节进1点1() {
        let frame = crate::run::descriptor_samples::turn2();
        let desc = crate::run::descriptor_field3(&frame).expect("样本必须带顶层 .3");
        let model = Model::new("default");
        let f0 = build_frame0_cli(
            "数字是?",
            &model,
            &[],
            "conv-1",
            "turn-2",
            CliTurn::Continuation(desc),
        );
        let body = sub(&f0, 1).unwrap();
        assert_eq!(sub(body, 1), Some(desc), "1.1 必须与描述符逐字节相同");
        assert_eq!(crate::run::descriptor_ref_count(sub(body, 1).unwrap()), 6);
    }

    /// 首轮 `1.1` 是**在场的空块**(服务端判首轮就看这个)。
    #[test]
    fn 首轮1点1为在场空块() {
        let model = Model::new("default");
        let f0 = build_frame0_cli("x", &model, &[], "c", "t", CliTurn::Opening);
        assert_eq!(sub(sub(&f0, 1).unwrap(), 1), Some(&b""[..]));
    }

    /// ⭐ 帧0 字段普查必须与 08-23 三轮实物一致:`{1,2,4,5,9,12,14×N,16,25}`。
    ///
    /// 三轮(turn1 首轮 / turn2、turn4 续轮)解包后字段集**完全相同**,所以两种轮次
    /// 用同一份断言。`.15` 必须缺席 —— 全部 req 帧、全部嵌套深度扫描零命中。
    #[test]
    fn 帧0字段普查_对齐08_23实物() {
        let model = Model::new("default");
        let catalog = vec![Model::new("grok-4.6"), Model::new("claude-opus-5")];
        let frame = crate::run::descriptor_samples::turn2();
        let desc = crate::run::descriptor_field3(&frame).unwrap();

        for (label, phase) in [
            ("首轮", CliTurn::Opening),
            ("续轮", CliTurn::Continuation(desc)),
        ] {
            let f0 = build_frame0_cli("q", &model, &catalog, "conv", "turn", phase);
            let body = sub(&f0, 1).unwrap();
            let fs = fields_of(body);
            assert_eq!(
                fs,
                vec![1, 2, 4, 5, 9, 12, 14, 14, 16, 25],
                "{label}:字段集/顺序必须与实物一致"
            );
            assert!(!fs.contains(&15), "{label}:.15 不存在于任何实物帧");
            // 1.9 两轮同形态:无参模型编出来就是 {1:name},与实物 {1:'default'} 一致。
            assert_eq!(sub(body, 9).and_then(|m| sub(m, 1)), Some(&b"default"[..]));
        }
    }

    /// 消息 `.4` = **2**(08-23 三轮一致)。早先写 1 是错的。
    #[test]
    fn 消息kind为2() {
        let model = Model::new("default");
        let f0 = build_frame0_cli("hi", &model, &[], "c", "t", CliTurn::Opening);
        let msg = sub(sub(sub(sub(&f0, 1).unwrap(), 2).unwrap(), 1).unwrap(), 1).unwrap();
        assert_eq!(
            fields_of(msg),
            vec![1, 2, 3, 4],
            "{{1:text,2:uuid,3:'',4:kind}}"
        );
        // varint 字段读不到就直接找字节:kind 是最后一个字段,值必须是 2。
        assert_eq!(msg[msg.len() - 1], 2, ".4 必须是 2");
        assert_eq!(sub(msg, 3), Some(&b""[..]), ".3 空附件容器必须在场");
    }

    /// ⭐ 帧序必须与 08-23 实物写序一致(13 帧)。
    ///
    /// 实物:帧0 → ENV → `{5:{1:''}}` → slot0..7 → `{7:''}` → slot8。
    /// 旧实现在帧0 与 ENV 之间插了两帧、slot 只到 7、`{7:}` 位置也不对。
    #[test]
    fn 帧序_对齐08_23实物写序() {
        let frames = cli_request_frames(b"F0", b"ENV");
        // 实物 req-000..req-012 = 13 帧:帧0 + ENV + 1 个 .5 + 9 个 slot(0..8) + 1 个 .7。
        assert_eq!(frames.len(), 13);
        assert_eq!(frames[0].0, b"F0");
        assert_eq!(frames[1].0, b"ENV", "ENV 必须紧跟帧0,中间不插控制帧");
        assert!(frames[0].1 && frames[1].1, "两个大帧 gzip");
        assert_eq!(
            frames[2].0,
            frame_field5_ack(),
            "第一个控制帧是 {{5:{{1:''}}}}"
        );
        assert!(frames[2..].iter().all(|(_, z)| !z), "控制帧一律裸发");
        // slot 0..=7 紧接其后。
        for (i, n) in (0u64..=7).enumerate() {
            assert_eq!(frames[3 + i].0, frame_field3_slot(n), "slot {n} 位置不对");
        }
        // {7:} 夹在 slot7 与 slot8 之间。
        assert_eq!(frames[11].0, frame_field7_empty(), "{{7:}} 在 slot7 之后");
        assert_eq!(frames[12].0, frame_field3_slot(8), "slot8 在 {{7:}} 之后");
        // 只有一个 {7:}。
        assert_eq!(
            frames
                .iter()
                .filter(|(p, _)| *p == frame_field7_empty())
                .count(),
            1
        );
    }

    #[test]
    fn context_frame_has_system_prompt_at_25() {
        let f = build_context_frame_cli("SYS", "tok", "conv", "Asia/Shanghai", "/");
        let l3 = sub(&f, 2).unwrap();
        let l2 = sub(l3, 10).unwrap();
        let l1 = sub(l2, 1).unwrap();
        let det = sub(l1, 1).unwrap();
        let sp = sub(det, DET_SYSTEM_PROMPT).expect("2.10.1.1.25 系统提示必须在场");
        assert_eq!(sp, b"SYS");
        // 环境 / hooks 块都在。
        for want in [DET_ENV, DET_HOOK_NAMES] {
            assert!(sub(det, want).is_some(), "详情块缺 {want}");
        }
        // ⭐ `.6` 身份块**必须缺席**(2026-08-23 离线 diff:官方 turn1 ENV 里没有它)。
        //
        // 这条断言方向是反的才对:早先我方发一个 112 B 身份块,是唯一一处「我方发了而
        // 官方不发」的字段,而首轮探针的症状是「只生成、不注册会话」。旧用例断言它在场,
        // 等于把这个差异钉死成"正确行为"—— 逆向出来的协议里,断言必须锚在抓包上,
        // 不能锚在我方当时的实现上。
        assert!(
            sub(det, DET_IDENTITY).is_none(),
            "不得发 `.6` 身份块 —— 官方 CLI 的 ENV 帧里没有它"
        );
    }

    #[test]
    fn struct_value_encoding_matches_capture() {
        // 抓包实证:string → {3: str};object → {5: {1:key, 2:Value}};
        // array → {6: {1: Value}};type:"object" 的编码前缀必现。
        let v = serde_json::json!({"type": "object", "required": ["query"], "n": 1.5, "ok": true});
        let mut w = Writer::new();
        write_struct_value(&mut w, &v);
        let b = w.into_bytes();
        // 顶层是 struct:field 5。
        assert_eq!(fields_of(&b), vec![5]);
        // 字符串 'object' 应以 {3: "object"} 形态出现在线格式里:tag 0x1a + len 6。
        let needle = [0x1a, 6, b'o', b'b', b'j', b'e', b'c', b't'];
        assert!(
            b.windows(needle.len()).any(|win| win == needle),
            "string 值必须按 field 3 编码(google.protobuf.Value)"
        );
    }

    #[test]
    fn small_frames_match_capture_shapes() {
        assert_eq!(fields_of(&frame_field7_empty()), vec![7]);
        assert_eq!(fields_of(&frame_field5_ack()), vec![5]);
        assert_eq!(fields_of(&frame_field3_slot(0)), vec![3]);
        let slot2 = frame_field3_slot(2);
        let inner = sub(&slot2, 3).unwrap();
        assert_eq!(fields_of(inner), vec![1, 3]);
    }
}
