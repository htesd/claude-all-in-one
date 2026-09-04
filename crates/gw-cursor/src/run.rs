//! `agent.v1.AgentService/Run` 的请求构造与响应解析。
//!
//! 字段号全部来自 `PROTOCOL-agent-run.md` §3(2026-08-06 对本机 3.14.27 抓真包逐字节解码)。
//!
//! ## 读字段号时最容易栽的一跤
//!
//! 文档 §3.1 写的 `1.1` / `1.2` / `1.9`,**最前面那个 `1` 是 Connect 帧 payload 的顶层
//! field 1**(帧类型 oneof),不是 message 名。也就是说 `1.9`(当前模型)的真实路径是
//! 「帧 payload 的 field 1 → 它内部的 field 9」,**不是**顶层 field 9。按字面写会整个请求作废。
//!
//! ## 哪些字段是必需的:文档答不了,只能实测
//!
//! 文档 §0 的三级验证(原样重放 / 变异重放 / 结构合成)用的都是抓包实物的**完整** 3 帧,
//! **没做过任何删字段、删帧的最小化实验**。所以「哪些能省」在文档里是空白。
//! 因此本模块把可疑分节做成 [`RunShape`] 开关,e2e 时二分,而不是拍脑袋写死。

use serde_json::{json, Value};

use sha2::{Digest, Sha256};

use crate::protobuf::{Reader, Value as PbValue, Writer};

// ── 帧 payload 顶层 ─────────────────────────────────────────────────────────
/// 帧0:RunRequest 主体。
const FRAME_BODY: u32 = 1;
/// 帧1/帧2:流式上传的上下文条目。
const FRAME_CONTEXT: u32 = 3;
/// 帧3/帧4:空字符串,「初始上下文发完」标记。
const FRAME_READY: u32 = 7;

// ── 主体(帧0 field 1)内部字段 ─────────────────────────────────────────────
const BODY_ENV: u32 = 1; // 1.1  消息与环境块
const BODY_CONVERSATION: u32 = 2; // 1.2  会话消息块
const BODY_EMPTY: u32 = 4; // 1.4  bin[0]
const BODY_CONVERSATION_ID: u32 = 5; // 1.5  conversation_id
const BODY_MODEL: u32 = 9; // 1.9  当前选中模型
const BODY_FLAG10: u32 = 10; // 1.10 var=0
const BODY_MODEL_CATALOG: u32 = 14; // 1.14 可用模型清单
/// `1.25` —— **每轮一个**的 turn id(与会话级的 `1.5` 区分开)。
///
/// 抓包实测:同一会话连续两轮 `1.5` 相同、`1.25` 不同。服务端的会话回显里回的
/// 也是 `1.25`,疑似用它给消息排序/去重。旧代码从不发它。
const BODY_TURN_ID: u32 = 25;

// ── 环境块(1.1)内部 ───────────────────────────────────────────────────────
const ENV_BUDGET: u32 = 5; // 1.1.5  token 预算分节表
const ENV_FLAG10: u32 = 10; // 1.1.10 var=1
const ENV_CLIENT_KIND: u32 = 22; // 1.1.22 str='ide'
const ENV_TIMESTAMP_MS: u32 = 26; // 1.1.26 毫秒时间戳
const ENV_TIMEZONE: u32 = 27; // 1.1.27 时区

// ── 会话块(1.2)内部 ───────────────────────────────────────────────────────
const CONV_TURN: u32 = 1; // 1.2.1    每轮一条(repeated)
const TURN_MESSAGE: u32 = 1; // 1.2.1.1  轮内消息
/// `1.2.1.2` —— 上下文块,**挂在轮内、与消息并列**,不是会话级的 `1.2.17`。
///
/// ## 这个字段在场与否,决定服务端生不生成
///
/// 2026-08-07 从真包逐帧削出来的结论:去掉它,上游照样回 200、照样回一帧会话回显,
/// 然后每 10 秒一个 4 字节心跳,**永不产出任何文本**;把它加回来(哪怕**长度为 0**)
/// 立刻正常出字。两个请求的字节数差 1(445 vs 446)。
///
/// 也就是说这不是「内容不够」而是「握手没完成」:服务端拿它判断本轮上下文
/// 已经声明完毕。没有任何错误码,所以只看返回值永远查不出来 —— 这是本 crate
/// 卡了整轮的那个 bug。
const TURN_CONTEXT: u32 = 2;
/// `1.2.17` —— 会话级上下文块。**后续轮**的上下文声明住在这里。
///
/// 真客户端里它装 `{1,3,5,7}` 的 bin[32] blob 哈希 + `{2,4,6,8}` 长度 + `{9}` 环境详情。
/// 我方**只发 `.9`**:哈希指向 FileSyncService 上传过的内容,我们没上传过,
/// 一发就 `invalid_argument: Failed to resolve request context blobs`(实测)。
const CONV_CONTEXT: u32 = 17;
/// `1.2.17.9` —— 与首轮 `1.2.1.2` **同构**的环境详情块(字段号一模一样)。
const CTX17_DETAIL: u32 = 9;

/// 一次请求处在会话的哪个阶段。**两种阶段的报文形态差 47 倍**(实测首轮 98858B /
/// 后续轮 2121B),不是可选优化 —— 历史由服务端按 `1.5` 持有,后续轮重发历史
/// 既浪费又可能被当成新内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 首轮:全量内联上下文,`1.1` 空,上下文声明在 `1.2.1.2`。
    ///
    /// 也是**降级形态**:会话在服务端不存在(或换了账号)时一律回到这里,
    /// 把全部历史当作首轮重新铺开。
    Opening,
    /// 后续轮:只发新消息,`1.1` 带预算表,上下文声明改在 `1.2.17`。
    Continuation,
}

impl Phase {
    pub fn is_opening(self) -> bool {
        self == Phase::Opening
    }
    pub fn is_continuation(self) -> bool {
        self == Phase::Continuation
    }
}
const MSG_TEXT: u32 = 1; // 1.2.1.1.1 纯文本
const MSG_UUID: u32 = 2; // 1.2.1.1.2 消息 uuid
/// `1.2.1.1.3` —— **图片附件容器**(repeated)。
///
/// ⚠️ 早先记成「空字符串,抓包实物恒为空」——那是因为当时抓的都是没附件的对话。
/// 2026-08-07 带图抓包证实:它装的是图片,没图时才为空。
const MSG_ATTACH: u32 = 3;
const MSG_KIND: u32 = 4; // 1.2.1.1.4 var=1
const MSG_RICH: u32 = 8; // 1.2.1.1.8 ProseMirror JSON

// ── 上下文块(1.2.1.2)内部 ─────────────────────────────────────────────────
//
// ⚠️ 这些字段号原先记在 `1.2.17.9` 下,是**路径记错了**。2026-08-07 的真包里
// `1.2.17` 只有 `{1,3,5,7}` 的 bin[32] blob 哈希与 `{2,4,6,8}` 的长度,而下面
// 这一串(4/25/26/27 与那堆开关)全部住在 `1.2.1.2`。字段号本身是对的。
//
// **别发 `1.2.17`**:它一旦出现,服务端就会去解析对应的 blob,而我们没有走
// FileSyncService 上传过任何东西 —— 实测直接 `invalid_argument:
// "Failed to resolve request context blobs"`。文件附件(§5 的 L2)做之前一律不发。
const DET_ENV: u32 = 4; // 1.2.1.2.4  环境详情
const DET_TOOL: u32 = 7; // 1.2.1.2.7  工具声明(repeated)
const DET_DOC: u32 = 20; // 1.2.1.2.20 文档附件(repeated,PDF 等)
const DET_SYSTEM_PROMPT: u32 = 25; // 1.2.1.2.25 系统提示全文
const DET_HOOKS_A: u32 = 26; // 1.2.1.2.26 'enabled'
const DET_HOOKS_B: u32 = 27; // 1.2.1.2.27 'enabled'

/// `1.2.1.2` 里一串布尔开关,抓包实物的原值。
///
/// 语义未知(bundle 里是压缩过的字段名),但**逐字照抄**是「完整模拟客户端」的一部分:
/// 少发这些会让我们的请求在服务端侧与真 IDE 可区分。
///
/// ⚠️ 但注意:它们**不是**生成的开关。真正决定生不生成的是 `1.2.1.2` 这个字段
/// 本身在不在场(见 [`TURN_CONTEXT`])—— 实测把这一串连同 `.4`/`.25` 全删光、
/// 只留一个长度为 0 的 `1.2.1.2`,照样正常出字。
const DET_FLAGS: &[(u32, u64)] = &[
    (17, 1),
    (24, 1),
    (32, 1),
    (33, 0),
    (35, 0),
    (36, 1),
    (39, 1),
    (40, 1),
    (41, 1),
    (42, 1),
    (43, 1),
    (44, 1),
    (45, 0),
    (50, 1),
];
const ENV_OS: u32 = 1; // 1.2.1.2.4.1  'linux 7.0.0-28-generic'
const ENV_SHELL: u32 = 3; // 1.2.1.2.4.3  'zsh'
const ENV_TZ: u32 = 10; // 1.2.1.2.4.10 时区
const ENV_CWD: u32 = 11; // 1.2.1.2.4.11 工作目录

// ── 模型(1.9 / 1.14)内部 ──────────────────────────────────────────────────
const MODEL_NAME: u32 = 1;
const MODEL_PARAMS: u32 = 3; // repeated {1:key, 2:val}
const PARAM_KEY: u32 = 1;
const PARAM_VAL: u32 = 2;

// ── 响应 ────────────────────────────────────────────────────────────────────
const RESP_MESSAGE: u32 = 1; // 响应 field 1 = 流式消息
/// `1.1` = **正文**增量块,`1.1.1` 是本体。
///
/// ⚠️ 曾经错把 `1.4` 当正文。`1.4` 是**思考**流 —— 两者都是 `{1: 文本, 2: 1}`,
/// 结构一模一样,只有字段号不同,所以解错了不会报任何错:请求正常、有字出来,
/// 只是出来的是模型的推理过程而不是它的回答,而且回答本身一个字都收不到。
/// 实测一次「只回答 PINEAPPLE」的会话:`1.4.1` 是「用户要求…」,
/// `1.1.1` 才是 `P` / `INE` / `APPLE`。
const RESP_TEXT: u32 = 1; // 1.1  正文增量块
const RESP_THINKING: u32 = 4; // 1.4  思考增量块
const RESP_DELTA_TEXT: u32 = 1; // 1.{1,4}.1 增量本体
                                // ── 图片附件(1.2.1.1.3)内部 ───────────────────────────────────────────────
                                //
                                // 逐字对齐 2026-08-07 带图抓包。**内联原始字节,不走 FileSync** ——
                                // 文档 §5 猜的「图片走 blob 上传」是错的:带图那次请求里没有任何 FSSyncFile 调用。
const ATT_UUID: u32 = 2;
const ATT_PATH: u32 = 3; // 客户端本地路径,如 '…/images/粘贴的图像-<uuid>.png'
const ATT_DIMS: u32 = 4; // {1: 宽, 2: 高}
const ATT_DIM_W: u32 = 1;
const ATT_DIM_H: u32 = 2;
const ATT_MIME: u32 = 7; // 'image/png'
const ATT_CONTENT: u32 = 9; // {1: bin[32] 内容哈希, 2: 原始字节}
const ATT_HASH: u32 = 1;
const ATT_BYTES: u32 = 2;

// ── 文档附件(1.2.1.2.20)内部 ──────────────────────────────────────────────
//
// 比图片还简单:只有路径 + 内容,没有 mime、没有哈希。
//
// ⚠️ **内容是 proto3 `string`,不是 `bytes`。** 真客户端把 PDF 当 UTF-8 文本读,
// 二进制部分被有损替换成 U+FFFD(抓包实物:`%PDF-1.4\r%` 后面紧跟三个 `\xef\xbf\xbd`)。
// 也就是说 **PDF 只有文本层能用**,这是上游客户端自己的行为,不是我们的损失。
// 我方照做:传进来的字节按 `from_utf8_lossy` 转一遍,与真客户端逐字节同构。
const DOC_PATH: u32 = 1;
const DOC_CONTENT: u32 = 2;

/// 一张图片附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    /// mime,如 `image/png`。
    pub mime: String,
    /// 原始字节(**已 base64 解码**)。
    pub bytes: Vec<u8>,
    /// 像素宽高。解不出来时给 `(0, 0)` —— 真客户端总是填真值,
    /// 但我们只解 PNG/JPEG 头,别的格式宁可填 0 也不猜。
    pub width: u32,
    pub height: u32,
}

/// 一份文档附件(PDF 等)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocAttachment {
    /// 展示用的文件名/路径。调用方给的是 base64,没有路径,所以由我方合成。
    pub path: String,
    /// 内容。见 [`DOC_CONTENT`]:上游是 string,二进制会被有损转换。
    pub text: String,
    /// 我方抽出的文本层(`None` = 抽不到,如扫描件)。
    ///
    /// 它**不进 protobuf** —— 由 `chat.rs` 塞进用户消息文本。放在这里只是让
    /// 「一份文档」的信息聚在一处,避免调用点各自维护两个平行的列表。
    pub extracted: Option<String>,
}

impl ImageAttachment {
    /// ⚠️ 外面还有一层 `.1`:真包是 `1.2.1.1.3` → **`.1`** → {2,3,4,7,9}。
    /// 少这一层服务端回 `internal`(结构错的信号,不是 invalid_argument)。
    fn encode(&self, seq: usize) -> Writer {
        let mut inner = Writer::new();
        let w = &mut inner;
        let id = uuid::Uuid::new_v4().to_string();
        w.string(ATT_UUID, &id);
        // 路径是客户端本地路径。我们没有真文件,合成一个同形的 —— 上游只是回显/展示用。
        let ext = self.mime.rsplit('/').next().unwrap_or("png");
        w.string(ATT_PATH, &format!("/tmp/gw-cursor/attach-{seq}-{id}.{ext}"));
        if self.width > 0 && self.height > 0 {
            let mut d = Writer::new();
            d.uint(ATT_DIM_W, self.width as u64);
            d.uint(ATT_DIM_H, self.height as u64);
            w.message(ATT_DIMS, &d);
        }
        w.string(ATT_MIME, &self.mime);
        let mut c = Writer::new();
        c.bytes(ATT_HASH, &Sha256::digest(&self.bytes));
        c.bytes(ATT_BYTES, &self.bytes);
        w.message(ATT_CONTENT, &c);
        let mut outer = Writer::new();
        outer.message(1, &inner);
        outer
    }
}

/// 从 PNG / JPEG 头里取像素宽高。**只读头,不解像素** —— 所以没有解压炸弹风险。
pub fn image_dims(bytes: &[u8]) -> (u32, u32) {
    // PNG:魔数 8 字节 + IHDR(长度4+类型4)后紧跟 width/height 各 4 字节大端。
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return (w, h);
    }
    // JPEG:扫 SOFn 段(0xC0..0xCF,排除 C4/C8/CC 这三个非 SOF)。
    if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 <= bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return (w, h);
            }
            i += 2 + seg_len.max(2);
        }
    }
    (0, 0)
}

// ── 工具声明(1.2.1.2.7)内部 ───────────────────────────────────────────────
//
// 逐字对齐真包(16 条 `cursor-ide-browser` 的 MCP 工具,5 个字段**全部必填**)。
const TOOL_FULL_NAME: u32 = 1; // '<命名空间>-<裸名>'
const TOOL_DESC: u32 = 2;
const TOOL_NAMESPACE: u32 = 4; // MCP server 名
const TOOL_BARE_NAME: u32 = 5;
const TOOL_SCHEMA: u32 = 6; // JSON Schema 字符串

/// 我方转发 Anthropic `tools` 时用的命名空间。
///
/// 真包里这一位是 MCP server 名(如 `cursor-ide-browser`)。我们不是真 MCP server,
/// 但这个字段必填,所以取一个固定值 —— 同时它也是**回调时认领工具的依据**:
/// 模型调 `<ns>-<name>` 回来,我们据此判断"这是调用方的工具"而不是 Cursor 内建工具。
pub const TOOL_NS: &str = "gwtools";

/// 一个从 Anthropic 请求 `tools` 里读出来的工具声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// `input_schema` 的 JSON 序列化。
    pub schema: String,
}

impl ToolDef {
    fn encode(&self) -> Writer {
        let mut w = Writer::new();
        w.string(TOOL_FULL_NAME, &format!("{TOOL_NS}-{}", self.name));
        w.string(TOOL_DESC, &self.description);
        w.string(TOOL_NAMESPACE, TOOL_NS);
        w.string(TOOL_BARE_NAME, &self.name);
        w.string(TOOL_SCHEMA, &self.schema);
        w
    }
}

/// 顶层 `field 2` 与 `1.2` = **工具调用通道**(§10.7:client 事件/工具结果)。
///
/// 2026-08-07 实测的停摆签名:`1.8.1 = 17` → `1.2`(458B)→ 顶层 `field 2`(286B)
/// → 然后每 10 秒一个 4 字节 `1.13` 心跳,**到永远**。
///
/// 语义:模型决定调工具了,正等客户端把结果用请求侧的 `field 2` 帧送回去。
/// 真 IDE 会执行工具并回帧;我们目前不会,所以必须**主动收口**而不是陪它等 ——
/// 陪等的代价是每个"需要动手"的请求都挂满 watchdog 的 90 秒。
const RESP_TOOL_CHANNEL: u32 = 2;

/// 内建 **exec 调用**(`ExecServerMessage`,2026-08-10 与 opencodex 项目的
/// `agent.v1` 全量 protobuf schema 交叉核对过字段号):服务端让**客户端**执行
/// 文件/命令操作,客户端在同一条 BiDi 流的请求侧回 `ExecClientMessage`。
/// 对我们最重要的两个:
///   field 3 = write_args {1:path, 2:file_text, 3:tool_call_id, 4:return_content, 5:file_bytes}
///   field 7 = read_args  {1:path, 2:tool_call_id}
///
/// 带图请求的必经流程:服务端把附件字节用 write 推给客户端「落盘」
/// (`/assets/attach-N-<uuid>.png`),模型随后用 read 读同一路径把图看进去。
/// 真 IDE 有真磁盘所以无感;我们不回执的话,服务端 90 秒心跳死等(2026-08-10 实测),
/// 表现就是带图请求全模型 502。
///
/// 实物帧(`tests/fixtures/asset_echo_real.bin`,200B):
///   顶层 field 2(exec_server_message)→ field 3 write_args{1:路径, 5:字节}
///   + field 19 span_context(标准 OTel){1:32hex trace, 2:16hex span, 3:flags} + field 55=0。
/// 注意它**不裹** `1.RESP_MESSAGE`,与工具调用帧(1.2)的包装不对称 —— 两路都要认。
/// 此前这一帧被误判成「内建工具调用」在出字前收口:凡是带图请求,全模型
/// (fable/opus/grok 实测)一律 502 EmptyResponse,客户端重试风暴。
const EXEC_WRITE_ARGS: u32 = 3; // exec_server_message.3 = write_args
const EXEC_READ_ARGS: u32 = 7; // exec_server_message.7 = read_args
/// exec 消息的关联 id(`uint32 id` / `string exec_id`),回执时原样带回。
/// 实测的资产写调用两个都缺省(0 / "")。
const EXEC_ID: u32 = 1;
const EXEC_EXEC_ID: u32 = 15;

// ── 工具调用帧(响应侧)的字段号 ────────────────────────────────────────────
//
// 2026-08-07 对真上游实测解出。整条路径:
//   1.2.1        call_id,形如 "call-<uuid>-N\nfc_<uuid>_N"(两段用 \n 连)
//   1.2.2.<N>    N = **工具身份**。内建工具是闭集枚举(.1 终端、.4 读文件…),
//                我们声明的外部/MCP 工具统一落在 **.15**
//   1.2.2.15.1   {1:全名, 2:[参数], 3:call_id, 4/9:命名空间, 5:裸名}
//   参数         repeated {1: key, 2: {3: 字符串值}} —— 值多裹一层,3 = string
const TC_CALL_ID: u32 = 1; // 1.2.1
const TC_DETAIL: u32 = 2; // 1.2.2
const TC_EXTERNAL: u32 = 15; // 1.2.2.15  外部/MCP 工具
const TC_INNER: u32 = 1; // 1.2.2.15.1
const TC_ARGS: u32 = 2; // …1.2  repeated
const TC_ARG_KEY: u32 = 1;
const TC_ARG_VAL: u32 = 2;
const TC_BARE_NAME: u32 = 5;

