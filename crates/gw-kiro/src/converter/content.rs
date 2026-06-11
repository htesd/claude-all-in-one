//! 消息 content 归一化:文本/图片/文档/tool_result 抽取。

use super::{ContentBlock, ConversionError};
use crate::kiro_types::conversation::{KiroDocument, KiroImage};
use crate::kiro_types::tool::ToolResult;

/// 处理消息内容，提取文本、图片、文档和工具结果
/// process_message_content 的返回:(文本, 图片, 文档, 工具结果)。
pub(super) type ProcessedContent = (String, Vec<KiroImage>, Vec<KiroDocument>, Vec<ToolResult>);

/// 合并当前轮的多条连续 user 消息为单一 ProcessedContent。
///
/// 🟢 借鉴 static_flow `current_user_message_range`:Claude Code / 代理链常把"一轮"
/// 拆成多条连续 user 消息(典型:text 块与 tool_result 块分两条发,或客户端把附件单独
/// 成条)。把尾部连续 user 整体视为当前轮,文本以 `\n` 连接、图片/文档/工具结果按序合并,
/// 避免误把当前轮的前半截归进 history(那会让 history 前缀随本轮内容抖动,毒化缓存)。
///
/// 与历史侧 `merge_user_messages` 同口径(文本 `\n` join),但返回原始 ProcessedContent
/// 交给调用方做当前轮专属的空内容/媒体兜底(MEDIA_ONLY_PLACEHOLDER 等),不在此兜底。
pub(super) fn merge_current_message_content(
    messages: &[crate::anthropic_types::Message],
) -> Result<ProcessedContent, ConversionError> {
    let mut text_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_documents = Vec::new();
    let mut all_tool_results = Vec::new();
    for msg in messages {
        let (text, images, documents, tool_results) = process_message_content(&msg.content)?;
        if !text.is_empty() {
            text_parts.push(text);
        }
        all_images.extend(images);
        all_documents.extend(documents);
        all_tool_results.extend(tool_results);
    }
    Ok((text_parts.join("\n"), all_images, all_documents, all_tool_results))
}

pub(super) fn process_message_content(
    content: &serde_json::Value,
) -> Result<ProcessedContent, ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut documents = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source {
                                if let Some(img) = anthropic_image_to_kiro(&source) {
                                    images.push(img);
                                }
                            }
                        }
                        "document" => {
                            // Anthropic 文档块 → Kiro documents[*]（PDF/Office/文本附件）
                            if let Some(source) = block.source {
                                if let Some(doc) = anthropic_document_to_kiro(block.name.as_deref(), &source) {
                                    documents.push(doc);
                                }
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let result_content = extract_tool_result_content(&block.content);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);

                                // 工具结果里也可能有图（如 browser 截图），单独抽出来
                                images.extend(extract_images_from_tool_result_content(&block.content));
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, documents, tool_results))
}

/// 从文档 media_type 获取 Kiro 文档格式。
/// Kiro 原生支持的文档类型集合（与 static_flow 对齐）。
pub(super) fn get_document_format(media_type: &str) -> Option<String> {
    let f = match media_type {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return None,
    };
    Some(f.to_string())
}

/// 把 Anthropic 文档块的 source 转为 KiroDocument。
///
/// - `type:"base64"` + `data` + `media_type` → 直接透传 base64 字节
/// - `type:"text"` + `data`(纯文本 media_type)→ base64 编码后透传(对齐 static_flow)
/// - `type:"url"` / `"file"` → 暂不支持（需异步抓取），跳过
pub(super) fn anthropic_document_to_kiro(
    name: Option<&str>,
    source: &crate::anthropic_types::ImageSource,
) -> Option<KiroDocument> {
    use base64::Engine;
    let media_type = source.media_type.as_deref()?;
    let format = get_document_format(media_type)?;
    let doc_name = name.filter(|n| !n.is_empty()).unwrap_or("document").to_string();
    match source.source_type.as_str() {
        "base64" => {
            let data = source.data.as_ref()?;
            Some(KiroDocument::from_base64(doc_name, format, data.clone()))
        }
        "text" => {
            // text 源仅纯文本类 media_type 合法(对齐 static_flow);Kiro 仍要 base64 字节。
            if !matches!(media_type, "text/plain" | "text/markdown" | "text/html" | "text/csv") {
                tracing::warn!(media_type, "text 源仅支持纯文本类 media_type，已跳过");
                return None;
            }
            let data = source.data.as_ref()?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(data.as_bytes());
            Some(KiroDocument::from_base64(doc_name, format, b64))
        }
        other => {
            tracing::warn!(source_type = other, media_type, "暂不支持的文档源类型，已跳过");
            None
        }
    }
}

