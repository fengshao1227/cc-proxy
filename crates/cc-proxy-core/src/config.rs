use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::ProxyError;

/// Which model tier was matched during model mapping
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Big,
    Middle,
    Small,
}

/// Proxy configuration — loaded from env vars, .env file, or config.json
#[derive(Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub openai_api_key: String,
    #[serde(default = "default_base_url")]
    pub openai_base_url: String,
    #[serde(default = "default_big_model")]
    pub big_model: String,
    #[serde(default)]
    pub middle_model: Option<String>,
    #[serde(default = "default_small_model")]
    pub small_model: String,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    #[serde(default)]
    pub azure_api_version: Option<String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens_limit: u32,
    #[serde(default = "default_min_tokens")]
    pub min_tokens_limit: u32,
    #[serde(default = "default_timeout")]
    pub request_timeout: u64,
    /// Streaming first-byte timeout (seconds). Max wait for the first SSE chunk.
    /// Claude extended thinking can be very long, default 300s.
    #[serde(default = "default_streaming_first_byte_timeout")]
    pub streaming_first_byte_timeout: u64,
    /// Streaming idle timeout (seconds). Max gap between consecutive SSE chunks.
    /// 0 = disabled. Default 300s.
    #[serde(default = "default_streaming_idle_timeout")]
    pub streaming_idle_timeout: u64,
    /// TCP connect timeout (seconds). Default 30s.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    /// Token count scaling factor (0.0-1.0). Compensates for upstream tokenizer
    /// inflating counts vs Claude's tokenizer due to format conversion overhead.
    /// Default 0.5 (~2x inflation correction). Set 1.0 to disable.
    #[serde(default = "default_token_count_scale")]
    pub token_count_scale: f64,
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    /// Global reasoning effort fallback (none/low/medium/high/xhigh)
    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
    /// Per-tier reasoning effort (overrides global)
    #[serde(default)]
    pub big_reasoning: Option<String>,
    #[serde(default)]
    pub middle_reasoning: Option<String>,
    #[serde(default)]
    pub small_reasoning: Option<String>,
}

// Manual Debug impl to redact secrets (F14)
impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("openai_api_key", &"[REDACTED]")
            .field("openai_base_url", &self.openai_base_url)
            .field("big_model", &self.big_model)
            .field("middle_model", &self.middle_model)
            .field("small_model", &self.small_model)
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "anthropic_api_key",
                &self.anthropic_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("log_level", &self.log_level)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_big_model() -> String {
    "gpt-4o".into()
}
fn default_small_model() -> String {
    "gpt-4o-mini".into()
}
fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8082
}
fn default_log_level() -> String {
    "info".into()
}
fn default_max_tokens() -> u32 {
    128000
}
fn default_min_tokens() -> u32 {
    100
}
fn default_timeout() -> u64 {
    600
}
fn default_streaming_first_byte_timeout() -> u64 {
    300
}
fn default_streaming_idle_timeout() -> u64 {
    300
}
fn default_connect_timeout() -> u64 {
    30
}
fn default_token_count_scale() -> f64 {
    0.5
}
fn default_reasoning_effort() -> String {
    "none".into()
}

impl ProxyConfig {
    /// Effective middle model (falls back to big_model)
    pub fn effective_middle_model(&self) -> &str {
        self.middle_model.as_deref().unwrap_or(&self.big_model)
    }

    /// Get reasoning effort for a specific model tier.
    /// Priority: per-tier > global > "none"
    pub fn reasoning_for_tier(&self, tier: ModelTier) -> &str {
        let per_tier = match tier {
            ModelTier::Big => self.big_reasoning.as_deref(),
            ModelTier::Middle => self.middle_reasoning.as_deref(),
            ModelTier::Small => self.small_reasoning.as_deref(),
        };
        per_tier.unwrap_or(&self.reasoning_effort)
    }

    /// Load config: env vars > .env > config.json > defaults
    pub fn load() -> Result<Self, ProxyError> {
        // Load .env if present (won't fail if missing)
        let _ = dotenvy::dotenv();

        let openai_api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProxyError::Config("OPENAI_API_KEY not set".into()))?;

