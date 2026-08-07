//! Cursor 线缆细节:`x-cursor-checksum` 生成、请求头 profile、ConnectRPC 分帧。
//!
//! 全部逐字对齐解包 workbench.desktop.main.js(3.14.27)得到的实现:
//! - checksum:`base64url(zyg(6字节时间戳)) + machineId [ + "/" + macMachineId ]`
//!   其中 `zyg`(社区称 "Jyh cipher")= `t=165; e[i]=(e[i]^t)+i%256; t=e[i]`。
//! - ConnectRPC(server-streaming)信封:`[flag:1][len:4 大端][payload]`,
//!   flag bit0=压缩,bit1=end-of-stream(尾帧是 JSON trailer,非 protobuf)。

use base64::Engine;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cursor 客户端版本(解包本机 3.14.27)。作为 `x-cursor-client-version` 发送。
pub const CLIENT_VERSION: &str = "3.14.27";
/// 客户端 build commit(product.json)。
///
/// ⚠️ 真 IDE 的 `AgentService/Run` **不发** `x-cursor-client-commit`(抓包实证,
/// 见 PROTOCOL-agent-run.md §2.2)。早期代码发它,是基于「版本过旧被软封」这个
/// 已被推翻的猜想 —— 那个 500 其实是打了退役端点。保留常量仅供 unary 端点与排查用。
pub const CLIENT_COMMIT: &str = "047548b00c1a079373d74d00183f32510a4a41e0";
/// 平台标识(逐字对齐真客户端)。
pub const CLIENT_OS: &str = "linux";
pub const CLIENT_ARCH: &str = "x64";
/// ⚠️ 同 `CLIENT_COMMIT`:真 IDE 的 Run 请求**不发** `x-cursor-client-os-version`。
pub const CLIENT_OS_VERSION: &str = "7.0.0-29-generic";

/// `x-cursor-client-type`。⚠️ 真值是 `glass` 而非 `ide` —— bundle 里是 `g ?? "ide"`,
/// `ide` 只是兜底分支,真 IDE 实际发 `glass`(PROTOCOL §2.2)。
pub const CLIENT_TYPE: &str = "glass";
/// `x-cursor-client-layout`,同为 `glass`。早期代码完全没发这个头。
pub const CLIENT_LAYOUT: &str = "glass";
/// Connect 客户端 UA,真 IDE 用的是 connect-es 的默认值。
pub const USER_AGENT: &str = "connect-es/1.6.1";

/// zyg 混淆(逐字复刻 bundle:`t=165; e[i]=(e[i]^t)+i%256; t=e[i]`)。
fn zyg(bytes: &mut [u8]) {
    let mut t: u8 = 165;
    for (i, b) in bytes.iter_mut().enumerate() {
        // JS 里是 `(e[i]^t)+i%256`,+ 后按 &255 截断,均在 u8 环上,故用 wrapping。
        let v = (*b ^ t).wrapping_add((i % 256) as u8);
        *b = v;
        t = v;
    }
}

/// URL-safe base64(无 padding),对齐 bundle 里传入 checksum 的 base64Fn。
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 生成 `x-cursor-checksum`。`now_unit` = `Math.floor(Date.now()/1e6)`(可注入以便测试)。
pub fn checksum_at(machine_id: &str, mac_machine_id: Option<&str>, now_unit: u64) -> String {
    let c = now_unit;
    let mut ts = [
        ((c >> 40) & 255) as u8,
        ((c >> 32) & 255) as u8,
        ((c >> 24) & 255) as u8,
        ((c >> 16) & 255) as u8,
        ((c >> 8) & 255) as u8,
        (c & 255) as u8,
    ];
    zyg(&mut ts);
    let head = b64url(&ts);
    match mac_machine_id {
        Some(m) if !m.is_empty() => format!("{head}{machine_id}/{m}"),
        _ => format!("{head}{machine_id}"),
    }
}

/// 当前时刻的 checksum。
pub fn checksum(machine_id: &str, mac_machine_id: Option<&str>) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    checksum_at(machine_id, mac_machine_id, now_ms / 1_000_000)
}

