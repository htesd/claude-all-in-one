use super::*;
// 测试专用类型(impl 部分不用,集中在此避免 lib 的 unused_imports)
use crate::kiro_types::conversation::{AssistantMessage, HistoryAssistantMessage, HistoryUserMessage, UserMessage};
use crate::kiro_types::tool::ToolResult;

#[test]
fn test_anthropic_message_has_cache_control_detects_block() {
    let v = serde_json::json!([
        {"type": "text", "text": "hi"},
        {"type": "text", "text": "world", "cache_control": {"type": "ephemeral"}}
    ]);
    assert!(anthropic_message_has_cache_control(&v));
}

#[test]
fn test_anthropic_message_has_cache_control_absent() {
    let v = serde_json::json!([{"type": "text", "text": "hi"}]);
    assert!(!anthropic_message_has_cache_control(&v));
    let v = serde_json::json!("plain string");
    assert!(!anthropic_message_has_cache_control(&v));
}

#[test]
fn test_cache_point_serializes_compact_when_set() {
    let mut um = UserInputMessage::new("hi", "claude-opus-4.7");
    um.cache_point = Some(CachePoint::ephemeral());
    um.client_cache_config = Some(ClientCacheConfig::default());
    let json = serde_json::to_string(&um).unwrap();
    assert!(json.contains("\"cachePoint\":{\"type\":\"EPHEMERAL\"}"));
    assert!(json.contains("\"clientCacheConfig\":{\"usePromptCache\":true}"));
}

#[test]
fn test_cache_point_absent_field_omitted_by_default() {
    let um = UserInputMessage::new("hi", "claude-opus-4.7");
    let json = serde_json::to_string(&um).unwrap();
    assert!(!json.contains("cachePoint"));
    assert!(!json.contains("clientCacheConfig"));
}

#[test]
fn test_strip_rolling_fingerprints_multibyte_no_panic() {
    // 回归：byte 12 落在 em-dash（3字节）中间时，旧实现 trimmed[..12] 会 panic。
    let input = "**Step 1** — write the memory to its own file.\n";
    assert_eq!(strip_rolling_fingerprints(input), input);
}

#[test]
fn test_strip_rolling_fingerprints_removes_billing_header() {
    let input = "keep this line\nx-anthropic-billing-header: cch=abcde;\nkeep too\n";
    let out = strip_rolling_fingerprints(input);
    assert!(!out.contains("x-anthropic-"));
    assert!(out.contains("keep this line"));
    assert!(out.contains("keep too"));
}

#[test]
fn test_strip_rolling_fingerprints_case_insensitive_billing_header() {
    // 块1a:收窄到 billing-header + cc 标记行,大小写不敏感、容忍前导空白。
    let input = "  X-Anthropic-Billing-Header: cc_version=1.2.3; cc_entrypoint=cli\nplain\n";
    let out = strip_rolling_fingerprints(input);
    assert!(!out.to_ascii_lowercase().contains("x-anthropic-"));
    assert!(out.contains("plain"));
}

#[test]
fn test_strip_rolling_fingerprints_preserves_non_billing_anthropic_line() {
    // 块1a:精确化后,非 billing 的 x-anthropic-* 行(无 cc 标记)不再被误删。
    let input = "x-anthropic-version: 2023-06-01\nkeep\n";
    let out = strip_rolling_fingerprints(input);
    assert!(out.contains("x-anthropic-version"), "非 billing 行应保留");
    assert!(out.contains("keep"));
}

#[test]
fn test_map_model_sonnet() {
    assert!(
        map_model("claude-sonnet-4-5-20250929")
            .unwrap()
            .contains("sonnet")
    );
    assert!(
        map_model("claude-sonnet-4-6")
            .unwrap()
            .contains("sonnet")
    );
}

#[test]
fn test_map_model_opus() {
    assert!(
        map_model("claude-opus-4-5")
            .unwrap()
            .contains("opus")
    );
}

#[test]
fn test_map_model_haiku() {
    assert!(
        map_model("claude-haiku-4-20250514")
            .unwrap()
            .contains("haiku")
    );
}

#[test]
fn test_map_model_unsupported() {
    assert!(map_model("gpt-4").is_none());
}

#[test]
fn test_map_model_thinking_suffix_sonnet() {
    // thinking 后缀不应影响 sonnet 模型映射
    let result = map_model("claude-sonnet-4-5-20250929-thinking");
    assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
}

#[test]
fn test_map_model_thinking_suffix_opus_4_5() {
    // thinking 后缀不应影响 opus 4.5 模型映射
    let result = map_model("claude-opus-4-5-20251101-thinking");
    assert_eq!(result, Some("claude-opus-4.5".to_string()));
}

#[test]
fn test_map_model_thinking_suffix_opus_4_6() {
    // thinking 后缀不应影响 opus 4.6 模型映射
    let result = map_model("claude-opus-4-6-thinking");
    assert_eq!(result, Some("claude-opus-4.6".to_string()));
}

#[test]
fn test_map_model_thinking_suffix_haiku() {
    // thinking 后缀不应影响 haiku 模型映射
    let result = map_model("claude-haiku-4-5-20251001-thinking");
    assert_eq!(result, Some("claude-haiku-4.5".to_string()));
}

#[test]
fn test_determine_chat_trigger_type() {
    // 无工具时返回 MANUAL
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
}

#[test]
fn test_collect_history_tool_names() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 创建包含工具使用的历史消息
    let mut assistant_msg = AssistantMessage::new("I'll read the file.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read")
            .with_input(serde_json::json!({"path": "/test.txt"})),
        ToolUseEntry::new("tool-2", "write")
            .with_input(serde_json::json!({"path": "/out.txt"})),
    ]);

    let history = vec![
        Message::User(HistoryUserMessage::new(
            "Read the file",
            "claude-sonnet-4.5",
        )),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    let tool_names = collect_history_tool_names(&history);
    assert_eq!(tool_names.len(), 2);
    assert!(tool_names.contains(&"read".to_string()));
    assert!(tool_names.contains(&"write".to_string()));
}

#[test]
fn test_create_placeholder_tool() {
    let tool = create_placeholder_tool("my_custom_tool");

    assert_eq!(tool.tool_specification.name, "my_custom_tool");
    assert!(!tool.tool_specification.description.is_empty());

    // 验证 JSON 序列化正确
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains("\"name\":\"my_custom_tool\""));
}

#[test]
fn test_shorten_tool_name_deterministic() {
    let long_name = "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
    assert!(long_name.len() > TOOL_NAME_MAX_LEN);

    let short1 = shorten_tool_name(long_name);
    let short2 = shorten_tool_name(long_name);
    assert_eq!(short1, short2, "相同输入应产生相同的短名称");
    assert!(short1.len() <= TOOL_NAME_MAX_LEN, "短名称长度应 <= 63，实际 {}", short1.len());
}

#[test]
fn test_shorten_tool_name_uniqueness() {
    let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
    let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
    let short_a = shorten_tool_name(name_a);
    let short_b = shorten_tool_name(name_b);
    assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
}

#[test]
fn test_map_tool_name_short_passthrough() {
    let mut map = HashMap::new();
    let result = map_tool_name("short_name", &mut map);
    assert_eq!(result, "short_name");
    assert!(map.is_empty(), "短名称不应产生映射");
}

