//! The tool domain as the wire layer reads it: the untruncated invocation
//! text a call carries, and whether a result reports a failure.
//!
//! Untruncated tool-call invocation text, shared by the verbose transcript
//! render and the search index. A single mapping for both is what makes
//! index-equals-render a structural guarantee rather than a convention: the
//! same tool_invocation(tool, input) string is what the verbose view paints
//! and what the search scans, so a match is always on text the user can see.
//!
//! Per-tool field selection picks the high-signal scalar (command / path /
//! pattern). Unknown tools fall back to canonical (key-sorted) full input
//! JSON so a long-tail tool args stay findable — serde_json uses a BTreeMap
//! (this workspace does not enable preserve_order), so a Value::Object
//! serializes with sorted keys, deterministic across wire and persistence.
//! The chip display truncates this; the verbose view and search use it
//! verbatim. Never truncated here: a truncated index silently misses the tail
//! of a long command with no signal to the user.

use serde_json::Value;

mod ansi;
mod bash_command;
mod failure;
pub use ansi::strip_ansi;
pub use bash_command::simple_command_word;
pub use failure::tool_result_failed;

/// The untruncated call-line argument for a tool call. Known tools pick the
/// scalar field the chip already shows; unknown tools keep the full input so
/// nothing is silently unsearchable. Returns null only for a null input with
/// no known field — an honest nothing-to-index, never a silent miss of a
/// real tail.
pub fn tool_invocation(tool: &str, input: &Value) -> String {
    let field = match tool {
        "bash" => "command",
        "read" | "write" | "edit" | "multiedit" => "path",
        "grep" | "glob" => "pattern",
        "save_memory" | "delete_memory" | "promote_memory" | "demote_memory" | "show_memory" => {
            "key"
        }
        "search_memory" => "query",
        _ => return canonical_json(input),
    };
    match input.get(field).and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => canonical_json(input),
    }
}

/// Canonical (key-sorted) JSON for an unknown tool input. With preserve_order
/// off, a Value::Object is a BTreeMap and to_string emits sorted keys —
/// deterministic regardless of insertion order, so the wire-frame and
/// persisted-event mappings of the same call agree byte for byte (the
/// provenance guarantee the search jump relies on). Non-objects pass through
/// verbatim.
fn canonical_json(input: &Value) -> String {
    input.to_string()
}

#[cfg(test)]
mod tests;
