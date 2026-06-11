//! 图像压缩模块 —— 🔵 移植 kiro.rs `src/image.rs`(其源头为 xkiro.rs,经生产审查)。
//!
//! 多模态请求里图片以 base64 原样透传会显著撑大请求体(撞上游字节上限/抬高成本),
//! 且恶意构造的「解压炸弹」(几 KB 文件、header 声明天文级尺寸)在解码时可 OOM 整个
//! worker 进程。本模块按四档阈值缩放 + 必要时重编码,并自带**解码前**护栏。
//!
//! 缩放规则(对齐 Anthropic 官方):
//! 1. 长边超过 `max_long_edge` → 等比缩放;
//! 2. 总像素超过 `max_pixels` → 等比缩放;
//! 3. 多图模式(图片数 ≥ `multi_threshold`)用独立的、可更严的像素上限。
//!
//! 失败策略:解码/编码任何一步失败都**回退原图**(绝不丢图),仅记 warning。
//! CPU 密集(解码/缩放)统一走 `spawn_blocking` + 全局信号量背压。
//!
//! 与母本差异:本项目 provider 的权威请求是 `serde_json::Value`(Anthropic body),
//! 故收集/写回直接操作 JSON 树,而非类型化 Message;配置类型挪到
//! [`gw_core::config::ImageConfig`](system.yaml `image` 段)。

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use gw_core::config::ImageConfig;
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

/// 全局图像压缩并发上限(背压):限制同时进行的压缩任务数,避免高并发多模态请求
/// 打满 tokio blocking 线程池、饿死其他 blocking 操作。上限 = CPU 核数(clamp 2–8)。
/// 超上限时**等待许可**(而非透传/丢弃):压缩仍发生,只是排队,控制峰值资源。
fn image_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
            .clamp(2, 8);
        Semaphore::new(n)
    })
}

/// 文件过大强制重编码阈值(200KB):尺寸合规但字节大的高质量图也压一遍。
const FORCE_REENCODE_BYTES: usize = 200_000;

/// 解码绝对像素上限(宽×高,约 1 亿像素 ≈ 10000×10000)。
///
/// **安全护栏(防解压炸弹/OOM)**:几 KB 的恶意图片可在 header 声明天文级尺寸
/// (如 64000×64000),`load_from_memory` 会按源尺寸先解出整张像素面 → 进程 OOM。
/// 缩放发生在解码**之后**,救不了;必须在解码前用 header 尺寸拦截:超限直接
/// 回退原图透传(不解码)。1 亿像素对正常图片(含高清照片)绰绰有余。
const MAX_DECODE_PIXELS: u64 = 100_000_000;

/// 解码原始字节上限(base64 解码后,64MB)。超大输入同样直接回退,避免在巨型 buffer 上解码。
const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

/// 图像压缩结果。
struct ProcessedImage {
    /// 处理后(或回退的原始)base64 数据。
    data: String,
    /// 最终格式("jpeg"/"png"/"gif"/"webp"),PNG 大文件兜底可能转 jpeg。
    format: String,
}

/// 压缩单张图片:解码 → 按四档缩放 → 必要时重编码,全程失败回退原图。
fn process_image(
    base64_data: &str,
    format: &str,
    cfg: &ImageConfig,
    image_count: usize,
) -> ProcessedImage {
    match try_process(base64_data, format, cfg, image_count) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, format, "图像压缩失败,回退原图透传");
            ProcessedImage {
                data: base64_data.to_string(),
                format: format.to_string(),
            }
        }
    }
}

