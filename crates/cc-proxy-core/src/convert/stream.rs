//! SSE streaming converter: OpenAI ChatCompletion chunks -> Claude SSE events.
//!
//! This is the core streaming pipeline of the proxy. It consumes an upstream
//! stream of OpenAI server-sent events and re-emits them as Claude-compatible
//! SSE events, maintaining a small state machine to track text blocks, tool
//! call blocks, and the final message envelope.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use axum::response::sse::Event;
use futures::stream::Stream;
use futures::StreamExt;
use serde_json::json;
use tokio::time::timeout;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::types::claude::{sse, stop_reason, Usage};
use crate::types::openai::ChatCompletionChunk;
use crate::util::{tool_id, tool_name};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed upstream event from the OpenAI SSE stream.
#[derive(Debug)]
pub enum OpenAiSseEvent {
    /// A parsed chunk from `data: {...}`.
    Chunk(ChatCompletionChunk),
    /// The terminal `data: [DONE]` sentinel.
    Done,
}

/// Errors that can occur while reading the upstream SSE stream.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("upstream connection error: {0}")]
    Connection(String),
    #[error("JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("upstream closed unexpectedly")]
    UnexpectedEof,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Tracks a single in-progress tool call being assembled from streamed deltas.
#[derive(Debug)]
struct ToolCallAccumulator {
    /// OpenAI tool call ID (e.g. "call_abc123").
    id: Option<String>,
    /// Function name (already restored via tool_name::restore).
    name: Option<String>,
    /// Accumulated raw JSON argument fragments from the upstream stream.
    /// Emitted as a single repaired `input_json_delta` during epilogue so
    /// providers that send non-standard JSON (single quotes etc.) round-trip
    /// correctly. See `emit_epilogue` and `util::fix_json`.
    args_buffer: String,
    /// The Claude content-block index assigned to this tool call.
    claude_index: Option<usize>,
    /// Whether we already emitted `content_block_start` for this tool call.
    started: bool,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self {
            id: None,
            name: None,
            args_buffer: String::new(),
            claude_index: None,
            started: false,
        }
    }
}

/// Full converter state threaded through the stream via `futures::stream::unfold`.
struct ConverterState<S> {
    upstream: Pin<Box<S>>,
    original_model: String,
    message_id: String,

    /// Index of the initial text content block (always 0).
    text_block_index: usize,
    /// Counter for tool-call blocks appended after the text block.
    tool_block_counter: usize,
    /// Tool calls keyed by their OpenAI delta index.
    tool_calls: HashMap<usize, ToolCallAccumulator>,
    /// Final stop reason collected from finish_reason.
    final_stop_reason: String,
    /// Usage data captured from the final chunk.
    usage: Usage,

    /// Phases: Prologue -> Streaming -> Epilogue -> Done
    phase: Phase,

    /// Idle timeout for waiting on upstream chunks. Zero = disabled.
    idle_timeout: Duration,
    /// Pre-computed Claude-equivalent input token estimate from original request chars.
    /// Used instead of upstream's inflated input_tokens.
    estimated_input_tokens: u32,
    /// canonical → original tool name map built from the inbound Claude request.
    /// Used to restore tool_use.name after upstream providers lowercase or
    /// otherwise mutate it. See `util::tool_name`.
    tool_name_map: tool_name::ToolNameMap,
}

