//! 将内联图片/PDF 转为 box 附件，正文保留含内容摘要的稳定引用。
use super::*;
use base64::Engine;
use sha2::{Digest, Sha256};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_FILES: usize = 16;

pub(super) struct Attachment {
    pub message: usize,
    pub name: String,
    pub data: String,
}

pub(super) fn prepare(mut body: Json) -> Result<(Json, Vec<Attachment>), UpstreamError> {
    let mut files = Vec::new();
    let mut bytes = 0;
    if let Some(messages) = body["messages"].as_array_mut() {
        for (index, message) in messages.iter_mut().enumerate() {
            if let Some(blocks) = message["content"].as_array_mut() {
                walk(blocks, index, &mut files, &mut bytes)?;
            }
        }
    }
    Ok((body, files))
}

fn walk(
    blocks: &mut [Json],
    message: usize,
    files: &mut Vec<Attachment>,
    bytes: &mut usize,
) -> Result<(), UpstreamError> {
    for block in blocks {
        if block["type"] == "tool_result" {
            if let Some(content) = block["content"].as_array_mut() {
                walk(content, message, files, bytes)?;
            }
            continue;
        }
        let image = block["type"] == "image";
        let pdf = block["type"] == "document" && block["source"]["media_type"] == "application/pdf";
        if !image && !pdf {
            continue;
        }
        if block["source"]["type"] != "base64" {
            return Err(UpstreamError::bad_request(
                "sandchat box: 图片/PDF 必须提供内联 base64，暂不抓取 URL",
            ));
        }
        let ext = match block["source"]["media_type"].as_str() {
            Some("image/png") => "png",
            Some("image/jpeg") => "jpg",
            Some("image/webp") => "webp",
            Some("image/gif") => "gif",
            Some("application/pdf") if pdf => "pdf",
            _ => return Err(UpstreamError::bad_request("sandchat box: 不支持的附件类型")),
        };
        let data = block["source"]["data"]
            .as_str()
            .ok_or_else(|| UpstreamError::bad_request("sandchat box: 附件缺少 base64 数据"))?;
        if files.len() >= MAX_FILES || data.len() > MAX_BYTES.div_ceil(3) * 4 {
            return Err(UpstreamError::bad_request(
                "sandchat box: 附件超过 16 个或 20 MiB 总预算",
            ));
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| UpstreamError::bad_request("sandchat box: 附件 base64 无效"))?;
        *bytes += decoded.len();
        if decoded.is_empty() || *bytes > MAX_BYTES {
            return Err(UpstreamError::bad_request(
                "sandchat box: 附件为空或超过 20 MiB 总预算",
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&decoded));
        let name = format!("attachment-{digest}.{ext}");
        files.push(Attachment {
            message,
            name: name.clone(),
            data: data.to_string(),
        });
        *block = serde_json::json!({"type":"text","text":format!("[Attached file: {name}. Read the actual attachment; its contents are not included in this text.]")});
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image(data: &str) -> Json {
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}})
    }

    #[test]
    fn 图片与工具结果内PDF保留稳定引用和所属轮次() {
        let body = json!({"messages":[{"role":"user","content":[image("aGVsbG8=")]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERg=="}}]}]}]});
        let (converted, files) = prepare(body.clone()).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].message, files[1].message), (0, 1));
        assert!(files[1].name.ends_with(".pdf"));
        assert_eq!(converted["messages"][0]["content"][0]["type"], "text");
        assert_eq!(files[0].name, prepare(body).unwrap().1[0].name);
        let (_, changed) = prepare(json!({"messages":[{"content":[image("d29ybGQ=")]}]})).unwrap();
        assert_ne!(files[0].name, changed[0].name);
    }

    #[test]
    fn 无效附件与URL明确报错不静默丢弃() {
        for mut block in [image("%%%"), image("")] {
            assert!(prepare(json!({"messages":[{"content":[block.clone()]}]})).is_err());
            block["source"]["type"] = json!("url");
            assert!(prepare(json!({"messages":[{"content":[block]}]})).is_err());
        }
    }
}
