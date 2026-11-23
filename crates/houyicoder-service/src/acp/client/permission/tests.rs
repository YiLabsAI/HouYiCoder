//! Tests for permission interception.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;

use super::{InterceptedVerdict, check_fence, check_gate, verdict_response};
use houyicoder_permission::{DefaultModeGate, ModeGate, PermissionMode};
use houyicoder_protocol::acp_wire::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
};
use houyicoder_protocol::frontend::session_update::{
    ToolCallId, ToolCallLocation, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::json;

fn ask_with(paths: Vec<&str>, input: Option<serde_json::Value>) -> RequestPermissionRequest {
    let mut fields = ToolCallUpdateFields::new();
    for p in paths {
        fields = fields.locations(vec![ToolCallLocation::new(p)]);
    }
    let mut fields = fields;
    if let Some(v) = input {
        fields.raw_input = Some(v);
    }
    RequestPermissionRequest {
        session_id: "s".into(),
        tool_call: ToolCallUpdate::new(ToolCallId::new("call-1"), fields),
        options: vec![
            PermissionOption {
                option_id: "allow_once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
                scope: None,
                meta: None,
            },
            PermissionOption {
                option_id: "reject_once".into(),
                name: "Reject once".into(),
                kind: PermissionOptionKind::RejectOnce,
                scope: None,
                meta: None,
            },
        ],
        meta: None,
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("perm-fence-{}-{}", tag, id()));
    create_dir_all(&dir).expect("root dir");
    dir
}

/// A path outside the workspace root is an escape and yields a local reject.
#[test]
fn test_fence_rejects_escape() {
    let root = temp_root("escape");
    let ask = ask_with(vec!["../outside.txt"], None);
    assert!(matches!(
        check_fence(&ask, &root),
        Some(InterceptedVerdict::Reject)
    ));
    remove_dir_all(&root).ok();
}

/// An absolute path outside the root is an escape too.
#[test]
fn test_fence_rejects_absolute_escape() {
    let root = temp_root("abs");
    let outside = temp_dir().join("perm-fence-outside-target.txt");
    let ask = ask_with(vec![&outside.display().to_string()], None);
    assert!(matches!(
        check_fence(&ask, &root),
        Some(InterceptedVerdict::Reject)
    ));
    remove_dir_all(&root).ok();
}

/// A path inside the root is not an escape.
#[test]
fn test_fence_allows_inside() {
    let root = temp_root("inside");
    let ask = ask_with(vec!["src/main.rs"], None);
    assert!(check_fence(&ask, &root).is_none());
    remove_dir_all(&root).ok();
}

/// An ask with no path carries nothing to fence.
#[test]
fn test_fence_no_path() {
    let root = temp_root("nopath");
    let ask = ask_with(vec![], Some(json!({"command": "ls"})));
    assert!(check_fence(&ask, &root).is_none());
    remove_dir_all(&root).ok();
}

/// The raw input path is checked when locations are empty.
#[test]
fn test_fence_checks_input_path() {
    let root = temp_root("input");
    let ask = ask_with(vec![], Some(json!({"path": "../secret.txt"})));
    assert!(matches!(
        check_fence(&ask, &root),
        Some(InterceptedVerdict::Reject)
    ));
    remove_dir_all(&root).ok();
}

/// A gate allow answers the child without asking the user.
#[test]
fn test_gate_allow_answers() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let ask = ask_with(vec![], Some(json!({"tool_name": "bash"})));
    assert!(matches!(
        check_gate(Some(&gate), &ask),
        Some(InterceptedVerdict::Allow)
    ));
}

/// A verdict response selects the option of the matching kind.
#[test]
fn test_verdict_response_selects() {
    let ask = ask_with(vec![], None);
    let resp = verdict_response(&ask, &InterceptedVerdict::Reject);
    match resp.outcome {
        RequestPermissionOutcome::Selected(sel) => {
            assert_eq!(sel.option_id, "reject_once");
        }
        other => panic!("expected Selected, got {other:?}"),
    }
}

/// A cancel verdict produces the Cancelled outcome.
#[test]
fn test_verdict_response_cancels() {
    let ask = ask_with(vec![], None);
    let resp = verdict_response(&ask, &InterceptedVerdict::Cancel);
    assert!(matches!(resp.outcome, RequestPermissionOutcome::Cancelled));
}

/// A verdict with no matching option falls back to cancellation.
#[test]
fn test_verdict_no_option_cancels() {
    let mut ask = ask_with(vec![], None);
    ask.options
        .retain(|o| o.kind == PermissionOptionKind::AllowOnce);
    let resp = verdict_response(&ask, &InterceptedVerdict::Reject);
    assert!(matches!(resp.outcome, RequestPermissionOutcome::Cancelled));
}

/// A workspace root that exists resolves through symlinks.
#[test]
fn test_fence_symlink_root() {
    let root = temp_root("symlink");
    let link = temp_dir().join(format!("perm-link-{}", id()));
    remove_file(&link).ok();
    #[cfg(unix)]
    symlink(&root, &link).expect("symlink");
    let ask = ask_with(vec!["src/main.rs"], None);
    assert!(check_fence(&ask, &link).is_none());
    remove_file(&link).ok();
    remove_dir_all(&root).ok();
}

/// A host with no gate refuses the ask: a synthesized permissive gate would
/// grant a process child what the session never approved.
#[test]
fn test_no_gate_refuses() {
    let ask = ask_with(vec![], Some(json!({"tool_name": "bash"})));
    assert!(
        matches!(check_gate(None, &ask), Some(InterceptedVerdict::Reject)),
        "an unwired gate is a refusal, never an allow"
    );
}
