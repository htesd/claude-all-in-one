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

/// 图片在 `messages` JSON 树里的位置。
#[derive(Debug, Clone, Copy)]
struct Slot {
    msg_idx: usize,
    block_idx: usize,
    /// `Some(n)` = 该块是 tool_result,图在它的 `content[n]`。
    nested_idx: Option<usize>,
    /// 图以 OpenAI `image_url` data URL 形态出现(写回时要重组 URL)。
    data_url: bool,
}

/// 一处待压缩图片:定位 + 原始 base64 + 格式。
struct ImageRef {
    slot: Slot,
    data: String,
    format: &'static str,
}

/// 从一个内容块读出可压缩的 base64 图片:`image`(base64 源)或 `image_url`(data URL)。
fn read_image_block(block: &serde_json::Value) -> Option<(String, &'static str, bool)> {
    match block.get("type").and_then(|v| v.as_str())? {
        "image" => {
            let source = block.get("source")?;
            if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
                return None;
            }
            let data = source.get("data").and_then(|v| v.as_str())?;
            let media_type = source.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
            Some((data.to_string(), format_from_media_type(media_type)?, false))
        }
        "image_url" => {
            let field = block.get("image_url")?;
            let url = field.as_str().or_else(|| field.get("url").and_then(|u| u.as_str()))?;
            let (meta, data) = url.strip_prefix("data:")?.split_once(',')?;
            let media_type = meta.strip_suffix(";base64")?.to_ascii_lowercase();
            Some((data.to_string(), format_from_media_type(&media_type)?, true))
        }
        _ => None,
    }
}

/// 把压缩结果写回一个内容块(与 [`read_image_block`] 的两种形态对应)。
fn write_image_block(block: &mut serde_json::Value, data_url: bool, out: &ProcessedImage) {
    if data_url {
        let url = format!("data:image/{};base64,{}", out.format, out.data);
        match block.get_mut("image_url") {
            Some(f) if f.is_string() => *f = serde_json::Value::String(url),
            Some(f) => f["url"] = serde_json::Value::String(url),
            None => {}
        }
    } else if let Some(source) = block.get_mut("source") {
        source["data"] = serde_json::Value::String(out.data.clone());
        source["media_type"] = serde_json::Value::String(format!("image/{}", out.format));
    }
}

/// 收集 body 里所有可压缩图片:顶层 content 块,以及 tool_result.content 里的嵌套块。
fn collect_image_refs(messages: &[serde_json::Value]) -> Vec<ImageRef> {
    let mut refs = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        let Some(arr) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, block) in arr.iter().enumerate() {
            if let Some((data, format, data_url)) = read_image_block(block) {
                let slot = Slot { msg_idx: mi, block_idx: bi, nested_idx: None, data_url };
                refs.push(ImageRef { slot, data, format });
                continue;
            }
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(inner) = block.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for (ni, item) in inner.iter().enumerate() {
                if let Some((data, format, data_url)) = read_image_block(item) {
                    let slot = Slot { msg_idx: mi, block_idx: bi, nested_idx: Some(ni), data_url };
                    refs.push(ImageRef { slot, data, format });
                }
            }
        }
    }
    refs
}

