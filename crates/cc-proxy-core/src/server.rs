use axum::{
    extract::{DefaultBodyLimit, Json, State},
    http::Method,
    middleware,
    response::{sse::Sse, IntoResponse},
    routing::{get, post},
    Extension, Router,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::auth;
use crate::client::UpstreamClient;
use crate::config::ProxyConfig;
use crate::convert;
use crate::error::ProxyError;
use crate::types::claude::{MessagesRequest, TokenCountRequest};
use crate::upstream;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub config: ProxyConfig,
    pub client: UpstreamClient,
    /// Upstream API protocol detected at startup.
    pub api_mode: upstream::mode::UpstreamApiMode,
}

/// Create the axum router
pub fn create_router(state: AppState) -> Router {
    let auth_key = state.config.anthropic_api_key.clone();

    // Authenticated routes (Claude API endpoints).
    //
    // count_tokens is registered and uses tiktoken o200k_base — the same
    // family OpenAI uses under the hood. This matches what the upstream
    // will actually charge, so Claude Code's context meter stays honest.
    // (Previously we let Claude Code fall back to its Anthropic BPE, which
    // over-estimates by 20-30% against OpenAI backends.)
    let api_routes = Router::new()
        .route("/v1/messages", post(create_message))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn(auth::auth_middleware))
        .layer(Extension(auth_key));

    // Public routes (health, info)
    let public_routes = Router::new()
        .route("/health", get(health))
        .route("/test-connection", get(test_connection))
        .route("/", get(root));

    // CORS: localhost only (F09)
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            let bytes = origin.as_bytes();
            bytes.starts_with(b"http://localhost")
                || bytes.starts_with(b"http://127.0.0.1")
                || bytes.starts_with(b"http://[::1]")
        }))
        .allow_methods([Method::POST, Method::GET, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any);

    api_routes
        .merge(public_routes)
        .layer(cors)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB (F20)
        .with_state(Arc::new(state))
}

/// Start the proxy server
pub async fn serve(config: ProxyConfig) -> Result<(), ProxyError> {
    let client = UpstreamClient::new(&config)?;

    // Probe the upstream to determine which API protocol to use.
    let api_mode = upstream::detector::detect(&config, client.inner_client()).await?;
    tracing::info!("upstream API mode: {}", api_mode.as_str());

    let addr = format!("{}:{}", config.host, config.port);

    let state = AppState {
        config: config.clone(),
        client,
        api_mode,
    };

    let app = create_router(state);

    tracing::info!("Proxy listening on {addr}");

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|source| ProxyError::BindFailed {
            addr: addr.clone(),
            source,
        })?;

    // Graceful shutdown on SIGTERM/SIGINT (F32)
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Shutdown signal received, draining connections...");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| ProxyError::Internal(format!("Server error: {e}")))?;

    Ok(())
}

// ===== Handlers =====

