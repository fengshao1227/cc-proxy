use std::collections::{HashMap, HashSet};

use crate::config::ProxyConfig;
use crate::convert::request::resolve_reasoning_effort;
use crate::model_map;
use crate::types::claude::{
    ContentBlock, Message, MessageContent, MessagesRequest, SystemContent, ToolResultContent,
};
use crate::types::responses::{
    ContentPart, FunctionCallItem, FunctionCallOutput, FunctionCallOutputItem, InputItem,
    MessageItem, ReasoningConfig, ResponsesRequest, ResponsesTool,
};

/// Prefix used to identify Anthropic billing-header strings that must be
/// filtered out of system content before forwarding to the Responses API.
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header: ";

/// Maximum allowed length for a tool / function name in the Responses API.
const TOOL_NAME_LIMIT: usize = 64;

/// Fixed `include` field required by the Responses API so that encrypted
/// reasoning content is returned and can be forwarded in multi-turn requests.
const INCLUDE_REASONING: &str = "reasoning.encrypted_content";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Translate a Claude Messages API request into an OpenAI Responses API request.
///
/// This follows the CPA algorithm documented in the Scout hand-book:
/// system → developer message, messages loop with flush, tools normalisation,
/// thinking → reasoning.effort, and fixed envelope fields.
pub fn claude_to_responses(req: &MessagesRequest, config: &ProxyConfig) -> ResponsesRequest {
    let mapped = model_map::map_model(&req.model, config);

    // Build the original→short name map from tools (needed when promoting tool_use).
    let tool_names: Vec<String> = req
        .tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    let short_map = build_short_name_map(&name_refs);

    let mut input: Vec<InputItem> = Vec::new();

    // ------------------------------------------------------------------
    // 1. System → developer role message
    // ------------------------------------------------------------------
    if let Some(ref system) = req.system {
        let parts = extract_system_parts(system);
        if !parts.is_empty() {
            input.push(InputItem::Message(MessageItem {
                role: "developer".into(),
                content: parts,
            }));
        }
    }

    // ------------------------------------------------------------------
    // 2. Messages loop with flushMessage mechanism
    // ------------------------------------------------------------------
    for msg in &req.messages {
        process_message(msg, &short_map, &mut input);
    }

    // ------------------------------------------------------------------
    // 3. Tools
    // ------------------------------------------------------------------
    let tools = build_tools(req, &short_map);

    // ------------------------------------------------------------------
    // 4. Reasoning effort
    // ------------------------------------------------------------------
    let reasoning = build_reasoning(req, config, mapped.tier);

    // ------------------------------------------------------------------
    // 5. Parallel tool calls
    // ------------------------------------------------------------------
    let parallel_tool_calls = !is_parallel_tool_calls_disabled(&req.tool_choice);

    // ------------------------------------------------------------------
    // 6. Assemble envelope
    // ------------------------------------------------------------------
    ResponsesRequest {
        model: mapped.model,
        instructions: None,
        input,
        tools,
        reasoning,
        parallel_tool_calls,
        store: false,
        include: vec![INCLUDE_REASONING.to_string()],
        stream: true,
        max_output_tokens: Some(req.max_tokens),
    }
}

// ---------------------------------------------------------------------------
// System field processing
// ---------------------------------------------------------------------------

