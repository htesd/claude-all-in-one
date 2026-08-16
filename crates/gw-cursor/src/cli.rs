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

// ── 帧0:RunRequest 主体 ─────────────────────────────────────────────────────
//
// 与 IDE 形态的差异(全部来自抓包实物,非推测):
// - 消息只有 `{1: 纯文本, 4: kind}` —— 没有 uuid(`.2`)、没有附件容器(`.3`)、
//   没有 ProseMirror(`.8`)。
// - 首轮 `1.14` 是 IDE 式带参数清单;后续轮改发 `1.15` 纯名清单({1:name, 7:0},
//   204 项,见 [`CLI_CATALOG_NAMES`],2026-08-16 本号实物)。
// - 多一个 `1.16` = conversation_id 的第二次出现。
// - 后续轮 `1.9` 只有模型名(无参数);首轮带参数。
// - 后续轮 `1.1` 环境块里 CLI 还有 `1.1.1`×6 / `1.1.8`×2 的 bin[32] 内容哈希。
//   我方暂不发(语义未钉死,疑似客户端侧的上下文内容寻址记账;消融实验再补)。

const BODY_ENV: u32 = 1;
const BODY_CONVERSATION: u32 = 2;
const BODY_EMPTY: u32 = 4;
const BODY_CONVERSATION_ID: u32 = 5;
const BODY_MODEL: u32 = 9;
const BODY_FLAG12: u32 = 12;
const BODY_MODEL_CATALOG: u32 = 14; // 首轮:带参数清单
const BODY_MODEL_CATALOG_V2: u32 = 15; // 后续轮:纯名清单 {1:name, 7:0}
const BODY_CONVERSATION_ID2: u32 = 16;
const BODY_TURN_ID: u32 = 25;

const ENV_WORKSPACE: u32 = 9; // 1.1.9  'file:///…'
const ENV_FLAG10: u32 = 10;
const ENV_CLIENT_KIND: u32 = 22; // 'cli'
const ENV_TIMESTAMP_MS: u32 = 26;
const ENV_TIMEZONE: u32 = 27;

const MSG_TEXT: u32 = 1;
const MSG_UUID: u32 = 2;
const MSG_ATTACH: u32 = 3;
const MSG_KIND: u32 = 4;
const MSG_KIND_USER: u64 = 1;

/// 一条 CLI 形态的用户消息:`{1: text, 2: uuid, 3: '', 4: 1}`(两种轮次同构,
/// 2026-08-16 抓包实证)。uuid 与空附件容器**都在** —— 第一版实现漏掉它们时,
/// 上游 200 接受、发完会话登记通知后只剩心跳,永不生成(与 IDE 形态缺
/// `1.2.1.2` 的静默挂起同形态)。
fn cli_user_message(text: &str) -> Writer {
    let mut msg = Writer::new();
    msg.string(MSG_TEXT, text);
    msg.string(MSG_UUID, &uuid::Uuid::new_v4().to_string());
    msg.string(MSG_ATTACH, "");
    msg.uint(MSG_KIND, MSG_KIND_USER);
    msg
}

/// CLI 形态帧0。
///
/// `opening` = 首轮(空 `1.1`、`1.14` 带参清单、`1.9` 带参数);
/// 否则后续轮(`1.1` 带预算表/工作区/时间戳、`1.15` 纯名清单、`1.9` 只有名)。
/// `budget` = (system_chars, history_chars),仅后续轮用。
pub fn build_frame0_cli(
    text: &str,
    model: &Model,
    catalog: &[Model],
    conversation_id: &str,
    turn_id: &str,
    timezone: &str,
    now_ms: u64,
    opening: bool,
    budget: (usize, usize),
    workspace: &str,
) -> Vec<u8> {
    let mut body = Writer::new();

    // 1.1 环境块:首轮空,后续轮装预算表 + 工作区 + 时间戳 + 时区。
    if opening {
        body.bytes(BODY_ENV, &[]);
    } else {
        let mut env = Writer::new();
        env.message(5, &crate::run::budget_table(budget.0, budget.1));
        env.string(ENV_WORKSPACE, workspace);
        env.uint(ENV_FLAG10, 1);
        env.string(ENV_CLIENT_KIND, "cli");
        env.uint(ENV_TIMESTAMP_MS, now_ms);
        env.string(ENV_TIMEZONE, timezone);
        body.message(BODY_ENV, &env);
    }

    // 1.2 会话块:唯一一条新消息(历史在服务端)。
    let mut turn = Writer::new();
    turn.message(1, &cli_user_message(text));
    let mut conv = Writer::new();
    conv.message(1, &turn);
    body.message(BODY_CONVERSATION, &conv);

    body.bytes(BODY_EMPTY, &[]);
    body.string(BODY_CONVERSATION_ID, conversation_id);

    // 1.9 当前模型:首轮带参数(IDE 式),后续轮只有名字。
    if opening {
        body.message(BODY_MODEL, &model.encode());
    } else {
        let mut m = Writer::new();
        m.string(1, &model.name);
        body.message(BODY_MODEL, &m);
    }
    body.uint(BODY_FLAG12, 0);

    if opening {
        // 1.14:IDE 式带参数清单(与 IDE 形态共用同一份目录)。
        for m in catalog {
            if !m.menu_visible {
                continue;
            }
            body.message(BODY_MODEL_CATALOG, &m.encode());
        }
    } else {
        // 1.15:纯名清单 {1: name, 7: 0}。
        for name in CLI_CATALOG_NAMES {
            let mut m = Writer::new();
            m.string(1, name);
            m.uint(7, 0);
            body.message(BODY_MODEL_CATALOG_V2, &m);
        }
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

fn derived_identity(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    format!("auth0|user_{}", hex_short(&d[..12]))
}

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
    token: &str,
    conversation_id: &str,
    timezone: &str,
    cwd: &str,
) -> Vec<u8> {
    let mut env = Writer::new();
    env.string(1, &format!("{} {}", crate::wire::CLIENT_OS, crate::wire::CLIENT_OS_VERSION));
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

    let mut idt = Writer::new();
    idt.string(1, ".");
    idt.string(4, &uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, token.as_bytes()).to_string());
    idt.string(5, &jwt_subject(token));
    idt.double(8, 0.0); // 抓包实物是 f64 0x0000000000000000
    // .10:43 字符的 base64url 随机串(实物 43-44 字符)。按 token+会话派生,保持会话内稳定。
    {
        use base64::Engine;
        let d = Sha256::digest(format!("{token}\x00{conversation_id}\x00id10").as_bytes());
        idt.string(
            10,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&d[..32])[..43],
        );
    }

    let mut hooks = Writer::new();
    hooks.string(1, "stop");
    hooks.string(1, "sessionStart");
    hooks.string(1, "sessionEnd");

    let mut det = Writer::new();
    det.message(DET_ENV, &env);
    det.message(DET_IDENTITY, &idt);
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

