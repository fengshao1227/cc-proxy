//! Tool ID sanitization for Claude API compatibility.
//!
//! Claude's `tool_use.id` field must match `^[a-zA-Z0-9_-]+$`. OpenAI
//! typically returns `call_xxxxx` which is already safe, but various
//! OpenAI-compatible providers (Azure, Gemini compat layer, vLLM, etc.)
//! may return ids containing dots, colons, slashes, or full UUIDs. If
//! we forward those to Claude Code unchanged, two things break:
//!
//! 1. The server-side Claude Messages validator rejects the response.
//! 2. Round-tripping fails: Claude Code echoes the mutated id back as
//!    `tool_result.tool_use_id`, and we can no longer match it against
//!    the original `call_xxx` the upstream expects.
//!
//! Algorithm adapted from CLIProxyAPI (MIT) — `util.SanitizeClaudeToolID`.
//! Any character outside `[a-zA-Z0-9_-]` is replaced with `_`. An empty
//! result falls back to a generated `toolu_<uuid>` id.

/// Replace any character outside `[a-zA-Z0-9_-]` with an underscore.
///
/// Returns a fallback `toolu_<uuid>` if the cleaned string is empty.
pub fn sanitize(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if cleaned.is_empty() {
        format!("toolu_{}", uuid::Uuid::new_v4().simple())
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_normal_openai_id() {
        assert_eq!(sanitize("call_abc123"), "call_abc123");
    }

    #[test]
    fn passes_dashed_id() {
        assert_eq!(sanitize("call-XYZ-99"), "call-XYZ-99");
    }

    #[test]
    fn replaces_dots() {
        assert_eq!(sanitize("tool.call.1"), "tool_call_1");
    }

    #[test]
    fn replaces_colons_and_slashes() {
        assert_eq!(sanitize("ns:provider/call:1"), "ns_provider_call_1");
    }

    #[test]
    fn replaces_non_ascii() {
        assert_eq!(sanitize("call_你好_1"), "call____1");
    }

    #[test]
    fn replaces_spaces() {
        assert_eq!(sanitize("call 1 2"), "call_1_2");
    }

    #[test]
    fn empty_gets_fallback_toolu() {
        let id = sanitize("");
        assert!(id.starts_with("toolu_"));
        assert!(id.len() > "toolu_".len());
    }

    #[test]
    fn all_invalid_becomes_underscores_not_fallback() {
        // Non-empty input that sanitizes to all underscores is still non-empty,
        // so no fallback — matches CPA behavior where only *empty* triggers gen.
        assert_eq!(sanitize("!!!"), "___");
    }

    #[test]
    fn unicode_replacement_does_not_panic() {
        let _ = sanitize("🔥🔥🔥");
    }

    #[test]
    fn preserves_full_alphanumeric() {
        assert_eq!(
            sanitize("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"),
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"
        );
    }
}
