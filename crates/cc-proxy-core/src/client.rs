use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use tokio::time::timeout;

use crate::config::ProxyConfig;
use crate::convert::stream::{OpenAiSseEvent, StreamError};
use crate::error::ProxyError;
use crate::types::openai::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse};
use crate::types::responses::{ResponsesRequest, ResponsesResponse, ResponsesStreamEvent};

/// HTTP client for upstream OpenAI-compatible API
#[derive(Clone)]
pub struct UpstreamClient {
    client: reqwest::Client,
    base_url: String,
}

impl UpstreamClient {
    pub fn new(config: &ProxyConfig) -> Result<Self, ProxyError> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert("content-type", HeaderValue::from_static("application/json"));

        // Custom headers
        for (key, value) in &config.custom_headers {
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                default_headers.insert(name, val);
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout))
            .connect_timeout(Duration::from_secs(config.connect_timeout))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_max_idle_per_host(10)
            .default_headers(default_headers)
            .build()
            .map_err(|e| ProxyError::Internal(format!("Failed to create HTTP client: {e}")))?;

        // Normalize base URL
        let base_url = config.openai_base_url.trim_end_matches('/').to_string();

        Ok(Self { client, base_url })
    }

    /// Send non-streaming chat completion
    pub async fn chat_completion(
        &self,
        request: &ChatCompletionRequest,
        api_key: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let msg = ProxyError::classify_upstream(&body);
            return Err(ProxyError::Internal(msg));
        }

        let resp: ChatCompletionResponse = response.json().await?;
        Ok(resp)
    }

    /// Returns a shared reference to the underlying `reqwest::Client`.
    ///
    /// Used by `upstream::detector::detect` to probe the upstream without
    /// constructing a separate client.
    pub fn inner_client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Send a "non-streaming" Responses API call.
    ///
    /// **Internal implementation detail**: most Responses API upstreams (OpenAI
    /// official, sub2api, api.150226.xyz, etc.) **always** respond with SSE
    /// regardless of `stream: false` in the request. CPA's codex executor
    /// hard-codes `stream = true` for exactly this reason.
    ///
    /// So cc-proxy's "non-streaming" path forces `stream = true` on the wire,
    /// consumes the SSE event stream internally, and aggregates events into a
    /// single `ResponsesResponse`. Callers (the Claude Code non-stream path)
    /// get the illusion of a non-streaming call without knowing the upstream
    /// quirk.
    pub async fn responses_completion(
        &self,
        request: &ResponsesRequest,
        api_key: &str,
        first_byte_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<ResponsesResponse, ProxyError> {
        use crate::types::responses::{
            ContentPart, InputTokensDetails, OutputItem, ResponsesUsage,
        };
        use futures::stream::StreamExt;

        // Always send stream=true — Responses upstreams refuse non-streaming.
        let mut streamed_request = request.clone();
        streamed_request.stream = true;

        // Aggregation state.
        let mut id = String::new();
        let mut model = String::new();
        let mut output: Vec<OutputItem> = Vec::new();
        let mut current_text = String::new();
        // (call_id, name) — the args buffer is maintained separately.
        let mut current_function: Option<(String, String)> = None;
        let mut args_buffer = String::new();
        let mut usage: Option<ResponsesUsage> = None;
        let mut stop_reason: Option<String> = None;

        // Helper to flush accumulated text into a message output item.
        fn flush_text(output: &mut Vec<OutputItem>, current_text: &mut String) {
            if !current_text.is_empty() {
                output.push(OutputItem::Message {
                    content: vec![ContentPart::OutputText {
                        text: std::mem::take(current_text),
                    }],
                });
            }
        }

        // Use the caller-provided timeouts (same values the Claude Code
        // non-stream path would use if Claude Code were talking to a real
        // non-streaming upstream). This keeps the "pretend non-stream"
        // aggregation under the same budget as genuine non-stream calls.
        let mut stream = Box::pin(
            self.responses_completion_stream(
                &streamed_request,
                api_key,
                first_byte_timeout,
                idle_timeout,
            )
            .await?,
        );

        while let Some(event) = stream.next().await {
            use crate::types::responses::ResponsesStreamEvent as Ev;
            match event.map_err(|e| ProxyError::Internal(format!("stream error: {e}")))? {
                Ev::Created { response } => {
                    if id.is_empty() {
                        id = response.id.clone();
                    }
                    if model.is_empty() {
                        model = response.model.clone();
                    }
                }
                Ev::ReasoningSummaryPartAdded {}
                | Ev::ReasoningSummaryTextDelta { .. }
                | Ev::ReasoningSummaryPartDone {}
                | Ev::ContentPartAdded {}
                | Ev::ContentPartDone {} => {
                    // Reasoning/content wrapper events do not affect the
                    // aggregated output on their own; text is carried by
                    // OutputTextDelta below.
                }
                Ev::OutputTextDelta { delta } => {
                    current_text.push_str(&delta);
                }
                Ev::OutputItemAdded { item } => {
                    // If we were accumulating text, flush it before starting
                    // a new function_call item.
                    flush_text(&mut output, &mut current_text);

                    if item.item_type == "function_call" {
                        let call_id = item.call_id.clone().unwrap_or_default();
                        let name = item.name.clone().unwrap_or_default();
                        current_function = Some((call_id, name));
                        args_buffer.clear();
                    }
                }
                Ev::FunctionCallArgumentsDelta { delta } => {
                    args_buffer.push_str(&delta);
                }
                Ev::FunctionCallArgumentsDone { arguments } => {
                    if !arguments.is_empty() && args_buffer.is_empty() {
                        args_buffer = arguments;
                    }
                }
                Ev::OutputItemDone {} => {
                    if let Some((call_id, name)) = current_function.take() {
                        output.push(OutputItem::FunctionCall {
                            call_id,
                            name,
                            arguments: std::mem::take(&mut args_buffer),
                        });
                    }
                }
                Ev::Completed { response } => {
                    // Final flush for any trailing text not yet pushed.
                    flush_text(&mut output, &mut current_text);

                    if id.is_empty() {
                        id = response.id.clone();
                    }
                    if model.is_empty() {
                        model = response.model.clone();
                    }
                    if let Some(u) = response.usage {
                        usage = Some(u);
                    }
                    stop_reason = response.stop_reason.clone();

                    // If upstream included a non-empty output in the completed
                    // payload itself, prefer it (it's canonical and includes
                    // any items we might have missed from delta streaming).
                    if !response.output.is_empty() {
                        output = response.output;
                    }
                    break;
                }
            }
        }

        // Final flush in case the stream ended without a completed event.
        flush_text(&mut output, &mut current_text);

        Ok(ResponsesResponse {
            id,
            model,
            output,
            usage: usage.unwrap_or(ResponsesUsage {
                input_tokens: 0,
                output_tokens: 0,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
            }),
            stop_reason,
        })
    }

    /// Send streaming Responses API call — returns a parsed SSE event stream.
    ///
    /// The Responses API stream terminates when a `response.completed` event
    /// is received (there is no separate `[DONE]` sentinel).
    pub async fn responses_completion_stream(
        &self,
        request: &ResponsesRequest,
        api_key: &str,
        first_byte_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<impl Stream<Item = Result<ResponsesStreamEvent, StreamError>> + Send, ProxyError>
    {
        let url = format!("{}/responses", self.base_url);

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let msg = ProxyError::classify_upstream(&body);
            return Err(ProxyError::Internal(msg));
        }

        let byte_stream = response.bytes_stream();
        let event_stream =
            parse_responses_sse_stream(byte_stream, first_byte_timeout, idle_timeout);

        Ok(event_stream)
    }

    /// Send streaming chat completion — returns a parsed SSE event stream
    pub async fn chat_completion_stream(
        &self,
        request: &ChatCompletionRequest,
        api_key: &str,
        first_byte_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<impl Stream<Item = Result<OpenAiSseEvent, StreamError>> + Send, ProxyError> {
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let msg = ProxyError::classify_upstream(&body);
            return Err(ProxyError::Internal(msg));
        }

        // Parse the SSE byte stream into OpenAiSseEvent with per-chunk timeouts
        let byte_stream = response.bytes_stream();
        let event_stream = parse_sse_stream(byte_stream, first_byte_timeout, idle_timeout);

        Ok(event_stream)
    }
}