#[test]
fn test_map_tool_name_long_creates_mapping() {
    let mut map = HashMap::new();
    let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
    let result = map_tool_name(long_name, &mut map);
    assert!(result.len() <= TOOL_NAME_MAX_LEN);
    assert_eq!(map.get(&result), Some(&long_name.to_string()));
}

#[test]
fn test_tool_name_mapping_in_convert_request() {
    use crate::anthropic_types::{Message as AnthropicMessage, Tool as AnthropicTool};

    let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
    assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

    let mut schema = std::collections::HashMap::new();
    schema.insert("type".to_string(), serde_json::json!("object"));
    schema.insert("properties".to_string(), serde_json::json!({}));

    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            },
        ],
        system: None,
        stream: false,
        tools: Some(vec![AnthropicTool {
            name: long_tool_name.to_string(),
            description: "A test tool".to_string(),
            input_schema: schema,
            tool_type: None,
            max_uses: None,
        }]),
        thinking: None,
        tool_choice: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };

    let result = convert_request(&req).unwrap();

    // 应该有映射
    assert_eq!(result.tool_name_map.len(), 1);

    // 映射中的值应该是原始名称
    let (short, original) = result.tool_name_map.iter().next().unwrap();
    assert_eq!(original, long_tool_name);
    assert!(short.len() <= TOOL_NAME_MAX_LEN);

    // Kiro 请求中的工具名应该是短名称
    let tools = &result.conversation_state.current_message.user_input_message
        .user_input_message_context.tools;
    assert_eq!(tools[0].tool_specification.name, *short);
}

#[test]
fn test_tool_name_mapping_in_history() {
    use crate::anthropic_types::{Message as AnthropicMessage, Tool as AnthropicTool};

    let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

    let mut schema = std::collections::HashMap::new();
    schema.insert("type".to_string(), serde_json::json!("object"));
    schema.insert("properties".to_string(), serde_json::json!({}));

    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("use the tool"),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "calling tool"},
                    {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                ]),
            },
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                ]),
            },
        ],
        system: None,
        stream: false,
        tools: Some(vec![AnthropicTool {
            name: long_tool_name.to_string(),
            description: "A test tool".to_string(),
            input_schema: schema,
            tool_type: None,
            max_uses: None,
        }]),
        thinking: None,
        tool_choice: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };

    let result = convert_request(&req).unwrap();
    let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

    // 历史中 assistant 消息的 tool_use name 也应该被映射
    let history = &result.conversation_state.history;
    let mut found = false;
    for msg in history {
        if let Message::Assistant(a) = msg {
            if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                for tu in tool_uses {
                    if tu.tool_use_id == "toolu_01" {
                        assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                        found = true;
                    }
                }
            }
        }
    }
    assert!(found, "应该在历史中找到 tool_use");
}

#[test]
fn test_history_tools_added_to_tools_list() {
    use crate::anthropic_types::Message as AnthropicMessage;

    // 创建一个请求，历史中有工具使用，但 tools 列表为空
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Read the file"),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "I'll read the file."},
                    {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                ]),
            },
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                ]),
            },
        ],
        stream: false,
        system: None,
        tools: None, // 没有提供工具定义
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };

    let result = convert_request(&req).unwrap();

    // 验证 tools 列表中包含了历史中使用的工具的占位符定义
    let tools = &result
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tools;

    assert!(!tools.is_empty(), "tools 列表不应为空");
    assert!(
        tools.iter().any(|t| t.tool_specification.name == "read"),
        "tools 列表应包含 'read' 工具的占位符定义"
    );
}

/// 提取 history[0]（系统消息折叠成的 user 消息）的文本内容
fn first_history_user_text(result: &ConversionResult) -> String {
    match result.conversation_state.history.first() {
        Some(Message::User(u)) => u.user_input_message.content.clone(),
        other => panic!("history[0] 应为系统消息折叠的 User，实际: {other:?}"),
    }
}

fn make_named_tool(name: &str) -> crate::anthropic_types::Tool {
    crate::anthropic_types::Tool {
        tool_type: None,
        name: name.to_string(),
        description: "desc".to_string(),
        input_schema: std::collections::HashMap::new(),
        max_uses: None,
    }
}

fn req_with_system_and_tools(
    tools: Option<Vec<crate::anthropic_types::Tool>>,
) -> MessagesRequest {
    MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!("hi"),
        }],
        stream: false,
        system: Some(vec![crate::anthropic_types::SystemMessage {
            text: "You are a helpful assistant.".to_string(),
        }]),
        tools,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    }
}

#[test]
fn test_chunked_policy_injected_when_write_tool_present() {
    let req = req_with_system_and_tools(Some(vec![make_named_tool("Write")]));
    let result = convert_request(&req).unwrap();
    assert!(
        first_history_user_text(&result).contains("comply silently"),
        "带 Write 工具时应注入 SYSTEM_CHUNKED_POLICY"
    );
}

#[test]
fn test_chunked_policy_injected_when_edit_tool_present() {
    let req = req_with_system_and_tools(Some(vec![make_named_tool("Edit")]));
    let result = convert_request(&req).unwrap();
    assert!(
        first_history_user_text(&result).contains("comply silently"),
        "带 Edit 工具时应注入 SYSTEM_CHUNKED_POLICY"
    );
}

#[test]
fn test_chunked_policy_absent_for_clean_client() {
    // 无工具的干净客户端（如第三方行为检测）：系统消息不应被污染
    let req = req_with_system_and_tools(None);
    let result = convert_request(&req).unwrap();
    let text = first_history_user_text(&result);
    assert!(
        !text.contains("comply silently"),
        "无 Write/Edit 工具时不应注入分块策略，避免行为污染"
    );
    assert!(text.contains("helpful assistant"), "原始系统提示应原样保留");
}

#[test]
fn test_chunked_policy_absent_when_only_other_tools() {
    // 带了别的工具（非 Write/Edit）也不应注入
    let req = req_with_system_and_tools(Some(vec![make_named_tool("get_weather")]));
    let result = convert_request(&req).unwrap();
    assert!(
        !first_history_user_text(&result).contains("comply silently"),
        "仅含非 Write/Edit 工具时不应注入分块策略"
    );
}

#[test]
fn test_pdf_document_block_maps_to_kiro_documents() {
    // hvoy PDF 探针: messages[0].content 含 document(application/pdf base64) + text
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ="}},
                {"type": "text", "text": "What text does this PDF contain?"}
            ]),
        }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let docs = &result.conversation_state.current_message.user_input_message.documents;
    assert_eq!(docs.len(), 1, "PDF document 块应转成 1 个 KiroDocument");
    assert_eq!(docs[0].format, "pdf");
    assert_eq!(docs[0].source.bytes, "JVBERi0xLjQ=");
    // 文本仍保留
    let content = &result.conversation_state.current_message.user_input_message.content;
    assert!(content.contains("What text does this PDF"));
}

#[test]
fn test_document_format_mapping() {
    assert_eq!(get_document_format("application/pdf").as_deref(), Some("pdf"));
    assert_eq!(get_document_format("text/csv").as_deref(), Some("csv"));
    assert_eq!(
        get_document_format("application/vnd.openxmlformats-officedocument.wordprocessingml.document").as_deref(),
        Some("docx")
    );
    assert_eq!(get_document_format("image/png"), None, "图片类型不应被当文档");
}