/// 压缩结果记忆(进程内,按输入字节 + 参数寻址)。
///
/// 为什么要有:Claude Code 每一轮都把整段历史(含全部截图)重发一次,长会话 40~100 张图
/// 很常见。每轮全部重新解码/缩放/编码是纯浪费(单张数十~上百 ms CPU,直接加在首字延迟上)。
/// 压缩是确定性的,同一张图 + 同一组参数结果恒定,记住即可 —— 也顺带保证跨轮字节稳定。
///
/// 只存"被替换"的结果;原样透传(含失败回退)只记一个标记,不占数据内存。
/// 总量按结果字节封顶,超限按插入顺序淘汰(FIFO 足够:会话内的图总是整批重复出现)。
mod memo {
    use super::ProcessedImage;
    use parking_lot::Mutex;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, OnceLock};

    /// 结果数据 + 索引的估算总量上限。
    const MAX_BYTES: usize = 128 * 1024 * 1024;
    /// 条目数硬上限:大量「原样透传」的小条目只占索引,光按数据字节记账挡不住(评审 medium)。
    const MAX_ENTRIES: usize = 20_000;
    /// 每条目的保守常驻开销:map 与 FIFO 各一份 32B 键、Outcome、哈希表控制字节与空桶、Arc 头。
    const ENTRY_OVERHEAD: usize = 256;

    #[derive(Clone)]
    pub(super) enum Outcome {
        Unchanged,
        Replaced(Arc<ProcessedImage>),
    }

    #[derive(Default)]
    struct Memo {
        map: HashMap<[u8; 32], Outcome>,
        order: VecDeque<[u8; 32]>,
        bytes: usize,
    }

    fn memo() -> &'static Mutex<Memo> {
        static M: OnceLock<Mutex<Memo>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(Memo::default()))
    }

    fn cost(o: &Outcome) -> usize {
        match o {
            Outcome::Unchanged => ENTRY_OVERHEAD,
            Outcome::Replaced(p) => p.data.len() + p.format.len() + ENTRY_OVERHEAD,
        }
    }

    pub(super) fn get(key: &[u8; 32]) -> Option<Outcome> {
        memo().lock().map.get(key).cloned()
    }

    pub(super) fn put(key: [u8; 32], outcome: Outcome) {
        let mut m = memo().lock();
        if m.map.contains_key(&key) {
            return;
        }
        m.bytes += cost(&outcome);
        m.map.insert(key, outcome);
        m.order.push_back(key);
        let mut evicted = false;
        while m.bytes > MAX_BYTES || m.map.len() > MAX_ENTRIES {
            let Some(old) = m.order.pop_front() else { break };
            if let Some(o) = m.map.remove(&old) {
                m.bytes -= cost(&o);
            }
            evicted = true;
        }
        // remove/pop 不缩容:淘汰后若容量远大于实际条目,归还多余容量,免得峰值容量永久驻留。
        if evicted && m.map.capacity() > 4 * m.map.len() + 1024 {
            m.map.shrink_to_fit();
            m.order.shrink_to_fit();
        }
    }

    #[cfg(test)]
    pub(super) fn stats() -> (usize, usize) {
        let m = memo().lock();
        (m.map.len(), m.bytes)
    }

    #[cfg(test)]
    pub(super) const TEST_MAX_ENTRIES: usize = MAX_ENTRIES;
}

/// 记忆键:输入 base64 + 格式 + 影响结果的全部参数(含单/多图档)。
fn memo_key(data: &str, format: &str, cfg: &ImageConfig, image_count: usize) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let max_pixels = if image_count >= cfg.multi_threshold {
        cfg.max_pixels_multi
    } else {
        cfg.max_pixels_single
    };
    let mut h = Sha256::new();
    h.update(format.as_bytes());
    h.update(cfg.max_long_edge.to_le_bytes());
    h.update(max_pixels.to_le_bytes());
    h.update(data.as_bytes());
    h.finalize().into()
}

/// 每条消息所属的「轮次」编号:相邻的 user 消息同属一轮,其余角色各自成轮。
fn user_turn_index(messages: &[serde_json::Value]) -> Vec<usize> {
    let mut out = Vec::with_capacity(messages.len());
    let (mut turn, mut prev_user) = (0usize, false);
    for m in messages {
        let is_user = m.get("role").and_then(|r| r.as_str()) == Some("user");
        if !(is_user && prev_user) {
            turn += 1;
        }
        out.push(turn);
        prev_user = is_user;
    }
    out
}

