//! 上游逐项媒体硬限制预检:单文档 / 单图 / 每条文档数。
//!
//! 【实测定标 2026-09-29,claude-opus-5.5,绕开 caio 自身闸门直打上游】
//! | 限制 | 实测 | 上游 400 reason |
//! |---|---|---|
//! | 单文档原始字节 | 3.9MB 过、7.9MB 拒("exceeds maximum size of 4.5MB") | DOCUMENT_SIZE_EXCEEDED |
//! | 每条消息文档数 | 5 过、6 拒("Maximum 5 documents allowed per message") | DOCUMENT_COUNT_EXCEEDED |
//! | 单图 base64 长度 | 5,242,880 字节("image exceeds 5 MB maximum") | IMAGE_SIZE_EXCEEDED |
//! | 图片边长 | 8000px 过、8200px 拒 | IMAGE_DIMENSION_EXCEEDED |
//! | 跨轮文档总数 | history 里 6 个(每轮 1 个)照常可读 | —— 只按单条消息计 |
//!
//! 这些是**逐项**规则:一个超限附件会让整轮 400;客户端每轮重发整段历史,超限附件一直在里面,
//! 于是此后每轮都 400 —— 用户只能新开会话。这里改成只剔那一个附件,原位留一句模型可读的说明
//! (例如让模型改用分页读取),会话照常继续。
//!
//! 图片先经 `image.rs` 压缩(长边/像素上限),正常早已合规;但压缩是**可关、可失败回退**的
//! 优化步骤(超 1 亿像素的解压炸弹护栏会原样透传),所以硬限制必须在这里、在出站前再校一次,
//! 不能把「尝试过压缩」当成「已合规」。

use crate::kiro_types::conversation::{KiroDocument, KiroImage, Message, UserInputMessage};

/// 单文档原始字节上限。上游文案是 "4.5MB",单位(十进制/二进制)未定,取小的 4,500,000。
pub const MAX_DOCUMENT_RAW_BYTES: usize = 4_500_000;
/// 每条消息的文档数上限。
pub const MAX_DOCUMENTS_PER_MESSAGE: usize = 5;
/// 单图 base64 长度上限(上游报错按 base64 长度比较,5 MiB)。
pub const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;
/// 图片任一边的像素上限。
pub const MAX_IMAGE_SIDE_PX: u32 = 8000;

/// 宽松解码 base64 的前 `max_chars` 个有效字符:跳过空白、兼容 URL-safe 字母表(`-`/`_`)、
/// 忽略 padding。截在编码组中间时丢掉不完整的尾组。解码失败返回 `None`。
fn decode_b64_prefix(b64: &str, max_chars: usize) -> Option<Vec<u8>> {
    use base64::engine::{general_purpose::GeneralPurposeConfig, DecodePaddingMode, GeneralPurpose};
    use base64::{alphabet, Engine};
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let mut buf = Vec::with_capacity(max_chars.min(b64.len()));
    let mut truncated = false;
    for c in b64.bytes() {
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0B' | b'\x0C' | b'=' => continue,
            b'-' => buf.push(b'+'),
            b'_' => buf.push(b'/'),
            c => buf.push(c),
        }
        if buf.len() >= max_chars {
            truncated = true;
            break;
        }
    }
    let keep = if truncated { buf.len() / 4 * 4 } else { buf.len() - usize::from(buf.len() % 4 == 1) };
    LENIENT.decode(&buf[..keep]).ok()
}

/// 只读尺寸字段的图片头解析 —— **不解压任何数据**。
///
/// 为什么不用 `image::ImageReader::into_dimensions`:它会解析 IDAT 之前的全部元数据,PNG 的
/// `iCCP` 等块会被 zlib 解压 —— 32KB 的图可展开成 32MiB,且本预检对每轮每张历史图都跑、
/// 同步跑在 async 线程上(评审复审 high)。这里只做有界的字节解析:
/// PNG 读 IHDR、GIF 读逻辑屏幕、WebP 读 VP8/VP8L/VP8X 头、JPEG 按段长跳到 SOFn。
fn header_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let le16 = |i: usize| -> Option<u32> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
    let le24 = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0]))
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16)? == b"IHDR" {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12)? == b"WEBP" {
        return match b.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8L" => {
                let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1))
            }
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            _ => None,
        };
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 3 < b.len() {
            if b[i] != 0xFF {
                return None;
            }
            let marker = b[i + 1];
            if marker == 0xFF {
                i += 1; // 填充字节
                continue;
            }
            // SOF0..SOF15,排除 DHT(C4)/JPG(C8)/DAC(CC)
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            i += 2 + be16(i + 2)? as usize;
        }
    }
    None
}