#[test]
fn test_document_converts_to_kiro_doc() {
    // anthropic document 块正确转成 KiroDocument
    let src = crate::anthropic_types::ImageSource {
        source_type: "base64".to_string(),
        media_type: Some("application/pdf".to_string()),
        data: Some("JVBERi0xLjQ=".to_string()),
        url: None,
        file_id: None,
    };
    let doc = anthropic_document_to_kiro(Some("report"), &src);
    assert!(doc.is_some());
    let doc = doc.unwrap();
    assert_eq!(doc.name, "report");
    assert_eq!(doc.format, "pdf");
}

#[test]
fn test_document_only_message_gets_media_placeholder() {
    // 实测根因回归：纯 document、无文本的 user 消息，Kiro 要求 content 非空，
    // 否则 400 Improperly formed request。converter 必须补 MEDIA_ONLY_PLACEHOLDER。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ="}}
            ]),
        }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let cm = &result.conversation_state.current_message.user_input_message;
    assert_eq!(cm.content, MEDIA_ONLY_PLACEHOLDER, "纯文档无文本应补引导语，不能留空");
    assert_eq!(cm.documents.len(), 1, "文档应保留");
}

#[test]
fn test_image_only_message_gets_media_placeholder() {
    // 同理：纯图像无文本也要补占位（实测纯图无文本 → Kiro 400）。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
            ]),
        }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let cm = &result.conversation_state.current_message.user_input_message;
    assert_eq!(cm.content, MEDIA_ONLY_PLACEHOLDER, "纯图像无文本应补引导语");
    assert_eq!(cm.images.len(), 1, "图像应保留");
}

#[test]
fn test_document_with_text_keeps_text() {
    // 带文本的文档消息：content 用用户文本，不覆盖成占位语。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ="}},
                {"type": "text", "text": "总结这个PDF"}
            ]),
        }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let cm = &result.conversation_state.current_message.user_input_message;
    assert_eq!(cm.content, "总结这个PDF", "有文本时保留用户文本");
}

#[test]
fn test_structured_output_injects_schema_instruction() {
    // hvoy 结构化输出探针: output_config.format=json_schema, 无 thinking
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!("计算 39 乘以 63"),
        }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: Some(crate::anthropic_types::OutputConfig {
            effort: "high".to_string(),
            format: Some(crate::anthropic_types::OutputFormat {
                format_type: "json_schema".to_string(),
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"expression": {"type": "string"}, "result": {"type": "integer"}},
                    "required": ["expression", "result"],
                    "additionalProperties": false
                })),
            }),
        }),
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let sys = first_history_user_text(&result);
    assert!(sys.contains("strictly conforms to this JSON Schema"), "应注入结构化输出指令");
    assert!(sys.contains("\"expression\""), "指令应含 schema 内容");
    // 无系统消息时也应建出系统消息承载指令
    assert!(sys.contains("Output the raw JSON object only"));
}

#[test]
fn test_structured_output_instruction_absent_without_schema() {
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!("hi"),
        }],
        stream: true,
        system: Some(vec![crate::anthropic_types::SystemMessage { text: "You are helpful.".to_string() }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    assert!(!first_history_user_text(&result).contains("JSON Schema"), "无 schema 不应注入");
}

#[test]
fn test_structured_output_with_empty_system_still_injects() {
    // 回归(对抗审查发现): system="" 时, 结构化指令不应被漏掉
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!("计算 1+1"),
        }],
        stream: true,
        system: Some(vec![crate::anthropic_types::SystemMessage { text: "".to_string() }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: Some(crate::anthropic_types::OutputConfig {
            effort: "high".to_string(),
            format: Some(crate::anthropic_types::OutputFormat {
                format_type: "json_schema".to_string(),
                schema: Some(serde_json::json!({"type": "object", "properties": {"r": {"type": "integer"}}})),
            }),
        }),
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    assert!(
        first_history_user_text(&result).contains("strictly conforms to this JSON Schema"),
        "空 system 时结构化指令仍应注入"
    );
}

#[test]
fn test_extract_session_id_valid() {
    // 测试有效的 user_id 格式
    let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
    let session_id = extract_session_id(user_id);
    assert_eq!(
        session_id,
        Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
    );
}

#[test]
fn test_extract_session_id_json_format() {
    // 测试 JSON 格式的 user_id
    let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
    let session_id = extract_session_id(user_id);
    assert_eq!(
        session_id,
        Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
    );
}

#[test]
fn test_extract_session_id_json_invalid_session() {
    // 测试 JSON 格式但 session_id 不是有效 UUID
    let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
    let session_id = extract_session_id(user_id);
    assert_eq!(session_id, None);
}

#[test]
fn test_extract_session_id_no_session() {
    // 测试没有 session 的 user_id
    let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
    let session_id = extract_session_id(user_id);
    assert_eq!(session_id, None);
}

#[test]
fn test_extract_session_id_invalid_uuid() {
    // 测试无效的 UUID 格式
    let user_id = "user_xxx_session_invalid-uuid";
    let session_id = extract_session_id(user_id);
    assert_eq!(session_id, None);
}

#[test]
fn test_convert_request_with_session_metadata() {
    use crate::anthropic_types::{Message as AnthropicMessage, Metadata};

    // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("Hello"),
        }],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: Some(Metadata {
            user_id: Some(
                "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
            ),
        }),
        context_management: None,
    };

    let result = convert_request(&req).unwrap();
    assert_eq!(
        result.conversation_state.conversation_id,
        "a0662283-7fd3-4399-a7eb-52b9a717ae88"
    );
}

#[test]
fn test_convert_request_without_metadata() {
    use crate::anthropic_types::Message as AnthropicMessage;

    // 测试没有 metadata 的请求，应该生成新的 UUID
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("Hello"),
        }],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };

    let result = convert_request(&req).unwrap();
    // 验证生成的是有效的 UUID 格式
    assert_eq!(result.conversation_state.conversation_id.len(), 36);
    assert_eq!(
        result
            .conversation_state
            .conversation_id
            .chars()
            .filter(|c| *c == '-')
            .count(),
        4
    );
}

// ===== v55: conversationId 派生纳入 system 身份 =====

/// 造一个无 metadata（走 fallback 派生）的请求：指定 system 文本 + user 文本。
fn req_with_system(system: Option<&str>, user: &str) -> MessagesRequest {
    use crate::anthropic_types::{Message as AnthropicMessage, SystemMessage};
    MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!(user),
        }],
        stream: false,
        system: system.map(|t| vec![SystemMessage { text: t.to_string() }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    }
}

fn conv_id(system: Option<&str>, user: &str) -> String {
    convert_request(&req_with_system(system, user))
        .unwrap()
        .conversation_state
        .conversation_id
}

#[test]
fn test_derive_conv_id_differs_by_system() {
    // 相同前 2 条 user、不同 system → 不同 conversationId（核心：分开多 agent）
    let user = "<session>\niiap@host:~$ run the task\n</session>";
    let main_agent = conv_id(Some("You are Claude Code, the official CLI."), user);
    let monitor = conv_id(Some("You are a security monitor for autonomous agents."), user);
    assert_ne!(
        main_agent, monitor,
        "相同 user 但不同 system 必须派生出不同 conversationId，否则多 agent 会共用缓存槽互踩"
    );
}

