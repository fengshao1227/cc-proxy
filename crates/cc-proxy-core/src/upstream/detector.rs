use std::time::Duration;

use crate::config::ProxyConfig;
use crate::error::ProxyError;
use crate::upstream::mode::UpstreamApiMode;

/// Probe the upstream API type by trying ChatCompletions first, falling back to Responses.
///
/// Order of attempts:
///   1. POST `<base>/chat/completions` with a minimal body → if 200 or 4xx (non-404/405), use ChatCompletions
///   2. POST `<base>/responses` with a minimal body → if 200 or 4xx (non-404/405), use Responses
///   3. If both fail (network error, 404, 405, 5xx) → return `ProxyError::Internal`
///
/// Rationale: 4xx non-404/405 (e.g. 401 auth / 400 bad model / 403 rate limit) still prove
/// the endpoint *exists*. Only 404 / 405 / network error / 5xx mean "endpoint unreachable".
///
/// # Why ChatCompletions is the default
///
/// Virtually every OpenAI-compatible relay implements `/v1/chat/completions`,
/// and cc-proxy's ChatCompletions translator has been battle-tested for many
/// releases. The Responses path is newer, and many relays (e.g. api.150226.xyz)
/// accept the endpoint but subtly reject fields like `stream:false` or certain
/// `reasoning.effort` values — producing confusing 500s at real-request time
/// even though a probe might pass. Defaulting to ChatCompletions keeps the
/// common case rock-solid; Responses is reserved for backends that *only*
/// implement the new API.
///
/// Each probe has a 10-second timeout; total worst-case is ~20 seconds.
pub async fn detect(
    config: &ProxyConfig,
    client: &reqwest::Client,
) -> Result<UpstreamApiMode, ProxyError> {
    let base = config.openai_base_url.trim_end_matches('/');
    let chat_url = format!("{}/chat/completions", base);
    let responses_url = format!("{}/responses", base);

    tracing::info!("→ probing ChatCompletions at {}", chat_url);

    match try_endpoint(
        client,
        &chat_url,
        chat_probe_body(&config.big_model),
        &config.openai_api_key,
    )
    .await
    {
        Ok(true) => {
            tracing::info!("✅ ChatCompletions available, using ChatCompletions mode");
            return Ok(UpstreamApiMode::ChatCompletions);
        }
        Ok(false) => {
            tracing::warn!(
                "❌ ChatCompletions unreachable at {}, falling back to Responses",
                chat_url
            );
        }
        Err(e) => {
            tracing::warn!(
                "❌ ChatCompletions probe returned unexpected error: {}, falling back to Responses",
                e
            );
        }
    }

    tracing::info!("→ probing Responses API at {}", responses_url);

    match try_endpoint(
        client,
        &responses_url,
        responses_probe_body(&config.big_model),
        &config.openai_api_key,
    )
    .await
    {
        Ok(true) => {
            tracing::info!("✅ Responses API available, using Responses mode");
            return Ok(UpstreamApiMode::Responses);
        }
        Ok(false) => {
            tracing::error!("❌ Responses API also unreachable at {}", responses_url);
        }
        Err(e) => {
            tracing::error!("❌ Responses API probe error: {}", e);
        }
    }

    Err(ProxyError::Internal(format!(
        "upstream does not support ChatCompletions API ({}/chat/completions) or \
         Responses API ({}/responses) at this base URL — \
         check OPENAI_BASE_URL and network connectivity",
        base, base
    )))
}

/// Probe a single endpoint by sending a minimal request.
///
/// Returns:
/// - `Ok(true)`  — endpoint exists (200, or 4xx that is not 404/405)
/// - `Ok(false)` — endpoint missing or unreliable (404, 405, 5xx, network error)
///
/// The caller may treat `Err` from this function as `Ok(false)` (fallback).
async fn try_endpoint(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
    api_key: &str,
) -> Result<bool, ProxyError> {
    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            if status.is_success() {
                return Ok(true);
            }
            if status == 404 || status == 405 {
                // Endpoint does not exist at this path.
                return Ok(false);
            }
            if status.is_client_error() {
                // 401 (bad key), 400 (bad model), 403 (rate limit) —
                // all prove the endpoint *exists*.
                return Ok(true);
            }
            if status.is_server_error() {
                // 5xx: endpoint may exist but is unreliable; treat as missing
                // so we fall back to the next candidate.
                return Ok(false);
            }
            Ok(false)
        }
        Err(_) => {
            // Network error, DNS failure, connection refused, etc.
            Ok(false)
        }
    }
}

/// Minimal probe body for the Responses API.
fn responses_probe_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hi" }
                ]
            }
        ],
        "max_output_tokens": 16,
        "stream": false
    })
}

/// Minimal probe body for the ChatCompletions API.
fn chat_probe_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": "hi" }
        ],
        "max_tokens": 16,
        "stream": false
    })
}

