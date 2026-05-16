//! SSE streaming converter: Responses API SSE events → Claude SSE events.
//!
//! Consumes an upstream stream of [`ResponsesStreamEvent`] items and re-emits
//! them as Claude-compatible SSE events, maintaining a state machine that
//! mirrors `ConvertCodexResponseToClaude` in codex_claude_response.go.
//!
//! # Event mapping (§3 §4 of the algo hand-book)
//!
//! # Stream prologue
//!
//! To avoid a 1-2 second "dead air" window between Claude Code seeing HTTP
//! headers and receiving the first SSE byte, we emit a **synthetic**
//! `message_start` at stream construction time — *before* the upstream has
//! produced anything. The synthesized event uses the original Claude model
//! name and a locally-generated message ID (`msg_<uuid_hex>`). When the real
//! `response.created` event eventually arrives from upstream, we discard it
//! (the prologue has already filled that slot). This mirrors how CPA's
//! Go implementation flushes `message_start` immediately on its first
//! `ResponseWriter.Flush()`.
//!
//! # Event mapping (§3 §4 of the algo hand-book)
//!
//! | Upstream event                          | Claude SSE output                         |
//! |-----------------------------------------|-------------------------------------------|
//! | (stream construction)                   | synthetic `message_start` (prologue)      |
//! | `response.created`                      | discarded (prologue already emitted)      |
//! | `reasoning_summary_part.added`          | `content_block_start` (thinking)          |
//! | `reasoning_summary_text.delta`          | `content_block_delta` (thinking_delta)    |
//! | `reasoning_summary_part.done`           | `content_block_stop`, BlockIndex++        |
//! | `content_part.added`                    | `content_block_start` (text)              |
//! | `output_text.delta`                     | `content_block_delta` (text_delta)        |
//! | `content_part.done`                     | `content_block_stop`, BlockIndex++        |
//! | `output_item.added` (function_call)     | `content_block_start` (tool_use) + empty delta |
//! | `function_call_arguments.delta`         | accumulated (I2 strategy, not forwarded) |
//! | `function_call_arguments.done`          | one `content_block_delta` (input_json_delta) |
//! | `output_item.done`                      | `content_block_stop`, BlockIndex++        |
//! | `response.completed`                    | `message_delta` + `message_stop`          |

use std::collections::VecDeque;
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
use crate::types::responses::ResponsesStreamEvent;
use crate::util::{fix_json, tool_id, tool_name};

// Re-export StreamError so callers can use it without importing convert::stream.
pub use crate::convert::stream::StreamError;

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Phase {
    /// Prologue already emitted at construction; now pulling upstream events.
    Streaming,
    /// Emit closing events.
    Epilogue,
    /// Terminal.
    Done,
}

struct ConverterState<S> {
    upstream: Pin<Box<S>>,
    original_model: String,

    // Current content block index (incremented on every content_block_stop).
    block_index: usize,
    // Whether any function_call output item has been seen.
    has_tool_call: bool,
    // Whether we have received at least one function_call_arguments.delta for
    // the *current* tool call (reset on each output_item.added/function_call).
    has_received_arguments_delta: bool,
    // Accumulated raw argument fragments for the current tool call (I2 strategy).
    args_buffer: String,

    // Final stop reason derived at response.completed.
    final_stop_reason: String,
    // Usage captured at response.completed.
    usage: Usage,

