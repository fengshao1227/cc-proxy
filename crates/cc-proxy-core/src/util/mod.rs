//! Small helper modules used across the proxy.
//!
//! Each submodule handles one surgical concern:
//! - [`tool_id`]: sanitize OpenAI tool_call ids for Claude's regex.
//! - [`tool_name`]: restore original tool names after providers mutate casing.
//! - [`fix_json`]: repair non-standard tool argument JSON (single quotes, etc.).

pub mod fix_json;
pub mod tool_id;
pub mod tool_name;
