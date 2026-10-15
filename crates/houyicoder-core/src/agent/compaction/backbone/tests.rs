//! Tests for this module.

use super::*;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use serde_json::json;

fn make_event(kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn folded_ids(events: &[SessionLogEntry]) -> HashSet<EventId> {
    events.iter().map(|e| e.id).collect()
}

fn tool_call(call_id: &str, tool: &str, input: serde_json::Value) -> SessionLogEntry {
    make_event(SessionEvent::ToolCall {
        call_id: call_id.into(),
        tool: tool.into(),
        input,
    })
}

fn tool_result(call_id: &str, output: serde_json::Value) -> SessionLogEntry {
    make_event(SessionEvent::ToolResult {
        call_id: call_id.into(),
        output,
        duration_ms: 0,
    })
}

#[test]
fn test_derive_extracts_touch_set() {
    let e1 = tool_call("c1", "read", json!({"path": "src/b.rs"}));
    let e2 = tool_call("c2", "write", json!({"path": "src/a.rs", "content": "x"}));
    let e3 = tool_call(
        "c3",
        "edit",
        json!({"path": "src/a.rs", "old_string": "x", "new_string": "y"}),
    );
    let events = vec![e1, e2, e3];
    let ids = folded_ids(&events);
    let b = derive_backbone(&events, &ids, None);
    assert_eq!(
        b.file_touch_set,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );
}

#[test]
fn test_derive_excludes_discovery_tools() {
    // glob/grep are read-only discovery — their path is a scope/pattern,
    // not a touched file, so they do not enter the file-touch set.
    let e1 = tool_call("c1", "glob", json!({"pattern": "**/*.rs", "path": "src"}));
    let e2 = tool_call("c2", "grep", json!({"pattern": "foo", "path": "src"}));
    let events = vec![e1, e2];
    let ids = folded_ids(&events);
    let b = derive_backbone(&events, &ids, None);
    assert!(b.file_touch_set.is_empty(), "discovery tools excluded");
}

#[test]
fn test_derive_extracts_test_results() {
    let pass = tool_call("c1", "bash", json!({"command": "cargo test"}));
    let pass_r = tool_result(
        "c1",
        json!({"stdout": "test result: ok", "exit_code": 0, "success": true}),
    );
    let fail = tool_call("c2", "bash", json!({"command": "cargo test"}));
    let fail_r = tool_result(
        "c2",
        json!({"stdout": "FAILED", "exit_code": 1, "success": false}),
    );
    let events = vec![pass, pass_r, fail, fail_r];
    let ids = folded_ids(&events);
    let b = derive_backbone(&events, &ids, None);
    assert_eq!(b.test_results.len(), 2);
    assert_eq!(b.test_results[0].outcome, TestOutcome::Pass);
    assert_eq!(b.test_results[1].outcome, TestOutcome::Fail);
}

#[test]
fn test_derive_todo_last_write() {
    let first = tool_call(
        "c1",
        "todo_write",
        json!({"todos": [{"content": "old", "status": "completed"}]}),
    );
    let first_r = tool_result(
        "c1",
        json!({"todos": [{"content": "old", "status": "completed"}]}),
    );
    let second = tool_call(
        "c2",
        "todo_write",
        json!({"todos": [{"content": "new", "status": "in_progress"}]}),
    );
    let second_r = tool_result(
        "c2",
        json!({"todos": [{"content": "new", "status": "in_progress"}]}),
    );
    let events = vec![first, first_r, second, second_r];
    let ids = folded_ids(&events);
    let b = derive_backbone(&events, &ids, None);
    let todos = b.todo_snapshot.expect("snapshot present");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].content, "new");
}

