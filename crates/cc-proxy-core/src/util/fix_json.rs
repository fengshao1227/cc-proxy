//! Repair non-standard tool argument JSON before parsing.
//!
//! Some OpenAI-compat providers emit `function.arguments` that isn't strictly
//! RFC 8259 — typically single-quoted strings, e.g.
//!
//! ```text
//! {'path': '/tmp/file', 'contents': 'He said "hi"'}
//! ```
//!
//! If we hand that directly to `serde_json::from_str`, it bails out and the
//! whole `tool_use.input` either becomes a `{"raw_arguments": "..."}`
//! placeholder (current fallback) or — worse — a broken tool call. This
//! module cheaply repairs single-quoted strings into double-quoted ones
//! with the necessary escaping, so the downstream JSON parser can consume
//! the result.
//!
//! Algorithm adapted from CLIProxyAPI (MIT) — `util.FixJSON`.
//!
//! # Rules
//!
//! - Existing double-quoted JSON strings are preserved byte-for-byte. Escapes
//!   inside them (`\n`, `\"`, `\\`, etc.) pass through unchanged.
//! - Single-quoted strings are converted to double-quoted strings.
//! - Inside a converted string, any literal `"` is escaped as `\"`.
//! - Common backslash escapes (`\n`, `\r`, `\t`, `\b`, `\f`, `\/`, `\\`,
//!   `\"`) inside single-quoted strings are kept intact.
//! - `\'` inside a single-quoted string becomes a literal `'` (no escaping
//!   needed in the double-quoted output).
//! - `\uXXXX` escapes are forwarded as-is, consuming up to four hex digits.
//! - Unknown escape sequences are preserved (backslash + next char).
//! - If the input ends mid-single-quote, a closing `"` is appended as a
//!   best-effort recovery.
//!
//! The function does not attempt to repair other non-JSON features (trailing
//! commas, comments, unquoted keys, etc.). For those, a lenient parser is
//! still needed — but in practice single quotes are by far the most common
//! failure mode observed in the wild.