/// 内部实现:任何步骤出错返回 Err,由 [`process_image`] 兜底回退。
fn try_process(
    base64_data: &str,
    format: &str,
    cfg: &ImageConfig,
    image_count: usize,
) -> Result<ProcessedImage, String> {
    let bytes = BASE64
        .decode(base64_data)
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let original_len = bytes.len();

    // 安全护栏 1:原始字节过大直接拒绝(避免在巨型 buffer 上解码)。
    if original_len > MAX_DECODE_BYTES {
        return Err(format!(
            "原始字节 {original_len} 超过解码上限 {MAX_DECODE_BYTES},回退原图"
        ));
    }

    // 先只读图片头拿尺寸,避免不必要的全量解码。
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| format!("格式识别失败: {e}"))?;
    let (ow, oh) = reader
        .into_dimensions()
        .map_err(|e| format!("读取尺寸失败: {e}"))?;

    // 安全护栏 2(关键,防解压炸弹/OOM):用 header 尺寸在**解码前**拦截天文级图片。
    let src_pixels = (ow as u64) * (oh as u64);
    if src_pixels > MAX_DECODE_PIXELS {
        return Err(format!(
            "源尺寸 {ow}x{oh}={src_pixels} 像素超过解码上限 {MAX_DECODE_PIXELS},回退原图"
        ));
    }

    // 多图档:图片数达阈值用更严的多图像素上限。
    let max_pixels = if image_count >= cfg.multi_threshold {
        cfg.max_pixels_multi
    } else {
        cfg.max_pixels_single
    };
    let (tw, th) = apply_scaling_rules(ow, oh, cfg.max_long_edge, max_pixels);
    let needs_resize = tw != ow || th != oh;

    // GIF(动图常"像素小但字节大")和大文件即使不缩放也重编码一遍。
    let force_gif = format.eq_ignore_ascii_case("gif");
    let force_large = original_len > FORCE_REENCODE_BYTES;

    if !(needs_resize || force_gif || force_large) {
        // 完全合规:原样透传。
        return Ok(ProcessedImage {
            data: base64_data.to_string(),
            format: format.to_string(),
        });
    }

    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片加载失败: {e}"))?;
    let processed = if needs_resize {
        img.resize(tw, th, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // 用原格式编码。
    let (mut best_data, mut best_len) = encode_image(&processed, format)?;
    let mut best_format = format.to_string();

    // PNG 大文件兜底:尝试 JPEG(有损、无 alpha),取更小者。
    if force_large && format.eq_ignore_ascii_case("png") {
        let rgb = DynamicImage::ImageRgb8(processed.to_rgb8());
        if let Ok((jpeg_data, jpeg_len)) = encode_image(&rgb, "jpeg") {
            if jpeg_len < best_len {
                best_data = jpeg_data;
                best_len = jpeg_len;
                best_format = "jpeg".to_string();
            }
        }
    }

    // 回退保护:无论是否缩放,处理后字节没变小就透传原图——不变量「绝不发送比
    // 客户端原始更大的图」(重编码偶尔会变大:已优化格式、小图轻微缩放等)。
    if best_len >= original_len {
        return Ok(ProcessedImage {
            data: base64_data.to_string(),
            format: format.to_string(),
        });
    }

    Ok(ProcessedImage {
        data: best_data,
        format: best_format,
    })
}

/// 应用缩放规则:长边限制 + 总像素限制(等比)。0 阈值视为不限制。
fn apply_scaling_rules(width: u32, height: u32, max_long_edge: u32, max_pixels: u32) -> (u32, u32) {
    let mut w = width as f64;
    let mut h = height as f64;

    let long_edge = w.max(h);
    if max_long_edge > 0 && long_edge > max_long_edge as f64 {
        let scale = max_long_edge as f64 / long_edge;
        w *= scale;
        h *= scale;
    }

    let pixels = w * h;
    if max_pixels > 0 && pixels > max_pixels as f64 {
        let scale = (max_pixels as f64 / pixels).sqrt();
        w *= scale;
        h *= scale;
    }

    (w.floor().max(1.0) as u32, h.floor().max(1.0) as u32)
}

/// 编码为指定格式并返回 (base64, 字节数)。
fn encode_image(img: &DynamicImage, format: &str) -> Result<(String, usize), String> {
    let mut buffer = Cursor::new(Vec::new());
    let image_format = match format {
        "jpeg" | "jpg" => ImageFormat::Jpeg,
        "png" => ImageFormat::Png,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::WebP,
        other => return Err(format!("不支持的格式: {other}")),
    };
    img.write_to(&mut buffer, image_format)
        .map_err(|e| format!("编码失败: {e}"))?;
    let encoded = buffer.into_inner();
    let len = encoded.len();
    Ok((BASE64.encode(encoded), len))
}

/// media_type → 压缩支持的格式名。仅这四种可解码缩放,其余返回 None(不压缩)。
fn format_from_media_type(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/jpeg" => Some("jpeg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// 一处待压缩图片的定位(messages JSON 树内索引)+ 原始数据。
struct ImageRef {
    msg_idx: usize,
    block_idx: usize,
    data: String,
    format: &'static str,
}

/// 异步预处理:压缩 Anthropic body `messages` 里所有 base64 图片,原地写回。
///
/// 设计:先同步遍历 JSON 收集所有图片(定位 + data + 格式),再把 CPU 密集的
/// 解码/缩放丢进 [`tokio::task::spawn_blocking`](信号量背压),压缩后按定位写回。
/// `enabled=false` 或无图片时直接返回。任何单图失败回退原图(process_image 内部兜底)。
///
/// 只处理顶层 content 数组里的 `image` 块;tool_result 内嵌图片(如 browser 截图)
/// 由 converter 在转换期单独抽取,体量小、暂不在此压缩(对齐母本取舍)。
pub async fn compress_body_images(body: &mut serde_json::Value, cfg: &ImageConfig) {
    if !cfg.enabled {
        return;
    }
    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return;
    };

    // 1. 同步收集所有 base64 图片(定位 + data + 格式)。
    let mut refs: Vec<ImageRef> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        let Some(arr) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, block) in arr.iter().enumerate() {
            if block.get("type").and_then(|v| v.as_str()) != Some("image") {
                continue;
            }
            let Some(source) = block.get("source") else { continue };
            if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
                continue;
            }
            let Some(data) = source.get("data").and_then(|v| v.as_str()) else {
                continue;
            };
            let media_type = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(format) = format_from_media_type(media_type) else {
                continue;
            };
            refs.push(ImageRef {
                msg_idx: mi,
                block_idx: bi,
                data: data.to_string(),
                format,
            });
        }
    }

    if refs.is_empty() {
        return;
    }

    let image_count = refs.len();
    let cfg = *cfg;

    // 2. 背压:先取并发许可(超上限排队),再把 CPU 密集压缩丢到 blocking 线程池。
    //    许可在 spawn_blocking 期间持有,task 结束自动释放。acquire 失败(信号量
    //    被关闭,正常不会发生)则跳过压缩、透传原图。
    let _permit = match image_semaphore().acquire().await {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!("图像压缩信号量已关闭,跳过压缩透传原图");
            return;
        }
    };
    let processed = tokio::task::spawn_blocking(move || {
        refs.into_iter()
            .map(|r| {
                let out = process_image(&r.data, r.format, &cfg, image_count);
                (r.msg_idx, r.block_idx, out)
            })
            .collect::<Vec<_>>()
    })
    .await;

    let processed = match processed {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "图像压缩任务 panic,全部回退原图");
            return;
        }
    };

    // 3. 按定位写回(更新 data + media_type,因 PNG 兜底可能转 jpeg)。
    for (mi, bi, out) in processed {
        if let Some(source) = body
            .get_mut("messages")
            .and_then(|m| m.get_mut(mi))
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.get_mut(bi))
            .and_then(|b| b.get_mut("source"))
        {
            source["data"] = serde_json::Value::String(out.data);
            source["media_type"] = serde_json::Value::String(format!("image/{}", out.format));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 image crate 生成一张 w×h 的纯色 PNG,返回 base64。
    fn make_png(w: u32, h: u32) -> String {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([120, 80, 40]),
        ));
        let (b64, _) = encode_image(&img, "png").unwrap();
        b64
    }

    fn b64_dims(b64: &str) -> (u32, u32) {
        let bytes = BASE64.decode(b64).unwrap();
        image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    }

    #[test]
    fn scaling_rules_long_edge() {
        // 长边 8000 限到 4000,等比 → 4000×2000
        assert_eq!(apply_scaling_rules(8000, 4000, 4000, u32::MAX), (4000, 2000));
    }

    #[test]
    fn scaling_rules_pixels() {
        // 2000×2000=4M,限 1M → scale=0.5 → 1000×1000
        assert_eq!(apply_scaling_rules(2000, 2000, u32::MAX, 1_000_000), (1000, 1000));
    }

    #[test]
    fn scaling_rules_no_change_when_within_limits() {
        assert_eq!(apply_scaling_rules(800, 600, 4000, 4_000_000), (800, 600));
    }

    #[test]
    fn scaling_rules_zero_means_unlimited() {
        assert_eq!(apply_scaling_rules(9999, 9999, 0, 0), (9999, 9999));
    }

    #[test]
    fn process_oversized_png_gets_resized() {
        let big = make_png(6000, 3000); // 18M px,长边 6000 > 4000
        let cfg = ImageConfig::default();
        let out = process_image(&big, "png", &cfg, 1);
        let (w, h) = b64_dims(&out.data);
        // 两条规则叠加:先长边 6000→4000(4000×2000=8M px),再总像素 8M→4M。
        assert!(w <= 4000 && h <= 4000, "长边应 <=4000, got {w}x{h}");
        assert!(
            (w as u64) * (h as u64) <= 4_000_000,
            "总像素应 <=4M, got {}",
            (w as u64) * (h as u64)
        );
        assert!((w as f64 / h as f64 - 2.0).abs() < 0.05, "宽高比应约 2:1, got {w}x{h}");
    }

    #[test]
    fn process_small_png_passthrough() {
        // 小图(尺寸合规 + 字节 < 200KB)原样透传。
        let small = make_png(50, 50);
        let cfg = ImageConfig::default();
        let out = process_image(&small, "png", &cfg, 1);
        assert_eq!(out.data, small, "小图应原样透传");
    }

    /// 伪造一个 IHDR 声明超大尺寸的 PNG(仅 header,几十字节),不含真实像素数据。
    /// `into_dimensions()` 读 IHDR 即可拿到尺寸,无需全量解码 → 触发解码前护栏。
    fn forged_huge_png(width: u32, height: u32) -> String {
        let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB
        let crc = crc32_png(&ihdr);
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc.to_be_bytes());
        BASE64.encode(&png)
    }

    /// 极简 PNG CRC32(IEEE)。仅测试用。
    fn crc32_png(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn decode_bomb_oversized_dimensions_falls_back() {
        // 64000×64000 ≈ 41 亿像素,远超 MAX_DECODE_PIXELS(1 亿)。
        // 必须在解码前(读 header 后)拦截 → 回退原图,绝不进 load_from_memory(否则 OOM)。
        let bomb = forged_huge_png(64000, 64000);
        let cfg = ImageConfig::default();
        let out = process_image(&bomb, "png", &cfg, 1);
        assert_eq!(out.data, bomb, "解压炸弹应被拦截并回退原图(不解码)");
    }

    #[test]
    fn oversized_bytes_falls_back() {
        let mut blob = vec![0u8; MAX_DECODE_BYTES + 1];
        blob[0] = 0x89;
        let b64 = BASE64.encode(&blob);
        let cfg = ImageConfig::default();
        let out = process_image(&b64, "png", &cfg, 1);
        assert_eq!(out.data, b64, "超字节上限应回退原图");
    }

    #[test]
    fn process_invalid_base64_falls_back() {
        let cfg = ImageConfig::default();
        let out = process_image("!!!not-base64!!!", "png", &cfg, 1);
        assert_eq!(out.data, "!!!not-base64!!!", "解码失败应回退原数据");
    }

    #[test]
    fn multi_threshold_uses_stricter_pixels() {
        let img = make_png(2000, 2000);
        let cfg = ImageConfig {
            enabled: true,
            max_long_edge: 4000,
            max_pixels_single: 4_000_000,
            max_pixels_multi: 1_000_000,
            multi_threshold: 2,
        };
        // 单图模式(count=1 < 2):4M 不超 single 上限 → 不缩。
        let single = process_image(&img, "png", &cfg, 1);
        assert_eq!(b64_dims(&single.data), (2000, 2000));
        // 多图模式(count=2 >= 2):超 multi 1M 上限 → 缩到 1000×1000。
        let multi = process_image(&img, "png", &cfg, 2);
        assert_eq!(b64_dims(&multi.data), (1000, 1000));
    }

    #[tokio::test]
    async fn compress_body_disabled_noop() {
        let big = make_png(6000, 3000);
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
            ]}]
        });
        let cfg = ImageConfig { enabled: false, ..Default::default() };
        let before = body.clone();
        compress_body_images(&mut body, &cfg).await;
        assert_eq!(body, before, "关闭时原样不动");
    }

    #[tokio::test]
    async fn compress_body_resizes_in_place() {
        // 4800×600:长边 4800>4000 触发缩放,总像素 2.88M<4M 不触发像素规则。
        let big = make_png(4800, 600);
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
            ]}]
        });
        let cfg = ImageConfig::default();
        compress_body_images(&mut body, &cfg).await;
        let new_data = body["messages"][0]["content"][1]["source"]["data"]
            .as_str()
            .unwrap();
        assert_ne!(new_data, big, "大图应被压缩替换");
        let (w, h) = b64_dims(new_data);
        assert_eq!((w, h), (4000, 500), "长边缩到 4000, 等比 → 4000×500");
        assert_eq!(body["messages"][0]["content"][0]["text"], "look", "text 块不受影响");
    }

    #[tokio::test]
    async fn compress_body_string_content_and_no_messages_noop() {
        // content 为字符串 / 无 messages:都不应 panic、不应改动。
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": "plain text"}]
        });
        let before = body.clone();
        compress_body_images(&mut body, &ImageConfig::default()).await;
        assert_eq!(body, before);
        let mut empty = serde_json::json!({});
        compress_body_images(&mut empty, &ImageConfig::default()).await;
        assert_eq!(empty, serde_json::json!({}));
    }
}
