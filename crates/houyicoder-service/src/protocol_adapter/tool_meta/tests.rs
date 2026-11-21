//! Tests for this module.

use super::*;
use serde_json::json;

/// A shell call shows the command's first line as its title and is an
/// execute call with no file target.
#[test]
fn test_bash_title_carries_command() {
    let input = json!({ "command": "cargo test\ncargo clippy" });
    assert_eq!(tool_title("bash", Some(&input)), "Run: cargo test");
    assert_eq!(tool_kind("bash"), ToolKind::Execute);
    assert!(tool_locations("bash", Some(&input)).is_empty());
}

/// A long command is capped so the title stays one line.
#[test]
fn test_bash_title_truncates() {
    let long = "x".repeat(120);
    let input = json!({ "command": long });
    let title = tool_title("bash", Some(&input));
    assert!(title.ends_with("..."), "{title}");
    assert!(
        title.chars().count() <= "Run: ".len() + COMMAND_TITLE_MAX + 3,
        "{title}"
    );
}

/// A file write names the path in its title and carries that path as the
/// jump target.
#[test]
fn test_write_title_names_path() {
    let input = json!({ "path": "src/lib.rs", "content": "x" });
    assert_eq!(tool_title("write", Some(&input)), "Edit src/lib.rs");
    assert_eq!(tool_kind("write"), ToolKind::Edit);
    let locations = tool_locations("write", Some(&input));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].path, "src/lib.rs");
}

/// Reading and searching map to their own categories, and a search names no
/// jump target because a pattern is not a file.
#[test]
fn test_read_and_search_kinds() {
    let read = json!({ "path": "src/main.rs" });
    assert_eq!(tool_title("read", Some(&read)), "Read src/main.rs");
    assert_eq!(tool_kind("read"), ToolKind::Read);

    let search = json!({ "pattern": "TODO" });
    assert_eq!(tool_title("grep", Some(&search)), "Search TODO");
    assert_eq!(tool_kind("glob"), ToolKind::Search);
    assert!(tool_locations("grep", Some(&search)).is_empty());
}

/// A tool with no mapping keeps its own name and the catch-all category.
#[test]
fn test_unknown_tool_falls_back() {
    assert_eq!(tool_title("task_list", None), "task_list");
    assert_eq!(tool_kind("task_list"), ToolKind::Other);
    assert!(tool_locations("task_list", None).is_empty());
}

/// An input that names no target still yields a readable title and no jump
/// location.
#[test]
fn test_missing_target_falls_back() {
    assert_eq!(tool_title("bash", None), "Run a command");
    assert_eq!(tool_title("write", None), "Edit with write");
    assert_eq!(tool_title("read", None), "Read a file");
    assert_eq!(tool_title("grep", None), "Search");
    assert!(tool_locations("write", None).is_empty());

    let empty = json!({ "path": "" });
    assert!(tool_locations("read", Some(&empty)).is_empty());
}
