//! Tests for this module.

use super::*;
use serde_json::Value;

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
fn test_memory_tools_key_field() {
    // The keyed memory tools take a "key" field, show_memory plus an
    // optional scope; the chip shows the key, not a raw JSON glimpse.
    for tool in [
        "save_memory",
        "delete_memory",
        "promote_memory",
        "demote_memory",
        "show_memory",
    ] {
        let input = serde_json::json!({ "key": "rule-x" });
        assert_eq!(
            tool_invocation(tool, &input),
            "rule-x",
            "{tool} should pull key"
        );
        assert!(
            !tool_invocation(tool, &input).contains('{'),
            "{tool} must not dump JSON"
        );
    }
}

#[test]
fn test_search_pulls_query() {
    // search_memory takes a free-text query rather than a key; the chip
    // and the search index must show the query, not a JSON glimpse of it.
    let input = serde_json::json!({ "query": "how the deploy gate works" });
    assert_eq!(
        tool_invocation("search_memory", &input),
        "how the deploy gate works"
    );
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
    assert_eq!(tool_invocation("bash", &Value::Null), "null");
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

// A known tool renders its one meaningful argument as a scalar field
// rather than as a JSON dump, and an unknown tool falls back to JSON.
// Adding a tool whose args deserve a scalar field without updating the
// known list makes it show as JSON here, which is the drift signal.
#[test]
fn test_known_tools_render_scalar() {
    let cases: &[(&str, Value, &str)] = &[
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
            "{tool}: known tool must use its scalar field, not JSON"
        );
        assert!(!inv.contains('{'), "{tool}: must not fall through to JSON");
    }
    // Unknown tools fall back to canonical JSON (non-empty, deterministic).
    let inv = tool_invocation("WebFetch", &serde_json::json!({ "url": "https://a" }));
    assert!(inv.contains("url"));
    assert!(!inv.is_empty());
}