/// Maximum SSE buffer size (4 MB) to prevent unbounded memory growth.
const MAX_SSE_BUFFER: usize = 4 * 1024 * 1024;

/// Parse a raw byte stream (from reqwest) into OpenAiSseEvent items.
///
/// Each chunk read is wrapped in `tokio::time::timeout` to prevent indefinite
/// blocking when the upstream pauses (e.g. during extended thinking).
fn parse_sse_stream(
    byte_stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<OpenAiSseEvent, StreamError>> + Send {
    // Accumulate raw bytes to avoid UTF-8 boundary corruption (F17)
    let line_stream = async_stream::stream! {
        let mut raw_buffer = Vec::<u8>::new();
        let mut is_first_chunk = true;

        tokio::pin!(byte_stream);
        loop {
            // Pick the right timeout: first-byte for the initial chunk, idle for subsequent ones.
            let timeout_dur = if is_first_chunk { first_byte_timeout } else { idle_timeout };

            let chunk_result = if timeout_dur.is_zero() {
                // Timeout of 0 = disabled, wait indefinitely (backward-compat).
                byte_stream.next().await
            } else {
                match timeout(timeout_dur, byte_stream.next()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let kind = if is_first_chunk { "first-byte" } else { "idle" };
                        tracing::error!(
                            "SSE stream {kind} timeout after {}s",
                            timeout_dur.as_secs()
                        );
                        yield Err(StreamError::Connection(
                            format!("stream {kind} timeout ({}s)", timeout_dur.as_secs()),
                        ));
                        return;
                    }
                }
            };

            match chunk_result {
                Some(Ok(bytes)) => {
                    is_first_chunk = false;
                    raw_buffer.extend_from_slice(&bytes);

                    // Buffer overflow protection
                    if raw_buffer.len() > MAX_SSE_BUFFER {
                        tracing::error!("SSE buffer exceeded {} bytes, aborting stream", MAX_SSE_BUFFER);
                        yield Err(StreamError::Connection("SSE buffer overflow".into()));
                        return;
                    }

                    // Process complete lines (delimited by \n)
                    while let Some(pos) = raw_buffer.iter().position(|&b| b == b'\n') {
                        let mut line_bytes = raw_buffer[..pos].to_vec();
                        raw_buffer = raw_buffer[pos + 1..].to_vec();

                        // Trim \r
                        if line_bytes.last() == Some(&b'\r') {
                            line_bytes.pop();
                        }

                        let line = String::from_utf8_lossy(&line_bytes).to_string();

                        if line.is_empty() {
                            continue;
                        }

                        if let Some(data) = line.strip_prefix("data: ") {
                            let data = data.trim();
                            if data == "[DONE]" {
                                yield Ok(OpenAiSseEvent::Done);
                                return;
                            }
                            match serde_json::from_str::<ChatCompletionChunk>(data) {
                                Ok(chunk) => yield Ok(OpenAiSseEvent::Chunk(chunk)),
                                Err(e) => {
                                    tracing::warn!("Failed to parse SSE chunk: {e}");
                                    // Skip unparseable chunks rather than failing
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    yield Err(StreamError::Connection(e.to_string()));
                    return;
                }
                None => {
                    // Stream ended normally.
                    return;
                }
            }
        }
    };

    line_stream
}

/// Parse a raw byte stream from the Responses API into `ResponsesStreamEvent` items.
///
/// Structurally identical to `parse_sse_stream` but:
/// - Deserialises `ResponsesStreamEvent` instead of `ChatCompletionChunk`.
/// - Terminates on the `response.completed` event (no `[DONE]` sentinel).
/// - Still handles `[DONE]` gracefully in case the upstream sends it anyway.
fn parse_responses_sse_stream(
    byte_stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ResponsesStreamEvent, StreamError>> + Send {
    async_stream::stream! {
        let mut raw_buffer = Vec::<u8>::new();
        let mut is_first_chunk = true;

        tokio::pin!(byte_stream);
        loop {
            let timeout_dur = if is_first_chunk { first_byte_timeout } else { idle_timeout };

            let chunk_result = if timeout_dur.is_zero() {
                byte_stream.next().await
            } else {
                match timeout(timeout_dur, byte_stream.next()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let kind = if is_first_chunk { "first-byte" } else { "idle" };
                        tracing::error!(
                            "Responses SSE stream {kind} timeout after {}s",
                            timeout_dur.as_secs()
                        );
                        yield Err(StreamError::Connection(
                            format!("responses stream {kind} timeout ({}s)", timeout_dur.as_secs()),
                        ));
                        return;
                    }
                }
            };

            match chunk_result {
                Some(Ok(bytes)) => {
                    is_first_chunk = false;
                    raw_buffer.extend_from_slice(&bytes);

                    if raw_buffer.len() > MAX_SSE_BUFFER {
                        tracing::error!(
                            "Responses SSE buffer exceeded {} bytes, aborting stream",
                            MAX_SSE_BUFFER
                        );
                        yield Err(StreamError::Connection("Responses SSE buffer overflow".into()));
                        return;
                    }

                    while let Some(pos) = raw_buffer.iter().position(|&b| b == b'\n') {
                        let mut line_bytes = raw_buffer[..pos].to_vec();
                        raw_buffer = raw_buffer[pos + 1..].to_vec();

                        if line_bytes.last() == Some(&b'\r') {
                            line_bytes.pop();
                        }

                        let line = String::from_utf8_lossy(&line_bytes).to_string();

                        if line.is_empty() {
                            continue;
                        }

                        if let Some(data) = line.strip_prefix("data: ") {
                            let data = data.trim();
                            // Guard: handle [DONE] sentinel in case upstream sends it.
                            if data == "[DONE]" {
                                return;
                            }
                            match serde_json::from_str::<ResponsesStreamEvent>(data) {
                                Ok(event) => {
                                    let is_completed = matches!(
                                        &event,
                                        ResponsesStreamEvent::Completed { .. }
                                    );
                                    yield Ok(event);
                                    if is_completed {
                                        // response.completed is the terminal event.
                                        return;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to parse Responses SSE event: {e} — data: {data}"
                                    );
                                    // Skip unparseable events rather than failing the stream.
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    yield Err(StreamError::Connection(e.to_string()));
                    return;
                }
                None => {
                    // Stream ended normally.
                    return;
                }
            }
        }
    }
}
