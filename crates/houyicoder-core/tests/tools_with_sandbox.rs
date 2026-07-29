//! Integration tests for BashTool/WriteTool/ReadTool against a real macOS
//! Seatbelt sandbox. Run with: make test-integration. Mac-only.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_core::agent::{
    BashTool, EditTool, GlobTool, GrepTool, MultiEditTool, ReadTool, WriteTool,
};
use houyicoder_sandbox::MacSeatbeltSession;
use serde_json::json;

fn session() -> Arc<dyn SandboxSession> {
    Arc::new(MacSeatbeltSession::new().expect("seatbelt session"))
}

#[tokio::test]
async fn test_bash_runs_echo() {
    let t = BashTool::new(session());
    let out = t
        .execute(ToolCtx::new("test"), json!({"command": "echo tool-works"}))
        .await
        .expect("bash executes");
    assert_eq!(out["stdout"].as_str().unwrap().trim(), "tool-works");
    assert_eq!(out["success"], true);
    assert!(t.requires_approval());
    assert!(t.is_destructive());
}

#[tokio::test]
async fn test_write_then_read() {
    let s = session();
    let w = WriteTool::new(s.clone());
    let r = ReadTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "dir/f.txt", "content": "hello tools"}),
    )
    .await
    .expect("write executes");
    let out = r
        .execute(ToolCtx::new("test"), json!({"path": "dir/f.txt"}))
        .await
        .expect("read executes");
    assert_eq!(out["content"].as_str().unwrap(), "hello tools");
    assert!(!r.requires_approval());
    assert!(r.is_read_only());
    assert!(w.requires_approval());
}

#[tokio::test]
async fn test_bash_no_command() {
    let t = BashTool::new(session());
    let err = t
        .execute(ToolCtx::new("test"), json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("command"));
}

#[tokio::test]
async fn test_edit_replaces_returns_diff() {
    let s = session();
    let w = WriteTool::new(s.clone());
    let e = EditTool::new(s.clone());
    let r = ReadTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "src/lib.rs", "content": "fn foo() {\n    1\n}\n"}),
    )
    .await
    .expect("write");
    let out = e
        .execute(
            ToolCtx::new("test"),
            json!({
                "path": "src/lib.rs",
                "old_string": "    1",
                "new_string": "    2"
            }),
        )
        .await
        .expect("edit");
    assert_eq!(out["occurrences_replaced"], 1);
    assert!(out["diff"].as_str().unwrap().contains("-    1"));
    assert!(out["diff"].as_str().unwrap().contains("+    2"));
    let after = r
        .execute(ToolCtx::new("test"), json!({"path": "src/lib.rs"}))
        .await
        .expect("read");
    assert_eq!(after["content"].as_str().unwrap(), "fn foo() {\n    2\n}\n");
    assert!(e.requires_approval());
    assert!(e.is_destructive());
}

#[tokio::test]
async fn test_edit_multi_needs_replace() {
    let s = session();
    WriteTool::new(s.clone())
        .execute(
            ToolCtx::new("test"),
            json!({"path": "a.txt", "content": "a a a"}),
        )
        .await
        .expect("write");
    let err = EditTool::new(s)
        .execute(
            ToolCtx::new("test"),
            json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Found 3 matches"));
}

#[tokio::test]
async fn test_multiedit_atomic_rollback_failure() {
    let s = session();
    let w = WriteTool::new(s.clone());
    let m = MultiEditTool::new(s.clone());
    let r = ReadTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "m.rs", "content": "alpha\nbeta\ngamma\n"}),
    )
    .await
    .expect("write");
    // Second edit references a string that does not exist ⇒ whole batch
    // rolls back, file must be unchanged.
    let err = m
        .execute(
            ToolCtx::new("test"),
            json!({
                "path": "m.rs",
                "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "nonexistent", "new_string": "X"}
                ]
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
    let after = r
        .execute(ToolCtx::new("test"), json!({"path": "m.rs"}))
        .await
        .expect("read");
    assert_eq!(after["content"].as_str().unwrap(), "alpha\nbeta\ngamma\n");
}

#[tokio::test]
async fn test_multiedit_applies_all_ordered() {
    let s = session();
    let w = WriteTool::new(s.clone());
    let m = MultiEditTool::new(s.clone());
    let r = ReadTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "m2.rs", "content": "one\ntwo\nthree\n"}),
    )
    .await
    .expect("write");
    let out = m
        .execute(
            ToolCtx::new("test"),
            json!({
                "path": "m2.rs",
                "edits": [
                    {"old_string": "one", "new_string": "ONE"},
                    {"old_string": "three", "new_string": "THREE"}
                ]
            }),
        )
        .await
        .expect("multiedit");
    assert_eq!(out["edits_applied"], 2);
    let after = r
        .execute(ToolCtx::new("test"), json!({"path": "m2.rs"}))
        .await
        .expect("read");
    assert_eq!(after["content"].as_str().unwrap(), "ONE\ntwo\nTHREE\n");
}