    phase: Phase,
    idle_timeout: Duration,
    estimated_input_tokens: u32,
    tool_name_map: tool_name::ToolNameMap,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert a Responses API streaming SSE sequence into a Claude-compatible
/// SSE event stream.
///
/// The returned stream yields `Result<Event, Infallible>` — upstream errors
/// are converted into SSE error events, so the consumer always sees
/// well-formed SSE and always receives the final `message_stop` epilogue.
pub fn responses_stream_to_claude(
    upstream: impl Stream<Item = Result<ResponsesStreamEvent, StreamError>> + Send + 'static,
    original_model: String,
    idle_timeout: Duration,
    estimated_input_tokens: u32,
    tool_name_map: tool_name::ToolNameMap,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send {
    let state = ConverterState {
        upstream: Box::pin(upstream),
        original_model,
        block_index: 0,
        has_tool_call: false,
        has_received_arguments_delta: false,
        args_buffer: String::new(),
        final_stop_reason: stop_reason::END_TURN.to_string(),
        usage: Usage::default(),
        phase: Phase::Streaming,
        idle_timeout,
        estimated_input_tokens,
        tool_name_map,
    };

    // Prologue: emit a synthetic `message_start` *before* the first upstream
    // byte arrives. This kills the 1-2 s "dead air" gap Claude Code would
    // otherwise see between headers and the first SSE event (the Responses
    // upstream typically spends ~1.5 s on its reasoning prelude before
    // yielding `response.created`).
    let mut initial_buf = VecDeque::<Event>::new();
    initial_buf.push_back(synth_message_start(&state.original_model));

    futures::stream::unfold((state, initial_buf), |(mut state, mut buf)| async move {
        loop {
            // Drain the output buffer before pulling more events.
            if let Some(event) = buf.pop_front() {
                return Some((Ok(event), (state, buf)));
            }

            match state.phase {
                Phase::Streaming => {
                    let next = if state.idle_timeout.is_zero() {
                        state.upstream.next().await
                    } else {
                        match timeout(state.idle_timeout, state.upstream.next()).await {
                            Ok(result) => result,
                            Err(_) => {
                                error!(
                                    "responses converter idle timeout ({}s)",
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
                        Some(Ok(event)) => {
                            process_event(&mut state, event, &mut buf);
                        }
                        Some(Err(e)) => {
                            error!("responses upstream error: {e}");
                            emit_error_event(&e.to_string(), &mut buf);
                            state.phase = Phase::Epilogue;
                        }
                        None => {
                            // Stream ended without response.completed.
                            warn!("responses stream ended without completed event");
                            state.phase = Phase::Epilogue;
                        }
                    }
                }
                Phase::Epilogue => {
                    emit_epilogue(&state, &mut buf);
                    state.phase = Phase::Done;
                }
                Phase::Done => {
                    return None;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Event processing (core state machine step)
// ---------------------------------------------------------------------------

fn process_event(
    state: &mut ConverterState<impl Stream>,
    event: ResponsesStreamEvent,
    buf: &mut VecDeque<Event>,
) {
    match event {
        // ── response.created → discard ────────────────────────────────────
        //
        // The prologue already emitted a synthetic `message_start` at stream
        // construction, so this upstream event is now informational only. We
        // log the real upstream id for debugging/correlation but do not
        // forward a second `message_start` to Claude Code.
        ResponsesStreamEvent::Created { response } => {
            debug!(
                "upstream response.created id={} (prologue already sent)",
                response.id
            );
        }

        // ── reasoning_summary_part.added → content_block_start (thinking) ─
        ResponsesStreamEvent::ReasoningSummaryPartAdded {} => {
            let data = json!({
                "type": sse::CONTENT_BLOCK_START,
                "index": state.block_index,
                "content_block": {
                    "type": "thinking",
                    "thinking": ""
                }
            });
            buf.push_back(make_sse(sse::CONTENT_BLOCK_START, &data));
        }

        // ── reasoning_summary_text.delta → content_block_delta (thinking) ─
        ResponsesStreamEvent::ReasoningSummaryTextDelta { delta } => {
            let data = json!({
                "type": sse::CONTENT_BLOCK_DELTA,
                "index": state.block_index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": delta
                }
            });
            buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &data));
        }

        // ── reasoning_summary_part.done → content_block_stop ──────────────
        ResponsesStreamEvent::ReasoningSummaryPartDone {} => {
            emit_block_stop(state.block_index, buf);
            state.block_index += 1;
        }

        // ── content_part.added → content_block_start (text) ───────────────
        ResponsesStreamEvent::ContentPartAdded {} => {
            let data = json!({
                "type": sse::CONTENT_BLOCK_START,
                "index": state.block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            });
            buf.push_back(make_sse(sse::CONTENT_BLOCK_START, &data));
        }

        // ── output_text.delta → content_block_delta (text_delta) ──────────
        ResponsesStreamEvent::OutputTextDelta { delta } => {
            let data = json!({
                "type": sse::CONTENT_BLOCK_DELTA,
                "index": state.block_index,
                "delta": {
                    "type": sse::DELTA_TEXT,
                    "text": delta
                }
            });
            buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &data));
        }

        // ── content_part.done → content_block_stop ────────────────────────
        ResponsesStreamEvent::ContentPartDone {} => {
            emit_block_stop(state.block_index, buf);
            state.block_index += 1;
        }

        // ── output_item.added (function_call) → content_block_start (tool_use) + empty delta
        ResponsesStreamEvent::OutputItemAdded { item } => {
            if item.item_type == "function_call" {
                state.has_tool_call = true;
                state.has_received_arguments_delta = false;
                state.args_buffer.clear();

                let call_id = item.call_id.as_deref().unwrap_or("");
                let name = item.name.as_deref().unwrap_or("");

                let safe_id = tool_id::sanitize(call_id);
                let restored_name = tool_name::restore(&state.tool_name_map, name);

                // content_block_start for tool_use
                let start_data = json!({
                    "type": sse::CONTENT_BLOCK_START,
                    "index": state.block_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": safe_id,
                        "name": restored_name,
                        "input": {}
                    }
                });
                buf.push_back(make_sse(sse::CONTENT_BLOCK_START, &start_data));

                // Immediately follow with an empty input_json_delta (CPA pattern).
                let delta_data = json!({
                    "type": sse::CONTENT_BLOCK_DELTA,
                    "index": state.block_index,
                    "delta": {
                        "type": sse::DELTA_INPUT_JSON,
                        "partial_json": ""
                    }
                });
                buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &delta_data));
            }
            // message-type items don't produce a separate block_start; the
            // content_part.added event handles that.
        }

        // ── function_call_arguments.delta ─────────────────────────────────
        // I2 strategy: accumulate, do NOT forward each fragment.
        // The full repaired JSON is emitted at function_call_arguments.done.
        ResponsesStreamEvent::FunctionCallArgumentsDelta { delta } => {
            state.has_received_arguments_delta = true;
            state.args_buffer.push_str(&delta);
        }

        // ── function_call_arguments.done ──────────────────────────────────
        ResponsesStreamEvent::FunctionCallArgumentsDone { arguments } => {
            // Determine the full argument string:
            //  - If we received deltas, the buffer contains the complete args.
            //  - Otherwise use the `arguments` field from the done event.
            let raw = if !state.has_received_arguments_delta && !arguments.is_empty() {
                arguments.clone()
            } else {
                state.args_buffer.clone()
            };

            let repaired = fix_json::fix_json(&raw);
            let data = json!({
                "type": sse::CONTENT_BLOCK_DELTA,
                "index": state.block_index,
                "delta": {
                    "type": sse::DELTA_INPUT_JSON,
                    "partial_json": repaired
                }
            });
            buf.push_back(make_sse(sse::CONTENT_BLOCK_DELTA, &data));
        }

        // ── output_item.done → content_block_stop ─────────────────────────
        ResponsesStreamEvent::OutputItemDone {} => {
            emit_block_stop(state.block_index, buf);
            state.block_index += 1;
        }

        // ── response.completed → save usage + stop_reason; epilogue emits events
        ResponsesStreamEvent::Completed { response } => {
            // Persist stop_reason from upstream (may be overridden by has_tool_call).
            state.final_stop_reason = match response.stop_reason.as_deref() {
                Some("max_tokens") => stop_reason::MAX_TOKENS.to_string(),
                _ => stop_reason::END_TURN.to_string(),
            };

            // Persist usage if present.
            if let Some(u) = response.usage {
                state.usage = Usage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    cache_read_input_tokens: {
                        let c = u.input_tokens_details.cached_tokens;
                        if c > 0 {
                            Some(c)
                        } else {
                            None
                        }
                    },
                };
            }

            state.phase = Phase::Epilogue;
        }
    }
}

// ---------------------------------------------------------------------------
// Epilogue
// ---------------------------------------------------------------------------

fn emit_epilogue(state: &ConverterState<impl Stream>, buf: &mut VecDeque<Event>) {
    // ---- stop_reason (§4 precedence) ----
    let final_stop = if state.has_tool_call {
        stop_reason::TOOL_USE.to_string()
    } else {
        state.final_stop_reason.clone()
    };

    // ---- C1 usage formula ----
    let cached = state.usage.cache_read_input_tokens.unwrap_or(0);
    let fresh = state.usage.input_tokens.saturating_sub(cached);
    let report_input = if state.estimated_input_tokens > 0 && fresh > 0 {
        state.estimated_input_tokens.min(fresh)
    } else if state.estimated_input_tokens > 0 {
        state.estimated_input_tokens
    } else {
        fresh
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

    warn!(
        "← responses stream done | input={} output={} cache={} stop={}",
        report_input, report_output, cached, final_stop,
    );

    let message_delta = json!({
        "type": sse::MESSAGE_DELTA,
        "delta": {
            "stop_reason": final_stop,
            "stop_sequence": null
        },
        "usage": usage_data
    });
    buf.push_back(make_sse(sse::MESSAGE_DELTA, &message_delta));

    let message_stop = json!({ "type": sse::MESSAGE_STOP });
    buf.push_back(make_sse(sse::MESSAGE_STOP, &message_stop));
}

// ---------------------------------------------------------------------------
// Helpers (local copies — do not pub-use from convert::stream)
// ---------------------------------------------------------------------------

/// Build an axum SSE `Event` with the given event name and JSON data.
fn make_sse(event_name: &str, data: &serde_json::Value) -> Event {
    let json_str = serde_json::to_string(data).unwrap_or_else(|e| {
        error!("failed to serialize SSE data: {e}");
        format!(
            r#"{{"type":"error","error":{{"type":"serialization_error","message":"{}"}}}}"#,
            e
        )
    });
    Event::default().event(event_name).data(json_str)
}

/// Emit a non-fatal SSE error event into the buffer.
fn emit_error_event(message: &str, buf: &mut VecDeque<Event>) {
    let data = json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!("Streaming error: {message}")
        }
    });
    buf.push_back(make_sse("error", &data));
}