#[test]
fn test_log_rederivable_no_staleness() {
    // Re-deriving from the same log + ids yields an identical backbone —
    // the no-drift invariant of the append-only log layer.
    let e1 = tool_call(
        "c1",
        "edit",
        json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
    );
    let e2 = tool_call("c2", "bash", json!({"command": "cargo test"}));
    let e3 = tool_result(
        "c2",
        json!({"stdout": "ok", "exit_code": 0, "success": true}),
    );
    let events = vec![e1, e2, e3];
    let ids = folded_ids(&events);
    let b1 = derive_backbone(&events, &ids, None);
    let b2 = derive_backbone(&events, &ids, None);
    assert_eq!(b1, b2, "re-derivation is deterministic");
}

#[test]
fn test_workspace_watermark_recorded() {
    let e1 = tool_call(
        "c1",
        "edit",
        json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
    );
    let events = vec![e1];
    let ids = folded_ids(&events);
    let probe = StubWorkspaceProbe {
        rev: Some("abc123".into()),
        hash: Some("deadbeef".into()),
    };
    let b = derive_backbone(&events, &ids, Some(&probe));
    assert_eq!(b.derivation.workspace_revision.as_deref(), Some("abc123"));
    assert_eq!(b.derivation.dirty_tree_hash.as_deref(), Some("deadbeef"));
    assert_eq!(b.derivation.last_folded_event_id, events[0].id);
}

#[test]
fn test_merge_appends_backbone_llm() {
    // v1 is add-only: the merged text has BOTH the LLM summary + the
    // derived-from-log backbone block.
    let backbone = CompactBackbone {
        file_touch_set: vec!["real.rs".into()],
        test_results: vec![],
        todo_snapshot: None,
        derivation: BackboneDerivation {
            last_folded_event_id: EventId::new(),
            workspace_revision: None,
            dirty_tree_hash: None,
        },
    };
    let (merged, _conflict) = merge_summary("summary text", &backbone);
    assert!(merged.contains("summary text"), "LLM summary kept");
    assert!(
        merged.contains("derived-from-log"),
        "backbone block appended"
    );
    assert!(
        merged.contains("real.rs"),
        "backbone lists the touched file"
    );
}

#[test]
fn test_merge_flags_fabricated_path() {
    // The LLM summary mentions a path never touched (a fabrication) — the
    // conflict rate must reflect it, and the backbone block lists only the
    // real set (backbone wins).
    let backbone = CompactBackbone {
        file_touch_set: vec!["real.rs".into()],
        test_results: vec![],
        todo_snapshot: None,
        derivation: BackboneDerivation {
            last_folded_event_id: EventId::new(),
            workspace_revision: None,
            dirty_tree_hash: None,
        },
    };
    let (merged, conflict) = merge_summary("we edited fake_file.rs here", &backbone);
    assert!(conflict.llm_fabrications >= 1, "fabrication counted");
    assert!(conflict.rate > 0.0, "rate > 0");
    // Add-only: the LLM narrative is kept verbatim (the fabrication stays
    // in the LLM section — that is what the conflict rate measures). The
    // backbone block (authoritative) lists the REAL set, not the
    // fabrication: backbone wins on conflict.
    assert!(
        merged.contains("derived-from-log"),
        "backbone block present"
    );
    let block = merged
        .split("--- derived-from-log")
        .nth(1)
        .expect("block after the marker");
    assert!(block.contains("real.rs"), "backbone lists the real path");
    assert!(
        !block.contains("fake_file.rs"),
        "backbone does not carry the fabrication"
    );
}

#[test]
fn test_merge_path_no_conflict() {
    // The LLM mentions a real touched path — no fabrication, zero rate.
    let backbone = CompactBackbone {
        file_touch_set: vec!["real.rs".into()],
        test_results: vec![],
        todo_snapshot: None,
        derivation: BackboneDerivation {
            last_folded_event_id: EventId::new(),
            workspace_revision: None,
            dirty_tree_hash: None,
        },
    };
    let (_merged, conflict) = merge_summary("we edited real.rs", &backbone);
    assert_eq!(
        conflict.llm_fabrications, 0,
        "real path is not a fabrication"
    );
    assert!((conflict.rate - 0.0).abs() < 1e-9, "zero rate");
}
