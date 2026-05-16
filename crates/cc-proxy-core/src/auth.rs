use axum::{extract::Request, http::StatusCode, middleware::Next, response::Response, Extension};
use serde_json::json;

/// Middleware: validate client API key if ANTHROPIC_API_KEY is configured.
/// The expected key is injected via Extension<Option<String>>.
pub async fn auth_middleware(
    Extension(expected_key): Extension<Option<String>>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<serde_json::Value>)> {
    // If no expected key configured, skip validation
    let Some(ref expected) = expected_key else {
        return Ok(next.run(request).await);
    };

    // Extract client key from x-api-key header or Authorization: Bearer
    let client_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| {
            request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(String::from)
        });

    match client_key {
        Some(ref key) if key == expected => Ok(next.run(request).await),
        _ => {
            tracing::warn!("Invalid API key from client");
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({
                    "type": "error",
                    "error": {
                        "type": "authentication_error",
                        "message": "Invalid API key."
                    }
                })),
            ))
        }
    }
}