#[derive(Debug, PartialEq)]
enum Phase {
    /// Emit the three opening events (message_start, content_block_start, ping).
    Prologue,
    /// Process upstream chunks one at a time.
    Streaming,
    /// Emit the closing events (content_block_stop(s), message_delta, message_stop).
    Epilogue,
    /// Terminal — no more events.
    Done,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert an OpenAI streaming response into a Claude-compatible SSE event stream.
///
/// The returned stream yields `Result<Event, Infallible>` — errors from the
/// upstream are converted into SSE error events rather than stream termination,
/// so the consumer always sees well-formed SSE.
pub fn openai_stream_to_claude(
    upstream: impl Stream<Item = Result<OpenAiSseEvent, StreamError>> + Send + 'static,
    original_model: String,
    idle_timeout: Duration,
    estimated_input_tokens: u32,
    tool_name_map: tool_name::ToolNameMap,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send {
    let message_id = generate_message_id();

    let state = ConverterState {
        upstream: Box::pin(upstream),
        original_model,
        message_id,
        text_block_index: 0,
        tool_block_counter: 0,
        tool_calls: HashMap::new(),
        final_stop_reason: stop_reason::END_TURN.to_string(),
        usage: Usage::default(),
        phase: Phase::Prologue,
        idle_timeout,
        estimated_input_tokens,
        tool_name_map,
    };

    // We collect events into a VecDeque because a single upstream chunk can
    // produce 0-N output events. `unfold` yields one item at a time, so we
    // buffer them internally.
    futures::stream::unfold(
        (state, std::collections::VecDeque::<Event>::new()),
        |(mut state, mut buf)| async move {
            loop {
                // Drain buffer first.
                if let Some(event) = buf.pop_front() {
                    return Some((Ok(event), (state, buf)));
                }

                match state.phase {
                    Phase::Prologue => {
                        emit_prologue(&state, &mut buf);
                        state.phase = Phase::Streaming;
                        // Loop back to drain buffer.
                    }
                    Phase::Streaming => {
                        // Wrap upstream read with idle timeout to prevent indefinite blocking.
                        let next = if state.idle_timeout.is_zero() {
                            state.upstream.next().await
                        } else {
                            match timeout(state.idle_timeout, state.upstream.next()).await {
                                Ok(result) => result,
                                Err(_) => {
                                    error!(
                                        "converter stream idle timeout ({}s)",
                                        state.idle_timeout.as_secs()
                                    );
                                    emit_error_event(
                                        &format!(
                                            "stream idle timeout ({}s)",
                                            state.idle_timeout.as_secs()
                                        ),
                                        &mut buf,
                                    );
                                    state.phase = Phase::Epilogue;
                                    continue;
                                }
                            }
                        };
                        match next {
                            Some(Ok(OpenAiSseEvent::Chunk(chunk))) => {
                                process_chunk(&mut state, &chunk, &mut buf);
                                // Loop back to drain buffer.
                            }
                            Some(Ok(OpenAiSseEvent::Done)) => {
                                debug!("upstream stream done");
                                state.phase = Phase::Epilogue;
                            }
                            Some(Err(e)) => {
                                error!("upstream stream error: {e}");
                                emit_error_event(&e.to_string(), &mut buf);
                                // Still emit epilogue so client gets proper stream termination (F16)
                                state.phase = Phase::Epilogue;
                            }
                            None => {
                                // Stream ended without [DONE]. Treat as normal completion.
                                warn!("upstream stream ended without [DONE] sentinel");
                                state.phase = Phase::Epilogue;
                            }
                        }
                    }
                    Phase::Epilogue => {
                        emit_epilogue(&state, &mut buf);
                        state.phase = Phase::Done;
                    }
                    Phase::Done => {
                        return None; // Stream is finished.
                    }
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Prologue: opening three events
// ---------------------------------------------------------------------------

fn emit_prologue(state: &ConverterState<impl Stream>, buf: &mut std::collections::VecDeque<Event>) {
    // 1. message_start — empty message envelope
    let message_start_data = json!({
        "type": sse::MESSAGE_START,
        "message": {
            "id": state.message_id,
            "type": "message",
            "role": "assistant",
            "model": state.original_model,
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0
            }
        }
    });
    buf.push_back(make_sse(sse::MESSAGE_START, &message_start_data));

    // 2. content_block_start — text block at index 0
    let block_start_data = json!({
        "type": sse::CONTENT_BLOCK_START,
        "index": 0,
        "content_block": {
            "type": "text",
            "text": ""
        }
    });
    buf.push_back(make_sse(sse::CONTENT_BLOCK_START, &block_start_data));

    // 3. ping
    let ping_data = json!({ "type": sse::PING });
    buf.push_back(make_sse(sse::PING, &ping_data));

    debug!("prologue emitted for model={}", state.original_model);
}

// ---------------------------------------------------------------------------
// Chunk processing: the core state machine step
// ---------------------------------------------------------------------------

fn process_chunk(
    state: &mut ConverterState<impl Stream>,
    chunk: &ChatCompletionChunk,
    buf: &mut std::collections::VecDeque<Event>,
) {
    // Capture usage if present (OpenAI sends this on the final data chunk
    // when stream_options.include_usage is true).
    if let Some(ref usage) = chunk.usage {
        state.usage = Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_input_tokens: usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
        };
    }

    // Process each choice (typically just one).
    for choice in &chunk.choices {
        let delta = &choice.delta;

        // --- Text delta ---
        if let Some(ref text) = delta.content {
            if !text.is_empty() {
                let data = json!({
                    "type": sse::CONTENT_BLOCK_DELTA,
                    "index": state.text_block_index,
                    "delta": {
                        "type": sse::DELTA_TEXT,
                        "text": text
                    }
                });
                buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &data));
            }
        }

        // --- Tool call deltas ---
        if let Some(ref tool_calls) = delta.tool_calls {
            for tc_delta in tool_calls {
                let tc_index = tc_delta.index;

                // Get or create accumulator for this tool call index.
                let acc = state
                    .tool_calls
                    .entry(tc_index)
                    .or_insert_with(ToolCallAccumulator::new);

                // Update ID if provided.
                if let Some(ref id) = tc_delta.id {
                    acc.id = Some(id.clone());
                }

                // Update function name if provided. Restore original casing
                // from the canonical map so Claude Code's tool registry can
                // find a match even if the upstream lowercased or prefixed it.
                if let Some(ref func) = tc_delta.function {
                    if let Some(ref name) = func.name {
                        acc.name = Some(tool_name::restore(&state.tool_name_map, name));
                    }
                }

                // Emit content_block_start once we have both id and name.
                if let (Some(ref id), Some(ref name)) = (&acc.id, &acc.name) {
                    if !acc.started {
                        state.tool_block_counter += 1;
                        let claude_index = state.text_block_index + state.tool_block_counter;
                        acc.claude_index = Some(claude_index);
                        acc.started = true;

                        // Sanitize the upstream id: some OpenAI-compat providers
                        // return ids with characters outside Claude's allowed
                        // regex. See util::tool_id.
                        let safe_id = tool_id::sanitize(id);
                        let data = json!({
                            "type": sse::CONTENT_BLOCK_START,
                            "index": claude_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": safe_id,
                                "name": name,
                                "input": {}
                            }
                        });
                        buf.push_back(make_sse(sse::CONTENT_BLOCK_START, &data));
                    }
                }

                // Accumulate function arguments into the buffer. We used to
                // forward each fragment as an incremental input_json_delta,
                // but some OpenAI-compat providers emit non-standard JSON
                // (single-quoted strings, etc.) that can't be repaired
                // piecewise — fix_json needs to see the whole document.
                //
                // So we buffer until epilogue, then emit a single repaired
                // input_json_delta containing the full arguments. Matches
                // CPA's behavior for the same reason.
                if let Some(ref func) = tc_delta.function {
                    if let Some(ref args_fragment) = func.arguments {
                        if acc.claude_index.is_some() && !args_fragment.is_empty() {
                            acc.args_buffer.push_str(args_fragment);
                        }
                    }
                }
            }
        }

        // --- Finish reason ---
        if let Some(ref reason) = choice.finish_reason {
            state.final_stop_reason = map_finish_reason(reason);
        }
    }
}

// ---------------------------------------------------------------------------
// Epilogue: closing events
// ---------------------------------------------------------------------------

fn emit_epilogue(state: &ConverterState<impl Stream>, buf: &mut std::collections::VecDeque<Event>) {
    warn!(
        "← stream done | input_tokens={} output_tokens={} cache_read={} stop={}",
        state.usage.input_tokens,
        state.usage.output_tokens,
        state.usage.cache_read_input_tokens.unwrap_or(0),
        state.final_stop_reason,
    );

    // 1. content_block_stop for the text block.
    let text_stop = json!({
        "type": sse::CONTENT_BLOCK_STOP,
        "index": state.text_block_index
    });
    buf.push_back(make_sse(sse::CONTENT_BLOCK_STOP, &text_stop));

    // 2. For each started tool call: flush the accumulated arguments as a
    //    single repaired input_json_delta, then content_block_stop.
    //    fix_json repairs non-standard JSON (single quotes etc.) so Claude
    //    Code receives valid JSON regardless of upstream formatting quirks.
    let mut tool_indices: Vec<usize> = state.tool_calls.keys().copied().collect();
    tool_indices.sort();
    let has_tool_calls = tool_indices.iter().any(|idx| state.tool_calls[idx].started);
    for idx in tool_indices {
        let acc = &state.tool_calls[&idx];
        if acc.started {
            if let Some(claude_idx) = acc.claude_index {
                // Flush accumulated tool arguments as a single input_json_delta.
                // Empty args_buffer is fine — fix_json returns "" unchanged,
                // and Claude's parser handles an empty partial_json.
                if !acc.args_buffer.is_empty() {
                    let repaired = crate::util::fix_json::fix_json(&acc.args_buffer);
                    let delta = json!({
                        "type": sse::CONTENT_BLOCK_DELTA,
                        "index": claude_idx,
                        "delta": {
                            "type": sse::DELTA_INPUT_JSON,
                            "partial_json": repaired
                        }
                    });
                    buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &delta));
                }

                let tool_stop = json!({
                    "type": sse::CONTENT_BLOCK_STOP,
                    "index": claude_idx
                });
                buf.push_back(make_sse(sse::CONTENT_BLOCK_STOP, &tool_stop));
            }
        }
    }