#[test]
fn test_derive_conv_id_same_when_system_prefix_same() {
    // 前 8192 字节相同、仅尾部不同的 system → 同一 conversationId（不误伤正常会话：
    // 同 agent 的 system 尾部含每日变化的日期等滚动参数，落在锚点之外）
    let user = "hello";
    let head = "X".repeat(9000); // 超过 8192，尾部差异落在锚点之外
    let sys_a = format!("{head}AAAA");
    let sys_b = format!("{head}BBBB");
    assert_eq!(
        conv_id(Some(&sys_a), user),
        conv_id(Some(&sys_b), user),
        "system 前 8192 字节相同时必须同一 conversationId（同 agent 的 system 尾部会变）"
    );
}

#[test]
fn test_derive_conv_id_differs_when_within_anchor() {
    // 差异落在锚点内（< 8192 字节）→ 不同 conversationId（区分不同 agent / 用户）
    let user = "hello";
    let head = "Y".repeat(2000); // 远小于 8192
    let sys_a = format!("{head}__agent_main__");
    let sys_b = format!("{head}__agent_monitor__");
    assert_ne!(
        conv_id(Some(&sys_a), user),
        conv_id(Some(&sys_b), user),
        "锚点内（如 char 2000 的角色/路径差异）必须区分"
    );
}

#[test]
fn test_derive_conv_id_empty_system_matches_legacy() {
    // 回归保护：空 / 纯空白 system 的派生结果 == 旧版纯 user 哈希（哈希输入不含 sys 段）
    use crate::anthropic_types::{Message as AnthropicMessage, SystemMessage};
    let msgs = vec![AnthropicMessage {
        role: "user".to_string(),
        content: serde_json::json!("Hello legacy"),
    }];
    let legacy = derive_conversation_id_from_messages(&msgs, "");
    let none_system = conv_id(None, "Hello legacy");
    let empty_system = conv_id(Some(""), "Hello legacy");
    assert_eq!(legacy, none_system, "无 system 必须等于空锚点派生（零迁移）");
    assert_eq!(empty_system, none_system, "空串 system 与无 system 等价");

    // Skeptic #1：多个空串 system 块 join 成 "\n"（非空字符串），必须仍视同无 system。
    let mut req_multi_empty = req_with_system(None, "Hello legacy");
    req_multi_empty.system = Some(vec![
        SystemMessage { text: "".to_string() },
        SystemMessage { text: "".to_string() },
    ]);
    let multi_empty = convert_request(&req_multi_empty)
        .unwrap()
        .conversation_state
        .conversation_id;
    assert_eq!(
        multi_empty, none_system,
        "Some([\"\",\"\"]) join 成 \"\\n\"，trim 后视同无 system，必须匹配旧版"
    );

    // 纯空白（空格/制表/换行）system 同样视同无 system
    let whitespace = conv_id(Some("  \n\t "), "Hello legacy");
    assert_eq!(whitespace, none_system, "纯空白 system 视同无 system");
}

#[test]
fn test_derive_conv_id_stable_across_turns() {
    // 同 agent 多轮：system 稳定 + 前 2 条 user 稳定 → 同一 conversationId。
    // 真实场景：Claude Code 长对话前 2 条 user（终端 session + 首个问题）跨轮不变，
    // 后续轮只在尾部追加。故构造两轮都已含相同的前 2 条 user，仅 turn2 尾部更长。
    use crate::anthropic_types::Message as AnthropicMessage;
    let sys = "You are Claude Code.";
    let u1 = || AnthropicMessage { role: "user".to_string(), content: serde_json::json!("first question") };
    let a1 = || AnthropicMessage { role: "assistant".to_string(), content: serde_json::json!("answer 1") };
    let u2 = || AnthropicMessage { role: "user".to_string(), content: serde_json::json!("second question") };

    let mut turn1 = req_with_system(Some(sys), "ignored");
    turn1.messages = vec![u1(), a1(), u2()]; // 前 2 条 user = [first, second]

    let mut turn2 = req_with_system(Some(sys), "ignored");
    turn2.messages = vec![
        u1(), a1(), u2(),
        AnthropicMessage { role: "assistant".to_string(), content: serde_json::json!("answer 2") },
        AnthropicMessage { role: "user".to_string(), content: serde_json::json!("third question") },
    ]; // 前 2 条 user 仍 = [first, second]

    let id1 = convert_request(&turn1).unwrap().conversation_state.conversation_id;
    let id2 = convert_request(&turn2).unwrap().conversation_state.conversation_id;
    assert_eq!(id1, id2, "同 agent 多轮、前 2 条 user 不变时 conversationId 必须稳定");
}

#[test]
fn test_normalized_client_system_strips_fingerprints() {
    // rolling fingerprint 行被剥除（与 build_history 同口径）
    let req = req_with_system(
        Some("x-anthropic-billing-header: cch=ab12c;\nReal system content here."),
        "hi",
    );
    let norm = normalized_client_system(&req);
    assert!(
        !norm.contains("x-anthropic-"),
        "rolling fingerprint 行应被剥除，得到：{norm:?}"
    );
    assert!(norm.contains("Real system content here."));
}

#[test]
fn test_safe_prefix_multibyte_boundary() {
    // 中文 / emoji 在字节边界处不 panic，且不超出 max_bytes
    let s = "中文测试🦀rust"; // 多字节
    for n in 0..s.len() + 2 {
        let p = safe_prefix(s, n);
        assert!(p.len() <= n.min(s.len()), "前缀字节数不应超过 max_bytes");
        assert!(s.starts_with(p), "前缀必须是原串的前缀");
        // is_char_boundary 隐式保证：能 &s[..end] 不 panic 即正确
    }
    assert_eq!(safe_prefix("abc", 100), "abc", "短串原样返回");
}


#[test]
fn test_validate_tool_pairing_orphaned_result() {
    // 测试孤立的 tool_result 被过滤
    // 历史中没有 tool_use，但 tool_results 中有 tool_result
    let history = vec![
        Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
        Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
    ];

    let tool_results = vec![ToolResult::success("orphan-123", "some result")];

    let (filtered, _) = validate_tool_pairing(&history, &tool_results);

    // 孤立的 tool_result 应该被过滤掉
    assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
}

#[test]
fn test_remove_orphaned_tool_results_midhistory() {
    use crate::kiro_types::tool::ToolUseEntry;
    use crate::kiro_types::conversation::UserMessage;

    // 复现线上 400：history 中段有个 tool_result，但发起它的 assistant 消息
    // 已被客户端 auto-compact 压掉（无对应 tool_use）。
    // 同时保留一对正常配对的 tool_use/tool_result，确认不被误删。
    let mut good_assistant = AssistantMessage::new("calling good tool");
    good_assistant = good_assistant.with_tool_uses(vec![
        ToolUseEntry::new("use-good", "search").with_input(serde_json::json!({"q": "x"})),
    ]);

    // 构造一个带 tool_results 的 history user 消息
    let mut orphan_ctx = UserInputMessageContext::new();
    orphan_ctx = orphan_ctx.with_tool_results(vec![
        ToolResult::success("use-orphan", "orphan result"), // 无对应 tool_use
        ToolResult::success("use-good", "good result"),     // 有对应 tool_use
    ]);
    let mut orphan_user = UserMessage::new("here are results", "claude-opus-4.8");
    orphan_user.user_input_message_context = orphan_ctx;

    let mut history = vec![
        Message::User(HistoryUserMessage::new("do something", "claude-opus-4.8")),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: good_assistant,
        }),
        Message::User(HistoryUserMessage {
            user_input_message: orphan_user,
        }),
    ];

    remove_orphaned_tool_results(&mut history);

    // 取出清理后的 tool_results
    let remaining: Vec<String> = history
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(&u.user_input_message.user_input_message_context.tool_results),
            _ => None,
        })
        .flatten()
        .map(|r| r.tool_use_id.clone())
        .collect();

    assert_eq!(remaining, vec!["use-good".to_string()], "孤儿 tool_result 应被删除，配对的应保留");
}