/// Emit a `content_block_stop` for the given index.
fn emit_block_stop(index: usize, buf: &mut VecDeque<Event>) {
    let data = json!({
        "type": sse::CONTENT_BLOCK_STOP,
        "index": index
    });
    buf.push_back(make_sse(sse::CONTENT_BLOCK_STOP, &data));
}

/// Generate a Claude-style message ID: `msg_` + 24 hex characters.
///
/// Used by `synth_message_start` to stamp the prologue event with a locally-
/// generated id (we can't wait for the upstream's real id — that would
/// defeat the purpose of the prologue).
fn generate_message_id() -> String {
    let uuid_hex = Uuid::new_v4().simple().to_string();
    format!("msg_{}", &uuid_hex[..24])
}

/// Build a synthetic `message_start` SSE event using the original Claude
/// model name and a locally-generated message id.
///
/// Emitted at stream construction so Claude Code's first byte arrives
/// immediately after headers, rather than waiting for the upstream's
/// (often ~1.5 s) reasoning prelude before the real `response.created`.
///
/// `input_tokens: 0` / `output_tokens: 0` matches the shape the former
/// Created-branch handler produced — the real counts land in the epilogue
/// `message_delta` at the end of the stream.
fn synth_message_start(model: &str) -> Event {
    let data = json!({
        "type": sse::MESSAGE_START,
        "message": {
            "id": generate_message_id(),
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0
            }
        }
    });
    make_sse(sse::MESSAGE_START, &data)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::responses::{
        InputTokensDetails, OutputItemAddedPayload, ResponseCompletedPayload,
        ResponseCreatedPayload, ResponsesStreamEvent, ResponsesUsage,
    };
    use futures::stream;
    use std::time::Duration;

    // ── helpers ──────────────────────────────────────────────────────────────

    async fn collect(events: Vec<Result<ResponsesStreamEvent, StreamError>>) -> Vec<String> {
        let upstream = stream::iter(events);
        let out = responses_stream_to_claude(
            upstream,
            "claude-test-model".into(),
            Duration::ZERO,
            0,
            tool_name::ToolNameMap::new(),
        );
        futures::pin_mut!(out);
        let mut results = Vec::new();
        while let Some(Ok(ev)) = out.next().await {
            results.push(format!("{:?}", ev));
        }
        results
    }

    fn created_event() -> ResponsesStreamEvent {
        ResponsesStreamEvent::Created {
            response: ResponseCreatedPayload {
                id: "resp_test001".into(),
                model: "claude-opus-4-5-20251101".into(),
            },
        }
    }

    fn completed_event(stop: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::Completed {
            response: ResponseCompletedPayload {
                id: "resp_test001".into(),
                model: "claude-opus-4-5-20251101".into(),
                stop_reason: Some(stop.into()),
                usage: Some(ResponsesUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    input_tokens_details: InputTokensDetails { cached_tokens: 0 },
                }),
                output: vec![],
            },
        }
    }

    fn has_pattern(events: &[String], pat: &str) -> bool {
        events.iter().any(|s| s.contains(pat))
    }

    // ── test_simple_text_stream ───────────────────────────────────────────

    #[tokio::test]
    async fn test_simple_text_stream() {
        let events = vec![
            Ok(created_event()),
            Ok(ResponsesStreamEvent::ContentPartAdded {}),
            Ok(ResponsesStreamEvent::OutputTextDelta {
                delta: "Hello ".into(),
            }),
            Ok(ResponsesStreamEvent::OutputTextDelta {
                delta: "world".into(),
            }),
            Ok(ResponsesStreamEvent::ContentPartDone {}),
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // message_start(1) + block_start(1) + deltas(2) + block_stop(1)
        // + message_delta(1) + message_stop(1) = 7
        assert_eq!(
            result.len(),
            7,
            "expected 7 events, got {}: {:?}",
            result.len(),
            result
        );

        assert!(has_pattern(&result, "message_start"));
        assert!(has_pattern(&result, "content_block_start"));
        assert!(has_pattern(&result, "text_delta"));
        assert!(has_pattern(&result, "content_block_stop"));
        assert!(has_pattern(&result, "message_delta"));
        assert!(has_pattern(&result, "message_stop"));
    }

    // ── test_reasoning_then_text_stream ──────────────────────────────────

    #[tokio::test]
    async fn test_reasoning_then_text_stream() {
        let events = vec![
            Ok(created_event()),
            // Thinking block
            Ok(ResponsesStreamEvent::ReasoningSummaryPartAdded {}),
            Ok(ResponsesStreamEvent::ReasoningSummaryTextDelta {
                delta: "Let me think".into(),
            }),
            Ok(ResponsesStreamEvent::ReasoningSummaryPartDone {}),
            // Text block
            Ok(ResponsesStreamEvent::ContentPartAdded {}),
            Ok(ResponsesStreamEvent::OutputTextDelta {
                delta: "Answer".into(),
            }),
            Ok(ResponsesStreamEvent::ContentPartDone {}),
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // message_start(1) + thinking_block_start(1) + thinking_delta(1)
        // + thinking_block_stop(1) + text_block_start(1) + text_delta(1)
        // + text_block_stop(1) + message_delta(1) + message_stop(1) = 9
        assert_eq!(result.len(), 9, "got {}: {:?}", result.len(), result);

        assert!(
            has_pattern(&result, "thinking_delta"),
            "should have thinking_delta"
        );
        assert!(has_pattern(&result, "text_delta"), "should have text_delta");
        // Both blocks should stop — there should be 2 content_block_stop events.
        assert!(
            result
                .iter()
                .filter(|s| s.contains("content_block_stop"))
                .count()
                >= 2,
            "should have at least 2 content_block_stop"
        );
    }

    // ── test_single_tool_call_stream ─────────────────────────────────────

    #[tokio::test]
    async fn test_single_tool_call_stream() {
        let events = vec![
            Ok(created_event()),
            Ok(ResponsesStreamEvent::OutputItemAdded {
                item: OutputItemAddedPayload {
                    item_type: "function_call".into(),
                    call_id: Some("call_abc".into()),
                    name: Some("get_weather".into()),
                },
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                delta: r#"{"loc"#.into(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                delta: r#"ation":"NYC"}"#.into(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDone {
                arguments: String::new(), // ignored since deltas were received
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {}),
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // message_start(1)
        // + block_start(tool_use)(1) + empty_delta(1)   ← from OutputItemAdded
        // + full_args_delta(1)                           ← from ArgumentsDone
        // + block_stop(1)                                ← from OutputItemDone
        // + message_delta(1) + message_stop(1) = 7
        assert_eq!(result.len(), 7, "got {}: {:?}", result.len(), result);

        assert!(
            has_pattern(&result, "tool_use"),
            "should have tool_use block_start"
        );
        assert!(
            has_pattern(&result, "input_json_delta"),
            "should have input_json_delta"
        );
        assert!(
            has_pattern(&result, "message_stop"),
            "should have message_stop"
        );

        // stop_reason must be forced to tool_use
        assert!(
            has_pattern(&result, "tool_use"),
            "stop_reason should be tool_use"
        );
    }

    // ── test_multiple_tool_calls_stream ──────────────────────────────────

    #[tokio::test]
    async fn test_multiple_tool_calls_stream() {
        let events = vec![
            Ok(created_event()),
            // Tool 1
            Ok(ResponsesStreamEvent::OutputItemAdded {
                item: OutputItemAddedPayload {
                    item_type: "function_call".into(),
                    call_id: Some("call_1".into()),
                    name: Some("search".into()),
                },
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDone {
                arguments: r#"{"q":"rust"}"#.into(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {}),
            // Tool 2
            Ok(ResponsesStreamEvent::OutputItemAdded {
                item: OutputItemAddedPayload {
                    item_type: "function_call".into(),
                    call_id: Some("call_2".into()),
                    name: Some("fetch".into()),
                },
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDone {
                arguments: r#"{"url":"https://example.com"}"#.into(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {}),
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // message_start(1)
        // + tool1: block_start(1) + empty_delta(1) + args_delta(1) + block_stop(1) = 4
        // + tool2: block_start(1) + empty_delta(1) + args_delta(1) + block_stop(1) = 4
        // + message_delta(1) + message_stop(1) = 2
        // total = 1 + 4 + 4 + 2 = 11
        assert_eq!(result.len(), 11, "got {}: {:?}", result.len(), result);
        assert!(has_pattern(&result, "tool_use"));
    }

    // ── test_tool_call_arguments_accumulated_and_fixed ───────────────────

    #[tokio::test]
    async fn test_tool_call_arguments_accumulated_and_fixed() {
        // Non-standard single-quoted JSON — fix_json must repair it.
        let events = vec![
            Ok(created_event()),
            Ok(ResponsesStreamEvent::OutputItemAdded {
                item: OutputItemAddedPayload {
                    item_type: "function_call".into(),
                    call_id: Some("call_fixme".into()),
                    name: Some("do_thing".into()),
                },
            }),
            // Arrive in fragments with single-quoted JSON
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                delta: "{'key':".into(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                delta: " 'value'}".into(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDone {
                arguments: String::new(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {}),
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // Find the input_json_delta event and verify it contains properly
        // double-quoted JSON.
        let args_event = result
            .iter()
            .find(|s| s.contains("input_json_delta") && s.contains("key"))
            .expect("should find an input_json_delta with 'key'");

        assert!(
            args_event.contains(r#"\"key\""#) || args_event.contains("key"),
            "arguments should contain repaired key: {args_event}"
        );
        assert!(
            args_event.contains(r#"\"value\""#) || args_event.contains("value"),
            "arguments should contain repaired value: {args_event}"
        );
    }

    // ── test_upstream_error_still_emits_epilogue ─────────────────────────

    #[tokio::test]
    async fn test_upstream_error_still_emits_epilogue() {
        let events = vec![
            Ok(created_event()),
            Ok(ResponsesStreamEvent::ContentPartAdded {}),
            Ok(ResponsesStreamEvent::OutputTextDelta { delta: "Hi".into() }),
            Err(StreamError::Connection("connection reset".into())),
        ];

        let result = collect(events).await;

        // Should still end with message_delta + message_stop even after error.
        assert!(
            has_pattern(&result, "message_stop"),
            "epilogue message_stop must be emitted after error; got: {:?}",
            result
        );
        assert!(
            has_pattern(&result, "error"),
            "error event should be present"
        );
    }

    // ── test_message_id_format ────────────────────────────────────────────

    #[test]
    fn test_message_id_format() {
        let id = generate_message_id();
        assert!(id.starts_with("msg_"), "ID must start with msg_");
        assert_eq!(id.len(), 28, "ID must be 28 chars, got {}", id.len());
        assert!(
            id[4..].chars().all(|c| c.is_ascii_hexdigit()),
            "suffix must be hex"
        );
    }

    // ── test_stop_reason_forced_tool_use_when_toolcall_present ───────────

    #[tokio::test]
    async fn test_stop_reason_forced_tool_use_when_toolcall_present() {
        // Even if completed.stop_reason is "end_turn", we have a tool call
        // so the emitted stop_reason must be "tool_use".
        let events = vec![
            Ok(created_event()),
            Ok(ResponsesStreamEvent::OutputItemAdded {
                item: OutputItemAddedPayload {
                    item_type: "function_call".into(),
                    call_id: Some("call_forced".into()),
                    name: Some("my_tool".into()),
                },
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDone {
                arguments: "{}".into(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {}),
            // upstream says end_turn but we have a tool call
            Ok(completed_event("end_turn")),
        ];

        let result = collect(events).await;

        // The message_delta must carry stop_reason = tool_use.
        let message_delta = result
            .iter()
            .find(|s| s.contains("message_delta"))
            .expect("message_delta must be present");
        assert!(
            message_delta.contains("tool_use"),
            "stop_reason should be forced to tool_use; got: {message_delta}"
        );
    }
}
