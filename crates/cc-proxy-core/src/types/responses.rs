use serde::{Deserialize, Serialize};

// ===== Request Types =====

/// Top-level request body for the OpenAI Responses API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// Target model name (already mapped by `model_map::map_model`).
    pub model: String,
    /// Optional instructions string (left empty; system is carried as a developer
    /// role message inside `input` per the CPA translation algorithm).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Ordered list of conversation turns and tool call records.
    pub input: Vec<InputItem>,
    /// Available tools/functions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    /// Reasoning / thinking configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// Whether to allow parallel tool calls (default true).
    pub parallel_tool_calls: bool,
    /// Whether to persist the conversation in server-side storage (always false).
    pub store: bool,
    /// Extra fields to include in the response (e.g. `["reasoning.encrypted_content"]`).
    pub include: Vec<String>,
    /// Whether to stream the response.
    pub stream: bool,
    /// Maximum tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// A single element of the `input` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputItem {
    /// A conversation message (user / assistant / developer).
    #[serde(rename = "message")]
    Message(MessageItem),
    /// A function call made by the assistant.
    #[serde(rename = "function_call")]
    FunctionCall(FunctionCallItem),
    /// The output of a function call.
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionCallOutputItem),
}

/// A message-type input item with a role and content parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageItem {
    /// Conversation role: `"developer"`, `"user"`, or `"assistant"`.
    pub role: String,
    /// Ordered list of content parts.
    pub content: Vec<ContentPart>,
}

/// A content part within a message or function call output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    /// Plain text from a non-assistant turn.
    #[serde(rename = "input_text")]
    InputText { text: String },
    /// Plain text from an assistant turn.
    #[serde(rename = "output_text")]
    OutputText { text: String },
    /// An image encoded as a data URL (`data:<mime>;base64,<data>`).
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

/// A top-level function call item (promoted from `tool_use` content block).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallItem {
    /// The call identifier (matches `call_id` in the corresponding output item).
    pub call_id: String,
    /// Function name (may be shortened to ≤64 characters).
    pub name: String,
    /// JSON-serialised function arguments.
    pub arguments: String,
}

/// A top-level function call output item (promoted from `tool_result` content block).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    /// The call identifier this output corresponds to.
    pub call_id: String,
    /// Output content — either a simple text string or an array of content parts.
    pub output: FunctionCallOutput,
}

/// Output payload of a function call — text shorthand or structured parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FunctionCallOutput {
    /// Structured multi-part output (the common case after translation).
    Parts(Vec<ContentPart>),
    /// Plain-text shorthand.
    Text(String),
}

/// A tool available to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTool {
    /// Tool type: `"function"` or `"web_search"`.
    #[serde(rename = "type")]
    pub tool_type: String,
    /// Function name (only present for `type == "function"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the function parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Whether to enforce strict JSON Schema validation (always false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Reasoning / thinking configuration for the Responses API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// Effort level: `"none"` / `"minimal"` / `"low"` / `"medium"` / `"high"` / `"xhigh"` / `"auto"`.
    pub effort: String,
    /// Summary mode (`"auto"`).
    pub summary: String,
}

// ===== Response Types =====

/// Top-level non-streaming response from the Responses API.
///
/// All optional fields use `#[serde(default)]` because upstream providers
/// vary wildly in which fields they include — some omit `stop_reason`
/// entirely, some return `model` as null, etc. We want permissive decoding
/// so a missing optional never breaks the whole response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: ResponsesUsage,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// A single element of the `output` array in a completed response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    /// A text / tool-use message from the assistant.
    #[serde(rename = "message")]
    Message { content: Vec<ContentPart> },
    /// A reasoning / thinking block.
    #[serde(rename = "reasoning")]
    Reasoning {
        /// Summary text parts (array form).
        #[serde(default)]
        summary: Vec<serde_json::Value>,
        /// Raw encrypted thinking content (string form, may be absent).
        #[serde(default)]
        content: Option<String>,
    },
    /// A function call record.
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
}

/// Token usage returned in a Responses API response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponsesUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub input_tokens_details: InputTokensDetails,
}

/// Breakdown of input token usage.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

// ===== Streaming Event Types =====

/// A server-sent event from the Responses API streaming endpoint.
///
/// All 12 `response.*` event types are represented here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesStreamEvent {
    /// Stream started; carries initial response metadata.
    #[serde(rename = "response.created")]
    Created { response: ResponseCreatedPayload },

    /// A reasoning summary block has started.
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {},

    /// A delta within a reasoning summary block.
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { delta: String },

    /// The current reasoning summary block has finished.
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {},

    /// A text content part has started.
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {},

    /// A delta within a text content part.
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },

    /// The current text content part has finished.
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {},

    /// An output item (message or function call) has started.
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OutputItemAddedPayload },

    /// A delta for function call arguments.
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { delta: String },

    /// Final (complete) function call arguments.
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone { arguments: String },

    /// The current output item has finished.
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {},

    /// The full response has completed; carries usage and stop reason.
    #[serde(rename = "response.completed")]
    Completed { response: ResponseCompletedPayload },
}

/// Payload carried by the `response.created` event.
///
/// All fields use `#[serde(default)]` so an upstream that omits `id` or
/// `model` at stream-start doesn't cause the whole `Created` variant to
/// fail to deserialize (which would make `parse_responses_sse_stream` drop
/// the event silently and leave Claude Code hanging without a
/// `message_start`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCreatedPayload {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
}

/// Payload carried by the `response.output_item.added` event.
///
/// `item_type` uses `#[serde(default)]` so a missing `type` field doesn't
/// silently drop the whole `OutputItemAdded` event — an empty `item_type`
/// will route to the no-op branch in `responses_stream_to_claude`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputItemAddedPayload {
    /// Type of the item being added (`"message"` or `"function_call"`).
    #[serde(default, rename = "type")]
    pub item_type: String,
    /// Call ID (only present when `item_type == "function_call"`).
    #[serde(default)]
    pub call_id: Option<String>,
    /// Function name (only present when `item_type == "function_call"`).
    #[serde(default)]
    pub name: Option<String>,
}

/// Payload carried by the `response.completed` event.
///
/// All fields permissive — see `ResponseCreatedPayload` rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCompletedPayload {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
    #[serde(default)]
    pub output: Vec<OutputItem>,
}