// ── 参数值 = `google.protobuf.Value` ───────────────────────────────────────
//
// `1.2.2.15.1.2.2` 里那"多裹的一层"不是我方猜的包装,是标准的
// `google.protobuf.Value` oneof。抓包实物只出现过 field 3(字符串),
// 早先据此把解析写死成"只读 field 3",于是**模型传数字时解出空串**:
// 2026-08-07 实测 opencode 的 `read` 收到 `limit=""` / `offset=""`,
// 工具失败 → 模型改用「数值类型」再试 → 还是空 → 无限重试
// (屏幕上是一串 `→Read … [limit=, offset=]`)。
//
// 字段号照 protobuf 官方定义,不是逆向来的:
const PBV_NULL: u32 = 1; // null_value  (enum, varint)
const PBV_NUMBER: u32 = 2; // number_value(double → wire type 1)
const PBV_STRING: u32 = 3; // string_value
const PBV_BOOL: u32 = 4; // bool_value  (varint)
const PBV_STRUCT: u32 = 5; // struct_value(Struct)
const PBV_LIST: u32 = 6; // list_value  (ListValue)
/// `Struct.fields` / `ListValue.values` 都是 field 1 的 repeated。
const PBV_ENTRIES: u32 = 1;
/// 嵌套深度上限。上游不可信,不设限就是一条栈溢出通道。
const PBV_MAX_DEPTH: u32 = 12;

/// 解一个 `google.protobuf.Value` 成 JSON。
///
/// 认不出来(空 oneof / 未知字段 / 超深)→ `Value::Null`,让调用方看到
/// `null` 而不是空字符串 —— 空字符串会被工具当成"给了个空值"而静默做错事,
/// `null` 至少是"没给"。
fn decode_pb_value(bytes: &[u8], depth: u32) -> Value {
    if depth > PBV_MAX_DEPTH {
        return Value::Null;
    }
    for (f, v) in Reader::new(bytes) {
        match (f, &v) {
            (PBV_NULL, _) => return Value::Null,
            (PBV_NUMBER, _) => {
                if let Some(n) = v.as_f64() {
                    // 整数值回成 JSON 整数:工具 schema 写 `"type":"integer"` 时
                    // `200.0` 可能过不了校验(而模型要的就是 200)。
                    if n.fract() == 0.0 && n.abs() < 9e15 {
                        return Value::from(n as i64);
                    }
                    return serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number);
                }
            }
            (PBV_STRING, PbValue::Len(s)) => {
                return Value::String(String::from_utf8_lossy(s).into_owned())
            }
            (PBV_BOOL, PbValue::Varint(n)) => return Value::Bool(*n != 0),
            (PBV_STRUCT, PbValue::Len(st)) => {
                let mut map = serde_json::Map::new();
                // Struct.fields = repeated MapEntry{1:key, 2:Value}
                for (ef, ev) in Reader::new(st) {
                    if ef != PBV_ENTRIES {
                        continue;
                    }
                    let PbValue::Len(entry) = ev else { continue };
                    let mut k = String::new();
                    let mut val = Value::Null;
                    for (kf, kv) in Reader::new(entry) {
                        match (kf, kv) {
                            (1, PbValue::Len(b)) => k = String::from_utf8_lossy(b).into_owned(),
                            (2, PbValue::Len(b)) => val = decode_pb_value(b, depth + 1),
                            _ => {}
                        }
                    }
                    if !k.is_empty() {
                        map.insert(k, val);
                    }
                }
                return Value::Object(map);
            }
            (PBV_LIST, PbValue::Len(list)) => {
                let mut arr = Vec::new();
                for (lf, lv) in Reader::new(list) {
                    if lf != PBV_ENTRIES {
                        continue;
                    }
                    if let PbValue::Len(b) = lv {
                        arr.push(decode_pb_value(b, depth + 1));
                    }
                }
                return Value::Array(arr);
            }
            _ => {}
        }
    }
    Value::Null
}

/// 上游发起的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 原样的 call id。回传结果时必须用它对上,所以**绝不能重新生成**。
    pub id: String,
    /// 裸工具名(不含命名空间前缀)—— 与调用方 Anthropic 请求里的 `name` 对得上。
    pub name: String,
    /// 参数 `{key: value}`。值是**已解好的 JSON**:上游用
    /// `google.protobuf.Value` 承载,数字/布尔/对象/数组都进得来
    /// (见 [`decode_pb_value`])。
    pub args: Vec<(String, Value)>,
}

