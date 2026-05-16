use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Upstream error: {0}")]
    Upstream(#[from] reqwest::Error),

    #[error("Request conversion error: {0}")]
    Conversion(String),

    #[error("Streaming error: {0}")]
    Streaming(String),

    #[error("Timeout")]
    Timeout,

    #[error("Client disconnected")]
    ClientDisconnected,

    #[error("Failed to bind {addr}: {source}")]
    BindFailed {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Internal(String),
}

impl ProxyError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Auth(_) => StatusCode::UNAUTHORIZED,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::ClientDisconnected => {
                StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST)
            }
            Self::Upstream(e) => {
                if e.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else if e.is_connect() {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Classify upstream error messages for user-friendly guidance
    pub fn classify_upstream(msg: &str) -> String {
        let lower = msg.to_lowercase();
        if lower.contains("unsupported_country_region_territory") {
            return "OpenAI API is not available in your region. Consider using DeepSeek, Azure, or a local model.".into();
        }
        if lower.contains("invalid_api_key") || lower.contains("unauthorized") {
            return "Invalid API key. Check your OPENAI_API_KEY.".into();
        }
        if lower.contains("rate_limit") || lower.contains("quota") {
            return "Rate limit exceeded. Wait and retry, or upgrade your plan.".into();
        }
        if lower.contains("model")
            && (lower.contains("not found") || lower.contains("does not exist"))
        {
            return "Model not found. Check BIG_MODEL / SMALL_MODEL config.".into();
        }
        msg.to_string()
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": self.to_string()
            }
        });
        (status, axum::Json(body)).into_response()
    }
}
