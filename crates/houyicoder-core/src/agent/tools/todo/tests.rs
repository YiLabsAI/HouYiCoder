//! Tests for this module.

use super::*;
use pollster::block_on;

/// Helper: build a todo input JSON with the given items.
fn input_json(items: &[(&str, &str)]) -> Value {
    let todos: Vec<Value> = items
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s, "activeForm": format!("{} {c}", s)}))
        .collect();
    json!({"todos": todos})
}

/// Helper: build a todo input without activeForm.
fn input_json_bare(items: &[(&str, &str)]) -> Value {
    let todos: Vec<Value> = items
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s}))
        .collect();
    json!({"todos": todos})
}

#[test]
fn test_empty_list_replaces_state() {
    let tool = TodoWriteTool::new();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("write code", "pending")]),
    ))
    .unwrap();
    assert_eq!(out["total"], 1);
    assert_eq!(out["pending"], 1);
    // Second call with empty list clears.
    let out = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": []}))).unwrap();
    assert_eq!(out["total"], 0);
    assert_eq!(out["old_todos"].as_array().unwrap().len(), 1);
    assert!(!out["cleared"].as_bool().unwrap());
}

#[test]
fn test_all_completed_clears_list() {
    let tool = TodoWriteTool::new();
    // Seed with three pending items.
    block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[
            ("task a", "pending"),
            ("task b", "pending"),
            ("task c", "pending"),
        ]),
    ))
    .unwrap();
    // Mark all completed.
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[
            ("task a", "completed"),
            ("task b", "completed"),
            ("task c", "completed"),
        ]),
    ))
    .unwrap();
    assert!(out["cleared"].as_bool().unwrap());
    assert_eq!(out["total"], 0);
    // The stored state is now empty.
    let handle = tool.state_handle();
    let guard = handle.lock().unwrap();
    assert!(guard.is_empty());
}

#[test]
fn test_partial_completion_keeps_list() {
    let tool = TodoWriteTool::new();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[
            ("done item", "completed"),
            ("active item", "in_progress"),
            ("later item", "pending"),
        ]),
    ))
    .unwrap();
    assert!(!out["cleared"].as_bool().unwrap());
    assert_eq!(out["completed"], 1);
    assert_eq!(out["in_progress"], 1);
    assert_eq!(out["pending"], 1);
    assert_eq!(out["total"], 3);
}

#[test]
fn test_old_todos_returned() {
    let tool = TodoWriteTool::new();
    block_on(tool.execute(ToolCtx::new("test"), input_json(&[("first", "pending")]))).unwrap();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("second", "in_progress")]),
    ))
    .unwrap();
    let old = out["old_todos"].as_array().unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[0]["content"], "first");
    assert_eq!(old[0]["status"], "pending");
    let new = out["todos"].as_array().unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0]["content"], "second");
    assert_eq!(new[0]["status"], "in_progress");
}

#[test]
fn test_active_form_optional() {
    let tool = TodoWriteTool::new();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json_bare(&[("task", "pending")]),
    ))
    .unwrap();
    let item = &out["todos"][0];
    assert_eq!(item["content"], "task");
    assert_eq!(item["status"], "pending");
    // activeForm absent from output when absent from input.
    assert!(item.get("activeForm").is_none() || item["activeForm"].is_null());
}

#[test]
fn test_output_includes_active_form() {
    let tool = TodoWriteTool::new();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("run tests", "in_progress")]),
    ))
    .unwrap();
    assert_eq!(out["todos"][0]["activeForm"], "in_progress run tests");
}

#[test]
fn test_rejects_empty_content() {
    let tool = TodoWriteTool::new();
    let result = block_on(tool.execute(
        ToolCtx::new("test"),
        json!({
            "todos": [{"content": "", "status": "pending"}]
        }),
    ));
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.message();
    assert!(msg.contains("content"));
}

#[test]
fn test_rejects_invalid_status() {
    let tool = TodoWriteTool::new();
    let result = block_on(tool.execute(
        ToolCtx::new("test"),
        json!({
            "todos": [{"content": "task", "status": "blocked"}]
        }),
    ));
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.message();
    assert!(msg.contains("status"));
}

#[test]
fn test_rejects_missing_todos_array() {
    let tool = TodoWriteTool::new();
    let result = block_on(tool.execute(ToolCtx::new("test"), json!({"items": []})));
    assert!(result.is_err());
}

#[test]
fn test_rejects_non_array_todos() {
    let tool = TodoWriteTool::new();
    let result = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": "not an array"})));
    assert!(result.is_err());
}

#[test]
fn test_shared_state_across_clone() {
    let tool = TodoWriteTool::new();
    let handle = tool.state_handle();
    let tool2 = TodoWriteTool::with_shared_state(handle);
    block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("shared task", "pending")]),
    ))
    .unwrap();
    // tool2 sees the same state because it shares the arc.
    let handle2 = tool2.state_handle();
    let guard = handle2.lock().unwrap();
    assert_eq!(guard.len(), 1);
    assert_eq!(guard[0].content, "shared task");
}

#[test]
fn test_state_accessible_via_handle() {
    let tool = TodoWriteTool::new();
    block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("a", "pending"), ("b", "in_progress")]),
    ))
    .unwrap();
    let handle = tool.state_handle();
    let guard = handle.lock().unwrap();
    assert_eq!(guard.len(), 2);
    assert_eq!(guard[0].status, TodoStatus::Pending);
    assert_eq!(guard[1].status, TodoStatus::InProgress);
}

#[test]
fn test_capability_flags() {
    let tool = TodoWriteTool::new();
    // Full-list-replace races under concurrent calls, so not safe.
    assert!(!tool.is_concurrency_safe());
    // Mutates session state.
    assert!(!tool.is_read_only());
    // In-memory and reversible (old list returned every call).
    assert!(!tool.is_destructive());
    // Meta-planning, no approval needed.
    assert!(!tool.requires_approval());
}

#[test]
fn test_full_replace_not_incremental() {
    let tool = TodoWriteTool::new();
    block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("a", "pending"), ("b", "pending"), ("c", "pending")]),
    ))
    .unwrap();
    // A replace with only one item drops the other two entirely.
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        input_json(&[("only one", "in_progress")]),
    ))
    .unwrap();
    assert_eq!(out["total"], 1);
    assert_eq!(out["todos"][0]["content"], "only one");
    let handle = tool.state_handle();
    let guard = handle.lock().unwrap();
    assert_eq!(guard.len(), 1);
}

#[test]
fn test_empty_list_not_done() {
    let tool = TodoWriteTool::new();
    // An empty list is not an all-done clear (nothing was completed).
    let out = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": []}))).unwrap();
    assert!(!out["cleared"].as_bool().unwrap());
    assert_eq!(out["total"], 0);
}

#[test]
fn test_schema_strict_object() {
    let tool = TodoWriteTool::new();
    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["todos"]["type"], "array");
    let item_schema = &schema["properties"]["todos"]["items"];
    assert_eq!(item_schema["additionalProperties"], false);
    let required = item_schema["required"].as_array().unwrap();
    assert!(required.iter().any(|r| r == "content"));
    assert!(required.iter().any(|r| r == "status"));
}