/// `x-client-key` = sha256hex(token)(社区验证:服务端接受每 token 稳定值)。
pub fn client_key(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// 缺省 machineId:账号未显式配置时,用 sha256hex(token)(握手实测服务端接受)。
pub fn default_machine_id(token: &str) -> String {
    client_key(token)
}

/// 缺省 macMachineId。
///
/// 真 IDE 的 `x-cursor-checksum` 是 **137 字符** = 8(b64 头)+ 64(machineId)
/// + 1(`/`)+ 64(macMachineId)。`mac_machine_id` 留空时 [`checksum_at`] 只产 72 字符,
/// **长度与真客户端不同**,这本身就是一个可区分特征。所以缺省也要派生一个,
/// 而不是让它是 `None`。用不同的盐,免得两个 id 相同(真机上它们是两个独立值)。
pub fn default_mac_machine_id(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update(b"/mac");
    hex(&h.finalize())
}

/// `x-session-id` = UUIDv5(DNS, token):按 token 确定性派生,保持会话稳定。
pub fn session_id(token: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, token.as_bytes()).to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// ConnectRPC 信封:给一段 payload 加 `[flag:1][len:4 大端]` 前缀(flag=0 非压缩数据帧)。
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0x00);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// 流式帧解码器:喂入任意分片字节,切出完整帧 `(flag, payload)`。
///
/// Connect server-streaming:每帧 5 字节头(1 flag + 4 大端长度)+ payload。
/// flag bit1(0x02)= end-of-stream,其 payload 是 JSON trailer(状态/错误),非 protobuf。
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 单帧声明长度的上限。
    ///
    /// 长度字段是 4 字节,最大能声明 4GB。声明一个荒谬的长度不会立刻分配 4GB
    /// (我们只在"字节到齐"后才切),但它会让 `buf` 一直吃进所有到达的字节、
    /// 永不切帧 —— 表现是内存缓慢涨到流结束,且 watchdog 看不出异常(有字节在来)。
    /// 帧长超过这个数就是协议出事了,应当立刻报错。
    const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

    /// 取出一个完整帧;不足一帧返回 `None`(等更多字节)。
    ///
    /// 声明长度荒谬时返回 `Err`(而不是无限等)。
    pub fn next_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        self.try_next_frame().unwrap_or(None)
    }

    /// 同 [`Self::next_frame`],但把"帧长非法"作为错误交出来。
    pub fn try_next_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let flag = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > Self::MAX_FRAME_LEN {
            return Err(format!(
                "Cursor 帧声明长度 {len} 超过 {} 上限,协议已漂移或链路被污染",
                Self::MAX_FRAME_LEN
            ));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((flag, payload)))
    }
}

// ── 逐帧 gzip ───────────────────────────────────────────────────────────────
//
// `AgentService/Run` 的请求与响应都带 `connect-content-encoding: gzip`,语义是
// **每个 Connect 数据帧的 payload 各自独立 gzip**(不是整条 HTTP body 压一次)。
// 这与 HTTP 层的 `content-encoding` 是两回事,reqwest 不会代劳。
// 帧头 flag 的 bit0(0x01)标记该帧是否压缩。

/// gzip 压缩一段字节。
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzEncoder::new(data, flate2::Compression::default()).read_to_end(&mut out)?;
    Ok(out)
}

/// 单帧解压后的字节上限。
///
/// 上游响应是**不可信输入**(协议会漂移,链路上还可能有代理)。一个几 KB 的合法 gzip
/// 帧可以解压出 GB 级 —— `read_to_end` 会照做,把 worker 撑爆。而这个 worker 上还跑着
/// 别的账号,OOM 的爆炸半径是整个进程,不是这一个请求。
///
/// 64MB 远大于任何真实的正文/思考增量帧(实测都在 KB 级),又不至于打爆内存。
/// `pdf::inflate` 早就防了这一手,这里补上同一道闸。
const MAX_FRAME_INFLATED: u64 = 64 * 1024 * 1024;