#[test]
fn test_remove_orphaned_tool_results_repairs_emptied_user_message() {
    use crate::kiro_types::conversation::UserMessage;

    // 排序隐患回归：tool-result-only 的 user 回合（空 content，仅靠 tool_result 合法），
    // 其唯一的 tool_result 是孤儿被删光后，消息变"彻底空" → 必须被占位符修复，
    // 否则会以 content="" 到达 Kiro 触发 400。
    let mut orphan_ctx = UserInputMessageContext::new();
    orphan_ctx = orphan_ctx.with_tool_results(vec![
        ToolResult::success("use-orphan-only", "orphan result"), // 无对应 tool_use
    ]);
    let mut empty_after = UserMessage::new("", "claude-opus-4.8"); // 空文本（工具结果回合）
    empty_after.user_input_message_context = orphan_ctx;

    let mut history = vec![
        Message::User(HistoryUserMessage::new("hi", "claude-opus-4.8")),
        Message::Assistant(HistoryAssistantMessage::new("hello")),
        Message::User(HistoryUserMessage {
            user_input_message: empty_after,
        }),
    ];

    remove_orphaned_tool_results(&mut history);

    if let Message::User(u) = &history[2] {
        assert!(
            u.user_input_message.user_input_message_context.tool_results.is_empty(),
            "孤儿 tool_result 应被删除"
        );
        assert!(
            !u.user_input_message.content.is_empty(),
            "被删空的 user 消息必须补占位符，不能残留空 content"
        );
    } else {
        panic!("history[2] 应为 user 消息");
    }
}

#[test]
fn test_empty_assistant_message_never_produces_empty_content() {
    // 复现线上确定性 400：上游空响应被客户端写回历史后，下一轮 converter
    // 必须保证 content 非空，否则 Kiro 返回 "Improperly formed request" 毒化会话。
    let mut tool_name_map = HashMap::new();

    // case 1: 彻底空的 assistant 消息（无 text/thinking/tool_use）
    let empty_msg = crate::anthropic_types::Message {
        role: "assistant".to_string(),
        content: serde_json::json!([]),
    };
    let converted = convert_assistant_message(&empty_msg, &mut tool_name_map).unwrap();
    assert!(
        !converted.assistant_response_message.content.is_empty(),
        "空 assistant 消息的 content 不能为空"
    );

    // case 2: content 为空字符串
    let empty_str_msg = crate::anthropic_types::Message {
        role: "assistant".to_string(),
        content: serde_json::json!(""),
    };
    let converted2 = convert_assistant_message(&empty_str_msg, &mut tool_name_map).unwrap();
    assert!(
        !converted2.assistant_response_message.content.is_empty(),
        "空字符串 assistant 消息的 content 不能为空"
    );

    // case 3: merge 多条全空 assistant 消息（连续断流场景）
    let m1 = crate::anthropic_types::Message { role: "assistant".to_string(), content: serde_json::json!([]) };
    let m2 = crate::anthropic_types::Message { role: "assistant".to_string(), content: serde_json::json!("") };
    let refs: Vec<&crate::anthropic_types::Message> = vec![&m1, &m2];
    let merged = merge_assistant_messages(&refs, &mut tool_name_map).unwrap();
    assert!(
        !merged.assistant_response_message.content.is_empty(),
        "合并多条全空 assistant 消息后 content 不能为空"
    );
}

#[test]
fn test_empty_assistant_with_tool_use_still_placeholder() {
    // 纯工具调用回合（无 text/thinking）仍应得到非空占位符，且保留 tool_use
    let mut tool_name_map = HashMap::new();
    let msg = crate::anthropic_types::Message {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "tool_use", "id": "tu-1", "name": "read", "input": {"path": "/x"}}
        ]),
    };
    let converted = convert_assistant_message(&msg, &mut tool_name_map).unwrap();
    assert!(!converted.assistant_response_message.content.is_empty());
    assert!(converted.assistant_response_message.tool_uses.is_some());
}

#[test]
fn test_history_assistant_strips_thinking_for_cache_stability() {
    // v49 回归：历史 assistant 的 thinking 必须被剥离，使前缀跨轮稳定（不被客户端
    // thinking 滚动裁剪打断 Kiro 缓存）。当前轮 thinking 能力不受影响（不经本函数）。
    let mut m = HashMap::new();
    // thinking + text → 只留 text
    let msg = crate::anthropic_types::Message {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "thinking", "thinking": "私密推理不该进历史"},
            {"type": "text", "text": "最终答案"}
        ]),
    };
    let c = convert_assistant_message(&msg, &mut m).unwrap();
    let content = c.assistant_response_message.content;
    assert_eq!(content, "最终答案", "应只保留正文，thinking 被剥离");
    assert!(!content.contains("<thinking>"));
    assert!(!content.contains("私密推理"));

    // thinking-only（无 text 无 tool_use）→ 占位符（不把 thinking 当兜底内容）
    let msg2 = crate::anthropic_types::Message {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "thinking", "thinking": "只有思考没有答案"}
        ]),
    };
    let c2 = convert_assistant_message(&msg2, &mut m).unwrap();
    let content2 = c2.assistant_response_message.content;
    assert!(!content2.contains("只有思考"), "thinking-only 不应进历史内容");
    assert!(!content2.is_empty(), "应兜底为非空占位符避免 Kiro 400");
}

#[test]
fn test_merge_user_messages_empty_gets_placeholder() {
    // 历史 user 消息彻底为空（无 text/tool_result/image）→ 必须兜底为非空
    let empty = crate::anthropic_types::Message {
        role: "user".to_string(),
        content: serde_json::json!([]),
    };
    let refs: Vec<&crate::anthropic_types::Message> = vec![&empty];
    let merged = merge_user_messages(&refs, "claude-opus-4.8").unwrap();
    assert!(
        !merged.user_input_message.content.is_empty(),
        "彻底空的历史 user 消息 content 不能为空"
    );
}

#[test]
fn test_merge_user_messages_tool_result_only_no_placeholder() {
    // "无文本但有 tool_result" 是正常工具结果回合：Kiro 接受空文本，
    // 不应注入占位符（线上证据：count=98 等工具回合 content="" 仍成功）。
    let tr = crate::anthropic_types::Message {
        role: "user".to_string(),
        content: serde_json::json!([
            {"type": "tool_result", "tool_use_id": "tu-9", "content": "result text"}
        ]),
    };
    let refs: Vec<&crate::anthropic_types::Message> = vec![&tr];
    let merged = merge_user_messages(&refs, "claude-opus-4.8").unwrap();
    // 文本应保持为空（不被占位符污染），但 tool_result 应存在
    assert_eq!(
        merged.user_input_message.content, "",
        "工具结果回合不应被占位符污染文本"
    );
    assert!(
        !merged.user_input_message.user_input_message_context.tool_results.is_empty(),
        "tool_result 应被保留"
    );
}

