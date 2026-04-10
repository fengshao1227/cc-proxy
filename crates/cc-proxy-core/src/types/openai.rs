use serde::{Deserialize, Serialize};

// ===== Request Types =====

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Reasoning effort for thinking models (none/low/medium/high/xhigh)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ChatFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

// ===== Response Types =====

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub message: ChoiceMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChoiceMessage {
    /// Assistant message content.
    ///
    /// Most OpenAI-compat providers return a string here. Some newer models
    /// (GPT-5, o1/o3, and certain Chinese providers) return an array of
    /// structured parts (`{"type":"text","text":...}`, `{"type":"reasoning",...}`,
    /// etc.). We accept both via an untagged enum and centralize extraction
    /// in `AssistantContent::as_text`.
    #[serde(default)]
    pub content: AssistantContent,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Flexible container for the `message.content` field in OpenAI-compat
/// responses. Accepts the legacy string form, the newer array form, or
/// a JSON `null` / missing field.
///
/// We deserialize by inspecting the raw JSON value so that `null`,
/// absent fields, and unexpected shapes all map cleanly to `Empty` —
/// `#[serde(untagged)]` can't express "null → Empty" directly.
#[derive(Debug, Clone, Default)]
pub enum AssistantContent {
    Text(String),
    Parts(Vec<serde_json::Value>),
    #[default]
    Empty,
}

impl<'de> Deserialize<'de> for AssistantContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value {
            serde_json::Value::Null => AssistantContent::Empty,
            serde_json::Value::String(s) => AssistantContent::Text(s),
            serde_json::Value::Array(parts) => AssistantContent::Parts(parts),
            // Unexpected shape (object, number, bool). Don't explode —
            // just treat as empty and let the rest of the response pass.
            _ => AssistantContent::Empty,
        })
    }
}

impl AssistantContent {
    /// Extract the textual portion of the content, joining multiple text
    /// parts with an empty separator so they concatenate naturally.
    /// Returns `None` if the content is empty, missing, or contains no
    /// textual parts at all.
    pub fn as_text(&self) -> Option<String> {
        match self {
            AssistantContent::Text(s) => {
                if s.is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            }
            AssistantContent::Parts(parts) => {
                let mut out = String::new();
                for part in parts {
                    // We accept any part whose `type` is "text" and pull its
                    // `text` field. Unknown part types (reasoning, image,
                    // tool_calls inside content, etc.) are handled elsewhere:
                    //  - nested tool_calls → extract_nested_tool_calls()
                    //  - reasoning        → not forwarded (out of scope)
                    if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            out.push_str(text);
                        }
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
            AssistantContent::Empty => None,
        }
    }

    /// Extract tool_calls that are **nested inside** the content array.
    ///
    /// Most OpenAI-compat providers put tool_calls in `message.tool_calls`
    /// (alongside `message.content`). A few quirky providers (early GLM-4,
    /// some Qwen2 fine-tunes, certain vLLM builds) instead nest them
    /// *inside* the content array as `{"type":"tool_calls","tool_calls":[...]}`
    /// entries. This method pulls those out so the caller can convert them
    /// into Claude `tool_use` blocks exactly like top-level tool_calls.
    ///
    /// Returns an empty Vec for the common case (Text / Empty / Parts
    /// without nested tool_calls), so callers can always iterate safely.
    ///
    /// Algorithm adapted from CLIProxyAPI (MIT) — `ConvertOpenAIResponseToClaudeNonStream`
    /// content-array handling in `openai_claude_response.go`.
    pub fn extract_nested_tool_calls(&self) -> Vec<ToolCall> {
        let AssistantContent::Parts(parts) = self else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for part in parts {
            if part.get("type").and_then(|v| v.as_str()) != Some("tool_calls") {
                continue;
            }
            let Some(nested) = part.get("tool_calls").and_then(|v| v.as_array()) else {
                continue;
            };
            for tc in nested {
                // Each nested entry mirrors the top-level OpenAI tool_call
                // shape: {id, type:"function", function:{name, arguments}}.
                // Deserialize via serde so we reuse the same ToolCall struct
                // and get free validation.
                if let Ok(parsed) = serde_json::from_value::<ToolCall>(tc.clone()) {
                    out.push(parsed);
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

// ===== Streaming Types =====

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolCall {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkFunction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}