    // Force stop_reason to tool_use if we saw any tool calls
    let final_stop = if has_tool_calls {
        stop_reason::TOOL_USE.to_string()
    } else {
        state.final_stop_reason.clone()
    };

    // 3. message_delta with stop_reason and usage.
    // Claude API semantics: input_tokens and cache_read_input_tokens are MUTUALLY
    // EXCLUSIVE. input_tokens = fresh tokens, cache_read_input_tokens = from cache.
    // Upstream reports prompt_tokens as the TOTAL (fresh + cached), so we must
    // subtract cached before reporting.
    //
    // Algorithm adapted from CLIProxyAPI (MIT) — extractOpenAIUsage.
    // Note: state.usage.input_tokens here was captured from upstream chunk.usage.prompt_tokens,
    // and state.usage.cache_read_input_tokens from prompt_tokens_details.cached_tokens
    // (see process_chunk). They are raw upstream numbers and must be normalized here.
    let cached = state.usage.cache_read_input_tokens.unwrap_or(0);
    let fresh_input = state.usage.input_tokens.saturating_sub(cached);
    let report_input = if state.estimated_input_tokens > 0 && fresh_input > 0 {
        state.estimated_input_tokens.min(fresh_input)
    } else if state.estimated_input_tokens > 0 {
        state.estimated_input_tokens
    } else {
        fresh_input
    };
    let report_output = state.usage.output_tokens;
    let usage_data = if cached > 0 {
        json!({
            "input_tokens": report_input,
            "output_tokens": report_output,
            "cache_read_input_tokens": cached
        })
    } else {
        json!({
            "input_tokens": report_input,
            "output_tokens": report_output
        })
    };
    let message_delta = json!({
        "type": sse::MESSAGE_DELTA,
        "delta": {
            "stop_reason": final_stop,
            "stop_sequence": null
        },
        "usage": usage_data
    });
    buf.push_back(make_sse(sse::MESSAGE_DELTA, &message_delta));