/// `{5: {3: ''}}` —— 2.10 上下文帧**之前**的那个确认帧。
///
/// ⭐ 2026-08-16 变体实测(H2 实验):帧0 之后、2.10 之前缺了 `{7:}` + `{5:{3:''}}`
/// 这两帧,上游 200 接受、发完会话登记通知后只剩心跳,**永不生成**;补上立刻出字。
/// 与 IDE 形态缺 `1.2.1.2` 的静默挂起是同一家族的「握手没完成」信号。
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

/// CLI 形态的完整请求帧序列,`(payload, 是否 gzip)`。
///
/// 帧序逐帧照抄 2026-08-16 的新鲜真包,并经变体实验钉死(`bs-H` 系列):
/// - `{7:}` 与 `{5:{3:''}}` 必须排在 2.10 上下文帧**之前**,缺了上游只发心跳
///   永不生成(H2 实验);帧0 压不压缩无所谓(H1 实验);
/// - 两种轮次用同一帧序(差异只在帧0 内部字段)。
pub fn cli_request_frames(frame0: &[u8], context: &[u8]) -> Vec<(Vec<u8>, bool)> {
    let mut v = vec![
        (frame0.to_vec(), true),
        (frame_field7_empty(), false),
        (frame_field5_ack3(), false),
        (context.to_vec(), true),
        (frame_field5_ack(), false),
        (frame_field3_slot(0), false),
    ];
    for n in 1..=3 {
        v.push((frame_field3_slot(n), false));
    }
    v.push((frame_field7_empty(), false));
    for n in 4..=7 {
        v.push((frame_field3_slot(n), false));
    }
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
            "Asia/Shanghai",
            0,
            true,
            (0, 0),
            "file:///",
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

    #[test]
    fn continuation_frame0_shape() {
        let model = Model::new("grok-4.6");
        let f0 = build_frame0_cli(
            "数字是?",
            &model,
            &[],
            "conv-1",
            "turn-2",
            "Asia/Shanghai",
            123,
            false,
            (100, 200),
            "file:///",
        );
        let body = sub(&f0, 1).unwrap();
        let fs = fields_of(body);
        // 1.15 纯名目录(204 项)在;1.14 不在。
        assert!(fs.contains(&BODY_MODEL_CATALOG_V2));
        assert!(!fs.contains(&BODY_MODEL_CATALOG));
        // 1.1 非空且含预算表(1.1.5)、工作区(1.1.9)、'cli'(1.1.22)。
        let env = sub(body, 1).unwrap();
        assert!(!env.is_empty());
        let env_fields = fields_of(env);
        for want in [5u32, 9, 10, 22, 26, 27] {
            assert!(env_fields.contains(&want), "1.1 缺字段 {want}");
        }
        // 1.9 后续轮只有模型名一个字段。
        let m = sub(body, 9).unwrap();
        assert_eq!(fields_of(m), vec![1]);
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
        // 环境/身份/hooks 块都在。
        for want in [DET_ENV, DET_IDENTITY, DET_HOOK_NAMES] {
            assert!(sub(det, want).is_some(), "详情块缺 {want}");
        }
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
