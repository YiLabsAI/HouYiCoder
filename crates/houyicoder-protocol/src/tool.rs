//! Untruncated tool-call invocation text, shared by the verbose transcript
//! render and the search index. A single projection for both is what makes
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
/// persisted-event projections of the same call agree byte for byte (the
/// provenance guarantee the search jump relies on). Non-objects pass through
/// verbatim.
fn canonical_json(input: &Value) -> String {
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bash_pulls_command_untruncated() {
        let long = "x".repeat(400);
        let input = serde_json::json!({ "command": long });
        let inv = tool_invocation("bash", &input);
        assert_eq!(inv.len(), 400);
        assert!(!inv.contains('…'));
        assert!(!inv.contains('{'));
    }

    #[test]
    fn test_read_path_not_content() {
        let input = serde_json::json!({ "path": "src/foo.rs", "content": "secret" });
        let inv = tool_invocation("read", &input);
        assert_eq!(inv, "src/foo.rs");
        assert!(!inv.contains("secret"));
    }

    #[test]
    fn test_edit_pulls_path() {
        let input = serde_json::json!({
            "path": "a.rs",
            "old_string": "x",
            "new_string": "y\nz\nw"
        });
        assert_eq!(tool_invocation("edit", &input), "a.rs");
    }

    #[test]
    fn test_grep_pulls_pattern() {
        let input = serde_json::json!({ "pattern": "fn \\w+", "output_mode": "content" });
        assert_eq!(tool_invocation("grep", &input), "fn \\w+");
    }

    #[test]
    fn test_unrecognized_tool_preserves_keys() {
        let input = serde_json::json!({ "b": 2, "a": 1, "c": 3 });
        let inv = tool_invocation("WebFetch", &input);
        // serde preserve_order keeps insertion order so a user-authored
        // settings file round-trips without key reshuffling.
        assert_eq!(inv, r#"{"b":2,"a":1,"c":3}"#);
    }

    #[test]
    fn test_known_tool_missing_field() {
        let input = serde_json::json!({ "path": "x" });
        let inv = tool_invocation("bash", &input);
        assert_eq!(inv, r#"{"path":"x"}"#);
    }

    #[test]
    fn test_null_input_yields_null() {
        assert_eq!(tool_invocation("bash", &serde_json::Value::Null), "null");
    }

    // I3: never truncated — a 300-char command and a large unknown-tool JSON
    // both come back whole, with no ellipsis and no byte loss. A truncated
    // index silently misses the tail; this pins that tool_invocation does not.
    #[test]
    fn test_invocation_not_truncated() {
        let long_cmd = "git rebase -i ".to_string() + &"x".repeat(300);
        let bash = serde_json::json!({ "command": long_cmd });
        let inv = tool_invocation("bash", &bash);
        assert_eq!(inv.len(), long_cmd.len());
        assert!(!inv.contains('…'));

        // Unknown tool with a large nested payload: full canonical JSON, not
        // a 60-char glimpse and not truncated.
        let big = serde_json::json!({ "prompt": "x".repeat(300), "k": 1 });
        let inv = tool_invocation("SomeNewTool", &big);
        assert!(inv.contains(&"x".repeat(300)));
        assert!(!inv.contains('…'));
    }

    // I4: known tools project to their scalar field is projected to that field
    // (not dumped as JSON), and unknown tools fall back to JSON. Adding a
    // tool whose args deserve a field projection without updating the known
    // list makes it show as JSON here — a visible drift signal.
    #[test]
    fn test_known_tools_project_scalar() {
        let cases: &[(&str, serde_json::Value, &str)] = &[
            ("bash", serde_json::json!({ "command": "ls" }), "ls"),
            ("read", serde_json::json!({ "path": "a.rs" }), "a.rs"),
            (
                "write",
                serde_json::json!({ "path": "b.rs", "content": "x" }),
                "b.rs",
            ),
            (
                "edit",
                serde_json::json!({ "path": "c.rs", "old_string": "x" }),
                "c.rs",
            ),
            ("multiedit", serde_json::json!({ "path": "d.rs" }), "d.rs"),
            ("grep", serde_json::json!({ "pattern": "fn" }), "fn"),
            (
                "glob",
                serde_json::json!({ "pattern": "**/*.rs" }),
                "**/*.rs",
            ),
        ];
        for (tool, input, want) in cases {
            let inv = tool_invocation(tool, input);
            assert_eq!(
                inv, *want,
                "{tool}: known tool must project to its scalar field, not JSON"
            );
            assert!(!inv.contains('{'), "{tool}: must not fall through to JSON");
        }
        // Unknown tools fall back to canonical JSON (non-empty, deterministic).
        let inv = tool_invocation("WebFetch", &serde_json::json!({ "url": "https://a" }));
        assert!(inv.contains("url"));
        assert!(!inv.is_empty());
    }
}