/// gzip 解压一段字节。超过 [`MAX_FRAME_INFLATED`] 即报错(而不是继续吃内存)。
pub fn gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    // `take` 到上限**再多一个字节**:读满 limit 说明还有更多,应当报错而不是
    // 悄悄给出一个被截断的 payload(截断的 protobuf 解出来是空,表现为"回复丢字")。
    flate2::read::GzDecoder::new(data)
        .take(MAX_FRAME_INFLATED + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > MAX_FRAME_INFLATED {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Cursor 帧解压后超过 {MAX_FRAME_INFLATED} 字节上限,疑似压缩炸弹"),
        ));
    }
    Ok(out)
}

/// ConnectRPC 数据帧,payload 经 gzip 压缩(flag bit0 = 1)。
pub fn frame_compressed(payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let z = gzip(payload)?;
    let mut out = Vec::with_capacity(5 + z.len());
    out.push(0x01);
    out.extend_from_slice(&(z.len() as u32).to_be_bytes());
    out.extend_from_slice(&z);
    Ok(out)
}

/// 还原一个帧的 payload:flag bit0 置位则 gunzip,否则原样返回。
///
/// 服务端**可以逐帧决定压不压**(小帧常常不压),所以必须按帧的 flag 判断,
/// 不能因为响应头写了 `connect-content-encoding: gzip` 就无条件解压。
pub fn frame_payload(flag: u8, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    if flag & 0x01 != 0 {
        gunzip(payload)
    } else {
        Ok(payload.to_vec())
    }
}

// ── 每请求/每会话随机值 ─────────────────────────────────────────────────────

/// n 字节**全熵**随机值的 hex 表示。
///
/// ## 为什么不能用 UUIDv4 拼字节
///
/// 早先这里是 `Uuid::new_v4().as_bytes()` 拼接,理由是"熵源相同(都是 getrandom),
/// 只是每 16 字节有 6 bit 被版本/变体位占掉"。**那 6 bit 是致命的**:UUIDv4 的
/// hex 第 13 位恒为 `4`、第 17 位恒在 `89ab`。于是本网关发出的**每一条**
/// `traceparent` 的 trace-id 都带着同一个固定位模式 —— 对做流量聚类的风控来说,
/// 这不是"随机得不够",而是一个**只属于我们**的稳定特征,每请求广播两次。
/// 真客户端用的是 `crypto.getRandomValues`,全熵。
pub fn random_hex(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    // 失败即 panic 是对的:熵源不可用时继续跑等于发出可预测的 trace id。
    getrandom::fill(&mut bytes).expect("系统熵源不可用");
    hex(&bytes)
}

/// W3C traceparent,trace-flags = `01`(已采样)。用于 `backend-traceparent`。
pub fn traceparent() -> String {
    format!("00-{}-{}-01", random_hex(16), random_hex(8))
}