/// 压缩一张图,命中记忆直接返回;`None` = 原样透传(无需写回)。
fn process_image_memo(
    data: &str,
    format: &str,
    cfg: &ImageConfig,
    image_count: usize,
) -> Option<std::sync::Arc<ProcessedImage>> {
    let key = memo_key(data, format, cfg, image_count);
    if let Some(hit) = memo::get(&key) {
        return match hit {
            memo::Outcome::Unchanged => None,
            memo::Outcome::Replaced(p) => Some(p),
        };
    }
    let out = process_image(data, format, cfg, image_count);
    let outcome = if out.data == data && out.format == format {
        memo::Outcome::Unchanged
    } else {
        memo::Outcome::Replaced(std::sync::Arc::new(out))
    };
    memo::put(key, outcome.clone());
    match outcome {
        memo::Outcome::Unchanged => None,
        memo::Outcome::Replaced(p) => Some(p),
    }
}

/// 异步预处理:压缩 Anthropic body `messages` 里所有 base64 图片,原地写回。
///
/// 覆盖范围:顶层 `image` 块、**tool_result 内嵌图片**、OpenAI `image_url` data URL。
/// 生产实测(2026-09-29,2000 条请求)5607 张图里 5039 张在 tool_result 里(Claude Code 的
/// Read 读截图、browser 截图)—— 旧实现只压顶层,以"体量小"为由跳过嵌套图,实际它们才是
/// 请求体膨胀的主因,也会撞上游单图 5MB / 边长 8000px 硬限。
///
/// 设计:先同步遍历 JSON 收集所有图片,再把 CPU 密集的解码/缩放丢进
/// [`tokio::task::spawn_blocking`](信号量背压),压缩后按定位写回。同一张图跨轮只压一次
/// (见 [`memo`])。`enabled=false` 或无图片时直接返回。任何单图失败回退原图。
pub async fn compress_body_images(body: &mut serde_json::Value, cfg: &ImageConfig) {
    if !cfg.enabled {
        return;
    }
    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return;
    };

    // 1. 同步收集所有 base64 图片(定位 + data + 格式)。
    let refs = collect_image_refs(messages);
    if refs.is_empty() {
        return;
    }

    // 单/多图档按**所在轮次自己的图片数**判定,不按全请求:历史轮次的内容跨轮不变,按它算
    // 才能保证旧截图的压缩结果不因后来追加的图片而改变(否则缓存前缀从旧图处断裂,评审 medium)。
    // 「轮次」= 连续 user 消息组 —— 与 converter 合并口径一致(历史 merge_user_messages、
    // 当前轮 current_user_message_range 都把相邻 user 消息并成一条上游消息),否则客户端把
    // 同一轮拆成几条发就能绕过多图档(评审复审 medium)。
    let turn_of = user_turn_index(messages);
    let mut per_turn: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for r in &refs {
        *per_turn.entry(turn_of[r.slot.msg_idx]).or_default() += 1;
    }
    let per_msg: std::collections::HashMap<usize, usize> = refs
        .iter()
        .map(|r| (r.slot.msg_idx, per_turn[&turn_of[r.slot.msg_idx]]))
        .collect();
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
            .filter_map(|r| {
                let image_count = per_msg.get(&r.slot.msg_idx).copied().unwrap_or(1);
                process_image_memo(&r.data, r.format, &cfg, image_count).map(|out| (r.slot, out))
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
    for (slot, out) in processed {
        let Some(block) = body
            .get_mut("messages")
            .and_then(|m| m.get_mut(slot.msg_idx))
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.get_mut(slot.block_idx))
        else {
            continue;
        };
        let target = match slot.nested_idx {
            None => Some(block),
            Some(n) => block.get_mut("content").and_then(|c| c.get_mut(n)),
        };
        if let Some(target) = target {
            write_image_block(target, slot.data_url, &out);
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
    async fn compress_body_covers_tool_result_and_image_url() {
        // 生产 90% 的图在 tool_result 里(Claude Code Read 截图),旧实现不压;image_url data URL 同理。
        let big = make_png(4800, 600);
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "shot"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
                    ]},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{big}")}},
                    {"type": "tool_result", "tool_use_id": "t2", "content": [
                        {"type": "image_url", "image_url": format!("data:image/png;base64,{big}")}
                    ]}
                ]}
            ]
        });
        compress_body_images(&mut body, &ImageConfig::default()).await;
        let c = &body["messages"][0]["content"];
        let nested = c[0]["content"][1]["source"]["data"].as_str().unwrap();
        assert_eq!(b64_dims(nested), (4000, 500), "tool_result 内嵌图应被压缩");
        assert_eq!(c[0]["content"][0]["text"], "shot");
        for url in [c[1]["image_url"]["url"].as_str().unwrap(), c[2]["content"][0]["image_url"].as_str().unwrap()] {
            let (meta, data) = url.split_once(',').unwrap();
            assert!(meta.starts_with("data:image/") && meta.ends_with(";base64"), "{meta}");
            assert_eq!(b64_dims(data), (4000, 500), "image_url data URL 应被压缩并重组");
        }
    }

    #[tokio::test]
    async fn history_image_bytes_stable_when_later_turns_add_images() {
        // 评审 medium:单/多图档不同时,旧实现按全请求图片数选档 → 下一轮多一张图,旧图被按
        // 更严档重压、字节改变、缓存前缀断裂。现按所在消息自己的图片数选档。
        let cfg = ImageConfig {
            max_long_edge: 0,
            max_pixels_single: 4_000_000,
            max_pixels_multi: 1_000_000,
            multi_threshold: 2,
            ..ImageConfig::default()
        };
        let old = make_png(2000, 2000);
        let turn = |data: &str| serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}]}]});
        let mut t1 = serde_json::json!({"messages": [turn(&old)]});
        let mut t2 = serde_json::json!({"messages": [turn(&old), {"role": "assistant", "content": "ok"}, turn(&make_png(64, 64))]});
        compress_body_images(&mut t1, &cfg).await;
        compress_body_images(&mut t2, &cfg).await;
        assert_eq!(t1["messages"][0], t2["messages"][0], "历史里的旧图字节必须跨轮不变");
    }

    #[tokio::test]
    async fn split_user_messages_count_as_one_turn() {
        // 同一轮拆成两条相邻 user 消息(converter 会合并成一条上游消息),多图档照样生效。
        let cfg = ImageConfig {
            max_long_edge: 0,
            max_pixels_single: 4_000_000,
            max_pixels_multi: 1_000_000,
            multi_threshold: 2,
            ..ImageConfig::default()
        };
        let img = |d: &str| serde_json::json!({"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": d}}]});
        let big = make_png(2000, 2000);
        let mut body = serde_json::json!({"messages": [img(&big), img(&big)]});
        compress_body_images(&mut body, &cfg).await;
        for i in 0..2 {
            let d = body["messages"][i]["content"][0]["source"]["data"].as_str().unwrap();
            assert_eq!(b64_dims(d), (1000, 1000), "拆开的两条应按同一轮 2 张图走多图档");
        }
    }

    #[test]
    fn memo_entry_count_is_bounded() {
        for i in 0..(memo::TEST_MAX_ENTRIES + 50) {
            memo::put(memo_key(&format!("x{i}"), "png", &ImageConfig::default(), 1), memo::Outcome::Unchanged);
        }
        let (n, bytes) = memo::stats();
        assert!(n <= memo::TEST_MAX_ENTRIES, "条目数必须受硬上限约束: {n}");
        assert!(bytes <= 128 * 1024 * 1024);
    }

    #[tokio::test]
    async fn compress_body_memo_is_deterministic_across_turns() {
        let big = make_png(4800, 600);
        let mk = || serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big.clone()}}
        ]}]});
        let (mut a, mut b) = (mk(), mk());
        compress_body_images(&mut a, &ImageConfig::default()).await;
        compress_body_images(&mut b, &ImageConfig::default()).await;
        assert_eq!(a, b, "同一张图跨轮压缩结果必须逐字节一致(缓存前缀稳定)");
        assert_ne!(a["messages"][0]["content"][0]["source"]["data"], serde_json::json!(big));
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
