use crate::config::{ModelTier, ProxyConfig};

/// Result of model mapping: the target model name and which tier it matched
#[derive(Debug, Clone)]
pub struct MappedModel {
    pub model: String,
    pub tier: Option<ModelTier>,
}

/// Map Claude model names to configured OpenAI-compatible model names.
/// Returns the mapped model and which tier it matched (for per-tier reasoning).
pub fn map_model(claude_model: &str, config: &ProxyConfig) -> MappedModel {
    let lower = claude_model.to_lowercase();

    if lower.contains("haiku") {
        MappedModel {
            model: config.small_model.clone(),
            tier: Some(ModelTier::Small),
        }
    } else if lower.contains("sonnet") {
        MappedModel {
            model: config.effective_middle_model().to_string(),
            tier: Some(ModelTier::Middle),
        }
    } else if lower.contains("opus") {
        MappedModel {
            model: config.big_model.clone(),
            tier: Some(ModelTier::Big),
        }
    } else if lower.starts_with("claude") {
        // Unknown Claude variant — default to big
        MappedModel {
            model: config.big_model.clone(),
            tier: Some(ModelTier::Big),
        }
    } else {
        // Non-Claude model — pass through as-is
        MappedModel {
            model: claude_model.to_string(),
            tier: None,
        }
    }
}

/// Legacy helper: just get the model name (for backward compat)
pub fn map_model_name(claude_model: &str, config: &ProxyConfig) -> String {
    map_model(claude_model, config).model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ProxyConfig {
        ProxyConfig {
            openai_api_key: "test".into(),
            openai_base_url: "https://api.openai.com/v1".into(),
            big_model: "gpt-4o".into(),
            middle_model: Some("gpt-4o".into()),
            small_model: "gpt-4o-mini".into(),
            host: "0.0.0.0".into(),
            port: 8082,
            anthropic_api_key: None,
            azure_api_version: None,
            log_level: "info".into(),
            max_tokens_limit: 128000,
            min_tokens_limit: 100,
            request_timeout: 600,
            streaming_first_byte_timeout: 300,
            streaming_idle_timeout: 300,
            connect_timeout: 30,
            token_count_scale: 1.0,
            custom_headers: Default::default(),
            reasoning_effort: "none".into(),
            big_reasoning: None,
            middle_reasoning: None,
            small_reasoning: None,
        }
    }

    #[test]
    fn test_haiku_mapping() {
        let cfg = test_config();
        let m = map_model("claude-3-5-haiku-20241022", &cfg);
        assert_eq!(m.model, "gpt-4o-mini");
        assert_eq!(m.tier, Some(ModelTier::Small));
    }

    #[test]
    fn test_sonnet_mapping() {
        let cfg = test_config();
        let m = map_model("claude-3-5-sonnet-20241022", &cfg);
        assert_eq!(m.model, "gpt-4o");
        assert_eq!(m.tier, Some(ModelTier::Middle));
    }

    #[test]
    fn test_opus_mapping() {
        let cfg = test_config();
        let m = map_model("claude-3-opus-20240229", &cfg);
        assert_eq!(m.model, "gpt-4o");
        assert_eq!(m.tier, Some(ModelTier::Big));
    }

    #[test]
    fn test_passthrough() {
        let cfg = test_config();
        let m = map_model("gpt-4o", &cfg);
        assert_eq!(m.model, "gpt-4o");
        assert_eq!(m.tier, None);
    }

    #[test]
    fn test_per_tier_reasoning() {
        let mut cfg = test_config();
        cfg.reasoning_effort = "low".into();
        cfg.big_reasoning = Some("xhigh".into());
        cfg.small_reasoning = Some("none".into());

        // Big tier uses its own reasoning
        assert_eq!(cfg.reasoning_for_tier(ModelTier::Big), "xhigh");
        // Middle falls back to global
        assert_eq!(cfg.reasoning_for_tier(ModelTier::Middle), "low");
        // Small uses its own
        assert_eq!(cfg.reasoning_for_tier(ModelTier::Small), "none");
    }
}
