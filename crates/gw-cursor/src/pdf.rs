//! PDF 文本层抽取(尽力而为,零新依赖)。
//!
//! ## 为什么必须在我方抽
//!
//! Cursor 的协议**不原生支持 PDF**。真客户端的做法是:把路径写进 prompt、把原文放进
//! `1.2.1.2.20` 登记表,然后让模型**调终端工具跑 `pdftotext`**,由客户端在真实磁盘上
//! 执行并把结果回传(2026-08-07 实测抓到 `pdftotext "/tmp/gw-cursor/doc-0.pdf"`)。
//!
//! 反代**不能**走这条路:答应内建终端工具调用等于在我们的服务器上执行模型选定的任意
//! shell 命令。所以只剩一条路 —— 自己把文本层抽出来,当普通文字塞进上下文。
//!
//! ## 范围与诚实的边界
//!
//! 只做两件事:解开 `FlateDecode` 的内容流,再从 `Tj` / `TJ` / `'` / `"` 这几个显示
//! 操作符里取字符串。这覆盖绝大多数「文字型」PDF。**不支持**:
//! - 扫描件 / 纯图片 PDF(没有文本层,抽不出东西 —— 那需要 OCR)
//! - 非 Flate 的压缩(LZW / RunLength / DCT 等)
//! - CID / 自定义编码字体的字形映射(抽出来可能是乱码)
//! - 阅读顺序:按流内出现顺序拼,多栏排版会串行
//!
//! 抽不到东西时返回 `None`,调用方据此给模型一句明确说明,而不是塞一段空白
//! 让它以为文档是空的。

use std::io::Read;

/// 抽取结果的**总**字节上限。
///
/// `inflate` 的 16MB 是**单流**上限,不构成总量保证:一个含 100 个 Flate 流的 PDF
/// 每流都合法地解到 16MB,累计就是 1.6GB —— 而这段 `String` 随后还要进 prompt、
/// 进 protobuf、进 gzip 缓冲,峰值是它的数倍。PDF 来自客户,不需要攻破上游就能触发。
///
/// 4MB 文本约等于 100 万汉字,远超任何能塞进 300k 上下文的量;超了直接停,
/// 已抽到的部分照样可用。
const MAX_EXTRACTED: usize = 4 * 1024 * 1024;

/// 扫描的内容流数量上限。防"几万个空流"把 `streams()` 拖成长时间同步占用。
const MAX_STREAMS: usize = 512;

/// 从 PDF 字节里尽力抽出文本。抽不到可读内容 → `None`。
pub fn extract_text(pdf: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut truncated = false;
    for stream in streams(pdf).into_iter().take(MAX_STREAMS) {
        if out.len() >= MAX_EXTRACTED {
            truncated = true;
            break;
        }
        let data = match stream.flate {
            true => match inflate(stream.body) {
                Some(d) => d,
                None => continue,
            },
            false => stream.body.to_vec(),
        };
        collect_shown_text(&data, &mut out);
    }
    if truncated || out.len() > MAX_EXTRACTED {
        // 在字符边界上截,别把多字节字符切一半。
        let mut cut = MAX_EXTRACTED.min(out.len());
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        tracing::warn!(bytes = out.len(), "cursor PDF 文本层超上限,已截断");
        out.push_str("\n\n[文档过长,以上是截断后的文本层]");
    }
    let cleaned = squeeze(&out);
    // 少于 2 个字符基本等于没抽到(常见于扫描件)。
    (cleaned.chars().count() >= 2).then_some(cleaned)
}

struct Stream<'a> {
    body: &'a [u8],
    flate: bool,
}

