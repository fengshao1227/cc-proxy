//! SSE streaming converter: OpenAI ChatCompletions SSE → Claude SSE events.
//!
//! Consumes an upstream stream of [`OpenAiSseEvent`] items and re-emits them
//! as Claude-compatible SSE events. Tool call arguments are accumulated (I2
//! strategy) and emitted as one repaired JSON blob at `finish_reason`.

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
use crate::types::openai::ChatCompletionChunk;
use crate::util::{fix_json, tool_id, tool_name};

// ---------------------------------------------------------------------------
// Public types (re-exported for client.rs)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum OpenAiSseEvent {
    Chunk(ChatCompletionChunk),
    Done,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("connection error: {0}")]
    Connection(String),
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Phase {
    AwaitingFirstChunk,
    Streaming,
    Epilogue,
    Done,
}

struct ToolAcc {
    id: String,
    name: String,
    args: String,
}

struct State<S> {
    upstream: Pin<Box<S>>,
    original_model: String,

    block_index: usize,
    text_block_open: bool,
    has_tool_call: bool,
    tool_accs: Vec<Option<ToolAcc>>,

    final_stop_reason: String,
    usage: Usage,

    phase: Phase,
    idle_timeout: Duration,
    estimated_input_tokens: u32,
    tool_name_map: tool_name::ToolNameMap,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn openai_stream_to_claude(
    upstream: impl Stream<Item = Result<OpenAiSseEvent, StreamError>> + Send + 'static,
    original_model: String,
    idle_timeout: Duration,
    estimated_input_tokens: u32,
    tool_name_map: tool_name::ToolNameMap,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send {
    let state = State {
        upstream: Box::pin(upstream),
        original_model,
        block_index: 0,
        text_block_open: false,
        has_tool_call: false,
        tool_accs: Vec::new(),
        final_stop_reason: stop_reason::END_TURN.to_string(),
        usage: Usage::default(),
        phase: Phase::AwaitingFirstChunk,
        idle_timeout,
        estimated_input_tokens,
        tool_name_map,
    };

    futures::stream::unfold(
        (state, VecDeque::<Event>::new()),
        |(mut st, mut buf)| async move {
            loop {
                if let Some(ev) = buf.pop_front() {
                    return Some((Ok(ev), (st, buf)));
                }
                match st.phase {
                    Phase::AwaitingFirstChunk | Phase::Streaming => {
                        let next = if st.idle_timeout.is_zero() {
                            st.upstream.next().await
                        } else {
                            match timeout(st.idle_timeout, st.upstream.next()).await {
                                Ok(r) => r,
                                Err(_) => {
                                    let kind = if st.phase == Phase::AwaitingFirstChunk {
                                        "first-byte"
                                    } else {
                                        "idle"
                                    };
                                    error!(
                                        "stream {kind} timeout ({}s)",
                                        st.idle_timeout.as_secs()
                                    );
                                    emit_error(
                                        &format!(
                                            "stream {kind} timeout ({}s)",
                                            st.idle_timeout.as_secs()
                                        ),
                                        &mut buf,
                                    );
                                    st.phase = Phase::Epilogue;
                                    continue;
                                }
                            }
                        };
                        match next {
                            Some(Ok(ev)) => process(&mut st, ev, &mut buf),
                            Some(Err(e)) => {
                                error!("upstream error: {e}");
                                emit_error(&e.to_string(), &mut buf);
                                st.phase = Phase::Epilogue;
                            }
                            None => {
                                if st.phase == Phase::AwaitingFirstChunk {
                                    warn!("stream ended without any chunks");
                                }
                                close_text(&mut st, &mut buf);
                                flush_tools(&mut st, &mut buf);
                                st.phase = Phase::Epilogue;
                            }
                        }
                    }
                    Phase::Epilogue => {
                        emit_epilogue(&st, &mut buf);
                        st.phase = Phase::Done;
                    }
                    Phase::Done => return None,
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Core event processing
// ---------------------------------------------------------------------------

fn process(st: &mut State<impl Stream>, event: OpenAiSseEvent, buf: &mut VecDeque<Event>) {
    match event {
        OpenAiSseEvent::Done => {
            close_text(st, buf);
            flush_tools(st, buf);
            st.phase = Phase::Epilogue;
        }
        OpenAiSseEvent::Chunk(chunk) => {
            if let Some(u) = &chunk.usage {
                st.usage.input_tokens = u.prompt_tokens;
                st.usage.output_tokens = u.completion_tokens;
                if let Some(ref d) = u.prompt_tokens_details {
                    if let Some(c) = d.cached_tokens {
                        if c > 0 {
                            st.usage.cache_read_input_tokens = Some(c);
                        }
                    }
                }
            }

            let Some(choice) = chunk.choices.first() else {
                return;
            };

            // message_start on first real chunk
            if st.phase == Phase::AwaitingFirstChunk {
                let id = format!("msg_{}", &Uuid::new_v4().simple().to_string()[..24]);
                buf.push_back(make_sse(
                    sse::MESSAGE_START,
                    &json!({
                        "type": sse::MESSAGE_START,
                        "message": {
                            "id": id,
                            "type": "message",
                            "role": "assistant",
                            "model": st.original_model,
                            "content": [],
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": { "input_tokens": 0, "output_tokens": 0 }
                        }
                    }),
                ));
                st.phase = Phase::Streaming;
                debug!("chat stream started, upstream id={}", chunk.id);
            }

            // Text delta
            if let Some(ref text) = choice.delta.content {
                if !text.is_empty() {
                    if !st.text_block_open {
                        buf.push_back(make_sse(
                            sse::CONTENT_BLOCK_START,
                            &json!({
                                "type": sse::CONTENT_BLOCK_START,
                                "index": st.block_index,
                                "content_block": { "type": "text", "text": "" }
                            }),
                        ));
                        st.text_block_open = true;
                    }
                    buf.push_back(make_sse(
                        sse::CONTENT_BLOCK_DELTA,
                        &json!({
                            "type": sse::CONTENT_BLOCK_DELTA,
                            "index": st.block_index,
                            "delta": { "type": sse::DELTA_TEXT, "text": text }
                        }),
                    ));
                }
            }

            // Tool call deltas — accumulate
            if let Some(ref tcs) = choice.delta.tool_calls {
                for tc in tcs {
                    while st.tool_accs.len() <= tc.index {
                        st.tool_accs.push(None);
                    }
                    let acc = st.tool_accs[tc.index].get_or_insert_with(|| ToolAcc {
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                    });
                    if let Some(ref id) = tc.id {
                        acc.id.clone_from(id);
                    }
                    if let Some(ref f) = tc.function {
                        if let Some(ref n) = f.name {
                            acc.name.clone_from(n);
                        }
                        if let Some(ref a) = f.arguments {
                            acc.args.push_str(a);
                        }
                    }
                }
                st.has_tool_call = true;
            }

            // finish_reason
            if let Some(ref reason) = choice.finish_reason {
                close_text(st, buf);
                match reason.as_str() {
                    "tool_calls" => {
                        flush_tools(st, buf);
                        st.final_stop_reason = stop_reason::TOOL_USE.to_string();
                    }
                    "length" => {
                        st.final_stop_reason = stop_reason::MAX_TOKENS.to_string();
                    }
                    _ => {
                        st.final_stop_reason = stop_reason::END_TURN.to_string();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn close_text(st: &mut State<impl Stream>, buf: &mut VecDeque<Event>) {
    if st.text_block_open {
        buf.push_back(make_sse(
            sse::CONTENT_BLOCK_STOP,
            &json!({
                "type": sse::CONTENT_BLOCK_STOP, "index": st.block_index
            }),
        ));
        st.block_index += 1;
        st.text_block_open = false;
    }
}

fn flush_tools(st: &mut State<impl Stream>, buf: &mut VecDeque<Event>) {
    for slot in st.tool_accs.drain(..) {
        let Some(acc) = slot else { continue };
        let safe_id = tool_id::sanitize(&acc.id);
        let restored = tool_name::restore(&st.tool_name_map, &acc.name);

        buf.push_back(make_sse(
            sse::CONTENT_BLOCK_START,
            &json!({
                "type": sse::CONTENT_BLOCK_START,
                "index": st.block_index,
                "content_block": {
                    "type": "tool_use", "id": safe_id,
                    "name": restored, "input": {}
                }
            }),
        ));
        buf.push_back(make_sse(
            sse::CONTENT_BLOCK_DELTA,
            &json!({
                "type": sse::CONTENT_BLOCK_DELTA,
                "index": st.block_index,
                "delta": { "type": sse::DELTA_INPUT_JSON, "partial_json": "" }
            }),
        ));
        let repaired = fix_json::fix_json(&acc.args);
        buf.push_back(make_sse(
            sse::CONTENT_BLOCK_DELTA,
            &json!({
                "type": sse::CONTENT_BLOCK_DELTA,
                "index": st.block_index,
                "delta": { "type": sse::DELTA_INPUT_JSON, "partial_json": repaired }
            }),
        ));
        buf.push_back(make_sse(
            sse::CONTENT_BLOCK_STOP,
            &json!({
                "type": sse::CONTENT_BLOCK_STOP, "index": st.block_index
            }),
        ));
        st.block_index += 1;
    }
}

fn emit_epilogue(st: &State<impl Stream>, buf: &mut VecDeque<Event>) {
    let final_stop = if st.has_tool_call {
        stop_reason::TOOL_USE.to_string()
    } else {
        st.final_stop_reason.clone()
    };

    let cached = st.usage.cache_read_input_tokens.unwrap_or(0);
    let fresh = st.usage.input_tokens.saturating_sub(cached);
    let report_input = if st.estimated_input_tokens > 0 && fresh > 0 {
        st.estimated_input_tokens.min(fresh)
    } else if st.estimated_input_tokens > 0 {
        st.estimated_input_tokens
    } else {
        fresh
    };

    let usage_data = if cached > 0 {
        json!({ "input_tokens": report_input, "output_tokens": st.usage.output_tokens, "cache_read_input_tokens": cached })
    } else {
        json!({ "input_tokens": report_input, "output_tokens": st.usage.output_tokens })
    };

    warn!(
        "← stream done | input_tokens={} output_tokens={} cache_read={} stop={}",
        report_input, st.usage.output_tokens, cached, final_stop
    );

    buf.push_back(make_sse(
        sse::MESSAGE_DELTA,
        &json!({
            "type": sse::MESSAGE_DELTA,
            "delta": { "stop_reason": final_stop, "stop_sequence": null },
            "usage": usage_data
        }),
    ));
    buf.push_back(make_sse(
        sse::MESSAGE_STOP,
        &json!({ "type": sse::MESSAGE_STOP }),
    ));
}

fn make_sse(event_name: &str, data: &serde_json::Value) -> Event {
    Event::default()
        .event(event_name)
        .data(serde_json::to_string(data).unwrap_or_default())
}

fn emit_error(message: &str, buf: &mut VecDeque<Event>) {
    buf.push_back(make_sse(
        "error",
        &json!({
            "type": "error",
            "error": { "type": "api_error", "message": format!("Streaming error: {message}") }
        }),
    ));
}