#[cfg(test)]
mod doc_source_tests {
    use super::*;
    use base64::Engine;

    fn src(stype: &str, media: &str, data: &str) -> crate::anthropic_types::ImageSource {
        crate::anthropic_types::ImageSource {
            source_type: stype.to_string(),
            media_type: Some(media.to_string()),
            data: Some(data.to_string()),
            url: None,
            file_id: None,
        }
    }

    #[test]
    fn text_source_markdown_is_base64_encoded() {
        let s = src("text", "text/markdown", "# Title\nbody");
        let doc = anthropic_document_to_kiro(Some("notes"), &s).expect("text 源应被接受");
        // KiroDocument 的 bytes 应是原文 base64。
        let want = base64::engine::general_purpose::STANDARD.encode("# Title\nbody".as_bytes());
        let json = serde_json::to_value(&doc).unwrap();
        assert!(json.to_string().contains(&want), "text 文档应 base64 编码: {json}");
    }

    #[test]
    fn text_source_rejected_for_binary_media() {
        // PDF 不能走 text 源。
        let s = src("text", "application/pdf", "not-really-pdf");
        assert!(anthropic_document_to_kiro(Some("x"), &s).is_none());
    }

    #[test]
    fn base64_source_still_passthrough() {
        let s = src("base64", "application/pdf", "JVBERi0=");
        assert!(anthropic_document_to_kiro(Some("x"), &s).is_some());
    }
}

/// 从 media_type 获取图片格式
pub(super) fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 把 Anthropic ImageSource 转为 KiroImage。
///
/// 当前支持：
/// - `type: "base64"` + `data` + `media_type` → 直接转
/// - `type: "url"` / `"file"` → 暂不支持（log warning，跳过），Kiro 需要 bytes，
///   抓取/解析 file_id 需要异步 IO，留待后续 v21+ 改造为 async preprocessing 一步搞定。
pub(super) fn anthropic_image_to_kiro(source: &crate::anthropic_types::ImageSource) -> Option<crate::kiro_types::conversation::KiroImage> {
    match source.source_type.as_str() {
        "base64" => {
            let data = source.data.as_ref()?;
            let media_type = source.media_type.as_deref()?;
            let format = get_image_format(media_type)?;
            Some(crate::kiro_types::conversation::KiroImage::from_base64(format, data.clone()))
        }
        "url" => {
            tracing::warn!(
                url = source.url.as_deref().unwrap_or(""),
                "暂不支持 URL 源图片，已跳过。客户端请把图片转 base64 再发送。"
            );
            None
        }
        "file" => {
            tracing::warn!(
                file_id = source.file_id.as_deref().unwrap_or(""),
                "暂不支持 file_id 源图片，已跳过"
            );
            None
        }
        other => {
            tracing::warn!(source_type = other, "未知图片源类型，已跳过");
            None
        }
    }
}

/// 从 tool_result 的 content 数组里抽出 image 块（如 browser 工具的截图）。
pub(super) fn extract_images_from_tool_result_content(
    content: &Option<serde_json::Value>,
) -> Vec<crate::kiro_types::conversation::KiroImage> {
    let mut images = Vec::new();
    if let Some(serde_json::Value::Array(arr)) = content {
        for item in arr {
            let block_type = item.get("type").and_then(|v| v.as_str());
            if block_type == Some("image") {
                if let Ok(src) = serde_json::from_value::<crate::anthropic_types::ImageSource>(
                    item.get("source").cloned().unwrap_or(serde_json::Value::Null),
                ) {
                    if let Some(img) = anthropic_image_to_kiro(&src) {
                        images.push(img);
                    }
                }
            }
        }
    }
    images
}

/// 提取工具结果内容
pub(super) fn extract_tool_result_content(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}