async fn create_message(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MessagesRequest>,
) -> Result<impl IntoResponse, ProxyError> {
    // Precise token counting using tiktoken BPE tokenizer (same family as OpenAI).
    // Counts from the ORIGINAL Claude-format request before OpenAI conversion,
    // giving Claude Code accurate context window tracking.
    let estimated_input_tokens = crate::token_count::count_request_tokens(&request);
    let msg_count = request.messages.len();

    // Build the canonical → original tool name map from the inbound request's
    // `tools` array. Used by response/stream converters to restore tool names
    // if the upstream provider mutates casing.
    //
    // For the Responses path we also register the **shortened** aliases that
    // `claude_to_responses` will apply to over-64-byte tool names, so name
    // restoration works when the upstream echoes back the short form.
    // See convert::responses::request::build_tool_name_map.
    let tool_name_map = match state.api_mode {
        upstream::mode::UpstreamApiMode::Responses => {
            convert::responses::request::build_tool_name_map(request.tools.as_deref())
        }
        upstream::mode::UpstreamApiMode::ChatCompletions => {
            crate::util::tool_name::build_map(request.tools.as_deref())
        }
    };

    tracing::info!(
        model = %request.model,
        stream = ?request.stream,
        messages = msg_count,
        tiktoken_input = estimated_input_tokens,
        max_tokens = request.max_tokens,
        tools = tool_name_map.len(),
        "→ request"
    );

    let first_byte_timeout = Duration::from_secs(state.config.streaming_first_byte_timeout);
    let idle_timeout = Duration::from_secs(state.config.streaming_idle_timeout);
    let is_stream = request.stream.unwrap_or(false);

    match state.api_mode {
        upstream::mode::UpstreamApiMode::Responses => {
            let responses_request =
                convert::responses::request::claude_to_responses(&request, &state.config);

            if is_stream {
                let event_stream = state
                    .client
                    .responses_completion_stream(
                        &responses_request,
                        &state.config.openai_api_key,
                        first_byte_timeout,
                        idle_timeout,
                    )
                    .await?;

                let claude_stream = convert::responses::stream::responses_stream_to_claude(
                    event_stream,
                    request.model.clone(),
                    idle_timeout,
                    estimated_input_tokens,
                    tool_name_map,
                );

                Ok(Sse::new(claude_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(15))
                            .text("ping"),
                    )
                    .into_response())
            } else {
                let responses_response = state
                    .client
                    .responses_completion(
                        &responses_request,
                        &state.config.openai_api_key,
                        first_byte_timeout,
                        idle_timeout,
                    )
                    .await?;

                let claude_response = convert::responses::response::responses_to_claude(
                    &responses_response,
                    &request.model,
                    estimated_input_tokens,
                    &tool_name_map,
                );

                Ok(Json(claude_response).into_response())
            }
        }

        upstream::mode::UpstreamApiMode::ChatCompletions => {
            let openai_request = convert::request::claude_to_openai(&request, &state.config);

            if is_stream {
                // Streaming response — with per-chunk timeout protection
                let event_stream = state
                    .client
                    .chat_completion_stream(
                        &openai_request,
                        &state.config.openai_api_key,
                        first_byte_timeout,
                        idle_timeout,
                    )
                    .await?;

                let claude_stream = convert::stream::openai_stream_to_claude(
                    event_stream,
                    request.model.clone(),
                    idle_timeout,
                    estimated_input_tokens,
                    tool_name_map,
                );

                Ok(Sse::new(claude_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(15))
                            .text("ping"),
                    )
                    .into_response())
            } else {
                // Non-streaming response
                let openai_response = state
                    .client
                    .chat_completion(&openai_request, &state.config.openai_api_key)
                    .await?;

                let claude_response = convert::response::openai_to_claude(
                    &openai_response,
                    &request.model,
                    estimated_input_tokens,
                    &tool_name_map,
                );

                Ok(Json(claude_response).into_response())
            }
        }
    }
}

/// `/v1/messages/count_tokens` — pre-flight token estimate for Claude Code.
///
/// Claude Code calls this before sending the real request, to decide how
/// much room is left in the context window. We answer with a tiktoken
/// o200k_base count (same family used by OpenAI backends under the hood),
/// so the estimate actually matches what the upstream will bill.
async fn count_tokens(Json(request): Json<TokenCountRequest>) -> Json<serde_json::Value> {
    let tokens = crate::token_count::count_token_count_request(&request);
    tracing::info!(
        messages = request.messages.len(),
        has_tools = request.tools.is_some(),
        tokens,
        "count_tokens"
    );
    Json(serde_json::json!({ "input_tokens": tokens }))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono_now(),
        "openai_api_configured": !state.config.openai_api_key.is_empty(),
        "client_api_key_validation": state.config.anthropic_api_key.is_some(),
    }))
}

async fn test_connection(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let test_req = crate::types::openai::ChatCompletionRequest {
        model: state.config.small_model.clone(),
        messages: vec![crate::types::openai::ChatMessage {
            role: "user".into(),
            content: Some(crate::types::openai::ChatContent::Text("Hello".into())),
            tool_calls: None,
            tool_call_id: None,
        }],
        max_tokens: 5,
        temperature: Some(0.0),
        top_p: None,
        stream: false,
        stop: None,
        tools: None,
        tool_choice: None,
        stream_options: None,
        reasoning_effort: None,
    };

    match state
        .client
        .chat_completion(&test_req, &state.config.openai_api_key)
        .await
    {
        Ok(resp) => Json(serde_json::json!({
            "status": "success",
            "message": "Connected to upstream API",
            "model_used": state.config.small_model,
            "response_id": resp.id,
        }))
        .into_response(),
        Err(e) => Json(serde_json::json!({
            "status": "failed",
            "error": e.to_string(),
        }))
        .into_response(),
    }
}

async fn root(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "message": format!("cc-proxy v{}", env!("CARGO_PKG_VERSION")),
        "status": "running",
        "config": {
            "openai_base_url": state.config.openai_base_url,
            "big_model": state.config.big_model,
            "middle_model": state.config.effective_middle_model(),
            "small_model": state.config.small_model,
        },
        "endpoints": {
            "messages": "/v1/messages",
            "count_tokens": "/v1/messages/count_tokens",
            "health": "/health",
            "test_connection": "/test-connection",
        }
    }))
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}