/// 切出所有 `stream … endstream` 段,并记录它前面的字典里有没有 `/FlateDecode`。
fn streams(pdf: &[u8]) -> Vec<Stream<'_>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = find(&pdf[i..], b"stream") {
        let kw = i + rel;
        // 字典在关键字之前。往回看一小段就够判断过滤器 —— 整份回看会把上一个对象的
        // 过滤器算进来。
        let back = kw.saturating_sub(400);
        let flate = find(&pdf[back..kw], b"/FlateDecode").is_some();
        // `stream` 后面是 CRLF 或 LF。
        let mut s = kw + 6;
        if pdf.get(s) == Some(&b'\r') {
            s += 1;
        }
        if pdf.get(s) == Some(&b'\n') {
            s += 1;
        }
        let Some(erel) = find(&pdf[s..], b"endstream") else {
            break;
        };
        let e = s + erel;
        out.push(Stream { body: &pdf[s..e], flate });
        i = e + 9;
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    // **限制解压上限**:PDF 是不可信输入,一个几 KB 的流可以声明成天文级尺寸。
    // 16MB 足够任何文字型 PDF 的单个内容流,又不至于把 worker 撑爆。
    const MAX: u64 = 16 * 1024 * 1024;
    flate2::read::ZlibDecoder::new(data)
        .take(MAX)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// 从内容流里取显示操作符的字符串实参。