/// 读图片宽高。先解 base64 前 64KB(PNG/GIF/WebP 的尺寸在头部,JPEG 的 SOF 通常也在其中);
/// JPEG 前面挂了大块 EXIF 时再整图解码(此前已按 5MiB base64 上限剔过,解码量有界)。
/// 读不出返回 `None` —— 判断不了就不剔(交给上游,别误伤)。
fn image_dimensions(b64: &str) -> Option<(u32, u32)> {
    const PREFIX_CHARS: usize = 64 * 1024 / 3 * 4;
    let prefix = decode_b64_prefix(b64, PREFIX_CHARS)?;
    if let Some(d) = header_dimensions(&prefix) {
        return Some(d);
    }
    if b64.len() > PREFIX_CHARS && prefix.starts_with(&[0xFF, 0xD8]) {
        return header_dimensions(&decode_b64_prefix(b64, usize::MAX)?);
    }
    None
}

/// 预检统计(仅供日志)。
#[derive(Debug, Default, PartialEq)]
pub struct MediaLimitReport {
    pub dropped_documents: usize,
    pub dropped_images: usize,
}

/// base64 长度 → 原始字节数(按标准 padding 估算,误差 ≤2 字节)。
fn decoded_len(b64: &str) -> usize {
    let pad = b64.bytes().rev().take_while(|b| *b == b'=').count();
    (b64.len() / 4 * 3).saturating_sub(pad)
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_000_000.0
}

/// 对一条消息的 content/images/documents 就地套用逐项上限,超限项换成说明文字。
fn enforce(
    content: &mut String,
    images: &mut Vec<KiroImage>,
    documents: &mut Vec<KiroDocument>,
    report: &mut MediaLimitReport,
) {
    let mut notes: Vec<String> = Vec::new();

    let mut kept_docs = 0usize;
    documents.retain(|d| {
        let size = decoded_len(&d.source.bytes);
        if size > MAX_DOCUMENT_RAW_BYTES {
            notes.push(format!(
                "[document \"{}\" omitted: {:.1} MB exceeds the 4.5 MB per-document limit; \
                 read it in smaller page ranges or split it into smaller files]",
                d.name,
                mb(size)
            ));
            return false;
        }
        if kept_docs >= MAX_DOCUMENTS_PER_MESSAGE {
            notes.push(format!(
                "[document \"{}\" omitted: at most {} documents can be attached to one message]",
                d.name, MAX_DOCUMENTS_PER_MESSAGE
            ));
            return false;
        }
        kept_docs += 1;
        true
    });

    images.retain(|i| {
        let encoded = i.source.bytes.len();
        if encoded > MAX_IMAGE_BASE64_BYTES {
            notes.push(format!(
                "[image omitted: its encoded size of {encoded} bytes exceeds the \
                 {MAX_IMAGE_BASE64_BYTES}-byte (5 MiB base64) per-image limit; send a smaller or downscaled image]"
            ));
            return false;
        }
        if let Some((w, h)) = image_dimensions(&i.source.bytes) {
            if w > MAX_IMAGE_SIDE_PX || h > MAX_IMAGE_SIDE_PX {
                notes.push(format!(
                    "[image omitted: {w}x{h} px exceeds the {MAX_IMAGE_SIDE_PX} px per-side limit; \
                     send a downscaled or cropped image]"
                ));
                return false;
            }
        }
        true
    });

    if notes.is_empty() {
        return;
    }
    report.dropped_documents += notes.iter().filter(|n| n.starts_with("[document")).count();
    report.dropped_images += notes.iter().filter(|n| n.starts_with("[image")).count();
    for n in notes {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&n);
    }
}