#[tokio::test]
async fn test_glob_finds_files_session() {
    let s = session();
    let w = WriteTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "src/a.rs", "content": "fn a() {}"}),
    )
    .await
    .expect("write");
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "src/b.rs", "content": "fn b() {}"}),
    )
    .await
    .expect("write");
    let g = GlobTool::new(s);
    let out = g
        .execute(ToolCtx::new("test"), json!({"pattern": "**/*.rs"}))
        .await
        .expect("glob");
    let files = out["filenames"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for f in files {
        let p = f.as_str().unwrap();
        assert!(
            !p.starts_with('/'),
            "expected relative path, got absolute: {p}"
        );
    }
    assert!(files.iter().any(|f| f.as_str().unwrap().contains("a.rs")));
    assert!(files.iter().any(|f| f.as_str().unwrap().contains("b.rs")));
    assert!(g.is_read_only());
    assert!(!g.requires_approval());
}

#[tokio::test]
async fn test_grep_searches_content_session() {
    let s = session();
    let w = WriteTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "src/a.rs", "content": "fn alpha() {}\n"}),
    )
    .await
    .expect("write");
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "src/b.rs", "content": "fn beta() {}\n"}),
    )
    .await
    .expect("write");
    let g = GrepTool::new(s);
    let out = g
        .execute(
            ToolCtx::new("test"),
            json!({"pattern": "alpha", "output_mode": "content"}),
        )
        .await
        .expect("grep");
    let content = out["content"].as_str().unwrap();
    assert!(content.contains("alpha"));
    let files = out["filenames"].as_array().unwrap();
    for f in files {
        let p = f.as_str().unwrap();
        assert!(
            !p.starts_with('/'),
            "expected relative path, got absolute: {p}"
        );
    }
    assert!(g.is_read_only());
    assert!(!g.requires_approval());
}

#[tokio::test]
async fn test_grep_rejects_path_traversal() {
    let s = session();
    let w = WriteTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "a.rs", "content": "match\n"}),
    )
    .await
    .expect("write");
    let g = GrepTool::new(s);
    let err = g
        .execute(
            ToolCtx::new("test"),
            json!({"pattern": "match", "path": "../../etc"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("workspace"));
}

#[tokio::test]
async fn test_glob_rejects_absolute_escape() {
    let s = session();
    let w = WriteTool::new(s.clone());
    w.execute(ToolCtx::new("test"), json!({"path": "a.rs", "content": ""}))
        .await
        .expect("write");
    let g = GlobTool::new(s);
    let err = g
        .execute(ToolCtx::new("test"), json!({"pattern": "/etc/*"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("workspace"));
}

#[tokio::test]
async fn test_read_rejects_zero_budget() {
    // A zero byte budget would truncate to an empty read, which the transcript
    // mislabels "Read 0 lines (empty file)" for a non-empty file. Reject it so
    // the cause surfaces as an error, not a silent empty body.
    let s = session();
    let w = WriteTool::new(s.clone());
    w.execute(
        ToolCtx::new("test"),
        json!({"path": "a.rs", "content": "non-empty content\n"}),
    )
    .await
    .expect("write");
    let r = ReadTool::new(s);
    let err = r
        .execute(
            ToolCtx::new("test"),
            json!({"path": "a.rs", "max_bytes": 0}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("max_bytes"),
        "zero max_bytes must be rejected: {err}"
    );
}
