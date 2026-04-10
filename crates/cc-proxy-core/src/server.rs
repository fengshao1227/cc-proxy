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
use crate::types::claude::{MessageContent, MessagesRequest, SystemContent, TokenCountRequest};

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub config: ProxyConfig,
    pub client: UpstreamClient,
}

/// Create the axum router
pub fn create_router(state: AppState) -> Router {
    let auth_key = state.config.anthropic_api_key.clone();

    // Authenticated routes (Claude API endpoints)
    // NOTE: count_tokens intentionally NOT registered — let Claude Code
    // fall back to its own internal tokenizer (more accurate than any
    // proxy-side estimate). cc-switch also omits this endpoint.
    let api_routes = Router::new()
        .route("/v1/messages", post(create_message))
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
    let addr = format!("{}:{}", config.host, config.port);

    let state = AppState {
        config: config.clone(),
        client,
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
    tracing::info!(
        model = %request.model,
        stream = ?request.stream,
        messages = msg_count,
        tiktoken_input = estimated_input_tokens,
        max_tokens = request.max_tokens,
        "→ request"
    );

    let openai_request = convert::request::claude_to_openai(&request, &state.config);

    if request.stream.unwrap_or(false) {
        // Streaming response — with per-chunk timeout protection
        let first_byte_timeout = Duration::from_secs(state.config.streaming_first_byte_timeout);
        let idle_timeout = Duration::from_secs(state.config.streaming_idle_timeout);

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
        );

        Ok(Json(claude_response).into_response())
    }
}

#[allow(dead_code)]
async fn count_tokens(Json(request): Json<TokenCountRequest>) -> Json<serde_json::Value> {
    let mut total_chars: usize = 0;

    if let Some(ref system) = request.system {
        match system {
            SystemContent::Text(s) => total_chars += s.len(),
            SystemContent::Blocks(blocks) => {
                for b in blocks {
                    if let Some(ref text) = b.text {
                        total_chars += text.len();
                    }
                }
            }
        }
    }

    for msg in &request.messages {
        match &msg.content {
            MessageContent::Text(s) => total_chars += s.len(),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let crate::types::claude::ContentBlock::Text { text } = block {
                        total_chars += text.len();
                    }
                }
            }
            MessageContent::Null => {}
        }
    }

    // More accurate estimate: ~2.5 chars per token for mixed English/code content,
    // plus overhead for message formatting (~4 tokens per message).
    let msg_overhead = request.messages.len() * 4;
    let estimated_tokens = ((total_chars as f64 / 2.5) as usize + msg_overhead).max(1);

    tracing::info!(
        total_chars = total_chars,
        messages = request.messages.len(),
        estimated_tokens = estimated_tokens,
        "count_tokens"
    );

    Json(serde_json::json!({ "input_tokens": estimated_tokens }))
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