/// Extract content parts from the Claude `system` field, filtering out
/// billing-header strings and non-text blocks.
fn extract_system_parts(system: &SystemContent) -> Vec<ContentPart> {
    match system {
        SystemContent::Text(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.starts_with(BILLING_HEADER_PREFIX) {
                vec![]
            } else {
                vec![ContentPart::InputText {
                    text: trimmed.to_string(),
                }]
            }
        }
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .filter(|text| {
                let t = text.trim();
                !t.is_empty() && !t.starts_with(BILLING_HEADER_PREFIX)
            })
            .map(|text| ContentPart::InputText {
                text: text.trim().to_string(),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Message processing (core loop + flushMessage)
// ---------------------------------------------------------------------------

/// State carried while converting a single Claude message.
struct MsgState {
    role: String,
    content: Vec<ContentPart>,
    has_content: bool,
}

impl MsgState {
    fn new(role: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Vec::new(),
            has_content: false,
        }
    }

    /// Emit the accumulated message into `out` and reset state.
    fn flush(&mut self, out: &mut Vec<InputItem>) {
        if self.has_content {
            out.push(InputItem::Message(MessageItem {
                role: self.role.clone(),
                content: std::mem::take(&mut self.content),
            }));
            self.has_content = false;
        }
    }
}

/// Process one Claude message, appending items to `out`.
fn process_message(msg: &Message, short_map: &HashMap<String, String>, out: &mut Vec<InputItem>) {
    let role = msg.role.as_str();
    let mut state = MsgState::new(role);

    let blocks = match &msg.content {
        MessageContent::Text(s) => {
            // Plain-text message — append directly, no flush needed.
            let part = text_part(s, role);
            state.content.push(part);
            state.has_content = true;
            state.flush(out);
            return;
        }
        MessageContent::Blocks(b) => b,
        MessageContent::Null => {
            return;
        }
    };

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                state.content.push(text_part(text, role));
                state.has_content = true;
            }
            ContentBlock::Image { source } => {
                // Build data URL from base64 source.
                let media_type = source
                    .media_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                let data = source.data.as_deref().unwrap_or("");
                let url = format!("data:{media_type};base64,{data}");
                state
                    .content
                    .push(ContentPart::InputImage { image_url: url });
                state.has_content = true;
            }
            ContentBlock::ToolUse { id, name, input } => {
                // Flush any pending text content first.
                state.flush(out);

                // Map original name → short name.
                let short_name = short_map
                    .get(name.as_str())
                    .cloned()
                    .unwrap_or_else(|| shorten_name_if_needed(name));

                let arguments = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());

                out.push(InputItem::FunctionCall(FunctionCallItem {
                    call_id: id.clone(),
                    name: short_name,
                    arguments,
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                // Flush any pending text content first.
                state.flush(out);

                let output_parts = tool_result_to_parts(content.as_ref());
                out.push(InputItem::FunctionCallOutput(FunctionCallOutputItem {
                    call_id: tool_use_id.clone(),
                    output: FunctionCallOutput::Parts(output_parts),
                }));
            }
        }
    }

    state.flush(out);
}

/// Choose `input_text` or `output_text` based on the message role.
fn text_part(text: &str, role: &str) -> ContentPart {
    if role == "assistant" {
        ContentPart::OutputText {
            text: text.to_string(),
        }
    } else {
        ContentPart::InputText {
            text: text.to_string(),
        }
    }
}

/// Convert a `tool_result` content payload to a `Vec<ContentPart>`.
fn tool_result_to_parts(content: Option<&ToolResultContent>) -> Vec<ContentPart> {
    match content {
        None => vec![],
        Some(ToolResultContent::Text(s)) => vec![ContentPart::InputText { text: s.clone() }],
        Some(ToolResultContent::Blocks(items)) => items
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match item_type {
                    "text" => {
                        let text = item
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(ContentPart::InputText { text })
                    }
                    "image" => {
                        // Extract base64 image from Claude image block inside tool_result.
                        let source = item.get("source")?;
                        let media_type = source
                            .get("media_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("application/octet-stream");
                        let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                        let url = format!("data:{media_type};base64,{data}");
                        Some(ContentPart::InputImage { image_url: url })
                    }
                    _ => None,
                }
            })
            .collect(),
        Some(ToolResultContent::Object(v)) => {
            // Treat as a single JSON value — extract text if present.
            let text = if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                t.to_string()
            } else {
                serde_json::to_string(v).unwrap_or_default()
            };
            vec![ContentPart::InputText { text }]
        }
    }
}

// ---------------------------------------------------------------------------
// Tools processing
// ---------------------------------------------------------------------------