///
/// 认四个:`Tj`(单串)、`TJ`(串/数混排的数组)、`'` 与 `"`(换行后显示)。
/// 参数一律是 `( … )` 形态的 PDF 字符串;十六进制 `< … >` 串少见,一并支持。
fn collect_shown_text(data: &[u8], out: &mut String) {
    let mut i = 0usize;
    // 待定的字符串:遇到显示操作符才落地,否则丢掉(避免把字典里的名字当正文)。
    let mut pending: Vec<String> = Vec::new();
    while i < data.len() {
        match data[i] {
            b'(' => {
                let (s, next) = read_literal(data, i + 1);
                pending.push(s);
                i = next;
            }
            b'<' if data.get(i + 1).is_some_and(|c| *c != b'<') => {
                let (s, next) = read_hex(data, i + 1);
                pending.push(s);
                i = next;
            }
            b'T' if data.get(i + 1) == Some(&b'j') || data.get(i + 1) == Some(&b'J') => {
                for s in pending.drain(..) {
                    out.push_str(&s);
                }
                out.push(' ');
                i += 2;
            }
            b'\'' | b'"' => {
                for s in pending.drain(..) {
                    out.push_str(&s);
                }
                out.push('\n');
                i += 1;
            }
            // 其它操作符出现 = 之前攒的串不是要显示的,丢掉。
            b'A'..=b'Z' | b'a'..=b'z' => {
                pending.clear();
                while i < data.len() && data[i].is_ascii_alphabetic() {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
}

/// 读 `( … )` 字符串。处理转义与嵌套括号。
fn read_literal(data: &[u8], mut i: usize) -> (String, usize) {
    let mut buf: Vec<u8> = Vec::new();
    let mut depth = 1usize;
    while i < data.len() {
        match data[i] {
            b'\\' => {
                i += 1;
                match data.get(i) {
                    Some(b'n') => buf.push(b'\n'),
                    Some(b'r') => buf.push(b'\r'),
                    Some(b't') => buf.push(b'\t'),
                    Some(b'(') => buf.push(b'('),
                    Some(b')') => buf.push(b')'),
                    Some(b'\\') => buf.push(b'\\'),
                    // 八进制 \ddd
                    Some(c) if c.is_ascii_digit() => {
                        let mut v = 0u32;
                        let mut n = 0;
                        while n < 3 && data.get(i).is_some_and(|c| c.is_ascii_digit()) {
                            v = v * 8 + (data[i] - b'0') as u32;
                            i += 1;
                            n += 1;
                        }
                        i -= 1;
                        buf.push((v & 0xFF) as u8);
                    }
                    Some(c) => buf.push(*c),
                    None => break,
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                buf.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
                buf.push(b')');
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    (decode_pdf_string(&buf), i)
}

/// 读 `< … >` 十六进制串。
fn read_hex(data: &[u8], mut i: usize) -> (String, usize) {
    let mut nibbles: Vec<u8> = Vec::new();
    while i < data.len() && data[i] != b'>' {
        let c = data[i];
        if let Some(v) = (c as char).to_digit(16) {
            nibbles.push(v as u8);
        }
        i += 1;
    }
    let bytes: Vec<u8> = nibbles.chunks(2).map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0)).collect();
    (decode_pdf_string(&bytes), i.min(data.len()).saturating_add(1))
}

/// PDF 字符串 → Rust String。
///
/// 带 UTF-16BE BOM 的按 UTF-16 解;否则当 Latin-1(PDFDocEncoding 的可读子集)。
/// 用 Latin-1 而不是 UTF-8:PDF 的默认单字节编码就是它,按 UTF-8 解会把
/// 拉丁扩展字符变成替换符。
fn decode_pdf_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let units: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    // 先试 UTF-8(现代生成器常直接写 UTF-8),不成再 Latin-1。
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|c| *c as char).collect(),
    }
}

/// 压掉连续空白,去掉控制字符。
fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            last_ws = true;
        } else if c.is_control() {
            // 丢掉
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未压缩内容流 + `Tj`:最基本的形态,也是我们自己造探针 PDF 的形态。
    #[test]
    fn 抽出未压缩流里的文本() {
        let pdf = b"%PDF-1.4\n4 0 obj\n<< /Length 60 >>\nstream\nBT /F1 14 Tf 40 700 Td (CURSOR-PDF-SENTINEL-7Q4X) Tj ET\nendstream\nendobj\n";
        let t = extract_text(pdf).expect("应抽到文本");
        assert!(t.contains("CURSOR-PDF-SENTINEL-7Q4X"), "实际抽到: {t:?}");
    }

    #[test]
    fn 抽出flate压缩流里的文本() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let content = b"BT (Hello) Tj (World) Tj ET";
        let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(content).unwrap();
        let comp = e.finish().unwrap();
        let mut pdf = b"%PDF-1.4\n1 0 obj\n<< /Filter /FlateDecode >>\nstream\n".to_vec();
        pdf.extend_from_slice(&comp);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let t = extract_text(&pdf).expect("应抽到文本");
        assert!(t.contains("Hello") && t.contains("World"), "实际: {t:?}");
    }

    /// 字典里的名字/数字**不能**被当成正文 —— 只有显示操作符前的串才算。
    #[test]
    fn 不把非显示字符串当正文() {
        let pdf = b"%PDF-1.4\nstream\n/Title (should not appear) /Foo 1 0 R\nendstream\n";
        assert!(extract_text(pdf).is_none(), "没有 Tj,不该抽出任何正文");
    }

    /// 扫描件(无文本层)必须返回 None,好让调用方给模型一句明确说明,
    /// 而不是塞一段空白让它以为文档是空的。
    #[test]
    fn 无文本层返回none() {
        assert!(extract_text(b"%PDF-1.4\n%%EOF").is_none());
        assert!(extract_text(b"").is_none());
    }

    #[test]
    fn 处理转义与八进制与十六进制串() {
        let pdf = b"%PDF\nstream\n(a\\(b\\)c) Tj (\\101\\102) Tj <414243> Tj\nendstream\n";
        let t = extract_text(pdf).unwrap();
        assert!(t.contains("a(b)c"), "转义括号: {t:?}");
        assert!(t.contains("AB"), "八进制: {t:?}");
        assert!(t.contains("ABC"), "十六进制串: {t:?}");
    }

    /// 解压有上限:不可信 PDF 可以声明天文级尺寸(解压炸弹)。
    #[test]
    fn 解压有上限() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        e.write_all(&vec![b'A'; 64 * 1024 * 1024]).unwrap();
        let comp = e.finish().unwrap();
        let got = inflate(&comp).expect("截断后仍返回内容");
        assert!(got.len() <= 16 * 1024 * 1024, "必须被 16MB 上限截住,实际 {}", got.len());
    }
}