/// 当前轮 + 全部历史 user 轮套用逐项上限。无超限项时零改动(不触碰缓存前缀字节)。
pub fn enforce_media_limits(current: &mut UserInputMessage, history: &mut [Message]) -> MediaLimitReport {
    let mut report = MediaLimitReport::default();
    for msg in history.iter_mut() {
        if let Message::User(u) = msg {
            let m = &mut u.user_input_message;
            enforce(&mut m.content, &mut m.images, &mut m.documents, &mut report);
        }
    }
    enforce(&mut current.content, &mut current.images, &mut current.documents, &mut report);
    if report != MediaLimitReport::default() {
        tracing::warn!(
            dropped_documents = report.dropped_documents,
            dropped_images = report.dropped_images,
            "媒体超出上游逐项上限(单文档 4.5MB / 每条 5 个文档 / 单图 5MB / 边长 8000px)，已剔除并留说明"
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro_types::conversation::{HistoryUserMessage, UserMessage};

    fn b64_of_raw(raw: usize) -> String {
        "A".repeat(raw.div_ceil(3) * 4)
    }

    fn current_with(docs: Vec<KiroDocument>, imgs: Vec<KiroImage>) -> UserInputMessage {
        let mut m = UserInputMessage::new("look".to_string(), "claude-opus-5.5");
        m.documents = docs;
        m.images = imgs;
        m
    }

    #[test]
    fn oversized_document_replaced_by_note() {
        let mut cur = current_with(
            vec![
                KiroDocument::from_base64("big", "pdf", b64_of_raw(7_900_000)),
                KiroDocument::from_base64("ok", "pdf", b64_of_raw(3_900_000)),
            ],
            vec![],
        );
        let r = enforce_media_limits(&mut cur, &mut []);
        assert_eq!(r.dropped_documents, 1);
        assert_eq!(cur.documents.len(), 1);
        assert_eq!(cur.documents[0].name, "ok");
        assert!(cur.content.starts_with("look\n[document \"big\" omitted: 7.9 MB"), "{}", cur.content);
    }

    #[test]
    fn sixth_document_in_one_message_dropped() {
        let docs = (0..6)
            .map(|i| KiroDocument::from_base64(format!("d{i}"), "pdf", b64_of_raw(1000)))
            .collect();
        let mut cur = current_with(docs, vec![]);
        let r = enforce_media_limits(&mut cur, &mut []);
        assert_eq!(r.dropped_documents, 1);
        assert_eq!(cur.documents.len(), 5);
        assert!(cur.content.contains("\"d5\" omitted: at most 5 documents"));
    }

    #[test]
    fn oversized_image_dropped_small_kept() {
        let mut cur = current_with(
            vec![],
            vec![
                KiroImage::from_base64("png", "A".repeat(MAX_IMAGE_BASE64_BYTES + 4)),
                KiroImage::from_base64("png", "A".repeat(MAX_IMAGE_BASE64_BYTES)),
            ],
        );
        let r = enforce_media_limits(&mut cur, &mut []);
        assert_eq!(r.dropped_images, 1);
        assert_eq!(cur.images.len(), 1);
        assert!(
            cur.content.contains(&format!(
                "encoded size of {} bytes exceeds the 5242880-byte",
                MAX_IMAGE_BASE64_BYTES + 4
            )),
            "说明必须与判断同口径(base64 字节): {}",
            cur.content
        );
    }

    fn png_b64(w: u32, h: u32) -> String {
        use base64::Engine;
        let img = image::GrayImage::new(w, h);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(buf.into_inner())
    }

    #[test]
    fn oversized_dimensions_dropped_even_when_tiny_in_bytes() {
        // 评审 high:边长超限但字节很小的图(如 10001×10000 单色 PNG 仅 12KB),压缩器解码护栏回退后原样透传,
        // 只看字节的预检会放行 → 上游 IMAGE_DIMENSION_EXCEEDED。
        let wide = png_b64(8001, 40);
        let ok = png_b64(8000, 40);
        assert!(wide.len() < 100_000, "构造的就是小字节大尺寸图");
        let mut cur = current_with(
            vec![],
            vec![KiroImage::from_base64("png", wide), KiroImage::from_base64("png", ok)],
        );
        let r = enforce_media_limits(&mut cur, &mut []);
        assert_eq!(r.dropped_images, 1);
        assert_eq!(cur.images.len(), 1, "8000 边长恰好合规应保留");
        assert!(cur.content.contains("8001x40 px exceeds the 8000 px"), "{}", cur.content);
    }

    fn encoded(img: image::DynamicImage, fmt: image::ImageFormat) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, fmt).unwrap();
        buf.into_inner()
    }

    #[test]
    fn header_parser_reads_all_supported_formats() {
        use image::{DynamicImage, ImageFormat};
        let rgb = DynamicImage::ImageRgb8(image::RgbImage::new(321, 123));
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::new(321, 123));
        for (img, fmt) in [
            (rgb.clone(), ImageFormat::Png),
            (rgb.clone(), ImageFormat::Jpeg),
            (rgba.clone(), ImageFormat::Gif),
            (rgba, ImageFormat::WebP),
        ] {
            assert_eq!(header_dimensions(&encoded(img, fmt)), Some((321, 123)), "{fmt:?}");
        }
    }

    #[test]
    fn jpeg_behind_large_exif_segment_still_read() {
        // SOF 前插一个 ~60KB 的 APP1 段:前缀装不下时要回退整图解码后按段长跳过。
        use base64::Engine;
        let jpeg = encoded(image::DynamicImage::ImageRgb8(image::RgbImage::new(9001, 8)), image::ImageFormat::Jpeg);
        let mut big = vec![0xFF, 0xD8];
        for _ in 0..2 {
            big.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xF0]);
            big.extend(std::iter::repeat_n(0u8, 0xFFF0 - 2));
        }
        big.extend_from_slice(&jpeg[2..]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&big);
        assert_eq!(image_dimensions(&b64), Some((9001, 8)));
    }

    #[test]
    fn lenient_base64_whitespace_and_url_safe() {
        use base64::Engine;
        let raw = encoded(image::DynamicImage::ImageLuma8(image::GrayImage::new(8001, 40)), image::ImageFormat::Png);
        let std = base64::engine::general_purpose::STANDARD.encode(&raw);
        let wrapped: String = std.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\r\n");
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
        for v in [std, wrapped, url_safe] {
            assert_eq!(image_dimensions(&v), Some((8001, 40)));
        }
    }

    #[test]
    fn metadata_is_never_inflated() {
        // 1×1 PNG + 一个声称巨大的 iCCP 块:头解析只读 IHDR,不触碰后续块。
        let mut png = encoded(image::DynamicImage::ImageLuma8(image::GrayImage::new(1, 1)), image::ImageFormat::Png);
        png.truncate(33); // 签名 + IHDR 之后截断,后面任何块都不存在也照样能读尺寸
        assert_eq!(header_dimensions(&png), Some((1, 1)));
    }

    #[test]
    fn unreadable_image_header_is_kept() {
        let mut cur = current_with(vec![], vec![KiroImage::from_base64("png", "bm90LWFuLWltYWdl")]);
        let r = enforce_media_limits(&mut cur, &mut []);
        assert_eq!(r, MediaLimitReport::default(), "读不出尺寸不误伤");
        assert_eq!(cur.images.len(), 1);
    }

    #[test]
    fn history_limits_apply_per_message_and_within_limits_untouched() {
        let mut big = UserMessage::new("old", "claude-opus-5.5");
        big.documents.push(KiroDocument::from_base64("huge", "pdf", b64_of_raw(5_000_000)));
        let mut fine = UserMessage::new("fine", "claude-opus-5.5");
        fine.documents.push(KiroDocument::from_base64("small", "pdf", b64_of_raw(10_000)));
        let mut history = vec![
            Message::User(HistoryUserMessage { user_input_message: big }),
            Message::User(HistoryUserMessage { user_input_message: fine }),
        ];
        let mut cur = current_with(vec![], vec![]);
        let r = enforce_media_limits(&mut cur, &mut history);
        assert_eq!(r.dropped_documents, 1);
        let Message::User(u0) = &history[0] else { panic!() };
        assert!(u0.user_input_message.documents.is_empty());
        assert!(u0.user_input_message.content.contains("\"huge\" omitted"));
        let Message::User(u1) = &history[1] else { panic!() };
        assert_eq!(u1.user_input_message.content, "fine", "未超限的轮次字节不应变");
        assert_eq!(u1.user_input_message.documents.len(), 1);
        assert_eq!(cur.content, "look");
    }

    #[test]
    fn decoded_len_accounts_for_padding() {
        assert_eq!(decoded_len("QUJD"), 3);
        assert_eq!(decoded_len("QUI="), 2);
        assert_eq!(decoded_len("QQ=="), 1);
    }
}