/// W3C traceparent,trace-flags = `00`(未采样)。用于 `traceparent`。
///
/// 抓包实物两条头的 flags **不同**:`traceparent` 是 `-00`、`backend-traceparent`
/// 是 `-01`。都写成 `-01` 会成为一个可区分特征。
pub fn traceparent_unsampled() -> String {
    format!("00-{}-{}-00", random_hex(16), random_hex(8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_shape_and_determinism() {
        // 固定时间戳 → 头部稳定;machineId 原样拼接在后。
        let cs = checksum_at("MACHINE", None, 0x0102_0304_0506);
        assert!(cs.ends_with("MACHINE"));
        assert_eq!(cs, checksum_at("MACHINE", None, 0x0102_0304_0506));
        // 带 macMachineId 用 "/" 分隔。
        let cs2 = checksum_at("MID", Some("MAC"), 0x0102_0304_0506);
        assert!(cs2.ends_with("MID/MAC"));
    }

    #[test]
    fn zyg_matches_reference_vector() {
        // 复刻 JS:t=165; e[i]=(e[i]^t)+i%256; t=e[i]。手算前 3 字节做金标准。
        let mut b = [0u8, 0u8, 0u8];
        super::zyg(&mut b);
        // e[0]=(0^165)+0=165; t=165. e[1]=(0^165)+1=166; t=166. e[2]=(0^166)+2=168; t=168.
        assert_eq!(b, [165, 166, 168]);
    }

    #[test]
    fn client_key_is_sha256_hex() {
        // sha256("") 的已知值前缀。
        let k = client_key("");
        assert_eq!(&k[..8], "e3b0c442");
        assert_eq!(k.len(), 64);
    }

    #[test]
    fn frame_roundtrip_single() {
        let payload = b"hello cursor";
        let framed = frame(payload);
        assert_eq!(framed[0], 0x00);
        let mut d = FrameDecoder::new();
        d.feed(&framed);
        let (flag, got) = d.next_frame().unwrap();
        assert_eq!(flag, 0x00);
        assert_eq!(got, payload);
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn frame_decoder_handles_split_and_multiple() {
        let f1 = frame(b"aaa");
        let f2 = frame(b"bbbb");
        let mut d = FrameDecoder::new();
        // 分片喂:先喂 f1 的一半
        d.feed(&f1[..3]);
        assert!(d.next_frame().is_none());
        d.feed(&f1[3..]);
        d.feed(&f2);
        assert_eq!(d.next_frame().unwrap().1, b"aaa");
        assert_eq!(d.next_frame().unwrap().1, b"bbbb");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn gzip_frame_roundtrip_through_decoder() {
        // Run 端点的数据帧:gzip payload + flag bit0。走一遍真实解码路径。
        let payload = b"the quick brown fox jumps over the lazy dog, repeatedly, so gzip helps";
        let framed = frame_compressed(payload).unwrap();
        assert_eq!(framed[0] & 0x01, 0x01, "压缩帧必须置 flag bit0");

        let mut d = FrameDecoder::new();
        d.feed(&framed);
        let (flag, body) = d.next_frame().unwrap();
        assert_ne!(body, payload, "帧内应是压缩后的字节");
        assert_eq!(frame_payload(flag, &body).unwrap(), payload);
    }

    #[test]
    fn frame_payload_passes_through_uncompressed_frames() {
        // 服务端可以逐帧决定压不压;未置位的帧不能去 gunzip,否则报错。
        let raw = b"not compressed";
        assert_eq!(frame_payload(0x00, raw).unwrap(), raw);
        // 明文帧若被误当成 gzip 解,会拿到 Err —— 断言这条边界是真的会炸,
        // 以免将来有人把判断改成「看响应头」而不是「看帧 flag」。
        assert!(frame_payload(0x01, raw).is_err());
    }

    #[test]
    fn gzip_roundtrip_bare() {
        let data = vec![7u8; 4096];
        let z = gzip(&data).unwrap();
        assert!(z.len() < data.len(), "全同字节应被显著压缩");
        assert_eq!(gunzip(&z).unwrap(), data);
    }

    #[test]
    fn random_hex_has_right_length_and_varies() {
        assert_eq!(random_hex(32).len(), 64);
        assert_eq!(random_hex(8).len(), 16);
        assert_eq!(random_hex(20).len(), 40);
        assert_ne!(random_hex(32), random_hex(32));
        assert!(random_hex(32).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn trace_ids_carry_no_uuid_version_bits() {
        // UUIDv4 会让第 13 位恒为 '4'、第 17 位恒在 89ab。**每请求广播两次的固定特征。**
        // 抽 200 个 trace-id,这两位必须都出现过多种取值。
        let mut at13 = std::collections::HashSet::new();
        let mut at17 = std::collections::HashSet::new();
        for _ in 0..200 {
            let id = random_hex(16);
            at13.insert(id.as_bytes()[12]);
            at17.insert(id.as_bytes()[16]);
        }
        assert!(at13.len() > 8, "第 13 位取值太集中,疑似又退回 UUID 拼接:{at13:?}");
        assert!(at17.len() > 8, "第 17 位取值太集中:{at17:?}");
    }

    #[test]
    fn traceparent_matches_w3c_shape() {
        let tp = traceparent();
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32, "trace-id 是 16 字节");
        assert_eq!(parts[2].len(), 16, "span-id 是 8 字节");
        assert_eq!(parts[3], "01");
    }

    #[test]
    fn end_of_stream_flag_visible() {
        // 手工构造一个 flag=0x02 的尾帧。
        let mut framed = frame(b"{}");
        framed[0] = 0x02;
        let mut d = FrameDecoder::new();
        d.feed(&framed);
        let (flag, _) = d.next_frame().unwrap();
        assert_eq!(flag & 0x02, 0x02);
    }
}
