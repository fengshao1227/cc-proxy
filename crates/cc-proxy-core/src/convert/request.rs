use crate::config::ProxyConfig;
use crate::model_map;
use crate::types::claude::{
    ContentBlock, Message, MessageContent, MessagesRequest, SystemContent, ToolResultContent,
};
use crate::types::openai::*;

/// Convert a Claude Messages API request to OpenAI Chat Completions format
pub fn claude_to_openai(req: &MessagesRequest, config: &ProxyConfig) -> ChatCompletionRequest {
    let mapped = model_map::map_model(&req.model, config);
    let openai_model = mapped.model;
    let model_tier = mapped.tier;
    let mut messages = Vec::new();

    // System message
    if let Some(ref system) = req.system {
        let text = extract_system_text(system);
        if !text.is_empty() {
            messages.push(ChatMessage {
                role: "system".into(),
                content: Some(ChatContent::Text(text)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    // Process messages
    let mut i = 0;
    while i < req.messages.len() {
        let msg = &req.messages[i];

        match msg.role.as_str() {
            "user" => {
                messages.push(convert_user_message(msg));
            }
            "assistant" => {
                messages.push(convert_assistant_message(msg));

                // Check if next message has tool results
                if i + 1 < req.messages.len() {
                    let next = &req.messages[i + 1];
                    if next.role == "user" && has_tool_results(&next.content) {
                        i += 1;
                        let tool_msgs = convert_tool_results(next);
                        messages.extend(tool_msgs);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    let max_tokens = req
        .max_tokens
        .max(config.min_tokens_limit)
        .min(config.max_tokens_limit);

    let mut request = ChatCompletionRequest {
        model: openai_model,
        messages,
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stream: req.stream.unwrap_or(false),
        stop: req.stop_sequences.clone(),
        tools: None,
        tool_choice: None,
        stream_options: None,
        reasoning_effort: None,
    };

    // Convert tools
    if let Some(ref tools) = req.tools {
        let openai_tools: Vec<ChatTool> = tools
            .iter()
            .filter(|t| !t.name.trim().is_empty())
            .map(|t| ChatTool {
                tool_type: "function".into(),
                function: ChatFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();
        if !openai_tools.is_empty() {
            request.tools = Some(openai_tools);
        }
    }

    // Convert tool_choice
    if let Some(ref choice) = req.tool_choice {
        request.tool_choice = Some(convert_tool_choice(choice));
    }

    // Stream options
    if request.stream {
        request.stream_options = Some(StreamOptions {
            include_usage: true,
        });
    }

    // Reasoning effort: per-tier > Claude thinking > global config
    let reasoning = resolve_reasoning_effort(req, config, model_tier);
    if let Some(effort) = reasoning {
        request.reasoning_effort = Some(effort);
    }

    request
}

fn extract_system_text(system: &SystemContent) -> String {
    match system {
        SystemContent::Text(s) => s.trim().to_string(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_string(),
    }
}

fn convert_user_message(msg: &Message) -> ChatMessage {
    match &msg.content {
        MessageContent::Null => ChatMessage {
            role: "user".into(),
            content: Some(ChatContent::Text(String::new())),
            tool_calls: None,
            tool_call_id: None,
        },
        MessageContent::Text(s) => ChatMessage {
            role: "user".into(),
            content: Some(ChatContent::Text(s.clone())),
            tool_calls: None,
            tool_call_id: None,
        },
        MessageContent::Blocks(blocks) => {
            let parts: Vec<ContentPart> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(ContentPart::Text { text: text.clone() }),
                    ContentBlock::Image { source } => {
                        if source.source_type == "base64" {
                            if let (Some(media), Some(data)) = (&source.media_type, &source.data) {
                                return Some(ContentPart::ImageUrl {
                                    image_url: ImageUrl {
                                        url: format!("data:{media};base64,{data}"),
                                    },
                                });
                            }
                        }
                        None
                    }
                    _ => None,
                })
                .collect();

            // Optimize: single text part → plain string
            if parts.len() == 1 {
                if let ContentPart::Text { ref text } = parts[0] {
                    return ChatMessage {
                        role: "user".into(),
                        content: Some(ChatContent::Text(text.clone())),
                        tool_calls: None,
                        tool_call_id: None,
                    };
                }
            }

            ChatMessage {
                role: "user".into(),
                content: Some(ChatContent::Parts(parts)),
                tool_calls: None,
                tool_call_id: None,
            }
        }
    }
}

fn convert_assistant_message(msg: &Message) -> ChatMessage {
    match &msg.content {
        MessageContent::Null => ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
        },
        MessageContent::Text(s) => ChatMessage {
            role: "assistant".into(),
            content: Some(ChatContent::Text(s.clone())),
            tool_calls: None,
            tool_call_id: None,
        },
        MessageContent::Blocks(blocks) => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in blocks {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            call_type: "function".into(),
                            function: FunctionCall {
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        });
                    }
                    _ => {}
                }
            }

            ChatMessage {
                role: "assistant".into(),
                content: if text_parts.is_empty() {
                    None
                } else {
                    Some(ChatContent::Text(text_parts.join("")))
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
            }
        }
    }
}

fn has_tool_results(content: &MessageContent) -> bool {
    match content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
        _ => false,
    }
}

fn convert_tool_results(msg: &Message) -> Vec<ChatMessage> {
    let mut results = Vec::new();
    if let MessageContent::Blocks(blocks) = &msg.content {
        for block in blocks {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
            } = block
            {
                let text = match content {
                    Some(ToolResultContent::Text(s)) => s.clone(),
                    Some(ToolResultContent::Blocks(items)) => items
                        .iter()
                        .filter_map(|item| {
                            item.get("text")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .or_else(|| serde_json::to_string(item).ok())
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Some(ToolResultContent::Object(v)) => v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| serde_json::to_string(v).unwrap_or_default()),
                    None => "No content provided".into(),
                };

                results.push(ChatMessage {
                    role: "tool".into(),
                    content: Some(ChatContent::Text(text)),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                });
            }
        }
    }
    results
}

fn convert_tool_choice(choice: &serde_json::Value) -> serde_json::Value {
    let choice_type = choice
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    match choice_type {
        "auto" => serde_json::json!("auto"),
        "any" => serde_json::json!("required"),
        "tool" => {
            if let Some(name) = choice.get("name").and_then(|v| v.as_str()) {
                serde_json::json!({
                    "type": "function",
                    "function": { "name": name }
                })
            } else {
                serde_json::json!("auto")
            }
        }
        _ => serde_json::json!("auto"),
    }
}

/// Resolve reasoning effort with per-tier support.
///
/// Priority:
/// 1. Per-tier reasoning (BIG_REASONING/MIDDLE_REASONING/SMALL_REASONING)
/// 2. Claude thinking: {enabled: true} → use per-tier or global or "medium"
/// 3. Global REASONING_EFFORT config
/// 4. "none" → return None
pub(crate) fn resolve_reasoning_effort(
    req: &MessagesRequest,
    config: &ProxyConfig,
    tier: Option<crate::config::ModelTier>,
) -> Option<String> {
    // Per-tier reasoning takes highest priority
    if let Some(t) = tier {
        let tier_effort = config.reasoning_for_tier(t);
        if tier_effort != "none" {
            return Some(tier_effort.to_string());
        }
    }

    // Check if Claude request explicitly enables thinking
    if let Some(ref thinking) = req.thinking {
        if thinking.enabled {
            let effort = if config.reasoning_effort != "none" {
                config.reasoning_effort.clone()
            } else {
                "medium".to_string()
            };
            return Some(effort);
        }
    }

    // Fall back to global config
    let effort = &config.reasoning_effort;
    if effort != "none" {
        Some(effort.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::claude::*;

    fn test_config() -> ProxyConfig {
        ProxyConfig {
            openai_api_key: "test-key".into(),
            openai_base_url: "https://api.openai.com/v1".into(),
            big_model: "gpt-4o".into(),
            middle_model: Some("gpt-4o".into()),
            small_model: "gpt-4o-mini".into(),
            host: "0.0.0.0".into(),
            port: 8082,
            anthropic_api_key: None,
            azure_api_version: None,
            log_level: "info".into(),
            max_tokens_limit: 128000,
            min_tokens_limit: 100,
            request_timeout: 600,
            streaming_first_byte_timeout: 300,
            streaming_idle_timeout: 300,
            connect_timeout: 30,
            token_count_scale: 1.0,
            custom_headers: Default::default(),
            reasoning_effort: "none".into(),
            big_reasoning: None,
            middle_reasoning: None,
            small_reasoning: None,
        }
    }

    fn base_request() -> MessagesRequest {
        MessagesRequest {
            model: "claude-3-5-sonnet-20241022".into(),
            max_tokens: 1024,
            messages: vec![],
            system: None,
            stop_sequences: None,
            stream: None,
            temperature: Some(1.0),
            top_p: None,
            top_k: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            thinking: None,
        }
    }

    // ---- F21-1: System message extraction ----

    #[test]
    fn test_extract_system_text_string() {
        let system = SystemContent::Text("  You are a helpful assistant.  ".into());
        assert_eq!(extract_system_text(&system), "You are a helpful assistant.");
    }

    #[test]
    fn test_extract_system_text_blocks() {
        let system = SystemContent::Blocks(vec![
            SystemBlock {
                block_type: "text".into(),
                text: Some("First block.".into()),
                cache_control: None,
            },
            SystemBlock {
                block_type: "text".into(),
                text: Some("Second block.".into()),
                cache_control: None,
            },
            SystemBlock {
                block_type: "other".into(),
                text: Some("Ignored.".into()),
                cache_control: None,
            },
        ]);
        assert_eq!(
            extract_system_text(&system),
            "First block.\n\nSecond block."
        );
    }

    #[test]
    fn test_extract_system_text_empty_blocks() {
        let system = SystemContent::Blocks(vec![SystemBlock {
            block_type: "other".into(),
            text: None,
            cache_control: None,
        }]);
        assert_eq!(extract_system_text(&system), "");
    }

    // ---- F21-2: User message with text only ----

    #[test]
    fn test_user_message_text_only() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Text("Hello world".into()),
        };
        let result = convert_user_message(&msg);
        assert_eq!(result.role, "user");
        match result.content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "Hello world"),
            _ => panic!("Expected ChatContent::Text"),
        }
    }

    #[test]
    fn test_user_message_single_text_block_optimized() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: "Only text".into(),
            }]),
        };
        let result = convert_user_message(&msg);
        // Single text block should be optimized to plain string
        match result.content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "Only text"),
            _ => panic!("Expected optimized ChatContent::Text for single text block"),
        }
    }

    // ---- F21-3: User message with base64 image ----

    #[test]
    fn test_user_message_with_image() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "What's in this image?".into(),
                },
                ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: Some("image/png".into()),
                        data: Some("iVBORw0KGgo=".into()),
                    },
                },
            ]),
        };
        let result = convert_user_message(&msg);
        match result.content {
            Some(ChatContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
                match &parts[1] {
                    ContentPart::ImageUrl { image_url } => {
                        assert_eq!(image_url.url, "data:image/png;base64,iVBORw0KGgo=");
                    }
                    _ => panic!("Expected ImageUrl part"),
                }
            }
            _ => panic!("Expected ChatContent::Parts"),
        }
    }

    #[test]
    fn test_user_message_image_non_base64_ignored() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: ImageSource {
                    source_type: "url".into(),
                    media_type: None,
                    data: None,
                },
            }]),
        };
        let result = convert_user_message(&msg);
        // Non-base64 image should be filtered out, resulting in empty parts
        match result.content {
            Some(ChatContent::Parts(parts)) => assert!(parts.is_empty()),
            _ => panic!("Expected ChatContent::Parts (empty)"),
        }
    }

    // ---- F21-4: Assistant message with tool calls ----

    #[test]
    fn test_assistant_message_with_tool_use() {
        let msg = Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me search.".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_123".into(),
                    name: "search".into(),
                    input: serde_json::json!({"query": "rust"}),
                },
            ]),
        };
        let result = convert_assistant_message(&msg);
        assert_eq!(result.role, "assistant");
        assert_eq!(
            result.content.as_ref().map(|c| match c {
                ChatContent::Text(t) => t.as_str(),
                _ => "",
            }),
            Some("Let me search.")
        );
        let calls = result.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_123");
        assert_eq!(calls[0].function.name, "search");
    }

    #[test]
    fn test_assistant_message_null_content() {
        let msg = Message {
            role: "assistant".into(),
            content: MessageContent::Null,
        };
        let result = convert_assistant_message(&msg);
        assert!(result.content.is_none());
        assert!(result.tool_calls.is_none());
    }

    // ---- F21-5: Tool result conversion ----

    #[test]
    fn test_tool_result_text() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: Some(ToolResultContent::Text("result text".into())),
            }]),
        };
        let results = convert_tool_results(&msg);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "tool");
        assert_eq!(results[0].tool_call_id.as_deref(), Some("call_1"));
        match &results[0].content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "result text"),
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_tool_result_blocks() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_2".into(),
                content: Some(ToolResultContent::Blocks(vec![
                    serde_json::json!({"text": "block one"}),
                    serde_json::json!({"text": "block two"}),
                ])),
            }]),
        };
        let results = convert_tool_results(&msg);
        assert_eq!(results.len(), 1);
        match &results[0].content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "block one\nblock two"),
            _ => panic!("Expected joined text"),
        }
    }

    #[test]
    fn test_tool_result_object_with_text() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_3".into(),
                content: Some(ToolResultContent::Object(
                    serde_json::json!({"text": "object text", "extra": 42}),
                )),
            }]),
        };
        let results = convert_tool_results(&msg);
        match &results[0].content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "object text"),
            _ => panic!("Expected text from object"),
        }
    }

    #[test]
    fn test_tool_result_object_no_text_field() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_4".into(),
                content: Some(ToolResultContent::Object(
                    serde_json::json!({"code": 200, "data": "ok"}),
                )),
            }]),
        };
        let results = convert_tool_results(&msg);
        match &results[0].content {
            Some(ChatContent::Text(t)) => {
                // Should fall back to JSON serialization
                let v: serde_json::Value = serde_json::from_str(t).unwrap();
                assert_eq!(v["code"], 200);
            }
            _ => panic!("Expected JSON-serialized text"),
        }
    }

    #[test]
    fn test_tool_result_none() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_5".into(),
                content: None,
            }]),
        };
        let results = convert_tool_results(&msg);
        match &results[0].content {
            Some(ChatContent::Text(t)) => assert_eq!(t, "No content provided"),
            _ => panic!("Expected fallback text"),
        }
    }

    // ---- F21-6: max_tokens clamping ----

    #[test]
    fn test_max_tokens_clamped_to_min() {
        let config = test_config(); // min=100, max=128000
        let mut req = base_request();
        req.max_tokens = 10; // Below min
        let result = claude_to_openai(&req, &config);
        assert_eq!(result.max_tokens, 100);
    }

    #[test]
    fn test_max_tokens_clamped_to_max() {
        let config = test_config(); // min=100, max=128000
        let mut req = base_request();
        req.max_tokens = 999999; // Above max
        let result = claude_to_openai(&req, &config);
        assert_eq!(result.max_tokens, 128000);
    }

    #[test]
    fn test_max_tokens_within_range() {
        let config = test_config();
        let mut req = base_request();
        req.max_tokens = 2048;
        let result = claude_to_openai(&req, &config);
        assert_eq!(result.max_tokens, 2048);
    }

    // ---- F21-7: Tool choice conversion ----

    #[test]
    fn test_tool_choice_auto() {
        let choice = serde_json::json!({"type": "auto"});
        assert_eq!(convert_tool_choice(&choice), serde_json::json!("auto"));
    }

    #[test]
    fn test_tool_choice_any_to_required() {
        let choice = serde_json::json!({"type": "any"});
        assert_eq!(convert_tool_choice(&choice), serde_json::json!("required"));
    }

    #[test]
    fn test_tool_choice_tool_with_name() {
        let choice = serde_json::json!({"type": "tool", "name": "get_weather"});
        let expected = serde_json::json!({
            "type": "function",
            "function": { "name": "get_weather" }
        });
        assert_eq!(convert_tool_choice(&choice), expected);
    }

    #[test]
    fn test_tool_choice_tool_without_name_fallback() {
        let choice = serde_json::json!({"type": "tool"});
        assert_eq!(convert_tool_choice(&choice), serde_json::json!("auto"));
    }

    #[test]
    fn test_tool_choice_unknown_type() {
        let choice = serde_json::json!({"type": "unknown_thing"});
        assert_eq!(convert_tool_choice(&choice), serde_json::json!("auto"));
    }

    // ---- F21-8: Null content handling ----

    #[test]
    fn test_user_message_null_content() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Null,
        };
        let result = convert_user_message(&msg);
        match result.content {
            Some(ChatContent::Text(t)) => assert_eq!(t, ""),
            _ => panic!("Expected empty text for null content"),
        }
    }

    // ---- F21-9: Reasoning effort mapping ----

    #[test]
    fn test_reasoning_thinking_enabled_default_medium() {
        let config = test_config(); // reasoning_effort = "none"
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: true });
        let effort = resolve_reasoning_effort(&req, &config, None);
        assert_eq!(effort.as_deref(), Some("medium"));
    }

    #[test]
    fn test_reasoning_thinking_enabled_config_override() {
        let mut config = test_config();
        config.reasoning_effort = "high".into();
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: true });
        let effort = resolve_reasoning_effort(&req, &config, None);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn test_reasoning_no_thinking_config_fallback() {
        let mut config = test_config();
        config.reasoning_effort = "low".into();
        let req = base_request();
        let effort = resolve_reasoning_effort(&req, &config, None);
        assert_eq!(effort.as_deref(), Some("low"));
    }

    #[test]
    fn test_reasoning_none_returns_none() {
        let config = test_config(); // reasoning_effort = "none"
        let req = base_request();
        let effort = resolve_reasoning_effort(&req, &config, None);
        assert!(effort.is_none());
    }

    #[test]
    fn test_reasoning_per_tier_overrides_global() {
        let mut config = test_config();
        config.reasoning_effort = "low".into();
        config.big_reasoning = Some("xhigh".into());
        let req = base_request();
        // Big tier should use its own reasoning
        let effort = resolve_reasoning_effort(&req, &config, Some(crate::config::ModelTier::Big));
        assert_eq!(effort.as_deref(), Some("xhigh"));
        // Middle falls back to global
        let effort =
            resolve_reasoning_effort(&req, &config, Some(crate::config::ModelTier::Middle));
        assert_eq!(effort.as_deref(), Some("low"));
    }

    #[test]
    fn test_reasoning_per_tier_none_falls_through() {
        let mut config = test_config();
        config.small_reasoning = Some("none".into());
        config.reasoning_effort = "medium".into();
        let req = base_request();
        // Small tier has "none", but global is "medium" — should fall through to thinking/global
        let effort = resolve_reasoning_effort(&req, &config, Some(crate::config::ModelTier::Small));
        // Per-tier "none" means don't override, falls to global "medium"
        // Actually the current logic: if per-tier is "none", it returns None from the first check,
        // then falls to thinking check, then global. Global is "medium" → Some("medium")
        assert_eq!(effort.as_deref(), Some("medium"));
    }

    #[test]
    fn test_reasoning_thinking_disabled_fallback_to_config() {
        let mut config = test_config();
        config.reasoning_effort = "high".into();
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: false });
        let effort = resolve_reasoning_effort(&req, &config, None);
        // thinking.enabled = false, so we fall through to config
        assert_eq!(effort.as_deref(), Some("high"));
    }

    // ---- Full integration: claude_to_openai ----

    #[test]
    fn test_full_conversion_with_system_and_messages() {
        let config = test_config();
        let req = MessagesRequest {
            model: "claude-3-5-sonnet-20241022".into(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("Hi".into()),
                },
                Message {
                    role: "assistant".into(),
                    content: MessageContent::Text("Hello!".into()),
                },
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("How are you?".into()),
                },
            ],
            system: Some(SystemContent::Text("You are helpful.".into())),
            stop_sequences: None,
            stream: Some(false),
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            thinking: None,
        };
        let result = claude_to_openai(&req, &config);
        // system + 3 user/assistant messages
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[0].role, "system");
        assert_eq!(result.model, "gpt-4o"); // sonnet maps to middle_model
        assert_eq!(result.max_tokens, 1024);
        assert!(!result.stream);
    }

    #[test]
    fn test_stream_options_set_when_streaming() {
        let config = test_config();
        let mut req = base_request();
        req.stream = Some(true);
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text("test".into()),
        });
        let result = claude_to_openai(&req, &config);
        assert!(result.stream);
        assert!(result.stream_options.is_some());
    }
}