    // 4. message_stop — final event.
    let message_stop = json!({ "type": sse::MESSAGE_STOP });
    buf.push_back(make_sse(sse::MESSAGE_STOP, &message_stop));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an axum SSE `Event` with the given event name and JSON data.
fn make_sse(event_name: &str, data: &serde_json::Value) -> Event {
    // serde_json::to_string produces UTF-8 (no ASCII escaping) by default,
    // matching the Python `ensure_ascii=False` behavior.
    let json_str = serde_json::to_string(data).unwrap_or_else(|e| {
        error!("failed to serialize SSE data: {e}");
        format!(
            r#"{{"type":"error","error":{{"type":"serialization_error","message":"{}"}}}}"#,
            e
        )
    });
    Event::default().event(event_name).data(json_str)
}

/// Emit an SSE error event (non-fatal — the stream continues to the next phase
/// or terminates, but the consumer still sees a well-formed event).
fn emit_error_event(message: &str, buf: &mut std::collections::VecDeque<Event>) {
    let data = json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!("Streaming error: {message}")
        }
    });
    buf.push_back(make_sse("error", &data));
}

/// Map OpenAI `finish_reason` to Claude `stop_reason`.
fn map_finish_reason(reason: &str) -> String {
    match reason {
        "stop" => stop_reason::END_TURN,
        "length" => stop_reason::MAX_TOKENS,
        "tool_calls" | "function_call" => stop_reason::TOOL_USE,
        _ => stop_reason::END_TURN,
    }
    .to_string()
}