/// 解一帧工具调用。不是外部工具(或根本不是工具帧)→ `None`。
///
/// 只认 `1.2.2.15`(我们声明的工具)。Cursor **内建**工具走别的字段号,
/// 那些我们执行不了 —— 认出来也只能报错,不如让调用方看到"模型跑偏了"。
pub fn parse_tool_call(payload: &[u8]) -> Option<ToolCall> {
    let msg = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (RESP_MESSAGE, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let ch = Reader::new(msg).find_map(|(f, v)| match (f, v) {
        (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;

    let mut id = String::new();
    let mut detail = None;
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (TC_CALL_ID, PbValue::Len(s)) => id = String::from_utf8_lossy(s).to_string(),
            (TC_DETAIL, PbValue::Len(s)) => detail = Some(s),
            _ => {}
        }
    }
    let inner = Reader::new(detail?)
        .find_map(|(f, v)| match (f, v) {
            (TC_EXTERNAL, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .and_then(|ext| {
            Reader::new(ext).find_map(|(f, v)| match (f, v) {
                (TC_INNER, PbValue::Len(s)) => Some(s),
                _ => None,
            })
        })?;

    let mut name = String::new();
    let mut args = Vec::new();
    for (f, v) in Reader::new(inner) {
        match (f, v) {
            (TC_BARE_NAME, PbValue::Len(s)) => name = String::from_utf8_lossy(s).to_string(),
            (TC_ARGS, PbValue::Len(kv)) => {
                let mut k = String::new();
                let mut val = Value::Null;
                for (f2, v2) in Reader::new(kv) {
                    match (f2, v2) {
                        (TC_ARG_KEY, PbValue::Len(s)) => k = String::from_utf8_lossy(s).to_string(),
                        // 值是 google.protobuf.Value,**不是**只有字符串一档。
                        (TC_ARG_VAL, PbValue::Len(wrap)) => val = decode_pb_value(wrap, 0),
                        _ => {}
                    }
                }
                if !k.is_empty() {
                    args.push((k, val));
                }
            }
            _ => {}
        }
    }
    if name.is_empty() {
        return None;
    }
    // ⚠️ **空 id 不许放行。** Anthropic 要求 `tool_use.id` 非空、且后续 `tool_result`
    // 靠它对上。上游帧缺 `1.2.1`(或字段号漂移)时这里会是空串,交出去的后果是
    // 客户端要么校验报错、要么回传时对不上,工具回路以很难查的方式断掉。
    // 而字段号漂移恰恰是这个 provider 的常态风险。
    //
    // 合成一个而不是丢弃整个调用:工具名解出来了,说明模型确实想调这个工具 ——
    // 丢掉等于凭空少了一次调用,比 id 难看糟得多。我方从不把 call id 发回上游
    // (每轮都是全新的 Opening 请求),所以合成是安全的。
    if id.is_empty() {
        let synth = format!("call_{}", uuid::Uuid::new_v4().simple());
        tracing::warn!(
            tool = %name, synth = %synth,
            "cursor 工具调用帧缺 call id(1.2.1),合成一个 —— 疑似上游字段号漂移,值得查"
        );
        return Some(ToolCall {
            id: synth,
            name,
            args,
        });
    }
    Some(ToolCall { id, name, args })
}

/// 内建工具调用的**身份字段号**(`1.2.2.<N>` 里的 N)。外部/MCP 工具(`.15`)返回
/// `None` —— 那条路由 [`parse_tool_call`] 处理。
///
/// ## 为什么是字段号而不是名字
///
/// Cursor 的内建工具是**闭集枚举、不带名字**的:身份就编码在字段号里
/// (§13.2 抓包实证:`.1` 终端命令、`.4` 读文件)。所以「模型刚才想干什么」这个
/// 问题在这里只能得到一个数字,认识的数字才翻得成人话。
///
/// 这个数字有两个用处:
/// 1. 下一轮纠偏时**指名**换哪个工具(见 chat.rs 的 `builtin_capability`);
/// 2. 收口日志里把**不认识**的字段号记下来 —— 抓包定枚举那一步的现成线索,
///    不用先猜哪些内建工具在被调。
///
/// 跳过的字段是已知的非身份位:`15` 外部工具、`57` 重复的 call_id、`59` 毫秒时间戳。
pub fn builtin_tool_ident(payload: &[u8]) -> Option<u32> {
    let ch = tool_channel(payload)?;
    let detail = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (TC_DETAIL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    Reader::new(detail).find_map(|(f, v)| match (f, v) {
        (TC_EXTERNAL, _) => None,
        (57 | 59, _) => None,
        (f, PbValue::Len(_)) => Some(f),
        _ => None,
    })
}

/// 一次**已实证**的内建工具调用(§13.2 抓包:`.1` 终端命令、`.4` 读文件)。
///
/// 只为这两个存在:它们是收口日志里的绝对大头,且参数字段号有实物依据
/// (`.1` = `{1:{1:'ls -la', 3:超时, 5:'ls', 8:argv, 15:描述}}`,
///  `.4` = `{1:{2:'README'}}`)。其余内建身份(代码检索、网页搜索…)参数形状
/// 未实证,**不猜** —— 翻译错比收口更糟:客户端会执行一个参数张冠李戴的工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinCall {
    /// `1.2.2.1`:终端命令,命令串在 `.1.1`。
    Terminal { id: String, command: String },
    /// `1.2.2.4`:读文件,路径在 `.1.2`。
    ReadFile { id: String, path: String },
}

/// 解一帧**内建**工具调用的参数(chat.rs 兼容转换层用)。
///
/// 与 [`builtin_tool_ident`] 同源(同一个 detail 块),但只认参数形状有抓包
/// 实证的 `.1` / `.4`;参数缺失或为空一律 `None`,让调用方落回收口 ——
/// 转换层的失败必须是单向的(认不出 → 维持原行为),绝不输出错参数的调用。
pub fn parse_builtin_call(payload: &[u8]) -> Option<BuiltinCall> {
    // ⚠️ 只认 `1.2`(RESP_MESSAGE → 工具通道)的包装形态,与 parse_tool_call 同款。
    // 不用 tool_channel():那个函数还接受**顶层** field 2 —— 那是 exec 通道
    // (asset-echo 帧,顶层 field 2 = exec_server_message),其 field 2 恰好是
    // shell_args,拿工具通道的字段号去解它是张冠李戴。exec 通道由
    // parse_exec_write/read 处理,这里绝不越界。
    let msg = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (RESP_MESSAGE, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let ch = Reader::new(msg).find_map(|(f, v)| match (f, v) {
        (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut id = String::new();
    let mut detail = None;
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (TC_CALL_ID, PbValue::Len(s)) => id = String::from_utf8_lossy(s).to_string(),
            (TC_DETAIL, PbValue::Len(s)) => detail = Some(s),
            _ => {}
        }
    }
    for (f, v) in Reader::new(detail?) {
        let PbValue::Len(body) = v else { continue };
        match f {
            // 终端命令:`.1.1.1` = 命令串。
            1 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let command = Reader::new(inner).find_map(|(f3, v3)| match (f3, v3) {
                    (1, PbValue::Len(s)) => Some(String::from_utf8_lossy(s).into_owned()),
                    _ => None,
                })?;
                if command.trim().is_empty() {
                    return None;
                }
                return Some(BuiltinCall::Terminal { id, command });
            }
            // 读文件:`.4.1.2` = 路径。
            4 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let path = Reader::new(inner).find_map(|(f3, v3)| match (f3, v3) {
                    (2, PbValue::Len(s)) => Some(String::from_utf8_lossy(s).into_owned()),
                    _ => None,
                })?;
                if path.trim().is_empty() {
                    return None;
                }
                return Some(BuiltinCall::ReadFile { id, path });
            }
            _ => {}
        }
    }
    None
}

/// 这一帧是不是「上游在等客户端执行工具」。
///
/// 判定**故意保持宽松**(字段在就算,不看 wire type):这里的首要职责是
/// 「别漏」—— 漏掉一帧真工具调用的代价是陪上游死等心跳到 watchdog。
/// 严的解析交给 [`parse_tool_call`] / [`parse_asset_echo`],认不出再分类。
pub fn is_tool_call(payload: &[u8]) -> bool {
    for (f, v) in Reader::new(payload) {
        if f == RESP_TOOL_CHANNEL {
            return true;
        }
        if f == RESP_MESSAGE {
            if let PbValue::Len(sub) = v {
                if Reader::new(sub).any(|(f2, _)| f2 == RESP_TOOL_CHANNEL) {
                    return true;
                }
            }
        }
    }
    false
}

/// CLI 形态的「本轮已提交」回显(CLI 响应**没有 `1.14` 用量帧**,2026-08-16 抓包实证):
/// 正文/思考增量之后,服务端按序回显 thinking 摘要、正文消息,最后一帧 field-4 回显是
/// 提交记录 —— `4.3.2.1` = `{1: bin[32], 2: bin[32]×N(消息哈希), 3: 本轮 turn id,
/// 4: 172 字符签名, 5: 0}`。认出它就该收口,否则要等 90s 心跳看门狗。
pub fn is_turn_commit(payload: &[u8]) -> bool {
    for (f, v) in Reader::new(payload) {
        if f != 4 {
            continue;
        }
        let PbValue::Len(echo) = v else { continue };
        let mut cur = Some(echo);
        for want in [3u32, 2, 1] {
            let Some(bytes) = cur else { break };
            cur = None;
            for (f2, v2) in Reader::new(bytes) {
                if f2 == want {
                    if let PbValue::Len(s) = v2 {
                        cur = Some(s);
                    }
                    break;
                }
            }
        }
        let Some(rec) = cur else { continue };
        // 提交记录的指纹:field 3 = 36 字符 turn id,field 4 = 长签名串。
        let mut has_turn = false;
        let mut has_sig = false;
        for (f3, v3) in Reader::new(rec) {
            match (f3, v3) {
                (3, PbValue::Len(s)) => has_turn = s.len() == 36,
                (4, PbValue::Len(s)) => has_sig = s.len() > 100,
                _ => {}
            }
        }
        if has_turn && has_sig {
            return true;
        }
    }
    false
}

/// Run 响应的「续轮描述符」(顶层 field 3,2026-08-23 抓包实证):服务端把跨轮
/// 状态当**不透明字节**回声给客户端,客户端下一轮原样回放(请求侧 `.1`)。
///
/// 实测形态(官方 CLI 两轮抓包,/tmp/cursor-capture):turn1(新会话)响应尾部
/// 两帧(resp-011/013)顶层各带一份 `.3`,内容逐字节相同;turn2(resume)尾部
/// 同样是两份相同的 `.3`,但 blob 引用从 4 个涨到 6 个(多出 turn1 产生的新
/// blob)。schema 与请求 `.1` 相同:{repeated .1 = 32B blob 引用, .5 = 预算表,
/// .8 = 32B, .9 = cwd, .10 = mode, .22 = 'cli', .26 = ts, .27 = tz}。
///
/// 对我们是**不透明字节:永不解析修改、永不构造**,只捕获存储(影子模式)。
/// 这里唯一的"读"是定位顶层 field 3 本身。一帧内多次出现时取**最后一份**
/// (尾部更接近流尾,状态最新)。
pub fn descriptor_field3(payload: &[u8]) -> Option<&[u8]> {
    let mut last = None;
    for (f, v) in Reader::new(payload) {
        if f == 3 {
            if let PbValue::Len(s) = v {
                // ⚠️ **零长度不算捕获**(codex 审查 #4)。空 `.3` 会让上层
                // `is_some()` 判成「有料」→ 选增量消息 + `1.1` 零长度 = 服务端没有
                // 历史、我方也没发 = 静默失忆。空描述符没有任何回放价值,当没有。
                if !s.is_empty() {
                    last = Some(s);
                }
            }
        }
    }
    last
}

/// 数描述符里 32B blob 引用(`.1`)的个数 —— 影子日志的元数据。
/// 这是影子模式对描述符内容的**唯一**一次读取,除此之外不碰内部字节。
pub fn descriptor_ref_count(desc: &[u8]) -> usize {
    Reader::new(desc)
        .filter(|(f, v)| *f == 1 && matches!(v, PbValue::Len(s) if s.len() == 32))
        .count()
}

/// KV 子协议请求(2026-09-04 对齐 YeautyYE/claude-cursor-proxy 的 `agent.v1` schema):
/// 顶层 field 4 = `kv_server_message` `{1: id(varint), 2: get_blob_args{1: blob_id},
/// 3: set_blob_args{1: blob_id, 2: blob_data}}`。
///
/// ⚠️ 这钉死了早先两个按形状猜的解析:
/// 1. 回执里的 `.1` 不是「按点名顺序的自增 slot」,是 **KV 请求 id 回显**
///    (08-23 实物里恰好是 0,1,2…,两者巧合同值);
/// 2. `4.3`(set_blob)不只是「回显让我们存」,**还必须回执 `set_blob_result{}`**
///    (= `kv_client_message{1: id, 3: {}}`,即 [`crate::cli::frame_field3_slot`])。
///    不回执的代价:服务端不出 checkpoint,90s 心跳死等 —— 2026-09-04 探针实证
///    (出字后 4 帧 set_blob 无人回执,`.3` 描述符与用量尾帧全部不来)。
pub enum KvRequest<'a> {
    /// get_blob_args:服务端点名要内容分节。回 `kv_client_message{id, get_blob_result{1: data}}`
    /// (= [`crate::cli::content_slot_frame`])。
    Get { id: u64, hash: [u8; 32] },
    /// set_blob_args:服务端推来一个内容节让我们存。存下并回
    /// `kv_client_message{id, set_blob_result{}}`(= [`crate::cli::frame_field3_slot`])。
    Set { id: u64, data: &'a [u8] },
}

/// 解析一帧里的全部 KV 请求(一帧可含多个顶层 field 4;单个 kv_server_message
/// 只会带 get/set 之一)。
pub fn kv_requests(payload: &[u8]) -> Vec<KvRequest<'_>> {
    let mut out = Vec::new();
    for (f, v) in Reader::new(payload) {
        if f != 4 {
            continue;
        }
        let PbValue::Len(kv) = v else { continue };
        // id 缺省 0(proto3 默认值不上线,实物里 id=0 的请求就是没有 `.1`)。
        let id = Reader::new(kv)
            .find_map(|(f2, v2)| match (f2, v2) {
                (1, PbValue::Varint(n)) => Some(n),
                _ => None,
            })
            .unwrap_or(0);
        for (f2, v2) in Reader::new(kv) {
            let PbValue::Len(args) = v2 else { continue };
            match f2 {
                // get_blob_args {1: blob_id}
                2 => {
                    for (f3, v3) in Reader::new(args) {
                        if f3 == 1 {
                            if let PbValue::Len(s) = v3 {
                                if let Ok(h) = <[u8; 32]>::try_from(s) {
                                    out.push(KvRequest::Get { id, hash: h });
                                }
                            }
                        }
                    }
                }
                // set_blob_args {1: blob_id, 2: blob_data}
                3 => {
                    for (f3, v3) in Reader::new(args) {
                        if f3 == 2 {
                            if let PbValue::Len(data) = v3 {
                                if !data.is_empty() {
                                    out.push(KvRequest::Set { id, data });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// 服务端「要求补传内容分节」的哈希回显个数(field-4 通道的 `4.2 = {1: bin[32]}`)。
///
/// ## 这是描述符路线唯一的地基风险
///
/// PROTOCOL §20.2 实测:我方**合成**的续轮(帧级照抄真包)被服务端用一串
/// `4.2={1:bin[32]}` 卡住 —— 那是内容寻址存储(CAS)在要客户端按需把内容分节
/// 补上来。而 §P3a(2026-08-23)观察官方 CLI 的 resume 时**零 FileSync/零上传**,
/// 于是结论写成「blob 闸门不存在,捕获 `.3` 回放即可」。
///
/// 两件事不矛盾但**不能互推**:官方首轮 Run 期间内容已经在服务端落好了,
/// 它 resume 时当然不用补;我方回放描述符时服务端手里有没有那些分节,
/// **没有任何证据**。所以每轮都数一次:一旦这个数 > 0,说明描述符回放在我方
/// 流量上还缺一条 CAS 上传腿,`.3` 回放不足以走通,必须回退并把这件事报上来。
///
/// 与 [`is_turn_commit`] 的区别:提交记录住在 `4.3.2.1`,哈希需求住在 `4.2.1`,
/// 同一个 field-4 回显通道的两个不同分支。
pub fn content_hash_echo(payload: &[u8]) -> usize {
    content_hash_demands(payload).len()
}

/// 服务端点名要的内容分节哈希(field-4 通道的 `4.2.1 = bin[32]`)。
///
/// ## 机制(2026-08-23 turn4 抓包 + 时序实证,已推翻早先的定性)
///
/// 这**不是错误信号,是正常协议事件**。turn4(resume,距首轮 2.4h)的时序:
///
/// ```text
/// .042  req-000  帧0(带描述符)
/// .365  resp-000
/// 212.236–.244  resp-001..007  ← 服务端 7 个 4.2 需求,一帧一个哈希
/// 212.246–.255  req-001..007   ← 客户端 2ms 后交出 7 个内容槽
/// 213.495 req-008  ENV         ← ENV 在内容交换之后
/// ```
///
/// 需求 7 个、上传 7 个、**逐一配对**(每个上传节的 sha256 正好等于被点名的哈希),
/// slot 序号 = 需求顺序 0..6。turn1/turn2 的响应里 `4.2` **零出现** —— 服务端当时
/// 还缓存着内容,不需要问。
///
/// 所以描述符 `.1`/`.8` 是一张**内容寻址清单**:`.1` 装消息节哈希、`.8` 装轮次提交
/// 记录哈希(实测 slot1 就是上一轮的 commit 记录 `{1:bin32, 2:bin32×N, 3:turn uuid,
/// 4:182B 签名, 5:3}`)。回放清单只是第一条腿,**按需交出内容是第二条腿**。
pub fn content_hash_demands(payload: &[u8]) -> Vec<[u8; 32]> {
    kv_requests(payload)
        .into_iter()
        .filter_map(|r| match r {
            KvRequest::Get { hash, .. } => Some(hash),
            _ => None,
        })
        .collect()
}

/// 服务端回显的内容分节(field-4 通道的 `4.3.2` 那一层的**原始字节**)。
///
/// ## 这是内容供给的唯一来源(2026-08-23 实证)
///
/// 服务端把它创建的每一个内容节都在响应流里回显一次,节字节就是 `4.3.2` 这层的原始
/// 字节 —— `sha256(那段字节)` 正好等于描述符里的引用。逐条验过:
///
/// ```text
/// turn2 resp-006  590B → 49efdd84…  = turn4 slot6(user 消息)
/// turn2 resp-008  330B → 8ef11f32…  = turn4 slot1(轮次提交记录)
/// turn2 resp-010 1269B → 055fd9c0…  = turn4 slot0(assistant 消息)
/// turn1 resp-002 2025B → 83ccb7a5…  = Composer 系统提示节
/// ```
///
/// turn4 描述符的 8 个引用(6 refs + 2 f8)**8/8** 都能在前几轮的回显里找到。
/// 官方 CLI 的 `~/.cursor/chats/*/*/store.db` `blobs` 表就是这么攒出来的 ——
/// 它不是知识库,是**回显缓存**(实测 70/70 条 `id == sha256(data)`)。
///
/// 所以我方**不需要自建任何节**,也不需要复现服务端的 JSON 序列化(那本来是做不到的:
/// hash 差一个字节就不认)。只要把回显存下来,后续被点名就能原样交回去。
pub fn section_echoes(payload: &[u8]) -> Vec<&[u8]> {
    kv_requests(payload)
        .into_iter()
        .filter_map(|r| match r {
            KvRequest::Set { data, .. } => Some(data),
            _ => None,
        })
        .collect()
}

/// 一轮 wire(CLI 形态)请求的收尾体检表 —— **正向判据**。
///
/// ## 为什么回退判据不能写成「服务端拒绝描述符」
///
/// 描述符不被接受时上游**不报错**:200 + 每 10s 心跳 + 永不 trailer,与 IDE 形态缺
/// `1.2.1.2` 同族。等一个 4xx 永远等不到 —— 请求会挂到超时再返回空,而那正是生产上
/// 偶发 `EmptyResponse` 的形态(3 次/60s → 该号冷却 60s)。所以判据是「**该出现的
/// 没出现**」,不是「出现了错误」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireTurnOutcome {
    /// 见到顶层 `.3` 续轮描述符(= 下一轮回放的清单)。
    pub saw_descriptor: bool,
    /// 见到干净收尾:CLI 的 turn_commit 回显,或用量帧 / trailer。
    pub saw_finish: bool,
    /// 正文增量累计字符数。**思考不计** —— 只吐思考不吐正文对客户就是空回复。
    pub content_chars: usize,
    /// 服务端点名要的内容分节个数(见 [`content_hash_demands`])。
    pub demanded: usize,
    /// 其中**我方拿不出来**的个数。这才是失败信号。
    pub unavailable: usize,
}

/// [`WireTurnOutcome`] 的判决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireVerdict {
    /// 有正文、有收尾。
    Ok,
    /// 服务端点名的分节里有我方拿不出来的 —— 描述符清单指向我方未持有的内容。
    ///
    /// 已知必然踩中的一类:每份描述符的第一个 `.1` 引用都是 Cursor 自己那 2025B 的
    /// Composer 系统提示节(`{"role":"system","content":"You are an AI coding
    /// assistant, powered by Composer…"}`),它**不在任何 ENV 帧里**(turn1/turn4 实测
    /// 都没有),是官方 CLI 自己持有的。我方无法自建,被点名就只能重铺。
    ContentUnavailable { missing: usize, demanded: usize },
    /// 裸态:接受了请求,一个字没吐,也没收尾。
    Barren,
    /// 干净收尾但**没有新的 `.3`**。
    ///
    /// 官方每轮尾部都给新描述符(实测 turn1/turn2/turn4 各 3 份 `.3`),不给就是异常。
    /// 后果不对称:本轮的回答是好的(不该扔),但**旧描述符已经缺了本轮** ——
    /// 留着它下轮回放就是只发增量却配一张少一轮的清单 = 静默失忆。
    /// 所以 `needs_reflow` 为真、`should_fallback` 为假。
    NoDescriptor,
    /// 出了字但没干净收尾:轮次可用,但**描述符不可信**(可能装着未完结的状态)。
    NoFinish,
}

impl WireVerdict {
    /// 这个判决是否要求**作废描述符**(下一轮重铺)。
    ///
    /// 比 [`WireTurnOutcome::should_fallback`] 宽一档:`NoFinish` 不让本轮失败
    /// (出了字的轮次对客户有价值),但它的描述符**不可信** —— 可能装着未完结轮次的
    /// 状态,留着下轮回放就是静默失忆。所以它进作废、不进本轮回退。
    pub fn needs_reflow(self) -> bool {
        !matches!(self, WireVerdict::Ok)
    }

    /// 本轮是否该判失败并回退成内联全量重铺。
    ///
    /// `NoFinish` **不**回退:出了字的轮次对客户是有价值的,扔掉等于白烧一次上游
    /// 额度;它的代价由 [`Self::needs_reflow`](作废描述符)承担。
    pub fn should_fallback(self) -> bool {
        matches!(
            self,
            WireVerdict::ContentUnavailable { .. } | WireVerdict::Barren
        )
    }
}

impl WireTurnOutcome {
    pub fn verdict(self) -> WireVerdict {
        // ⚠️ 顺序:拿不出内容排最前 —— 它通常同时表现为裸态,但它是解释裸态成因的
        // 那个诊断,判成 Barren 会把根因盖掉。
        //
        // 注意**只有 `unavailable > 0` 才是失败**。`demanded > 0` 本身是正常协议事件
        // (服务端缓存过期后按需索取,见 `content_hash_demands`);早先把
        // 「出现 4.2」当 NO-GO 是错的,会把一次健康的 resume 判成方案不可行。
        if self.unavailable > 0 {
            return WireVerdict::ContentUnavailable {
                missing: self.unavailable,
                demanded: self.demanded,
            };
        }
        if self.content_chars == 0 && !self.saw_finish {
            return WireVerdict::Barren;
        }
        if !self.saw_finish {
            return WireVerdict::NoFinish;
        }
        if !self.saw_descriptor {
            return WireVerdict::NoDescriptor;
        }
        WireVerdict::Ok
    }

    /// 本轮是否该判失败并回退成内联全量重铺(委托给 [`WireVerdict::should_fallback`])。
    ///
    /// ⚠️ **`saw_descriptor == false` 不进这个判断。** 那说的是「下一轮没有回放的料」,
    /// 不是「这一轮失败了」—— 首轮本来就没有描述符可捕获,把它算进回退会让每条新会话
    /// 的首轮都白跑一次。没料的处理是下一轮走首轮形态,不是重试这一轮。
    pub fn should_fallback(self) -> bool {
        self.verdict().should_fallback()
    }
}

/// 会话登记通知(CLI 形态抓包新帧,2026-08-16):顶层 field 2 的
/// `request_context`(`{10: {2: conversation_id}, 19: span_context, 55: 0}`)。
///
/// 真 CLI 收到它**不回任何帧**(上下文已经在请求侧的 2.10 帧里推过了),
/// 生成照常开始。我方早期实现把它误收成「内建工具调用」,首轮还没出字就掐断。
/// 它与真正的 exec 请求的区分:只有 tracing 类字段(10/19/55),没有任何
/// 可执行负载(2 shell / 3 write / 7 read / 11 mcp / 14 shell_stream …)。
pub fn is_session_notice(payload: &[u8]) -> bool {
    /// exec 通道里「需要客户端动作」的负载字段号(PROTOCOL §18 的 schema 实证枚举)。
    const ACTIONABLE: &[u32] = &[2, 3, 4, 5, 7, 8, 9, 11, 14];
    for (f, v) in Reader::new(payload) {
        if f != RESP_TOOL_CHANNEL {
            continue;
        }
        let PbValue::Len(sub) = v else { continue };
        let mut has_request_context = false;
        for (f2, v2) in Reader::new(sub) {
            match f2 {
                10 => {
                    // request_context:{2: conversation_id}
                    if let PbValue::Len(inner) = v2 {
                        has_request_context = Reader::new(inner).any(|(f3, _)| f3 == 2);
                    }
                }
                f if ACTIONABLE.contains(&f) => return false,
                _ => {}
            }
        }
        return has_request_context;
    }
    false
}

/// 取出工具通道(`field 2`)的字节。工具调用帧裹在 `1.2` 里,资源回显帧
/// 直接挂在顶层 field 2(见 [`RESP_ASSET`] 的注释),两路都要认。
///
/// 与 [`is_tool_call`] 不同,这里是**解析**入口,只接受 length-delimited;
/// wire type 漂移的帧会落回调用方的「认不出」分支,而不是被静默跳过。
fn tool_channel(payload: &[u8]) -> Option<&[u8]> {
    for (f, v) in Reader::new(payload) {
        match (f, v) {
            (RESP_TOOL_CHANNEL, PbValue::Len(s)) => return Some(s),
            (RESP_MESSAGE, PbValue::Len(sub)) => {
                if let Some(s) = Reader::new(sub).find_map(|(f2, v2)| match (f2, v2) {
                    (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
                    _ => None,
                }) {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

/// 一次服务端写盘调用(`write_args`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecWrite {
    /// 关联 id,回执原样带回(实测的资产写调用里是缺省 0)。
    pub id: u64,
    pub exec_id: String,
    pub path: String,
    /// 要落盘的内容:`file_bytes`(field 5)优先,只有 `file_text`(field 2)时取其字节。
    pub bytes: Vec<u8>,
}

/// 一次服务端读文件调用(`read_args`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRead {
    pub id: u64,
    pub exec_id: String,
    pub path: String,
}

/// 取 exec 消息的关联 id(`1`=uint32、`15`=string),缺省即 0 / ""。
fn exec_ids(ch: &[u8]) -> (u64, String) {
    let mut id = 0u64;
    let mut exec_id = String::new();
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (EXEC_ID, PbValue::Varint(n)) => id = n,
            (EXEC_EXEC_ID, PbValue::Len(s)) => exec_id = String::from_utf8_lossy(s).to_string(),
            _ => {}
        }
    }
    (id, exec_id)
}

/// 解服务端写盘调用。不是 write_args(或结构对不上)→ `None`。
///
/// ⚠️ 认领条件:路径必须是**绝对路径**(以 `/` 开头)且内容在场
/// (`file_bytes` 或 `file_text`)。真工具调用帧的 `1.2.3` 也是个字符串
/// (另一个 id,形如 `<uuid>-0-<4字符>`,见 PROTOCOL §13.2)—— 单靠
/// 「field 3 能解出字符串」会把工具帧吞掉,工具回路直接哑掉。
pub fn parse_exec_write(payload: &[u8]) -> Option<ExecWrite> {
    let ch = tool_channel(payload)?;
    let args = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (EXEC_WRITE_ARGS, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut path = String::new();
    let mut bytes = None;
    for (f, v) in Reader::new(args) {
        match (f, v) {
            (1, PbValue::Len(s)) => path = String::from_utf8_lossy(s).to_string(),
            (2, PbValue::Len(s)) => {
                if bytes.is_none() {
                    bytes = Some(s.to_vec());
                }
            }
            (5, PbValue::Len(s)) => bytes = Some(s.to_vec()),
            _ => {}
        }
    }
    if !path.starts_with('/') {
        return None;
    }
    let (id, exec_id) = exec_ids(ch);
    Some(ExecWrite {
        id,
        exec_id,
        path,
        bytes: bytes?,
    })
}

/// 解服务端读文件调用。不是 read_args → `None`。认领只需绝对路径:
/// read_args 只有 {1:path, 2:tool_call_id} 两个字段,没有可混淆的同号字符串。
pub fn parse_exec_read(payload: &[u8]) -> Option<ExecRead> {
    let ch = tool_channel(payload)?;
    let args = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (EXEC_READ_ARGS, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut path = String::new();
    for (f, v) in Reader::new(args) {
        if let (1, PbValue::Len(s)) = (f, v) {
            path = String::from_utf8_lossy(s).to_string();
        }
    }
    if !path.starts_with('/') {
        return None;
    }
    let (id, exec_id) = exec_ids(ch);
    Some(ExecRead { id, exec_id, path })
}

/// 客户端 exec 回执的公共骨架:`AgentClientMessage{2: exec_client_message{…}}`。
/// `result_field` 与结果体外层由调用方定(write_result=3 / read_result=7,与服务侧同号)。
fn exec_client_frame(id: u64, exec_id: &str, result_field: u32, result: &Writer) -> Vec<u8> {
    let mut ecm = Writer::new();
    if id != 0 {
        ecm.uint(EXEC_ID, id);
    }
    if !exec_id.is_empty() {
        ecm.string(EXEC_EXEC_ID, exec_id);
    }
    ecm.message(result_field, result);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ecm);
    top.into_bytes()
}

/// 编码写盘成功回执:`write_result{1: write_success{1:path, 3:file_size}}`。
pub fn encode_write_success(id: u64, exec_id: &str, path: &str, file_size: usize) -> Vec<u8> {
    let mut ws = Writer::new();
    ws.string(1, path);
    ws.uint(3, file_size as u64);
    let mut wr = Writer::new();
    wr.message(1, &ws); // WriteResult.success
    exec_client_frame(id, exec_id, EXEC_WRITE_ARGS, &wr)
}

/// 编码读文件成功回执(二进制):`read_result{1: read_success{1:path, 4:file_size, 5:data}}`。
/// 图片必须走 `data`(bytes)而不是 `content`(string)—— PNG 不是合法 UTF-8。
pub fn encode_read_success_data(id: u64, exec_id: &str, path: &str, data: &[u8]) -> Vec<u8> {
    let mut rs = Writer::new();
    rs.string(1, path);
    rs.uint(4, data.len() as u64);
    rs.bytes(5, data);
    let mut rr = Writer::new();
    rr.message(1, &rs); // ReadResult.success
    exec_client_frame(id, exec_id, EXEC_READ_ARGS, &rr)
}

/// `1.14 {1: 输入, 2: 输出, 3: 缓存命中}` —— 用量,**同时是本轮的收尾信号**。
///
/// BiDi 流不会自己结束:发完这一帧上游就转成每 10 秒一个 4 字节心跳,永远挂着。
/// 不认它的话,每个请求都要等客户端超时才返回 —— 表现为「答完了但一直转圈」。
const RESP_USAGE: u32 = 14;

/// `1.14` 用量帧的分解。`input` 是**总量**(含 `cache_read` + `cache_write`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// `1.1.22` 内层仍是字符串 `'ide'`。
///
/// ⚠️ 这与请求头 `x-cursor-client-type: glass` **故意不同** —— 抓包实物就是这样
/// (PROTOCOL §2.2 vs §3.1)。别为了「统一」把这里也改成 `glass`。
const ENV_CLIENT_KIND_VALUE: &str = "ide";

/// 消息类型:1 = 用户。
///
/// 助手轮用 2 是**推断**(沿用退役的 ChatService 的 HUMAN=1/AI=2 约定),
/// 抓包只覆盖了用户轮。多轮历史若表现异常,这里是第一个怀疑点。
const MSG_KIND_USER: u64 = 1;
const MSG_KIND_ASSISTANT: u64 = 2;

/// 一个模型及其参数(`1.9` / `1.14` 同构)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub name: String,
    /// `1.9.3` 的 `{1:key, 2:val}` 列表。
    ///
    /// 值的类型文档没写(`fast=false` / `context=300k` / `effort=high` 混在一起),
    /// 这里一律按字符串编码 —— 与 `effort=high`、`context=300k` 这类字面量一致。
    pub params: Vec<(String, String)>,
    /// 是否进 `1.14` 可用模型清单(每个 Run 请求都带整份)。
    ///
    /// 探测中的模型(还没在真机菜单里证实)必须 `false`:可选中(`1.9` 当前模型),
    /// 但不进清单 —— 未证实条目混进 `1.14` 会污染**所有**模型的请求
    /// (审查 gpt-5.6-sol 高危),也让我们的清单与任何真实客户端版本都对不上。
    pub menu_visible: bool,
}

impl Model {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            params: Vec::new(),
            menu_visible: true,
        }
    }

    pub fn with_params(name: impl Into<String>, params: &[(&str, &str)]) -> Self {
        Self {
            name: name.into(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            menu_visible: true,
        }
    }

    /// 标记为探测项:可选中但不进 `1.14` 清单(见 `menu_visible` 注释)。
    pub fn probe(mut self) -> Self {
        self.menu_visible = false;
        self
    }

    /// `pub(crate)`:CLI 形态的首轮 `1.14` 目录复用同一编码(见 `cli.rs`)。
    pub(crate) fn encode(&self) -> Writer {
        let mut w = Writer::new();
        w.string(MODEL_NAME, &self.name);
        for (k, v) in &self.params {
            let mut p = Writer::new();
            p.string(PARAM_KEY, k);
            p.string(PARAM_VAL, v);
            w.message(MODEL_PARAMS, &p);
        }
        w
    }
}

/// 帧0 里哪些可疑分节要发。
///
/// 存在的理由见模块头:文档没做过最小化实验,「必需字段集」是未知的。
/// 出问题时靠关掉分节来二分,而不是重写编码器。
#[derive(Debug, Clone, Copy)]
pub struct RunShape {
    /// `1.1` 整个环境块(时间戳/时区/client kind)。
    pub env_block: bool,
    /// `1.1.5` token 预算分节表。
    pub budget_table: bool,
    /// `1.2.1.1.8` ProseMirror 富文本(与纯文本同内容)。
    pub prosemirror: bool,
    /// `1.14` 可用模型清单。
    pub model_catalog: bool,
    /// `1.2.17` 大上下文块(环境详情 + 系统提示)。
    pub context_block: bool,
}

impl Default for RunShape {
    fn default() -> Self {
        // 目标是**尽量完整模拟客户端**,所以默认全发。这些开关的用途是出问题时二分
        // 定位哪一节不被接受,不是用来长期精简请求 —— 少发字段会让我们的请求在
        // 服务端侧与真 IDE 可区分,那正是要避免的。
        Self {
            env_block: true,
            budget_table: true,
            prosemirror: true,
            model_catalog: true,
            context_block: true,
        }
    }
}

/// 一轮对话消息(已从 Anthropic 请求体抽成纯文本)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub text: String,
    pub is_user: bool,
}

/// 本次请求携带的媒体附件。
///
/// 打成一个结构体而不是两个参数:它们**去处不同**(图片挂消息、文档挂上下文块),
/// 但来源相同(调用方同一条消息里的 content 块),放一起不容易漏传。
#[derive(Debug, Clone, Copy, Default)]
pub struct Media<'a> {
    /// 图片 → `1.2.1.1.3`(挂在**最后一条用户消息**上)。
    pub images: &'a [ImageAttachment],
    /// 文档 → `1.2.1.2.20`(挂在上下文块里)。
    pub docs: &'a [DocAttachment],
}

/// 构造帧0 的 payload(**未** gzip、**未**分帧)。
///
/// `catalog` 是 `1.14` 的可用模型清单 —— 真 IDE 每次 Run 都把整份清单回传
/// (它拿来渲染模型下拉框)。为了与真客户端不可区分,这里也照发,
/// 见 [`crate::models::catalog`]。`now_ms` 可注入以便测试确定性。
pub fn build_frame0(
    turns: &[Turn],
    system: &str,
    tools: &[ToolDef],
    media: Media<'_>,
    model: &Model,
    catalog: &[Model],
    conversation_id: &str,
    timezone: &str,
    now_ms: u64,
    shape: RunShape,
    phase: Phase,
) -> Vec<u8> {
    let mut body = Writer::new();

    let system_chars = system.chars().count();
    let conv_chars: usize = turns.iter().map(|t| t.text.chars().count()).sum();

    // 1.1 环境块。
    //
    // ⚠️ **首轮它是空的**(抓包实物 `1.1 <0B>` —— 字段在、长度 0),后续轮才装
    // 预算表 + 时间戳 + 时区。这不是可有可无的差别:首轮那 98KB 上下文是内联发的,
    // 服务端自己看得见;后续轮上下文在服务端手里,客户端只能**报账**告诉它各节多大。
    if shape.env_block {
        let mut env = Writer::new();
        if phase.is_continuation() {
            if shape.budget_table {
                env.message(ENV_BUDGET, &budget_table(system_chars, conv_chars));
            }
            env.uint(ENV_FLAG10, 1);
            env.string(ENV_CLIENT_KIND, ENV_CLIENT_KIND_VALUE);
            env.uint(ENV_TIMESTAMP_MS, now_ms);
            env.string(ENV_TIMEZONE, timezone);
        }
        body.message(BODY_ENV, &env);
    }

    // 1.2 会话块
    //
    // 上下文块 `1.2.1.2` 挂在**最后一轮**(也就是本次要回答的那条用户消息)上,
    // 与真客户端一致 —— 抓包实物里 `1.2.1` 只有一条,历史由服务端按
    // conversation_id 自己持有。挂到每一轮会把同一块内容重复几十 KB。
    let mut detail = Writer::new();
    if shape.context_block {
        let mut envd = Writer::new();
        envd.string(
            ENV_OS,
            &format!("{} {}", crate::wire::CLIENT_OS, env_os_release()),
        );
        envd.string(ENV_SHELL, "bash");
        envd.string(ENV_TZ, timezone);
        envd.string(ENV_CWD, "/");

        detail.message(DET_ENV, &envd);
        // 系统提示。实测有效:塞一句「只准回答 PINEAPPLE」进去,模型就照做,
        // 且它的思考里把这段称作 hooks_context。不发时服务端用自己那套。
        if !system.is_empty() {
            detail.string(DET_SYSTEM_PROMPT, system);
        }
        // 调用方声明的工具。真包里这一位放的是 MCP 工具,格式逐字对齐(见 [`ToolDef`])。
        // ⚠️ Cursor 的**内建**工具(终端/读文件)不在这里 —— 它们是服务端自带的,
        // 所以即使我们一个工具都不声明,模型照样会去调内建工具。
        for t in tools {
            detail.message(DET_TOOL, &t.encode());
        }
        // 文档(PDF 等)。与图片不同,它们挂在**上下文块**里而不是消息上 —— 抓包实物如此。
        for d in media.docs {
            let mut w = Writer::new();
            w.string(DOC_PATH, &d.path);
            w.string(DOC_CONTENT, &d.text);
            detail.message(DET_DOC, &w);
        }
        detail.string(DET_HOOKS_A, "enabled");
        detail.string(DET_HOOKS_B, "enabled");
        for (f, v) in DET_FLAGS {
            detail.uint(*f, *v);
        }
    }

    // `1.2.1.2` 是出不出字的开关(见 TURN_CONTEXT),而空 `turns` 会让下面的循环
    // 一次都不执行 → 造出的正是那个 445B 的静默挂起形态。invariant 收回函数本地:
    // 这是 `pub fn`,不能只靠 `chat_stream` 那一个调用点偶然守住。
    assert!(
        !turns.is_empty(),
        "build_frame0 需要至少一轮消息,否则会造出不生成的请求"
    );

    let mut conv = Writer::new();
    // 上下文块挂**最后一条用户轮**,不是最后一轮。Anthropic 允许以 assistant 消息
    // 结尾(prefill:`[user, assistant:"答案是"]` 让模型续写),那时最后一轮是助手轮 ——
    // 而抓包实物里这个块永远长在用户轮上。挂错轮若让服务端判不出「上下文声明完毕」,
    // 就重新落回静默挂起,且那是一类没有错误码的失败。
    let last = turns
        .iter()
        .rposition(|t| t.is_user)
        .unwrap_or(turns.len() - 1);
    // 后续轮**只发最后那条用户消息**:历史在服务端手里,重发既浪费又可能被当成新内容。
    // 抓包实物 turn2 的 `1.2.1` 就只有一条(整个请求 2121B,对比首轮 98858B)。
    let send: Vec<(usize, &Turn)> = match phase {
        Phase::Opening => turns.iter().enumerate().collect(),
        Phase::Continuation => vec![(last, &turns[last])],
    };
    for (i, t) in send {
        let mut msg = Writer::new();
        msg.string(MSG_TEXT, &t.text);
        msg.string(MSG_UUID, &uuid::Uuid::new_v4().to_string());
        // 附件容器。这一轮有图就装图,没图发空 —— **不能两个都发**,
        // 否则 `1.2.1.1.3` 出现两次,读侧取到的是空的那个。
        let carries_images = i == last && !media.images.is_empty();
        if !carries_images {
            msg.string(MSG_ATTACH, "");
        }
        msg.uint(
            MSG_KIND,
            if t.is_user {
                MSG_KIND_USER
            } else {
                MSG_KIND_ASSISTANT
            },
        );
        if shape.prosemirror {
            // 只有承载附件的那一条带 mention 节点。
            let mentions: Vec<String> = if i == last {
                media.docs.iter().map(|d| d.path.clone()).collect()
            } else {
                Vec::new()
            };
            msg.string(MSG_RICH, &prosemirror_doc(&t.text, &mentions));
        }
        // 图片挂**最后一条用户消息**(与上下文块同一条)。历史轮里的图我们一并挂到
        // 当前这条 —— 反正 fold_history 已经把历史折成一条了。
        if carries_images {
            for (n, img) in media.images.iter().enumerate() {
                msg.message(MSG_ATTACH, &img.encode(n));
            }
        }
        let mut turn = Writer::new();
        turn.message(TURN_MESSAGE, &msg);
        // ⚠️ 「上下文声明」是出不出字的开关,不是可选装饰(见 TURN_CONTEXT)。
        // 但**它在两种轮次里住在不同地方**:首轮挂轮内 `1.2.1.2`,后续轮改挂会话级
        // `1.2.17`(抓包实物 turn2 的 `1.2.1` 里根本没有 `.2`)。所以这里只在首轮发。
        // ⭐ **两种轮次都挂在这里。** 2026-08-08 实测三个变体(见 PROTOCOL §17):
        //   A 后续轮发 `1.2.17.9`         → 挂起(只剩 10s 心跳)
        //   B 后续轮什么都不发            → 2s 返回但**丢历史**
        //   C 后续轮也挂轮内 `1.2.1.2`    → ✅ 出字 + 记得历史 + 98.7% 缓存命中
        // 早先「后续轮改挂会话级 1.2.17」的判断是错的 —— 那是照抄真客户端的
        // 位置,但真客户端那个块里有 FileSync 上传过的 blob 哈希,我们没有;
        // 半个块(只有 `.9`)比不发更糟,服务端会等一个永远不来的 blob。
        if i == last {
            turn.message(TURN_CONTEXT, &detail);
        }
        conv.message(CONV_TURN, &turn);
    }
    // ⚠️ **永远不发 `1.2.17`**(会话级上下文块)。真客户端后续轮发它,但里面是 4 个
    // 经 FileSyncService 上传的 blob 哈希;我们一个都没上传:
    //   带哈希 → `invalid_argument: Failed to resolve request context blobs`
    //   只发 `.9` 不带哈希 → 服务端静默等一个永不到来的 blob,10s 心跳到超时
    // 声明改挂轮内 `1.2.1.2`(上面那段),两种轮次同一个位置。
    body.message(BODY_CONVERSATION, &conv);

    // 1.4 空 bytes(抓包里恒为 bin[0])
    body.bytes(BODY_EMPTY, &[]);
    // 1.5 conversation_id
    body.string(BODY_CONVERSATION_ID, conversation_id);
    // 1.9 当前模型
    body.message(BODY_MODEL, &model.encode());
    // 1.10 var=0
    body.uint(BODY_FLAG10, 0);
    // 1.25 本轮 turn id。每轮必须换 —— 复用同一个值等于告诉服务端「还是那一轮」。
    body.string(BODY_TURN_ID, &uuid::Uuid::new_v4().to_string());
    // 1.14 可用清单(repeated,每个模型一条;探测项不进清单,见 Model::menu_visible)
    if shape.model_catalog {
        for m in catalog {
            if !m.menu_visible {
                continue;
            }
            body.message(BODY_MODEL_CATALOG, &m.encode());
        }
    }

    let mut frame = Writer::new();
    frame.message(FRAME_BODY, &body);
    frame.into_bytes()
}

/// 上下文窗口上限,抓包实物里 `1.1.5.2` 恒为 256000。
const BUDGET_MAX_TOKENS: u64 = 256_000;

/// 客户端自报的 OS 版本(拼进 `1.2.17.9.4.1`,真值形如 `linux 7.0.0-28-generic`)。
fn env_os_release() -> &'static str {
    crate::wire::CLIENT_OS_VERSION
}

/// 一个 token 预算分节 `{1:key, 2:显示名, 3:tokens, 4:chars}`。
///
/// tokens 用 chars/4 粗估 —— 我们没有 Cursor 的分词器,而这张表是客户端自报的账。
fn budget_section(key: &str, display: &str, chars: usize) -> Writer {
    let mut w = Writer::new();
    w.string(1, key);
    w.string(2, display);
    w.uint(3, (chars as u64).div_ceil(4));
    w.uint(4, chars as u64);
    w
}

/// `1.1.5` token 预算表。
///
/// ⚠️ 结构是**三层**,不是一层:`{1: 合计tokens, 2: 上限, 3: {1: 合计, 2: 上限, 3: [分节…]}}`。
/// 早先按一层写(分节直接挂 `1.1.5`)会让服务端解析崩掉并回 `internal`。
/// 合计必须等于各分节 tokens 之和 —— 抓包实物里
/// `467+9517+3246+5081+2243+1106+0+8891 = 30551` 正好等于 `1.1.5.1`。
///
/// 真客户端报 8 节;反代只有 system_prompt 与 conversation 是真的,其余恒 0 ——
/// **这是有意的**:声明 tools/mcp/subagents 会让模型回工具调用,而我们无法应答
/// (抓包里那条 `cursor-app-control-move_agent_to_root` 就是这么来的)。
///
/// `pub(crate)`:CLI 形态的后续轮预算表复用同一张表(见 `cli.rs`)。
pub(crate) fn budget_table(system_chars: usize, conv_chars: usize) -> Writer {
    let sections: [(&str, &str, usize); 8] = [
        ("system_prompt", "System prompt", system_chars),
        ("tools", "Tool definitions", 0),
        ("rules", "Rules", 0),
        ("skills", "Skills", 0),
        ("mcp", "MCP & dynamic tools", 0),
        ("subagents", "Subagent definitions", 0),
        ("summarized_conversation", "Summarized conversation", 0),
        ("conversation", "Conversation", conv_chars),
    ];
    let total: u64 = sections
        .iter()
        .map(|(_, _, c)| (*c as u64).div_ceil(4))
        .sum();

    let mut inner = Writer::new();
    inner.uint(1, total);
    inner.uint(2, BUDGET_MAX_TOKENS);
    for (k, d, c) in sections {
        inner.message(3, &budget_section(k, d, c));
    }

    let mut outer = Writer::new();
    outer.uint(1, total);
    outer.uint(2, BUDGET_MAX_TOKENS);
    outer.message(3, &inner);
    outer
}

/// 同一句话的富文本形态。真 IDE 对每条用户消息同时发纯文本与 ProseMirror JSON。
fn prosemirror_doc(text: &str, mentions: &[String]) -> String {
    let mut content: Vec<Value> = Vec::new();

    // 文档「提及」节点。**这是模型知道有附件的唯一途径** ——
    // `1.2.1.2.20` 只是内容登记表,靠**路径**连接;真客户端把路径同时写进用户文本
    // 和这个 mentionNode 里。少了它,模型看到的只是一句没有指向的问话,
    // 于是它会说「我来找这个文件」然后无从下手(实测如此)。
    if !mentions.is_empty() {
        let nodes: Vec<Value> = mentions
            .iter()
            .map(|p| {
                let uri = format!("file:file://{}", percent_encode_path(p));
                json!({
                    "type": "mentionNode",
                    "attrs": {
                        "id": uri,
                        "label": p.rsplit('/').next().unwrap_or(p),
                        "mentionSuggestionChar": "@",
                        "uuid": uri,
                        "rawText": p,
                        "plainText": Value::Null,
                        "chipIcon": Value::Null,
                        "mentionTypeRaw": "file",
                        "mentionType": "file",
                        "secondaryText": p.rsplitn(2, '/').nth(1).unwrap_or(""),
                        "payload": {
                            "case": "fileSelection",
                            "uri": { "scheme": "file", "path": p }
                        }
                    }
                })
            })
            .collect();
        content.push(json!({"type":"paragraph","content": nodes}));
    }

    if text.is_empty() {
        if content.is_empty() {
            content.push(json!({"type": "paragraph"}));
        }
    } else {
        for line in text.split('\n') {
            content.push(if line.is_empty() {
                json!({"type": "paragraph"})
            } else {
                json!({"type":"paragraph","content":[{"type":"text","text":line}]})
            });
        }
    }
    json!({"type": "doc", "content": content}).to_string()
}

/// 路径的百分号编码(只转非 ASCII 与空格,`/` 保留)。
///
/// 真客户端的 mentionNode id 里中文路径是 `%E4%B8%8B%E8%BD%BD` 这种形态。
fn percent_encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.as_bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b if b.is_ascii_alphanumeric() => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 帧0 之后紧跟的开场帧序列(逐字照抄抓包实物)。
///
/// ## ⚠️ 它们**不是**生成的必要条件(2026-08-07 更正)
///
/// 这里原先写着「只发帧0 上游就只心跳、永不生成」—— **那个归因是错的**。
/// 从真包做减法证明:只发帧0(丢掉后面全部 26 帧)照样正常出字;真正的开关是帧0 里的
/// `1.2.1.2`(见 [`TURN_CONTEXT`])。当时两件事同时缺,把因果记到了帧上。
///
/// 仍然照发,是因为目标是**模拟一个完整的客户端**:真 IDE 发它们,不发就是一处
/// 可被服务端区分的差异。但别再把它们当成"修复"——删掉不会影响出字。
///
/// 真客户端(3.14.27)紧跟着发的是:
///
/// ```text
/// 帧1  {3: {3: ""}}            field 3,2 字节
/// 帧2  {3: {1: 1, 3: ""}}      field 3,4 字节
/// 帧3  {7: ""}                 field 7,2 字节
/// 帧4  {7: ""}                 field 7,2 字节
/// ```
///
/// 之后真客户端才发 field 2(MCP 工具定义,9.5KB×3)—— **那部分我们故意不发**:
/// 声明了工具,模型就会回工具调用,而文本代理无法应答
/// (抓包里那条 `cursor-app-control-move_agent_to_root` 就是这么来的)。
///
/// 全部是**未压缩**帧(flag=0x00),抓包实物如此 —— 几字节的东西压了反而更大。
pub fn build_prelude_frames() -> Vec<Vec<u8>> {
    // 帧1:{3:{3:""}}
    let mut a_inner = Writer::new();
    a_inner.string(3, "");
    let mut a = Writer::new();
    a.message(FRAME_CONTEXT, &a_inner);

    // 帧2:{3:{1:1, 3:""}}
    let mut b_inner = Writer::new();
    b_inner.uint(1, 1);
    b_inner.string(3, "");
    let mut b = Writer::new();
    b.message(FRAME_CONTEXT, &b_inner);

    // 帧3/帧4:{7:""}
    let mut c = Writer::new();
    c.string(FRAME_READY, "");
    let mut d = Writer::new();
    d.string(FRAME_READY, "");

    vec![
        a.into_bytes(),
        b.into_bytes(),
        c.into_bytes(),
        d.into_bytes(),
    ]
}

/// 上游一帧里可能有的东西。一帧只会命中其中一样,但用一个结构体承载
/// 比三个各扫一遍的函数省两次遍历,也让调用方不可能漏处理某一类。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RespFrame {
    /// `1.1.1` 正文增量。
    pub text: String,
    /// `1.4.1` 思考增量。**不能混进正文** —— 那是模型的推理过程,
    /// 拼进回答里客户会看到「用户要求我…」这种自言自语。
    pub thinking: String,
    /// `1.14` 用量 `(输入, 输出, 缓存命中)`。出现即表示本轮结束。
    /// `1.14`(`InteractionUpdate.turn_ended`)的用量分解。
    ///
    /// 字段号经 2026-08-18 本机取证钉死(钩 `cursor-agent` 的 http2,原始帧落盘):
    /// `1 input_tokens / 2 output_tokens / 3 cache_read_tokens /
    ///  4 cache_write_tokens / 5 reasoning_tokens`。
    /// **field 1 是总量**(含 3 与 4),客户端自己显示时才减 —— 与 CLI `result` 事件
    /// 的口径相反(那边发的是减过的未命中量),见 `clidrv::usage_from_result`。
    ///
    /// `reasoning_tokens` 不入账:实测 `output=34 / reasoning=33`,它是 output 的
    /// **子集**,单独计会重复收费。
    pub usage: Option<WireUsage>,
}

/// 解一帧响应。非 `field 1` 的帧(会话回显 `field 4`、计时 `field 8`)一律返回空。
pub fn parse_frame(payload: &[u8]) -> RespFrame {
    let mut out = RespFrame::default();
    for (field, val) in Reader::new(payload) {
        if field != RESP_MESSAGE {
            continue;
        }
        let PbValue::Len(sub) = val else { continue };
        for (f2, v2) in Reader::new(sub) {
            let PbValue::Len(inner) = v2 else { continue };
            match f2 {
                RESP_TEXT | RESP_THINKING => {
                    for (f3, v3) in Reader::new(inner) {
                        if f3 == RESP_DELTA_TEXT {
                            if let PbValue::Len(b) = v3 {
                                let s = String::from_utf8_lossy(b);
                                if f2 == RESP_TEXT {
                                    out.text.push_str(&s);
                                } else {
                                    out.thinking.push_str(&s);
                                }
                            }
                        }
                    }
                }
                RESP_USAGE => {
                    let mut u = [0u64; 5];
                    let mut seen = 0;
                    for (f3, v3) in Reader::new(inner) {
                        if let (1..=5, PbValue::Varint(n)) = (f3, v3) {
                            u[(f3 - 1) as usize] = n;
                            if f3 <= 3 {
                                seen += 1;
                            }
                        }
                    }
                    // 三个数没齐 = 上游改了 wire type 或增减了字段。仍然收口(不收口是
                    // 永久挂起,更坏),但必须留下痕迹 —— 否则表现是「用量莫名归零」,
                    // 和「本来就没用量」分不开。
                    if seen != 3 {
                        tracing::debug!(seen, "cursor 用量帧字段不全,计费数可能失真");
                    }
                    out.usage = Some(WireUsage {
                        input: u[0],
                        output: u[1],
                        cache_read: u[2],
                        cache_write: u[3],
                    });
                    if u[4] > 0 {
                        tracing::debug!(
                            reasoning = u[4],
                            output = u[1],
                            "cursor 用量帧带推理 token(是 output 的子集,不入账)"
                        );
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// 一帧 payload 的顶层字段号列表(排查用:看清上游到底发了什么种类的帧)。
pub fn top_fields(payload: &[u8]) -> Vec<u32> {
    Reader::new(payload).map(|(f, _)| f).collect()
}

/// `field 1` 里面那一层的字段号(排查用)。
///
/// 顶层几乎恒为 `[1]`,信息量全在内层:`1.1`=正文、`1.4`=思考、`1.8`=状态、
/// `1.14`=用量、`1.17`=计数……而**没被认出来的内层字段号就是「模型在等什么」的线索**。
/// 没有它的话,「200 但卡住」在日志里只是一串 `fields=[1]`,看不出区别。
pub fn inner_fields(payload: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    for (f, v) in Reader::new(payload) {
        if f == RESP_MESSAGE {
            if let PbValue::Len(sub) = v {
                out.extend(Reader::new(sub).map(|(f2, _)| f2));
            }
        }
    }
    out
}

/// 状态帧 `1.8.1` 的取值(实测见过 1/2/3/6/9)。语义未知,但**卡住时它是唯一还在变的东西**。
pub fn status_code(payload: &[u8]) -> Option<u64> {
    for (f, v) in Reader::new(payload) {
        if f != RESP_MESSAGE {
            continue;
        }
        let PbValue::Len(sub) = v else { continue };
        for (f2, v2) in Reader::new(sub) {
            if f2 != 8 {
                continue;
            }
            let PbValue::Len(inner) = v2 else { continue };
            for (f3, v3) in Reader::new(inner) {
                if let (1, PbValue::Varint(n)) = (f3, v3) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// end-stream trailer(`flag & 0x02`)里的错误。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrailerError {
    /// Connect 层 code,如 `resource_exhausted` / `unauthenticated`。
    pub code: String,
    /// Cursor 自己的错误名,如 `ERROR_RATE_LIMITED_CHANGEABLE`。
    pub debug_error: String,
    pub title: String,
    pub detail: String,
    /// 服务端建议自动切换到的模型(计费降级时出现)。
    pub auto_switch_to_model: String,
}

/// 解析 trailer JSON。没有 `error` 字段 → `None`(正常收尾)。
///
/// 结构嵌得很深且各层都可能缺失,所以逐层用 `and_then` 而不是 serde 结构体 ——
/// 另外 `spendLimits` 的值是 JSON **字符串** `"[50,100,200]"` 而不是数组,
/// 用结构体反序列化会直接炸。我们不需要它,索性不碰。
pub fn parse_trailer(json_text: &str) -> Option<TrailerError> {
    let v: Value = serde_json::from_str(json_text).ok()?;
    let err = v.get("error")?;

    let mut out = TrailerError {
        code: str_at(err, &["code"]),
        ..Default::default()
    };

    // error.details[0].debug.{error, details:{title, detail, additionalInfo:{autoSwitchToModel}}}
    let debug = err
        .get("details")
        .and_then(|d| d.as_array())
        .and_then(|a| a.iter().find_map(|d| d.get("debug")));
    if let Some(dbg) = debug {
        out.debug_error = str_at(dbg, &["error"]);
        out.title = str_at(dbg, &["details", "title"]);
        out.detail = str_at(dbg, &["details", "detail"]);
        out.auto_switch_to_model = str_at(dbg, &["details", "additionalInfo", "autoSwitchToModel"]);
    }

    // 整条 trailer 里连一个可辨识字段都没有时,也别返回一个全空的 TrailerError ——
    // 那会让上层报一句没有信息量的错。至少把原始 JSON 截一段带出去。
    if out.code.is_empty() && out.debug_error.is_empty() && out.title.is_empty() {
        out.detail = json_text.chars().take(300).collect();
    }
    Some(out)
}

fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or_default().to_string()
}

impl TrailerError {
    /// 人类可读的一行摘要。
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if !self.code.is_empty() {
            s.push_str(&self.code);
        }
        if !self.debug_error.is_empty() {
            if !s.is_empty() {
                s.push('/');
            }
            s.push_str(&self.debug_error);
        }
        let text = if !self.detail.is_empty() {
            &self.detail
        } else {
            &self.title
        };
        if !text.is_empty() {
            if !s.is_empty() {
                s.push_str(": ");
            }
            s.push_str(text);
        }
        if s.is_empty() {
            s.push_str("上游未给出错误详情");
        }
        s
    }
}

/// 影子模式测试向量:官方 CLI 两轮抓包(2026-08-23)里响应尾部的真实续轮
/// 描述符帧(顶层 `.3`;已去 connect 帧头,flag=0 未压缩)。
/// turn1(新会话)= 4 个 blob 引用;turn2(resume)= 6 个(多出的 2 个是 turn1
/// 产生的新 blob)。chat.rs 的影子状态机测试也复用这份样本。
#[cfg(test)]
pub(crate) mod descriptor_samples {
    const TURN1_HEX: &str = concat!(
        "1a93040a2083ccb7a56d69b4e802e6d3e5bd95f480c74e90509a99c0e7e86199a253420e9d0a209d3e7d77",
        "8fe69cee10f80bc46cc6b488b3e9647c6c685c405bfcdaf853a81b190a200e854779b77b9c42c1cce78cb4",
        "4f5a20a0bdebf3e7bff700f02275e9382909a90a20eba17aceecd9f9f0cc20147089bef4c5e54395096e6d",
        "d791abe4aa0722be1ec62aaf0208938a011080d00f1aa40208938a011080d00f1a240a0d73797374656d5f",
        "70726f6d7074120d53797374656d2070726f6d707418ea0320a20f1a200a05746f6f6c731210546f6f6c",
        "20646566696e6974696f6e7318ea3820eae2011a140a0572756c6573120552756c657318c01420ea511a",
        "170a06736b696c6c731206536b696c6c7318fd2720cf9f011a200a036d637012134d435020262064796e",
        "616d696320746f6f6c73189e0720ee1c1a270a097375626167656e747312145375626167656e74206465",
        "66696e6974696f6e7318df0520f8161a340a1773756d6d6172697a65645f636f6e766572736174696f6e",
        "121753756d6d6172697a656420636f6e766572736174696f6e20001a220a0c636f6e766572736174696f",
        "6e120c436f6e766572736174696f6e18850420931042202079b7f951d09870cf25df5f3fad51d63d0c7d",
        "f18f0382782a0964568a7eacae4a1566696c653a2f2f2f746d702f636c692d70726f62655002b2010363",
        "6c69d001b0cd9fe68234da010d417369612f5368616e67686169",
    );
    const TURN2_HEX: &str = concat!(
        "1af9040a2083ccb7a56d69b4e802e6d3e5bd95f480c74e90509a99c0e7e86199a253420e9d0a209d3e7d77",
        "8fe69cee10f80bc46cc6b488b3e9647c6c685c405bfcdaf853a81b190a200e854779b77b9c42c1cce78cb4",
        "4f5a20a0bdebf3e7bff700f02275e9382909a90a20eba17aceecd9f9f0cc20147089bef4c5e54395096e6d",
        "d791abe4aa0722be1ec60a2049efdd84a6f34b5a5627262f8be867f55519b8fdd912c8d9a09974eec1b18b",
        "bc0a20055fd9c055dde4c3ceaa81df885f49e5a601f8a1339952d96042ecc669e610cb2aaf0208b98b0110",
        "80d00f1aa40208b98b011080d00f1a240a0d73797374656d5f70726f6d7074120d53797374656d2070726f",
        "6d707418ea0320a20f1a200a05746f6f6c731210546f6f6c20646566696e6974696f6e7318ea3820eae201",
        "1a140a0572756c6573120552756c657318c01420ea511a170a06736b696c6c731206536b696c6c7318fd27",
        "20cf9f011a200a036d637012134d435020262064796e616d696320746f6f6c73189e0720ee1c1a270a0973",
        "75626167656e747312145375626167656e7420646566696e6974696f6e7318df0520f8161a340a177375",
        "6d6d6172697a65645f636f6e766572736174696f6e121753756d6d6172697a656420636f6e7665727361",
        "74696f6e20001a220a0c636f6e766572736174696f6e120c436f6e766572736174696f6e18ab0520a513",
        "42202079b7f951d09870cf25df5f3fad51d63d0c7df18f0382782a0964568a7eacae42208ef11f326b346f",
        "31a60f7404fc4e2fae4e3e43335adeada5207242c3e5d0f34a4a1566696c653a2f2f2f746d702f636c69",
        "2d70726f62655002b20103636c69d001b0cd9fe68234da010d417369612f5368616e67686169",
    );

    pub(crate) fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("样本必须是合法 hex"))
            .collect()
    }

    /// turn1(新会话)响应尾部的描述符帧 payload:4 个 blob 引用。
    pub(crate) fn turn1() -> Vec<u8> {
        hex_to_bytes(TURN1_HEX)
    }

    /// turn2(resume)响应尾部的描述符帧 payload:6 个 blob 引用。
    pub(crate) fn turn2() -> Vec<u8> {
        hex_to_bytes(TURN2_HEX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 会话登记通知(CLI 形态)必须被认出,且不能误吞真正的 exec 请求。
    #[test]
    fn session_notice_recognized_and_exec_untouched() {
        // 抓包实物同构:{2: {10: {2: conv}, 19: {1: trace, 2: span, 3: 0}, 55: 0}}
        let mut rc = Writer::new();
        rc.string(2, "9ea80b89-54e0-4273-a369-da13d190177c");
        let mut span = Writer::new();
        span.string(1, "3c0527122e953ea293870b02f1176698");
        span.string(2, "19ed9930a989daae");
        span.uint(3, 0);
        let mut ch = Writer::new();
        ch.message(10, &rc);
        ch.message(19, &span);
        ch.uint(55, 0);
        let mut notice = Writer::new();
        notice.message(2, &ch);
        assert!(is_session_notice(&notice.into_bytes()));

        // 带可执行负载(shell=field 2)的 exec 帧绝不能被判成通知。
        let mut shell = Writer::new();
        shell.string(1, "ls -la");
        let mut ch2 = Writer::new();
        ch2.message(2, &shell);
        ch2.uint(55, 0);
        let mut exec = Writer::new();
        exec.message(2, &ch2);
        assert!(!is_session_notice(&exec.into_bytes()));
    }

    /// 测试包装:绝大多数用例不关心 `1.14` 清单,统一传空。
    fn f0(
        turns: &[Turn],
        model: &Model,
        conversation_id: &str,
        timezone: &str,
        now_ms: u64,
        shape: RunShape,
        phase: Phase,
    ) -> Vec<u8> {
        build_frame0(
            turns,
            "",
            &[],
            Media::default(),
            model,
            &[],
            conversation_id,
            timezone,
            now_ms,
            shape,
            phase,
        )
    }

    /// 按字段号路径取一个 length-delimited 子消息。
    fn dig<'a>(buf: &'a [u8], path: &[u32]) -> Option<&'a [u8]> {
        let mut cur = buf;
        for want in path {
            let mut found = None;
            for (f, v) in Reader::new(cur) {
                if f == *want {
                    if let PbValue::Len(s) = v {
                        found = Some(s);
                        break;
                    }
                }
            }
            cur = found?;
        }
        Some(cur)
    }

    fn string_at(buf: &[u8], field: u32) -> Option<String> {
        Reader::new(buf).find_map(|(f, v)| match (f, v) {
            (ff, PbValue::Len(s)) if ff == field => Some(String::from_utf8_lossy(s).to_string()),
            _ => None,
        })
    }

    fn varint_at(buf: &[u8], field: u32) -> Option<u64> {
        Reader::new(buf).find_map(|(f, v)| match (f, v) {
            (ff, PbValue::Varint(n)) if ff == field => Some(n),
            _ => None,
        })
    }

    fn one_user(text: &str) -> Vec<Turn> {
        vec![Turn {
            text: text.into(),
            is_user: true,
        }]
    }

    #[test]
    fn body_lives_under_frame_field_1_not_at_top_level() {
        // 这条测试守的是模块头说的那跤:主体必须裹在帧 payload 的 field 1 里。
        // 若有人把 1.9 当顶层 field 9 写,这条会红。
        let bytes = f0(
            &one_user("hi"),
            &Model::new("grok-4.5"),
            "conv-1",
            "Asia/Shanghai",
            1_700_000_000_000,
            RunShape::default(),
            Phase::Opening,
        );
        let mut top = Reader::new(&bytes);
        let (f, _) = top.next().unwrap();
        assert_eq!(f, FRAME_BODY, "帧 payload 顶层只应有 field 1");
        assert!(top.next().is_none(), "顶层不应有第二个字段");

        // 模型在 body 的 field 9,不在顶层
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        let model = dig(body, &[BODY_MODEL]).unwrap();
        assert_eq!(string_at(model, MODEL_NAME).as_deref(), Some("grok-4.5"));
        assert!(
            dig(&bytes, &[BODY_MODEL]).is_none() || dig(&bytes, &[BODY_MODEL]).unwrap() != model,
            "顶层 field 9 不应直接是模型块"
        );
    }

    #[test]
    fn encodes_conversation_id_and_env() {
        let bytes = f0(
            &one_user("hi"),
            &Model::new("default"),
            "conv-xyz",
            "Asia/Shanghai",
            1_700_000_000_123,
            RunShape::default(),
            Phase::Continuation,
        );
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        assert_eq!(
            string_at(body, BODY_CONVERSATION_ID).as_deref(),
            Some("conv-xyz")
        );
        assert_eq!(varint_at(body, BODY_FLAG10), Some(0));

        let env = dig(body, &[BODY_ENV]).unwrap();
        assert_eq!(varint_at(env, ENV_TIMESTAMP_MS), Some(1_700_000_000_123));
        assert_eq!(
            string_at(env, ENV_TIMEZONE).as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(varint_at(env, ENV_FLAG10), Some(1));
    }

    #[test]
    fn env_client_kind_stays_ide_not_glass() {
        // 抓包实物:头是 glass,体内是 ide。防止有人「统一」掉。
        let bytes = f0(
            &one_user("hi"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Continuation,
        );
        let env = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
        assert_eq!(string_at(env, ENV_CLIENT_KIND).as_deref(), Some("ide"));
        assert_eq!(crate::wire::CLIENT_TYPE, "glass", "头部才是 glass");
    }

    #[test]
    fn message_carries_text_kind_and_rich_form() {
        let turns = vec![
            Turn {
                text: "问题".into(),
                is_user: true,
            },
            Turn {
                text: "回答".into(),
                is_user: false,
            },
        ];
        let bytes = f0(
            &turns,
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        let conv = dig(body, &[BODY_CONVERSATION]).unwrap();

        // 两轮各一个 CONV_TURN
        let turns_seen: Vec<&[u8]> = Reader::new(conv)
            .filter_map(|(f, v)| match (f, v) {
                (CONV_TURN, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(turns_seen.len(), 2);

        let m0 = dig(turns_seen[0], &[TURN_MESSAGE]).unwrap();
        assert_eq!(string_at(m0, MSG_TEXT).as_deref(), Some("问题"));
        assert_eq!(varint_at(m0, MSG_KIND), Some(MSG_KIND_USER));
        let rich = string_at(m0, MSG_RICH).unwrap();
        let doc: Value = serde_json::from_str(&rich).unwrap();
        assert_eq!(doc["type"], "doc");
        assert_eq!(doc["content"][0]["content"][0]["text"], "问题");
        // uuid 每条独立
        assert_ne!(
            string_at(m0, MSG_UUID),
            string_at(dig(turns_seen[1], &[TURN_MESSAGE]).unwrap(), MSG_UUID)
        );

        let m1 = dig(turns_seen[1], &[TURN_MESSAGE]).unwrap();
        assert_eq!(varint_at(m1, MSG_KIND), Some(MSG_KIND_ASSISTANT));
    }

    #[test]
    fn model_params_encode_as_key_value_pairs() {
        let m = Model::with_params("grok-4.5", &[("effort", "high"), ("fast", "false")]);
        let bytes = f0(
            &one_user("x"),
            &m,
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let model = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_MODEL]).unwrap();
        assert_eq!(string_at(model, MODEL_NAME).as_deref(), Some("grok-4.5"));

        let pairs: Vec<(String, String)> = Reader::new(model)
            .filter_map(|(f, v)| match (f, v) {
                (MODEL_PARAMS, PbValue::Len(s)) => Some((
                    string_at(s, PARAM_KEY).unwrap_or_default(),
                    string_at(s, PARAM_VAL).unwrap_or_default(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("effort".to_string(), "high".to_string()),
                ("fast".to_string(), "false".to_string())
            ]
        );
    }

    #[test]
    fn catalog_emits_one_repeated_entry_per_model() {
        // 真 IDE 每次 Run 都回传整份可用模型清单;为了不可区分,我们也发。
        let catalog = vec![
            Model::new("default"),
            Model::with_params("grok-4.5", &[("effort", "high")]),
        ];
        let bytes = build_frame0(
            &one_user("hi"),
            "",
            &[],
            Media::default(),
            &Model::new("default"),
            &catalog,
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        let listed: Vec<String> = Reader::new(body)
            .filter_map(|(f, v)| match (f, v) {
                (BODY_MODEL_CATALOG, PbValue::Len(s)) => string_at(s, MODEL_NAME),
                _ => None,
            })
            .collect();
        assert_eq!(listed, vec!["default", "grok-4.5"]);

        // 关掉开关就一条都不发
        let off = RunShape {
            model_catalog: false,
            ..RunShape::default()
        };
        let b2 = build_frame0(
            &one_user("hi"),
            "",
            &[],
            Media::default(),
            &Model::new("default"),
            &catalog,
            "c",
            "UTC",
            1,
            off,
            Phase::Opening,
        );
        assert!(dig(&b2, &[FRAME_BODY, BODY_MODEL_CATALOG]).is_none());
    }

    #[test]
    fn probe_models_stay_out_of_the_menu() {
        // 探测项(menu_visible=false)可被选中为当前模型,但不进 1.14 清单 ——
        // 未证实的条目不污染所有模型的请求(审查 gpt-5.6-sol 高危)。
        let catalog = vec![
            Model::new("default"),
            Model::with_params("grok-4.6", &[("effort", "high")]).probe(),
        ];
        let bytes = build_frame0(
            &one_user("hi"),
            "",
            &[],
            Media::default(),
            &Model::with_params("grok-4.6", &[("effort", "high")]).probe(),
            &catalog,
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        let listed: Vec<String> = Reader::new(body)
            .filter_map(|(f, v)| match (f, v) {
                (BODY_MODEL_CATALOG, PbValue::Len(s)) => string_at(s, MODEL_NAME),
                _ => None,
            })
            .collect();
        assert_eq!(listed, vec!["default"], "探测项不该进 1.14 清单");
        // 但 1.9 当前模型确实是 grok-4.6 —— 探测流量真的打到它身上。
        let cur = dig(body, &[BODY_MODEL]).unwrap();
        assert_eq!(string_at(cur, MODEL_NAME).as_deref(), Some("grok-4.6"));
    }

    #[test]
    fn shape_toggles_actually_drop_sections() {
        let off = RunShape {
            env_block: false,
            budget_table: false,
            prosemirror: false,
            model_catalog: false,
            context_block: false,
        };
        let bytes = f0(
            &one_user("hi"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            off,
            Phase::Continuation,
        );
        let body = dig(&bytes, &[FRAME_BODY]).unwrap();
        assert!(
            dig(body, &[BODY_ENV]).is_none(),
            "env_block=false 应整块不发"
        );
        let m = dig(
            dig(body, &[BODY_CONVERSATION]).unwrap(),
            &[CONV_TURN, TURN_MESSAGE],
        )
        .unwrap();
        assert!(
            string_at(m, MSG_RICH).is_none(),
            "prosemirror=false 应不发富文本"
        );
        // 会话与模型是骨架,任何 shape 下都要在
        assert_eq!(string_at(m, MSG_TEXT).as_deref(), Some("hi"));
        assert!(dig(body, &[BODY_MODEL]).is_some());

        // 预算表单独关
        let no_budget = RunShape {
            budget_table: false,
            ..RunShape::default()
        };
        let b2 = f0(
            &one_user("hi"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            no_budget,
            Phase::Continuation,
        );
        let env2 = dig(dig(&b2, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
        assert!(dig(env2, &[ENV_BUDGET]).is_none());
        // 但环境块其余字段还在
        assert_eq!(string_at(env2, ENV_TIMEZONE).as_deref(), Some("UTC"));
    }

    #[test]
    fn budget_table_reports_real_char_count() {
        let turns = vec![Turn {
            text: "12345678".into(),
            is_user: true,
        }];
        let bytes = f0(
            &turns,
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Continuation,
        );
        let env = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
        // 三层:1.1.5{1:合计,2:上限,3:{1:合计,2:上限,3:[分节…]}}
        let outer = dig(env, &[ENV_BUDGET]).unwrap();
        assert_eq!(
            varint_at(outer, 2),
            Some(BUDGET_MAX_TOKENS),
            "上限恒为 256000"
        );
        let inner = dig(outer, &[3]).unwrap();
        assert_eq!(varint_at(inner, 2), Some(BUDGET_MAX_TOKENS));
        let secs: Vec<&[u8]> = Reader::new(inner)
            .filter_map(|(f, v)| match (f, v) {
                (3, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(secs.len(), 8, "真客户端报 8 节");
        let conv = secs
            .iter()
            .find(|s| string_at(s, 1).as_deref() == Some("conversation"))
            .unwrap();
        assert_eq!(varint_at(conv, 4), Some(8), "chars 必须是真实字符数");
        assert_eq!(varint_at(conv, 3), Some(2), "tokens = ceil(chars/4)");
        // 合计 = 各节之和(抓包实物这条恒成立)
        let sum: u64 = secs.iter().map(|s| varint_at(s, 3).unwrap_or(0)).sum();
        assert_eq!(varint_at(outer, 1), Some(sum));
        assert_eq!(varint_at(inner, 1), Some(sum));
        // 工具/mcp/subagents 必须报 0 —— 声明了就会收到无法应答的工具调用
        for k in ["tools", "mcp", "subagents", "rules", "skills"] {
            let sec = secs
                .iter()
                .find(|s| string_at(s, 1).as_deref() == Some(k))
                .unwrap();
            assert_eq!(varint_at(sec, 3).unwrap_or(0), 0, "{k} 必须报 0");
        }
    }

    #[test]
    fn prosemirror_splits_paragraphs_and_escapes() {
        let doc: Value = serde_json::from_str(&prosemirror_doc("a\n\nb\"q\"", &[])).unwrap();
        let c = doc["content"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0]["content"][0]["text"], "a");
        assert!(c[1].get("content").is_none(), "空行是无内容的段落");
        assert_eq!(c[2]["content"][0]["text"], "b\"q\"");
    }

    /// 文档必须在 ProseMirror 里有 `mentionNode`,否则模型不知道附件存在。
    #[test]
    fn 文档在富文本里带提及节点() {
        let s = prosemirror_doc("讲什么", &["/tmp/gw-cursor/doc-0.pdf".to_string()]);
        let doc: Value = serde_json::from_str(&s).unwrap();
        let node = &doc["content"][0]["content"][0];
        assert_eq!(node["type"], "mentionNode");
        assert_eq!(node["attrs"]["mentionType"], "file");
        assert_eq!(node["attrs"]["label"], "doc-0.pdf");
        assert_eq!(node["attrs"]["rawText"], "/tmp/gw-cursor/doc-0.pdf");
        assert_eq!(
            node["attrs"]["payload"]["uri"]["path"],
            "/tmp/gw-cursor/doc-0.pdf"
        );
        // id/uuid 是 `file:file://<百分号编码路径>`,与真客户端同形。
        assert_eq!(node["attrs"]["id"], "file:file:///tmp/gw-cursor/doc-0.pdf");
        // 文本仍在后面的段落里。
        assert_eq!(doc["content"][1]["content"][0]["text"], "讲什么");
    }

    #[test]
    fn 路径百分号编码只转非ascii() {
        assert_eq!(percent_encode_path("/tmp/a-b_c.pdf"), "/tmp/a-b_c.pdf");
        // 中文按 UTF-8 逐字节转,与真客户端的 %E4%B8%8B%E8%BD%BD 同形。
        assert_eq!(
            percent_encode_path("/下载/x.pdf"),
            "/%E4%B8%8B%E8%BD%BD/x.pdf"
        );
        assert_eq!(percent_encode_path("/a b"), "/a%20b");
    }

    #[test]
    fn prelude_frames_match_captured_client_exactly() {
        // 逐字对齐真客户端。⚠️ 这四帧**不影响生成**(只发帧0 也出字),
        // 守的是「与真 IDE 不可区分」,不是「能不能出字」——别再把因果记反。
        let f = build_prelude_frames();
        assert_eq!(f.len(), 4);

        // 帧1 {3:{3:""}}
        let i0 = dig(&f[0], &[FRAME_CONTEXT]).unwrap();
        assert_eq!(string_at(i0, 3).as_deref(), Some(""));
        assert!(varint_at(i0, 1).is_none(), "帧1 没有 3.1");

        // 帧2 {3:{1:1,3:""}}
        let i1 = dig(&f[1], &[FRAME_CONTEXT]).unwrap();
        assert_eq!(varint_at(i1, 1), Some(1));
        assert_eq!(string_at(i1, 3).as_deref(), Some(""));

        // 帧3/帧4 {7:""}
        for k in [2usize, 3] {
            assert_eq!(string_at(&f[k], FRAME_READY).as_deref(), Some(""));
        }
        // 与抓包实物的帧 payload 字节数逐一对齐(解压后 4 / 6 / 2 / 2)。
        // 这条是「逐字照抄」的硬证据 —— 差一个字节就说明结构又不一样了。
        assert_eq!(
            f.iter().map(|x| x.len()).collect::<Vec<_>>(),
            vec![4, 6, 2, 2]
        );
    }

    /// 造一帧 `1.<slot>.{1: 文本, 2: 1}`。
    fn delta_frame(slot: u32, text: &str) -> Vec<u8> {
        let mut delta = Writer::new();
        delta.string(RESP_DELTA_TEXT, text);
        delta.uint(2, 1);
        let mut msg = Writer::new();
        msg.message(slot, &delta);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        outer.into_bytes()
    }

    #[test]
    fn 文本增量是三层不是两层() {
        // 抓包实物:1 -> 1 -> {1: 文本, 2: 1}。少钻一层会把 message 的原始字节当文本。
        assert_eq!(parse_frame(&delta_frame(RESP_TEXT, "你好")).text, "你好");
    }

    /// **正文在 `1.1.1`,思考在 `1.4.1`,两者结构完全相同。**
    ///
    /// 这条守的是一个不会报错的坏法:解错字段号时请求成功、也有字出来,
    /// 但出来的是模型的推理过程,而它的真实回答一个字都收不到。
    /// 实测「只回答 PINEAPPLE」那次:`1.4.1`='用户要求…',`1.1.1`='P'/'INE'/'APPLE'。
    #[test]
    fn 思考流绝不能混进正文() {
        let think = parse_frame(&delta_frame(RESP_THINKING, "用户要求我只回答一个词"));
        assert_eq!(think.text, "", "1.4 是思考,一个字都不许进正文");
        assert_eq!(think.thinking, "用户要求我只回答一个词");

        let body = parse_frame(&delta_frame(RESP_TEXT, "PINEAPPLE"));
        assert_eq!(body.text, "PINEAPPLE");
        assert_eq!(body.thinking, "");
    }

    /// 用量帧既是计费数据,也是**本轮唯一的收尾信号** —— BiDi 流不会自己关。
    #[test]
    fn 用量帧解出三个数且是收尾信号() {
        let mut u = Writer::new();
        u.uint(1, 12106);
        u.uint(2, 74);
        u.uint(3, 11904);
        let mut msg = Writer::new();
        msg.message(RESP_USAGE, &u);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        let fr = parse_frame(&outer.into_bytes());
        assert_eq!(
            fr.usage,
            Some(WireUsage {
                input: 12106,
                output: 74,
                cache_read: 11904,
                cache_write: 0
            })
        );
        // 普通文本帧不能被误判成收尾,否则第一个字出来就把流关了。
        assert_eq!(parse_frame(&delta_frame(RESP_TEXT, "x")).usage, None);
    }

    /// 心跳帧与会话回显都不该被当成任何东西。
    ///
    /// ⚠️ 真心跳是 **4 字节的 `field 1` 帧**,而 `field 1` 正是解析器唯一下钻的顶层字段 ——
    /// 也就是说心跳和正文走**同一条**解析路径,这才是最该钉住的那条。
    /// 只构造 `field 4` 回显是在考一条根本不会走错的路。
    #[test]
    fn 心跳与会话回显不产生文本也不收尾() {
        // ① 真心跳:field 1 里裹一个空串字段(§10.6 的 1.13)。
        let mut hb_inner = Writer::new();
        hb_inner.string(13, "");
        let mut hb = Writer::new();
        hb.message(RESP_MESSAGE, &hb_inner);
        assert_eq!(
            parse_frame(&hb.into_bytes()),
            RespFrame::default(),
            "心跳不许产出任何东西"
        );

        // ② 状态帧 1.8.1 也走 field 1,同样不该产出。
        let mut st_inner = Writer::new();
        st_inner.uint(1, 9);
        let mut st_msg = Writer::new();
        st_msg.message(8, &st_inner);
        let mut st = Writer::new();
        st.message(RESP_MESSAGE, &st_msg);
        assert_eq!(parse_frame(&st.into_bytes()), RespFrame::default());

        // ③ 会话回显走 field 4,连下钻都不该下。
        let mut echo = Writer::new();
        echo.string(1, "conversation-echo");
        let mut outer = Writer::new();
        outer.message(4, &echo);
        assert_eq!(parse_frame(&outer.into_bytes()), RespFrame::default());
    }

    /// **prefill**:Anthropic 允许以 assistant 消息结尾让模型续写。
    /// 那时最后一轮是助手轮,而上下文块在抓包实物里永远长在**用户轮**上。
    #[test]
    fn prefill时上下文块挂在最后一条用户轮上() {
        let turns = vec![
            Turn {
                text: "问题".into(),
                is_user: true,
            },
            Turn {
                text: "答案是".into(),
                is_user: false,
            },
        ];
        let bytes = build_frame0(
            &turns,
            "sys",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
        let seen: Vec<&[u8]> = Reader::new(conv)
            .filter_map(|(f, v)| match (f, v) {
                (CONV_TURN, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(seen.len(), 2);
        assert!(dig(seen[0], &[TURN_CONTEXT]).is_some(), "必须挂在用户轮");
        assert!(
            dig(seen[1], &[TURN_CONTEXT]).is_none(),
            "助手轮(prefill)不该带上下文块"
        );
    }

    /// 同一帧里正文与用量并存时,正文**不能**被丢。
    /// 「一帧只装一样」只是抓包观察,protobuf 层没有任何东西保证它。
    #[test]
    fn 正文与用量同帧时两者都要解出来() {
        let mut txt = Writer::new();
        txt.string(RESP_DELTA_TEXT, "APPLE");
        txt.uint(2, 1);
        let mut u = Writer::new();
        u.uint(1, 10);
        u.uint(2, 2);
        u.uint(3, 0);
        let mut msg = Writer::new();
        msg.message(RESP_TEXT, &txt);
        msg.message(RESP_USAGE, &u);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        let fr = parse_frame(&outer.into_bytes());
        assert_eq!(fr.text, "APPLE", "收口帧里的正文必须照样解出来");
        assert_eq!(
            fr.usage,
            Some(WireUsage {
                input: 10,
                output: 2,
                cache_read: 0,
                cache_write: 0
            })
        );
    }

    #[should_panic(expected = "至少一轮消息")]
    #[test]
    fn 空消息列表当场panic而不是造出不生成的请求() {
        build_frame0(
            &[],
            "",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
    }

    #[test]
    fn system_prompt_goes_to_context_block_not_user_turn() {
        let bytes = build_frame0(
            &one_user("问题"),
            "你是助手",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
        let det = dig(conv, &[CONV_TURN, TURN_CONTEXT]).unwrap();
        assert_eq!(
            string_at(det, DET_SYSTEM_PROMPT).as_deref(),
            Some("你是助手")
        );
        // 用户消息里**不能**混进系统提示
        let m = dig(conv, &[CONV_TURN, TURN_MESSAGE]).unwrap();
        assert_eq!(string_at(m, MSG_TEXT).as_deref(), Some("问题"));
        // 环境详情带 os/shell/时区
        let envd = dig(det, &[DET_ENV]).unwrap();
        assert!(string_at(envd, ENV_OS).unwrap().starts_with("linux "));
        assert_eq!(string_at(envd, ENV_TZ).as_deref(), Some("UTC"));
        // 那串开关要逐一在场
        for (f, v) in DET_FLAGS {
            assert_eq!(varint_at(det, *f), Some(*v), "9.{f} 应为 {v}");
        }
        assert_eq!(string_at(det, DET_HOOKS_A).as_deref(), Some("enabled"));
        // 空 system 时不发 .25
        let b2 = build_frame0(
            &one_user("q"),
            "",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let det2 = dig(
            &b2,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
        )
        .unwrap();
        assert!(string_at(det2, DET_SYSTEM_PROMPT).is_none());
    }

    /// **上下文块必须在场,哪怕是空的** —— 这条守的是本 crate 卡了整轮的那个 bug。
    ///
    /// 2026-08-07 从真包削出来的对照:两个请求只差这一个长度为 0 的字段
    /// (445B vs 446B),没有它上游回 200、回一帧会话回显,然后每 10 秒一个
    /// 4 字节心跳,**永不出字**;有它立刻正常生成。既没有错误码也没有超时,
    /// 单看返回值永远查不出来。
    ///
    /// 所以它不能挂在 `RunShape.context_block` 下面 —— 那个开关只管里面装什么。
    #[test]
    fn 上下文块即使为空也必须发出否则上游静默挂起() {
        let shape = RunShape {
            context_block: false,
            ..RunShape::default()
        };
        let bytes = build_frame0(
            &one_user("q"),
            "系统提示",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            shape,
            Phase::Opening,
        );
        let det = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
        )
        .expect("1.2.1.2 必须在场,否则服务端接受请求但永远不生成");
        assert!(
            det.is_empty(),
            "关掉 context_block 时里面应为空,但字段本身要在"
        );
    }

    /// **首轮 `1.1` 必须是空的**(字段在、长度 0),后续轮才装预算表。
    ///
    /// 抓包实测:首轮 98858B / 后续轮 2121B。首轮那 98KB 上下文是内联发的、
    /// 服务端自己看得见;后续轮上下文在服务端手里,客户端只能报账告诉它各节多大。
    #[test]
    fn 首轮环境块为空后续轮才报账() {
        let open = f0(
            &one_user("q"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let env = dig(&open, &[FRAME_BODY, BODY_ENV]).expect("字段本身要在");
        assert!(env.is_empty(), "首轮 1.1 必须是空的,实际 {}B", env.len());

        let cont = f0(
            &one_user("q"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Continuation,
        );
        let env = dig(&cont, &[FRAME_BODY, BODY_ENV]).unwrap();
        assert!(!env.is_empty(), "后续轮 1.1 要带预算表");
        assert_eq!(string_at(env, ENV_TIMEZONE).as_deref(), Some("UTC"));
    }

    /// 后续轮**只发最后那条用户消息**,上下文声明从 `1.2.1.2` 挪到 `1.2.17`。
    ///
    /// 历史在服务端手里(按 `1.5` 持有)。重发历史既浪费也可能被当成新内容 ——
    /// 抓包实物 turn2 的 `1.2.1` 就只有一条,且里面**没有** `.2`。
    #[test]
    fn 后续轮只发新消息但上下文声明仍挂轮内() {
        let turns = vec![
            Turn {
                text: "第一问".into(),
                is_user: true,
            },
            Turn {
                text: "第一答".into(),
                is_user: false,
            },
            Turn {
                text: "第二问".into(),
                is_user: true,
            },
        ];
        let bytes = build_frame0(
            &turns,
            "sys",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Continuation,
        );
        let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
        let seen: Vec<&[u8]> = Reader::new(conv)
            .filter_map(|(f, v)| match (f, v) {
                (CONV_TURN, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(seen.len(), 1, "后续轮只该发一条消息,实际 {}", seen.len());
        let m = dig(seen[0], &[TURN_MESSAGE]).unwrap();
        assert_eq!(
            string_at(m, MSG_TEXT).as_deref(),
            Some("第二问"),
            "发的必须是最新那条"
        );
        // ⭐ 上下文声明**仍在轮内** 1.2.1.2 —— 这是 2026-08-08 实测出来的唯一可用形态
        // (挪到会话级 1.2.17 会让上游静默挂起,见 PROTOCOL §17)。
        let det = dig(seen[0], &[TURN_CONTEXT]).expect("后续轮轮内必须有 1.2.1.2");
        assert_eq!(string_at(det, DET_SYSTEM_PROMPT).as_deref(), Some("sys"));

        // 且**绝不发** 1.2.17:那个块要求 FileSync 上传过的 blob 哈希。
        assert!(
            dig(conv, &[CONV_CONTEXT]).is_none(),
            "1.2.17 一旦出现,服务端会等一个我们永远不会上传的 blob"
        );
    }

    /// 多轮时上下文块只挂**最后一轮**:与真客户端一致,也避免几十 KB 重复。
    #[test]
    fn 多轮对话只有最后一轮带上下文块() {
        let turns = vec![
            Turn {
                text: "第一问".into(),
                is_user: true,
            },
            Turn {
                text: "第一答".into(),
                is_user: false,
            },
            Turn {
                text: "第二问".into(),
                is_user: true,
            },
        ];
        let bytes = build_frame0(
            &turns,
            "sys",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
        let seen: Vec<&[u8]> = Reader::new(conv)
            .filter_map(|(f, v)| match (f, v) {
                (CONV_TURN, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(seen.len(), 3);
        assert!(
            dig(seen[0], &[TURN_CONTEXT]).is_none(),
            "前面几轮不该重复上下文块"
        );
        assert!(dig(seen[1], &[TURN_CONTEXT]).is_none());
        let det = dig(seen[2], &[TURN_CONTEXT]).expect("最后一轮必须带");
        assert_eq!(string_at(det, DET_SYSTEM_PROMPT).as_deref(), Some("sys"));
    }

    /// 造一帧真实形态的外部工具调用。
    ///
    /// ⚠️ 这个 helper 用的是与 `parse_tool_call` **同一套常量**,所以它只能证明
    /// 「编码解码互逆」,证明不了「与真上游一致」——字段号整套写错也照样绿。
    /// 真正的锚点是 `tests/fixtures/*.bin`(抓包实物),见
    /// [`解析抓包实物里的外部工具调用帧`]。本 helper 只用来覆盖那些实物里没有的组合
    /// (数值参数、多参数)。
    fn tool_call_frame(bare: &str, call_id: &str, args: &[(&str, Value)]) -> Vec<u8> {
        let mut inner = Writer::new();
        inner.string(TOOL_FULL_NAME, &format!("{TOOL_NS}-{bare}"));
        for (k, v) in args {
            let mut wrap = Writer::new();
            match v {
                Value::String(sv) => wrap.string(PBV_STRING, sv),
                Value::Number(n) => wrap.double(PBV_NUMBER, n.as_f64().unwrap()),
                Value::Bool(b) => wrap.uint(PBV_BOOL, u64::from(*b)),
                Value::Null => wrap.uint(PBV_NULL, 0),
                other => panic!("helper 不支持 {other:?},请手写字节"),
            }
            let mut kv = Writer::new();
            kv.string(TC_ARG_KEY, k);
            kv.message(TC_ARG_VAL, &wrap);
            inner.message(TC_ARGS, &kv);
        }
        inner.string(3, call_id);
        inner.string(4, TOOL_NS);
        inner.string(TC_BARE_NAME, bare);
        let mut ext = Writer::new();
        ext.message(TC_INNER, &inner);
        let mut detail = Writer::new();
        detail.message(TC_EXTERNAL, &ext);
        detail.string(57, call_id);
        let mut ch = Writer::new();
        ch.string(TC_CALL_ID, call_id);
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        outer.into_bytes()
    }

    #[test]
    fn 解出外部工具调用的名字与参数() {
        let f = tool_call_frame(
            "get_weather",
            "call-abc-0\nfc_def_0",
            &[("city", json!("北京"))],
        );
        assert!(is_tool_call(&f));
        let tc = parse_tool_call(&f).expect("应解出工具调用");
        // 名字用**裸名**,与调用方 Anthropic 请求里的 name 对得上(不带命名空间前缀)。
        assert_eq!(tc.name, "get_weather");
        // call id 必须原样带回,重新生成就对不上了。
        assert_eq!(tc.id, "call-abc-0\nfc_def_0");
        assert_eq!(tc.args, vec![("city".to_string(), json!("北京"))]);
    }

    /// ⭐ **锚在抓包实物上**:直接解 2026-08-07 落盘的那一帧。
    ///
    /// 这条与 `解出外部工具调用的名字与参数` 的区别是它**不经过我方 Writer** ——
    /// 字段号(`1.2.2.15.1` 那一串)若记错,只有这条会红。
    #[test]
    fn 解析抓包实物里的外部工具调用帧() {
        let raw = include_bytes!("../tests/fixtures/tool_call_external_real.bin");
        assert!(is_tool_call(raw));
        let tc = parse_tool_call(raw).expect("实物帧必须解得出来");
        assert_eq!(tc.name, "get_weather");
        assert!(tc.id.starts_with("call-d167f527-"), "call id: {}", tc.id);
        assert_eq!(tc.args, vec![("city".to_string(), json!("北京"))]);
    }

    /// ⭐ 同上,内建工具的实物帧(`1.2.2.4` = 读文件/glob)**必须认不出来**。
    #[test]
    fn 抓包实物里的内建工具帧不被当成外部工具() {
        let raw = include_bytes!("../tests/fixtures/tool_call_builtin_real.bin");
        assert!(is_tool_call(raw), "是工具帧");
        assert!(
            parse_tool_call(raw).is_none(),
            "内建工具没有名字,认成外部工具会给调用方一个它没有的工具"
        );
    }

    /// ⭐ **锚在 2026-08-10 的抓包实物上**:带图请求的资产落盘调用
    /// (`exec_server_message.3` = write_args,字段号经 opencodex 的
    /// `agent.v1` schema 交叉核对)。
    ///
    /// 这是「带图请求全模型 502」的直接病因:这一帧走 exec 通道,被误判成
    /// 内建工具调用在出字前收口;而只跳过不回执,服务端又 90s 心跳死等。
    /// 它必须被解成**写盘调用**(存内存 + 回执),既不是外部工具也不是跳过对象。
    #[test]
    fn 抓包实物里的资产写调用被认出且不被当成工具() {
        let raw = include_bytes!("../tests/fixtures/asset_echo_real.bin");
        assert!(is_tool_call(raw), "它确实走 exec 通道");
        let w = parse_exec_write(raw).expect("资产写调用必须解得出来");
        assert_eq!(
            w.path,
            "/assets/attach-0-6bd00159-1e01-4924-b8c2-12f28cc81e53.png"
        );
        assert_eq!(w.bytes.len(), 73, "1x1 探针 PNG 的原始字节数");
        assert_eq!(w.id, 0, "实物帧里关联 id 缺省");
        assert_eq!(w.exec_id, "");
        assert!(
            parse_tool_call(raw).is_none(),
            "资产写调用没有工具名,绝不能交给调用方当工具执行"
        );
        assert!(parse_exec_read(raw).is_none(), "写不是读");
    }

    /// 反向钉住:真工具帧(外部/内建)**不是**写/读调用 —— 否则工具回路直接哑掉。
    #[test]
    fn 工具帧不被当成_exec_调用() {
        let ext = include_bytes!("../tests/fixtures/tool_call_external_real.bin");
        assert!(parse_exec_write(ext).is_none(), "外部工具帧不是写调用");
        assert!(parse_exec_read(ext).is_none(), "外部工具帧不是读调用");
        let builtin = include_bytes!("../tests/fixtures/tool_call_builtin_real.bin");
        assert!(parse_exec_write(builtin).is_none(), "内建工具帧不是写调用");
        assert!(parse_exec_read(builtin).is_none(), "内建工具帧不是读调用");
    }

    /// 写调用认领条件:绝对路径 + 内容在场(file_bytes 或 file_text),缺任何一个
    /// 都不认领 —— 真工具调用帧的 `1.2.3` 也是个字符串(另一个 id,PROTOCOL §13.2),
    /// 单靠「能解出字段」会把工具帧吞成写调用,工具回路直接哑掉。
    #[test]
    fn 写调用缺路径或缺内容都不认领() {
        // 有字节没路径。
        let mut args = Writer::new();
        args.bytes(5, b"xxxx");
        let mut ch = Writer::new();
        ch.message(EXEC_WRITE_ARGS, &args);
        let mut top = Writer::new();
        top.message(RESP_TOOL_CHANNEL, &ch);
        assert!(parse_exec_write(&top.into_bytes()).is_none());

        // 有路径没内容。
        let mut args = Writer::new();
        args.string(1, "/assets/attach-0-x.png");
        let mut ch = Writer::new();
        ch.message(EXEC_WRITE_ARGS, &args);
        let mut top = Writer::new();
        top.message(RESP_TOOL_CHANNEL, &ch);
        assert!(parse_exec_write(&top.into_bytes()).is_none());

        // 相对路径不认领(工具帧 1.2.3 的 id 形状)。
        let mut args = Writer::new();
        args.string(1, "3f2a1b9c-0-x7ab");
        args.bytes(5, b"xxxx");
        let mut ch = Writer::new();
        ch.message(EXEC_WRITE_ARGS, &args);
        let mut top = Writer::new();
        top.message(RESP_TOOL_CHANNEL, &ch);
        assert!(parse_exec_write(&top.into_bytes()).is_none());

        // file_text(field 2)也是合法内容载体。
        let mut args = Writer::new();
        args.string(1, "/assets/note.txt");
        args.string(2, "hello");
        let mut ch = Writer::new();
        ch.uint(EXEC_ID, 42);
        ch.string(EXEC_EXEC_ID, "exec-1");
        ch.message(EXEC_WRITE_ARGS, &args);
        let mut top = Writer::new();
        top.message(RESP_TOOL_CHANNEL, &ch);
        let w = parse_exec_write(&top.into_bytes()).expect("file_text 形态也要认得");
        assert_eq!(w.bytes, b"hello");
        assert_eq!(w.id, 42);
        assert_eq!(w.exec_id, "exec-1");
    }

    /// `is_tool_call` 保持宽松(字段在就算,不看 wire type)—— 宁可错进
    /// 工具分支再分类,也不能漏掉真工具帧陪上游死等心跳。
    #[test]
    fn 工具通道判定不挑_wire_type() {
        let mut top = Writer::new();
        top.uint(RESP_TOOL_CHANNEL, 0);
        assert!(is_tool_call(&top.into_bytes()));
        // 但解析入口是严的:wire type 不是 length-delimited 就不认领。
        let mut top2 = Writer::new();
        top2.uint(RESP_TOOL_CHANNEL, 0);
        let top2 = top2.into_bytes();
        assert!(parse_exec_write(&top2).is_none());
        assert!(parse_tool_call(&top2).is_none());
    }

    /// ⭐ 合帧:资产写调用(顶层 field 2)与用量(`1.14`)并进同一帧时,
    /// 两边都得解得出来。chat.rs 的流程依赖这个 —— 先处理 exec、再判工具、
    /// 最后判用量,谁也不许把谁吞掉(吞了用量 = 陪上游死等心跳到 watchdog)。
    #[test]
    fn 资产写调用与用量合帧两者都可解() {
        let mut args = Writer::new();
        args.string(1, "/assets/attach-0-x.png");
        args.bytes(5, b"png-bytes");
        let mut ch = Writer::new();
        ch.message(EXEC_WRITE_ARGS, &args);
        let mut u = Writer::new();
        u.uint(1, 100);
        u.uint(2, 5);
        u.uint(3, 90);
        let mut msg = Writer::new();
        msg.message(RESP_USAGE, &u);
        let mut top = Writer::new();
        top.message(RESP_MESSAGE, &msg);
        top.message(RESP_TOOL_CHANNEL, &ch);
        let frame = top.into_bytes();
        let w = parse_exec_write(&frame).expect("合帧里的写调用必须解得出");
        assert_eq!(w.path, "/assets/attach-0-x.png");
        assert_eq!(w.bytes, b"png-bytes");
        assert!(parse_tool_call(&frame).is_none(), "合帧里没有工具调用");
        assert_eq!(
            parse_frame(&frame).usage,
            Some(WireUsage {
                input: 100,
                output: 5,
                cache_read: 90,
                cache_write: 0
            }),
            "合帧里的用量绝不能被写调用吞掉"
        );
    }

    /// 回执编码:字段号必须与 `agent.v1` schema 一致 —— 自己写自己解是白测,
    /// 所以这里手解字节核对关键字段的位置。
    #[test]
    fn 写盘回执的字段号对齐_schema() {
        let ack = encode_write_success(42, "exec-1", "/assets/a.png", 73);
        // AgentClientMessage.2 = exec_client_message
        let ecm = Reader::new(&ack)
            .find_map(|(f, v)| match (f, v) {
                (2, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .expect("顶层必须有 field 2");
        let mut seen_id = None;
        let mut seen_exec_id = None;
        let mut wr = None;
        for (f, v) in Reader::new(ecm) {
            match (f, v) {
                (1, PbValue::Varint(n)) => seen_id = Some(n),
                (15, PbValue::Len(s)) => {
                    seen_exec_id = Some(String::from_utf8_lossy(s).to_string())
                }
                (3, PbValue::Len(s)) => wr = Some(s),
                _ => {}
            }
        }
        assert_eq!(seen_id, Some(42), "id 必须原样带回(field 1)");
        assert_eq!(
            seen_exec_id.as_deref(),
            Some("exec-1"),
            "exec_id 在 field 15"
        );
        // write_result.1 = write_success{1:path, 3:file_size}
        let ws = Reader::new(wr.expect("write_result 在 field 3"))
            .find_map(|(f, v)| match (f, v) {
                (1, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .expect("success 在 field 1");
        let mut path = None;
        let mut size = None;
        for (f, v) in Reader::new(ws) {
            match (f, v) {
                (1, PbValue::Len(s)) => path = Some(String::from_utf8_lossy(s).to_string()),
                (3, PbValue::Varint(n)) => size = Some(n),
                _ => {}
            }
        }
        assert_eq!(path.as_deref(), Some("/assets/a.png"));
        assert_eq!(size, Some(73));

        // 缺省 id(0 / "")不占字段 —— 实测的资产写调用就是这个形态。
        let ack0 = encode_write_success(0, "", "/assets/a.png", 1);
        let ecm0 = Reader::new(&ack0)
            .find_map(|(f, v)| match (f, v) {
                (2, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(
            !Reader::new(ecm0).any(|(f, _)| f == 1 || f == 15),
            "缺省 id 不该出现在线上"
        );
    }

    /// 读盘回执:图片走 `data`(field 5,bytes),file_size 在 field 4。
    #[test]
    fn 读盘回执的字段号对齐_schema() {
        let reply = encode_read_success_data(7, "e2", "/assets/a.png", b"\x89PNG");
        let ecm = Reader::new(&reply)
            .find_map(|(f, v)| match (f, v) {
                (2, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .expect("顶层必须有 field 2");
        let mut seen_id = None;
        let mut rr = None;
        for (f, v) in Reader::new(ecm) {
            match (f, v) {
                (1, PbValue::Varint(n)) => seen_id = Some(n),
                (7, PbValue::Len(s)) => rr = Some(s),
                _ => {}
            }
        }
        assert_eq!(seen_id, Some(7));
        let rs = Reader::new(rr.expect("read_result 在 field 7"))
            .find_map(|(f, v)| match (f, v) {
                (1, PbValue::Len(s)) => Some(s),
                _ => None,
            })
            .expect("success 在 field 1");
        let mut path = None;
        let mut size = None;
        let mut data = None;
        for (f, v) in Reader::new(rs) {
            match (f, v) {
                (1, PbValue::Len(s)) => path = Some(String::from_utf8_lossy(s).to_string()),
                (4, PbValue::Varint(n)) => size = Some(n),
                (5, PbValue::Len(s)) => data = Some(s.to_vec()),
                _ => {}
            }
        }
        assert_eq!(path.as_deref(), Some("/assets/a.png"));
        assert_eq!(size, Some(4));
        assert_eq!(
            data.as_deref(),
            Some(&b"\x89PNG"[..]),
            "图片字节走 field 5(data)"
        );
    }

    /// 读调用解析:绝对路径才认领,关联 id 带回。
    #[test]
    fn 读调用解析() {
        let mut args = Writer::new();
        args.string(1, "/assets/attach-0-x.png");
        let mut ch = Writer::new();
        ch.uint(EXEC_ID, 9);
        ch.message(EXEC_READ_ARGS, &args);
        let mut top = Writer::new();
        top.message(RESP_TOOL_CHANNEL, &ch);
        let r = parse_exec_read(&top.into_bytes()).expect("读调用必须解得出");
        assert_eq!(r.path, "/assets/attach-0-x.png");
        assert_eq!(r.id, 9);
        assert!(
            parse_exec_write(&{
                let mut args = Writer::new();
                args.string(1, "/assets/attach-0-x.png");
                let mut ch = Writer::new();
                ch.message(EXEC_READ_ARGS, &args);
                let mut top = Writer::new();
                top.message(RESP_TOOL_CHANNEL, &ch);
                top.into_bytes()
            })
            .is_none(),
            "读不是写"
        );
    }

    /// ⭐ 数值参数。这是 2026-08-07 那次「grok 无限重试」的直接病因:
    /// 旧解析只读 `google.protobuf.Value` 的 string 档(field 3),
    /// 模型传 `limit: 200` 时值在 number 档(field 2, double),解出空串,
    /// 客户端收到 `limit=""` → 工具失败 → 模型换个说法再试 → 永远不收敛。
    #[test]
    fn 数值与布尔参数不再解成空() {
        let f = tool_call_frame(
            "read",
            "call-1-0",
            &[
                ("filePath", json!("/tmp/a.rss")),
                ("offset", json!(0)),
                ("limit", json!(200)),
                ("raw", json!(true)),
            ],
        );
        let tc = parse_tool_call(&f).unwrap();
        let m: std::collections::HashMap<_, _> = tc.args.into_iter().collect();
        assert_eq!(m["filePath"], json!("/tmp/a.rss"));
        // 必须是 JSON **数字**,不是 "200" —— schema 写 number/integer 的工具会拒字符串。
        assert_eq!(m["limit"], json!(200));
        assert!(m["limit"].is_i64(), "整数不能回成浮点或字符串");
        assert_eq!(m["offset"], json!(0));
        assert_eq!(m["raw"], json!(true));
    }

    /// 手写字节的 `google.protobuf.Value`:嵌套 Struct 与 List。
    ///
    /// 不用 helper —— 这条要钉住的正是"我方对 Value 的字段号理解",
    /// 用自己的 Writer 造就白测了。
    #[test]
    fn 解析嵌套的_struct_与_list_参数() {
        // ListValue{ 1: [ Value{2: double 1.0}, Value{3: "x"} ] }
        let mut list = Vec::new();
        let mut v1 = vec![0x11u8]; // field 2, wt 1
        v1.extend_from_slice(&1.0f64.to_le_bytes());
        list.push(0x0au8); // field 1, wt 2
        list.push(v1.len() as u8);
        list.extend_from_slice(&v1);
        let v2 = vec![0x1au8, 0x01, b'x']; // field 3, wt 2, "x"
        list.push(0x0a);
        list.push(v2.len() as u8);
        list.extend_from_slice(&v2);
        // Value{ 6: list }
        let mut val = vec![0x32u8]; // field 6, wt 2
        val.push(list.len() as u8);
        val.extend_from_slice(&list);
        assert_eq!(decode_pb_value(&val, 0), json!([1, "x"]));

        // Struct{ 1: MapEntry{1:"k", 2: Value{4: true}} } → Value{5: struct}
        let inner_true = vec![0x20u8, 0x01]; // field 4, wt 0, 1
        let mut entry = vec![0x0au8, 0x01, b'k']; // 1: "k"
        entry.push(0x12); // field 2, wt 2
        entry.push(inner_true.len() as u8);
        entry.extend_from_slice(&inner_true);
        let mut st = vec![0x0au8]; // Struct.fields = field 1
        st.push(entry.len() as u8);
        st.extend_from_slice(&entry);
        let mut sval = vec![0x2au8]; // field 5, wt 2
        sval.push(st.len() as u8);
        sval.extend_from_slice(&st);
        assert_eq!(decode_pb_value(&sval, 0), json!({"k": true}));
    }

    #[test]
    fn 认不出的参数值解成_null_而不是空串() {
        // 未知 oneof 档位(field 99)→ null。空串会被工具当成"给了个空值"。
        let unknown = vec![0xfau8, 0x06, 0x01, b'z']; // field 99, wt 2, len 1
        assert_eq!(decode_pb_value(&unknown, 0), Value::Null);
        assert_eq!(decode_pb_value(&[], 0), Value::Null);
    }

    /// Cursor **内建**工具走别的字段号(实测 `.1` 终端、`.4` 读文件),
    /// 必须**认不出来** —— 认出来会让调用方收到一个它没有的工具名。
    #[test]
    fn 内建工具调用不被当成外部工具() {
        let mut term = Writer::new();
        term.string(1, "ls -la");
        let mut one = Writer::new();
        one.message(1, &term);
        let mut detail = Writer::new();
        detail.message(1, &one); // 1.2.2.1 = 终端工具
        let mut ch = Writer::new();
        ch.string(TC_CALL_ID, "call-x-0");
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        let f = outer.into_bytes();
        assert!(is_tool_call(&f), "仍要认出「这是工具帧」好主动收口");
        assert!(parse_tool_call(&f).is_none(), "但不能当成可转发的外部工具");
    }

    /// 兼容转换层正例:内建终端帧(§13.2 实证 `1.2.2.1.1.1` = 命令串)要解得出来。
    #[test]
    fn 内建终端帧解出命令串() {
        let mut term = Writer::new();
        term.string(1, "ls -la");
        let mut one = Writer::new();
        one.message(1, &term);
        let mut detail = Writer::new();
        detail.message(1, &one); // 1.2.2.1 = 终端工具
        let mut ch = Writer::new();
        ch.string(TC_CALL_ID, "call-x-0");
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        assert_eq!(
            parse_builtin_call(&outer.into_bytes()),
            Some(BuiltinCall::Terminal {
                id: "call-x-0".into(),
                command: "ls -la".into()
            })
        );
    }

    /// 内建读文件帧(§13.2 实证 `.4 = {1:{2:'README'}}`,路径在 `1.2.2.4.1.2`)。
    #[test]
    fn 内建读文件帧解出路径() {
        let mut rd = Writer::new();
        rd.string(2, "/tmp/a.png");
        let mut one = Writer::new();
        one.message(1, &rd);
        let mut detail = Writer::new();
        detail.message(4, &one); // 1.2.2.4 = 读文件
        let mut ch = Writer::new();
        ch.string(TC_CALL_ID, "call-y-0");
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        assert_eq!(
            parse_builtin_call(&outer.into_bytes()),
            Some(BuiltinCall::ReadFile {
                id: "call-y-0".into(),
                path: "/tmp/a.png".into()
            })
        );
    }

    /// 边界钉死:外部工具帧、exec 资产帧(顶层 field 2)、空命令都不许被
    /// 当成内建调用 —— 转换层的失败必须是单向的(认不出 → 落回收口)。
    #[test]
    fn 内建调用解析的边界() {
        // 外部工具(.15)归 parse_tool_call,不归转换层。
        let mut inner = Writer::new();
        inner.string(1, "gwtools-read");
        inner.string(TC_BARE_NAME, "read");
        let mut ext = Writer::new();
        ext.message(TC_INNER, &inner);
        let mut detail = Writer::new();
        detail.message(TC_EXTERNAL, &ext);
        let mut ch = Writer::new();
        ch.string(TC_CALL_ID, "call-z-0");
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        assert_eq!(parse_builtin_call(&outer.into_bytes()), None);

        // exec 资产帧不裹 `1.2`(顶层 field 2 = exec_server_message),它的
        // field 2 恰好是 shell_args —— 越界去解就是张冠李戴翻译出假终端调用。
        let real: &[u8] = include_bytes!("../tests/fixtures/asset_echo_real.bin");
        assert_eq!(parse_builtin_call(real), None);

        // 空命令 → None(落回收口),绝不产出空参数的调用。
        let mut term = Writer::new();
        term.string(1, "   ");
        let mut one = Writer::new();
        one.message(1, &term);
        let mut detail = Writer::new();
        detail.message(1, &one);
        let mut ch = Writer::new();
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        assert_eq!(parse_builtin_call(&outer.into_bytes()), None);
    }

    /// 工具声明落在 `1.2.1.2.7`,**5 个字段一个不能少**(真包 16/16 全都有)。
    #[test]
    fn 工具声明逐字对齐真包的五个字段() {
        let tools = vec![ToolDef {
            name: "get_weather".into(),
            description: "查天气".into(),
            schema: r#"{"type":"object"}"#.into(),
        }];
        let bytes = build_frame0(
            &one_user("北京天气"),
            "sys",
            &tools,
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let det = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
        )
        .unwrap();
        let t = dig(det, &[DET_TOOL]).expect("工具必须落在 1.2.1.2.7");
        // 全名 = <命名空间>-<裸名>,回调时靠这个前缀认领「这是调用方的工具」。
        assert_eq!(
            string_at(t, TOOL_FULL_NAME).as_deref(),
            Some("gwtools-get_weather")
        );
        assert_eq!(string_at(t, TOOL_DESC).as_deref(), Some("查天气"));
        assert_eq!(string_at(t, TOOL_NAMESPACE).as_deref(), Some(TOOL_NS));
        assert_eq!(string_at(t, TOOL_BARE_NAME).as_deref(), Some("get_weather"));
        assert_eq!(
            string_at(t, TOOL_SCHEMA).as_deref(),
            Some(r#"{"type":"object"}"#)
        );
    }

    /// 没声明工具时**不能**凭空多出一个空的 `1.2.1.2.7`。
    #[test]
    fn 没有工具时不发空的工具声明() {
        let bytes = build_frame0(
            &one_user("hi"),
            "",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let det = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
        )
        .unwrap();
        assert!(dig(det, &[DET_TOOL]).is_none());
    }

    /// **绝不发 `1.2.17`**:那里只放 blob 哈希,而我们从没走 FileSync 上传过东西。
    /// 实测发了(带真哈希)会被 `invalid_argument: Failed to resolve request context
    /// blobs` 直接拒掉,而这是唯一一条会让整轮请求作废的踩法。
    #[test]
    fn 绝不发会话级的1_2_17blob哈希块() {
        let bytes = build_frame0(
            &one_user("q"),
            "sys",
            &[],
            Media::default(),
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
        assert!(
            dig(conv, &[17]).is_none(),
            "1.2.17 一旦出现,服务端会去解析我们根本没上传的 blob"
        );
    }

    /// 无图时 `1.2.1.1.3` 发**空**(与真客户端一致);它不是"恒为空",而是附件容器。
    #[test]
    fn 无图时附件容器为空() {
        let bytes = f0(
            &one_user("x"),
            &Model::new("default"),
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let m = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
        )
        .unwrap();
        assert_eq!(string_at(m, MSG_ATTACH).as_deref(), Some(""));
    }

    /// 图片内联进 `1.2.1.1.3`,逐字对齐带图抓包:uuid / 路径 / 宽高 / mime / {哈希, 原始字节}。
    ///
    /// ⚠️ **不走 FileSync**。文档 §5 猜的「图片走 blob 上传」是错的 ——
    /// 带图那次真包里没有任何 FSSyncFile 调用,字节就在请求里。
    #[test]
    fn 图片内联进消息附件容器() {
        // 2×1 的最小合法 PNG(只为让 image_dims 解出尺寸)。
        let png = {
            let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
            v.extend_from_slice(&[0, 0, 0, 13]);
            v.extend_from_slice(b"IHDR");
            v.extend_from_slice(&2u32.to_be_bytes());
            v.extend_from_slice(&1u32.to_be_bytes());
            v.extend_from_slice(&[8, 2, 0, 0, 0]);
            v
        };
        let imgs = vec![ImageAttachment {
            mime: "image/png".into(),
            bytes: png.clone(),
            width: 2,
            height: 1,
        }];
        let bytes = build_frame0(
            &one_user("这是什么"),
            "",
            &[],
            Media {
                images: &imgs,
                docs: &[],
            },
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let m = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
        )
        .unwrap();
        // 注意 `.1`:真包是 1.2.1.1.3 → .1 → 各字段,少这层服务端回 internal。
        let att = dig(m, &[MSG_ATTACH, 1]).expect("图片必须落在 1.2.1.1.3.1");
        assert_eq!(string_at(att, ATT_MIME).as_deref(), Some("image/png"));
        let dims = dig(att, &[ATT_DIMS]).expect("要带宽高");
        assert_eq!(varint_at(dims, ATT_DIM_W), Some(2));
        assert_eq!(varint_at(dims, ATT_DIM_H), Some(1));
        let content = dig(att, &[ATT_CONTENT]).expect("要有 .9 内容块");
        // 原始字节内联,不是哈希引用。
        let inlined = dig(content, &[ATT_BYTES]).expect("原始字节要在 .9.2");
        assert_eq!(inlined, &png[..], "必须是原始字节,不能是 base64 或哈希");
        let hash = dig(content, &[ATT_HASH]).expect("要有 32 字节哈希");
        assert_eq!(hash.len(), 32);
    }

    /// 文档挂**上下文块**的 `1.2.1.2.20`,不是消息附件容器 —— 两者去处不同。
    #[test]
    fn 文档挂上下文块而不是消息() {
        let docs = vec![DocAttachment {
            path: "/tmp/x.pdf".into(),
            text: "%PDF-1.4 内容".into(),
            extracted: None,
        }];
        let bytes = build_frame0(
            &one_user("这个 PDF 讲什么"),
            "",
            &[],
            Media {
                images: &[],
                docs: &docs,
            },
            &Model::new("default"),
            &[],
            "c",
            "UTC",
            1,
            RunShape::default(),
            Phase::Opening,
        );
        let det = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
        )
        .unwrap();
        let d = dig(det, &[DET_DOC]).expect("文档必须落在 1.2.1.2.20");
        assert_eq!(string_at(d, DOC_PATH).as_deref(), Some("/tmp/x.pdf"));
        assert_eq!(string_at(d, DOC_CONTENT).as_deref(), Some("%PDF-1.4 内容"));
        // 而消息附件容器必须还是空的。
        let m = dig(
            &bytes,
            &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
        )
        .unwrap();
        assert_eq!(string_at(m, MSG_ATTACH).as_deref(), Some(""));
    }

    #[test]
    fn 图片尺寸只读头不解像素() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&614u32.to_be_bytes());
        png.extend_from_slice(&694u32.to_be_bytes());
        assert_eq!(image_dims(&png), (614, 694));
        // JPEG:SOF0 段里 高在前、宽在后。
        let jpeg = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x02, 0x58, // SOF0: h=300 w=600
        ];
        assert_eq!(image_dims(&jpeg), (600, 300));
        // 认不出来的格式给 (0,0),**不猜** —— 猜错的宽高会让上游的图像分块算错。
        assert_eq!(image_dims(b"GIF89a...."), (0, 0));
        assert_eq!(image_dims(b""), (0, 0));
    }

    /// **绝不发 `1.2.17`**

    #[test]
    fn parse_text_delta_ignores_session_echo_and_unknown_fields() {
        // field 4 是会话回显,不是文本;还塞一个未知的 fixed64 确认不会错位。
        let mut echo = Writer::new();
        echo.string(1, "conversation-echo");
        let mut inner = Writer::new();
        inner.string(2, "not text");
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &inner);
        outer.message(4, &echo);
        assert_eq!(parse_frame(&outer.into_bytes()), RespFrame::default());
    }

    #[test]
    fn parse_trailer_none_when_no_error() {
        assert!(parse_trailer("{}").is_none());
        assert!(parse_trailer(r#"{"metadata":{}}"#).is_none());
        assert!(parse_trailer("not json").is_none());
    }

    #[test]
    fn parse_trailer_extracts_rate_limit_downgrade() {
        // §4 的实测错误体,逐字。
        let raw = r#"{"error":{"code":"resource_exhausted","details":[{"debug":{"error":"ERROR_RATE_LIMITED_CHANGEABLE",
 "details":{"title":"API usage limit reached",
 "detail":"Switched to grok-4.5 after reaching API limit.",
 "additionalInfo":{"autoSwitchToModel":"grok-4.5","spendLimits":"[50,100,200]"},
 "analyticsMetadata":{"actionRequired":"upgrade"}}}}]}}"#;
        let e = parse_trailer(raw).unwrap();
        assert_eq!(e.code, "resource_exhausted");
        assert_eq!(e.debug_error, "ERROR_RATE_LIMITED_CHANGEABLE");
        assert_eq!(e.title, "API usage limit reached");
        assert_eq!(e.auto_switch_to_model, "grok-4.5");
        assert!(e.summary().contains("ERROR_RATE_LIMITED_CHANGEABLE"));
    }

    #[test]
    fn parse_trailer_survives_missing_layers() {
        // 只有 code,没有 details —— 不能 panic,也不能丢掉 code。
        let e = parse_trailer(r#"{"error":{"code":"unauthenticated"}}"#).unwrap();
        assert_eq!(e.code, "unauthenticated");
        assert!(e.debug_error.is_empty());
        assert_eq!(e.summary(), "unauthenticated");
    }

    #[test]
    fn parse_trailer_keeps_raw_when_nothing_recognizable() {
        // error 存在但形状完全陌生:不能返回一个全空、summary 没信息量的东西。
        let e = parse_trailer(r#"{"error":{"weird":123}}"#).unwrap();
        assert!(e.detail.contains("weird"));
        assert!(!e.summary().contains("上游未给出错误详情"));
    }

    /// `4.2` 内容分节哈希需求检测。
    ///
    /// ⚠️ 帧字节**手写十六进制**,不用 `Writer`/常量造 —— lessons §7:逆向出来的
    /// 协议若拿与解析器同一套常量造帧再解,字段号整套写错也照样绿。
    /// 结构 `{4: {2: {1: bin[32]}}}`:
    ///   `22 24`(field4,len36)`12 22`(field2,len34)`0a 20`(field1,len32)+32B
    #[test]
    fn 内容分节哈希需求_按实物结构识别() {
        let h32: Vec<u8> = (0u8..32).collect();
        let mut frame = vec![0x22, 0x24, 0x12, 0x22, 0x0a, 0x20];
        frame.extend_from_slice(&h32);
        assert_eq!(content_hash_echo(&frame), 1);

        // 同一个 field-4 里两条需求。
        let mut two = vec![0x22, 0x48, 0x12, 0x22, 0x0a, 0x20];
        two.extend_from_slice(&h32);
        two.extend_from_slice(&[0x12, 0x22, 0x0a, 0x20]);
        two.extend_from_slice(&h32);
        assert_eq!(content_hash_echo(&two), 2);

        // 负面①:`.1` 不是 32 字节 → 不是内容分节哈希。
        let short = vec![0x22, 0x06, 0x12, 0x04, 0x0a, 0x02, 0xaa, 0xbb];
        assert_eq!(content_hash_echo(&short), 0);
        // 负面②:空帧/非 field-4。
        assert_eq!(content_hash_echo(&[]), 0);
        assert_eq!(content_hash_echo(&[0x0a, 0x02, 0x01, 0x02]), 0);
    }

    /// 负面③(**用真实抓包字节**):带顶层 `.3` 描述符的真响应帧里没有内容分节需求。
    /// 这条是防误报的关键 —— 若 `.3` 被误算成需求,每条正常续轮都会被判成要 CAS 上传。
    #[test]
    fn 内容分节哈希需求_真描述符帧不误报() {
        assert_eq!(
            content_hash_echo(&crate::run::descriptor_samples::turn1()),
            0
        );
        assert_eq!(
            content_hash_echo(&crate::run::descriptor_samples::turn2()),
            0
        );
    }

    /// 判决语义:**只有「拿不出来」才是失败**,「被点名」本身不是。
    ///
    /// 这条是 2026-08-23 时序实证之后的更正。早先把「出现 `4.2`」当 NO-GO,
    /// 会把一次健康的 resume(服务端缓存过期→索取→客户端交付)判成方案不可行。
    #[test]
    fn wire判决_点名不是失败_拿不出来才是() {
        // 点名了、也全都交付了(上传腿接上后的正常 resume)→ Ok。
        let served = WireTurnOutcome {
            saw_descriptor: true,
            saw_finish: true,
            content_chars: 42,
            demanded: 7,
            unavailable: 0,
        };
        assert_eq!(served.verdict(), WireVerdict::Ok);
        assert!(!served.should_fallback());
        assert!(!served.verdict().needs_reflow());

        // 点名 7 个、拿不出 7 个(当前无上传腿的形态)→ 本轮失败 + 作废描述符。
        let missing = WireTurnOutcome {
            demanded: 7,
            unavailable: 7,
            ..Default::default()
        };
        assert_eq!(
            missing.verdict(),
            WireVerdict::ContentUnavailable {
                missing: 7,
                demanded: 7
            }
        );
        assert!(missing.should_fallback());
        assert!(missing.verdict().needs_reflow());

        // 部分拿不出来也算失败:少一节就是少一段历史。
        let partial = WireTurnOutcome {
            saw_finish: true,
            content_chars: 10,
            demanded: 7,
            unavailable: 1,
            ..Default::default()
        };
        assert!(partial.should_fallback(), "缺一节也不能记成功");
    }

    /// 判决优先级与 NoFinish 的不对称处理。
    #[test]
    fn wire判决_优先级与NoFinish不回退() {
        // 裸态 + 拿不出内容:必须报 ContentUnavailable(它解释裸态成因)。
        let both = WireTurnOutcome {
            demanded: 3,
            unavailable: 3,
            ..Default::default()
        };
        assert!(matches!(
            both.verdict(),
            WireVerdict::ContentUnavailable { .. }
        ));

        // 纯裸态。
        let barren = WireTurnOutcome::default();
        assert_eq!(barren.verdict(), WireVerdict::Barren);
        assert!(barren.should_fallback());

        // 出了字但没收尾:**不**回退本轮(答案有价值),但**要**作废描述符。
        let nf = WireTurnOutcome {
            content_chars: 42,
            ..Default::default()
        };
        assert_eq!(nf.verdict(), WireVerdict::NoFinish);
        assert!(!nf.should_fallback(), "出了字的轮次不该扔掉");
        assert!(nf.verdict().needs_reflow(), "但它的描述符不可信,必须作废");

        // 干净收尾但没有新 `.3` → NoDescriptor(codex 二轮 #2)。
        //
        // 官方每轮尾部都给新描述符(实测 turn1/turn2/turn4 各 3 份),不给就是异常。
        // 处理不对称:**本轮不回退**(回答是好的,扔掉等于白烧一次额度),
        // 但**旧描述符必须作废** —— 它已经缺了本轮,留着下轮回放就是静默失忆。
        let no_desc = WireTurnOutcome {
            saw_descriptor: false,
            saw_finish: true,
            content_chars: 42,
            ..Default::default()
        };
        assert_eq!(no_desc.verdict(), WireVerdict::NoDescriptor);
        assert!(!no_desc.should_fallback(), "出了字的轮次不该扔掉");
        assert!(no_desc.verdict().needs_reflow(), "但旧描述符必须作废");

        // 有新描述符 + 干净收尾 + 出字 = 唯一的 Ok。
        let ok = WireTurnOutcome {
            saw_descriptor: true,
            saw_finish: true,
            content_chars: 42,
            ..Default::default()
        };
        assert_eq!(ok.verdict(), WireVerdict::Ok);
        assert!(!ok.verdict().needs_reflow());
    }

    /// 空顶层 `.3` 不算捕获(codex 审查 #4):否则上层判成「有料」→ 静默失忆。
    #[test]
    fn 空描述符不算捕获() {
        // {3: ""} —— 结构合法但零长度。
        let payload = vec![0x1a, 0x00];
        assert!(descriptor_field3(&payload).is_none(), "空 .3 必须当没有");
        // 一帧里先空后非空:取非空那份。
        let mut mixed = vec![0x1a, 0x00];
        mixed.extend_from_slice(&[0x1a, 0x03, b'a', b'b', b'c']);
        assert_eq!(descriptor_field3(&mixed), Some(&b"abc"[..]));
        // 先非空后空:空的**不得**把非空那份挤掉。
        let mut mixed2 = vec![0x1a, 0x03, b'a', b'b', b'c'];
        mixed2.extend_from_slice(&[0x1a, 0x00]);
        assert_eq!(descriptor_field3(&mixed2), Some(&b"abc"[..]));
    }

    /// 影子模式①:真实抓包帧(resp-011/013 形态)里解出顶层 `.3` 并数对 ref 个数。
    /// turn1 = 4 个 blob 引用,turn2 = 6 个(turn1 产生的新 blob 追加进来)。
    #[test]
    fn 描述符_真实帧解出field3并数对ref() {
        let t1 = crate::run::descriptor_samples::turn1();
        let d1 = descriptor_field3(&t1).expect("turn1 帧必须带顶层 .3");
        assert_eq!(descriptor_ref_count(d1), 4, "turn1 描述符 = 4 个 blob 引用");
        // 整帧就是一份 `.3`(抓包实物:顶层只有 field 3)。
        assert_eq!(d1.len() + (t1.len() - d1.len()), t1.len());

        let t2 = crate::run::descriptor_samples::turn2();
        let d2 = descriptor_field3(&t2).expect("turn2 帧必须带顶层 .3");
        assert_eq!(descriptor_ref_count(d2), 6, "turn2 描述符 = 6 个 blob 引用");
        // turn1 的 4 个 ref 原样排在 turn2 前头(纯追加,顺序不动)——
        // 这是「描述符 = 累计态」的直接证据,也是将来回放能跨轮的依据。
        let refs_of = |d: &[u8]| -> Vec<Vec<u8>> {
            Reader::new(d)
                .filter(|(f, v)| *f == 1 && matches!(v, PbValue::Len(s) if s.len() == 32))
                .map(|(_, v)| match v {
                    PbValue::Len(s) => s.to_vec(),
                    _ => unreachable!(),
                })
                .collect()
        };
        let (r1, r2) = (refs_of(d1), refs_of(d2));
        assert_eq!(
            &r2[..4],
            &r1[..],
            "turn2 的前 4 个 ref 必须就是 turn1 的全部"
        );
    }

    /// 影子模式②:顶层不带 field 3 的帧不产生捕获。
    #[test]
    fn 描述符_无field3不捕获() {
        // 真实 turn1 的 resp-012(顶层只有 field 1 的状态帧)。
        let payload = [0x0a, 0x08, 0x8a, 0x01, 0x05, 0x08, 0x02, 0x10, 0xda, 0x17];
        assert!(descriptor_field3(&payload).is_none());
        assert!(descriptor_field3(&[]).is_none());
    }

    /// 一帧内 `.3` 多次出现时取**最后一份**(尾部状态最新,见函数注释)。
    #[test]
    fn 描述符_一帧多份取尾部() {
        let mut w = Writer::new();
        w.bytes(3, b"first");
        w.bytes(1, b"text");
        w.bytes(3, b"second");
        let p = w.into_bytes();
        assert_eq!(descriptor_field3(&p), Some(&b"second"[..]));
    }
}
