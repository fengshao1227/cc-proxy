//! Non-streaming Responses API → Claude Messages response converter.
//!
//! Translates a completed [`ResponsesResponse`] (the single JSON object
//! returned by the non-streaming Responses API endpoint) into a
//! [`MessagesResponse`] that Claude Code expects.
//!
//! Algorithm aligned with CLIProxyAPI `ConvertCodexResponseToClaudeNonStream`
//! (codex_claude_response.go:203-328).

use crate::types::claude::{self, MessagesResponse, ResponseContentBlock, Usage};
use crate::types::responses::{ContentPart, OutputItem, ResponsesResponse};
use crate::util::{fix_json, tool_id, tool_name};

/// Convert a completed Responses API response into a Claude [`MessagesResponse`].
///
/// # Arguments
/// - `response` – the parsed Responses API response object.
/// - `original_model` – the model name to surface in the returned envelope
///   (the Claude-facing model string, not the upstream alias).
/// - `estimated_input_tokens` – tiktoken-estimated input token count derived
///   from the original Claude request; used to clamp the reported fresh
///   input token count (C1 formula).
/// - `tool_name_map` – canonical → original name map built from the inbound
///   Claude request's `tools` array; used to restore shortened tool names.
pub fn responses_to_claude(
    response: &ResponsesResponse,
    original_model: &str,
    estimated_input_tokens: u32,
    tool_name_map: &tool_name::ToolNameMap,
) -> MessagesResponse {
    let mut content_blocks: Vec<ResponseContentBlock> = Vec::new();
    let mut has_tool_call = false;

    for item in &response.output {
        match item {
            OutputItem::Message { content } => {
                // Extract all output_text parts and emit one text block each.
                for part in content {
                    if let ContentPart::OutputText { text } = part {
                        if !text.is_empty() {
                            content_blocks.push(ResponseContentBlock::Text { text: text.clone() });
                        }
                    }
                }
            }

            OutputItem::Reasoning { summary, content } => {
                // ResponseContentBlock has no Thinking variant — skip per spec.
                // We still extract the text for completeness, but since the
                // Claude type system doesn't model thinking blocks in non-stream
                // responses, we drop this item silently.
                let _ = extract_thinking_text(summary, content.as_deref());
            }

            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                has_tool_call = true;

                // Restore shortened tool name to its original form.
                let restored_name = tool_name::restore(tool_name_map, name);

                // Sanitize call_id to satisfy Claude's regex `^[a-zA-Z0-9_-]+$`.
                let safe_id = tool_id::sanitize(call_id);

                // Parse arguments with lenient fallback for non-standard JSON.
                let input = fix_json::parse_lenient(arguments)
                    .unwrap_or_else(|_| serde_json::json!({"raw_arguments": arguments}));

                content_blocks.push(ResponseContentBlock::ToolUse {
                    id: safe_id,
                    name: restored_name,
                    input,
                });
            }
        }
    }

    // Guarantee at least one content block so downstream consumers never
    // receive an empty content array.
    if content_blocks.is_empty() {
        content_blocks.push(ResponseContentBlock::Text {
            text: String::new(),
        });
    }

    // ---- stop_reason derivation (§4 precedence rules) ----
    // 1. Any FunctionCall output → "tool_use" (highest priority).
    // 2. response.stop_reason == "max_tokens" → "max_tokens".
    // 3. Otherwise → "end_turn".
    let stop_reason = if has_tool_call {
        claude::stop_reason::TOOL_USE.to_string()
    } else {
        match response.stop_reason.as_deref().unwrap_or("") {
            "max_tokens" => claude::stop_reason::MAX_TOKENS.to_string(),
            _ => claude::stop_reason::END_TURN.to_string(),
        }
    };

    // ---- Usage calculation (C1 formula) ----
    // Responses API reports input_tokens as the TOTAL (fresh + cached).
    // Claude API semantics: input_tokens = fresh only;
    //                       cache_read_input_tokens = cached portion.
    //
    // cached = input_tokens_details.cached_tokens (0 if absent)
    // fresh  = input_tokens - cached  (saturating to avoid underflow)
    // report_input = min(estimated, fresh) when both > 0, else whichever > 0
    let cached = response.usage.input_tokens_details.cached_tokens;
    let fresh = response.usage.input_tokens.saturating_sub(cached);
    let report_input = if estimated_input_tokens > 0 && fresh > 0 {
        estimated_input_tokens.min(fresh)
    } else if estimated_input_tokens > 0 {
        estimated_input_tokens
    } else {
        fresh
    };

    let usage = Usage {
        input_tokens: report_input,
        output_tokens: response.usage.output_tokens,
        cache_read_input_tokens: if cached > 0 { Some(cached) } else { None },
    };

    MessagesResponse {
        id: response.id.clone(),
        response_type: "message".into(),
        role: "assistant".into(),
        model: original_model.to_string(),
        content: content_blocks,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract thinking text from a reasoning output item.
///
/// Mirrors `extractThinkingText` in codex_claude_response.go:231-267.
/// Tries `summary` (array of objects with `.text` or plain strings) first;
/// falls back to `content` string if summary produces nothing.
///
/// This is currently only used defensively; since [`ResponseContentBlock`]
/// has no thinking variant the result is discarded in non-stream context.
fn extract_thinking_text(summary: &[serde_json::Value], content: Option<&str>) -> String {
    let mut buf = String::new();

    for part in summary {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            buf.push_str(text);
        } else if let Some(s) = part.as_str() {
            buf.push_str(s);
        }
    }

    if buf.is_empty() {
        if let Some(c) = content {
            buf.push_str(c);
        }
    }

    buf
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::responses::{
        ContentPart, InputTokensDetails, OutputItem, ResponsesResponse, ResponsesUsage,
    };

    fn empty_map() -> tool_name::ToolNameMap {
        tool_name::ToolNameMap::new()
    }

    fn make_response(output: Vec<OutputItem>, stop_reason: Option<&str>) -> ResponsesResponse {
        ResponsesResponse {
            id: "resp_test123".into(),
            model: "claude-opus-4-5-20251101".into(),
            output,
            usage: ResponsesUsage {
                input_tokens: 100,
                output_tokens: 50,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
            },
            stop_reason: stop_reason.map(str::to_string),
        }
    }

    // ---- test_pure_text_response ----

    #[test]
    fn test_pure_text_response() {
        let resp = make_response(
            vec![OutputItem::Message {
                content: vec![ContentPart::OutputText {
                    text: "Hello world".into(),
                }],
            }],
            Some("end_turn"),
        );

        let result = responses_to_claude(&resp, "claude-opus-4-5-20251101", 80, &empty_map());

        assert_eq!(result.id, "resp_test123");
        assert_eq!(result.response_type, "message");
        assert_eq!(result.role, "assistant");
        assert_eq!(result.model, "claude-opus-4-5-20251101");
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected text block"),
        }
    }

    // ---- test_reasoning_plus_text ----

    #[test]
    fn test_reasoning_plus_text() {
        // Reasoning block should be skipped (no Thinking variant in ResponseContentBlock),
        // only the text block survives.
        let resp = make_response(
            vec![
                OutputItem::Reasoning {
                    summary: vec![serde_json::json!({"text": "Let me think..."})],
                    content: None,
                },
                OutputItem::Message {
                    content: vec![ContentPart::OutputText {
                        text: "The answer is 42.".into(),
                    }],
                },
            ],
            Some("end_turn"),
        );

        let result = responses_to_claude(&resp, "claude-opus-4-5-20251101", 80, &empty_map());

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            _ => panic!("expected text block"),
        }
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    // ---- test_single_tool_use ----

    #[test]
    fn test_single_tool_use() {
        let resp = make_response(
            vec![OutputItem::FunctionCall {
                call_id: "call_abc123".into(),
                name: "get_weather".into(),
                arguments: r#"{"location":"Tokyo"}"#.into(),
            }],
            Some("end_turn"),
        );

        let result = responses_to_claude(&resp, "claude-opus-4-5-20251101", 80, &empty_map());

        // stop_reason forced to tool_use regardless of response.stop_reason
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc123");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => panic!("expected tool_use block"),
        }
    }

    // ---- test_multiple_tool_uses ----

    #[test]
    fn test_multiple_tool_uses() {
        let resp = make_response(
            vec![
                OutputItem::FunctionCall {
                    call_id: "call_1".into(),
                    name: "search".into(),
                    arguments: r#"{"q":"rust"}"#.into(),
                },
                OutputItem::FunctionCall {
                    call_id: "call_2".into(),
                    name: "fetch".into(),
                    arguments: r#"{"url":"https://example.com"}"#.into(),
                },
            ],
            None,
        );

        let result = responses_to_claude(&resp, "claude-opus-4-5-20251101", 80, &empty_map());

        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(result.content.len(), 2);
        let names: Vec<_> = result
            .content
            .iter()
            .filter_map(|b| match b {
                ResponseContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["search", "fetch"]);
    }

    // ---- test_mixed_text_and_tool_use ----

    #[test]
    fn test_mixed_text_and_tool_use() {
        let resp = make_response(
            vec![
                OutputItem::Message {
                    content: vec![ContentPart::OutputText {
                        text: "Let me check that for you.".into(),
                    }],
                },
                OutputItem::FunctionCall {
                    call_id: "call_xyz".into(),
                    name: "lookup".into(),
                    arguments: r#"{"key":"val"}"#.into(),
                },
            ],
            Some("end_turn"),
        );

        let result = responses_to_claude(&resp, "claude-opus-4-5-20251101", 80, &empty_map());

        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(result.content.len(), 2);
        assert!(matches!(
            &result.content[0],
            ResponseContentBlock::Text { .. }
        ));
        assert!(matches!(
            &result.content[1],
            ResponseContentBlock::ToolUse { .. }
        ));
    }

    // ---- test_usage_with_cache ----

    #[test]
    fn test_usage_with_cache() {
        let mut resp = make_response(
            vec![OutputItem::Message {
                content: vec![ContentPart::OutputText { text: "hi".into() }],
            }],
            Some("end_turn"),
        );
        resp.usage = ResponsesUsage {
            input_tokens: 150,
            output_tokens: 60,
            input_tokens_details: InputTokensDetails { cached_tokens: 30 },
        };

        // estimated=200 > fresh(120), so fresh wins: min(200, 120) = 120
        let result = responses_to_claude(&resp, "test-model", 200, &empty_map());
        assert_eq!(result.usage.input_tokens, 120); // min(200, 150-30)
        assert_eq!(result.usage.output_tokens, 60);
        assert_eq!(result.usage.cache_read_input_tokens, Some(30));
    }

    // ---- test_usage_without_cache ----

    #[test]
    fn test_usage_without_cache() {
        let mut resp = make_response(
            vec![OutputItem::Message {
                content: vec![ContentPart::OutputText { text: "hi".into() }],
            }],
            Some("end_turn"),
        );
        resp.usage = ResponsesUsage {
            input_tokens: 50,
            output_tokens: 25,
            input_tokens_details: InputTokensDetails { cached_tokens: 0 },
        };

        // estimated=10 < fresh(50): min(10, 50) = 10
        let result = responses_to_claude(&resp, "test-model", 10, &empty_map());
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 25);
        assert!(result.usage.cache_read_input_tokens.is_none());
    }

    // ---- test_empty_output_fallback ----

    #[test]
    fn test_empty_output_fallback() {
        let resp = make_response(vec![], Some("end_turn"));

        let result = responses_to_claude(&resp, "test-model", 0, &empty_map());

        // Must produce at least one empty text block
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, ""),
            _ => panic!("expected empty text fallback"),
        }
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    // ---- test_stop_reason_forced_to_tool_use ----

    #[test]
    fn test_stop_reason_forced_to_tool_use() {
        // Even if response.stop_reason is "end_turn", FunctionCall presence
        // forces "tool_use".
        let resp = make_response(
            vec![OutputItem::FunctionCall {
                call_id: "call_force".into(),
                name: "do_thing".into(),
                arguments: "{}".into(),
            }],
            Some("end_turn"), // would normally produce end_turn
        );

        let result = responses_to_claude(&resp, "test-model", 0, &empty_map());
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
    }

    // ---- test_stop_reason_max_tokens ----

    #[test]
    fn test_stop_reason_max_tokens() {
        let resp = make_response(
            vec![OutputItem::Message {
                content: vec![ContentPart::OutputText {
                    text: "truncated".into(),
                }],
            }],
            Some("max_tokens"),
        );

        let result = responses_to_claude(&resp, "test-model", 0, &empty_map());
        assert_eq!(result.stop_reason.as_deref(), Some("max_tokens"));
    }

    // ---- test_malformed_tool_arguments_falls_back ----

    #[test]
    fn test_malformed_tool_arguments_falls_back() {
        let resp = make_response(
            vec![OutputItem::FunctionCall {
                call_id: "call_bad".into(),
                name: "bad_tool".into(),
                arguments: "not valid json {{{".into(),
            }],
            None,
        );

        let result = responses_to_claude(&resp, "test-model", 0, &empty_map());
        match &result.content[0] {
            ResponseContentBlock::ToolUse { input, .. } => {
                assert_eq!(input["raw_arguments"], "not valid json {{{");
            }
            _ => panic!("expected tool_use block"),
        }
    }

    // ---- test_tool_name_restoration ----

    #[test]
    fn test_tool_name_restoration() {
        use crate::types::claude::Tool;

        let tools = vec![Tool {
            name: "GetWeather".into(),
            description: None,
            input_schema: serde_json::json!({}),
        }];
        let map = tool_name::build_map(Some(&tools));

        // Upstream returns lowercase version of the tool name
        let resp = make_response(
            vec![OutputItem::FunctionCall {
                call_id: "call_restore".into(),
                name: "getweather".into(), // mutated by upstream
                arguments: "{}".into(),
            }],
            None,
        );

        let result = responses_to_claude(&resp, "test-model", 0, &map);
        match &result.content[0] {
            ResponseContentBlock::ToolUse { name, .. } => {
                assert_eq!(name, "GetWeather"); // restored
            }
            _ => panic!("expected tool_use block"),
        }
    }
}