#[test]
fn test_validate_tool_pairing_orphaned_use() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
    let mut assistant_msg = AssistantMessage::new("I'll read the file.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-orphan", "read")
            .with_input(serde_json::json!({"path": "/test.txt"})),
    ]);

    let history = vec![
        Message::User(HistoryUserMessage::new(
            "Read the file",
            "claude-sonnet-4.5",
        )),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    // 没有 tool_result
    let tool_results: Vec<ToolResult> = vec![];

    let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

    // 结果应该为空（因为没有 tool_result）
    // 同时应该返回孤立的 tool_use_id
    assert!(filtered.is_empty());
    assert!(orphaned.contains("tool-orphan"));
}

#[test]
fn test_validate_tool_pairing_valid() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试正常配对的情况
    let mut assistant_msg = AssistantMessage::new("I'll read the file.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read")
            .with_input(serde_json::json!({"path": "/test.txt"})),
    ]);

    let history = vec![
        Message::User(HistoryUserMessage::new(
            "Read the file",
            "claude-sonnet-4.5",
        )),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    let tool_results = vec![ToolResult::success("tool-1", "file content")];

    let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

    // 配对成功，应该保留，无孤立
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].tool_use_id, "tool-1");
    assert!(orphaned.is_empty());
}

#[test]
fn test_validate_tool_pairing_mixed() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试混合情况：部分配对成功，部分孤立
    let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
    ]);

    let history = vec![
        Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    // tool_results: tool-1 配对，tool-3 孤立
    let tool_results = vec![
        ToolResult::success("tool-1", "result 1"),
        ToolResult::success("tool-3", "orphan result"), // 孤立
    ];

    let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

    // 只有 tool-1 应该保留
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].tool_use_id, "tool-1");
    // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
    assert!(orphaned.contains("tool-2"));
}

#[test]
fn test_validate_tool_pairing_history_already_paired() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试历史中已配对的 tool_use 不应该被报告为孤立
    // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
    let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
    assistant_msg1 = assistant_msg1.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read")
            .with_input(serde_json::json!({"path": "/test.txt"})),
    ]);

    // 构建历史中的 user 消息，包含 tool_result
    let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
    let mut ctx = UserInputMessageContext::new();
    ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
    user_msg_with_result = user_msg_with_result.with_context(ctx);

    let history = vec![
        // 第一轮：用户请求
        Message::User(HistoryUserMessage::new(
            "Read the file",
            "claude-sonnet-4.5",
        )),
        // 第一轮：assistant 使用工具
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg1,
        }),
        // 第二轮：用户返回工具结果（历史中已配对）
        Message::User(HistoryUserMessage {
            user_input_message: user_msg_with_result,
        }),
        // 第二轮：assistant 响应
        Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
    ];

    // 当前消息没有 tool_results（用户只是继续对话）
    let tool_results: Vec<ToolResult> = vec![];

    let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

    // 结果应该为空，且不应该有孤立 tool_use
    // 因为 tool-1 已经在历史中配对了
    assert!(filtered.is_empty());
    assert!(orphaned.is_empty());
}

#[test]
fn test_validate_tool_pairing_duplicate_result() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
    let mut assistant_msg = AssistantMessage::new("I'll read the file.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read")
            .with_input(serde_json::json!({"path": "/test.txt"})),
    ]);

    // 历史中已有 tool_result
    let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
    let mut ctx = UserInputMessageContext::new();
    ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
    user_msg_with_result = user_msg_with_result.with_context(ctx);

    let history = vec![
        Message::User(HistoryUserMessage::new(
            "Read the file",
            "claude-sonnet-4.5",
        )),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
        Message::User(HistoryUserMessage {
            user_input_message: user_msg_with_result,
        }),
        Message::Assistant(HistoryAssistantMessage::new("Done")),
    ];

    // 当前消息又发送了相同的 tool_result（重复）
    let tool_results = vec![ToolResult::success("tool-1", "file content again")];

    let (filtered, _) = validate_tool_pairing(&history, &tool_results);

    // 重复的 tool_result 应该被过滤掉
    assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
}

#[test]
fn test_convert_assistant_message_tool_use_only() {
    use crate::anthropic_types::Message as AnthropicMessage;

    // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
    // Kiro API 要求 content 字段不能为空
    let msg = AnthropicMessage {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
        ]),
    };

    let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

    // 验证 content 不为空（使用占位符）
    assert!(
        !result.assistant_response_message.content.is_empty(),
        "content 不应为空"
    );
    assert_eq!(
        result.assistant_response_message.content, " ",
        "仅 tool_use 时应使用 ' ' 占位符"
    );

    // 验证 tool_uses 被正确保留
    let tool_uses = result
        .assistant_response_message
        .tool_uses
        .expect("应该有 tool_uses");
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    assert_eq!(tool_uses[0].name, "read_file");
}

#[test]
fn test_convert_assistant_message_with_text_and_tool_use() {
    use crate::anthropic_types::Message as AnthropicMessage;

    // 测试同时包含 text 和 tool_use 的 assistant 消息
    let msg = AnthropicMessage {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "text", "text": "Let me read that file for you."},
            {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
        ]),
    };

    let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

    // 验证 content 使用原始文本（不是占位符）
    assert_eq!(
        result.assistant_response_message.content,
        "Let me read that file for you."
    );

    // 验证 tool_uses 被正确保留
    let tool_uses = result
        .assistant_response_message
        .tool_uses
        .expect("应该有 tool_uses");
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
}

#[test]
fn test_remove_orphaned_tool_uses() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试从历史中移除孤立的 tool_use
    let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
    ]);

    let mut history = vec![
        Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    // 移除 tool-1 和 tool-3
    let mut orphaned = std::collections::HashSet::new();
    orphaned.insert("tool-1".to_string());
    orphaned.insert("tool-3".to_string());

    remove_orphaned_tool_uses(&mut history, &orphaned);

    // 验证只剩下 tool-2
    if let Message::Assistant(ref assistant_msg) = history[1] {
        let tool_uses = assistant_msg
            .assistant_response_message
            .tool_uses
            .as_ref()
            .expect("应该还有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "tool-2");
    } else {
        panic!("应该是 Assistant 消息");
    }
}

#[test]
fn test_remove_orphaned_tool_uses_all_removed() {
    use crate::kiro_types::tool::ToolUseEntry;

    // 测试移除所有 tool_use 后，tool_uses 变为 None
    let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
    assistant_msg = assistant_msg.with_tool_uses(vec![
        ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
    ]);

    let mut history = vec![
        Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant_msg,
        }),
    ];

    let mut orphaned = std::collections::HashSet::new();
    orphaned.insert("tool-1".to_string());

    remove_orphaned_tool_uses(&mut history, &orphaned);

    // 验证 tool_uses 变为 None
    if let Message::Assistant(ref assistant_msg) = history[1] {
        assert!(
            assistant_msg.assistant_response_message.tool_uses.is_none(),
            "移除所有 tool_use 后应为 None"
        );
    } else {
        panic!("应该是 Assistant 消息");
    }
}