        let big_model = env_or("BIG_MODEL", &default_big_model());
        let middle_model_raw = std::env::var("MIDDLE_MODEL").ok();
        let middle_model = if middle_model_raw.as_deref() == Some("") {
            None
        } else {
            middle_model_raw
        };

        let custom_headers = Self::load_custom_headers();

        Ok(Self {
            openai_api_key,
            openai_base_url: env_or("OPENAI_BASE_URL", &default_base_url()),
            big_model,
            middle_model,
            small_model: env_or("SMALL_MODEL", &default_small_model()),
            host: env_or("HOST", &default_host()),
            port: env_or("PORT", &default_port().to_string())
                .parse()
                .unwrap_or(default_port()),
            anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            azure_api_version: std::env::var("AZURE_API_VERSION").ok(),
            log_level: env_or("LOG_LEVEL", &default_log_level()),
            max_tokens_limit: env_or("MAX_TOKENS_LIMIT", &default_max_tokens().to_string())
                .parse()
                .unwrap_or(default_max_tokens()),
            min_tokens_limit: env_or("MIN_TOKENS_LIMIT", &default_min_tokens().to_string())
                .parse()
                .unwrap_or(default_min_tokens()),
            request_timeout: env_or("REQUEST_TIMEOUT", &default_timeout().to_string())
                .parse()
                .unwrap_or(default_timeout()),
            streaming_first_byte_timeout: env_or(
                "STREAMING_FIRST_BYTE_TIMEOUT",
                &default_streaming_first_byte_timeout().to_string(),
            )
            .parse()
            .unwrap_or(default_streaming_first_byte_timeout()),
            streaming_idle_timeout: env_or(
                "STREAMING_IDLE_TIMEOUT",
                &default_streaming_idle_timeout().to_string(),
            )
            .parse()
            .unwrap_or(default_streaming_idle_timeout()),
            connect_timeout: env_or("CONNECT_TIMEOUT", &default_connect_timeout().to_string())
                .parse()
                .unwrap_or(default_connect_timeout()),
            token_count_scale: env_or(
                "TOKEN_COUNT_SCALE",
                &default_token_count_scale().to_string(),
            )
            .parse()
            .unwrap_or(default_token_count_scale()),
            custom_headers,
            reasoning_effort: env_or("REASONING_EFFORT", &default_reasoning_effort()),
            big_reasoning: std::env::var("BIG_REASONING")
                .ok()
                .filter(|s| !s.is_empty()),
            middle_reasoning: std::env::var("MIDDLE_REASONING")
                .ok()
                .filter(|s| !s.is_empty()),
            small_reasoning: std::env::var("SMALL_REASONING")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }

    /// Load from JSON config file
    pub fn load_from_file(path: &PathBuf) -> Result<Self, ProxyError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ProxyError::Config(format!("Failed to read config: {e}")))?;
        serde_json::from_str(&content)
            .map_err(|e| ProxyError::Config(format!("Invalid config JSON: {e}")))
    }

    /// Save to JSON config file (with restrictive permissions on Unix)
    pub fn save_to_file(&self, path: &PathBuf) -> Result<(), ProxyError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ProxyError::Config(format!("Failed to create config dir: {e}")))?;
            // Set directory to owner-only (F02)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| ProxyError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(path, &content)
            .map_err(|e| ProxyError::Config(format!("Failed to write config: {e}")))?;
        // Set file to owner-only read/write (F02)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Config file path: ~/.cc-proxy/config.json
    pub fn default_config_path() -> PathBuf {
        dirs_or_home().join(".cc-proxy").join("config.json")
    }

    /// Extract CUSTOM_HEADER_* env vars (blocklist sensitive headers)
    fn load_custom_headers() -> HashMap<String, String> {
        const BLOCKED: &[&str] = &[
            "host",
            "authorization",
            "content-type",
            "content-length",
            "transfer-encoding",
            "connection",
        ];
        std::env::vars()
            .filter(|(k, _)| k.starts_with("CUSTOM_HEADER_"))
            .map(|(k, v)| {
                let header_name = k[14..].replace('_', "-").to_lowercase();
                (header_name, v)
            })
            .filter(|(name, _)| !BLOCKED.contains(&name.as_str()))
            .collect()
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn dirs_or_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}