/// Convert non-standard (often single-quoted) JSON into a form that should
/// be parseable by a strict JSON parser.
///
/// If the input is already valid JSON, the output is **byte-identical** to
/// the input except in the single case where stray single quotes exist
/// outside of double-quoted strings (those get interpreted as string
/// delimiters — same semantics as CPA's `FixJSON`).
pub fn fix_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());

    let mut in_double = false;
    let mut in_single = false;
    // Within the *current* string, was the previous char a backslash?
    let mut escaped = false;

    // We iterate over `char`s so multi-byte UTF-8 is preserved correctly.
    // CPA's Go implementation does the same (range over []rune).
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // --- Inside a standard (double-quoted) JSON string ---
        if in_double {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        // --- Inside a (to-be-converted) single-quoted string ---
        if in_single {
            if escaped {
                escaped = false;
                match c {
                    // Standard escapes: keep backslash + char.
                    'n' | 'r' | 't' | 'b' | 'f' | '/' | '"' => {
                        out.push('\\');
                        out.push(c);
                    }
                    // \\ → \\ (two chars out)
                    '\\' => {
                        out.push('\\');
                        out.push('\\');
                    }
                    // \' inside a single-quoted string becomes a literal '.
                    // In the double-quoted output, ' needs no escaping.
                    '\'' => {
                        out.push('\'');
                    }
                    // \uXXXX — forward the escape and up to 4 hex digits.
                    'u' => {
                        out.push('\\');
                        out.push('u');
                        let mut consumed = 0;
                        while consumed < 4 && i + 1 < n {
                            let peek = chars[i + 1];
                            if peek.is_ascii_hexdigit() {
                                out.push(peek);
                                i += 1;
                                consumed += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    // Unknown escape: preserve backslash + char verbatim.
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                i += 1;
                continue;
            }

            // Not currently in an escape.
            if c == '\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if c == '\'' {
                // End of the single-quoted string.
                out.push('"');
                in_single = false;
                i += 1;
                continue;
            }
            // Regular character inside the converted string.
            // A literal double quote must be escaped in the double-quoted
            // output so the resulting JSON remains valid.
            if c == '"' {
                out.push('\\');
                out.push('"');
            } else {
                out.push(c);
            }
            i += 1;
            continue;
        }

        // --- Outside any string ---
        if c == '"' {
            in_double = true;
            out.push(c);
        } else if c == '\'' {
            // Start of a non-standard single-quoted string.
            in_single = true;
            out.push('"');
        } else {
            out.push(c);
        }
        i += 1;
    }

    // Best-effort recovery: if we ended while still inside a single-quoted
    // string, close it so the output is at least shaped like valid JSON.
    if in_single {
        out.push('"');
    }

    out
}

/// Try `serde_json::from_str` first; on failure, run `fix_json` and retry.
///
/// This keeps the common (strict JSON) path zero-cost — we only pay the
/// rewrite when the upstream actually emitted something non-standard.
pub fn parse_lenient(input: &str) -> Result<serde_json::Value, serde_json::Error> {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => Ok(v),
        Err(_) => serde_json::from_str::<serde_json::Value>(&fix_json(input)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(s: &str) -> serde_json::Value {
        parse_lenient(s).expect("should parse")
    }

    #[test]
    fn strict_json_passes_through() {
        let input = r#"{"a":1,"b":"hello"}"#;
        assert_eq!(fix_json(input), input);
    }

    #[test]
    fn single_quotes_become_double() {
        let input = r#"{'a': 1, 'b': '2'}"#;
        let got = fix_json(input);
        // The keys and values should now be double-quoted.
        assert_eq!(got, r#"{"a": 1, "b": "2"}"#);
    }

    #[test]
    fn embedded_double_quote_gets_escaped() {
        let input = r#"{'t': 'He said "hi"'}"#;
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["t"], "He said \"hi\"");
    }

    #[test]
    fn escaped_single_quote_becomes_literal() {
        // \' inside a single-quoted string → literal '
        let input = r"{'msg': 'it\'s fine'}";
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["msg"], "it's fine");
    }

    #[test]
    fn newline_escape_preserved() {
        let input = r"{'m': 'line1\nline2'}";
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["m"], "line1\nline2");
    }

    #[test]
    fn backslash_escape_preserved() {
        let input = r"{'path': 'C:\\Users\\root'}";
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["path"], "C:\\Users\\root");
    }

    #[test]
    fn unicode_escape_forwarded() {
        let input = r"{'c': '\u4f60\u597d'}";
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["c"], "你好");
    }

    #[test]
    fn double_quoted_strings_unchanged_when_mixed() {
        // Double-quoted strings should pass through even when other parts
        // of the input are single-quoted.
        let input = r#"{"a": 'mix', "b": "normal"}"#;
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["a"], "mix");
        assert_eq!(v["b"], "normal");
    }

    #[test]
    fn nested_object_with_single_quotes() {
        let input = r#"{'outer': {'inner': 'val'}}"#;
        let v = parse(input);
        assert_eq!(v, json!({"outer": {"inner": "val"}}));
    }

    #[test]
    fn unterminated_single_quote_best_effort() {
        // Input ends while still inside a single-quoted string — close it.
        let input = "{'a': 'oops";
        let got = fix_json(input);
        assert!(got.ends_with('"'));
    }

    #[test]
    fn parse_lenient_uses_strict_first() {
        // A valid JSON with single quotes only inside a double-quoted
        // string should parse via strict path, not via fix_json.
        let input = r#"{"message": "it's fine"}"#;
        let v = parse_lenient(input).unwrap();
        assert_eq!(v["message"], "it's fine");
    }

    #[test]
    fn parse_lenient_falls_back_to_fix() {
        let input = r#"{'name': 'Read', 'path': '/tmp'}"#;
        let v = parse_lenient(input).unwrap();
        assert_eq!(v["name"], "Read");
        assert_eq!(v["path"], "/tmp");
    }

    #[test]
    fn empty_object_passes() {
        assert_eq!(fix_json("{}"), "{}");
    }

    #[test]
    fn array_with_single_quoted_strings() {
        let input = r#"['a', 'b', 'c']"#;
        let v = parse(input);
        assert_eq!(v, json!(["a", "b", "c"]));
    }

    #[test]
    fn tab_and_cr_escape_preserved() {
        let input = r"{'m': 'a\tb\rc'}";
        let got = fix_json(input);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["m"], "a\tb\rc");
    }
}