#[test]
fn test_merge_consecutive_assistant_messages() {
    // 测试连续 assistant 消息被正确合并（Issue #79）
    use crate::anthropic_types::Message as AnthropicMessage;

    let msg1 = AnthropicMessage {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "thinking", "thinking": "Let me think about this..."},
            {"type": "text", "text": " "}
        ]),
    };

    let msg2 = AnthropicMessage {
        role: "assistant".to_string(),
        content: serde_json::json!([
            {"type": "thinking", "thinking": "I should read the file."},
            {"type": "text", "text": "Let me read that file."},
            {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
        ]),
    };

    let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
    let result = merge_assistant_messages(&messages, &mut HashMap::new()).expect("合并应成功");

    let content = &result.assistant_response_message.content;
    // v49：历史 thinking 被刻意丢弃（缓存稳定性），故合并后**不应**含 thinking 标签
    assert!(!content.contains("<thinking>"), "历史 thinking 应被剥离，不含标签");
    assert!(!content.contains("Let me think about this"), "历史 thinking 文本应被剥离");
    assert!(!content.contains("I should read the file"), "历史 thinking 文本应被剥离");
    assert!(content.contains("Let me read that file"), "应保留第二条消息的 text 内容");

    let tool_uses = result.assistant_response_message.tool_uses.expect("应有 tool_uses");
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
}

#[test]
fn test_convert_request_strips_history_thinking_keeps_current_turn() {
    // v49 集成回归：完整 convert_request 路径下，
    //   1) 历史 assistant 的 thinking 文本不出现在发给 Kiro 的 history
    //   2) 当前轮（最后一条 user）内容不受影响
    //   3) 开启 thinking 时 system 仍注入 thinking 前缀（当前轮能力不受损）
    use crate::anthropic_types::{Message as AnthropicMessage, Thinking};
    let req = MessagesRequest {
        model: "claude-opus-4-6".to_string(),
        max_tokens: 1024,
        messages: vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("第一问"),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "thinking", "thinking": "历史私密推理ABC"},
                    {"type": "text", "text": "历史答案DEF"}
                ]),
            },
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("当前轮提问GHI"),
            },
        ],
        system: None,
        stream: true,
        tools: None,
        thinking: Some(Thinking {
            thinking_type: "enabled".to_string(),
            display: None,
            budget_tokens: 2000,
        }),
        tool_choice: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let result = convert_request(&req).unwrap();
    let cs = result.conversation_state;
    let history_json = serde_json::to_string(&cs.history).unwrap();
    // 1) 历史 thinking 文本被剥离
    assert!(!history_json.contains("历史私密推理ABC"), "历史 thinking 不应进 Kiro 请求");
    assert!(!history_json.contains("<thinking>"), "历史不应含 thinking 标签");
    // 历史正文保留
    assert!(history_json.contains("历史答案DEF"), "历史正文应保留");
    // 2) 块2b:thinking 前缀现注入**当前轮**(不再进 system/history),当前轮原文仍保留在末尾
    let current = &cs.current_message.user_input_message.content;
    assert!(current.contains("当前轮提问GHI"), "当前轮原文应保留,实际={}", current);
    assert!(
        current.contains("<thinking_mode>"),
        "开启 thinking 时前缀应注入当前轮(保智力且不毒化缓存前缀),实际={}",
        current
    );
    // 3) 块2b:thinking 标签不应再出现在 history(system 折叠块)
    assert!(
        !history_json.contains("<thinking_mode>"),
        "thinking 标签不应进 history(已移至当前轮)"
    );
}

#[test]
fn test_consecutive_assistant_with_tool_use_result_pairing() {
    // 测试 Issue #79 的完整场景
    use crate::anthropic_types::Message as AnthropicMessage;

    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Read the config file"),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "thinking", "thinking": "I need to read the file..."},
                    {"type": "text", "text": " "}
                ]),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "thinking", "thinking": "Let me read the config."},
                    {"type": "text", "text": "I'll read the config file for you."},
                    {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                ]),
            },
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                ]),
            },
        ],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };

    let result = convert_request(&req);
    assert!(result.is_ok(), "连续 assistant 消息场景不应报错: {:?}", result.err());

    let state = result.unwrap().conversation_state;
    let mut found_tool_use = false;
    for msg in &state.history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                    found_tool_use = true;
                    break;
                }
            }
        }
    }
    assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
}

// === 块1b:当前轮范围锚定(尾部连续 user 合并) ===

#[test]
fn test_trailing_consecutive_user_messages_merge_into_current_turn() {
    use crate::anthropic_types::Message as AM;
    // 代理链把"一轮"拆成两条连续 user(文本 + 补充),尾部连续 user 应整体作当前轮,
    // 不应把前半截("前半")误归 history。
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AM { role: "user".to_string(), content: serde_json::json!("历史问题") },
            AM { role: "assistant".to_string(), content: serde_json::json!("历史回答") },
            AM { role: "user".to_string(), content: serde_json::json!("当前轮前半") },
            AM { role: "user".to_string(), content: serde_json::json!("当前轮后半") },
        ],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let cs = convert_request(&req).unwrap().conversation_state;
    // 当前轮 = 两条尾部 user 合并(\n 连接)
    assert_eq!(
        cs.current_message.user_input_message.content,
        "当前轮前半\n当前轮后半",
        "尾部连续 user 应合并进当前轮"
    );
    // history 只含"历史问题/历史回答"一对,不应混入当前轮的任何一条
    let history_texts: Vec<String> = cs
        .history
        .iter()
        .map(|m| match m {
            Message::User(u) => u.user_input_message.content.clone(),
            Message::Assistant(a) => a.assistant_response_message.content.clone(),
        })
        .collect();
    assert!(
        history_texts.iter().any(|t| t.contains("历史问题")),
        "历史问题应在 history"
    );
    assert!(
        !history_texts.iter().any(|t| t.contains("当前轮")),
        "当前轮的任何一条都不应出现在 history,实际 history={:?}",
        history_texts
    );
}

#[test]
fn test_trailing_user_tool_result_merges_with_current_text() {
    use crate::anthropic_types::Message as AM;
    // 典型形态:assistant 发起 tool_use,客户端把 tool_result 与后续文本分两条 user 发。
    // 两条尾部 user 应合并:tool_result 进 context、文本进 content。
    let req = MessagesRequest {
        model: "claude-sonnet-4-5".to_string(),
        max_tokens: 1024,
        messages: vec![
            AM { role: "user".to_string(), content: serde_json::json!("用工具查一下") },
            AM {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "好的"},
                    {"type": "tool_use", "id": "toolu_AB1", "name": "search", "input": {"q": "x"}}
                ]),
            },
            AM {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "toolu_AB1", "content": "结果数据"}
                ]),
            },
            AM { role: "user".to_string(), content: serde_json::json!("基于结果继续") },
        ],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let cs = convert_request(&req).unwrap().conversation_state;
    let cm = &cs.current_message.user_input_message;
    assert_eq!(cm.content, "基于结果继续", "当前轮文本应来自最后一条 user");
    assert_eq!(
        cm.user_input_message_context.tool_results.len(),
        1,
        "tool_result 应合并进当前轮 context"
    );
    assert_eq!(
        cm.user_input_message_context.tool_results[0].tool_use_id,
        "toolu_AB1"
    );
}

// === 块1a:system 净化升级(三级分流 + model identity) ===

fn req_with_messages(messages: Vec<crate::anthropic_types::Message>) -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages,
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    }
}

#[test]
fn test_system_role_stable_prefix_promoted_to_history() {
    use crate::anthropic_types::Message as AM;
    // messages 数组中段注入的 SessionStart 稳定前缀,应提升进 history[0] 折叠块。
    let req = req_with_messages(vec![
        AM {
            role: "system".to_string(),
            content: serde_json::json!("SessionStart hook additional context: project uses Rust"),
        },
        AM { role: "user".to_string(), content: serde_json::json!("hi") },
    ]);
    let result = convert_request(&req).unwrap();
    assert!(
        first_history_user_text(&result).contains("SessionStart hook additional context"),
        "稳定 system 前缀应提升进 history[0]"
    );
}

