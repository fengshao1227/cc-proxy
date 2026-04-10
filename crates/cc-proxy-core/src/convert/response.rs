use crate::types::claude::{self, MessagesResponse, ResponseContentBlock, Usage};
use crate::types::openai::ChatCompletionResponse;
use crate::util::{fix_json, tool_id, tool_name};

/// Convert OpenAI non-streaming response to Claude Messages format.
///
/// `tool_name_map` is the canonical→original tool name lookup built from
/// the inbound Claude request's `tools` array (see `util::tool_name::build_map`).
/// Pass an empty map when there are no tools declared — the conversion will
/// still work, just without name restoration.
pub fn openai_to_claude(
    response: &ChatCompletionResponse,
    original_model: &str,
    estimated_input_tokens: u32,
    tool_name_map: &tool_name::ToolNameMap,
) -> MessagesResponse {
    let choice = response.choices.first();
    let message = choice.map(|c| &c.message);

    let mut content_blocks = Vec::new();

    // Text content. `AssistantContent` handles both the string form and
    // the structured-parts form (GPT-5 / o1 / o3), returning None when
    // there's nothing textual to emit.
    if let Some(text) = message.and_then(|m| m.content.as_text()) {
        if !text.is_empty() {
            content_blocks.push(ResponseContentBlock::Text { text });
        }
    }

    // Collect tool_calls from two possible locations:
    //   1. `message.tool_calls` — the mainstream shape (OpenAI, Azure,
    //      DeepSeek, Qwen, most vLLMs).
    //   2. `message.content[i].tool_calls` — a quirky shape used by early
    //      GLM-4, some Qwen2 fine-tunes, and a few custom vLLM builds,
    //      where tool_calls are *nested* inside the content array.
    //
    // We merge both and process them uniformly so callers of either shape
    // behave identically.
    let mut all_tool_calls: Vec<&crate::types::openai::ToolCall> = Vec::new();
    if let Some(tcs) = message.and_then(|m| m.tool_calls.as_ref()) {
        all_tool_calls.extend(tcs.iter());
    }
    // Nested tool_calls are returned by value (extract_nested_tool_calls
    // owns the parsed structs), so we hold them in a stable Vec and then
    // extend with references into it.
    let nested: Vec<crate::types::openai::ToolCall> = message
        .map(|m| m.content.extract_nested_tool_calls())
        .unwrap_or_default();
    all_tool_calls.extend(nested.iter());

    for tc in &all_tool_calls {
        if tc.call_type == "function" {
            // Parse tool arguments with lenient fallback: some providers
            // emit single-quoted JSON that strict serde_json rejects.
            // See util::fix_json for the repair rules.
            let input = fix_json::parse_lenient(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({"raw_arguments": tc.function.arguments}));

            content_blocks.push(ResponseContentBlock::ToolUse {
                // Sanitize: OpenAI-compat providers may return ids with
                // characters Claude's regex rejects. See util::tool_id.
                id: tool_id::sanitize(&tc.id),
                // Restore original casing if the upstream mutated it.
                // See util::tool_name for the full list of observed
                // mutation patterns.
                name: tool_name::restore(tool_name_map, &tc.function.name),
                input,
            });
        }
    }

    // Ensure at least one content block
    if content_blocks.is_empty() {
        content_blocks.push(ResponseContentBlock::Text {
            text: String::new(),
        });
    }

    // Map finish reason. `has_tool_calls` now covers BOTH top-level and
    // nested-in-content shapes, so the stop_reason forcing below fires
    // correctly regardless of which shape the upstream used.
    let has_tool_calls = !all_tool_calls.is_empty();

    let finish_reason = choice
        .and_then(|c| c.finish_reason.as_deref())
        .unwrap_or("stop");
    let stop_reason = if has_tool_calls {
        // Force tool_use if response contains tool calls, regardless of finish_reason
        claude::stop_reason::TOOL_USE
    } else {
        match finish_reason {
            "stop" => claude::stop_reason::END_TURN,
            "length" => claude::stop_reason::MAX_TOKENS,
            "tool_calls" | "function_call" => claude::stop_reason::TOOL_USE,
            _ => claude::stop_reason::END_TURN,
        }
    };

    let usage = response
        .usage
        .as_ref()
        .map(|u| {
            // Claude API semantics: input_tokens and cache_read_input_tokens are
            // MUTUALLY EXCLUSIVE. input_tokens = fresh tokens only, cache_read_input_tokens
            // = tokens served from cache. OpenAI's prompt_tokens is the TOTAL (fresh + cached).
            //
            // Algorithm adapted from CLIProxyAPI (MIT) — extractOpenAIUsage.
            // If we don't subtract cached tokens, Claude Code's context meter will
            // double-count cached tokens and show an inflated usage.
            let cached = u
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0);
            let fresh_input = u.prompt_tokens.saturating_sub(cached);

            // Use min(tiktoken, fresh_input) to avoid over-reporting, but only if
            // both have non-zero values. tiktoken is computed from pre-conversion
            // Claude content; fresh_input is the upstream's post-conversion count.
            let report_input = if estimated_input_tokens > 0 && fresh_input > 0 {
                estimated_input_tokens.min(fresh_input)
            } else if estimated_input_tokens > 0 {
                estimated_input_tokens
            } else {
                fresh_input
            };

            Usage {
                input_tokens: report_input,
                output_tokens: u.completion_tokens,
                cache_read_input_tokens: if cached > 0 { Some(cached) } else { None },
            }
        })
        .unwrap_or_default();

    MessagesResponse {
        id: response.id.clone(),
        response_type: "message".into(),
        role: "assistant".into(),
        model: original_model.to_string(),
        content: content_blocks,
        stop_reason: Some(stop_reason.to_string()),
        stop_sequence: None,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::openai::*;

    /// Empty tool-name map for tests that don't exercise name restoration.
    fn empty_map() -> tool_name::ToolNameMap {
        tool_name::ToolNameMap::new()
    }

    // ---- F22-1: Normal text response ----

    #[test]
    fn test_normal_text_response() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-abc".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("Hello, how can I help?".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ResponseUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                prompt_tokens_details: None,
            }),
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());

        assert_eq!(result.id, "chatcmpl-abc");
        assert_eq!(result.response_type, "message");
        assert_eq!(result.role, "assistant");
        assert_eq!(result.model, "claude-3-5-sonnet-20241022");
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            claude::ResponseContentBlock::Text { text } => {
                assert_eq!(text, "Hello, how can I help?");
            }
            _ => panic!("Expected text content block"),
        }

        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 20);
    }

    // ---- F22-2: Tool call response ----

    #[test]
    fn test_tool_call_response_stop_reason() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-tool".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Empty,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc123".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "get_weather".into(),
                            arguments: r#"{"location":"Tokyo"}"#.into(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());

        // Must be "tool_use" regardless of the OpenAI finish_reason value
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            claude::ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc123");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => panic!("Expected tool_use content block"),
        }
    }

    #[test]
    fn test_tool_call_with_text_and_stop_finish_reason() {
        // Even if finish_reason is "stop", presence of tool_calls forces "tool_use"
        let response = ChatCompletionResponse {
            id: "chatcmpl-mixed".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("Calling tool...".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_xyz".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "search".into(),
                            arguments: r#"{"q":"rust"}"#.into(),
                        },
                    }]),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(result.content.len(), 2); // text + tool_use
    }

    // ---- F22-3: Empty choices array ----

    #[test]
    fn test_empty_choices() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-empty".into(),
            choices: vec![],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());

        // Should produce at least one empty text block as fallback
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            claude::ResponseContentBlock::Text { text } => assert_eq!(text, ""),
            _ => panic!("Expected empty text fallback block"),
        }
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    // ---- F22-4: Malformed tool call arguments ----

    #[test]
    fn test_malformed_tool_arguments_uses_raw_fallback() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-bad".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Empty,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_bad".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "do_thing".into(),
                            arguments: "not valid json {{{".into(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());
        match &result.content[0] {
            claude::ResponseContentBlock::ToolUse { input, .. } => {
                // Should fall back to {"raw_arguments": "not valid json {{{"}
                assert_eq!(input["raw_arguments"], "not valid json {{{");
            }
            _ => panic!("Expected tool_use block"),
        }
    }

    // ---- F22-5: Usage mapping with cache_read_input_tokens ----

    #[test]
    fn test_usage_with_cache_read_tokens() {
        // Upstream reports: prompt_tokens=500 total, 300 of which were cached.
        // Under Claude API semantics, input_tokens = fresh = 500 - 300 = 200,
        // cache_read_input_tokens = 300 (the cached portion itself).
        // The estimated_input_tokens arg is a tiktoken count of the pre-conversion
        // Claude content; used to clamp the fresh portion only.
        let response = ChatCompletionResponse {
            id: "chatcmpl-cache".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("cached".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ResponseUsage {
                prompt_tokens: 500,
                completion_tokens: 100,
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(300),
                }),
            }),
        };

        // estimated=250: larger than fresh(200), so fresh wins (min).
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 250, &empty_map());
        assert_eq!(result.usage.input_tokens, 200); // min(250, 500-300)
        assert_eq!(result.usage.output_tokens, 100);
        assert_eq!(result.usage.cache_read_input_tokens, Some(300));
    }

    #[test]
    fn test_usage_cached_clamped_by_tiktoken_estimate() {
        // Upstream reports 500 total, 300 cached → fresh = 200.
        // tiktoken estimate = 50 → input_tokens = min(50, 200) = 50.
        let response = ChatCompletionResponse {
            id: "chatcmpl-clamp".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("x".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ResponseUsage {
                prompt_tokens: 500,
                completion_tokens: 30,
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(300),
                }),
            }),
        };
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 50, &empty_map());
        assert_eq!(result.usage.input_tokens, 50);
        assert_eq!(result.usage.cache_read_input_tokens, Some(300));
    }

    #[test]
    fn test_usage_all_cached_fresh_zero() {
        // Edge case: 100% cache hit. fresh = 0. input_tokens should be 0.
        let response = ChatCompletionResponse {
            id: "chatcmpl-full-cache".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("x".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ResponseUsage {
                prompt_tokens: 300,
                completion_tokens: 5,
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(300),
                }),
            }),
        };
        // estimated = 0 means "no tiktoken estimate available"
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());
        assert_eq!(result.usage.input_tokens, 0); // all cached
        assert_eq!(result.usage.cache_read_input_tokens, Some(300));
    }

    #[test]
    fn test_usage_without_cache_details() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-nocache".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("no cache".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ResponseUsage {
                prompt_tokens: 50,
                completion_tokens: 25,
                prompt_tokens_details: None,
            }),
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());
        assert_eq!(result.usage.input_tokens, 10); // estimated, not upstream's 50
        assert_eq!(result.usage.output_tokens, 25);
        assert!(result.usage.cache_read_input_tokens.is_none());
    }

    #[test]
    fn test_usage_missing_defaults_to_zero() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-nousage".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("hi".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());
        assert_eq!(result.usage.input_tokens, 0);
        assert_eq!(result.usage.output_tokens, 0);
    }

    // ---- Finish reason mapping ----

    #[test]
    fn test_finish_reason_length_maps_to_max_tokens() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-len".into(),
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: AssistantContent::Text("truncated...".into()),
                    tool_calls: None,
                },
                finish_reason: Some("length".into()),
            }],
            usage: None,
        };

        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 10, &empty_map());
        assert_eq!(result.stop_reason.as_deref(), Some("max_tokens"));
    }

    // ---- I4: ChoiceMessage.content array form (GPT-5/o1/o3 and some providers) ----

    #[test]
    fn test_content_array_form_is_deserialized() {
        // Some providers return message.content as an array of structured
        // parts instead of a plain string. The untagged AssistantContent
        // enum accepts both; AssistantContent::as_text() joins the text parts.
        let raw = r#"{
            "id": "chatcmpl-arr",
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "Hello "},
                        {"type": "reasoning", "text": "(ignored)"},
                        {"type": "text", "text": "world"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            claude::ResponseContentBlock::Text { text } => {
                // Reasoning part is skipped; two text parts concatenated.
                assert_eq!(text, "Hello world");
            }
            _ => panic!("Expected text content block"),
        }
    }

    #[test]
    fn test_content_missing_field_handled() {
        // content field entirely absent — should deserialize as Empty and
        // produce a zero-text body (fallback empty text block).
        let raw = r#"{
            "id": "chatcmpl-no-content",
            "choices": [{
                "message": {"tool_calls": null},
                "finish_reason": "stop"
            }]
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            claude::ResponseContentBlock::Text { text } => assert_eq!(text, ""),
            _ => panic!("Expected empty text fallback"),
        }
    }

    #[test]
    fn test_content_null_handled() {
        // content is explicitly null.
        let raw = r#"{
            "id": "chatcmpl-null",
            "choices": [{
                "message": {"content": null, "tool_calls": null},
                "finish_reason": "stop"
            }]
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());
        assert_eq!(result.content.len(), 1); // empty fallback block
    }

    // ---- I4 completeness: tool_calls nested inside content array ----

    #[test]
    fn test_content_array_with_nested_tool_calls() {
        // Quirky provider shape: tool_calls live inside the content array
        // instead of at message.tool_calls. cc-proxy must still emit them
        // as tool_use blocks and force stop_reason to tool_use.
        let raw = r#"{
            "id": "chatcmpl-nested",
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "Let me check."},
                        {"type": "tool_calls", "tool_calls": [
                            {
                                "id": "call_nested_1",
                                "type": "function",
                                "function": {
                                    "name": "get_weather",
                                    "arguments": "{\"location\":\"Tokyo\"}"
                                }
                            }
                        ]}
                    ]
                },
                "finish_reason": "stop"
            }]
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());

        // Should have: text block + tool_use block
        assert_eq!(result.content.len(), 2);
        match &result.content[0] {
            claude::ResponseContentBlock::Text { text } => assert_eq!(text, "Let me check."),
            _ => panic!("Expected text block"),
        }
        match &result.content[1] {
            claude::ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_nested_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => panic!("Expected tool_use block"),
        }
        // Stop reason must be forced to tool_use even though finish_reason=stop,
        // just like the top-level tool_calls case.
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn test_nested_tool_calls_merged_with_top_level() {
        // Defensive test: if a provider emits BOTH top-level and nested
        // tool_calls in the same response, we should emit both. (This is
        // hypothetical — no known provider does this — but our merge
        // logic shouldn't drop either set.)
        let raw = r#"{
            "id": "chatcmpl-both",
            "choices": [{
                "message": {
                    "content": [
                        {"type": "tool_calls", "tool_calls": [
                            {
                                "id": "call_nested",
                                "type": "function",
                                "function": {"name": "f1", "arguments": "{}"}
                            }
                        ]}
                    ],
                    "tool_calls": [
                        {
                            "id": "call_top",
                            "type": "function",
                            "function": {"name": "f2", "arguments": "{}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());

        // Expect two tool_use blocks — order: top-level first, then nested.
        let tool_blocks: Vec<_> = result
            .content
            .iter()
            .filter_map(|b| match b {
                claude::ResponseContentBlock::ToolUse { id, name, .. } => {
                    Some((id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(tool_blocks.len(), 2);
        assert!(tool_blocks.iter().any(|(id, _)| id == "call_top"));
        assert!(tool_blocks.iter().any(|(id, _)| id == "call_nested"));
    }

    #[test]
    fn test_nested_tool_calls_bad_shape_skipped() {
        // Malformed nested entry (missing id/function) should be silently
        // dropped, not crash the whole response.
        let raw = r#"{
            "id": "chatcmpl-bad-nested",
            "choices": [{
                "message": {
                    "content": [
                        {"type": "tool_calls", "tool_calls": [
                            {"garbage": "field"}
                        ]}
                    ]
                },
                "finish_reason": "stop"
            }]
        }"#;
        let response: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let result = openai_to_claude(&response, "claude-3-5-sonnet-20241022", 0, &empty_map());
        // No tool_use blocks — the malformed entry was dropped.
        let has_tool_use = result
            .content
            .iter()
            .any(|b| matches!(b, claude::ResponseContentBlock::ToolUse { .. }));
        assert!(!has_tool_use);
    }
}