// ===== Unit tests =====
//
// We spin up a real but ephemeral TCP listener on 127.0.0.1:0 (OS-assigned
// port) that returns a fixed HTTP status code, avoiding any external
// dependencies or new crates.
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a minimal HTTP server that always replies with the given status code.
    /// Returns the address it is listening on.
    async fn mock_server(status: u16) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                // Read the request (we don't care about the body)
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status} Status\r\n\
                     Content-Length: 2\r\n\
                     Content-Type: application/json\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {{}}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    /// Build a minimal `ProxyConfig` pointing to the given base URL.
    fn make_config(base_url: String) -> ProxyConfig {
        ProxyConfig {
            openai_api_key: "test-key".into(),
            openai_base_url: base_url,
            big_model: "test-model".into(),
            middle_model: None,
            small_model: "test-small".into(),
            host: "127.0.0.1".into(),
            port: 0,
            anthropic_api_key: None,
            azure_api_version: None,
            log_level: "error".into(),
            max_tokens_limit: 4096,
            min_tokens_limit: 1,
            request_timeout: 30,
            streaming_first_byte_timeout: 30,
            streaming_idle_timeout: 30,
            connect_timeout: 5,
            token_count_scale: 1.0,
            custom_headers: Default::default(),
            reasoning_effort: "none".into(),
            big_reasoning: None,
            middle_reasoning: None,
            small_reasoning: None,
        }
    }

    fn plain_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    // ── Detection order tests ───────────────────────────────────────────────
    //
    // NOTE: as of the "ChatCompletions-first" refactor, detect() probes
    // `/chat/completions` BEFORE `/responses`. A mock that returns the same
    // status code on every path therefore yields `ChatCompletions` (the
    // first probe wins). Fallback tests must force a real failure on the
    // ChatCompletions path to reach the Responses fallback.

    /// A 200 on all paths → ChatCompletions mode (first probe wins).
    #[tokio::test]
    async fn test_all_200_returns_chat_mode() {
        let addr = mock_server(200).await;
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let mode = detect(&config, &client).await.unwrap();
        assert_eq!(mode, UpstreamApiMode::ChatCompletions);
    }

    /// A 401 on all paths (bad key) still proves the ChatCompletions
    /// endpoint exists → ChatCompletions mode.
    #[tokio::test]
    async fn test_all_401_still_returns_chat_mode() {
        let addr = mock_server(401).await;
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let mode = detect(&config, &client).await.unwrap();
        assert_eq!(mode, UpstreamApiMode::ChatCompletions);
    }

    /// 404 on `/chat/completions` and 200 on `/responses` → Responses mode.
    ///
    /// This is the legacy "Responses-only backend" case and must keep
    /// working after the probe order flip.
    #[tokio::test]
    async fn test_chat_404_falls_back_to_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let status = if req.contains("POST /chat/completions") {
                    404u16
                } else {
                    200u16
                };
                let response = format!(
                    "HTTP/1.1 {status} Status\r\n\
                     Content-Length: 2\r\n\
                     Content-Type: application/json\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {{}}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let mode = detect(&config, &client).await.unwrap();
        assert_eq!(mode, UpstreamApiMode::Responses);
    }

    /// Both endpoints return 404 → error.
    #[tokio::test]
    async fn test_both_404_returns_error() {
        let addr = mock_server(404).await;
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let result = detect(&config, &client).await;
        assert!(
            result.is_err(),
            "expected error when both endpoints return 404"
        );
    }

    /// Network error on `/chat/completions` (connection dropped) and 200 on
    /// `/responses` → fall back to Responses.
    #[tokio::test]
    async fn test_chat_network_error_falls_back_to_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                if req.contains("POST /chat/completions") {
                    // Simulate connection drop — just close immediately.
                    drop(stream);
                } else {
                    let response = "HTTP/1.1 200 OK\r\n\
                                    Content-Length: 2\r\n\
                                    Content-Type: application/json\r\n\
                                    Connection: close\r\n\
                                    \r\n\
                                    {}";
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            }
        });
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let mode = detect(&config, &client).await.unwrap();
        assert_eq!(mode, UpstreamApiMode::Responses);
    }

    /// 5xx on `/chat/completions` and 200 on `/responses` → fall back to
    /// Responses (5xx is treated as "endpoint unreliable").
    #[tokio::test]
    async fn test_chat_5xx_falls_back_to_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let status = if req.contains("POST /chat/completions") {
                    500u16
                } else {
                    200u16
                };
                let response = format!(
                    "HTTP/1.1 {status} Status\r\n\
                     Content-Length: 2\r\n\
                     Content-Type: application/json\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {{}}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let base = format!("http://{}", addr);
        let config = make_config(base);
        let client = plain_client();
        let mode = detect(&config, &client).await.unwrap();
        assert_eq!(mode, UpstreamApiMode::Responses);
    }
}