/// Build the Responses API `tools` array from the Claude request.
fn build_tools(
    req: &MessagesRequest,
    short_map: &HashMap<String, String>,
) -> Option<Vec<ResponsesTool>> {
    let tools = req.tools.as_deref()?;
    if tools.is_empty() {
        return None;
    }

    let converted: Vec<ResponsesTool> = tools
        .iter()
        .map(|tool| {
            // Special case: web_search tool → minimal `{"type":"web_search"}`.
            let tool_type_raw = tool
                .input_schema
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // The Claude tool name field tells us if it's web_search_20250305.
            // The CPA algorithm checks the `type` field of the tool object itself
            // (not input_schema). Claude API tools don't have a `type` field in
            // the Tool struct — the type is always "function" for normal tools.
            // web_search_20250305 is identified by the tool name convention.
            if tool.name == "web_search_20250305" || tool_type_raw == "web_search_20250305" {
                return ResponsesTool {
                    tool_type: "web_search".into(),
                    name: None,
                    description: None,
                    parameters: None,
                    strict: None,
                };
            }

            let short_name = short_map
                .get(tool.name.as_str())
                .cloned()
                .unwrap_or_else(|| shorten_name_if_needed(&tool.name));

            let parameters = normalize_tool_parameters(&tool.input_schema);

            ResponsesTool {
                tool_type: "function".into(),
                name: Some(short_name),
                description: tool.description.clone(),
                parameters: Some(parameters),
                strict: Some(false),
            }
        })
        .collect();

    Some(converted)
}