#[test]
fn test_system_role_dynamic_noise_dropped() {
    use crate::anthropic_types::Message as AM;
    // 已知动态噪声整条丢弃,不进 history 也不进当前轮。
    let req = req_with_messages(vec![
        AM { role: "user".to_string(), content: serde_json::json!("第一问") },
        AM { role: "assistant".to_string(), content: serde_json::json!("答") },
        AM {
            role: "system".to_string(),
            content: serde_json::json!("The task tools haven't been used recently. Consider whether to continue."),
        },
        AM { role: "user".to_string(), content: serde_json::json!("继续") },
    ]);
    let cs = convert_request(&req).unwrap().conversation_state;
    let all_text: String = cs
        .history
        .iter()
        .map(|m| match m {
            Message::User(u) => u.user_input_message.content.clone(),
            Message::Assistant(a) => a.assistant_response_message.content.clone(),
        })
        .chain(std::iter::once(cs.current_message.user_input_message.content.clone()))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        !all_text.contains("haven't been used recently"),
        "动态噪声 system 应被丢弃,实际={}",
        all_text
    );
}

#[test]
fn test_system_role_interrupted_user_converted_to_user() {
    use crate::anthropic_types::Message as AM;
    // interrupted-user 框成 system 的内容,应提正文转 user(遇 IMPORTANT 截断)。
    // 注:转出的 user 紧邻尾部真实 user,按块1b"尾部连续 user"规则会合并进当前轮。
    let req = req_with_messages(vec![
        AM { role: "user".to_string(), content: serde_json::json!("做任务") },
        AM { role: "assistant".to_string(), content: serde_json::json!("进行中") },
        AM {
            role: "system".to_string(),
            content: serde_json::json!("The user sent a new message while you were working:\n等一下先看这个\n\nIMPORTANT: do not stop"),
        },
        AM { role: "user".to_string(), content: serde_json::json!("继续") },
    ]);
    let cs = convert_request(&req).unwrap().conversation_state;
    let current = &cs.current_message.user_input_message.content;
    assert!(current.contains("等一下先看这个"), "中断正文应转 user(并入当前轮),实际={}", current);
    assert!(!current.contains("IMPORTANT"), "IMPORTANT 之后应被截断");
    // history 不应再含被转走的 system 原文
    let history_text: String = cs
        .history
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(u.user_input_message.content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(!history_text.contains("sent a new message"), "system 原文不应残留 history");
}

#[test]
fn test_system_role_unknown_wrapped_as_system_context() {
    use crate::anthropic_types::Message as AM;
    // 未知 system 包 <system_context> 转 user。此处让其后跟 assistant,确保落 history 验证包裹。
    let req = req_with_messages(vec![
        AM { role: "user".to_string(), content: serde_json::json!("问") },
        AM {
            role: "system".to_string(),
            content: serde_json::json!("某些未知的中段系统提示"),
        },
        AM { role: "assistant".to_string(), content: serde_json::json!("答") },
        AM { role: "user".to_string(), content: serde_json::json!("再问") },
    ]);
    let cs = convert_request(&req).unwrap().conversation_state;
    let history_text: String = cs
        .history
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(u.user_input_message.content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        history_text.contains("<system_context>") && history_text.contains("某些未知的中段系统提示"),
        "未知 system 应包 <system_context> 转 user 保序,实际={}",
        history_text
    );
}

#[test]
fn test_model_identity_normalized_in_system_block() {
    // top-level system 含 CC 身份行 + 旧 model identity 行,应被规范化成请求的官方 model。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![crate::anthropic_types::Message {
            role: "user".to_string(),
            content: serde_json::json!("hi"),
        }],
        stream: false,
        system: Some(vec![crate::anthropic_types::SystemMessage {
            text: "You are Claude Code, Anthropic's official CLI for Claude.\nYou are powered by the model named Quince. The exact model ID is claude-quince.".to_string(),
        }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let text = first_history_user_text(&convert_request(&req).unwrap());
    assert!(
        text.contains("You are powered by the model named Opus 4.8. The exact model ID is claude-opus-4-8."),
        "model identity 应被规范化成官方名,实际={}",
        text
    );
    assert!(!text.contains("claude-quince"), "不应残留 Kiro 真实代号");
}

#[test]
fn test_no_system_role_messages_is_noop() {
    use crate::anthropic_types::Message as AM;
    // 无 role=system 消息时,路由应为 None(零拷贝),行为与块1a前一致。
    let req = req_with_messages(vec![
        AM { role: "user".to_string(), content: serde_json::json!("只有普通对话") },
    ]);
    let cs = convert_request(&req).unwrap().conversation_state;
    assert_eq!(cs.current_message.user_input_message.content, "只有普通对话");
}

// === 块2b:thinking 前缀注入当前轮(不进 system 折叠块) ===

#[test]
fn test_thinking_prefix_injected_into_current_turn_not_system() {
    use crate::anthropic_types::{Message as AM, Thinking};
    // 带 system + thinking=adaptive:前缀应进当前轮,system 折叠块不含 thinking 标签。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![AM { role: "user".to_string(), content: serde_json::json!("用户问题") }],
        stream: false,
        system: Some(vec![crate::anthropic_types::SystemMessage {
            text: "You are helpful.".to_string(),
        }]),
        tools: None,
        tool_choice: None,
        thinking: Some(Thinking {
            thinking_type: "adaptive".to_string(),
            display: None,
            budget_tokens: 20000,
        }),
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let cs = convert_request(&req).unwrap().conversation_state;
    let current = &cs.current_message.user_input_message.content;
    // 当前轮含 adaptive 前缀 + 原文
    assert!(current.contains("<thinking_mode>adaptive</thinking_mode>"), "当前轮应含 adaptive 前缀,实际={}", current);
    assert!(current.contains("<thinking_effort>"), "adaptive 应带 effort,实际={}", current);
    assert!(current.contains("用户问题"), "当前轮原文应保留");
    // system 折叠块(history[0])不含 thinking 标签
    let history_json = serde_json::to_string(&cs.history).unwrap();
    assert!(history_json.contains("You are helpful"), "system 正文应在 history[0]");
    assert!(!history_json.contains("<thinking_mode>"), "thinking 不应进 system 折叠块");
}

#[test]
fn test_thinking_prefix_not_duplicated_when_already_present() {
    use crate::anthropic_types::{Message as AM, Thinking};
    // 当前轮内容已含 thinking 标签 → has_thinking_tags 守卫,不重复注入。
    let req = MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages: vec![AM {
            role: "user".to_string(),
            content: serde_json::json!("<thinking_mode>enabled</thinking_mode><max_thinking_length>100</max_thinking_length>\n已经带标签了"),
        }],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: Some(Thinking { thinking_type: "enabled".to_string(), display: None, budget_tokens: 20000 }),
        output_config: None,
        metadata: None,
        context_management: None,
    };
    let cs = convert_request(&req).unwrap().conversation_state;
    let current = &cs.current_message.user_input_message.content;
    assert_eq!(current.matches("<thinking_mode>").count(), 1, "不应重复注入 thinking 标签,实际={}", current);
}
