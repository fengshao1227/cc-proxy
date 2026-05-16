//! Tool name canonicalization — restore original casing after upstream mutations.
//!
//! Claude Code's tool registry is case-sensitive: it registers tools like
//! `Read`, `Write`, `Grep`, and will reject `read` or `READ` as unknown.
//!
//! Many OpenAI-compat providers silently mutate `function.name` on the way
//! back. Observed in the wild:
//!
//! | Upstream                    | What they return for `Read` |
//! |-----------------------------|------------------------------|
//! | Certain GLM-4 deployments   | `read` (lowercased)          |
//! | Qwen fine-tunes             | `_Read` (prefixed)           |
//! | Early DeepSeek Chat         | `read_file` → `readfile`     |
//! | Some vLLM builds            | `Read-file` → `Read_file`    |
//! | Azure GPT-4 (rare)          | `READ` (uppercased)          |
//!
//! If we forward the mutated name, Claude Code says "Tool 'read' not found"
//! and the whole tool call is wasted — even though the model intent was
//! perfect. The fix is a canonical-form lookup: at request time we record
//! every declared tool's canonical form → original name; at response time
//! we canonicalize whatever the upstream returned and look it up to restore
//! the original casing.
//!
//! Algorithm adapted from CLIProxyAPI (MIT) — `util.CanonicalToolName`,
//! `util.ToolNameMapFromClaudeRequest`, `util.MapToolName`.

use std::collections::HashMap;

use crate::types::claude::Tool;

/// Compute a loose canonical form used as the lookup key.
///
/// The form is: `trim whitespace → drop leading underscores → lowercase`.
/// Two names that differ only in case, leading underscores, or surrounding
/// whitespace collapse to the same canonical string.
pub fn canonical(name: &str) -> String {
    name.trim().trim_start_matches('_').to_lowercase()
}

/// A canonical → original tool name map.
pub type ToolNameMap = HashMap<String, String>;

/// Build a canonical-name → original-name map from the Claude request tools.
///
/// First occurrence wins (matches CPA behavior): if two tools share a
/// canonical form, the one that appeared first in the request is kept.
/// Returns an empty map when there are no tools.
pub fn build_map(tools: Option<&[Tool]>) -> ToolNameMap {
    let mut map = ToolNameMap::new();
    let Some(tools) = tools else { return map };

    for tool in tools {
        let key = canonical(&tool.name);
        if !key.is_empty() {
            map.entry(key).or_insert_with(|| tool.name.clone());
        }
    }
    map
}

/// Restore the original tool name from whatever the upstream returned.
///
/// Canonicalizes the incoming `got` and looks it up in `map`. Returns the
/// original declared name on hit; falls back to `got` unchanged on miss so
/// unknown tools (e.g. ones the model hallucinated) still flow through
/// untouched.
pub fn restore(map: &ToolNameMap, got: &str) -> String {
    if got.is_empty() || map.is_empty() {
        return got.to_string();
    }
    map.get(&canonical(got))
        .cloned()
        .unwrap_or_else(|| got.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: None,
            input_schema: json!({}),
        }
    }

    #[test]
    fn canonical_basic() {
        assert_eq!(canonical("Read"), "read");
        assert_eq!(canonical("READ"), "read");
        assert_eq!(canonical("read"), "read");
        assert_eq!(canonical("_Read"), "read");
        assert_eq!(canonical("___Read"), "read");
        assert_eq!(canonical("  Read  "), "read");
        assert_eq!(canonical(""), "");
    }

    #[test]
    fn build_map_skeleton_tools() {
        let tools = vec![tool("Read"), tool("Write"), tool("Grep")];
        let map = build_map(Some(&tools));
        assert_eq!(map.get("read"), Some(&"Read".to_string()));
        assert_eq!(map.get("write"), Some(&"Write".to_string()));
        assert_eq!(map.get("grep"), Some(&"Grep".to_string()));
    }

    #[test]
    fn build_map_none_returns_empty() {
        let map = build_map(None);
        assert!(map.is_empty());
    }

    #[test]
    fn build_map_first_occurrence_wins() {
        // Two tools with the same canonical form — first one is retained.
        let tools = vec![tool("Read"), tool("_read")];
        let map = build_map(Some(&tools));
        assert_eq!(map.get("read"), Some(&"Read".to_string()));
    }

    #[test]
    fn build_map_ignores_empty_names() {
        let tools = vec![tool(""), tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("read"), Some(&"Read".to_string()));
    }

    #[test]
    fn restore_lowercase_to_original() {
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, "read"), "Read");
    }

    #[test]
    fn restore_uppercase_to_original() {
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, "READ"), "Read");
    }

    #[test]
    fn restore_prefixed_underscore_to_original() {
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, "_Read"), "Read");
    }

    #[test]
    fn restore_already_correct_is_noop() {
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, "Read"), "Read");
    }

    #[test]
    fn restore_unknown_tool_passes_through() {
        // Tool the model hallucinated — not in declared tools.
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, "Fly"), "Fly");
    }

    #[test]
    fn restore_empty_map_is_passthrough() {
        let map = build_map(None);
        assert_eq!(restore(&map, "anything"), "anything");
    }

    #[test]
    fn restore_empty_name_is_passthrough() {
        let tools = vec![tool("Read")];
        let map = build_map(Some(&tools));
        assert_eq!(restore(&map, ""), "");
    }
}
