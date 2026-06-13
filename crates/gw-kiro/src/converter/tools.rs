//! 工具转换:JSON Schema 归一化、超长名 sanitize+映射、占位符、分块策略后缀。

use std::collections::HashMap;
use sha2::{Digest, Sha256};
use super::MessagesRequest;
use crate::kiro_types::conversation::Message;
use crate::kiro_types::tool::{InputSchema, Tool, ToolSpecification};

/// 请求含图片时,Kiro 会拒绝带这些 JSON-schema 关键字的工具 schema(返回 400
/// "Improperly formed request")。🟢 对齐 static_flow `converter/schema.rs`。
const MULTIMODAL_UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "anyOf",
    "oneOf",
    "allOf",
    "contains",
    "dependentSchemas",
    "patternProperties",
    "$defs",
    "definitions",
    "prefixItems",
    "unevaluatedProperties",
];

/// 递归判断 schema 是否含多模态下 Kiro 不支持的关键字。
fn schema_contains_multimodal_unsupported_keywords(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(k, child)| {
            MULTIMODAL_UNSUPPORTED_SCHEMA_KEYWORDS.contains(&k.as_str())
                || schema_contains_multimodal_unsupported_keywords(child)
        }),
        serde_json::Value::Array(items) => {
            items.iter().any(schema_contains_multimodal_unsupported_keywords)
        }
        _ => false,
    }
}

/// 请求含图片时,把带不支持关键字的工具 schema **整体替换**为宽松 object schema,避免 Kiro 因
/// "多模态 + 复杂 schema" 返回 400。无图片则原样不动。🟢 对齐 static_flow
/// `apply_multimodal_tool_schema_compatibility`。
pub(super) fn apply_multimodal_tool_schema_compatibility(tools: &mut [Tool], has_images: bool) {
    if !has_images {
        return;
    }
    for tool in tools.iter_mut() {
        if schema_contains_multimodal_unsupported_keywords(
            &tool.tool_specification.input_schema.json,
        ) {
            tool.tool_specification.input_schema = InputSchema::from_json(serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            }));
        }
    }
}

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
///
/// Claude Code / MCP 工具定义偶尔会出现 `required: null`、`properties: null` 等，
/// 导致上游返回 400 "Improperly formed request"。
pub(super) fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // type（必须是字符串）
    if obj.get("type").and_then(|v| v.as_str()).is_none_or(|s| s.is_empty()) {
        obj.insert("type".to_string(), serde_json::Value::String("object".to_string()));
    }

    // properties（必须是 object）
    match obj.get("properties") {
        Some(serde_json::Value::Object(_)) => {}
        _ => { obj.insert("properties".to_string(), serde_json::Value::Object(serde_json::Map::new())); }
    }

    // required（必须是 string 数组）
    let required = match obj.remove("required") {
        Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                .collect(),
        ),
        _ => serde_json::Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    // additionalProperties（允许 bool 或 object，其他按 true 处理）
    match obj.get("additionalProperties") {
        Some(serde_json::Value::Bool(_)) | Some(serde_json::Value::Object(_)) => {}
        _ => { obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(true)); }
    }

    serde_json::Value::Object(obj)
}

/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
pub(super) fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 54 prefix + 1 underscore + 8 hash = 63
    let prefix_max = TOOL_NAME_MAX_LEN - 1 - 8;
    let prefix = match name.char_indices().nth(prefix_max) {
        Some((idx, _)) => &name[..idx],
        None => name,
    };
    format!("{}_{}", prefix, hash_suffix)
}

/// 如果名称超长则缩短，并记录映射（short → original）
pub(super) fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    let short = shorten_tool_name(name);
    tool_name_map.insert(short.clone(), name.to_string());
    short
}

/// 转换工具定义
pub(super) fn convert_tools(tools: &Option<Vec<crate::anthropic_types::Tool>>, tool_name_map: &mut HashMap<String, String>) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };

    tools
        .iter()
        .map(|t| {
            let mut description = t.description.clone();

            // 对 Write/Edit 工具追加自定义描述后缀
            let suffix = match t.name.as_str() {
                "Write" => WRITE_TOOL_DESCRIPTION_SUFFIX,
                "Edit" => EDIT_TOOL_DESCRIPTION_SUFFIX,
                _ => "",
            };
            if !suffix.is_empty() {
                description.push('\n');
                description.push_str(suffix);
            }

            // 空描述兜底:某些 Kiro 模型版本拒绝空 description 工具规格(400)。
            // 对齐 static_flow:填 "Client-provided tool '{name}'"。
            if description.trim().is_empty() {
                description = format!("Client-provided tool '{}'", t.name);
            }

            // 限制描述长度（安全截断 UTF-8，单次遍历）
            let description = match description.char_indices().nth(TOOL_DESCRIPTION_MAX_LEN) {
                Some((idx, _)) => description[..idx].to_string(),
                None => description,
            };

            Tool {
                tool_specification: ToolSpecification {
                    name: map_tool_name(&t.name, tool_name_map),
                    description,
                    input_schema: InputSchema::from_json(normalize_json_schema(serde_json::json!(t.input_schema))),
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod desc_tests {
    use super::*;
    use std::collections::HashMap;

    fn atool(name: &str, desc: &str) -> crate::anthropic_types::Tool {
        crate::anthropic_types::Tool {
            tool_type: None,
            name: name.to_string(),
            description: desc.to_string(),
            input_schema: HashMap::new(),
            max_uses: None,
        }
    }

    #[test]
    fn empty_description_is_filled() {
        let tools = Some(vec![atool("my_tool", "   ")]);
        let mut map = HashMap::new();
        let out = convert_tools(&tools, &mut map);
        assert_eq!(out.len(), 1);
        // 空/空白描述应兜底为 "Client-provided tool '{name}'",避免 Kiro 400。
        assert_eq!(
            out[0].tool_specification.description,
            "Client-provided tool 'my_tool'"
        );
    }

    #[test]
    fn nonempty_description_preserved() {
        let tools = Some(vec![atool("t", "real desc")]);
        let mut map = HashMap::new();
        let out = convert_tools(&tools, &mut map);
        assert_eq!(out[0].tool_specification.description, "real desc");
    }
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
pub(super) fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

/// 收集历史消息中使用的所有工具名称
pub(super) fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 请求里是否带了受分块策略约束的工具（Write / Edit）。
/// 仅当存在这两个工具时，才把 SYSTEM_CHUNKED_POLICY 注入系统消息，
/// 避免对不含这两个工具的干净客户端造成行为污染。
pub(super) fn request_has_chunked_tools(req: &MessagesRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.name == "Write" || t.name == "Edit"))
}

/// Kiro API 工具名称最大长度限制
pub(super) const TOOL_NAME_MAX_LEN: usize = 63;

/// 工具描述最大长度（字符）。超长描述安全截断，避免上游因超大 schema 报错。
/// 命名常量而非散落魔法数；converter 为无状态自由函数，不引入 config 穿透。
pub(super) const TOOL_DESCRIPTION_MAX_LEN: usize = 10000;

/// 追加到 Write 工具 description 末尾的内容
pub(super) const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
pub(super) const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";