/// Normalise a JSON Schema value for use as `parameters` in the Responses API.
///
/// Rules (per CPA `normalizeToolParameters`):
/// - Ensure `"type"` field is present (default `"object"`).
/// - If `type == "object"` and `"properties"` is absent, add `"properties": {}`.
/// - Remove `$schema`, `cache_control`, `defer_loading` fields.
pub fn normalize_tool_parameters(schema: &serde_json::Value) -> serde_json::Value {
    if schema.is_null() {
        return serde_json::json!({"type": "object", "properties": {}});
    }

    let mut obj = match schema {
        serde_json::Value::Object(m) => m.clone(),
        _ => {
            return serde_json::json!({"type": "object", "properties": {}});
        }
    };

    // Remove fields that Responses API rejects.
    obj.remove("$schema");
    obj.remove("cache_control");
    obj.remove("defer_loading");

    // Ensure "type" is present.
    if !obj.contains_key("type") {
        obj.insert("type".into(), serde_json::Value::String("object".into()));
    }

    // If type is "object" and "properties" is missing, add empty properties.
    if obj.get("type").and_then(|v| v.as_str()) == Some("object") && !obj.contains_key("properties")
    {
        obj.insert(
            "properties".into(),
            serde_json::Value::Object(Default::default()),
        );
    }

    serde_json::Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Reasoning / thinking configuration
// ---------------------------------------------------------------------------

/// Convert a Claude `thinking` config + proxy config into a Responses API
/// `ReasoningConfig`.
///
/// Translation table (per Scout hand-book §2.5):
/// - `thinking.enabled == true`  → effort from `resolve_reasoning_effort` or `"medium"`
/// - `thinking.enabled == false` → effort `"none"`
/// - `thinking` absent           → effort from global config or `"none"`
fn build_reasoning(
    req: &MessagesRequest,
    config: &ProxyConfig,
    tier: Option<crate::config::ModelTier>,
) -> Option<ReasoningConfig> {
    let effort = match &req.thinking {
        Some(thinking) if thinking.enabled => {
            resolve_reasoning_effort(req, config, tier).unwrap_or_else(|| "medium".to_string())
        }
        Some(_) => "none".to_string(),
        None => resolve_reasoning_effort(req, config, tier)?,
    };

    Some(ReasoningConfig {
        effort,
        summary: "auto".into(),
    })
}

// ---------------------------------------------------------------------------
// Parallel tool calls
// ---------------------------------------------------------------------------

/// Return `true` if `tool_choice.disable_parallel_tool_use` is explicitly `true`.
fn is_parallel_tool_calls_disabled(tool_choice: &Option<serde_json::Value>) -> bool {
    tool_choice
        .as_ref()
        .and_then(|v| v.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tool name shortening helpers
// ---------------------------------------------------------------------------

/// UTF-8 safe byte-level truncation.
///
/// The CPA algorithm operates in byte space (Go's `s[:64]` is byte indexing),
/// so we match that by capping at `max_bytes` — but we walk backwards from
/// `max_bytes` to the nearest char boundary so we never cut a multi-byte
/// UTF-8 sequence in half (which would panic on `&str` slicing).
fn truncate_bytes_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    // `is_char_boundary(0)` is always true, so this loop always terminates.
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Shorten a single tool name to at most [`TOOL_NAME_LIMIT`] bytes.
///
/// Algorithm (per CPA `shortenNameIfNeeded`):
/// 1. If `len <= LIMIT`, return as-is.
/// 2. If name starts with `"mcp__"`, take the suffix after the last `"__"` and
///    prepend `"mcp__"`. If the result is still too long, truncate to LIMIT.
/// 3. Otherwise truncate directly to LIMIT.
///
/// All truncations use [`truncate_bytes_safe`] so non-ASCII tool names
/// (e.g. Chinese, emoji) don't panic on the slice boundary.
pub fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= TOOL_NAME_LIMIT {
        return name.to_string();
    }

    if name.starts_with("mcp__") {
        // Find the *last* `__` separator.
        if let Some(last_idx) = name.rfind("__") {
            if last_idx > 0 {
                let suffix = &name[last_idx + 2..];
                let candidate = format!("mcp__{suffix}");
                return if candidate.len() <= TOOL_NAME_LIMIT {
                    candidate
                } else {
                    truncate_bytes_safe(&candidate, TOOL_NAME_LIMIT)
                };
            }
        }
    }

    // Default: direct truncation.
    truncate_bytes_safe(name, TOOL_NAME_LIMIT)
}

/// Build the server-side tool-name restoration map for the Responses path.
///
/// Unlike ChatCompletions — which sends tool names verbatim — the Responses
/// path may **shorten** names over 64 bytes before sending them upstream
/// (see [`shorten_name_if_needed`] / [`build_short_name_map`]). If a tool
/// called `very_long_mcp_tool_with_really_long_namespace__do_the_thing`
/// becomes `mcp__do_the_thing` on the wire, the upstream will echo back
/// `mcp__do_the_thing` in its `function_call.name` field — and the normal
/// `util::tool_name::build_map` (which only knows the original long name)
/// can no longer restore it.
///
/// This helper builds a combined map that **additionally** contains
/// `canonical(short_name) → original_name` entries for every tool whose
/// name was shortened, so the response-side `tool_name::restore` call
/// succeeds in both directions.
pub fn build_tool_name_map(
    tools: Option<&[crate::types::claude::Tool]>,
) -> crate::util::tool_name::ToolNameMap {
    use crate::util::tool_name::{build_map, canonical};

    // Start with the standard canonical(original) → original entries.
    let mut map = build_map(tools);

    let Some(tools) = tools else { return map };

    // Compute the short-name aliases the request path will use upstream,
    // then also register canonical(short) → original for each shortened tool.
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let short_map = build_short_name_map(&names);
    for (original, short) in short_map {
        if short != original {
            let key = canonical(&short);
            if !key.is_empty() {
                // Don't overwrite if another original already claimed this
                // key (shouldn't happen in practice, but be conservative).
                map.entry(key).or_insert(original);
            }
        }
    }

    map
}

/// Build an `original → short_name` map for all tool names.
///
/// Ensures all short names are unique by appending `_1`, `_2`, … on collision.
pub fn build_short_name_map(names: &[&str]) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut map: HashMap<String, String> = HashMap::new();

    for &name in names {
        let base = shorten_name_if_needed(name);
        let unique = make_unique_name(&base, &mut used);
        used.insert(unique.clone());
        map.insert(name.to_string(), unique);
    }

    map
}

/// Return `base` if not already used, otherwise append `_1`, `_2`, … until unique.
fn make_unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }

    for i in 1..10_000usize {
        let suffix = format!("_{i}");
        let allowed = TOOL_NAME_LIMIT.saturating_sub(suffix.len());
        // UTF-8 safe truncation — `String::truncate` would panic if `allowed`
        // lands in the middle of a multi-byte sequence.
        let mut candidate = truncate_bytes_safe(base, allowed);
        candidate.push_str(&suffix);
        if !used.contains(&candidate) {
            return candidate;
        }
    }

    base.to_string() // unreachable in practice
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

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

    // ---------- helpers ----------

    fn first_input_message(req: &ResponsesRequest) -> &MessageItem {
        match &req.input[0] {
            InputItem::Message(m) => m,
            _ => panic!("expected message at index 0"),
        }
    }

    // ---------- system field ----------

    #[test]
    fn test_system_string() {
        let config = test_config();
        let mut req = base_request();
        req.system = Some(SystemContent::Text("Be helpful".into()));

        let result = claude_to_responses(&req, &config);
        assert_eq!(result.input.len(), 1);
        let dev = first_input_message(&result);
        assert_eq!(dev.role, "developer");
        assert_eq!(dev.content.len(), 1);
        match &dev.content[0] {
            ContentPart::InputText { text } => assert_eq!(text, "Be helpful"),
            _ => panic!("expected InputText"),
        }
    }

    #[test]
    fn test_system_array_multiple_texts() {
        let config = test_config();
        let mut req = base_request();
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                block_type: "text".into(),
                text: Some("Block one".into()),
                cache_control: None,
            },
            SystemBlock {
                block_type: "text".into(),
                text: Some("Block two".into()),
                cache_control: None,
            },
        ]));

        let result = claude_to_responses(&req, &config);
        let dev = first_input_message(&result);
        assert_eq!(dev.role, "developer");
        assert_eq!(dev.content.len(), 2);
    }

    #[test]
    fn test_system_filters_billing_header() {
        let config = test_config();
        let mut req = base_request();
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                block_type: "text".into(),
                text: Some("x-anthropic-billing-header: tenant-abc".into()),
                cache_control: None,
            },
            SystemBlock {
                block_type: "text".into(),
                text: Some("Valid system text".into()),
                cache_control: None,
            },
        ]));

        let result = claude_to_responses(&req, &config);
        let dev = first_input_message(&result);
        assert_eq!(dev.content.len(), 1);
        match &dev.content[0] {
            ContentPart::InputText { text } => assert_eq!(text, "Valid system text"),
            _ => panic!("expected InputText"),
        }
    }

    // ---------- messages ----------

    #[test]
    fn test_message_text_only() {
        let config = test_config();
        let mut req = base_request();
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text("Hello".into()),
        });

        let result = claude_to_responses(&req, &config);
        assert_eq!(result.input.len(), 1);
        let msg = first_input_message(&result);
        assert_eq!(msg.role, "user");
        match &msg.content[0] {
            ContentPart::InputText { text } => assert_eq!(text, "Hello"),
            _ => panic!("expected InputText"),
        }
    }

    #[test]
    fn test_message_with_image() {
        let config = test_config();
        let mut req = base_request();
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Look at this".into(),
                },
                ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: Some("image/png".into()),
                        data: Some("iVBORw0KGgo=".into()),
                    },
                },
            ]),
        });

        let result = claude_to_responses(&req, &config);
        let msg = first_input_message(&result);
        assert_eq!(msg.content.len(), 2);
        match &msg.content[1] {
            ContentPart::InputImage { image_url } => {
                assert_eq!(image_url, "data:image/png;base64,iVBORw0KGgo=");
            }
            _ => panic!("expected InputImage"),
        }
    }

    // ---------- tool_use promoted ----------

    #[test]
    fn test_tool_use_promoted_to_top_level() {
        let config = test_config();
        let mut req = base_request();
        req.messages.push(Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_123".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"location": "NYC"}),
                },
            ]),
        });

        let result = claude_to_responses(&req, &config);
        // Text message + function_call item
        assert_eq!(result.input.len(), 2);
        match &result.input[0] {
            InputItem::Message(m) => {
                assert_eq!(m.role, "assistant");
                match &m.content[0] {
                    ContentPart::OutputText { text } => assert_eq!(text, "Let me check."),
                    _ => panic!("expected OutputText"),
                }
            }
            _ => panic!("expected Message first"),
        }
        match &result.input[1] {
            InputItem::FunctionCall(fc) => {
                assert_eq!(fc.call_id, "toolu_123");
                assert_eq!(fc.name, "get_weather");
                let args: serde_json::Value = serde_json::from_str(&fc.arguments).unwrap();
                assert_eq!(args["location"], "NYC");
            }
            _ => panic!("expected FunctionCall second"),
        }
    }

    #[test]
    fn test_tool_result_promoted_to_top_level() {
        let config = test_config();
        let mut req = base_request();
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_123".into(),
                content: Some(ToolResultContent::Text("Sunny 72F".into())),
            }]),
        });

        let result = claude_to_responses(&req, &config);
        assert_eq!(result.input.len(), 1);
        match &result.input[0] {
            InputItem::FunctionCallOutput(fco) => {
                assert_eq!(fco.call_id, "toolu_123");
                match &fco.output {
                    FunctionCallOutput::Parts(parts) => {
                        assert_eq!(parts.len(), 1);
                        match &parts[0] {
                            ContentPart::InputText { text } => assert_eq!(text, "Sunny 72F"),
                            _ => panic!("expected InputText in output"),
                        }
                    }
                    _ => panic!("expected Parts"),
                }
            }
            _ => panic!("expected FunctionCallOutput"),
        }
    }

    // ---------- tools normalisation ----------

    #[test]
    fn test_tools_input_schema_normalized() {
        let config = test_config();
        let mut req = base_request();
        req.tools = Some(vec![Tool {
            name: "do_thing".into(),
            description: Some("Does a thing".into()),
            input_schema: serde_json::json!({"type": "object"}),
        }]);
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text("hi".into()),
        });

        let result = claude_to_responses(&req, &config);
        let tools = result.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        let params = tools[0].parameters.as_ref().unwrap();
        // Should have injected "properties": {}
        assert!(params.get("properties").is_some());
    }

    #[test]
    fn test_tools_web_search_special_handling() {
        let config = test_config();
        let mut req = base_request();
        req.tools = Some(vec![Tool {
            name: "web_search_20250305".into(),
            description: None,
            input_schema: serde_json::json!({}),
        }]);
        req.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text("search".into()),
        });

        let result = claude_to_responses(&req, &config);
        let tools = result.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "web_search");
        assert!(tools[0].name.is_none());
    }

    // ---------- tool name shortening ----------

    #[test]
    fn test_tool_name_shortened_over_64_chars() {
        let long2 = "mcp__some_server__a_very_long_function_name_that_clearly_exceeds_limit";
        let result = shorten_name_if_needed(long2);
        assert!(result.len() <= TOOL_NAME_LIMIT, "got len {}", result.len());
    }

    #[test]
    fn test_tool_name_short_map_uniqueness() {
        // Two different long names that would shorten to the same candidate.
        let names = [
            "mcp__serverA__duplicate_suffix",
            "mcp__serverB__duplicate_suffix",
        ];
        let refs: Vec<&str> = names.iter().map(|s| s.as_ref()).collect();
        let map = build_short_name_map(&refs);
        let values: Vec<&String> = map.values().collect();
        // All values must be distinct.
        let unique: HashSet<&&String> = values.iter().collect();
        assert_eq!(unique.len(), values.len(), "short names must be unique");
        // All short names must be within the limit.
        for v in &values {
            assert!(v.len() <= TOOL_NAME_LIMIT, "name too long: {v}");
        }
    }

    // ---------- thinking / reasoning ----------

    #[test]
    fn test_thinking_enabled_budget_tokens() {
        // When thinking.enabled = true, reasoning should be present.
        let config = test_config(); // reasoning_effort = "none"
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: true });

        let result = claude_to_responses(&req, &config);
        let r = result.reasoning.as_ref().expect("reasoning should be set");
        // Global effort is "none" but thinking.enabled=true → default "medium".
        assert_eq!(r.effort, "medium");
        assert_eq!(r.summary, "auto");
    }

    #[test]
    fn test_thinking_adaptive_with_effort() {
        // When global config has a non-"none" effort AND thinking.enabled=true,
        // use the config effort.
        let mut config = test_config();
        config.reasoning_effort = "high".into();
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: true });

        let result = claude_to_responses(&req, &config);
        let r = result.reasoning.as_ref().unwrap();
        assert_eq!(r.effort, "high");
    }

    #[test]
    fn test_thinking_disabled() {
        let config = test_config();
        let mut req = base_request();
        req.thinking = Some(ThinkingConfig { enabled: false });

        let result = claude_to_responses(&req, &config);
        let r = result
            .reasoning
            .as_ref()
            .expect("reasoning present even when none");
        assert_eq!(r.effort, "none");
    }

    // ---------- parallel tool calls ----------

    #[test]
    fn test_parallel_tool_calls_default_true() {
        let config = test_config();
        let req = base_request();
        let result = claude_to_responses(&req, &config);
        assert!(result.parallel_tool_calls);
    }

    #[test]
    fn test_parallel_tool_calls_disabled_by_tool_choice() {
        let config = test_config();
        let mut req = base_request();
        req.tool_choice = Some(serde_json::json!({
            "type": "auto",
            "disable_parallel_tool_use": true
        }));

        let result = claude_to_responses(&req, &config);
        assert!(!result.parallel_tool_calls);
    }

    // ---------- normalize_tool_parameters ----------

    #[test]
    fn test_normalize_tool_parameters_adds_properties() {
        let schema = serde_json::json!({"type": "object"});
        let result = normalize_tool_parameters(&schema);
        assert!(result.get("properties").is_some());
    }

    #[test]
    fn test_normalize_tool_parameters_null_input() {
        let result = normalize_tool_parameters(&serde_json::Value::Null);
        assert_eq!(result["type"], "object");
        assert!(result.get("properties").is_some());
    }

    #[test]
    fn test_normalize_tool_parameters_removes_schema_key() {
        let schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {}
        });
        let result = normalize_tool_parameters(&schema);
        assert!(result.get("$schema").is_none());
    }

    // ---------- shorten_name_if_needed unit ----------

    #[test]
    fn test_shorten_short_name_unchanged() {
        let name = "get_weather";
        assert_eq!(shorten_name_if_needed(name), name);
    }

    #[test]
    fn test_shorten_mcp_name_uses_suffix() {
        let name = "mcp__extremely_long_package__very_long_function_name_that_exceeds_limit_here";
        let result = shorten_name_if_needed(name);
        assert!(result.len() <= TOOL_NAME_LIMIT, "result: {result}");
        assert!(result.starts_with("mcp__"), "result: {result}");
    }

    #[test]
    fn test_shorten_plain_long_name_truncated() {
        let name = "a_very_long_function_name_that_is_way_over_sixty_four_characters_limit";
        let result = shorten_name_if_needed(name);
        assert_eq!(result.len(), TOOL_NAME_LIMIT);
    }

    // ---- UTF-8 safety: non-ASCII tool names must not panic ----

    #[test]
    fn test_shorten_handles_non_ascii_without_panic() {
        // A name whose 64th byte would land in the middle of a CJK char.
        // Chinese chars are 3 bytes each in UTF-8, so 22 × 3 = 66 bytes
        // comfortably crosses the boundary with room for a 64-byte cut.
        let name = "极速天气查询工具极速天气查询工具极速天气查询工具极速天气查询";
        assert!(name.len() > TOOL_NAME_LIMIT);
        let result = shorten_name_if_needed(name);
        assert!(result.len() <= TOOL_NAME_LIMIT);
        // Must be a valid UTF-8 string (implicit — if truncation cut a
        // multi-byte seq, `to_string()` would have panicked above).
        assert!(!result.is_empty());
    }

    #[test]
    fn test_shorten_handles_emoji_without_panic() {
        // Emoji are 4 bytes in UTF-8; a run of 17 = 68 bytes > LIMIT.
        let name = "🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥";
        assert!(name.len() > TOOL_NAME_LIMIT);
        let result = shorten_name_if_needed(name);
        assert!(result.len() <= TOOL_NAME_LIMIT);
    }

    #[test]
    fn test_truncate_bytes_safe_never_panics_in_middle_of_char() {
        // Walk every byte boundary on a multi-byte string and ensure the
        // helper always returns a valid String (i.e. stopped at a char
        // boundary).
        let s = "ab中文cd";
        for n in 0..=s.len() + 2 {
            let out = truncate_bytes_safe(s, n);
            // String must be valid UTF-8 (trivially true since out: String)
            // and its byte length must not exceed n.
            assert!(out.len() <= n.min(s.len()));
        }
    }

    #[test]
    fn test_make_unique_name_with_cjk_base() {
        let mut used = HashSet::new();
        let base = "搜索工具搜索工具搜索工具搜索工具搜索工具搜索工具搜索工具搜索工";
        // Pre-seed so we force the suffix path.
        used.insert(base.to_string());
        let unique = make_unique_name(base, &mut used);
        assert_ne!(unique, base);
        assert!(unique.ends_with("_1"));
        assert!(unique.len() <= TOOL_NAME_LIMIT);
    }

    // ---- build_tool_name_map: responses-side restoration ----

    #[test]
    fn test_build_tool_name_map_restores_short_alias() {
        use crate::types::claude::Tool;
        use crate::util::tool_name::restore;
        use serde_json::json;

        // A tool whose full name is over 64 bytes → gets shortened via the
        // mcp__ shortcut path.
        let long_name = "mcp__extremely_long_namespace_prefix__very_descriptive_function_name";
        assert!(long_name.len() > TOOL_NAME_LIMIT);

        let tools = vec![Tool {
            name: long_name.to_string(),
            description: None,
            input_schema: json!({}),
        }];

        let map = build_tool_name_map(Some(&tools));

        // The upstream will echo back the shortened form. Compute what that
        // shortened form is, then verify restore() recovers the original.
        let short = shorten_name_if_needed(long_name);
        assert_ne!(short, long_name, "test precondition: name should shorten");

        let restored = restore(&map, &short);
        assert_eq!(
            restored, long_name,
            "short → original restoration failed: map did not include alias"
        );

        // And the original name should still restore to itself.
        let restored_orig = restore(&map, long_name);
        assert_eq!(restored_orig, long_name);
    }

    #[test]
    fn test_build_tool_name_map_short_name_unchanged_for_simple_tools() {
        use crate::types::claude::Tool;
        use crate::util::tool_name::restore;
        use serde_json::json;

        let tools = vec![
            Tool {
                name: "Read".into(),
                description: None,
                input_schema: json!({}),
            },
            Tool {
                name: "Write".into(),
                description: None,
                input_schema: json!({}),
            },
        ];

        let map = build_tool_name_map(Some(&tools));

        // Short tools go through the same restoration as before (case fix).
        assert_eq!(restore(&map, "read"), "Read");
        assert_eq!(restore(&map, "WRITE"), "Write");
    }

    #[test]
    fn test_build_tool_name_map_none_returns_empty() {
        let map = build_tool_name_map(None);
        assert!(map.is_empty());
    }

    // ---------- fixed envelope fields ----------

    #[test]
    fn test_envelope_fixed_fields() {
        let config = test_config();
        let req = base_request();
        let result = claude_to_responses(&req, &config);
        assert!(result.stream);
        assert!(!result.store);
        assert!(result
            .include
            .contains(&"reasoning.encrypted_content".to_string()));
    }
}