/// Generate a Claude-style message ID: `msg_` + 24 hex characters.
fn generate_message_id() -> String {
    let uuid_hex = Uuid::new_v4().simple().to_string(); // 32 hex chars
    format!("msg_{}", &uuid_hex[..24])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::openai::*;
    use futures::stream;
    use std::time::Duration;

    /// Helper to collect all events from the converter.
    async fn collect_events(
        events: Vec<Result<OpenAiSseEvent, StreamError>>,
        model: &str,
    ) -> Vec<Event> {
        let upstream = stream::iter(events);
        let output = openai_stream_to_claude(
            upstream,
            model.to_string(),
            Duration::ZERO,
            0,
            tool_name::ToolNameMap::new(),
        );
        futures::pin_mut!(output);
        let mut results = Vec::new();
        while let Some(Ok(event)) = output.next().await {
            results.push(event);
        }
        results
    }

    fn make_text_chunk(text: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chatcmpl-test".into(),
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: Some(text.to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn make_finish_chunk(reason: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chatcmpl-test".into(),
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some(reason.to_string()),
            }],
            usage: None,
        }
    }

    #[tokio::test]
    async fn test_simple_text_stream() {
        let events = vec![
            Ok(OpenAiSseEvent::Chunk(make_text_chunk("Hello"))),
            Ok(OpenAiSseEvent::Chunk(make_text_chunk(" world"))),
            Ok(OpenAiSseEvent::Chunk(make_finish_chunk("stop"))),
            Ok(OpenAiSseEvent::Done),
        ];

        let result = collect_events(events, "claude-3-opus-20240229").await;

        // Prologue: message_start + content_block_start + ping = 3
        // Streaming: 2 text deltas (one per text chunk, finish chunk has no content)
        // Epilogue: content_block_stop + message_delta + message_stop = 3
        // Total = 3 + 2 + 3 = 8
        assert_eq!(result.len(), 8, "expected 8 events, got {}", result.len());
    }

    #[tokio::test]
    async fn test_empty_stream() {
        // Only [DONE], no content at all.
        let events = vec![Ok(OpenAiSseEvent::Done)];
        let result = collect_events(events, "claude-3-opus-20240229").await;

        // Prologue(3) + Epilogue(3) = 6
        assert_eq!(result.len(), 6, "expected 6 events for empty stream");
    }

    #[tokio::test]
    async fn test_tool_call_stream() {
        let events = vec![
            // First chunk: tool call starts with id + name
            Ok(OpenAiSseEvent::Chunk(ChatCompletionChunk {
                id: "chatcmpl-tool".into(),
                choices: vec![ChunkChoice {
                    delta: ChunkDelta {
                        content: None,
                        tool_calls: Some(vec![ChunkToolCall {
                            index: 0,
                            id: Some("call_abc123".into()),
                            function: Some(ChunkFunction {
                                name: Some("get_weather".into()),
                                arguments: Some(r#"{"lo"#.into()),
                            }),
                        }]),
                    },
                    finish_reason: None,
                }],
                usage: None,
            })),
            // Second chunk: more arguments
            Ok(OpenAiSseEvent::Chunk(ChatCompletionChunk {
                id: "chatcmpl-tool".into(),
                choices: vec![ChunkChoice {
                    delta: ChunkDelta {
                        content: None,
                        tool_calls: Some(vec![ChunkToolCall {
                            index: 0,
                            id: None,
                            function: Some(ChunkFunction {
                                name: None,
                                arguments: Some(r#"cation":"SF"}"#.into()),
                            }),
                        }]),
                    },
                    finish_reason: None,
                }],
                usage: None,
            })),
            Ok(OpenAiSseEvent::Chunk(make_finish_chunk("tool_calls"))),
            Ok(OpenAiSseEvent::Done),
        ];

        let result = collect_events(events, "claude-3-opus-20240229").await;

        // With buffered tool-args emission (see emit_epilogue):
        //   Prologue(3): message_start + content_block_start(text) + ping
        //   Streaming:   content_block_start(tool) only — args are buffered
        //   Epilogue(5): text_stop + input_json_delta(merged) + tool_stop
        //                + message_delta + message_stop
        // Total = 3 + 1 + 5 = 9
        assert_eq!(
            result.len(),
            9,
            "expected 9 events for tool call stream, got {}",
            result.len()
        );

        // The merged input_json_delta must contain the full reassembled
        // arguments. Find it in the event sequence and verify its payload.
        let merged = result
            .iter()
            .find_map(|evt| {
                // Debug-format the Event and fish the JSON payload out. The
                // axum Event type doesn't expose its fields directly, but
                // its Debug impl dumps `data: Some("...")`.
                let s = format!("{:?}", evt);
                if s.contains("input_json_delta") {
                    Some(s)
                } else {
                    None
                }
            })
            .expect("should find a merged input_json_delta event");
        // Debug-format escapes quotes twice, so look for the unambiguous
        // tail of the reassembled arguments instead.
        assert!(
            merged.contains("location") && merged.contains("SF"),
            "merged partial_json should contain the reassembled args, got: {}",
            merged
        );
    }

    #[tokio::test]
    async fn test_error_event() {
        let events = vec![
            Ok(OpenAiSseEvent::Chunk(make_text_chunk("Hi"))),
            Err(StreamError::Connection("connection reset".into())),
        ];

        let result = collect_events(events, "test-model").await;

        // Prologue(3) + text_delta(1) + error(1) + Epilogue(3) = 8
        // Epilogue still emitted after error for proper stream termination.
        assert_eq!(
            result.len(),
            8,
            "expected 8 events with error, got {}",
            result.len()
        );
    }

    #[tokio::test]
    async fn test_message_id_format() {
        let id = generate_message_id();
        assert!(id.starts_with("msg_"), "ID should start with msg_");
        // "msg_" (4) + 24 hex chars = 28 total
        assert_eq!(id.len(), 28, "ID should be 28 chars, got {}", id.len());
        // Everything after "msg_" should be hex.
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("function_call"), "tool_use");
        assert_eq!(map_finish_reason("content_filter"), "end_turn");
    }
}
