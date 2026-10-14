use super::super::*;
use crate::todo_view::{TodoStatus, TodoView};
use houyicoder_protocol::frontend::status::{
    SessionDescriptorSummary, SessionProvenance, StatusSnapshot,
};
use houyicoder_protocol::llm::Usage;

fn snap(model: &str) -> StatusSnapshot {
    StatusSnapshot {
        model: model.into(),
        breaker_state: None,
        breaker_reason: None,
        breaker_cool_down_secs: None,
        cumulative_usage: Usage {
            input_tokens: 100,
            output_tokens: 40,
            total_tokens: 140,
            non_cached_input_tokens: 10,
            cache_read_input_tokens: 80,
            cache_write_input_tokens: 10,
            reasoning_tokens: 5,
        },
        last_input_tokens: 100,
        context_window: 200_000,
        tool_calls: 3,
        tool_success: 2,
        tool_errors: 1,
        descriptor: None,
        version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    }
}

fn snap_with_descriptor(model: &str) -> StatusSnapshot {
    let mut s = snap(model);
    s.descriptor = Some(SessionDescriptorSummary {
        name: Some("fix login bug".to_string()),
        cwd: "/work/app".to_string(),
        model: model.into(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        provenance: SessionProvenance::Fresh,
    });
    s
}

fn item(content: &str, status: TodoStatus) -> TodoView {
    TodoView {
        content: content.into(),
        status,
        active_form: None,
    }
}

#[test]
fn test_todo_section_empty_cleared() {
    assert!(render_todo_section(&[]).is_empty());
}

#[test]
fn test_todo_section_lists_glyphs() {
    let todos = vec![
        item("done", TodoStatus::Completed),
        item("now", TodoStatus::InProgress),
        item("later", TodoStatus::Pending),
    ];
    let out = render_todo_section(&todos);
    assert!(out.contains("tasks: 3 (1 done, 1 in progress, 0 paused, 1 open)"));
    // Grouped order: in-progress first, then pending, then completed.
    let now_pos = out.find("◼ now").unwrap();
    let later_pos = out.find("◻ later").unwrap();
    let done_pos = out.find("✔ done").unwrap();
    assert!(now_pos < later_pos && later_pos < done_pos);
}

#[test]
fn test_status_renders_config_fields() {
    let s = render_status(
        &snap("glm-5.1"),
        &SessionId::new("test"),
        "mac-seatbelt",
        &[],
    );
    assert!(s.contains("glm-5.1") && s.contains("Model:"), "model: {s}");
    assert!(s.contains("Auth token:"), "auth token source: {s}");
    assert!(s.contains("Anthropic base URL:"), "base url: {s}");
    assert!(s.contains("Setting sources:"), "setting sources: {s}");
    assert!(s.contains("sandbox:"), "sandbox: {s}");
    // Tokens + wall duration moved to the Usage tab.
    assert!(!s.contains("tokens:"), "no tokens in status: {s}");
    assert!(
        !s.contains("tasks:"),
        "no todo section when none wired: {s}"
    );
}

#[test]
fn test_status_folds_todo_present() {
    let todos = vec![item("run tests", TodoStatus::InProgress)];
    let s = render_status(
        &snap("glm-5.1"),
        &SessionId::new("test"),
        "mac-seatbelt",
        &todos,
    );
    assert!(s.contains("tasks: 1 (0 done, 1 in progress, 0 paused, 0 open)"));
    assert!(s.contains("◼ run tests"));
}

#[test]
fn test_status_renders_identity_block() {
    let s = render_status(
        &snap_with_descriptor("glm-5.1"),
        &SessionId::new("sess-123"),
        "mac-seatbelt",
        &[],
    );
    // Identity order: Version, Session name, Session ID, cwd, then
    // Auth token / base URL / Model / sandbox / Setting sources, then
    // provenance last.
    let v = s.find(env!("CARGO_PKG_VERSION")).unwrap();
    let n = s.find("fix login bug").unwrap();
    let id = s.find("sess-123").unwrap();
    let cwd = s.find("/work/app").unwrap();
    let prov = s.find("provenance:").unwrap();
    assert!(
        v < n && n < id && id < cwd,
        "version < name < id < cwd: {s}"
    );
    assert!(prov > cwd, "provenance after cwd: {s}");
}

#[test]
fn test_status_without_descriptor_unnamed() {
    let s = render_status(
        &snap("glm-5.1"),
        &SessionId::new("sess-123"),
        "mac-seatbelt",
        &[],
    );
    // No descriptor: Session name still renders (an unnamed session shows a
    // placeholder, never blank), falling to "(unnamed)". Version is the
    // running build (top-level on the snapshot, set by the server) so it
    // always renders; cwd and provenance come from the descriptor and drop.
    assert!(s.contains("(unnamed)") && s.contains("Session name:"));
    assert!(s.contains("Version:"));
    assert!(!s.contains("cwd:"));
    assert!(!s.contains("provenance:"));
    assert!(s.contains("sess-123") && s.contains("Session ID:"));
}

/// A child's descriptor carries no cwd; the status drops the line rather
/// than rendering it blank, and still renders the provenance -- the fact
/// the descriptor does hold.
#[test]
fn test_child_status_drops_cwd() {
    let mut s = snap_with_descriptor("glm-5.1");
    s.descriptor = s.descriptor.map(|mut m| {
        m.cwd = String::new();
        m.provenance = SessionProvenance::SpawnedBy {
            parent_session_id: "parent-1".into(),
            subagent_type: "explore".into(),
            task_id: "task-7".into(),
        };
        m
    });
    let out = render_status(&s, &SessionId::new("sess-123"), "mac-seatbelt", &[]);
    assert!(!out.contains("cwd:"), "an empty cwd drops the line: {out}");
    assert!(out.contains("provenance:"), "provenance renders: {out}");
}

#[test]
fn test_status_provenance_fork_format() {
    let mut s = snap_with_descriptor("glm-5.1");
    s.descriptor = s.descriptor.map(|mut m| {
        m.provenance = SessionProvenance::ForkedFrom {
            from_sid: "sess-aaa".into(),
            from_seq: Some(7),
        };
        m
    });
    let out = render_status(&s, &SessionId::new("sess-123"), "mac-seatbelt", &[]);
    assert!(out.contains("forked from sess-aaa at turn 7") && out.contains("provenance:"));
}

#[test]
fn test_status_provenance_spawned_format() {
    let mut s = snap_with_descriptor("glm-5.1");
    s.descriptor = s.descriptor.map(|mut m| {
        m.provenance = SessionProvenance::SpawnedBy {
            parent_session_id: "parent-1".into(),
            subagent_type: "explore".into(),
            task_id: "task-7".into(),
        };
        m
    });
    let out = render_status(&s, &SessionId::new("sess-123"), "mac-seatbelt", &[]);
    assert!(out.contains("spawned by parent-1 (explore)") && out.contains("provenance:"));
}

#[test]
fn test_status_provenance_resumed_format() {
    let mut s = snap_with_descriptor("glm-5.1");
    s.descriptor = s.descriptor.map(|mut m| {
        m.provenance = SessionProvenance::ResumedFromExport {
            source_session_id: "sess-orig".into(),
        };
        m
    });
    let out = render_status(&s, &SessionId::new("sess-123"), "mac-seatbelt", &[]);
    assert!(out.contains("resumed from export sess-orig") && out.contains("provenance:"));
}

#[test]
fn test_status_unnamed_placeholder() {
    let mut s = snap_with_descriptor("glm-5.1");
    s.descriptor = s.descriptor.map(|mut m| {
        m.name = None;
        m
    });
    let out = render_status(&s, &SessionId::new("sess-123"), "mac-seatbelt", &[]);
    assert!(out.contains("(unnamed)") && out.contains("Session name:"));
}
